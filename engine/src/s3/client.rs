//! An S3 client, built on the signing in [`super::sign`].
//!
//! Covers what shipping a backup off-site needs and nothing else: put, head,
//! list, delete, and multipart for objects too large for a single request.

use std::path::Path;

use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use super::sign::{self, CanonicalRequest, Header};
use super::xml;

/// Objects at or above this go through multipart.
///
/// S3 caps a single `PUT` at 5 GiB, so a threshold is not optional. This one is
/// far below that because the real reason to split is retry granularity: a
/// network drop 4 GiB into a single request restarts from zero, whereas a
/// failed part is one part.
pub const MULTIPART_THRESHOLD: u64 = 64 * 1024 * 1024;

/// Size of each part. S3 requires at least 5 MiB for every part but the last.
pub const PART_SIZE: u64 = 16 * 1024 * 1024;

/// Keys requested per listing round trip. 1000 is the S3 maximum and default.
///
/// The number matters less than the fact that [`S3Client::list`] follows the
/// continuation token past it. A listing that stops at the first page is how
/// off-site retention would decide what to keep while seeing only part of what
/// is there.
pub const LIST_PAGE_SIZE: u32 = 1000;

/// The service name in the credential scope. Fixed for object storage.
const SERVICE: &str = "s3";

#[derive(Debug, Clone)]
pub struct S3Config {
    /// Base URL, with scheme. `https://s3.eu-west-1.amazonaws.com`, or
    /// `http://127.0.0.1:9000` for a local MinIO.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Prefix applied to every key, so one bucket can hold several sources.
    pub prefix: String,
    /// `https://endpoint/bucket/key` rather than `https://bucket.endpoint/key`.
    ///
    /// Required by MinIO and most self-hosted gateways, and by any bucket whose
    /// name is not a valid DNS label.
    pub path_style: bool,
    pub access_key_id: String,
    pub secret_access_key: SecretString,
}

#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("{operation} failed: {code}: {message}")]
    Api {
        operation: &'static str,
        code: String,
        message: String,
    },
    #[error("{operation} returned {status}, and the body was not an S3 error: {body}")]
    Unexpected {
        operation: &'static str,
        status: u16,
        body: String,
    },
    #[error("could not reach {endpoint}: {source}")]
    Transport {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("upload was cancelled")]
    Cancelled,
}

/// What a stored object looks like from outside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    /// When the object was last written, as the server reports it.
    ///
    /// `None` when the timestamp was missing or unparseable. Off-site retention
    /// treats that as "do not touch this" rather than substituting the current
    /// time, which would make an unreadable date look like a brand-new object —
    /// or, with the opposite default, delete one that might be the only copy.
    pub last_modified: Option<chrono::DateTime<chrono::Utc>>,
}

/// One request, before it is signed.
///
/// A struct rather than eight positional parameters: `send("uploading", PUT,
/// key, "", headers, body, None)` reads as a puzzle, and swapping two of the
/// three strings compiles cleanly.
struct Request<'a> {
    operation: &'static str,
    method: reqwest::Method,
    key: &'a str,
    query: &'a str,
    extra_headers: Vec<Header>,
    body: Vec<u8>,
    /// Precomputed digest, for a body already hashed elsewhere.
    payload_hash: Option<String>,
}

impl<'a> Request<'a> {
    fn new(operation: &'static str, method: reqwest::Method, key: &'a str) -> Self {
        Self {
            operation,
            method,
            key,
            query: "",
            extra_headers: Vec::new(),
            body: Vec::new(),
            payload_hash: None,
        }
    }

    fn query(mut self, query: &'a str) -> Self {
        self.query = query;
        self
    }

    /// Sets the body and its `content-length` together, so they cannot drift.
    fn body(mut self, body: Vec<u8>) -> Self {
        self.extra_headers
            .push(Header::new("content-length", body.len().to_string()));
        self.body = body;
        self
    }

    fn payload_hash(mut self, hash: String) -> Self {
        self.payload_hash = Some(hash);
        self
    }
}

#[derive(Debug)]
pub struct S3Client {
    config: S3Config,
    http: reqwest::Client,
}

