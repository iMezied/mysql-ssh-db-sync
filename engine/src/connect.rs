//! Opening a working connection to a profile.
//!
//! Ties together the three things that have to succeed before any real work can
//! start: resolving secrets from the keychain, standing up an SSH tunnel, and
//! connecting a database client through it. Lives in the engine so the CLI and
//! the GUI share one implementation — and one definition of what "connected"
//! means.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

use crate::db::{ConnectParams, Introspector, connect as db_connect};
use crate::profile::ConnectionProfile;
use crate::secrets::{self, SecretKind};
use crate::ssh::{
    HopCredentials, RusshTunnelProvider, SshCredentials, StoreHostKeyVerifier, TunnelError,
    TunnelHandle, TunnelProvider,
};
use crate::sshconn::ResolvedSsh;
use crate::store::Store;

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error(transparent)]
    Tunnel(#[from] TunnelError),
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    #[error("keychain error: {0}")]
    Secrets(#[from] secrets::SecretError),
    #[error("store error: {0}")]
    Store(#[from] crate::store::StoreError),
}

/// A live connection to a profile.
///
/// The tunnel handle is held alongside the client: dropping this struct closes
/// the tunnel, so a connection cannot outlive its transport.
pub struct ProfileConnection {
    pub introspector: Box<dyn Introspector>,
    /// `None` for a direct (non-tunnelled) profile.
    pub tunnel: Option<TunnelHandle>,
}

impl ProfileConnection {
    pub async fn close(self) {
        self.introspector.close().await;
        if let Some(t) = self.tunnel {
            t.close();
        }
    }
}

/// Read an SSH connection's secrets out of the keychain.
///
/// Each hop is looked up under its *own* connection id. Before saved
/// connections existed both hops shared the profile's single entry, so a
/// bastion and the server behind it could not have different passphrases; now
/// they are different records and each carries its own.
pub fn resolve_credentials(resolved: &ResolvedSsh) -> Result<SshCredentials, ConnectError> {
    let mut creds = SshCredentials::default();

    // Only fetch a passphrase when the connection says a key file needs one;
    // asking otherwise can trigger a pointless keychain prompt.
    if resolved.connection.endpoint.needs_passphrase() {
        creds.endpoint = HopCredentials {
            key_passphrase: secrets::get_secret(
                resolved.connection.id,
                SecretKind::SshKeyPassphrase,
            )?,
        };
    }

    if let Some(jump) = &resolved.jump_host
        && jump.endpoint.needs_passphrase()
    {
        creds.jump_host = HopCredentials {
            key_passphrase: secrets::get_secret(jump.id, SecretKind::SshKeyPassphrase)?,
        };
    }

    Ok(creds)
}

/// Open a tunnel for the profile, if it needs one.
pub async fn open_tunnel(
    profile: &ConnectionProfile,
    store: &Store,
) -> Result<Option<TunnelHandle>, ConnectError> {
    let Some(ssh_id) = profile.ssh_connection_id else {
        return Ok(None);
    };

    let resolved = store.resolve_ssh(ssh_id).await?;
    let credentials = resolve_credentials(&resolved)?;
    let verifier = Arc::new(StoreHostKeyVerifier::new(store.clone()));
    let provider = RusshTunnelProvider::new(verifier);

    let handle = provider
        .open(
            &resolved.config,
            &credentials,
            &profile.db.host,
            profile.db.port,
        )
        .await?;

    Ok(Some(handle))
}

/// Connect and authenticate a saved SSH connection, without a database.
///
/// This is what makes an SSH connection testable on its own terms. Reaching a
/// bastion is a different question from reaching a database through it, and
/// answering them separately is the point of the record existing.
pub async fn test_ssh_connection(id: Uuid, store: &Store) -> SshReport {
    let resolved = match store.resolve_ssh(id).await {
        Ok(r) => r,
        Err(e) => {
            return SshReport {
                ssh: StepOutcome::failed(e.to_string()),
                host_key: None,
                authenticated_as: None,
                host_key_prompt: None,
            };
        }
    };

    let credentials = match resolve_credentials(&resolved) {
        Ok(c) => c,
        Err(e) => {
            return SshReport {
                ssh: StepOutcome::failed(format!("could not read the keychain: {e}")),
                host_key: None,
                authenticated_as: None,
                host_key_prompt: None,
            };
        }
    };

    let provider = RusshTunnelProvider::new(Arc::new(StoreHostKeyVerifier::new(store.clone())));
    match provider.probe(&resolved.config, &credentials).await {
        Ok(probe) => SshReport {
            ssh: StepOutcome::ok(match &resolved.jump_host {
                Some(jump) => format!("authenticated, through {}", jump.name),
                None => "authenticated".to_string(),
            }),
            host_key: Some(probe.host_key.fingerprint),
            authenticated_as: Some(probe.authenticated_as),
            host_key_prompt: None,
        },
        Err(e) => {
            let error = ConnectError::Tunnel(e);
            SshReport {
                ssh: StepOutcome::failed(error.to_string()),
                host_key: None,
                authenticated_as: None,
                // The fingerprint is the whole content of a first-contact
                // failure; dropping it here would leave no way to accept it.
                host_key_prompt: host_key_prompt_from(&error, store).await,
            }
        }
    }
}

/// Open a full connection: tunnel (if configured) plus a database client.
pub async fn open(
    profile: &ConnectionProfile,
    store: &Store,
    database: Option<&str>,
) -> Result<ProfileConnection, ConnectError> {
    let tunnel = open_tunnel(profile, store).await?;

    // With a tunnel, the client talks to the local end of it; without one it
    // talks to the configured host directly.
    let (host, port) = match &tunnel {
        Some(t) => ("127.0.0.1".to_string(), t.local_port()),
        None => (profile.db.host.clone(), profile.db.port),
    };

    let params = ConnectParams {
        engine: profile.engine,
        host,
        port,
        user: profile.db.user.clone(),
        password: secrets::get_secret(profile.id, SecretKind::DbPassword)?,
        database: database
            .map(str::to_string)
            .or_else(|| profile.db.database.clone()),
    };

    let introspector = db_connect(&params).await?;
    Ok(ProfileConnection {
        introspector,
        tunnel,
    })
}

// ── Test-connection reporting ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum StepOutcome {
    Ok { detail: String },
    Failed { detail: String },
    Skipped { detail: String },
}

impl StepOutcome {
    fn ok(detail: impl Into<String>) -> Self {
        Self::Ok {
            detail: detail.into(),
        }
    }
    fn failed(detail: impl Into<String>) -> Self {
        Self::Failed {
            detail: detail.into(),
        }
    }
    fn skipped(detail: impl Into<String>) -> Self {
        Self::Skipped {
            detail: detail.into(),
        }
    }

    pub const fn is_ok(&self) -> bool {
        matches!(self, StepOutcome::Ok { .. })
    }
    pub const fn is_failed(&self) -> bool {
        matches!(self, StepOutcome::Failed { .. })
    }
}

/// An unverified host key the user must decide about.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct HostKeyPrompt {
    pub host_port: String,
    pub algorithm: String,
    pub fingerprint: String,
    /// True when a *different* key was previously pinned. Far more serious
    /// than first contact: it is what a machine-in-the-middle looks like.
    pub changed: bool,
    /// The fingerprint we had pinned, when `changed`.
    pub previous_fingerprint: Option<String>,
}

