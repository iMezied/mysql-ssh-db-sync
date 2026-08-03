//! Restore options and orchestration.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::manifest::{ArtifactFormat, BackupManifest};
use crate::profile::ConnectionProfile;
use crate::types::{Engine, EnvironmentTag};

pub mod mongo;
pub mod mysql;
pub mod postgres;

pub use mongo::run_mongo_restore;
pub use mysql::run_mysql_restore;
pub use postgres::run_postgres_restore;

/// How the destination database is chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "strategy")]
pub enum TargetNaming {
    /// Create `{prefix}_{YYYYMMDD_HHMMSS}`. Non-destructive; the default.
    NewTimestamped { prefix: String },
    /// Use a fixed name, dropping it first if it exists.
    DropAndRecreate { name: String },
    /// Restore into an existing database without dropping.
    IntoExisting { name: String },
}

impl TargetNaming {
    /// Whether this strategy can destroy existing data.
    pub const fn is_destructive(&self) -> bool {
        matches!(self, TargetNaming::DropAndRecreate { .. })
    }

    pub fn resolve(&self, now: chrono::DateTime<chrono::Utc>) -> String {
        match self {
            TargetNaming::NewTimestamped { prefix } => {
                format!("{}_{}", prefix, now.format("%Y%m%d_%H%M%S"))
            }
            TargetNaming::DropAndRecreate { name } | TargetNaming::IntoExisting { name } => {
                name.clone()
            }
        }
    }

    /// Resolve to a name the server is not already using.
    ///
    /// Only `NewTimestamped` moves. Its timestamp has one-second resolution, so
    /// two restores starting in the same second ask for the same database —
    /// two schedules due at the same minute, a pipeline run beside a manual
    /// one, or somebody restoring an artifact twice to compare. The first wins
    /// and the second used to fail on a name, having done nothing wrong.
    ///
    /// Walking to the next free second is what a person would do anyway, and it
    /// keeps the `{prefix}_{stamp}` shape: nothing that reads these names —
    /// [`crate::ops::is_drill_database`] most of all, which decides what a
    /// drill is allowed to drop — has to learn a second one. The name is a few
    /// seconds ahead of the clock, which is the cost, and it is a name rather
    /// than a record of when the job ran.
    ///
    /// `None` means a whole [`NAMING_WINDOW_SECS`]-second run of names is
    /// taken. That is not a collision to route around; it is the caller's cue
    /// to report the collision it was always going to report.
    ///
    /// The fixed strategies come back unchanged, existing or not:
    /// `DropAndRecreate` means *that* database, and `IntoExisting` needs it to
    /// already be there. Deciding those is the caller's job.
    pub fn resolve_free(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        taken: &[String],
    ) -> Option<String> {
        if !matches!(self, TargetNaming::NewTimestamped { .. }) {
            return Some(self.resolve(now));
        }
        (0..NAMING_WINDOW_SECS)
            .map(|seconds| self.resolve(now + chrono::Duration::seconds(seconds)))
            .find(|name| !taken.iter().any(|t| t == name))
    }
}

/// How far [`TargetNaming::resolve_free`] will walk to find an unused name.
///
/// A minute of one-second names. Long enough that every realistic burst — a
/// handful of restores kicked off together — lands, short enough that a
/// destination genuinely full of these still gets told so.
const NAMING_WINDOW_SECS: i64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MysqlRestoreOptions {
    pub foreign_key_checks_off: bool,
    pub unique_checks_off: bool,
    pub autocommit_off: bool,
    /// Skip writing the restore to the destination's binary log.
    pub disable_binlog: bool,
    pub charset: String,
    pub collation: String,
}

