//! Connection profiles.
//!
//! A profile describes *how to reach* one database server. It never contains a
//! secret: the database password lives in the OS keychain, addressed by profile
//! id. See [`crate::secrets`].
//!
//! It does not describe the *tunnel* either. An SSH server is shared between
//! databases far more often than not, so it is a record of its own —
//! [`crate::sshconn`] — and a profile names one.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

use crate::types::{Engine, EnvironmentTag};

/// Database coordinates *as seen from the SSH host* (or from this machine when
/// `ConnectionProfile::ssh_connection_id` is `None`).
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
    /// The saved SSH connection this profile tunnels through. `None` means
    /// connect directly, without a tunnel.
    pub ssh_connection_id: Option<Uuid>,
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
    #[serde(default)]
    pub ssh_connection_id: Option<Uuid>,
    pub db: DbConfig,
    #[serde(default)]
    pub tool_overrides: ToolOverrides,
}

/// Patch for an existing profile. An omitted field means "leave unchanged".
///
/// `ssh_connection_id` is doubly-optional on purpose, and the distinction is
/// carried by *presence*, not by value: omitting the key leaves the tunnel
/// alone, while sending an explicit `null` detaches it. Every field is
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
    // The wire type is an *optional field* holding `Uuid | null`, which is not
    // what `Option<Option<T>>` looks like to specta — the custom deserializer
    // changes the shape, so state it explicitly.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[specta(type = Option<Uuid>)]
    pub ssh_connection_id: Option<Option<Uuid>>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The GUI sends these patches as JSON, so the omit-vs-null distinction has
    /// to survive deserialisation or "stop tunnelling this profile" becomes
    /// unreachable from the frontend.
    #[test]
    fn omitted_ssh_field_means_leave_unchanged() {
        let patch: ProfileUpdate = serde_json::from_str(r#"{"name":"renamed"}"#).unwrap();
        assert_eq!(patch.name.as_deref(), Some("renamed"));
        assert_eq!(
            patch.ssh_connection_id, None,
            "omitted key must mean 'leave unchanged'"
        );
    }

    #[test]
    fn explicit_null_ssh_field_means_detach() {
        let patch: ProfileUpdate = serde_json::from_str(r#"{"ssh_connection_id":null}"#).unwrap();
        assert_eq!(
            patch.ssh_connection_id,
            Some(None),
            "an explicit null must mean 'connect directly from now on'"
        );
    }

    #[test]
    fn explicit_ssh_value_means_replace() {
        let id = Uuid::new_v4();
        let patch: ProfileUpdate =
            serde_json::from_str(&format!(r#"{{"ssh_connection_id":"{id}"}}"#)).unwrap();
        assert_eq!(patch.ssh_connection_id, Some(Some(id)));
    }

    #[test]
    fn empty_patch_changes_nothing() {
        let patch: ProfileUpdate = serde_json::from_str("{}").unwrap();
        assert_eq!(patch, ProfileUpdate::default());
    }
}
