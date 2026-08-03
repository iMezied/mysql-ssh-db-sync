//! SQLite persistence.
//!
//! This lives in the engine, not in the Tauri layer, so that `engine-cli` can
//! read the same profiles and write the same job history as the GUI. Anything
//! that puts row-mapping in the desktop crate breaks headless parity.

use std::path::Path;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::backup::TableSelection;
use crate::destination::{Destination, DestinationCreate, DestinationUpdate};
use crate::events::JobKind;
use crate::job::{JobOutcome, JobRecord};
use crate::pipeline::{Pipeline, PipelineCreate, PipelineUpdate};
use crate::plan::{SyncPlan, SyncPlanCreate};
use crate::profile::{ConnectionProfile, DbConfig, ProfileCreate, ProfileUpdate, ToolOverrides};
use crate::schedule::{NotifyPolicy, Schedule, ScheduleCreate, ScheduleKind, ScheduleUpdate};
use crate::settings;
use crate::sshconn::{
    ResolvedSsh, SshConfig, SshConnection, SshConnectionCreate, SshConnectionError,
    SshConnectionUpdate, SshEndpoint,
};
use crate::step::{JobStep, JobStepDetail, JobStepKind, JobStepOutcome};
use crate::types::{Engine, EnvironmentTag};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("stored row is corrupt ({field}): {source}")]
    Corrupt {
        field: &'static str,
        #[source]
        source: anyhow::Error,
    },
    #[error("no profile with id {0}")]
    ProfileNotFound(Uuid),
    #[error("a profile named {0:?} already exists")]
    DuplicateName(String),
    #[error("no sync plan with id {0}")]
    SyncPlanNotFound(Uuid),
    #[error("no schedule with id {0}")]
    ScheduleNotFound(Uuid),
    #[error("no destination with id {0}")]
    DestinationNotFound(Uuid),
    #[error("no pipeline with id {0}")]
    PipelineNotFound(Uuid),
    #[error(transparent)]
    InvalidPipeline(crate::pipeline::PipelineError),
    #[error(transparent)]
    InvalidSchedule(crate::schedule::ScheduleError),
    #[error(transparent)]
    InvalidDestination(crate::destination::DestinationError),
    #[error(transparent)]
    InvalidSshConnection(#[from] SshConnectionError),
    #[error(transparent)]
    Secrets(crate::secrets::SecretError),
}

type Result<T> = std::result::Result<T, StoreError>;

