//! Shareable configuration: what a team can hand each other.
//!
//! A bundle carries connections, sync plans and off-site destinations — the
//! shape of the work, not the ability to do it.
//!
//! # The invariant
//!
//! **No secret is ever in an export, and an import never writes one.** Not
//! because a bundle is expected to be handled carelessly, but because it is
//! expected to be handled *normally*: committed to a repository, pasted into a
//! ticket, attached to an onboarding document. Every one of those is fine for a
//! hostname and a table selection and catastrophic for a production password.
//!
//! The types here have no field a secret could occupy, so this is a property of
//! the shape rather than of remembering to redact. [`ConfigBundle`] is built
//! from the store, which does not hold secrets either; the keychain is never
//! consulted in either direction.
//!
//! # What an import deliberately does not do
//!
//! It does not enable anything. A connection arrives without a password, and
//! the report says so by name — the receiver supplies their own credential for
//! their own access, which is the point of sharing configuration rather than
//! access.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::backup::TableSelection;
use crate::destination::DestinationKind;
use crate::mask::MaskRule;
use crate::profile::{DbConfig, ToolOverrides};
use crate::sshconn::{SshConfig, SshEndpoint};
use crate::types::{Engine, EnvironmentTag};

/// Bumped when the bundle layout changes incompatibly.
///
/// 2 — SSH connections became shared records, so the tunnel moved out of each
/// profile and into a list of its own. Version 1 bundles still import: their
/// inline configs are adopted the same way an upgraded database's are.
pub const BUNDLE_VERSION: u32 = 2;

#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    #[error("this bundle is version {found}, and this version of DBSync understands {supported}")]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("the bundle is not readable: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error("{0}")]
    Invalid(String),
}

/// A connection, minus the ability to connect.
///
/// Identified by name rather than by id: two machines generate different ids
/// for the same server, and an import that matched on id would duplicate
/// everything every time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SharedProfile {
    pub name: String,
    pub engine: Engine,
    pub environment: EnvironmentTag,
    /// Host, port, user and database. No password — there is no field for one.
    pub db: DbConfig,
    /// The SSH connection this tunnels through, named rather than copied — so
    /// a bundle describing six databases behind one bastion says "bastion"
    /// six times and describes it once.
    #[serde(default)]
    pub ssh_connection: Option<String>,
    /// The inline tunnel a version 1 bundle carried.
    ///
    /// Read on import and never written: a bundle from an older DBSync must
    /// not lose the tunnel it believed it was sharing. See [`import`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    #[serde(default)]
    pub tool_overrides: ToolOverrides,
}

/// An SSH server, minus the ability to authenticate to it.
///
/// Endpoint and auth *method*. A key-file path is a path, not a key, and the
/// passphrase protecting it is in the receiver's keychain or nowhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SharedSshConnection {
    pub name: String,
    pub endpoint: SshEndpoint,
    /// Another entry in the same list, by name — for the same reason profiles
    /// are matched by name: ids differ between machines.
    #[serde(default)]
    pub jump_host: Option<String>,
}

/// A sync plan, keyed to its profile by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SharedPlan {
    pub profile_name: String,
    pub name: String,
    pub database: String,
    pub selections: Vec<TableSelection>,
    /// Masking rules travel with the plan.
    ///
    /// They describe which columns are sensitive, which is knowledge a team
    /// wants shared — and they carry no salt: the salt is derived from a local
    /// secret and never leaves the machine. See [`crate::mask`].
    #[serde(default)]
    pub masking: Vec<MaskRule>,
}

/// An off-site destination, minus its credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SharedDestination {
    pub name: String,
    pub kind: DestinationKind,
    #[serde(default)]
    pub retention: crate::retention::RetentionPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ConfigBundle {
    pub bundle_version: u32,
    pub exported_at: DateTime<Utc>,
    /// The version of DBSync that wrote it, for a human reading a diff.
    pub engine_version: String,
    /// Defaulted rather than required, so a version 1 bundle still parses.
    #[serde(default)]
    pub ssh_connections: Vec<SharedSshConnection>,
    pub profiles: Vec<SharedProfile>,
    pub plans: Vec<SharedPlan>,
    pub destinations: Vec<SharedDestination>,
}

