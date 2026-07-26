//! Off-site destinations: where a copy of each artifact is sent.
//!
//! A backup that only exists on the machine that made it is one failure away
//! from not existing — the same disk, the same laptop, the same office. A
//! destination is the second copy.
//!
//! # What lives here and what does not
//!
//! Everything in a [`Destination`] is safe to print, log and export: an
//! endpoint, a bucket, a region, an access key id. The secret access key is
//! **not** part of this type and is never persisted in the store. It goes in
//! the OS keychain under [`crate::secrets::SecretKind::ObjectStoreSecret`],
//! keyed by the destination's id, and is resolved only at the moment a request
//! is signed. That split is deliberate: it means a destination can be listed,
//! serialised into a job's options, and shown in a UI without any code path
//! having to remember to redact something.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

use crate::retention::RetentionPolicy;
use crate::s3::S3Config;

/// Suffix identifying the sidecar file that describes an artifact.
///
/// Off-site retention needs to tell the two apart: a manifest is not an
/// artifact and must never be counted as one of the copies being kept, but it
/// must be removed alongside the artifact it describes.
pub const MANIFEST_SUFFIX: &str = ".manifest.json";

#[derive(Debug, thiserror::Error)]
pub enum DestinationError {
    #[error("a destination needs a name")]
    NoName,
    #[error("{0}")]
    Invalid(String),
    #[error(
        "{endpoint} is a plaintext http:// endpoint on a remote host, so the backup and the \
         credentials signing it would both cross the network in the clear. Use https://, or \
         point at a loopback address if this is a local object store"
    )]
    InsecureEndpoint { endpoint: String },
}

/// The transports a destination can use.
///
/// A tagged enum rather than a bare struct because the persisted form is JSON
/// in a single column: adding SFTP or a second object-store dialect later is a
/// new variant, not a migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DestinationKind {
    /// Anything speaking the S3 API: AWS, Cloudflare R2, Backblaze B2,
    /// Wasabi, MinIO.
    S3(S3Destination),
}

/// An S3-compatible bucket. No secret: see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct S3Destination {
    /// Base URL with scheme, e.g. `https://s3.eu-west-1.amazonaws.com`.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Key prefix, so one bucket can hold several sources.
    #[serde(default)]
    pub prefix: String,
    /// `https://endpoint/bucket/key` rather than `https://bucket.endpoint/key`.
    #[serde(default)]
    pub path_style: bool,
    pub access_key_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Destination {
    pub id: Uuid,
    pub name: String,
    pub kind: DestinationKind,
    /// Disabled destinations are skipped by every backup but keep their
    /// configuration and their credential, so turning one off for an
    /// afternoon does not mean setting it up again.
    pub enabled: bool,
    /// Retention applied to the objects *at this destination*.
    ///
    /// Separate from the local policy on purpose. Off-site storage is usually
    /// cheaper and is the copy that survives losing the machine, so keeping
    /// more there than locally is the common case — a single shared policy
    /// would force the two to move together.
    pub retention: RetentionPolicy,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What the caller supplies to create one. The secret is handled separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DestinationCreate {
    pub name: String,
    pub kind: DestinationKind,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub retention: RetentionPolicy,
}

/// A partial edit. `None` leaves a field alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DestinationUpdate {
    pub name: Option<String>,
    pub kind: Option<DestinationKind>,
    pub enabled: Option<bool>,
    pub retention: Option<RetentionPolicy>,
}

const fn default_true() -> bool {
    true
}

impl DestinationKind {
    /// Reject anything that cannot be made to work, before it is stored.
    pub fn validate(&self) -> Result<(), DestinationError> {
        match self {
            DestinationKind::S3(s3) => s3.validate(),
        }
    }

    /// One line naming where this points, for logs and lists.
    pub fn describe(&self) -> String {
        match self {
            DestinationKind::S3(s3) => {
                let prefix = s3.prefix.trim_matches('/');
                if prefix.is_empty() {
                    format!("s3://{}", s3.bucket)
                } else {
                    format!("s3://{}/{prefix}", s3.bucket)
                }
            }
        }
    }
}