/// Result of testing a saved SSH connection on its own.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SshReport {
    pub ssh: StepOutcome,
    /// Fingerprint of the key the server presented, once it is trusted.
    pub host_key: Option<String>,
    pub authenticated_as: Option<String>,
    pub host_key_prompt: Option<HostKeyPrompt>,
}

impl SshReport {
    pub fn succeeded(&self) -> bool {
        self.ssh.is_ok()
    }
}

/// Step-by-step result of testing a profile.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ConnectionReport {
    pub ssh: StepOutcome,
    pub tunnel: StepOutcome,
    pub db_ping: StepOutcome,
    pub catalog_read: StepOutcome,
    pub server_version: Option<String>,
    /// Present when the connection stopped on an unverified host key. The UI
    /// shows the fingerprint and, if the user accepts, pins it and retries.
    pub host_key_prompt: Option<HostKeyPrompt>,
}

impl ConnectionReport {
    pub fn succeeded(&self) -> bool {
        !self.ssh.is_failed()
            && !self.tunnel.is_failed()
            && self.db_ping.is_ok()
            && !self.catalog_read.is_failed()
    }
}

/// Test a profile end to end, reporting each step rather than a single boolean.
///
/// A bare "connection failed" is close to useless when four different things
/// could be wrong; a DBA needs to know *which*.
pub async fn test_connection(profile: &ConnectionProfile, store: &Store) -> ConnectionReport {
    let mut report = ConnectionReport {
        ssh: StepOutcome::skipped("profile connects directly; no SSH step"),
        tunnel: StepOutcome::skipped("no tunnel needed"),
        db_ping: StepOutcome::failed("not attempted"),
        catalog_read: StepOutcome::skipped("not attempted"),
        server_version: None,
        host_key_prompt: None,
    };

    let tunnel = if profile.ssh_connection_id.is_some() {
        match open_tunnel(profile, store).await {
            Ok(handle) => {
                // Named, not just "authenticated": with connections shared
                // between profiles, *which* one was used is the thing a reader
                // cannot infer from the profile in front of them.
                report.ssh = StepOutcome::ok(match ssh_connection_name(profile, store).await {
                    Some(name) => format!("authenticated to {name}"),
                    None => "authenticated".to_string(),
                });
                match &handle {
                    Some(t) => {
                        report.tunnel = StepOutcome::ok(format!(
                            "forwarding 127.0.0.1:{} to {}:{}",
                            t.local_port(),
                            profile.db.host,
                            profile.db.port
                        ));
                    }
                    None => report.tunnel = StepOutcome::skipped("no tunnel needed"),
                }
                handle
            }
            Err(e) => {
                report.ssh = StepOutcome::failed(e.to_string());
                report.tunnel = StepOutcome::skipped("SSH did not connect");
                report.db_ping = StepOutcome::skipped("SSH did not connect");
                report.host_key_prompt = host_key_prompt_from(&e, store).await;
                return report;
            }
        }
    } else {
        None
    };

    let (host, port) = match &tunnel {
        Some(t) => ("127.0.0.1".to_string(), t.local_port()),
        None => (profile.db.host.clone(), profile.db.port),
    };

    let password = match secrets::get_secret(profile.id, SecretKind::DbPassword) {
        Ok(p) => p,
        Err(e) => {
            report.db_ping = StepOutcome::failed(format!("could not read the keychain: {e}"));
            return report;
        }
    };

    let params = ConnectParams {
        engine: profile.engine,
        host,
        port,
        user: profile.db.user.clone(),
        password,
        database: profile.db.database.clone(),
    };

    match db_connect(&params).await {
        Ok(introspector) => {
            report.db_ping = StepOutcome::ok(format!("connected as {}", profile.db.user));

            match introspector.server_info().await {
                Ok(info) => {
                    report.server_version = Some(info.version.clone());
                    report.catalog_read = if info.can_read_catalog {
                        StepOutcome::ok("catalog is readable")
                    } else {
                        // Connecting is not enough: without catalog access we
                        // cannot list tables or verify a restore.
                        StepOutcome::failed(
                            "connected, but this user cannot read the catalog; \
                             table listing and verification will not work"
                                .to_string(),
                        )
                    };
                }
                Err(e) => report.catalog_read = StepOutcome::failed(e.to_string()),
            }

            introspector.close().await;
        }
        Err(e) => report.db_ping = StepOutcome::failed(e.to_string()),
    }

    if let Some(t) = tunnel {
        t.close();
    }

    report
}