impl ConfigBundle {
    pub fn to_json(&self) -> Result<String, ShareError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn from_json(raw: &str) -> Result<Self, ShareError> {
        let bundle: ConfigBundle = serde_json::from_str(raw)?;
        if bundle.bundle_version > BUNDLE_VERSION {
            return Err(ShareError::UnsupportedVersion {
                found: bundle.bundle_version,
                supported: BUNDLE_VERSION,
            });
        }
        Ok(bundle)
    }
}

/// What an import changed, and what it deliberately did not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ImportReport {
    pub ssh_connections_created: Vec<String>,
    pub ssh_connections_updated: Vec<String>,
    pub profiles_created: Vec<String>,
    pub profiles_updated: Vec<String>,
    pub plans_created: Vec<String>,
    pub plans_updated: Vec<String>,
    pub destinations_created: Vec<String>,
    pub destinations_updated: Vec<String>,
    /// SSH connections whose key needs a passphrase that is not in this
    /// machine's keychain. Named individually for the same reason connections
    /// needing a password are: a count is not something anyone acts on.
    pub ssh_needing_passphrase: Vec<String>,
    /// Connections that now exist but cannot connect until someone supplies a
    /// password. Named individually, because "some of these need credentials"
    /// is not something anyone acts on.
    pub needs_credentials: Vec<String>,
    /// Plans whose profile was not in the bundle and is not on this machine.
    pub orphaned_plans: Vec<String>,
    /// Connections that named an SSH connection the bundle did not carry.
    /// They are imported as direct connections and listed here, because a
    /// tunnelled profile silently becoming a direct one is exactly the kind
    /// of change that is only noticed when it fails.
    pub orphaned_ssh_references: Vec<String>,
    /// Destinations that exist but have no stored access key.
    pub destinations_needing_keys: Vec<String>,
}

impl ImportReport {
    pub fn is_empty(&self) -> bool {
        self.ssh_connections_created.is_empty()
            && self.ssh_connections_updated.is_empty()
            && self.profiles_created.is_empty()
            && self.profiles_updated.is_empty()
            && self.plans_created.is_empty()
            && self.plans_updated.is_empty()
            && self.destinations_created.is_empty()
            && self.destinations_updated.is_empty()
    }
}

// ── Against the store ───────────────────────────────────────────────────

/// Build a bundle from everything on this machine.
///
/// The keychain is not consulted. It could not contribute anything: none of
/// the types above has a field for a secret.
pub async fn export(store: &crate::Store) -> Result<ConfigBundle, ShareError> {
    let profiles = store.list_profiles().await?;
    let ssh = store.list_ssh_connections().await?;

    // Ids are meaningless on the receiving machine, so every reference in a
    // bundle is by name. Resolved once here rather than at each use.
    let ssh_name = |id: uuid::Uuid| ssh.iter().find(|c| c.id == id).map(|c| c.name.clone());

    let mut plans = Vec::new();
    for profile in &profiles {
        for plan in store.list_sync_plans(profile.id).await? {
            plans.push(SharedPlan {
                profile_name: profile.name.clone(),
                name: plan.name,
                database: plan.database,
                selections: plan.selections,
                masking: plan.masking,
            });
        }
    }

    Ok(ConfigBundle {
        bundle_version: BUNDLE_VERSION,
        exported_at: Utc::now(),
        engine_version: crate::ENGINE_VERSION.to_string(),
        ssh_connections: ssh
            .iter()
            .map(|c| SharedSshConnection {
                name: c.name.clone(),
                endpoint: c.endpoint.clone(),
                jump_host: c.jump_host_id.and_then(ssh_name),
            })
            .collect(),
        profiles: profiles
            .into_iter()
            .map(|p| SharedProfile {
                name: p.name,
                engine: p.engine,
                environment: p.environment,
                db: p.db,
                ssh_connection: p.ssh_connection_id.and_then(ssh_name),
                // Never written: the shape exists only so older bundles can
                // still be read.
                ssh: None,
                tool_overrides: p.tool_overrides,
            })
            .collect(),
        plans,
        destinations: store
            .list_destinations()
            .await?
            .into_iter()
            .map(|d| SharedDestination {
                name: d.name,
                kind: d.kind,
                retention: d.retention,
            })
            .collect(),
    })
}