impl S3Destination {
    pub fn validate(&self) -> Result<(), DestinationError> {
        if self.bucket.trim().is_empty() {
            return Err(DestinationError::Invalid(
                "a bucket name is required".into(),
            ));
        }
        if self.region.trim().is_empty() {
            return Err(DestinationError::Invalid(
                "a region is required; use us-east-1 if the provider does not use regions".into(),
            ));
        }
        if self.access_key_id.trim().is_empty() {
            return Err(DestinationError::Invalid(
                "an access key id is required".into(),
            ));
        }

        let endpoint = self.endpoint.trim();
        if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
            return Err(DestinationError::Invalid(format!(
                "endpoint {endpoint:?} needs a scheme, e.g. https://s3.eu-west-1.amazonaws.com"
            )));
        }
        if let Some(rest) = endpoint.strip_prefix("http://")
            && !is_loopback(rest)
        {
            return Err(DestinationError::InsecureEndpoint {
                endpoint: endpoint.to_string(),
            });
        }

        Ok(())
    }

    /// Pair the stored configuration with a resolved secret.
    pub fn to_config(&self, secret: secrecy::SecretString) -> S3Config {
        S3Config {
            endpoint: self.endpoint.trim().to_string(),
            region: self.region.trim().to_string(),
            bucket: self.bucket.trim().to_string(),
            prefix: self.prefix.trim_matches('/').to_string(),
            path_style: self.path_style,
            access_key_id: self.access_key_id.trim().to_string(),
            secret_access_key: secret,
        }
    }
}

