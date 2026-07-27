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
use crate::profile::{DbConfig, SshConfig, ToolOverrides};
use crate::types::{Engine, EnvironmentTag};

/// Bumped when the bundle layout changes incompatibly.
pub const BUNDLE_VERSION: u32 = 1;

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
    /// Endpoint and auth *method*. A key-file path is a path, not a key.
    pub ssh: Option<SshConfig>,
    #[serde(default)]
    pub tool_overrides: ToolOverrides,
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
    pub profiles_created: Vec<String>,
    pub profiles_updated: Vec<String>,
    pub plans_created: Vec<String>,
    pub plans_updated: Vec<String>,
    pub destinations_created: Vec<String>,
    pub destinations_updated: Vec<String>,
    /// Connections that now exist but cannot connect until someone supplies a
    /// password. Named individually, because "some of these need credentials"
    /// is not something anyone acts on.
    pub needs_credentials: Vec<String>,
    /// Plans whose profile was not in the bundle and is not on this machine.
    pub orphaned_plans: Vec<String>,
    /// Destinations that exist but have no stored access key.
    pub destinations_needing_keys: Vec<String>,
}

impl ImportReport {
    pub fn is_empty(&self) -> bool {
        self.profiles_created.is_empty()
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
        profiles: profiles
            .into_iter()
            .map(|p| SharedProfile {
                name: p.name,
                engine: p.engine,
                environment: p.environment,
                db: p.db,
                ssh: p.ssh,
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

    // ── Connections ─────────────────────────────────────────────────────
    for incoming in &bundle.profiles {
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
                            ssh: Some(incoming.ssh.clone()),
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
                        ssh: incoming.ssh.clone(),
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
    use crate::profile::{SshAuth, SshEndpoint};

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
            ssh: Some(SshConfig {
                endpoint: SshEndpoint {
                    host: "bastion.example.com".into(),
                    port: 22,
                    user: "ops".into(),
                    auth: SshAuth::KeyFile {
                        path: "~/.ssh/id_ed25519".into(),
                        passphrase_in_keychain: true,
                    },
                },
                jump_host: None,
            }),
            tool_overrides: ToolOverrides::default(),
        }
    }

    fn bundle() -> ConfigBundle {
        ConfigBundle {
            bundle_version: BUNDLE_VERSION,
            exported_at: Utc::now(),
            engine_version: "0.1.0".into(),
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
    fn an_empty_report_is_recognisable() {
        assert!(ImportReport::default().is_empty());
        let mut report = ImportReport::default();
        report.profiles_created.push("a".into());
        assert!(!report.is_empty());
    }
}