fn corrupt(field: &'static str, e: impl Into<anyhow::Error>) -> StoreError {
    StoreError::Corrupt {
        field,
        source: e.into(),
    }
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open (creating if absent) the store at `path` and run migrations.
    ///
    /// `:memory:` is accepted and gives an ephemeral store, which tests use.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            // Referential integrity is off by default in SQLite; sync_plans
            // depends on it to clean up when a profile is deleted.
            .foreign_keys(true);

        // Every connection to `:memory:` opens a *separate* database, so a
        // multi-connection pool would run the migrations on one connection and
        // then serve queries from empty ones — surfacing as "no such table"
        // long after the mistake. Pinning to a single connection makes an
        // in-memory store behave the way anyone writing one would expect.
        let max_connections = if path == Path::new(":memory:") { 1 } else { 5 };

        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await?;

        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    // ── Profiles ────────────────────────────────────────────────────────

    pub async fn list_profiles(&self) -> Result<Vec<ConnectionProfile>> {
        let rows = sqlx::query(&format!("SELECT {PROFILE_COLUMNS} FROM profiles ORDER BY name"))
            .fetch_all(&self.pool)
            .await?;

        rows.into_iter().map(row_to_profile).collect()
    }

    pub async fn get_profile(&self, id: Uuid) -> Result<Option<ConnectionProfile>> {
        let row = sqlx::query(&format!(
            "SELECT {PROFILE_COLUMNS} FROM profiles WHERE id = ?1"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(row_to_profile).transpose()
    }

    /// Every profile that tunnels through a given SSH connection.
    ///
    /// What makes "you cannot delete this, three databases go through it"
    /// answerable by name rather than by count.
    pub async fn profiles_using_ssh_connection(
        &self,
        ssh_connection_id: Uuid,
    ) -> Result<Vec<ConnectionProfile>> {
        let rows = sqlx::query(&format!(
            "SELECT {PROFILE_COLUMNS} FROM profiles WHERE ssh_connection_id = ?1 ORDER BY name"
        ))
        .bind(ssh_connection_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(row_to_profile).collect()
    }

    /// Like [`get_profile`] but errors when absent, for callers that require one.
    pub async fn require_profile(&self, id: Uuid) -> Result<ConnectionProfile> {
        self.get_profile(id)
            .await?
            .ok_or(StoreError::ProfileNotFound(id))
    }

    pub async fn create_profile(&self, input: ProfileCreate) -> Result<ConnectionProfile> {
        // Checked here rather than left to the foreign key, so a bad reference
        // reads as "no SSH connection with id …" instead of a constraint error.
        if let Some(id) = input.ssh_connection_id {
            self.require_ssh_connection(id).await?;
        }

        let now = Utc::now();
        let profile = ConnectionProfile {
            id: Uuid::new_v4(),
            name: input.name,
            engine: input.engine,
            environment: input.environment,
            ssh_connection_id: input.ssh_connection_id,
            db: input.db,
            tool_overrides: input.tool_overrides,
            created_at: now,
            updated_at: now,
        };

        self.insert_profile(&profile).await?;
        Ok(profile)
    }

    async fn insert_profile(&self, p: &ConnectionProfile) -> Result<()> {
        let result = sqlx::query(
            "INSERT INTO profiles (id, name, engine, environment, ssh_connection_id, db_config, \
             tool_overrides, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(p.id.to_string())
        .bind(&p.name)
        .bind(p.engine.as_str())
        .bind(p.environment.as_str())
        .bind(p.ssh_connection_id.map(|u| u.to_string()))
        .bind(serde_json::to_string(&p.db).map_err(|e| corrupt("db_config", e))?)
        .bind(serde_json::to_string(&p.tool_overrides).map_err(|e| corrupt("tool_overrides", e))?)
        .bind(p.created_at.to_rfc3339())
        .bind(p.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(()),
            Err(e) if is_unique_violation(&e) => Err(StoreError::DuplicateName(p.name.clone())),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn update_profile(
        &self,
        id: Uuid,
        patch: ProfileUpdate,
    ) -> Result<ConnectionProfile> {
        let mut p = self.require_profile(id).await?;

        if let Some(v) = patch.name {
            p.name = v;
        }
        if let Some(v) = patch.engine {
            p.engine = v;
        }
        if let Some(v) = patch.environment {
            p.environment = v;
        }
        // Doubly-optional: Some(None) detaches the tunnel, None leaves alone.
        if let Some(v) = patch.ssh_connection_id {
            if let Some(id) = v {
                self.require_ssh_connection(id).await?;
            }
            p.ssh_connection_id = v;
        }
        if let Some(v) = patch.db {
            p.db = v;
        }
        if let Some(v) = patch.tool_overrides {
            p.tool_overrides = v;
        }
        p.updated_at = Utc::now();

        let result = sqlx::query(
            "UPDATE profiles SET name = ?2, engine = ?3, environment = ?4, \
             ssh_connection_id = ?5, db_config = ?6, tool_overrides = ?7, updated_at = ?8 \
             WHERE id = ?1",
        )
        .bind(p.id.to_string())
        .bind(&p.name)
        .bind(p.engine.as_str())
        .bind(p.environment.as_str())
        .bind(p.ssh_connection_id.map(|u| u.to_string()))
        .bind(serde_json::to_string(&p.db).map_err(|e| corrupt("db_config", e))?)
        .bind(serde_json::to_string(&p.tool_overrides).map_err(|e| corrupt("tool_overrides", e))?)
        .bind(p.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(p),
            Err(e) if is_unique_violation(&e) => Err(StoreError::DuplicateName(p.name.clone())),
            Err(e) => Err(e.into()),
        }
    }

    /// Delete a profile. Callers are responsible for purging its keychain
    /// entries via [`crate::secrets::delete_all_for_profile`].
    pub async fn delete_profile(&self, id: Uuid) -> Result<bool> {
        let res = sqlx::query("DELETE FROM profiles WHERE id = ?1")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    // ── Job history ─────────────────────────────────────────────────────

    pub async fn insert_job(&self, rec: &JobRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO job_history (id, kind, source_profile_id, dest_profile_id, started_at, \
             finished_at, outcome, artifact_path, options_json, log) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(rec.id.to_string())
        .bind(job_kind_str(rec.kind))
        .bind(rec.source_profile_id.to_string())
        .bind(rec.dest_profile_id.map(|u| u.to_string()))
        .bind(rec.started_at.to_rfc3339())
        .bind(rec.finished_at.map(|t| t.to_rfc3339()))
        .bind(rec.outcome.map(|o| o.as_str()))
        .bind(&rec.artifact_path)
        .bind(&rec.options_json)
        .bind(&rec.log)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record the terminal state of a job, including its full log.
    pub async fn finish_job(
        &self,
        id: Uuid,
        outcome: JobOutcome,
        artifact_path: Option<String>,
        log: String,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE job_history SET finished_at = ?2, outcome = ?3, artifact_path = ?4, log = ?5 \
             WHERE id = ?1",
        )
        .bind(id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind(outcome.as_str())
        .bind(artifact_path)
        .bind(log)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_jobs(&self, limit: i64) -> Result<Vec<JobRecord>> {
        let rows = sqlx::query(
            "SELECT id, kind, source_profile_id, dest_profile_id, started_at, finished_at, \
             outcome, artifact_path, options_json, log FROM job_history \
             ORDER BY started_at DESC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(row_to_job).collect()
    }

    pub async fn get_job(&self, id: Uuid) -> Result<Option<JobRecord>> {
        let row = sqlx::query(
            "SELECT id, kind, source_profile_id, dest_profile_id, started_at, finished_at, \
             outcome, artifact_path, options_json, log FROM job_history WHERE id = ?1",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(row_to_job).transpose()
    }

    // ── Job steps ───────────────────────────────────────────────────────

    /// Write down every step a job intends to run, before the first one starts.
    ///
    /// Replaces any existing plan for the job, so a re-run of the same id — the
    /// scheduler's `run_now` on a schedule that is already recorded — does not
    /// interleave two plans.
    pub async fn plan_job_steps(
        &self,
        job_id: Uuid,
        steps: &[(JobStepKind, String)],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM job_steps WHERE job_id = ?1")
            .bind(job_id.to_string())
            .execute(&mut *tx)
            .await?;

        for (i, (kind, label)) in steps.iter().enumerate() {
            sqlx::query("INSERT INTO job_steps (job_id, idx, kind, label) VALUES (?1, ?2, ?3, ?4)")
                .bind(job_id.to_string())
                .bind(i as i64 + 1)
                .bind(kind.as_str())
                .bind(label)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn begin_job_step(&self, job_id: Uuid, index: u32) -> Result<()> {
        sqlx::query("UPDATE job_steps SET started_at = ?3 WHERE job_id = ?1 AND idx = ?2")
            .bind(job_id.to_string())
            .bind(i64::from(index))
            .bind(Utc::now().to_rfc3339())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn finish_job_step(
        &self,
        job_id: Uuid,
        index: u32,
        outcome: JobStepOutcome,
        detail: &JobStepDetail,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE job_steps SET finished_at = ?3, outcome = ?4, detail_json = ?5 \
             WHERE job_id = ?1 AND idx = ?2",
        )
        .bind(job_id.to_string())
        .bind(i64::from(index))
        .bind(Utc::now().to_rfc3339())
        .bind(outcome.as_str())
        .bind(serde_json::to_string(detail).unwrap_or_else(|_| "{}".into()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Settle every step a finished job left open.
    ///
    /// The one that was running takes `outcome` and the job's error message;
    /// the ones never reached are `skipped`. Called from
    /// [`crate::ops::record_finish`] rather than from each operation, so an
    /// early return through `?` anywhere in a six-step sync is covered without
    /// the operations having to know these rows exist.
    pub async fn close_open_steps(
        &self,
        job_id: Uuid,
        outcome: JobStepOutcome,
        error: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();

        // A step that started but never finished is the one that failed.
        let detail = serde_json::to_string(&JobStepDetail {
            error: error.map(str::to_owned),
            ..JobStepDetail::default()
        })
        .unwrap_or_else(|_| "{}".into());

        sqlx::query(
            "UPDATE job_steps SET finished_at = ?2, outcome = ?3, detail_json = ?4 \
             WHERE job_id = ?1 AND outcome IS NULL AND started_at IS NOT NULL",
        )
        .bind(job_id.to_string())
        .bind(&now)
        .bind(outcome.as_str())
        .bind(detail)
        .execute(&self.pool)
        .await?;

        // Everything after it never ran. No finished_at: it did not end,
        // it never began, and a duration of zero would imply otherwise.
        sqlx::query(
            "UPDATE job_steps SET outcome = ?2 \
             WHERE job_id = ?1 AND outcome IS NULL AND started_at IS NULL",
        )
        .bind(job_id.to_string())
        .bind(JobStepOutcome::Skipped.as_str())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn list_job_steps(&self, job_id: Uuid) -> Result<Vec<JobStep>> {
        let rows = sqlx::query(&format!(
            "SELECT {JOB_STEP_COLUMNS} FROM job_steps WHERE job_id = ?1 ORDER BY idx"
        ))
        .bind(job_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(row_to_job_step).collect()
    }

    // ── Pipelines ───────────────────────────────────────────────────────

    pub async fn list_pipelines(&self) -> Result<Vec<Pipeline>> {
        let rows = sqlx::query(&format!(
            "SELECT {PIPELINE_COLUMNS} FROM pipelines ORDER BY name"
        ))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(row_to_pipeline).collect()
    }

    pub async fn get_pipeline(&self, id: Uuid) -> Result<Option<Pipeline>> {
        let row = sqlx::query(&format!(
            "SELECT {PIPELINE_COLUMNS} FROM pipelines WHERE id = ?1"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(row_to_pipeline).transpose()
    }

    pub async fn require_pipeline(&self, id: Uuid) -> Result<Pipeline> {
        self.get_pipeline(id)
            .await?
            .ok_or(StoreError::PipelineNotFound(id))
    }

    /// Validated before it is written, so no path — CLI, import, IPC — can
    /// store a pipeline the runner would have to refuse later.
    pub async fn create_pipeline(&self, input: PipelineCreate) -> Result<Pipeline> {
        input.validate().map_err(StoreError::InvalidPipeline)?;

        let now = Utc::now();
        let pipeline = Pipeline {
            id: Uuid::new_v4(),
            name: input.name.trim().to_string(),
            steps: input.steps,
            unattended_ack: None,
            created_at: now,
            updated_at: now,
        };

        sqlx::query(
            "INSERT INTO pipelines (id, name, steps_json, unattended_ack, created_at, \
             updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(pipeline.id.to_string())
        .bind(&pipeline.name)
        .bind(serde_json::to_string(&pipeline.steps).map_err(|e| corrupt("steps_json", e))?)
        .bind(Option::<String>::None)
        .bind(pipeline.created_at.to_rfc3339())
        .bind(pipeline.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                StoreError::DuplicateName(pipeline.name.clone())
            } else {
                StoreError::Sqlx(e)
            }
        })?;

        Ok(pipeline)
    }

    /// Apply a patch, then re-validate the result.
    ///
    /// After, not before: a patch that is fine on its own can still leave the
    /// pipeline invalid — removing the backup step out from under a restore
    /// that consumes it — and the stored row is what the runner will read.
    ///
    /// Editing the steps clears `unattended_ack`. Permission to drop a database
    /// unattended is granted for the targets somebody typed, and re-checking
    /// the signature on read would already catch a rename; clearing it here as
    /// well means an edit that happens to keep the same names still costs a
    /// deliberate re-arm.
    pub async fn update_pipeline(&self, id: Uuid, patch: PipelineUpdate) -> Result<Pipeline> {
        let mut pipeline = self.require_pipeline(id).await?;
        let steps_changed = patch.steps.is_some();

        if let Some(name) = patch.name {
            pipeline.name = name.trim().to_string();
        }
        if let Some(steps) = patch.steps {
            pipeline.steps = steps;
        }
        if steps_changed {
            pipeline.unattended_ack = None;
        }
        pipeline.updated_at = Utc::now();

        pipeline.validate().map_err(StoreError::InvalidPipeline)?;

        sqlx::query(
            "UPDATE pipelines SET name = ?2, steps_json = ?3, unattended_ack = ?4, \
             updated_at = ?5 WHERE id = ?1",
        )
        .bind(id.to_string())
        .bind(&pipeline.name)
        .bind(serde_json::to_string(&pipeline.steps).map_err(|e| corrupt("steps_json", e))?)
        .bind(pipeline.unattended_ack.as_deref())
        .bind(pipeline.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                StoreError::DuplicateName(pipeline.name.clone())
            } else {
                StoreError::Sqlx(e)
            }
        })?;

        Ok(pipeline)
    }

    /// Record that a human typed the destructive targets back, authorising
    /// this pipeline to run with nobody present.
    ///
    /// `typed` is what they typed; it must match the pipeline's current
    /// signature exactly. Passing `None` disarms. A pipeline that destroys
    /// nothing cannot be armed, because there is nothing to authorise.
    pub async fn arm_pipeline(&self, id: Uuid, typed: Option<&str>) -> Result<Pipeline> {
        let mut pipeline = self.require_pipeline(id).await?;

        let ack = match typed {
            None => None,
            Some(typed) => {
                let expected = pipeline.destructive_signature().ok_or_else(|| {
                    StoreError::InvalidPipeline(crate::pipeline::PipelineError::NothingToAuthorise)
                })?;
                if typed != expected {
                    return Err(StoreError::InvalidPipeline(
                        crate::pipeline::PipelineError::ConfirmationDoesNotMatch {
                            expected,
                            got: typed.to_string(),
                        },
                    ));
                }
                Some(expected)
            }
        };

        pipeline.unattended_ack = ack;
        pipeline.updated_at = Utc::now();

        sqlx::query("UPDATE pipelines SET unattended_ack = ?2, updated_at = ?3 WHERE id = ?1")
            .bind(id.to_string())
            .bind(pipeline.unattended_ack.as_deref())
            .bind(pipeline.updated_at.to_rfc3339())
            .execute(&self.pool)
            .await?;

        Ok(pipeline)
    }

    pub async fn delete_pipeline(&self, id: Uuid) -> Result<bool> {
        let res = sqlx::query("DELETE FROM pipelines WHERE id = ?1")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    // ── Known hosts ─────────────────────────────────────────────────────

    pub async fn get_known_host(&self, host_port: &str) -> Result<Option<(String, String)>> {
        let row = sqlx::query("SELECT key_type, fingerprint FROM known_hosts WHERE host_port = ?1")
            .bind(host_port)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.map(|r| {
            (
                r.get::<String, _>("key_type"),
                r.get::<String, _>("fingerprint"),
            )
        }))
    }

    pub async fn remember_host(
        &self,
        host_port: &str,
        key_type: &str,
        fingerprint: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO known_hosts (host_port, key_type, fingerprint, first_seen) \
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT (host_port) DO NOTHING",
        )
        .bind(host_port)
        .bind(key_type)
        .bind(fingerprint)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replace a pinned host key. Only call after the user has explicitly
    /// accepted the change — a silently rotating key is indistinguishable from
    /// a machine-in-the-middle.
    pub async fn replace_host_key(
        &self,
        host_port: &str,
        key_type: &str,
        fingerprint: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO known_hosts (host_port, key_type, fingerprint, first_seen) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (host_port) DO UPDATE SET key_type = ?2, fingerprint = ?3",
        )
        .bind(host_port)
        .bind(key_type)
        .bind(fingerprint)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

const fn job_kind_str(k: JobKind) -> &'static str {
    match k {
        JobKind::Backup => "backup",
        JobKind::Restore => "restore",
        JobKind::Verify => "verify",
        JobKind::Sync => "sync",
    }
}

fn parse_job_kind(s: &str) -> Option<JobKind> {
    match s {
        "backup" => Some(JobKind::Backup),
        "restore" => Some(JobKind::Restore),
        "verify" => Some(JobKind::Verify),
        "sync" => Some(JobKind::Sync),
        _ => None,
    }
}

fn parse_outcome(s: &str) -> Option<JobOutcome> {
    match s {
        "success" => Some(JobOutcome::Success),
        "failed" => Some(JobOutcome::Failed),
        "cancelled" => Some(JobOutcome::Cancelled),
        _ => None,
    }
}

fn parse_uuid(s: &str, field: &'static str) -> Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| corrupt(field, e))
}

fn parse_ts(s: &str, field: &'static str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| corrupt(field, e))
}

/// Every column [`row_to_profile`] reads, in one place so the queries above
/// cannot drift apart from it.
const PROFILE_COLUMNS: &str = "id, name, engine, environment, ssh_connection_id, db_config, \
     tool_overrides, created_at, updated_at";

fn row_to_profile(row: sqlx::sqlite::SqliteRow) -> Result<ConnectionProfile> {
    let ssh_raw: Option<String> = row.get("ssh_connection_id");
    let tools_raw: String = row.get("tool_overrides");
    let db_raw: String = row.get("db_config");

    Ok(ConnectionProfile {
        id: parse_uuid(&row.get::<String, _>("id"), "id")?,
        name: row.get("name"),
        engine: Engine::from_str(&row.get::<String, _>("engine"))
            .map_err(|e| corrupt("engine", e))?,
        environment: EnvironmentTag::from_str(&row.get::<String, _>("environment"))
            .map_err(|e| corrupt("environment", e))?,
        // An unreadable reference is corruption, not "no SSH" — surfacing it
        // beats silently turning a tunnelled profile into a direct connection.
        ssh_connection_id: ssh_raw
            .map(|s| parse_uuid(&s, "ssh_connection_id"))
            .transpose()?,
        db: serde_json::from_str::<DbConfig>(&db_raw).map_err(|e| corrupt("db_config", e))?,
        tool_overrides: serde_json::from_str::<ToolOverrides>(&tools_raw)
            .map_err(|e| corrupt("tool_overrides", e))?,
        created_at: parse_ts(&row.get::<String, _>("created_at"), "created_at")?,
        updated_at: parse_ts(&row.get::<String, _>("updated_at"), "updated_at")?,
    })
}

/// Every column [`row_to_pipeline`] reads, in one place so the queries above
/// cannot drift apart from it.
const PIPELINE_COLUMNS: &str = "id, name, steps_json, unattended_ack, created_at, updated_at";

fn row_to_pipeline(row: sqlx::sqlite::SqliteRow) -> Result<Pipeline> {
    let steps_raw: String = row.get("steps_json");

    Ok(Pipeline {
        id: parse_uuid(&row.get::<String, _>("id"), "id")?,
        name: row.get("name"),
        // Unlike a step's detail blob, this one is the pipeline. A definition
        // that cannot be read is corruption worth surfacing — running half of
        // it would be worse than refusing to list it.
        steps: serde_json::from_str(&steps_raw).map_err(|e| corrupt("steps_json", e))?,
        unattended_ack: row.get("unattended_ack"),
        created_at: parse_ts(&row.get::<String, _>("created_at"), "created_at")?,
        updated_at: parse_ts(&row.get::<String, _>("updated_at"), "updated_at")?,
    })
}

/// Every column [`row_to_job_step`] reads, in one place so the query above
/// cannot drift apart from it.
const JOB_STEP_COLUMNS: &str =
    "job_id, idx, kind, label, started_at, finished_at, outcome, detail_json";

fn row_to_job_step(row: sqlx::sqlite::SqliteRow) -> Result<JobStep> {
    let kind_raw: String = row.get("kind");
    let started_raw: Option<String> = row.get("started_at");
    let finished_raw: Option<String> = row.get("finished_at");
    let outcome_raw: Option<String> = row.get("outcome");
    let detail_raw: String = row.get("detail_json");

    Ok(JobStep {
        job_id: parse_uuid(&row.get::<String, _>("job_id"), "job_id")?,
        index: row.get::<i64, _>("idx") as u32,
        kind: JobStepKind::parse(&kind_raw)
            .ok_or_else(|| corrupt("kind", anyhow::anyhow!("unknown step kind {kind_raw:?}")))?,
        label: row.get("label"),
        started_at: started_raw
            .map(|s| parse_ts(&s, "started_at"))
            .transpose()?,
        finished_at: finished_raw
            .map(|s| parse_ts(&s, "finished_at"))
            .transpose()?,
        outcome: outcome_raw.as_deref().and_then(JobStepOutcome::parse),
        // Unlike every other JSON column here, an unreadable blob is not
        // corruption worth failing on: this is the annotation beside a step,
        // and losing it must not stop the job page from rendering the step.
        detail: serde_json::from_str(&detail_raw).unwrap_or_default(),
    })
}

fn row_to_job(row: sqlx::sqlite::SqliteRow) -> Result<JobRecord> {
    let finished_raw: Option<String> = row.get("finished_at");
    let outcome_raw: Option<String> = row.get("outcome");
    let dest_raw: Option<String> = row.get("dest_profile_id");
    let kind_raw: String = row.get("kind");

    Ok(JobRecord {
        id: parse_uuid(&row.get::<String, _>("id"), "id")?,
        kind: parse_job_kind(&kind_raw)
            .ok_or_else(|| corrupt("kind", anyhow::anyhow!("unknown job kind {kind_raw:?}")))?,
        source_profile_id: parse_uuid(
            &row.get::<String, _>("source_profile_id"),
            "source_profile_id",
        )?,
        dest_profile_id: dest_raw
            .map(|s| parse_uuid(&s, "dest_profile_id"))
            .transpose()?,
        started_at: parse_ts(&row.get::<String, _>("started_at"), "started_at")?,
        finished_at: finished_raw
            .map(|s| parse_ts(&s, "finished_at"))
            .transpose()?,
        outcome: outcome_raw.as_deref().and_then(parse_outcome),
        artifact_path: row.get("artifact_path"),
        options_json: row.get("options_json"),
        log: row.get("log"),
    })
}

// ── Sync plans ──────────────────────────────────────────────────────────

impl Store {
    pub async fn list_sync_plans(&self, profile_id: Uuid) -> Result<Vec<SyncPlan>> {
        let rows = sqlx::query(
            "SELECT id, profile_id, name, database_name, table_selections, masking, revision, \
             created_at, updated_at FROM sync_plans WHERE profile_id = ?1 ORDER BY name",
        )
        .bind(profile_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(row_to_plan).collect()
    }

    pub async fn get_sync_plan(&self, id: Uuid) -> Result<Option<SyncPlan>> {
        let row = sqlx::query(
            "SELECT id, profile_id, name, database_name, table_selections, masking, revision, \
             created_at, updated_at FROM sync_plans WHERE id = ?1",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(row_to_plan).transpose()
    }

    pub async fn create_sync_plan(&self, input: SyncPlanCreate) -> Result<SyncPlan> {
        let now = Utc::now();
        let plan = SyncPlan {
            id: Uuid::new_v4(),
            profile_id: input.profile_id,
            name: input.name,
            database: input.database,
            selections: input.selections,
            masking: input.masking,
            revision: 1,
            created_at: now,
            updated_at: now,
        };

        sqlx::query(
            "INSERT INTO sync_plans (id, profile_id, name, database_name, table_selections, \
             masking, revision, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(plan.id.to_string())
        .bind(plan.profile_id.to_string())
        .bind(&plan.name)
        .bind(&plan.database)
        .bind(serde_json::to_string(&plan.selections).map_err(|e| corrupt("table_selections", e))?)
        .bind(serde_json::to_string(&plan.masking).map_err(|e| corrupt("masking", e))?)
        .bind(plan.revision)
        .bind(plan.created_at.to_rfc3339())
        .bind(plan.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                StoreError::DuplicateName(plan.name.clone())
            } else {
                e.into()
            }
        })?;

        Ok(plan)
    }

    /// Rename a table set.
    ///
    /// Deliberately does **not** bump `revision`. A revision means "what this
    /// set backs up changed", and a schedule pointing at it has nothing to
    /// re-examine because the selections are untouched — bumping would cry wolf
    /// on every typo fix.
    pub async fn rename_sync_plan(&self, id: Uuid, name: String) -> Result<SyncPlan> {
        let mut plan = self
            .get_sync_plan(id)
            .await?
            .ok_or(StoreError::SyncPlanNotFound(id))?;

        plan.name = name;
        plan.updated_at = Utc::now();

        sqlx::query("UPDATE sync_plans SET name = ?2, updated_at = ?3 WHERE id = ?1")
            .bind(plan.id.to_string())
            .bind(&plan.name)
            .bind(plan.updated_at.to_rfc3339())
            .execute(&self.pool)
            .await
            .map_err(|e| {
                if is_unique_violation(&e) {
                    StoreError::DuplicateName(plan.name.clone())
                } else {
                    e.into()
                }
            })?;

        Ok(plan)
    }

    /// Replace a plan's selections, bumping its revision.
    ///
    /// The revision is what makes a plan that changed under a schedule visible
    /// rather than silent.
    pub async fn update_sync_plan(
        &self,
        id: Uuid,
        selections: Vec<TableSelection>,
    ) -> Result<SyncPlan> {
        let mut plan = self
            .get_sync_plan(id)
            .await?
            .ok_or(StoreError::SyncPlanNotFound(id))?;

        plan.selections = selections;
        plan.revision += 1;
        plan.updated_at = Utc::now();

        sqlx::query(
            "UPDATE sync_plans SET table_selections = ?2, revision = ?3, updated_at = ?4 \
             WHERE id = ?1",
        )
        .bind(plan.id.to_string())
        .bind(serde_json::to_string(&plan.selections).map_err(|e| corrupt("table_selections", e))?)
        .bind(plan.revision)
        .bind(plan.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(plan)
    }

    /// Replace a plan's masking rules, bumping its revision.
    ///
    /// Separate from [`Store::update_sync_plan`] because the two are edited in
    /// different places and for different reasons — and because bumping the
    /// revision here is the point: a schedule whose masking changed underneath
    /// it must be as visible as one whose table selection did.
    pub async fn set_sync_plan_masking(
        &self,
        id: Uuid,
        masking: Vec<crate::mask::MaskRule>,
    ) -> Result<SyncPlan> {
        let mut plan = self
            .get_sync_plan(id)
            .await?
            .ok_or(StoreError::SyncPlanNotFound(id))?;

        plan.masking = masking;
        plan.revision += 1;
        plan.updated_at = Utc::now();

        sqlx::query(
            "UPDATE sync_plans SET masking = ?2, revision = ?3, updated_at = ?4 WHERE id = ?1",
        )
        .bind(plan.id.to_string())
        .bind(serde_json::to_string(&plan.masking).map_err(|e| corrupt("masking", e))?)
        .bind(plan.revision)
        .bind(plan.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(plan)
    }

    pub async fn delete_sync_plan(&self, id: Uuid) -> Result<bool> {
        let res = sqlx::query("DELETE FROM sync_plans WHERE id = ?1")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

// ── Audit log ───────────────────────────────────────────────────────────

impl Store {
    /// Record a configuration change.
    ///
    /// Returns `()` rather than a `Result` the caller must handle, because a
    /// failure to write the audit row must never abort the operation being
    /// audited: refusing to delete a profile because the log was unwritable
    /// would be a worse outcome than an incomplete log. It is logged instead.
    pub async fn audit(
        &self,
        action: crate::audit::AuditAction,
        subject: impl Into<String>,
        detail: impl Into<String>,
    ) {
        let subject = subject.into();
        let result = sqlx::query(
            "INSERT INTO audit_log (id, at, action, subject, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(Utc::now().to_rfc3339())
        .bind(action.as_str())
        .bind(&subject)
        .bind(detail.into())
        .execute(&self.pool)
        .await;

        if let Err(e) = result {
            tracing::error!(
                "could not record {} for {subject:?} in the audit log: {e}",
                action.as_str()
            );
        }
    }

    /// Recent configuration changes, newest first.
    pub async fn list_audit(&self, limit: i64) -> Result<Vec<crate::audit::AuditEntry>> {
        let rows = sqlx::query(
            "SELECT id, at, action, subject, detail FROM audit_log ORDER BY at DESC LIMIT ?1",
        )
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(crate::audit::AuditEntry {
                    id: parse_uuid(&row.get::<String, _>("id"), "id")?,
                    at: parse_ts(&row.get::<String, _>("at"), "at")?,
                    action: row.get("action"),
                    subject: row.get("subject"),
                    detail: row.get("detail"),
                })
            })
            .collect()
    }
}

// ── Off-site destinations ───────────────────────────────────────────────

const DESTINATION_COLUMNS: &str =
    "id, name, kind, enabled, retention, created_at, updated_at FROM destinations";

impl Store {
    pub async fn list_destinations(&self) -> Result<Vec<Destination>> {
        let rows = sqlx::query(&format!("SELECT {DESTINATION_COLUMNS} ORDER BY name"))
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_destination).collect()
    }

    /// The destinations a backup actually ships to.
    pub async fn list_enabled_destinations(&self) -> Result<Vec<Destination>> {
        let rows = sqlx::query(&format!(
            "SELECT {DESTINATION_COLUMNS} WHERE enabled = 1 ORDER BY name"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_destination).collect()
    }

    pub async fn get_destination(&self, id: Uuid) -> Result<Option<Destination>> {
        let row = sqlx::query(&format!("SELECT {DESTINATION_COLUMNS} WHERE id = ?1"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_destination).transpose()
    }

    pub async fn require_destination(&self, id: Uuid) -> Result<Destination> {
        self.get_destination(id)
            .await?
            .ok_or(StoreError::DestinationNotFound(id))
    }

    pub async fn create_destination(&self, input: DestinationCreate) -> Result<Destination> {
        let now = Utc::now();
        let destination = Destination {
            id: Uuid::new_v4(),
            name: input.name.trim().to_string(),
            kind: input.kind,
            enabled: input.enabled,
            retention: input.retention,
            created_at: now,
            updated_at: now,
        };
        // Refused here rather than at the point of use: a destination that
        // cannot work is worse stored than rejected, because it looks
        // configured and only fails at 3am.
        destination
            .validate()
            .map_err(StoreError::InvalidDestination)?;

        let result = sqlx::query(
            "INSERT INTO destinations (id, name, kind, enabled, retention, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(destination.id.to_string())
        .bind(&destination.name)
        .bind(serde_json::to_string(&destination.kind).map_err(|e| corrupt("kind", e))?)
        .bind(destination.enabled)
        .bind(serde_json::to_string(&destination.retention).map_err(|e| corrupt("retention", e))?)
        .bind(destination.created_at.to_rfc3339())
        .bind(destination.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(destination),
            Err(e) if is_unique_violation(&e) => Err(StoreError::DuplicateName(destination.name)),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn update_destination(
        &self,
        id: Uuid,
        patch: DestinationUpdate,
    ) -> Result<Destination> {
        let mut d = self.require_destination(id).await?;

        if let Some(v) = patch.name {
            d.name = v.trim().to_string();
        }
        if let Some(v) = patch.kind {
            d.kind = v;
        }
        if let Some(v) = patch.enabled {
            d.enabled = v;
        }
        if let Some(v) = patch.retention {
            d.retention = v;
        }
        d.updated_at = Utc::now();
        d.validate().map_err(StoreError::InvalidDestination)?;

        let result = sqlx::query(
            "UPDATE destinations SET name = ?2, kind = ?3, enabled = ?4, retention = ?5, \
             updated_at = ?6 WHERE id = ?1",
        )
        .bind(d.id.to_string())
        .bind(&d.name)
        .bind(serde_json::to_string(&d.kind).map_err(|e| corrupt("kind", e))?)
        .bind(d.enabled)
        .bind(serde_json::to_string(&d.retention).map_err(|e| corrupt("retention", e))?)
        .bind(d.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(d),
            Err(e) if is_unique_violation(&e) => Err(StoreError::DuplicateName(d.name)),
            Err(e) => Err(e.into()),
        }
    }

    /// Remove the row only.
    ///
    /// The stored credential is **not** touched here — this module is SQLite
    /// persistence, and reaching into the OS keychain from it would make every
    /// test of this table need an unlocked one. Callers use
    /// [`crate::ops::forget_destination`], which does both in the right order.
    pub async fn delete_destination(&self, id: Uuid) -> Result<bool> {
        let res = sqlx::query("DELETE FROM destinations WHERE id = ?1")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

fn row_to_destination(row: sqlx::sqlite::SqliteRow) -> Result<Destination> {
    Ok(Destination {
        id: parse_uuid(&row.get::<String, _>("id"), "id")?,
        name: row.get("name"),
        kind: serde_json::from_str(&row.get::<String, _>("kind"))
            .map_err(|e| corrupt("kind", e))?,
        enabled: row.get("enabled"),
        // A destination whose retention cannot be read must not fall back to
        // "no policy": that reads as "keep everything", which is safe, but it
        // is a different policy than the one the user set and would be silent.
        retention: serde_json::from_str(&row.get::<String, _>("retention"))
            .map_err(|e| corrupt("retention", e))?,
        created_at: parse_ts(&row.get::<String, _>("created_at"), "created_at")?,
        updated_at: parse_ts(&row.get::<String, _>("updated_at"), "updated_at")?,
    })
}

// ── SSH connections ─────────────────────────────────────────────────────

const SSH_COLUMNS: &str = "id, name, endpoint, jump_host_id, created_at, updated_at";

impl Store {
    pub async fn list_ssh_connections(&self) -> Result<Vec<SshConnection>> {
        let rows = sqlx::query(&format!(
            "SELECT {SSH_COLUMNS} FROM ssh_connections ORDER BY name"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_ssh_connection).collect()
    }

    pub async fn get_ssh_connection(&self, id: Uuid) -> Result<Option<SshConnection>> {
        let row = sqlx::query(&format!(
            "SELECT {SSH_COLUMNS} FROM ssh_connections WHERE id = ?1"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_ssh_connection).transpose()
    }

    pub async fn require_ssh_connection(&self, id: Uuid) -> Result<SshConnection> {
        self.get_ssh_connection(id)
            .await?
            .ok_or(StoreError::InvalidSshConnection(
                SshConnectionError::NotFound(id),
            ))
    }

    /// Other connections that route through this one.
    pub async fn ssh_connections_jumping_through(&self, id: Uuid) -> Result<Vec<SshConnection>> {
        let rows = sqlx::query(&format!(
            "SELECT {SSH_COLUMNS} FROM ssh_connections WHERE jump_host_id = ?1 ORDER BY name"
        ))
        .bind(id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_ssh_connection).collect()
    }

    pub async fn create_ssh_connection(
        &self,
        input: SshConnectionCreate,
    ) -> Result<SshConnection> {
        input.validate().map_err(StoreError::InvalidSshConnection)?;

        let now = Utc::now();
        let connection = SshConnection {
            id: Uuid::new_v4(),
            name: input.name.trim().to_string(),
            endpoint: input.endpoint,
            jump_host_id: input.jump_host_id,
            created_at: now,
            updated_at: now,
        };
        self.check_jump_host(&connection).await?;

        let result = sqlx::query(
            "INSERT INTO ssh_connections (id, name, endpoint, jump_host_id, created_at, \
             updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(connection.id.to_string())
        .bind(&connection.name)
        .bind(serde_json::to_string(&connection.endpoint).map_err(|e| corrupt("endpoint", e))?)
        .bind(connection.jump_host_id.map(|u| u.to_string()))
        .bind(connection.created_at.to_rfc3339())
        .bind(connection.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(connection),
            Err(e) if is_unique_violation(&e) => Err(StoreError::DuplicateName(connection.name)),
            Err(e) => Err(e.into()),
        }
    }

    /// Edit a connection in place.
    ///
    /// Every profile pointing at it follows immediately, which is the reason
    /// this record exists — so the validation below runs on the *result* of the
    /// patch, not on the patch itself.
    pub async fn update_ssh_connection(
        &self,
        id: Uuid,
        patch: SshConnectionUpdate,
    ) -> Result<SshConnection> {
        let mut c = self.require_ssh_connection(id).await?;

        if let Some(v) = patch.name {
            if v.trim().is_empty() {
                return Err(StoreError::InvalidSshConnection(SshConnectionError::NoName));
            }
            c.name = v.trim().to_string();
        }
        if let Some(v) = patch.endpoint {
            v.validate().map_err(StoreError::InvalidSshConnection)?;
            c.endpoint = v;
        }
        // Doubly-optional: Some(None) detaches the bastion, None leaves alone.
        if let Some(v) = patch.jump_host_id {
            c.jump_host_id = v;
        }
        c.updated_at = Utc::now();
        self.check_jump_host(&c).await?;

        let result = sqlx::query(
            "UPDATE ssh_connections SET name = ?2, endpoint = ?3, jump_host_id = ?4, \
             updated_at = ?5 WHERE id = ?1",
        )
        .bind(c.id.to_string())
        .bind(&c.name)
        .bind(serde_json::to_string(&c.endpoint).map_err(|e| corrupt("endpoint", e))?)
        .bind(c.jump_host_id.map(|u| u.to_string()))
        .bind(c.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(c),
            Err(e) if is_unique_violation(&e) => Err(StoreError::DuplicateName(c.name)),
            Err(e) => Err(e.into()),
        }
    }

    /// Enforce the single-hop rule from both directions.
    ///
    /// The foreign key alone would allow a chain, and a chain would be accepted
    /// at save time and fail at connect time — on a schedule, in the middle of
    /// the night, with the user's last memory being that it saved fine.
    async fn check_jump_host(&self, connection: &SshConnection) -> Result<()> {
        let Some(jump_id) = connection.jump_host_id else {
            return Ok(());
        };

        if jump_id == connection.id {
            return Err(StoreError::InvalidSshConnection(
                SshConnectionError::JumpsToItself,
            ));
        }

        let jump = self.require_ssh_connection(jump_id).await?;
        if jump.jump_host_id.is_some() {
            return Err(StoreError::InvalidSshConnection(
                SshConnectionError::ChainedJump { name: jump.name },
            ));
        }

        // The other direction: this connection is somebody else's bastion, so
        // giving it one of its own would make that route two hops.
        let dependents = self.ssh_connections_jumping_through(connection.id).await?;
        if !dependents.is_empty() {
            return Err(StoreError::InvalidSshConnection(
                SshConnectionError::WouldChainJump {
                    name: connection.name.clone(),
                    used_by: name_list(dependents.iter().map(|d| d.name.as_str())),
                },
            ));
        }

        Ok(())
    }

    /// Remove a connection, refusing while anything still routes through it.
    ///
    /// Deleting it out from under a profile would leave a tunnelled connection
    /// silently connecting directly — to a host and port that were only ever
    /// meaningful *from the SSH server*.
    ///
    /// The keychain entry is not touched here; [`crate::ops::forget_ssh_connection`]
    /// does both in the right order, for the same reason destinations do.
    pub async fn delete_ssh_connection(&self, id: Uuid) -> Result<bool> {
        let Some(connection) = self.get_ssh_connection(id).await? else {
            return Ok(false);
        };

        let profiles = self.profiles_using_ssh_connection(id).await?;
        let dependents = self.ssh_connections_jumping_through(id).await?;
        if !profiles.is_empty() || !dependents.is_empty() {
            let used_by = name_list(
                profiles
                    .iter()
                    .map(|p| p.name.as_str())
                    .chain(dependents.iter().map(|d| d.name.as_str())),
            );
            return Err(StoreError::InvalidSshConnection(
                SshConnectionError::InUse {
                    name: connection.name,
                    used_by,
                },
            ));
        }

        let res = sqlx::query("DELETE FROM ssh_connections WHERE id = ?1")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Look a connection up together with its bastion.
    pub async fn resolve_ssh(&self, id: Uuid) -> Result<ResolvedSsh> {
        let connection = self.require_ssh_connection(id).await?;

        // One lookup deep, never a loop: the single-hop rule is enforced when
        // the row is written, so this cannot recurse.
        let jump_host = match connection.jump_host_id {
            Some(jump_id) => Some(self.require_ssh_connection(jump_id).await?),
            None => None,
        };

        Ok(ResolvedSsh {
            config: SshConfig {
                endpoint: connection.endpoint.clone(),
                jump_host: jump_host.as_ref().map(|j| j.endpoint.clone()),
            },
            connection,
            jump_host,
        })
    }

    // ── Adopting pre-existing embedded configurations ───────────────────

    /// Profiles still carrying an inline SSH config from before saved
    /// connections existed. See [`crate::sshconn::adopt_legacy_configs`].
    pub async fn legacy_ssh_configs(&self) -> Result<Vec<(Uuid, String, SshConfig)>> {
        let rows = sqlx::query(
            "SELECT id, name, ssh_config FROM profiles \
             WHERE ssh_config IS NOT NULL AND ssh_connection_id IS NULL ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let raw: String = row.get("ssh_config");
                Ok((
                    parse_uuid(&row.get::<String, _>("id"), "id")?,
                    row.get("name"),
                    serde_json::from_str::<SshConfig>(&raw)
                        .map_err(|e| corrupt("ssh_config", e))?,
                ))
            })
            .collect()
    }

    /// Point a profile at a saved connection and retire its inline config.
    ///
    /// Both halves in one statement: a profile that ended up referencing a
    /// connection *and* still holding the old blob would be adopted twice on
    /// the next start.
    pub async fn attach_ssh_connection(
        &self,
        profile_id: Uuid,
        ssh_connection_id: Uuid,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE profiles SET ssh_connection_id = ?2, ssh_config = NULL, updated_at = ?3 \
             WHERE id = ?1",
        )
        .bind(profile_id.to_string())
        .bind(ssh_connection_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// `"a", "b" and "c"` — a list a person can read in an error message.
fn name_list<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let quoted: Vec<String> = names.map(|n| format!("{n:?}")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

fn row_to_ssh_connection(row: sqlx::sqlite::SqliteRow) -> Result<SshConnection> {
    let endpoint_raw: String = row.get("endpoint");
    let jump_raw: Option<String> = row.get("jump_host_id");

    Ok(SshConnection {
        id: parse_uuid(&row.get::<String, _>("id"), "id")?,
        name: row.get("name"),
        endpoint: serde_json::from_str::<SshEndpoint>(&endpoint_raw)
            .map_err(|e| corrupt("endpoint", e))?,
        jump_host_id: jump_raw
            .map(|s| parse_uuid(&s, "jump_host_id"))
            .transpose()?,
        created_at: parse_ts(&row.get::<String, _>("created_at"), "created_at")?,
        updated_at: parse_ts(&row.get::<String, _>("updated_at"), "updated_at")?,
    })
}

// ── Application settings ────────────────────────────────────────────────

impl Store {
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM app_settings WHERE key = ?1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("value")))
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO app_settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = ?2",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every stored preference, with defaults filled in.
    ///
    /// `launch_at_login` is left at its default here: the OS owns that state,
    /// and the desktop layer overwrites it with what the OS actually reports.
    /// Where client binaries come from, with a fallback that always answers.
    ///
    /// Read fresh at the start of every job rather than cached: the app and
    /// the scheduler both outlive the settings page, so a source changed at
    /// 10am has to take effect on the 2am run without a restart. One store
    /// read is nothing next to a backup.
    ///
    /// An unreadable store falls back to local binaries — the same answer
    /// [`settings::parse_tool_source`] gives a corrupt value, and for the same
    /// reason: a preferences problem must not stop a backup that could still
    /// run.
    pub async fn tool_source(&self) -> crate::tools::ToolSource {
        match self.app_settings().await {
            Ok(s) => s.tool_source,
            Err(e) => {
                tracing::warn!("could not read the tool source, using local binaries: {e}");
                crate::tools::ToolSource::Local
            }
        }
    }

    pub async fn app_settings(&self) -> Result<settings::AppSettings> {
        let defaults = settings::AppSettings::default();
        Ok(settings::AppSettings {
            scheduler_enabled: settings::parse_flag(
                self.get_setting(settings::SCHEDULER_ENABLED)
                    .await?
                    .as_deref(),
                defaults.scheduler_enabled,
            ),
            close_to_tray: settings::parse_flag(
                self.get_setting(settings::CLOSE_TO_TRAY).await?.as_deref(),
                defaults.close_to_tray,
            ),
            tool_source: settings::parse_tool_source(
                self.get_setting(settings::TOOL_SOURCE).await?.as_deref(),
            ),
            background_notice_shown: settings::parse_flag(
                self.get_setting(settings::BACKGROUND_NOTICE_SHOWN)
                    .await?
                    .as_deref(),
                defaults.background_notice_shown,
            ),
            ..defaults
        })
    }

    pub async fn set_flag(&self, key: &str, value: bool) -> Result<()> {
        self.set_setting(key, settings::flag_str(value)).await
    }
}

// ── Schedules ───────────────────────────────────────────────────────────

/// Every column the row mapper needs, in one place, so the four queries below
/// cannot drift apart from each other or from [`row_to_schedule`].
const SCHEDULE_COLUMNS: &str = "id, name, kind, sync_plan_id, dest_profile_id, pipeline_id, \
     cron_expression, \
     timezone, enabled, action_json, webhook_url, notify, catch_up, last_run_at, last_outcome, \
     last_job_id, created_at, updated_at";

impl Store {
    pub async fn list_schedules(&self) -> Result<Vec<Schedule>> {
        let sql = format!("SELECT {SCHEDULE_COLUMNS} FROM schedules ORDER BY name");
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        rows.into_iter().map(row_to_schedule).collect()
    }

    /// Only the schedules the scheduler needs to consider on a tick.
    pub async fn list_enabled_schedules(&self) -> Result<Vec<Schedule>> {
        let sql =
            format!("SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE enabled = 1 ORDER BY name");
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        rows.into_iter().map(row_to_schedule).collect()
    }

    pub async fn get_schedule(&self, id: Uuid) -> Result<Option<Schedule>> {
        let sql = format!("SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE id = ?1");
        let row = sqlx::query(&sql)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_schedule).transpose()
    }

    pub async fn require_schedule(&self, id: Uuid) -> Result<Schedule> {
        self.get_schedule(id)
            .await?
            .ok_or(StoreError::ScheduleNotFound(id))
    }

    /// Refuse to store a schedule that would drop a database unattended
    /// without a person having authorised it.
    ///
    /// Checked on every write rather than only on create: a pipeline can be
    /// re-pointed at a real database after the schedule exists, and
    /// `update_pipeline` clearing the acknowledgment is what makes this catch
    /// it. Also checked again at run time by the scheduler, because the
    /// pipeline can change between the two.
    async fn check_pipeline_target(
        &self,
        kind: ScheduleKind,
        pipeline_id: Option<Uuid>,
    ) -> Result<()> {
        if kind != ScheduleKind::Pipeline {
            return Ok(());
        }
        let Some(id) = pipeline_id else {
            return Ok(()); // `validate` already reports the missing pipeline.
        };
        let pipeline = self.require_pipeline(id).await?;

        if pipeline.is_destructive() && !pipeline.is_armed() {
            return Err(StoreError::InvalidSchedule(
                crate::schedule::ScheduleError::PipelineNotArmed {
                    targets: pipeline.destructive_targets().join(", "),
                    name: pipeline.name,
                },
            ));
        }
        Ok(())
    }

    pub async fn create_schedule(&self, input: ScheduleCreate) -> Result<Schedule> {
        // Refuse to persist something that could never safely run. Validating
        // only in the UI would leave the CLI able to write an unattended
        // drop-and-recreate straight into the table.
        input.validate().map_err(StoreError::InvalidSchedule)?;
        self.check_pipeline_target(input.kind, input.pipeline_id)
            .await?;

        let now = Utc::now();
        let schedule = Schedule {
            id: Uuid::new_v4(),
            name: input.name,
            kind: input.kind,
            plan_id: input.plan_id,
            dest_profile_id: input.dest_profile_id,
            pipeline_id: input.pipeline_id,
            cron: input.cron,
            timezone: input.timezone,
            enabled: input.enabled,
            action: input.action,
            webhook_url: input.webhook_url,
            notify: input.notify,
            catch_up: input.catch_up,
            last_run_at: None,
            last_outcome: None,
            last_job_id: None,
            created_at: now,
            updated_at: now,
        };

        sqlx::query(
            "INSERT INTO schedules (id, name, kind, sync_plan_id, dest_profile_id, pipeline_id, \
             cron_expression, timezone, enabled, action_json, webhook_url, notify, catch_up, \
             created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )
        .bind(schedule.id.to_string())
        .bind(&schedule.name)
        .bind(schedule.kind.as_str())
        .bind(schedule.plan_id.map(|u| u.to_string()))
        .bind(schedule.dest_profile_id.map(|u| u.to_string()))
        .bind(schedule.pipeline_id.map(|u| u.to_string()))
        .bind(schedule.cron.as_str())
        .bind(schedule.timezone.as_str())
        .bind(schedule.enabled)
        .bind(serde_json::to_string(&schedule.action).map_err(|e| corrupt("action_json", e))?)
        .bind(&schedule.webhook_url)
        .bind(schedule.notify.as_str())
        .bind(schedule.catch_up)
        .bind(schedule.created_at.to_rfc3339())
        .bind(schedule.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(schedule)
    }

    pub async fn update_schedule(&self, id: Uuid, patch: ScheduleUpdate) -> Result<Schedule> {
        let mut s = self.require_schedule(id).await?;

        if let Some(v) = patch.name {
            s.name = v;
        }
        if let Some(v) = patch.cron {
            s.cron = v;
        }
        if let Some(v) = patch.timezone {
            s.timezone = v;
        }
        if let Some(v) = patch.enabled {
            s.enabled = v;
        }
        if let Some(v) = patch.action {
            s.action = v;
        }
        // Doubly-optional: Some(None) clears, None leaves alone.
        if let Some(v) = patch.webhook_url {
            s.webhook_url = v;
        }
        if let Some(v) = patch.notify {
            s.notify = v;
        }
        if let Some(v) = patch.catch_up {
            s.catch_up = v;
        }
        if let Some(v) = patch.dest_profile_id {
            s.dest_profile_id = v;
        }
        s.updated_at = Utc::now();

        // Re-checked after the patch, not before: a patch that only changes the
        // naming strategy would otherwise slip a destructive target past the
        // check that create_schedule applies.
        s.validate().map_err(StoreError::InvalidSchedule)?;

        sqlx::query(
            "UPDATE schedules SET name = ?2, dest_profile_id = ?3, cron_expression = ?4, \
             timezone = ?5, enabled = ?6, action_json = ?7, webhook_url = ?8, notify = ?9, \
             catch_up = ?10, updated_at = ?11 WHERE id = ?1",
        )
        .bind(s.id.to_string())
        .bind(&s.name)
        .bind(s.dest_profile_id.map(|u| u.to_string()))
        .bind(s.cron.as_str())
        .bind(s.timezone.as_str())
        .bind(s.enabled)
        .bind(serde_json::to_string(&s.action).map_err(|e| corrupt("action_json", e))?)
        .bind(&s.webhook_url)
        .bind(s.notify.as_str())
        .bind(s.catch_up)
        .bind(s.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(s)
    }

    /// Record that a schedule fired, against the occurrence it fired *for*.
    ///
    /// Stamping the occurrence rather than "now" is what makes the high-water
    /// mark exact: a run that started 40 seconds late must not leave a mark 40
    /// seconds past its own occurrence, or a schedule finer than that would
    /// skip its next tick.
    pub async fn mark_schedule_started(
        &self,
        id: Uuid,
        occurrence: DateTime<Utc>,
        job_id: Uuid,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE schedules SET last_run_at = ?2, last_job_id = ?3, last_outcome = NULL \
             WHERE id = ?1",
        )
        .bind(id.to_string())
        .bind(occurrence.to_rfc3339())
        .bind(job_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_schedule_finished(&self, id: Uuid, outcome: JobOutcome) -> Result<()> {
        sqlx::query("UPDATE schedules SET last_outcome = ?2 WHERE id = ?1")
            .bind(id.to_string())
            .bind(outcome.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_schedule(&self, id: Uuid) -> Result<bool> {
        let res = sqlx::query("DELETE FROM schedules WHERE id = ?1")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

fn row_to_schedule(row: sqlx::sqlite::SqliteRow) -> Result<Schedule> {
    let action_raw: String = row.get("action_json");
    let cron_raw: String = row.get("cron_expression");
    let tz_raw: String = row.get("timezone");
    let notify_raw: String = row.get("notify");
    let kind_raw: String = row.get("kind");
    let plan_raw: Option<String> = row.get("sync_plan_id");
    let dest_raw: Option<String> = row.get("dest_profile_id");
    let pipeline_raw: Option<String> = row.get("pipeline_id");
    let last_run_raw: Option<String> = row.get("last_run_at");
    let last_outcome_raw: Option<String> = row.get("last_outcome");
    let last_job_raw: Option<String> = row.get("last_job_id");

    Ok(Schedule {
        id: parse_uuid(&row.get::<String, _>("id"), "id")?,
        name: row.get("name"),
        // An unrecognised kind is corruption, not "probably a sync". Guessing
        // would run a drill's row through the sync path, which reads its
        // missing plan id as a deleted plan and reports the wrong failure.
        kind: ScheduleKind::parse(&kind_raw).ok_or_else(|| {
            corrupt(
                "kind",
                anyhow::anyhow!("unknown schedule kind {kind_raw:?}"),
            )
        })?,
        plan_id: plan_raw
            .map(|s| parse_uuid(&s, "sync_plan_id"))
            .transpose()?,
        dest_profile_id: dest_raw
            .map(|s| parse_uuid(&s, "dest_profile_id"))
            .transpose()?,
        pipeline_id: pipeline_raw
            .map(|s| parse_uuid(&s, "pipeline_id"))
            .transpose()?,
        // A cron expression that no longer parses is corruption, not "never
        // run". Reporting it beats a schedule that silently stops firing.
        cron: cron_raw
            .parse()
            .map_err(|e: crate::cron::CronError| corrupt("cron_expression", anyhow::anyhow!(e)))?,
        timezone: tz_raw
            .parse()
            .map_err(|e: crate::cron::CronError| corrupt("timezone", anyhow::anyhow!(e)))?,
        enabled: row.get::<i64, _>("enabled") != 0,
        action: serde_json::from_str(&action_raw).map_err(|e| corrupt("action_json", e))?,
        webhook_url: row.get("webhook_url"),
        notify: NotifyPolicy::parse(&notify_raw)
            .ok_or_else(|| corrupt("notify", anyhow::anyhow!("unknown policy {notify_raw:?}")))?,
        catch_up: row.get::<i64, _>("catch_up") != 0,
        last_run_at: last_run_raw
            .map(|s| parse_ts(&s, "last_run_at"))
            .transpose()?,
        last_outcome: last_outcome_raw.as_deref().and_then(parse_outcome),
        last_job_id: last_job_raw
            .map(|s| parse_uuid(&s, "last_job_id"))
            .transpose()?,
        created_at: parse_ts(&row.get::<String, _>("created_at"), "created_at")?,
        updated_at: parse_ts(&row.get::<String, _>("updated_at"), "updated_at")?,
    })
}

fn row_to_plan(row: sqlx::sqlite::SqliteRow) -> Result<SyncPlan> {
    let selections_raw: String = row.get("table_selections");
    let masking_raw: String = row.get("masking");

    Ok(SyncPlan {
        id: parse_uuid(&row.get::<String, _>("id"), "id")?,
        profile_id: parse_uuid(&row.get::<String, _>("profile_id"), "profile_id")?,
        name: row.get("name"),
        database: row.get("database_name"),
        selections: serde_json::from_str(&selections_raw)
            .map_err(|e| corrupt("table_selections", e))?,
        // Corrupt masking rules are an error, never an empty list: silently
        // reading them as "no masking" would hand somebody an unmasked
        // destination while the plan still says the column is protected.
        masking: serde_json::from_str(&masking_raw).map_err(|e| corrupt("masking", e))?,
        revision: row.get::<i64, _>("revision").max(0) as u32,
        created_at: parse_ts(&row.get::<String, _>("created_at"), "created_at")?,
        updated_at: parse_ts(&row.get::<String, _>("updated_at"), "updated_at")?,
    })
}