/// Whether an endpoint's host is on this machine.
///
/// Only loopback earns an exemption from the https requirement, and it is
/// decided by the *host*, not by the string containing "localhost" somewhere:
/// `http://localhost.evil.example` is a remote host with a reassuring name.
fn is_loopback(after_scheme: &str) -> bool {
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Strip credentials, if someone pasted a URL carrying them.
    let authority = authority.rsplit('@').next().unwrap_or(authority);

    let host = match authority.strip_prefix('[') {
        // IPv6 literal: `[::1]:9000`.
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => authority.split(':').next().unwrap_or(authority),
    };

    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

impl Destination {
    pub fn validate(&self) -> Result<(), DestinationError> {
        if self.name.trim().is_empty() {
            return Err(DestinationError::NoName);
        }
        self.kind.validate()
    }

    /// The key an artifact filename lands under, including any prefix.
    pub fn key_for(&self, filename: &str) -> String {
        match &self.kind {
            DestinationKind::S3(s3) => {
                let prefix = s3.prefix.trim_matches('/');
                if prefix.is_empty() {
                    filename.to_string()
                } else {
                    format!("{prefix}/{filename}")
                }
            }
        }
    }

    /// Where one object lives, as a URL a human can go and look at.
    ///
    /// Takes the *full* key, which already carries the prefix. Composing this
    /// from [`DestinationKind::describe`] instead would print the prefix twice
    /// and name a path that does not exist — the sort of message that sends
    /// someone hunting for a file in the wrong place during an incident.
    pub fn object_url(&self, key: &str) -> String {
        match &self.kind {
            DestinationKind::S3(s3) => format!("s3://{}/{key}", s3.bucket),
        }
    }

    /// The prefix a listing of this destination's own objects starts from.
    pub fn list_prefix(&self) -> String {
        match &self.kind {
            DestinationKind::S3(s3) => {
                let prefix = s3.prefix.trim_matches('/');
                if prefix.is_empty() {
                    String::new()
                } else {
                    format!("{prefix}/")
                }
            }
        }
    }
}

/// Whether an object key names an artifact rather than its manifest.
pub fn is_artifact_key(key: &str) -> bool {
    !key.ends_with(MANIFEST_SUFFIX) && (key.ends_with(".sql.gz") || key.ends_with(".dump"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3() -> S3Destination {
        S3Destination {
            endpoint: "https://s3.eu-west-1.amazonaws.com".into(),
            region: "eu-west-1".into(),
            bucket: "backups".into(),
            prefix: "prod".into(),
            path_style: false,
            access_key_id: "AKIDEXAMPLE".into(),
        }
    }

    fn destination(kind: DestinationKind) -> Destination {
        Destination {
            id: Uuid::new_v4(),
            name: "off-site".into(),
            kind,
            enabled: true,
            retention: RetentionPolicy::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn a_well_formed_destination_validates() {
        assert!(destination(DestinationKind::S3(s3())).validate().is_ok());
    }

    #[test]
    fn a_destination_needs_a_name() {
        let mut d = destination(DestinationKind::S3(s3()));
        d.name = "   ".into();
        assert!(matches!(d.validate(), Err(DestinationError::NoName)));
    }

    // ── The https requirement ───────────────────────────────────────────

    #[test]
    fn plaintext_http_to_a_remote_host_is_refused() {
        // Both the artifact and the credentials that sign for it would cross
        // the network readable. SigV4 authenticates a request; it does not
        // encrypt one.
        let mut cfg = s3();
        cfg.endpoint = "http://s3.example.com".into();
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, DestinationError::InsecureEndpoint { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("https://"), "and it says the fix");
    }

    #[test]
    fn plaintext_http_to_loopback_is_allowed() {
        // A local MinIO is the normal way to try this out, and nothing leaves
        // the machine.
        for endpoint in [
            "http://127.0.0.1:9000",
            "http://localhost:9000",
            "http://[::1]:9000",
            "http://127.0.0.1:9000/",
        ] {
            let mut cfg = s3();
            cfg.endpoint = endpoint.into();
            cfg.path_style = true;
            assert!(cfg.validate().is_ok(), "{endpoint} is on this machine");
        }
    }

    #[test]
    fn a_hostname_that_merely_starts_with_localhost_is_still_remote() {
        // `localhost.evil.example` resolves wherever its owner points it. A
        // substring check would have waved this through.
        for endpoint in [
            "http://localhost.evil.example:9000",
            "http://notlocalhost",
            "http://127.0.0.1.evil.example",
        ] {
            let mut cfg = s3();
            cfg.endpoint = endpoint.into();
            assert!(
                matches!(
                    cfg.validate(),
                    Err(DestinationError::InsecureEndpoint { .. })
                ),
                "{endpoint} is not this machine"
            );
        }
    }

    #[test]
    fn https_is_accepted_everywhere() {
        for endpoint in [
            "https://s3.eu-west-1.amazonaws.com",
            "https://abc123.r2.cloudflarestorage.com",
            "https://s3.us-west-002.backblazeb2.com",
        ] {
            let mut cfg = s3();
            cfg.endpoint = endpoint.into();
            assert!(cfg.validate().is_ok(), "{endpoint}");
        }
    }

    #[test]
    fn an_endpoint_without_a_scheme_is_refused() {
        let mut cfg = s3();
        cfg.endpoint = "s3.amazonaws.com".into();
        assert!(cfg.validate().unwrap_err().to_string().contains("scheme"));
    }

    #[test]
    fn the_required_fields_are_required() {
        for (field, mutate) in [
            (
                "bucket",
                (|c: &mut S3Destination| c.bucket = " ".into()) as fn(&mut S3Destination),
            ),
            ("region", |c: &mut S3Destination| c.region = "".into()),
            ("access key", |c: &mut S3Destination| {
                c.access_key_id = "".into()
            }),
        ] {
            let mut cfg = s3();
            mutate(&mut cfg);
            assert!(cfg.validate().is_err(), "{field} must be required");
        }
    }

    // ── Keys ────────────────────────────────────────────────────────────

    #[test]
    fn the_prefix_is_applied_without_doubling_slashes() {
        let mut cfg = s3();
        cfg.prefix = "/prod/nightly/".into();
        let d = destination(DestinationKind::S3(cfg));
        assert_eq!(d.key_for("app_2026.sql.gz"), "prod/nightly/app_2026.sql.gz");
        assert_eq!(d.list_prefix(), "prod/nightly/");
    }

    #[test]
    fn an_empty_prefix_puts_objects_at_the_bucket_root() {
        let mut cfg = s3();
        cfg.prefix = String::new();
        let d = destination(DestinationKind::S3(cfg));
        assert_eq!(d.key_for("app.sql.gz"), "app.sql.gz");
        assert_eq!(
            d.list_prefix(),
            "",
            "an empty prefix must not become \"/\", which matches nothing"
        );
    }

    #[test]
    fn manifests_are_not_counted_as_artifacts() {
        // Off-site retention keeps N artifacts. Counting each manifest as one
        // would halve what is actually kept.
        assert!(is_artifact_key("prod/app.sql.gz"));
        assert!(is_artifact_key("prod/app.dump"));
        assert!(!is_artifact_key("prod/app.sql.gz.manifest.json"));
        assert!(!is_artifact_key("prod/notes.txt"));
    }

    // ── The secret boundary ─────────────────────────────────────────────

    #[test]
    fn a_serialised_destination_carries_no_secret() {
        // The property the whole module rests on: this type is safe to log,
        // list and export because there is nothing in it to redact.
        let d = destination(DestinationKind::S3(s3()));
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("secret"), "{json}");
        assert!(
            json.contains("AKIDEXAMPLE"),
            "the key id is not secret and identifies the credential: {json}"
        );
    }

    #[test]
    fn the_config_handed_to_the_client_trims_stray_whitespace() {
        // Pasted from a console, these routinely arrive with a trailing space,
        // which becomes part of a signed header and fails as a 403.
        let cfg = S3Destination {
            endpoint: " https://s3.example.com ".into(),
            region: " eu-west-1 ".into(),
            bucket: " backups ".into(),
            prefix: "/prod/".into(),
            path_style: false,
            access_key_id: " AKIDEXAMPLE ".into(),
        };
        let built = cfg.to_config(secrecy::SecretString::from("x"));
        assert_eq!(built.endpoint, "https://s3.example.com");
        assert_eq!(built.region, "eu-west-1");
        assert_eq!(built.bucket, "backups");
        assert_eq!(built.prefix, "prod");
        assert_eq!(built.access_key_id, "AKIDEXAMPLE");
    }

    #[test]
    fn a_destination_describes_itself_without_the_endpoint_host() {
        let d = destination(DestinationKind::S3(s3()));
        assert_eq!(d.kind.describe(), "s3://backups/prod");
    }

    #[test]
    fn an_object_url_does_not_repeat_the_prefix() {
        // `describe()` already ends in the prefix and `key_for` already starts
        // with it. Joining the two gives `s3://backups/prod/prod/app.sql.gz`,
        // which points at nothing and reads as if it does.
        let d = destination(DestinationKind::S3(s3()));
        let key = d.key_for("app.sql.gz");
        assert_eq!(key, "prod/app.sql.gz");
        assert_eq!(d.object_url(&key), "s3://backups/prod/app.sql.gz");
    }

    #[test]
    fn a_stored_destination_without_the_optional_fields_still_loads() {
        // `prefix` and `path_style` are defaulted; a hand-written or older
        // record must not fail to deserialise.
        let json = serde_json::json!({
            "kind": "s3",
            "endpoint": "https://s3.example.com",
            "region": "eu-west-1",
            "bucket": "backups",
            "access_key_id": "AKIDEXAMPLE"
        });
        let kind: DestinationKind = serde_json::from_value(json).expect("must load");
        let DestinationKind::S3(s3) = kind;
        assert_eq!(s3.prefix, "");
        assert!(!s3.path_style);
    }
}