/// The name of the SSH connection a profile tunnels through, if it has one.
async fn ssh_connection_name(profile: &ConnectionProfile, store: &Store) -> Option<String> {
    let id = profile.ssh_connection_id?;
    store
        .get_ssh_connection(id)
        .await
        .ok()
        .flatten()
        .map(|c| c.name)
}

/// Turn a host-key failure into something the UI can prompt about.
async fn host_key_prompt_from(error: &ConnectError, store: &Store) -> Option<HostKeyPrompt> {
    let ConnectError::Tunnel(tunnel_error) = error else {
        return None;
    };

    match tunnel_error {
        TunnelError::HostKeyUnknown {
            host_port,
            algorithm,
            fingerprint,
        } => Some(HostKeyPrompt {
            host_port: host_port.clone(),
            algorithm: algorithm.clone(),
            fingerprint: fingerprint.clone(),
            changed: false,
            previous_fingerprint: None,
        }),
        TunnelError::HostKeyChanged {
            host_port,
            expected,
            actual,
        } => {
            let algorithm = store
                .get_known_host(host_port)
                .await
                .ok()
                .flatten()
                .map(|(kind, _)| kind)
                .unwrap_or_else(|| "unknown".into());

            Some(HostKeyPrompt {
                host_port: host_port.clone(),
                algorithm,
                fingerprint: actual.clone(),
                changed: true,
                previous_fingerprint: Some(expected.clone()),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_needs_a_successful_ping_to_count_as_connected() {
        let mut report = ConnectionReport {
            ssh: StepOutcome::skipped("direct"),
            tunnel: StepOutcome::skipped("direct"),
            db_ping: StepOutcome::ok("connected"),
            catalog_read: StepOutcome::ok("readable"),
            server_version: Some("8.0.42".into()),
            host_key_prompt: None,
        };
        assert!(report.succeeded());

        report.db_ping = StepOutcome::failed("access denied");
        assert!(!report.succeeded());
    }

    #[test]
    fn an_unreadable_catalog_fails_the_report() {
        // Connecting but not being able to list tables is not a usable state.
        let report = ConnectionReport {
            ssh: StepOutcome::skipped("direct"),
            tunnel: StepOutcome::skipped("direct"),
            db_ping: StepOutcome::ok("connected"),
            catalog_read: StepOutcome::failed("no permission"),
            server_version: Some("8.0.42".into()),
            host_key_prompt: None,
        };
        assert!(!report.succeeded());
    }

    #[test]
    fn skipped_steps_do_not_fail_the_report() {
        let report = ConnectionReport {
            ssh: StepOutcome::skipped("direct"),
            tunnel: StepOutcome::skipped("direct"),
            db_ping: StepOutcome::ok("connected"),
            catalog_read: StepOutcome::skipped("not attempted"),
            server_version: None,
            host_key_prompt: None,
        };
        assert!(report.succeeded());
    }

    #[tokio::test]
    async fn unknown_host_key_becomes_a_first_contact_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).await.unwrap();

        let err = ConnectError::Tunnel(TunnelError::HostKeyUnknown {
            host_port: "db:22".into(),
            algorithm: "ssh-ed25519".into(),
            fingerprint: "SHA256:new".into(),
        });

        let prompt = host_key_prompt_from(&err, &store).await.expect("prompt");
        assert!(!prompt.changed);
        assert_eq!(prompt.fingerprint, "SHA256:new");
        assert!(prompt.previous_fingerprint.is_none());
    }

    #[tokio::test]
    async fn changed_host_key_prompt_carries_both_fingerprints() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).await.unwrap();
        store
            .remember_host("db:22", "ssh-ed25519", "SHA256:old")
            .await
            .unwrap();

        let err = ConnectError::Tunnel(TunnelError::HostKeyChanged {
            host_port: "db:22".into(),
            expected: "SHA256:old".into(),
            actual: "SHA256:new".into(),
        });

        let prompt = host_key_prompt_from(&err, &store).await.expect("prompt");
        assert!(prompt.changed, "a changed key must be flagged as such");
        assert_eq!(prompt.previous_fingerprint.as_deref(), Some("SHA256:old"));
        assert_eq!(prompt.fingerprint, "SHA256:new");
        assert_eq!(prompt.algorithm, "ssh-ed25519");
    }

    #[tokio::test]
    async fn ordinary_failures_do_not_produce_a_host_key_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).await.unwrap();

        let err = ConnectError::Tunnel(TunnelError::Auth {
            user: "u".into(),
            host: "h".into(),
            detail: "denied".into(),
        });
        assert!(host_key_prompt_from(&err, &store).await.is_none());
    }
}
