//! AWS Signature Version 4.
//!
//! # Why this is hand-written
//!
//! Everywhere else in this project, the rule has been to hand work to something
//! that already does it correctly — the server computes masking, the vendor
//! tools write the dumps — because the failure mode of getting it subtly wrong
//! is *silent* and produces data that looks fine.
//!
//! Request signing is the opposite. A signature that is wrong by one byte is a
//! `403 SignatureDoesNotMatch` on the first request, every time, immediately.
//! There is no version of this that half-works and corrupts a backup. That
//! makes it one of the few places where writing it out is the cheaper trade
//! than adding the reference SDK's forty-odd transitive crates to a bundle we
//! sign and notarise — and it leaves cancellation and byte-level progress under
//! our own control, which a long upload needs.
//!
//! The specification is stable and public; the algorithm has not changed since
//! 2012.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
pub const AWS4_REQUEST: &str = "aws4_request";

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Percent-encode per RFC 3986, which is stricter than most URL encoders.
///
/// AWS is specific about the unreserved set, and `+`, `*` and `~` are the three
/// that common encoders get wrong: `+` must not stand for a space, `*` must be
/// escaped, and `~` must not be.
pub fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// One header, already lowercased and trimmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: String,
}

impl Header {
    pub fn new(name: &str, value: impl Into<String>) -> Self {
        Self {
            name: name.to_ascii_lowercase(),
            value: value.into().trim().to_string(),
        }
    }
}

/// Everything the signature covers.
#[derive(Debug, Clone)]
pub struct CanonicalRequest {
    pub method: String,
    /// Already percent-encoded, with `/` preserved.
    pub path: String,
    /// Sorted `key=value` pairs, both percent-encoded.
    pub query: String,
    pub headers: Vec<Header>,
    /// Hex SHA-256 of the body. Always computed — every request here has a body
    /// small enough to hash, or is hashed part by part.
    pub payload_hash: String,
}

impl CanonicalRequest {
    /// Header names, lowercased and sorted, joined with `;`.
    pub fn signed_headers(&self) -> String {
        let mut names: Vec<&str> = self.headers.iter().map(|h| h.name.as_str()).collect();
        names.sort_unstable();
        names.join(";")
    }

    pub fn to_canonical_string(&self) -> String {
        let mut headers = self.headers.clone();
        headers.sort_by(|a, b| a.name.cmp(&b.name));

        let canonical_headers: String = headers
            .iter()
            .map(|h| format!("{}:{}\n", h.name, h.value))
            .collect();

        format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            self.method,
            self.path,
            self.query,
            canonical_headers,
            self.signed_headers(),
            self.payload_hash
        )
    }
}

/// Credential scope: `date/region/service/aws4_request`.
pub fn scope(date: &str, region: &str, service: &str) -> String {
    format!("{date}/{region}/{service}/{AWS4_REQUEST}")
}

pub fn string_to_sign(timestamp: &str, scope: &str, canonical_request: &str) -> String {
    format!(
        "{ALGORITHM}\n{timestamp}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    )
}

/// Derive the date/region/service-scoped signing key.
///
/// Four chained HMACs rather than the secret directly, so a leaked signing key
/// is useless outside one day, one region and one service.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date);
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, service);
    hmac(&k_service, AWS4_REQUEST)
}

