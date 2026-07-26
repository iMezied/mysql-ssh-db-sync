//! Connection profiles.
//!
//! A profile describes *how to reach* one database server. It never contains a
//! secret: passwords and key passphrases live in the OS keychain, addressed by
//! profile id. See [`crate::secrets`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

use crate::types::{Engine, EnvironmentTag};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SshAuth {
    /// Use the running ssh-agent. Preferred: no key material touches us.
    Agent,
    /// Use a private key file. `passphrase_in_keychain` records whether a
    /// passphrase was stored; the passphrase itself is never in this struct.
    KeyFile {
        path: String,
        passphrase_in_keychain: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SshEndpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SshConfig {
    #[serde(flatten)]
    pub endpoint: SshEndpoint,
    /// Optional single-hop ProxyJump. Chained jumps are out of scope for v1.
    pub jump_host: Option<SshEndpoint>,
}

/// Database coordinates *as seen from the SSH host* (or from this machine when
/// `ConnectionProfile::ssh` is `None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DbConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: Option<String>,
}

/// Per-profile overrides for external client binaries.
///
/// We never bundle Oracle's `mysqldump` (GPLv2); binaries are discovered on the
/// host and may be pointed at explicitly here. See DECISIONS.md.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ToolOverrides {
    pub mysqldump: Option<String>,
    pub mysql: Option<String>,
    pub mydumper: Option<String>,
    pub myloader: Option<String>,
    pub pg_dump: Option<String>,
    pub pg_dumpall: Option<String>,
    pub pg_restore: Option<String>,
    pub psql: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ConnectionProfile {
    pub id: Uuid,
    pub name: String,
    pub engine: Engine,
    pub environment: EnvironmentTag,
    /// `None` means connect directly, without a tunnel.
    pub ssh: Option<SshConfig>,
    pub db: DbConfig,
    pub tool_overrides: ToolOverrides,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ProfileCreate {
    pub name: String,
    pub engine: Engine,
    pub environment: EnvironmentTag,
    pub ssh: Option<SshConfig>,
    pub db: DbConfig,
    #[serde(default)]
    pub tool_overrides: ToolOverrides,
}

/// Patch for an existing profile. An omitted field means "leave unchanged".
///
/// `ssh` is doubly-optional on purpose, and the distinction is carried by
/// *presence*, not by value: omitting the key leaves the SSH config alone,
/// while sending an explicit `null` clears it. Every field is
/// `#[serde(default)]` so that omission is legal and so the generated
/// TypeScript renders them as optional — without it the frontend would be
/// forced to send every key and could never express "leave unchanged".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ProfileUpdate {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub engine: Option<Engine>,
    #[serde(default)]
    pub environment: Option<EnvironmentTag>,
    // The wire type is an *optional field* holding `SshConfig | null`, which is
    // not what `Option<Option<T>>` looks like to specta — the custom
    // deserializer changes the shape, so state it explicitly.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[specta(type = Option<SshConfig>)]
    pub ssh: Option<Option<SshConfig>>,
    #[serde(default)]
    pub db: Option<DbConfig>,
    #[serde(default)]
    pub tool_overrides: Option<ToolOverrides>,
}

/// Deserialise a nullable-but-optional field.
///
/// Serde's default handling collapses an explicit `null` into `None`, which
/// makes `Option<Option<T>>` useless on its own — "absent" and "null" both
/// arrive as `None`. Running the inner `Option<T>` through and wrapping the
/// result in `Some` preserves the distinction: this function only runs when the
/// key is present, so absence still falls through to `Default`.
pub(crate) fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

impl ConnectionProfile {
    /// Stable key used for the known-hosts table.
    pub fn ssh_host_key_id(&self) -> Option<String> {
        self.ssh
            .as_ref()
            .map(|s| format!("{}:{}", s.endpoint.host, s.endpoint.port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GUI sends these patches as JSON, so the omit-vs-null distinction has
    /// to survive deserialisation or "clear the SSH config" becomes
    /// unreachable from the frontend.
    #[test]
    fn omitted_ssh_field_means_leave_unchanged() {
        let patch: ProfileUpdate = serde_json::from_str(r#"{"name":"renamed"}"#).unwrap();
        assert_eq!(patch.name.as_deref(), Some("renamed"));
        assert_eq!(patch.ssh, None, "omitted key must mean 'leave unchanged'");
    }

    #[test]
    fn explicit_null_ssh_field_means_clear() {
        let patch: ProfileUpdate = serde_json::from_str(r#"{"ssh":null}"#).unwrap();
        assert_eq!(
            patch.ssh,
            Some(None),
            "an explicit null must mean 'clear the SSH config'"
        );
    }

    #[test]
    fn explicit_ssh_value_means_replace() {
        let json = r#"{
            "ssh": {
                "host": "db.example.com",
                "port": 22,
                "user": "ubuntu",
                "auth": { "kind": "agent" },
                "jump_host": null
            }
        }"#;
        let patch: ProfileUpdate = serde_json::from_str(json).unwrap();
        let ssh = patch.ssh.expect("present").expect("not cleared");
        assert_eq!(ssh.endpoint.host, "db.example.com");
        assert_eq!(ssh.endpoint.auth, SshAuth::Agent);
        assert!(ssh.jump_host.is_none());
    }

    #[test]
    fn empty_patch_changes_nothing() {
        let patch: ProfileUpdate = serde_json::from_str("{}").unwrap();
        assert_eq!(patch, ProfileUpdate::default());
    }

    #[test]
    fn ssh_config_flattens_the_endpoint() {
        // `SshConfig` flattens `SshEndpoint`, so host/port/user sit at the top
        // level rather than nested — the stored JSON depends on this shape.
        let cfg = SshConfig {
            endpoint: SshEndpoint {
                host: "h".into(),
                port: 22,
                user: "u".into(),
                auth: SshAuth::Agent,
            },
            jump_host: None,
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["host"], "h");
        assert_eq!(json["port"], 22);

        let back: SshConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn key_file_auth_round_trips_without_the_passphrase() {
        let auth = SshAuth::KeyFile {
            path: "~/.ssh/id_ed25519".into(),
            passphrase_in_keychain: true,
        };
        let json = serde_json::to_string(&auth).unwrap();
        assert!(
            !json.contains("passphrase\":\""),
            "the passphrase itself must never be serialised into the profile"
        );
        assert_eq!(serde_json::from_str::<SshAuth>(&json).unwrap(), auth);
    }
}
