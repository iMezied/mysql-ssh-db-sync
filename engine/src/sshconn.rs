//! Saved SSH connections.
//!
//! An SSH server is a *thing on the network*, not a property of one database.
//! The same bastion commonly fronts a dozen databases, and when its address,
//! user or key changes it changes for all of them at once. Storing the tunnel
//! inside each connection profile — as this application did until now — meant
//! that fact was represented by copies, and a copy is something you can forget
//! to update.
//!
//! So an SSH connection is its own record with its own name, and a profile
//! holds a reference to one. Editing it in one place changes every database
//! reached through it, which is the entire point.
//!
//! # Secrets
//!
//! As everywhere else, no secret is in these types. A key-file *path* is
//! configuration; the key it points at stays on disk, and the passphrase
//! protecting it lives in the OS keychain under
//! [`crate::secrets::SecretKind::SshKeyPassphrase`] keyed by the **SSH
//! connection's** id. That keying is what makes the sharing real: the
//! passphrase moved with the record, so a bastion used by six profiles has one
//! keychain entry rather than six.
//!
//! # Jump hosts are references, not copies
//!
//! A connection may name another saved connection as its bastion. The
//! alternative — embedding the jump host's fields — would push the same
//! duplication one level down, which is the problem this module exists to
//! solve.
//!
//! Chained jumps remain out of scope: a connection used as a jump host may not
//! have a jump host of its own. That is enforced at write time
//! ([`crate::store::Store::create_ssh_connection`]) rather than at connect
//! time, so an impossible route cannot be stored and then fail at 3am.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

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

/// A route to an SSH server with its bastion resolved: what [`crate::ssh`]
/// needs in order to actually connect.
///
/// This is a *derived* value, built by [`crate::store::Store::resolve_ssh`]
/// from a stored [`SshConnection`] and the connection it jumps through. It is
/// deliberately not persisted in this shape any more.
///
/// It does still describe the layout of the `profiles.ssh_config` column that
/// versions before saved SSH connections wrote, which is why the endpoint stays
/// flattened: [`adopt_legacy_configs`] parses those rows with this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SshConfig {
    #[serde(flatten)]
    pub endpoint: SshEndpoint,
    /// Optional single-hop ProxyJump. Chained jumps are out of scope for v1.
    pub jump_host: Option<SshEndpoint>,
}

/// A named SSH server, reusable by any number of connection profiles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SshConnection {
    pub id: Uuid,
    /// Unique, and what every profile and the audit log refer to it by.
    pub name: String,
    pub endpoint: SshEndpoint,
    /// Another saved connection used as a bastion. See the module docs for why
    /// this is a reference.
    pub jump_host_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SshConnectionCreate {
    pub name: String,
    pub endpoint: SshEndpoint,
    #[serde(default)]
    pub jump_host_id: Option<Uuid>,
}

/// A partial edit. An omitted field means "leave unchanged".
///
/// `jump_host_id` is doubly-optional and carries the distinction by *presence*,
/// exactly as [`crate::profile::ProfileUpdate::ssh_connection_id`] does:
/// omitting the key keeps the current bastion, an explicit `null` removes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SshConnectionUpdate {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub endpoint: Option<SshEndpoint>,
    #[serde(
        default,
        deserialize_with = "crate::profile::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[specta(type = Option<Uuid>)]
    pub jump_host_id: Option<Option<Uuid>>,
}

#[derive(Debug, thiserror::Error)]
pub enum SshConnectionError {
    #[error("an SSH connection needs a name")]
    NoName,
    #[error("an SSH connection needs a host")]
    NoHost,
    #[error("an SSH connection needs a user")]
    NoUser,
    #[error("{0} is not a usable port")]
    BadPort(u16),
    #[error("a key-file connection needs the path to the key")]
    NoKeyPath,
    #[error("an SSH connection cannot be its own jump host")]
    JumpsToItself,
    #[error(
        "{name:?} reaches its own server through a jump host, so it cannot also be used as one — \
         chained jumps are not supported"
    )]
    ChainedJump { name: String },
    #[error(
        "{name:?} is already used as a jump host by {used_by}, so it cannot route through one \
         itself — chained jumps are not supported"
    )]
    WouldChainJump { name: String, used_by: String },
    #[error("no SSH connection with id {0}")]
    NotFound(Uuid),
    #[error("{name:?} is still used by {used_by}")]
    InUse { name: String, used_by: String },
}

