//! Backup options and orchestration.
//!
//! Options are split into a common part and a per-engine part. A single flat
//! struct cannot represent every engine honestly: `--hex-blob` is meaningless
//! to `pg_dump`, `--format=custom` is meaningless to `mysqldump`, and `--oplog`
//! is meaningless to both. Modelling that as an enum makes the invalid
//! combinations unrepresentable rather than silently ignored.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::manifest::ArtifactFormat;
use crate::profile::ConnectionProfile;
use crate::types::Engine;

pub mod mongo;
pub mod mysql;
pub mod postgres;

pub use mongo::run_mongo_backup;
pub use mysql::run_mysql_backup;
pub use postgres::run_postgres_backup;

/// What to do with one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum TableMode {
    /// Structure and rows.
    SchemaAndData,
    /// Structure only — the default for everything not explicitly selected.
    SchemaOnly,
    /// Omit entirely.
    Exclude,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TableSelection {
    pub name: String,
    pub mode: TableMode,
    /// Optional row filter applied only when `mode` is `SchemaAndData`.
    pub where_filter: Option<String>,
}

impl TableSelection {
    pub fn schema_only(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            mode: TableMode::SchemaOnly,
            where_filter: None,
        }
    }

    pub fn with_data(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            mode: TableMode::SchemaAndData,
            where_filter: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct CommonBackupOptions {
    pub database: String,
    pub selections: Vec<TableSelection>,
    pub output_dir: PathBuf,
    /// Gzip the stream. Ignored for formats that compress internally.
    pub compress: bool,
    /// Encrypt at rest with age. Wired in a later milestone; the manifest
    /// already records the flag.
    pub encrypt: bool,
    /// Count the rows of each data table before dumping, and record the
    /// numbers in the manifest.
    ///
    /// # What it buys and what it costs
    ///
    /// A restore drill checks an artifact against its own manifest. Without
    /// these numbers the manifest only says which tables were *selected* for
    /// data, so a table that comes back empty cannot be told apart from one
    /// that was empty at the source, and the drill has to record it as
    /// not compared. With them the drill compares exactly, and a dump that
    /// lost rows is caught.
    ///
    /// The cost is an exact `COUNT(*)` per data table — a full scan, on top of
    /// the scan the dump itself does. That is why it is a choice.
    /// [`crate::db::TableInfo::estimated_rows`] is not usable here: the planner
    /// estimate reads zero for tables that plainly have rows, which is the
    /// exact failure this project replaced in the Bash tool.
    ///
    /// Defaulted to off on deserialise so a schedule stored before this
    /// existed does not silently acquire the cost, matching `deep_verify`.
    #[serde(default)]
    pub record_row_counts: bool,
}

impl CommonBackupOptions {
    pub fn tables_with_data(&self) -> Vec<&TableSelection> {
        self.selections
            .iter()
            .filter(|s| s.mode == TableMode::SchemaAndData)
            .collect()
    }

    pub fn included_tables(&self) -> Vec<&TableSelection> {
        self.selections
            .iter()
            .filter(|s| s.mode != TableMode::Exclude)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MysqlBackupOptions {
    /// Consistent snapshot without locking. Only meaningful for InnoDB.
    pub single_transaction: bool,
    /// Emit binary columns as hex. Without this, BLOBs can corrupt in transit.
    pub hex_blob: bool,
    pub set_gtid_purged_off: bool,
    pub add_drop_table: bool,
    pub extended_insert: bool,
    pub routines: bool,
    pub triggers: bool,
    pub events: bool,
    pub default_character_set: String,
    /// Send `--column-statistics=0`, required when an 8.x client talks to a
    /// pre-8.0 server.
    pub disable_column_statistics: bool,
    /// Strip `DEFINER=` clauses so restores need no SUPER privilege.
    pub strip_definer: bool,
    /// Use mydumper for a parallel dump when the binary is available.
    pub parallel_threads: Option<u16>,
    pub extra_flags: Vec<String>,
}

impl Default for MysqlBackupOptions {
    fn default() -> Self {
        Self {
            single_transaction: true,
            hex_blob: true,
            set_gtid_purged_off: true,
            add_drop_table: true,
            extended_insert: true,
            routines: true,
            triggers: true,
            events: true,
            default_character_set: "utf8mb4".to_string(),
            disable_column_statistics: false,
            strip_definer: true,
            parallel_threads: None,
            extra_flags: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum PgDumpFormat {
    /// `-Fc`. Default: the only format supporting both selective and parallel
    /// restore.
    Custom,
    /// `-Fd`. Adds parallel *dump*.
    Directory,
    /// `-Fp`. Plain SQL; no selective restore.
    Plain,
}

impl PgDumpFormat {
    pub const fn flag(self) -> &'static str {
        match self {
            PgDumpFormat::Custom => "-Fc",
            PgDumpFormat::Directory => "-Fd",
            PgDumpFormat::Plain => "-Fp",
        }
    }

    pub const fn artifact_format(self) -> ArtifactFormat {
        match self {
            PgDumpFormat::Custom => ArtifactFormat::PgCustom,
            PgDumpFormat::Directory => ArtifactFormat::PgDirectory,
            PgDumpFormat::Plain => ArtifactFormat::SqlGz,
        }
    }

    /// `pg_dump -j` is only valid for the directory format.
    pub const fn supports_parallel_dump(self) -> bool {
        matches!(self, PgDumpFormat::Directory)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PostgresBackupOptions {
    pub format: PgDumpFormat,
    pub no_owner: bool,
    pub no_privileges: bool,
    pub blobs: bool,
    /// Restrict to these schemas; empty means all.
    pub schemas: Vec<String>,
    pub serializable_deferrable: bool,
    /// Parallel dump jobs. Only honoured for `Directory`.
    pub parallel_jobs: Option<u16>,
    /// Additionally dump roles/globals via `pg_dumpall --globals-only`.
    pub include_globals: bool,
    pub extra_flags: Vec<String>,
}

impl Default for PostgresBackupOptions {
    fn default() -> Self {
        Self {
            format: PgDumpFormat::Custom,
            no_owner: true,
            no_privileges: true,
            blobs: true,
            schemas: Vec::new(),
            serializable_deferrable: false,
            parallel_jobs: None,
            include_globals: false,
            extra_flags: Vec::new(),
        }
    }
}

/// Derived rather than written out, unlike the other two engines': every
/// default here really is the type's own. The reasoning that matters — why
/// `oplog` is off — belongs on the field, not in a hand-written `impl` that
/// restates `false`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MongoBackupOptions {
    /// Capture the oplog during the dump so the restore can replay to a single
    /// point in time.
    ///
    /// This is MongoDB's answer to `--single-transaction`, and the reason it is
    /// **off** by default is that it is not equivalent: `--oplog` requires a
    /// replica set and fails outright on a standalone server, which is what
    /// most development databases are. Defaulting it on would make the common
    /// case an error.
    ///
    /// Without it, a dump of a database being written to is consistent within
    /// each collection and not across them. That is a real caveat and the UI
    /// says so rather than burying it here.
    pub oplog: bool,
    /// Collections dumped at once. `None` leaves `mongodump` to its own
    /// default.
    pub parallel_collections: Option<u16>,
    /// Also dump the users and roles defined on this database.
    ///
    /// The counterpart of PostgreSQL's `include_globals`, and off for the same
    /// reason: restoring them into another server changes who can log in there.
    pub dump_users_and_roles: bool,
    pub extra_flags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "engine")]
pub enum EngineBackupOptions {
    Mysql(MysqlBackupOptions),
    Postgres(PostgresBackupOptions),
    Mongo(MongoBackupOptions),
}

impl EngineBackupOptions {
    pub const fn engine(&self) -> Engine {
        match self {
            EngineBackupOptions::Mysql(_) => Engine::Mysql,
            EngineBackupOptions::Postgres(_) => Engine::Postgres,
            EngineBackupOptions::Mongo(_) => Engine::Mongo,
        }
    }

    pub fn artifact_format(&self) -> ArtifactFormat {
        match self {
            EngineBackupOptions::Mysql(o) => {
                if o.parallel_threads.is_some() {
                    ArtifactFormat::MydumperDir
                } else {
                    ArtifactFormat::SqlGz
                }
            }
            EngineBackupOptions::Postgres(o) => o.format.artifact_format(),
            EngineBackupOptions::Mongo(_) => ArtifactFormat::MongoArchive,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct BackupRequest {
    pub common: CommonBackupOptions,
    pub engine: EngineBackupOptions,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("profile engine is {profile:?} but options are for {options:?}")]
    EngineMismatch { profile: Engine, options: Engine },
    #[error("{0}")]
    Invalid(String),
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
    #[error("could not find {tool}; install it or set an override on the profile")]
    ToolMissing { tool: String },
    #[error(transparent)]
    Exec(#[from] crate::exec::ExecError),
    #[error("io error: {0}")]
    Io(String),
    #[error(transparent)]
    Connect(#[from] Box<crate::connect::ConnectError>),
    #[error("job was cancelled")]
    Cancelled,
}

impl BackupRequest {
    /// Reject combinations the tools will refuse, before opening any tunnel.
    pub fn validate(&self, profile: &ConnectionProfile) -> Result<(), BackupError> {
        if profile.engine != self.engine.engine() {
            return Err(BackupError::EngineMismatch {
                profile: profile.engine,
                options: self.engine.engine(),
            });
        }

        if self.common.database.trim().is_empty() {
            return Err(BackupError::Invalid("no database selected".into()));
        }

        if self.common.included_tables().is_empty() {
            return Err(BackupError::Invalid(
                "every table is excluded; nothing to back up".into(),
            ));
        }

        if let EngineBackupOptions::Postgres(o) = &self.engine
            && let Some(jobs) = o.parallel_jobs
        {
            if !o.format.supports_parallel_dump() {
                return Err(BackupError::Invalid(format!(
                    "parallel dump requires the directory format, not {:?}",
                    o.format
                )));
            }
            if jobs == 0 {
                return Err(BackupError::Invalid(
                    "parallel_jobs must be at least 1".into(),
                ));
            }
        }

        // Not yet implemented, and refused rather than ignored.
        //
        // Ignoring it would be actively dangerous: `artifact_format()` returns
        // `MydumperDir` whenever this is set, and `MydumperDir::is_directory()`
        // is true — so a silently-ignored setting produces a single gzipped
        // file carrying a manifest that declares it a directory, which the
        // restore path then believes. A missing feature is a disappointment; a
        // manifest that lies about the artifact is a corrupted restore.
        if let EngineBackupOptions::Mysql(o) = &self.engine
            && o.parallel_threads.is_some()
        {
            return Err(BackupError::NotImplemented(
                "parallel MySQL dumps (mydumper). Leave parallel_threads unset for a normal dump",
            ));
        }

        // Encryption streams, and a directory-format dump is written by the
        // tool itself as many separate files with no stream to wrap. Silently
        // producing an unencrypted artifact for a request that asked for
        // encryption is the one outcome that must never happen, so the
        // combination is refused instead.
        if self.common.encrypt
            && let EngineBackupOptions::Postgres(o) = &self.engine
            && o.format != PgDumpFormat::Plain
        {
            return Err(BackupError::Invalid(format!(
                "encryption needs a single output stream, and the {:?} format writes its own                  archive. Use the plain format to encrypt a PostgreSQL backup.",
                o.format
            )));
        }

        if self.common.encrypt
            && let EngineBackupOptions::Mysql(o) = &self.engine
            && o.parallel_threads.is_some()
        {
            return Err(BackupError::Invalid(
                "encryption needs a single output stream, and a parallel dump writes a directory                  of files. Turn off one or the other."
                    .into(),
            ));
        }

        // A WHERE filter on a schema-only table silently does nothing; that is
        // almost always a mistake in the plan.
        for s in &self.common.selections {
            if s.where_filter.is_some() && s.mode != TableMode::SchemaAndData {
                return Err(BackupError::Invalid(format!(
                    "table {:?} has a row filter but is not set to schema+data",
                    s.name
                )));
            }
        }

        // ── What MongoDB cannot express, refused rather than approximated ──
        //
        // Both of these follow the rule this project settled in M11: a
        // silently-ignored option that makes the manifest describe an artifact
        // it did not produce is worse than a missing feature, because the
        // restore path and the drill both believe the manifest.
        if matches!(self.engine, EngineBackupOptions::Mongo(_)) {
            // A "schema-only collection" would be a collection with its indexes
            // and no documents. `mongodump` writes one archive in one pass and
            // has no per-collection document filter within it, so the choice is
            // the whole collection or none of it. Approximating either way puts
            // a name in the manifest's `tables` that the drill then looks for
            // and cannot find, or leaves one out that is really there.
            if let Some(s) = self
                .common
                .selections
                .iter()
                .find(|s| s.mode == TableMode::SchemaOnly)
            {
                return Err(BackupError::Invalid(format!(
                    "collection {:?} is set to structure-only, which mongodump cannot do in a \
                     single archive. Set it to include documents, or exclude it.",
                    s.name
                )));
            }

            // `mongodump --query` exists, but it applies to one collection and
            // demands `--collection`, which is incompatible with dumping a
            // whole database into one archive.
            if let Some(s) = self
                .common
                .selections
                .iter()
                .find(|s| s.where_filter.is_some())
            {
                return Err(BackupError::Invalid(format!(
                    "collection {:?} has a row filter, which mongodump cannot apply while \
                     dumping a whole database into one archive — its --query takes a single \
                     --collection. Remove the filter, or exclude the collection.",
                    s.name
                )));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{DbConfig, ToolOverrides};
    use crate::types::EnvironmentTag;
    use chrono::Utc;
    use uuid::Uuid;

    fn profile(engine: Engine) -> ConnectionProfile {
        ConnectionProfile {
            id: Uuid::new_v4(),
            name: "p".into(),
            engine,
            environment: EnvironmentTag::Dev,
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

    fn common() -> CommonBackupOptions {
        CommonBackupOptions {
            database: "app".into(),
            selections: vec![
                TableSelection::with_data("orders"),
                TableSelection::schema_only("audit_log"),
            ],
            output_dir: PathBuf::from("/tmp"),
            compress: true,
            encrypt: false,
            record_row_counts: false,
        }
    }

    #[test]
    fn engine_mismatch_is_rejected() {
        let req = BackupRequest {
            common: common(),
            engine: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
        };
        assert!(matches!(
            req.validate(&profile(Engine::Postgres)),
            Err(BackupError::EngineMismatch { .. })
        ));
    }

    #[test]
    fn matching_engine_validates() {
        let req = BackupRequest {
            common: common(),
            engine: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
        };
        assert!(req.validate(&profile(Engine::Mysql)).is_ok());
    }

    #[test]
    fn parallel_pg_dump_requires_directory_format() {
        let mut opts = PostgresBackupOptions {
            parallel_jobs: Some(4),
            ..Default::default()
        };
        opts.format = PgDumpFormat::Custom;

        let req = BackupRequest {
            common: common(),
            engine: EngineBackupOptions::Postgres(opts.clone()),
        };
        assert!(req.validate(&profile(Engine::Postgres)).is_err());

        opts.format = PgDumpFormat::Directory;
        let req = BackupRequest {
            common: common(),
            engine: EngineBackupOptions::Postgres(opts),
        };
        assert!(req.validate(&profile(Engine::Postgres)).is_ok());
    }

    #[test]
    fn excluding_everything_is_rejected() {
        let mut c = common();
        for s in &mut c.selections {
            s.mode = TableMode::Exclude;
        }
        let req = BackupRequest {
            common: c,
            engine: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
        };
        assert!(req.validate(&profile(Engine::Mysql)).is_err());
    }

    #[test]
    fn row_filter_on_schema_only_table_is_rejected() {
        let mut c = common();
        c.selections.push(TableSelection {
            name: "sessions".into(),
            mode: TableMode::SchemaOnly,
            where_filter: Some("created_at > NOW()".into()),
        });
        let req = BackupRequest {
            common: c,
            engine: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
        };
        assert!(req.validate(&profile(Engine::Mysql)).is_err());
    }

    // ── Encryption ──────────────────────────────────────────────────────

    #[test]
    fn encryption_is_refused_for_postgres_archive_formats() {
        // These formats have pg_dump write its own files, so there is no
        // stream to encrypt. Producing a plaintext artifact for a request that
        // asked for encryption is the one outcome that must never happen.
        for format in [PgDumpFormat::Custom, PgDumpFormat::Directory] {
            let mut c = common();
            c.encrypt = true;
            let req = BackupRequest {
                common: c,
                engine: EngineBackupOptions::Postgres(PostgresBackupOptions {
                    format,
                    ..Default::default()
                }),
            };
            let err = req.validate(&profile(Engine::Postgres)).unwrap_err();
            assert!(
                err.to_string().contains("single output stream"),
                "{format:?} should be refused, got: {err}"
            );
        }
    }

    #[test]
    fn encryption_is_allowed_for_the_plain_postgres_format() {
        let mut c = common();
        c.encrypt = true;
        let req = BackupRequest {
            common: c,
            engine: EngineBackupOptions::Postgres(PostgresBackupOptions {
                format: PgDumpFormat::Plain,
                ..Default::default()
            }),
        };
        assert!(req.validate(&profile(Engine::Postgres)).is_ok());
    }

    #[test]
    fn a_parallel_mysql_dump_is_refused_rather_than_ignored() {
        // The dangerous shape this guards: `parallel_threads` flips
        // `artifact_format()` to `MydumperDir`, whose `is_directory()` is true,
        // but nothing in the MySQL backend reads the option. Ignoring it would
        // write one gzipped file and label it a directory.
        let req = BackupRequest {
            common: common(),
            engine: EngineBackupOptions::Mysql(MysqlBackupOptions {
                parallel_threads: Some(4),
                ..Default::default()
            }),
        };
        let err = req.validate(&profile(Engine::Mysql)).unwrap_err();
        assert!(matches!(err, BackupError::NotImplemented(_)), "got: {err}");
        assert!(err.to_string().contains("parallel_threads"), "got: {err}");
    }

    #[test]
    fn the_format_a_refused_option_would_have_declared_is_a_directory() {
        // Documents *why* the check above has to exist. If this ever stops
        // being a directory format, the refusal can be revisited.
        let parallel = EngineBackupOptions::Mysql(MysqlBackupOptions {
            parallel_threads: Some(4),
            ..Default::default()
        });
        assert!(parallel.artifact_format().is_directory());
        assert!(
            !EngineBackupOptions::Mysql(MysqlBackupOptions::default())
                .artifact_format()
                .is_directory()
        );
    }

    #[test]
    fn an_ordinary_mysql_backup_may_be_encrypted() {
        let mut c = common();
        c.encrypt = true;
        let req = BackupRequest {
            common: c,
            engine: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
        };
        assert!(req.validate(&profile(Engine::Mysql)).is_ok());
    }

    #[test]
    fn mysql_defaults_are_production_safe() {
        let o = MysqlBackupOptions::default();
        assert!(o.single_transaction, "must not lock a production source");
        assert!(o.hex_blob, "binary columns corrupt without this");
        assert!(o.strip_definer, "restores must not need SUPER");
    }

    #[test]
    fn postgres_defaults_favour_portable_restores() {
        let o = PostgresBackupOptions::default();
        assert_eq!(o.format, PgDumpFormat::Custom);
        assert!(o.no_owner);
        assert!(o.no_privileges);
        assert!(o.format.artifact_format().supports_selective_restore());
    }

    #[test]
    fn artifact_format_tracks_parallel_mode() {
        let seq = EngineBackupOptions::Mysql(MysqlBackupOptions::default());
        assert_eq!(seq.artifact_format(), ArtifactFormat::SqlGz);

        let par = EngineBackupOptions::Mysql(MysqlBackupOptions {
            parallel_threads: Some(4),
            ..Default::default()
        });
        assert_eq!(par.artifact_format(), ArtifactFormat::MydumperDir);
    }

    #[test]
    fn selection_helpers_partition_correctly() {
        let c = common();
        assert_eq!(c.tables_with_data().len(), 1);
        assert_eq!(c.included_tables().len(), 2);
    }
}
