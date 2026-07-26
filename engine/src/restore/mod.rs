//! Restore options and orchestration.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::manifest::{ArtifactFormat, BackupManifest};
use crate::profile::ConnectionProfile;
use crate::types::{Engine, EnvironmentTag};

pub mod mysql;
pub mod postgres;

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
}

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
#[serde(rename_all = "snake_case", tag = "engine")]
pub enum EngineRestoreOptions {
    Mysql(MysqlRestoreOptions),
    Postgres(PostgresRestoreOptions),
}

impl EngineRestoreOptions {
    pub const fn engine(&self) -> Engine {
        match self {
            EngineRestoreOptions::Mysql(_) => Engine::Mysql,
            EngineRestoreOptions::Postgres(_) => Engine::Postgres,
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
            ssh: None,
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