/// Create or update every SSH connection in a bundle, and return their ids by
/// name so the profiles that follow can reference them.
///
/// Two passes, because a bundle's connections may reference each other: the
/// first writes every record without its bastion, the second attaches them.
/// One pass would depend on the order the exporter happened to list them in.
async fn import_ssh_connections(
    store: &crate::Store,
    bundle: &ConfigBundle,
    report: &mut ImportReport,
) -> Result<std::collections::HashMap<String, uuid::Uuid>, ShareError> {
    use crate::sshconn::{SshConnectionCreate, SshConnectionUpdate};

    let mut ids = std::collections::HashMap::new();

    for incoming in &bundle.ssh_connections {
        let existing = store
            .list_ssh_connections()
            .await?
            .into_iter()
            .find(|c| c.name == incoming.name);

        let id = match existing {
            Some(current) => {
                store
                    .update_ssh_connection(
                        current.id,
                        SshConnectionUpdate {
                            endpoint: Some(incoming.endpoint.clone()),
                            ..Default::default()
                        },
                    )
                    .await?;
                report.ssh_connections_updated.push(incoming.name.clone());
                current.id
            }
            None => {
                let created = store
                    .create_ssh_connection(SshConnectionCreate {
                        name: incoming.name.clone(),
                        endpoint: incoming.endpoint.clone(),
                        jump_host_id: None,
                    })
                    .await?;
                report.ssh_connections_created.push(incoming.name.clone());
                created.id
            }
        };

        ids.insert(incoming.name.clone(), id);

        // Checked, never supplied — the same rule as a database password. A
        // key that needs a passphrase is unusable until the receiver enters
        // theirs, and saying so by name is the difference between a bundle
        // that works and one that fails on its first scheduled run.
        if incoming.endpoint.needs_passphrase()
            && !crate::secrets::has_secret(id, crate::secrets::SecretKind::SshKeyPassphrase)
                .unwrap_or(false)
        {
            report.ssh_needing_passphrase.push(incoming.name.clone());
        }
    }

    for incoming in &bundle.ssh_connections {
        let Some(jump_name) = &incoming.jump_host else {
            continue;
        };
        let (Some(id), Some(jump_id)) = (ids.get(&incoming.name), ids.get(jump_name)) else {
            report
                .orphaned_ssh_references
                .push(format!("{} (needs jump host {jump_name:?})", incoming.name));
            continue;
        };

        store
            .update_ssh_connection(
                *id,
                SshConnectionUpdate {
                    jump_host_id: Some(Some(*jump_id)),
                    ..Default::default()
                },
            )
            .await?;
    }

    Ok(ids)
}

/// Adopt the inline tunnel a version 1 bundle carried into a saved connection.
async fn adopt_inline_ssh(
    store: &crate::Store,
    legacy: &SshConfig,
    report: &mut ImportReport,
) -> Result<uuid::Uuid, ShareError> {
    let mut existing = store.list_ssh_connections().await?;

    let jump_host_id = match &legacy.jump_host {
        Some(endpoint) => Some(
            crate::sshconn::adopt_endpoint(store, &mut existing, endpoint, None)
                .await?
                .0,
        ),
        None => None,
    };

    let (id, created) =
        crate::sshconn::adopt_endpoint(store, &mut existing, &legacy.endpoint, jump_host_id)
            .await?;

    if created && let Some(adopted) = existing.iter().find(|c| c.id == id) {
        report.ssh_connections_created.push(adopted.name.clone());
    }
    if legacy.endpoint.needs_passphrase()
        && !crate::secrets::has_secret(id, crate::secrets::SecretKind::SshKeyPassphrase)
            .unwrap_or(false)
        && let Some(adopted) = existing.iter().find(|c| c.id == id)
    {
        report.ssh_needing_passphrase.push(adopted.name.clone());
    }

    Ok(id)
}