impl S3Client {
    pub fn new(config: S3Config) -> Result<Self, S3Error> {
        if config.bucket.trim().is_empty() {
            return Err(S3Error::Invalid("no bucket configured".into()));
        }
        if !config.endpoint.starts_with("http://") && !config.endpoint.starts_with("https://") {
            return Err(S3Error::Invalid(format!(
                "endpoint {:?} needs a scheme, e.g. https://s3.eu-west-1.amazonaws.com",
                config.endpoint
            )));
        }

        let http = reqwest::Client::builder()
            // Redirects are not followed: a signed request replayed to another
            // host is a signature failure at best, and credentials pointed
            // somewhere unintended at worst.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|source| S3Error::Transport {
                endpoint: config.endpoint.clone(),
                source,
            })?;

        Ok(Self { config, http })
    }

    pub fn config(&self) -> &S3Config {
        &self.config
    }

    /// The full key for an artifact filename, including the configured prefix.
    pub fn key_for(&self, filename: &str) -> String {
        let prefix = self.config.prefix.trim_matches('/');
        if prefix.is_empty() {
            filename.to_string()
        } else {
            format!("{prefix}/{filename}")
        }
    }

    /// Host header and URL path for a key, honouring the addressing style.
    fn address(&self, key: &str) -> (String, String) {
        let base = self.config.endpoint.trim_end_matches('/').to_string();
        let encoded = sign::uri_encode(key, false);

        if self.config.path_style {
            let host = base
                .split_once("://")
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_else(|| base.clone());
            (host, format!("/{}/{}", self.config.bucket, encoded))
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            let _ = scheme;
            (
                format!("{}.{}", self.config.bucket, rest),
                format!("/{encoded}"),
            )
        }
    }

    fn url(&self, path: &str, query: &str) -> String {
        let base = self.config.endpoint.trim_end_matches('/');
        let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
        let host = if self.config.path_style {
            rest.to_string()
        } else {
            format!("{}.{}", self.config.bucket, rest)
        };
        if query.is_empty() {
            format!("{scheme}://{host}{path}")
        } else {
            format!("{scheme}://{host}{path}?{query}")
        }
    }

    /// Sign and send one request.
    async fn send(
        &self,
        request: Request<'_>,
    ) -> Result<(reqwest::StatusCode, reqwest::header::HeaderMap, String), S3Error> {
        let Request {
            operation,
            method,
            key,
            query,
            extra_headers,
            body,
            payload_hash,
        } = request;

        let (host, path) = self.address(key);
        let timestamp = sign::amz_date(chrono::Utc::now());
        let hash = payload_hash.unwrap_or_else(|| sign::sha256_hex(&body));

        let mut headers = vec![
            Header::new("host", &host),
            Header::new("x-amz-date", &timestamp),
            Header::new("x-amz-content-sha256", &hash),
        ];
        headers.extend(extra_headers);

        let canonical = CanonicalRequest {
            method: method.as_str().to_string(),
            path: path.clone(),
            query: query.to_string(),
            headers: headers.clone(),
            payload_hash: hash,
        };

        let authorization = sign::sign(
            &canonical,
            &self.config.access_key_id,
            self.config.secret_access_key.expose_secret(),
            &timestamp,
            &self.config.region,
            SERVICE,
        );

        let mut request = self.http.request(method, self.url(&path, query));
        for header in &headers {
            request = request.header(&header.name, &header.value);
        }
        request = request.header("authorization", authorization);

        let response = request
            .body(body)
            .send()
            .await
            .map_err(|source| S3Error::Transport {
                endpoint: self.config.endpoint.clone(),
                source,
            })?;

        let status = response.status();
        let response_headers = response.headers().clone();
        let text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(match xml::parse_error(&text) {
                Some(e) => S3Error::Api {
                    operation,
                    code: e.code,
                    message: e.message,
                },
                None => S3Error::Unexpected {
                    operation,
                    status: status.as_u16(),
                    body: text.chars().take(300).collect(),
                },
            });
        }

        Ok((status, response_headers, text))
    }

    /// Check the endpoint, credentials and bucket in one round trip.
    ///
    /// Done before a backup rather than after: discovering that the bucket name
    /// is wrong at the end of an hour-long dump is an expensive way to find out.
    pub async fn check_access(&self) -> Result<(), S3Error> {
        // `max-keys=1`, not `0`. Asking for zero keys looks like the cheaper
        // question and is not the same question: MinIO answers it with an
        // empty result *without checking the bucket exists*, so a typo'd
        // bucket name passed this check and failed at upload time instead.
        // Asking for one key takes the path the server actually validates.
        let query = sign::canonical_query(&[("list-type", "2"), ("max-keys", "1")]);
        self.send(Request::new("checking bucket access", reqwest::Method::GET, "").query(&query))
            .await?;
        Ok(())
    }

    /// Create the bucket, treating "it already exists" as success.
    ///
    /// Deliberately not called automatically anywhere. A typo in a bucket name
    /// should surface as "that bucket does not exist", not as a second bucket
    /// quietly appearing on someone's account — so this is only ever reached by
    /// an explicit request.
    pub async fn create_bucket(&self) -> Result<(), S3Error> {
        // The region must be declared in the body for anything but us-east-1,
        // and declaring it there is harmless when it matches.
        let body = if self.config.region == "us-east-1" {
            Vec::new()
        } else {
            format!(
                "<CreateBucketConfiguration>\
                 <LocationConstraint>{}</LocationConstraint>\
                 </CreateBucketConfiguration>",
                xml::escape(&self.config.region)
            )
            .into_bytes()
        };

        let mut request = Request::new("creating the bucket", reqwest::Method::PUT, "");
        if !body.is_empty() {
            request = request.body(body);
        }

        match self.send(request).await {
            Ok(_) => Ok(()),
            Err(S3Error::Api { code, .. })
                if code == "BucketAlreadyOwnedByYou" || code == "BucketAlreadyExists" =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Size of an object, or `None` when it is not there.
    pub async fn head(&self, key: &str) -> Result<Option<u64>, S3Error> {
        match self
            .send(Request::new(
                "reading object metadata",
                reqwest::Method::HEAD,
                key,
            ))
            .await
        {
            Ok((_, headers, _)) => Ok(headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())),
            // A HEAD has no body, so a 404 arrives with nothing to parse.
            Err(S3Error::Unexpected { status: 404, .. }) => Ok(None),
            Err(S3Error::Api { code, .. }) if code == "NoSuchKey" || code == "404" => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Every object under a prefix, following continuation tokens to the end.
    pub async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>, S3Error> {
        self.list_paged(prefix, LIST_PAGE_SIZE).await
    }

    /// [`S3Client::list`] with an explicit page size.
    ///
    /// Exists so the pagination can be exercised against a real server without
    /// uploading a thousand objects: ask for one key at a time and the
    /// continuation path is the only path.
    pub async fn list_paged(
        &self,
        prefix: &str,
        page_size: u32,
    ) -> Result<Vec<ObjectInfo>, S3Error> {
        let page_size = page_size.clamp(1, LIST_PAGE_SIZE).to_string();
        let mut out = Vec::new();
        let mut token: Option<String> = None;

        loop {
            let mut params = vec![
                ("list-type", "2"),
                ("max-keys", page_size.as_str()),
                ("prefix", prefix),
            ];
            if let Some(t) = &token {
                params.push(("continuation-token", t.as_str()));
            }

            let (_, _, body) = self
                .send(
                    Request::new("listing objects", reqwest::Method::GET, "")
                        .query(&sign::canonical_query(&params)),
                )
                .await?;

            out.extend(parse_listing(&body));

            // `IsTruncated` is the server's own statement that there is more.
            // Stopping on an empty page instead would end the listing early
            // whenever a page is filtered down to nothing.
            let truncated = xml::first(&body, "IsTruncated").as_deref() == Some("true");
            token = xml::first(&body, "NextContinuationToken");

            match (truncated, &token) {
                (true, Some(_)) => continue,
                (true, None) => {
                    // The server says there is more and did not say where to
                    // resume. Returning a short list as if it were complete is
                    // what retention would then act on.
                    return Err(S3Error::Invalid(format!(
                        "the listing of {prefix:?} is incomplete: the server reported more \
                         results but sent no continuation token"
                    )));
                }
                (false, _) => return Ok(out),
            }
        }
    }

    pub async fn delete(&self, key: &str) -> Result<(), S3Error> {
        self.send(Request::new(
            "deleting an object",
            reqwest::Method::DELETE,
            key,
        ))
        .await?;
        Ok(())
    }

    /// Upload a file, choosing single or multipart by size.
    ///
    /// `progress` is called with bytes sent so far. `cancelled` is checked
    /// between parts; a cancelled multipart upload is aborted so the parts
    /// already sent do not linger and accrue storage charges.
    pub async fn upload_file(
        &self,
        path: &Path,
        key: &str,
        progress: &mut (dyn FnMut(u64, u64) + Send),
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<u64, S3Error> {
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|source| S3Error::Io {
                path: path.display().to_string(),
                source,
            })?;
        let total = metadata.len();

        if total < MULTIPART_THRESHOLD {
            self.put_small_file(path, key, total, progress).await?;
        } else {
            self.put_multipart(path, key, total, progress, cancelled)
                .await?;
        }

        // ── Prove it arrived ────────────────────────────────────────────
        //
        // A 200 says the request was accepted, not that the object is readable
        // at the size we sent. Reading it back is the only thing that does,
        // and this is the same reasoning as every other verification here.
        match self.head(key).await? {
            Some(stored) if stored == total => Ok(total),
            Some(stored) => Err(S3Error::Invalid(format!(
                "uploaded {total} bytes to {key} but the object reads back as {stored}"
            ))),
            None => Err(S3Error::Invalid(format!(
                "uploaded {key} but it is not there when read back"
            ))),
        }
    }

    async fn put_small_file(
        &self,
        path: &Path,
        key: &str,
        total: u64,
        progress: &mut (dyn FnMut(u64, u64) + Send),
    ) -> Result<(), S3Error> {
        let body = tokio::fs::read(path).await.map_err(|source| S3Error::Io {
            path: path.display().to_string(),
            source,
        })?;

        self.send(Request::new("uploading", reqwest::Method::PUT, key).body(body))
            .await?;

        progress(total, total);
        Ok(())
    }

    async fn put_multipart(
        &self,
        path: &Path,
        key: &str,
        total: u64,
        progress: &mut (dyn FnMut(u64, u64) + Send),
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), S3Error> {
        let query = sign::canonical_query(&[("uploads", "")]);
        let (_, _, body) = self
            .send(
                Request::new("starting a multipart upload", reqwest::Method::POST, key)
                    .query(&query),
            )
            .await?;

        let upload_id = xml::first(&body, "UploadId").ok_or_else(|| {
            S3Error::Invalid("the server did not return an upload id".to_string())
        })?;

        match self
            .upload_parts(path, key, &upload_id, total, progress, cancelled)
            .await
        {
            Ok(etags) => {
                self.complete_multipart(key, &upload_id, &etags).await?;
                Ok(())
            }
            Err(e) => {
                // Abandoned parts are billed until they are cleaned up, and a
                // failed upload should not quietly cost money.
                let _ = self.abort_multipart(key, &upload_id).await;
                Err(e)
            }
        }
    }

    async fn upload_parts(
        &self,
        path: &Path,
        key: &str,
        upload_id: &str,
        total: u64,
        progress: &mut (dyn FnMut(u64, u64) + Send),
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<String>, S3Error> {
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|source| S3Error::Io {
                path: path.display().to_string(),
                source,
            })?;

        let mut etags = Vec::new();
        let mut sent = 0u64;
        let mut part_number = 1u32;

        loop {
            if cancelled() {
                return Err(S3Error::Cancelled);
            }

            let mut buffer = vec![0u8; PART_SIZE as usize];
            let mut filled = 0usize;
            while filled < buffer.len() {
                let n = file
                    .read(&mut buffer[filled..])
                    .await
                    .map_err(|source| S3Error::Io {
                        path: path.display().to_string(),
                        source,
                    })?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            buffer.truncate(filled);

            let hash = format!("{:x}", Sha256::digest(&buffer));
            let part = part_number.to_string();
            let query =
                sign::canonical_query(&[("partNumber", part.as_str()), ("uploadId", upload_id)]);

            let (_, headers, _) = self
                .send(
                    Request::new("uploading a part", reqwest::Method::PUT, key)
                        .query(&query)
                        .body(buffer)
                        .payload_hash(hash),
                )
                .await?;

            let etag = headers
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    S3Error::Invalid(format!("part {part_number} was stored without an ETag"))
                })?
                .to_string();

            etags.push(etag);
            sent += filled as u64;
            progress(sent, total);
            part_number += 1;
        }

        Ok(etags)
    }

    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        etags: &[String],
    ) -> Result<(), S3Error> {
        let mut body = String::from("<CompleteMultipartUpload>");
        for (i, etag) in etags.iter().enumerate() {
            body.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                i + 1,
                xml::escape(etag)
            ));
        }
        body.push_str("</CompleteMultipartUpload>");

        let query = sign::canonical_query(&[("uploadId", upload_id)]);
        let bytes = body.into_bytes();

        let (_, _, response) = self
            .send(
                Request::new("completing a multipart upload", reqwest::Method::POST, key)
                    .query(&query)
                    .body(bytes),
            )
            .await?;

        // S3 can return 200 with an error document in the body for this one
        // call, because the response streams while the parts are assembled.
        // Treating that as success would report an upload that never completed.
        if let Some(e) = xml::parse_error(&response) {
            return Err(S3Error::Api {
                operation: "completing a multipart upload",
                code: e.code,
                message: e.message,
            });
        }

        Ok(())
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<(), S3Error> {
        let query = sign::canonical_query(&[("uploadId", upload_id)]);
        self.send(
            Request::new("aborting a multipart upload", reqwest::Method::DELETE, key).query(&query),
        )
        .await?;
        Ok(())
    }
}