pub fn signature(signing_key: &[u8], string_to_sign: &str) -> String {
    let mut out = String::new();
    for byte in hmac(signing_key, string_to_sign) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The complete `Authorization` header value.
pub fn authorization_header(
    access_key_id: &str,
    scope: &str,
    signed_headers: &str,
    signature: &str,
) -> String {
    format!(
        "{ALGORITHM} Credential={access_key_id}/{scope}, \
         SignedHeaders={signed_headers}, Signature={signature}"
    )
}

/// Sign a request, returning the `Authorization` value.
pub fn sign(
    request: &CanonicalRequest,
    access_key_id: &str,
    secret_access_key: &str,
    timestamp: &str,
    region: &str,
    service: &str,
) -> String {
    let date = &timestamp[..8];
    let scope = scope(date, region, service);
    let sts = string_to_sign(timestamp, &scope, &request.to_canonical_string());
    let key = signing_key(secret_access_key, date, region, service);
    authorization_header(
        access_key_id,
        &scope,
        &request.signed_headers(),
        &signature(&key, &sts),
    )
}

/// `YYYYMMDDTHHMMSSZ`, the only timestamp format SigV4 accepts.
pub fn amz_date(now: chrono::DateTime<chrono::Utc>) -> String {
    now.format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The published AWS SigV4 test-suite credentials. Not a real key.
    const KEY_ID: &str = "AKIDEXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const TIMESTAMP: &str = "20150830T123600Z";
    const REGION: &str = "us-east-1";
    const SERVICE: &str = "service";

    /// `get-vanilla` from the AWS SigV4 test suite.
    ///
    /// This is the one case with a published, byte-exact expected output, so it
    /// pins the whole chain: canonical request, string to sign, key derivation
    /// and final signature. Everything else here checks a single step.
    #[test]
    fn matches_the_published_get_vanilla_vector() {
        let request = CanonicalRequest {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            headers: vec![
                Header::new("host", "example.amazonaws.com"),
                Header::new("x-amz-date", TIMESTAMP),
            ],
            payload_hash: sha256_hex(b""),
        };

        assert_eq!(
            request.to_canonical_string(),
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\n\
             host;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let auth = sign(&request, KEY_ID, SECRET, TIMESTAMP, REGION, SERVICE);
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn the_empty_payload_hash_is_the_known_sha256_of_nothing() {
        // Interpolated into every request that has no body; a wrong constant
        // here would fail every one of them.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // ── URI encoding ────────────────────────────────────────────────────

    #[test]
    fn unreserved_characters_are_left_alone() {
        assert_eq!(uri_encode("abcXYZ019-._~", true), "abcXYZ019-._~");
    }

    #[test]
    fn a_tilde_is_not_escaped_and_a_star_is() {
        // The two most common encoder bugs. RFC 3986 says `~` is unreserved;
        // many encoders escape it anyway, and many leave `*` alone.
        assert_eq!(uri_encode("~", true), "~");
        assert_eq!(uri_encode("*", true), "%2A");
    }

    #[test]
    fn a_space_becomes_percent_20_not_plus() {
        // `+` for a space is form encoding, not URI encoding. A key with a
        // space in it would sign one way and be stored under another.
        assert_eq!(uri_encode("a b", true), "a%20b");
    }

    #[test]
    fn a_slash_is_preserved_in_paths_and_escaped_in_query_values() {
        assert_eq!(uri_encode("a/b", false), "a/b");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
    }

    #[test]
    fn non_ascii_is_encoded_per_utf8_byte() {
        // Object keys carry database names, and those are not always ASCII.
        assert_eq!(uri_encode("café", true), "caf%C3%A9");
        assert_eq!(uri_encode("日本", true), "%E6%97%A5%E6%9C%AC");
    }

    // ── Canonicalisation ────────────────────────────────────────────────

    #[test]
    fn headers_are_sorted_regardless_of_the_order_given() {
        // The server sorts before verifying, so anything else is a 403.
        let request = CanonicalRequest {
            method: "PUT".into(),
            path: "/".into(),
            query: String::new(),
            headers: vec![
                Header::new("x-amz-date", TIMESTAMP),
                Header::new("host", "example.com"),
                Header::new("content-length", "10"),
            ],
            payload_hash: sha256_hex(b""),
        };

        assert_eq!(request.signed_headers(), "content-length;host;x-amz-date");
        let canonical = request.to_canonical_string();
        let header_block = canonical.lines().skip(3).take(3).collect::<Vec<_>>();
        assert_eq!(
            header_block,
            vec![
                "content-length:10",
                "host:example.com",
                "x-amz-date:20150830T123600Z"
            ]
        );
    }

    #[test]
    fn header_names_are_lowercased_and_values_trimmed() {
        let h = Header::new("X-Amz-Content-Sha256", "  abc  ");
        assert_eq!(h.name, "x-amz-content-sha256");
        assert_eq!(h.value, "abc");
    }

    // ── Key derivation ──────────────────────────────────────────────────

    #[test]
    fn the_signing_key_is_scoped_to_date_region_and_service() {
        // Each component must actually reach the chain. A key that ignored the
        // region would still sign, and would still be rejected — but only in
        // the region nobody tested.
        let base = signing_key(SECRET, "20150830", REGION, SERVICE);
        assert_ne!(base, signing_key(SECRET, "20150831", REGION, SERVICE));
        assert_ne!(base, signing_key(SECRET, "20150830", "eu-west-1", SERVICE));
        assert_ne!(base, signing_key(SECRET, "20150830", REGION, "s3"));
    }

    #[test]
    fn the_scope_string_has_the_shape_the_credential_field_needs() {
        assert_eq!(
            scope("20150830", "us-east-1", "s3"),
            "20150830/us-east-1/s3/aws4_request"
        );
    }

    #[test]
    fn the_date_is_the_first_eight_characters_of_the_timestamp() {
        // `sign` slices rather than reformatting; if the timestamp format ever
        // changes this is what breaks.
        assert_eq!(&TIMESTAMP[..8], "20150830");
        assert_eq!(
            amz_date(
                chrono::DateTime::parse_from_rfc3339("2026-07-26T17:04:05Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc)
            ),
            "20260726T170405Z"
        );
    }

    #[test]
    fn signatures_are_lowercase_hex() {
        // The server compares the string, not the bytes.
        let key = signing_key(SECRET, "20150830", REGION, SERVICE);
        let sig = signature(&key, "anything");
        assert_eq!(sig.len(), 64);
        assert!(
            sig.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }
}