/// Apply a bundle to this machine, matching existing records by name.
///
/// Creates what is missing and updates what is there. It never writes a
/// secret, and it never removes anything the bundle omits: an import is
/// additive, because "I shared my config with you" should not be able to
/// delete a connection you rely on.
pub async fn import(
    store: &crate::Store,
    bundle: &ConfigBundle,
) -> Result<ImportReport, ShareError> {
    use crate::profile::{ProfileCreate, ProfileUpdate};

    let mut report = ImportReport::default();

    // ── SSH connections ─────────────────────────────────────────────────
    //
    // First, because a profile cannot reference one that does not exist yet.
    let ssh_ids = import_ssh_connections(store, bundle, &mut report).await?;

    // ── Connections ─────────────────────────────────────────────────────
    for incoming in &bundle.profiles {
        // A version 1 bundle carried the tunnel inside the profile. Adopting
        // it here rather than refusing the bundle keeps the promise that an
        // older export still means what its author intended — and it goes
        // through the same deduplication as an upgraded database, so two
        // profiles sharing a bastion still end up sharing one record.
        let ssh_connection_id = match (&incoming.ssh_connection, &incoming.ssh) {
            (Some(name), _) => match ssh_ids.get(name.as_str()) {
                Some(id) => Some(*id),
                None => {
                    report
                        .orphaned_ssh_references
                        .push(format!("{} (needs SSH connection {name:?})", incoming.name));
                    None
                }
            },
            (None, Some(legacy)) => Some(adopt_inline_ssh(store, legacy, &mut report).await?),
            (None, None) => None,
        };

        let existing = store
            .list_profiles()
            .await?
            .into_iter()
            .find(|p| p.name == incoming.name);

        let id = match existing {
            Some(current) => {
                store
                    .update_profile(
                        current.id,
                        ProfileUpdate {
                            engine: Some(incoming.engine),
                            environment: Some(incoming.environment),
                            db: Some(incoming.db.clone()),
                            ssh_connection_id: Some(ssh_connection_id),
                            tool_overrides: Some(incoming.tool_overrides.clone()),
                            ..Default::default()
                        },
                    )
                    .await?;
                report.profiles_updated.push(incoming.name.clone());
                current.id
            }
            None => {
                let created = store
                    .create_profile(ProfileCreate {
                        name: incoming.name.clone(),
                        engine: incoming.engine,
                        environment: incoming.environment,
                        ssh_connection_id,
                        db: incoming.db.clone(),
                        tool_overrides: incoming.tool_overrides.clone(),
                    })
                    .await?;
                report.profiles_created.push(incoming.name.clone());
                created.id
            }
        };

        // Checked, never supplied. An existing password is left alone, so
        // re-importing a bundle does not lock someone out of a connection
        // they had already set up.
        if !crate::secrets::has_secret(id, crate::secrets::SecretKind::DbPassword).unwrap_or(false)
        {
            report.needs_credentials.push(incoming.name.clone());
        }
    }

    // ── Plans ───────────────────────────────────────────────────────────
    let profiles = store.list_profiles().await?;
    for incoming in &bundle.plans {
        let Some(owner) = profiles.iter().find(|p| p.name == incoming.profile_name) else {
            // The bundle referenced a connection it did not carry and this
            // machine does not have. Reported rather than silently dropped:
            // the sender believed they shared a working plan.
            report.orphaned_plans.push(format!(
                "{} (needs connection {:?})",
                incoming.name, incoming.profile_name
            ));
            continue;
        };

        let existing = store
            .list_sync_plans(owner.id)
            .await?
            .into_iter()
            .find(|p| p.name == incoming.name);

        match existing {
            Some(current) => {
                store
                    .update_sync_plan(current.id, incoming.selections.clone())
                    .await?;
                store
                    .set_sync_plan_masking(current.id, incoming.masking.clone())
                    .await?;
                report.plans_updated.push(incoming.name.clone());
            }
            None => {
                store
                    .create_sync_plan(crate::plan::SyncPlanCreate {
                        profile_id: owner.id,
                        name: incoming.name.clone(),
                        database: incoming.database.clone(),
                        selections: incoming.selections.clone(),
                        masking: incoming.masking.clone(),
                    })
                    .await?;
                report.plans_created.push(incoming.name.clone());
            }
        }
    }

    // ── Destinations ────────────────────────────────────────────────────
    for incoming in &bundle.destinations {
        let existing = store
            .list_destinations()
            .await?
            .into_iter()
            .find(|d| d.name == incoming.name);

        let id = match existing {
            Some(current) => {
                store
                    .update_destination(
                        current.id,
                        crate::destination::DestinationUpdate {
                            kind: Some(incoming.kind.clone()),
                            retention: Some(incoming.retention),
                            ..Default::default()
                        },
                    )
                    .await?;
                report.destinations_updated.push(incoming.name.clone());
                current.id
            }
            None => {
                let created = store
                    .create_destination(crate::destination::DestinationCreate {
                        name: incoming.name.clone(),
                        kind: incoming.kind.clone(),
                        // Arrives switched off. A destination with no
                        // credential cannot upload, and an enabled one that
                        // cannot upload fails every backup until somebody
                        // notices — see the off-site rules.
                        enabled: false,
                        retention: incoming.retention,
                    })
                    .await?;
                report.destinations_created.push(incoming.name.clone());
                created.id
            }
        };

        if !crate::secrets::has_secret(id, crate::secrets::SecretKind::ObjectStoreSecret)
            .unwrap_or(false)
        {
            report.destinations_needing_keys.push(incoming.name.clone());
        }
    }

    // Recorded here rather than at each call site, so the CLI and the app
    // produce the same record. An import that only showed up in the log when
    // it happened to be done through the GUI would be worse than none: the
    // absence of an entry would mean nothing.
    store
        .audit(
            crate::audit::AuditAction::ConfigImported,
            format!("bundle from DBSync {}", bundle.engine_version),
            format!(
                "{} connection(s) and {} plan(s) created or updated",
                report.profiles_created.len() + report.profiles_updated.len(),
                report.plans_created.len() + report.plans_updated.len()
            ),
        )
        .await;

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshconn::SshAuth;

    fn endpoint() -> SshEndpoint {
        SshEndpoint {
            host: "bastion.example.com".into(),
            port: 22,
            user: "ops".into(),
            auth: SshAuth::KeyFile {
                path: "~/.ssh/id_ed25519".into(),
                passphrase_in_keychain: true,
            },
        }
    }

    fn profile() -> SharedProfile {
        SharedProfile {
            name: "prod-eu".into(),
            engine: Engine::Mysql,
            environment: EnvironmentTag::Prod,
            db: DbConfig {
                host: "db.internal".into(),
                port: 3306,
                user: "backup".into(),
                database: Some("app".into()),
            },
            ssh_connection: Some("eu-bastion".into()),
            ssh: None,
            tool_overrides: ToolOverrides::default(),
        }
    }

    fn bundle() -> ConfigBundle {
        ConfigBundle {
            bundle_version: BUNDLE_VERSION,
            exported_at: Utc::now(),
            engine_version: "0.1.0".into(),
            ssh_connections: vec![SharedSshConnection {
                name: "eu-bastion".into(),
                endpoint: endpoint(),
                jump_host: None,
            }],
            profiles: vec![profile()],
            plans: vec![SharedPlan {
                profile_name: "prod-eu".into(),
                name: "nightly".into(),
                database: "app".into(),
                selections: vec![TableSelection::with_data("users")],
                masking: vec![MaskRule::email("users", "email")],
            }],
            destinations: Vec::new(),
        }
    }

    #[test]
    fn a_bundle_round_trips() {
        let original = bundle();
        let parsed = ConfigBundle::from_json(&original.to_json().unwrap()).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn an_exported_bundle_has_nowhere_to_put_a_secret() {
        // The property the whole module rests on. Not "we remembered to
        // redact" — there is no field a password could occupy, so a bundle is
        // safe to commit, paste into a ticket, or attach to an onboarding doc.
        let json = bundle().to_json().unwrap();
        let lowered = json.to_lowercase();
        for forbidden in [
            "password",
            "secret",
            "passphrase\":\"",
            "private_key",
            "token",
        ] {
            assert!(
                !lowered.contains(forbidden),
                "{forbidden:?} appears in an export: {json}"
            );
        }
    }

    #[test]
    fn the_ssh_key_path_travels_but_the_key_does_not() {
        // A path is configuration; the file it points at is a credential and
        // stays on the machine that holds it.
        let json = bundle().to_json().unwrap();
        assert!(json.contains("~/.ssh/id_ed25519"));
        assert!(json.contains("bastion.example.com"));
        assert!(!json.contains("BEGIN OPENSSH PRIVATE KEY"));
    }

    #[test]
    fn masking_rules_travel_but_the_salt_does_not() {
        // Which columns are sensitive is knowledge a team wants shared. The
        // salt is derived from a local secret and would make the pseudonyms
        // reversible by anyone holding both.
        let json = bundle().to_json().unwrap();
        assert!(json.contains("\"email\""));
        assert!(!json.to_lowercase().contains("salt"));
    }

    #[test]
    fn a_newer_bundle_is_refused_rather_than_half_read() {
        // Reading unknown fields as absent would silently drop configuration
        // the sender believed they had shared.
        let mut raw: serde_json::Value =
            serde_json::from_str(&bundle().to_json().unwrap()).unwrap();
        raw["bundle_version"] = serde_json::json!(BUNDLE_VERSION + 1);

        let err = ConfigBundle::from_json(&raw.to_string()).unwrap_err();
        assert!(
            matches!(err, ShareError::UnsupportedVersion { .. }),
            "{err}"
        );
    }

    #[test]
    fn an_older_bundle_still_loads() {
        let mut raw: serde_json::Value =
            serde_json::from_str(&bundle().to_json().unwrap()).unwrap();
        raw["plans"][0].as_object_mut().unwrap().remove("masking");
        let parsed = ConfigBundle::from_json(&raw.to_string()).expect("must load");
        assert!(parsed.plans[0].masking.is_empty());
    }

    #[test]
    fn a_version_1_bundle_keeps_its_inline_tunnel() {
        // The shape a previous DBSync wrote: no `ssh_connections` list, and the
        // tunnel inside the profile. Dropping it on the floor would silently
        // turn a tunnelled connection into a direct one on the receiver's
        // machine — pointing at a host that only resolves from the bastion.
        let raw = serde_json::json!({
            "bundle_version": 1,
            "exported_at": Utc::now(),
            "engine_version": "0.1.0",
            "profiles": [{
                "name": "prod-eu",
                "engine": "mysql",
                "environment": "prod",
                "db": { "host": "db.internal", "port": 3306, "user": "backup",
                        "database": "app" },
                "ssh": {
                    "host": "bastion.example.com", "port": 22, "user": "ops",
                    "auth": { "kind": "agent" },
                    "jump_host": null
                }
            }],
            "plans": [],
            "destinations": []
        });

        let parsed = ConfigBundle::from_json(&raw.to_string()).expect("must load");
        assert!(parsed.ssh_connections.is_empty());

        let profile = &parsed.profiles[0];
        assert!(profile.ssh_connection.is_none());
        assert_eq!(
            profile.ssh.as_ref().expect("inline tunnel").endpoint.host,
            "bastion.example.com",
            "an older bundle's tunnel must survive parsing so import can adopt it"
        );
    }

    #[test]
    fn an_export_never_writes_the_legacy_inline_field() {
        // Reading it is compatibility; writing it would mean two places a
        // tunnel could live, and eventually two that disagree.
        let json = bundle().to_json().unwrap();
        assert!(
            !json.contains("\"ssh\":"),
            "the legacy inline field must not be exported: {json}"
        );
        assert!(json.contains("\"ssh_connection\": \"eu-bastion\""));
    }

    #[test]
    fn a_shared_connection_is_described_once_however_many_use_it() {
        // The reason the list exists. Six databases behind one bastion should
        // produce one description of it, so changing it is one edit.
        let mut b = bundle();
        for name in ["prod-eu-2", "prod-eu-3"] {
            let mut extra = profile();
            extra.name = name.into();
            b.profiles.push(extra);
        }

        let json = b.to_json().unwrap();
        assert_eq!(
            json.matches("bastion.example.com").count(),
            1,
            "the endpoint must appear once, not once per profile: {json}"
        );
        assert_eq!(
            json.matches("\"eu-bastion\"").count(),
            4,
            "one definition, three references"
        );
    }

    #[test]
    fn a_jump_host_travels_as_a_name() {
        let mut b = bundle();
        b.ssh_connections.push(SharedSshConnection {
            name: "edge".into(),
            endpoint: SshEndpoint {
                host: "edge.example.com".into(),
                port: 22,
                user: "jump".into(),
                auth: SshAuth::Agent,
            },
            jump_host: None,
        });
        b.ssh_connections[0].jump_host = Some("edge".into());

        let parsed = ConfigBundle::from_json(&b.to_json().unwrap()).unwrap();
        assert_eq!(parsed.ssh_connections[0].jump_host.as_deref(), Some("edge"));
    }

    #[test]
    fn an_empty_report_is_recognisable() {
        assert!(ImportReport::default().is_empty());
        let mut report = ImportReport::default();
        report.profiles_created.push("a".into());
        assert!(!report.is_empty());

        let mut ssh_only = ImportReport::default();
        ssh_only.ssh_connections_created.push("bastion".into());
        assert!(
            !ssh_only.is_empty(),
            "importing only SSH connections is still a change"
        );
    }
}