/// Pull the objects out of one `ListObjectsV2` page.
///
/// Parsed one `<Contents>` block at a time rather than by collecting every
/// `<Key>` and every `<Size>` into parallel lists and zipping them. The zip
/// reads fine and is wrong the moment a single element is absent from one
/// entry: every later object silently acquires the previous one's size and
/// timestamp, and retention would then delete by the wrong dates.
fn parse_listing(body: &str) -> Vec<ObjectInfo> {
    body.split("<Contents>")
        // The first chunk is everything before the first entry.
        .skip(1)
        .filter_map(|chunk| {
            let entry = chunk.split("</Contents>").next().unwrap_or(chunk);
            Some(ObjectInfo {
                key: xml::first(entry, "Key")?,
                size: xml::first(entry, "Size")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                last_modified: xml::first(entry, "LastModified")
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                    .map(|t| t.with_timezone(&chrono::Utc)),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(path_style: bool) -> S3Config {
        S3Config {
            endpoint: "https://s3.eu-west-1.amazonaws.com".into(),
            region: "eu-west-1".into(),
            bucket: "backups".into(),
            prefix: "prod".into(),
            path_style,
            access_key_id: "AKIDEXAMPLE".into(),
            // Distinctive so a leak cannot be confused with the field name.
            secret_access_key: SecretString::from("wJalrXUtnFEMI-TESTVALUE"),
        }
    }

    #[test]
    fn path_style_puts_the_bucket_in_the_path() {
        // What MinIO and most self-hosted gateways require.
        let c = S3Client::new(config(true)).unwrap();
        let (host, path) = c.address("prod/a.sql.gz");
        assert_eq!(host, "s3.eu-west-1.amazonaws.com");
        assert_eq!(path, "/backups/prod/a.sql.gz");
    }

    #[test]
    fn virtual_host_style_puts_the_bucket_in_the_host() {
        let c = S3Client::new(config(false)).unwrap();
        let (host, path) = c.address("prod/a.sql.gz");
        assert_eq!(host, "backups.s3.eu-west-1.amazonaws.com");
        assert_eq!(path, "/prod/a.sql.gz");
    }

    #[test]
    fn the_signed_host_matches_the_host_the_request_goes_to() {
        // These are computed separately, and a mismatch is a 403 that looks
        // like a credential problem rather than an addressing one.
        for path_style in [true, false] {
            let c = S3Client::new(config(path_style)).unwrap();
            let (host, path) = c.address("k");
            let url = c.url(&path, "");
            assert!(
                url.contains(&host),
                "path_style={path_style}: signed {host}, sent {url}"
            );
        }
    }

    #[test]
    fn slashes_in_a_key_stay_slashes() {
        // Encoding them would store one object literally named "a%2Fb".
        let c = S3Client::new(config(true)).unwrap();
        let (_, path) = c.address("2026/07/a.sql.gz");
        assert_eq!(path, "/backups/2026/07/a.sql.gz");
    }

    #[test]
    fn a_space_in_a_key_is_encoded() {
        let c = S3Client::new(config(true)).unwrap();
        let (_, path) = c.address("my backups/a.sql.gz");
        assert_eq!(path, "/backups/my%20backups/a.sql.gz");
    }

    #[test]
    fn the_prefix_is_applied_once_and_slashes_are_not_doubled() {
        let mut cfg = config(true);
        cfg.prefix = "/prod/".into();
        let c = S3Client::new(cfg).unwrap();
        assert_eq!(c.key_for("a.sql.gz"), "prod/a.sql.gz");
    }

    #[test]
    fn an_empty_prefix_leaves_the_key_alone() {
        let mut cfg = config(true);
        cfg.prefix = String::new();
        let c = S3Client::new(cfg).unwrap();
        assert_eq!(c.key_for("a.sql.gz"), "a.sql.gz");
    }

    #[test]
    fn an_endpoint_without_a_scheme_is_refused() {
        // `s3.amazonaws.com` looks reasonable and would silently produce a
        // malformed URL, so it is rejected with the fix in the message.
        let mut cfg = config(true);
        cfg.endpoint = "s3.amazonaws.com".into();
        let err = S3Client::new(cfg).unwrap_err();
        assert!(err.to_string().contains("needs a scheme"), "{err}");
    }

    #[test]
    fn an_empty_bucket_is_refused() {
        let mut cfg = config(true);
        cfg.bucket = "  ".into();
        assert!(S3Client::new(cfg).is_err());
    }

    #[test]
    fn a_trailing_slash_on_the_endpoint_does_not_double_up() {
        let mut cfg = config(true);
        cfg.endpoint = "http://127.0.0.1:9000/".into();
        let c = S3Client::new(cfg).unwrap();
        let (_, path) = c.address("a");
        assert_eq!(c.url(&path, ""), "http://127.0.0.1:9000/backups/a");
    }

    #[test]
    fn debug_output_does_not_leak_the_secret_key() {
        // These get logged, and an S3 secret key is a credential like any
        // other. `secrecy` redacts it; this pins that it still does.
        let c = S3Client::new(config(true)).unwrap();
        let rendered = format!("{c:?}");
        assert!(!rendered.contains("wJalrXUtnFEMI-TESTVALUE"), "{rendered}");
        assert!(rendered.contains("REDACTED"), "{rendered}");
        assert!(
            rendered.contains("AKIDEXAMPLE"),
            "the key id is not secret and is useful in a log: {rendered}"
        );
    }

    // ── Listing ─────────────────────────────────────────────────────────

    const PAGE: &str = "<ListBucketResult>\
        <Name>backups</Name><Prefix>prod/</Prefix><KeyCount>2</KeyCount>\
        <Contents><Key>prod/a.sql.gz</Key>\
          <LastModified>2026-07-20T03:00:00.000Z</LastModified>\
          <Size>1024</Size></Contents>\
        <Contents><Key>prod/b.sql.gz</Key>\
          <LastModified>2026-07-26T03:00:00.000Z</LastModified>\
          <Size>2048</Size></Contents>\
        </ListBucketResult>";

    #[test]
    fn a_listing_page_is_parsed_entry_by_entry() {
        let objects = parse_listing(PAGE);
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].key, "prod/a.sql.gz");
        assert_eq!(objects[0].size, 1024);
        assert_eq!(objects[1].size, 2048);
        assert!(objects[0].last_modified.unwrap() < objects[1].last_modified.unwrap());
    }

    #[test]
    fn the_bucket_name_is_not_mistaken_for_an_object() {
        // `<Name>` and `<Prefix>` sit outside `<Contents>`; parsing the whole
        // document flat would pick them up.
        assert!(
            parse_listing(PAGE)
                .iter()
                .all(|o| o.key.starts_with("prod/"))
        );
    }

    #[test]
    fn an_entry_missing_a_size_does_not_shift_the_others() {
        // The exact failure that zipping parallel `<Key>`/`<Size>` lists
        // produces: every object after the gap inherits its neighbour's size,
        // and a retention decision is then made on numbers belonging to a
        // different file.
        let xml = "<ListBucketResult>\
            <Contents><Key>a</Key></Contents>\
            <Contents><Key>b</Key><Size>77</Size></Contents>\
            </ListBucketResult>";

        let objects = parse_listing(xml);
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].key, "a");
        assert_eq!(objects[0].size, 0, "unknown, not b's size");
        assert_eq!(objects[1].key, "b");
        assert_eq!(objects[1].size, 77);
    }

    #[test]
    fn an_unparseable_timestamp_reads_as_unknown_rather_than_now() {
        let xml = "<ListBucketResult><Contents><Key>a</Key>\
            <LastModified>whenever</LastModified><Size>1</Size></Contents></ListBucketResult>";
        assert_eq!(parse_listing(xml)[0].last_modified, None);
    }

    #[test]
    fn an_empty_listing_parses_to_nothing() {
        let xml = "<ListBucketResult><Name>backups</Name><KeyCount>0</KeyCount></ListBucketResult>";
        assert!(parse_listing(xml).is_empty());
    }

    #[test]
    fn the_multipart_threshold_is_below_the_single_put_limit() {
        // S3 refuses a single PUT above 5 GiB; anything at or above the
        // threshold must take the other path.
        const { assert!(MULTIPART_THRESHOLD < 5 * 1024 * 1024 * 1024) };
        // And every part but the last must be at least 5 MiB.
        const { assert!(PART_SIZE >= 5 * 1024 * 1024) };
    }
}