impl Default for MysqlRestoreOptions {
    fn default() -> Self {
        Self {
            foreign_key_checks_off: true,
            unique_checks_off: true,
            autocommit_off: true,
            disable_binlog: false,
            charset: "utf8mb4".into(),
            collation: "utf8mb4_unicode_ci".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PostgresRestoreOptions {
    pub no_owner: bool,
    pub no_privileges: bool,
    /// `pg_restore -j`. Only valid for archive formats.
    pub parallel_jobs: Option<u16>,
    /// Restore only these tables. Requires an archive format.
    pub only_tables: Vec<String>,
    pub clean: bool,
}

impl Default for PostgresRestoreOptions {
    fn default() -> Self {
        Self {
            no_owner: true,
            no_privileges: true,
            parallel_jobs: None,
            only_tables: Vec::new(),
            clean: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MongoRestoreOptions {
    /// Drop each collection before restoring it.
    ///
    /// Distinct from [`TargetNaming::DropAndRecreate`], which drops the whole
    /// database: this is what makes `IntoExisting` replace rather than merge.
    /// Off by default, so the safe reading of "restore into this database" is
    /// the one that happens without asking.
    pub drop_collections: bool,
    /// Restore only these collections. Uses `--nsInclude`, so it needs the
    /// archive format — which every MongoDB artifact this app writes is.
    pub only_collections: Vec<String>,
    /// Collections restored at once, and insertion workers within each.
    pub parallel_collections: Option<u16>,
    pub insertion_workers: Option<u16>,
    /// Stop at the first failed document rather than carrying on.
    ///
    /// On by default, and the default is the point: `mongorestore` otherwise
    /// reports failures on stderr, exits 0, and leaves a database that is
    /// missing documents nobody was told about. A restore that half-worked has
    /// to be a failed restore, or the drill is checking a lie.
    pub stop_on_error: bool,
    /// Rebuild indexes after the documents land.
    pub restore_indexes: bool,
    /// Skip the destination's schema validators.
    ///
    /// Off by default: a validator rejecting a document is information, not an
    /// obstacle. It matters most after masking, where a masked value can stop
    /// matching a pattern the source enforced.
    pub bypass_document_validation: bool,
}

impl Default for MongoRestoreOptions {
    fn default() -> Self {
        Self {
            drop_collections: false,
            only_collections: Vec::new(),
            parallel_collections: None,
            insertion_workers: None,
            stop_on_error: true,
            restore_indexes: true,
            bypass_document_validation: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "engine")]
pub enum EngineRestoreOptions {
    Mysql(MysqlRestoreOptions),
    Postgres(PostgresRestoreOptions),
    Mongo(MongoRestoreOptions),
}

impl EngineRestoreOptions {
    pub const fn engine(&self) -> Engine {
        match self {
            EngineRestoreOptions::Mysql(_) => Engine::Mysql,
            EngineRestoreOptions::Postgres(_) => Engine::Postgres,
            EngineRestoreOptions::Mongo(_) => Engine::Mongo,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct RestoreRequest {
    pub artifact_path: PathBuf,
    pub naming: TargetNaming,
    pub engine: EngineRestoreOptions,
    /// Verify the artifact checksum before touching the destination.
    pub verify_checksum: bool,
    /// Typed confirmation supplied by the user for a destructive restore.
    pub typed_confirmation: Option<String>,
}

/// What `ops::restore` has settled by the time it knows which engine will run.
///
/// One struct for the same reason as [`crate::backup::BackupRun`]: all three
/// engine entry points take exactly this set, and `target` arriving as a fourth
/// borrow beside `request` and `tools` is precisely the transposition that
/// comment warns about.
pub struct RestoreRun<'a> {
    pub profile: &'a ConnectionProfile,
    pub request: &'a RestoreRequest,
    /// The database to write to, already resolved.
    ///
    /// Resolved once, by the caller, and *not* recomputed here — see
    /// [`TargetNaming::resolve_free`]. A worker that called `resolve` again
    /// would be naming its database from a clock that has moved on since the
    /// name was checked.
    pub target: String,
    /// Already resolved — with a tunnel this is its local end.
    pub endpoint: crate::backup::mysql::Endpoint,
    /// Where the client binaries come from: this machine, or a container.
    pub tools: &'a crate::tools::ToolSource,
}

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("profile engine is {profile:?} but options are for {options:?}")]
    EngineMismatch { profile: Engine, options: Engine },
    #[error("artifact was produced for {artifact:?} but the target is {profile:?}")]
    ArtifactEngineMismatch { artifact: Engine, profile: Engine },
    #[error(
        "this drops the existing database {expected:?}; type its name to confirm (got {got:?})"
    )]
    ConfirmationRequired {
        expected: String,
        got: Option<String>,
    },
    #[error("selective restore needs an archive format; {0:?} cannot do it")]
    SelectiveRestoreUnsupported(ArtifactFormat),
    #[error("parallel restore needs an archive format; {0:?} cannot do it")]
    ParallelRestoreUnsupported(ArtifactFormat),
    #[error("{0}")]
    Invalid(String),
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
    #[error(transparent)]
    Exec(#[from] crate::exec::ExecError),
    #[error("job was cancelled")]
    Cancelled,
}

impl RestoreRequest {
    /// Validate against the target profile and the artifact's manifest.
    ///
    /// Everything checkable offline is checked here, before a tunnel is opened
    /// or a destination database is created.
    pub fn validate(
        &self,
        profile: &ConnectionProfile,
        manifest: Option<&BackupManifest>,
    ) -> Result<(), RestoreError> {
        if profile.engine != self.engine.engine() {
            return Err(RestoreError::EngineMismatch {
                profile: profile.engine,
                options: self.engine.engine(),
            });
        }

        if let Some(m) = manifest
            && m.engine != profile.engine
        {
            return Err(RestoreError::ArtifactEngineMismatch {
                artifact: m.engine,
                profile: profile.engine,
            });
        }

        // Destructive restores always need typed confirmation. Production
        // targets need it even when the strategy looks benign, because
        // "into existing" can still clobber rows.
        let needs_confirmation = self.naming.is_destructive()
            || (profile.environment.requires_typed_confirmation()
                && !matches!(self.naming, TargetNaming::NewTimestamped { .. }));

        if needs_confirmation {
            let expected = self.naming.resolve(chrono::Utc::now());
            if self.typed_confirmation.as_deref() != Some(expected.as_str()) {
                return Err(RestoreError::ConfirmationRequired {
                    expected,
                    got: self.typed_confirmation.clone(),
                });
            }
        }

        if let EngineRestoreOptions::Postgres(o) = &self.engine {
            let format = manifest.map(|m| m.format);

            if !o.only_tables.is_empty()
                && let Some(f) = format
                && !f.supports_selective_restore()
            {
                return Err(RestoreError::SelectiveRestoreUnsupported(f));
            }

            if let Some(jobs) = o.parallel_jobs {
                if jobs == 0 {
                    return Err(RestoreError::Invalid(
                        "parallel_jobs must be at least 1".into(),
                    ));
                }
                if let Some(f) = format
                    && !f.supports_selective_restore()
                {
                    return Err(RestoreError::ParallelRestoreUnsupported(f));
                }
            }
        }

        if let EngineRestoreOptions::Mongo(o) = &self.engine {
            if let Some(jobs) = o.parallel_collections
                && jobs == 0
            {
                return Err(RestoreError::Invalid(
                    "parallel_collections must be at least 1".into(),
                ));
            }
            if let Some(workers) = o.insertion_workers
                && workers == 0
            {
                return Err(RestoreError::Invalid(
                    "insertion_workers must be at least 1".into(),
                ));
            }

            // A namespace filter is matched against the archive's contents, so
            // a pattern is fine but an empty string would match nothing and
            // restore an empty database while reporting success.
            if o.only_collections.iter().any(|c| c.trim().is_empty()) {
                return Err(RestoreError::Invalid(
                    "a blank collection name in only_collections would match nothing and \
                     restore an empty database"
                        .into(),
                ));
            }
        }

        Ok(())
    }
}

/// Environments where an accidental restore is most costly get the strictest
/// defaults.
pub fn default_naming_for(environment: EnvironmentTag) -> TargetNaming {
    let _ = environment;
    TargetNaming::NewTimestamped {
        prefix: "restore".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::MANIFEST_VERSION;
    use crate::profile::{DbConfig, ToolOverrides};
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    fn profile(engine: Engine, environment: EnvironmentTag) -> ConnectionProfile {
        ConnectionProfile {
            id: Uuid::new_v4(),
            name: "target".into(),
            engine,
            environment,
            ssh_connection_id: None,
            db: DbConfig {
                host: "127.0.0.1".into(),
                port: engine.default_port(),
                user: "root".into(),
                database: None,
            },
            tool_overrides: ToolOverrides::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn manifest(engine: Engine, format: ArtifactFormat) -> BackupManifest {
        BackupManifest {
            manifest_version: MANIFEST_VERSION,
            id: Uuid::new_v4(),
            source_profile_id: Uuid::new_v4(),
            source_profile_name: "src".into(),
            engine,
            server_version: "16.2".into(),
            dump_tool: "pg_dump".into(),
            dump_tool_version: "16.2".into(),
            database: "app".into(),
            created_at: Utc::now(),
            format,
            tables: vec![],
            tables_with_data: vec![],
            source_row_counts: Default::default(),
            options: serde_json::json!({}),
            artifact_filename: "a".into(),
            size_bytes: 0,
            sha256: String::new(),
            encrypted: false,
            encryption_recipients: Vec::new(),
        }
    }

    fn pg_request(naming: TargetNaming, opts: PostgresRestoreOptions) -> RestoreRequest {
        RestoreRequest {
            artifact_path: PathBuf::from("/tmp/a.dump"),
            naming,
            engine: EngineRestoreOptions::Postgres(opts),
            verify_checksum: true,
            typed_confirmation: None,
        }
    }

    #[test]
    fn timestamped_names_are_unique_per_second() {
        let n = TargetNaming::NewTimestamped {
            prefix: "restore".into(),
        };
        let t = Utc.with_ymd_and_hms(2026, 3, 15, 14, 30, 22).unwrap();
        assert_eq!(n.resolve(t), "restore_20260315_143022");
    }

    #[test]
    fn a_free_generated_name_is_the_plain_timestamp() {
        // The overwhelmingly common case, and the one whose name must not get
        // uglier to serve the rare one.
        let n = TargetNaming::NewTimestamped {
            prefix: "restore".into(),
        };
        let t = Utc.with_ymd_and_hms(2026, 3, 15, 14, 30, 22).unwrap();
        assert_eq!(
            n.resolve_free(t, &["something_else".into()]),
            Some("restore_20260315_143022".into())
        );
    }

    #[test]
    fn a_taken_generated_name_walks_to_the_next_free_second() {
        let n = TargetNaming::NewTimestamped {
            prefix: "restore".into(),
        };
        let t = Utc.with_ymd_and_hms(2026, 3, 15, 14, 30, 22).unwrap();
        let taken = vec![
            "restore_20260315_143022".into(),
            "restore_20260315_143023".into(),
        ];
        assert_eq!(
            n.resolve_free(t, &taken),
            Some("restore_20260315_143024".into())
        );
    }

    #[test]
    fn walking_keeps_the_shape_a_drill_name_is_recognised_by() {
        // `ops::is_drill_database` parses `{prefix}_{stamp}`, and it is what
        // decides whether a drill may drop its own scratch database. A name it
        // cannot parse is one nothing will ever clean up, so the walk must not
        // invent a third component.
        let n = TargetNaming::NewTimestamped {
            prefix: crate::ops::DRILL_PREFIX.into(),
        };
        let t = Utc.with_ymd_and_hms(2026, 7, 26, 3, 0, 0).unwrap();
        let first = n.resolve_free(t, &[]).unwrap();
        let second = n.resolve_free(t, std::slice::from_ref(&first)).unwrap();

        assert_ne!(first, second);
        assert!(crate::ops::is_drill_database(&first));
        assert!(crate::ops::is_drill_database(&second));
    }

    #[test]
    fn a_full_window_of_names_is_a_collision_to_report() {
        // Walking is for the burst that would otherwise fail on a name. A
        // destination genuinely full of these is not that, and saying so beats
        // walking somewhere arbitrary.
        let n = TargetNaming::NewTimestamped {
            prefix: "restore".into(),
        };
        let t = Utc.with_ymd_and_hms(2026, 3, 15, 14, 30, 22).unwrap();
        let taken: Vec<String> = (0..NAMING_WINDOW_SECS)
            .map(|s| n.resolve(t + chrono::Duration::seconds(s)))
            .collect();
        assert_eq!(n.resolve_free(t, &taken), None);
    }

    #[test]
    fn a_fixed_name_never_moves() {
        // `DropAndRecreate` means that database and `IntoExisting` needs it to
        // be there, so "already taken" is the normal case for both, not a
        // reason to pick a different one.
        let t = Utc.with_ymd_and_hms(2026, 3, 15, 14, 30, 22).unwrap();
        for naming in [
            TargetNaming::DropAndRecreate {
                name: "staging_app".into(),
            },
            TargetNaming::IntoExisting {
                name: "staging_app".into(),
            },
        ] {
            assert_eq!(
                naming.resolve_free(t, &["staging_app".into()]),
                Some("staging_app".into())
            );
        }
    }

    #[test]
    fn timestamped_restore_is_not_destructive() {
        assert!(!default_naming_for(EnvironmentTag::Prod).is_destructive());
    }

    #[test]
    fn drop_and_recreate_requires_typed_confirmation() {
        let req = RestoreRequest {
            artifact_path: PathBuf::from("/tmp/a.sql.gz"),
            naming: TargetNaming::DropAndRecreate {
                name: "staging_app".into(),
            },
            engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify_checksum: true,
            typed_confirmation: None,
        };
        assert!(matches!(
            req.validate(&profile(Engine::Mysql, EnvironmentTag::Staging), None),
            Err(RestoreError::ConfirmationRequired { .. })
        ));
    }

    #[test]
    fn correct_typed_confirmation_unlocks_destructive_restore() {
        let req = RestoreRequest {
            artifact_path: PathBuf::from("/tmp/a.sql.gz"),
            naming: TargetNaming::DropAndRecreate {
                name: "staging_app".into(),
            },
            engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify_checksum: true,
            typed_confirmation: Some("staging_app".into()),
        };
        assert!(
            req.validate(&profile(Engine::Mysql, EnvironmentTag::Staging), None)
                .is_ok()
        );
    }

    #[test]
    fn wrong_typed_confirmation_is_refused() {
        let req = RestoreRequest {
            artifact_path: PathBuf::from("/tmp/a.sql.gz"),
            naming: TargetNaming::DropAndRecreate {
                name: "staging_app".into(),
            },
            engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify_checksum: true,
            typed_confirmation: Some("staging".into()),
        };
        assert!(
            req.validate(&profile(Engine::Mysql, EnvironmentTag::Staging), None)
                .is_err()
        );
    }

    #[test]
    fn restoring_into_existing_production_db_requires_confirmation() {
        let req = pg_request(
            TargetNaming::IntoExisting {
                name: "prod_app".into(),
            },
            PostgresRestoreOptions::default(),
        );
        assert!(matches!(
            req.validate(&profile(Engine::Postgres, EnvironmentTag::Prod), None),
            Err(RestoreError::ConfirmationRequired { .. })
        ));
    }

    #[test]
    fn engine_mismatch_between_profile_and_options_is_rejected() {
        let req = pg_request(
            TargetNaming::NewTimestamped { prefix: "r".into() },
            PostgresRestoreOptions::default(),
        );
        assert!(matches!(
            req.validate(&profile(Engine::Mysql, EnvironmentTag::Dev), None),
            Err(RestoreError::EngineMismatch { .. })
        ));
    }

    #[test]
    fn mysql_artifact_cannot_restore_into_postgres() {
        let req = pg_request(
            TargetNaming::NewTimestamped { prefix: "r".into() },
            PostgresRestoreOptions::default(),
        );
        let m = manifest(Engine::Mysql, ArtifactFormat::SqlGz);
        assert!(matches!(
            req.validate(&profile(Engine::Postgres, EnvironmentTag::Dev), Some(&m)),
            Err(RestoreError::ArtifactEngineMismatch { .. })
        ));
    }

    #[test]
    fn selective_restore_from_plain_sql_is_rejected() {
        let req = pg_request(
            TargetNaming::NewTimestamped { prefix: "r".into() },
            PostgresRestoreOptions {
                only_tables: vec!["orders".into()],
                ..Default::default()
            },
        );
        let m = manifest(Engine::Postgres, ArtifactFormat::SqlGz);
        assert!(matches!(
            req.validate(&profile(Engine::Postgres, EnvironmentTag::Dev), Some(&m)),
            Err(RestoreError::SelectiveRestoreUnsupported(_))
        ));
    }

    #[test]
    fn selective_restore_from_custom_archive_is_allowed() {
        let req = pg_request(
            TargetNaming::NewTimestamped { prefix: "r".into() },
            PostgresRestoreOptions {
                only_tables: vec!["orders".into()],
                parallel_jobs: Some(4),
                ..Default::default()
            },
        );
        let m = manifest(Engine::Postgres, ArtifactFormat::PgCustom);
        assert!(
            req.validate(&profile(Engine::Postgres, EnvironmentTag::Dev), Some(&m))
                .is_ok()
        );
    }

    #[test]
    fn parallel_restore_from_plain_sql_is_rejected() {
        let req = pg_request(
            TargetNaming::NewTimestamped { prefix: "r".into() },
            PostgresRestoreOptions {
                parallel_jobs: Some(4),
                ..Default::default()
            },
        );
        let m = manifest(Engine::Postgres, ArtifactFormat::SqlGz);
        assert!(matches!(
            req.validate(&profile(Engine::Postgres, EnvironmentTag::Dev), Some(&m)),
            Err(RestoreError::ParallelRestoreUnsupported(_))
        ));
    }

    #[test]
    fn mysql_restore_defaults_are_fast_but_reversible() {
        let o = MysqlRestoreOptions::default();
        assert!(o.foreign_key_checks_off);
        assert!(o.unique_checks_off);
        assert!(!o.disable_binlog, "binlog suppression must be opt-in");
    }
}