/// An SSH connection with its bastion looked up: everything needed to open a
/// tunnel, and the ids needed to find the passphrases for each hop.
#[derive(Debug, Clone)]
pub struct ResolvedSsh {
    pub connection: SshConnection,
    pub jump_host: Option<SshConnection>,
    /// The two above, in the shape [`crate::ssh::TunnelProvider`] consumes.
    pub config: SshConfig,
}

impl SshEndpoint {
    /// Reject anything that could never connect, before it is stored.
    pub fn validate(&self) -> Result<(), SshConnectionError> {
        if self.host.trim().is_empty() {
            return Err(SshConnectionError::NoHost);
        }
        if self.user.trim().is_empty() {
            return Err(SshConnectionError::NoUser);
        }
        if self.port == 0 {
            return Err(SshConnectionError::BadPort(self.port));
        }
        if let SshAuth::KeyFile { path, .. } = &self.auth
            && path.trim().is_empty()
        {
            return Err(SshConnectionError::NoKeyPath);
        }
        Ok(())
    }

    /// Stable key for the pinned host-key table.
    pub fn host_key_id(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Whether connecting needs a passphrase out of the keychain.
    ///
    /// Asking the keychain when the answer is no is not free: on macOS it can
    /// raise an authorisation prompt for a secret that was never stored.
    pub fn needs_passphrase(&self) -> bool {
        matches!(
            self.auth,
            SshAuth::KeyFile {
                passphrase_in_keychain: true,
                ..
            }
        )
    }

    /// `user@host` — or `user@host:port` when the port is not the default.
    pub fn describe(&self) -> String {
        if self.port == DEFAULT_SSH_PORT {
            format!("{}@{}", self.user, self.host)
        } else {
            format!("{}@{}:{}", self.user, self.host, self.port)
        }
    }
}

pub const DEFAULT_SSH_PORT: u16 = 22;

impl SshConnectionCreate {
    pub fn validate(&self) -> Result<(), SshConnectionError> {
        if self.name.trim().is_empty() {
            return Err(SshConnectionError::NoName);
        }
        self.endpoint.validate()
    }
}

impl SshConnection {
    pub fn host_key_id(&self) -> String {
        self.endpoint.host_key_id()
    }

    /// One line naming where this points, for lists and logs.
    pub fn describe(&self) -> String {
        self.endpoint.describe()
    }
}

// ── Adopting configurations written before SSH connections existed ──────

use crate::secrets::{self, SecretKind};
use crate::store::{Store, StoreError};

/// One profile's embedded SSH config, turned into a saved connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    pub profile_id: Uuid,
    pub profile_name: String,
    pub ssh_connection_id: Uuid,
    pub ssh_connection_name: String,
    /// True when this profile's config produced a new record rather than
    /// matching one an earlier profile had already contributed.
    pub created: bool,
}

/// Convert every remaining embedded SSH config into a saved connection.
///
/// Runs at startup in both the app and the CLI, and does nothing at all once
/// there is nothing left to adopt — which is the normal case, so it costs one
/// query.
///
/// # Why this is not a SQL migration
///
/// Two of the three things it has to do are beyond a migration file. It has to
/// *deduplicate*: three profiles behind one bastion should end up sharing one
/// record, or the upgrade would hand the user three copies of the thing this
/// change exists to stop them having. And it has to move the key passphrase in
/// the OS keychain from the profile's id to the new connection's, or a
/// tunnelled profile would come back from the upgrade unable to authenticate
/// with no visible reason why.
///
/// # Why a failed keychain move is not fatal
///
/// The passphrase is re-enterable; the configuration is not. If the keychain is
/// locked or unavailable, the records are still adopted and the user is asked
/// for the passphrase again — the alternative is refusing to start.
pub async fn adopt_legacy_configs(store: &Store) -> Result<Vec<Adoption>, StoreError> {
    let legacy = store.legacy_ssh_configs().await?;
    if legacy.is_empty() {
        return Ok(Vec::new());
    }

    let mut existing = store.list_ssh_connections().await?;
    let mut adoptions = Vec::new();

    for (profile_id, profile_name, config) in legacy {
        // A jump host is adopted first: the connection that jumps through it
        // cannot be written until it has an id to point at.
        let jump_host_id = match &config.jump_host {
            Some(endpoint) => Some(
                adopt_endpoint(store, &mut existing, endpoint, None)
                    .await?
                    .0,
            ),
            None => None,
        };

        let (ssh_connection_id, created) =
            adopt_endpoint(store, &mut existing, &config.endpoint, jump_host_id).await?;

        store
            .attach_ssh_connection(profile_id, ssh_connection_id)
            .await?;

        // The passphrase was filed under the profile; it belongs to the server
        // now. Copy before deleting, so an interrupted move loses nothing.
        if config.endpoint.needs_passphrase()
            || config
                .jump_host
                .as_ref()
                .is_some_and(SshEndpoint::needs_passphrase)
        {
            move_passphrase(profile_id, ssh_connection_id);
        }

        adoptions.push(Adoption {
            profile_id,
            profile_name,
            ssh_connection_id,
            ssh_connection_name: existing
                .iter()
                .find(|c| c.id == ssh_connection_id)
                .map(|c| c.name.clone())
                .unwrap_or_default(),
            created,
        });
    }

    Ok(adoptions)
}

/// Find or create the saved connection matching `endpoint`.
///
/// Matching is on the endpoint and its bastion, not on the name: the same
/// server typed into three profiles is one server, and giving the user three
/// identically-configured records would defeat the point.
///
/// `existing` is the caller's view of what is already stored and is extended
/// with anything created, so a run adopting several configs converges on one
/// record per server rather than one per config.
pub async fn adopt_endpoint(
    store: &Store,
    existing: &mut Vec<SshConnection>,
    endpoint: &SshEndpoint,
    jump_host_id: Option<Uuid>,
) -> Result<(Uuid, bool), StoreError> {
    if let Some(found) = existing
        .iter()
        .find(|c| &c.endpoint == endpoint && c.jump_host_id == jump_host_id)
    {
        return Ok((found.id, false));
    }

    let created = store
        .create_ssh_connection(SshConnectionCreate {
            name: unique_name(existing, &endpoint.describe()),
            endpoint: endpoint.clone(),
            jump_host_id,
        })
        .await?;

    let id = created.id;
    existing.push(created);
    Ok((id, true))
}

/// `base`, or `base (2)`, `base (3)`… until it is not taken.
///
/// Names collide when two profiles reach the same address as the same user but
/// authenticate differently — rare, but a unique-constraint failure during an
/// upgrade would be a very bad way to find out.
fn unique_name(existing: &[SshConnection], base: &str) -> String {
    if !existing.iter().any(|c| c.name == base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} ({n})"))
        .find(|candidate| !existing.iter().any(|c| &c.name == candidate))
        .expect("an unused suffix always exists")
}

/// Re-file a stored key passphrase from a profile onto its SSH connection.
fn move_passphrase(from_profile: Uuid, to_connection: Uuid) {
    let existing = match secrets::get_secret(from_profile, SecretKind::SshKeyPassphrase) {
        Ok(Some(value)) => value,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(
                "could not read the stored SSH passphrase while adopting a saved connection; \
                 it will have to be entered again: {e}"
            );
            return;
        }
    };

    use secrecy::ExposeSecret;
    if let Err(e) = secrets::set_secret(
        to_connection,
        SecretKind::SshKeyPassphrase,
        existing.expose_secret(),
    ) {
        tracing::warn!("could not move the SSH passphrase to its saved connection: {e}");
        return;
    }

    if let Err(e) = secrets::set_secret(from_profile, SecretKind::SshKeyPassphrase, "") {
        // Harmless: the copy that matters is written. Left behind rather than
        // retried, because the profile's entry is no longer read by anything.
        tracing::debug!("could not clear the old SSH passphrase entry: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> SshEndpoint {
        SshEndpoint {
            host: "bastion.example.com".into(),
            port: 22,
            user: "ops".into(),
            auth: SshAuth::Agent,
        }
    }

    fn connection(name: &str) -> SshConnection {
        SshConnection {
            id: Uuid::new_v4(),
            name: name.into(),
            endpoint: endpoint(),
            jump_host_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn an_endpoint_without_a_host_is_refused() {
        let mut e = endpoint();
        e.host = "  ".into();
        assert!(matches!(
            e.validate(),
            Err(SshConnectionError::NoHost)
        ));
    }

    #[test]
    fn an_endpoint_without_a_user_is_refused() {
        let mut e = endpoint();
        e.user = String::new();
        assert!(matches!(e.validate(), Err(SshConnectionError::NoUser)));
    }

    #[test]
    fn a_key_file_without_a_path_is_refused() {
        // Storing one would produce a connection that fails only when used.
        let mut e = endpoint();
        e.auth = SshAuth::KeyFile {
            path: String::new(),
            passphrase_in_keychain: false,
        };
        assert!(matches!(e.validate(), Err(SshConnectionError::NoKeyPath)));
    }

    #[test]
    fn a_connection_needs_a_name() {
        let input = SshConnectionCreate {
            name: " ".into(),
            endpoint: endpoint(),
            jump_host_id: None,
        };
        assert!(matches!(
            input.validate(),
            Err(SshConnectionError::NoName)
        ));
    }

    #[test]
    fn the_default_port_is_left_out_of_the_description() {
        // The generated name is what the user sees after an upgrade, so it
        // should read like something they would have typed.
        assert_eq!(endpoint().describe(), "ops@bastion.example.com");

        let mut e = endpoint();
        e.port = 2222;
        assert_eq!(e.describe(), "ops@bastion.example.com:2222");
    }

    #[test]
    fn only_a_key_file_with_a_stored_passphrase_needs_the_keychain() {
        assert!(!endpoint().needs_passphrase());

        let mut e = endpoint();
        e.auth = SshAuth::KeyFile {
            path: "~/.ssh/id_ed25519".into(),
            passphrase_in_keychain: false,
        };
        assert!(!e.needs_passphrase(), "no passphrase was ever stored");

        e.auth = SshAuth::KeyFile {
            path: "~/.ssh/id_ed25519".into(),
            passphrase_in_keychain: true,
        };
        assert!(e.needs_passphrase());
    }

    #[test]
    fn generated_names_do_not_collide() {
        let existing = vec![connection("ops@bastion.example.com")];
        assert_eq!(
            unique_name(&existing, "ops@bastion.example.com"),
            "ops@bastion.example.com (2)"
        );
        assert_eq!(unique_name(&existing, "ops@other"), "ops@other");
    }

    #[test]
    fn generated_names_keep_counting_past_the_second_collision() {
        let existing = vec![
            connection("ops@bastion.example.com"),
            connection("ops@bastion.example.com (2)"),
        ];
        assert_eq!(
            unique_name(&existing, "ops@bastion.example.com"),
            "ops@bastion.example.com (3)"
        );
    }

    #[test]
    fn the_legacy_column_shape_still_parses() {
        // What versions before saved connections wrote into
        // `profiles.ssh_config`. Adoption reads exactly this, so the flattened
        // layout is load-bearing rather than incidental.
        let raw = r#"{
            "host": "bastion.example.com",
            "port": 2222,
            "user": "ops",
            "auth": { "kind": "key_file", "path": "~/.ssh/id_ed25519",
                      "passphrase_in_keychain": true },
            "jump_host": {
                "host": "edge.example.com",
                "port": 22,
                "user": "jump",
                "auth": { "kind": "agent" }
            }
        }"#;

        let parsed: SshConfig = serde_json::from_str(raw).expect("legacy row must still parse");
        assert_eq!(parsed.endpoint.host, "bastion.example.com");
        assert_eq!(parsed.endpoint.port, 2222);
        assert!(parsed.endpoint.needs_passphrase());
        assert_eq!(parsed.jump_host.expect("jump host").user, "jump");
    }

    #[test]
    fn a_key_file_round_trips_without_the_passphrase() {
        let auth = SshAuth::KeyFile {
            path: "~/.ssh/id_ed25519".into(),
            passphrase_in_keychain: true,
        };
        let json = serde_json::to_string(&auth).unwrap();
        assert!(
            !json.contains("passphrase\":\""),
            "the passphrase itself must never be serialised"
        );
        assert_eq!(serde_json::from_str::<SshAuth>(&json).unwrap(), auth);
    }

    #[test]
    fn an_omitted_jump_host_differs_from_an_explicit_null() {
        // The GUI sends these as JSON; without the distinction "detach the
        // bastion" would be unreachable from the frontend.
        let keep: SshConnectionUpdate = serde_json::from_str(r#"{"name":"renamed"}"#).unwrap();
        assert_eq!(keep.jump_host_id, None, "omitted means leave alone");

        let clear: SshConnectionUpdate = serde_json::from_str(r#"{"jump_host_id":null}"#).unwrap();
        assert_eq!(clear.jump_host_id, Some(None), "null means detach");
    }
}
