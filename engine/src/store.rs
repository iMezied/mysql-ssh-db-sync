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
use crate::plan::{SyncPlan, SyncPlanCreate};
use crate::profile::{
    ConnectionProfile, DbConfig, ProfileCreate, ProfileUpdate, SshConfig, ToolOverrides,
};
use crate::schedule::{NotifyPolicy, Schedule, ScheduleCreate, ScheduleKind, ScheduleUpdate};
use crate::settings;
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
    #[error(transparent)]
    InvalidSchedule(crate::schedule::ScheduleError),
    #[error(transparent)]
    InvalidDestination(crate::destination::DestinationError),
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
        let rows = sqlx::query(
            "SELECT id, name, engine, environment, ssh_config, db_config, tool_overrides, \
             created_at, updated_at FROM profiles ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(row_to_profile).collect()
    }

    pub async fn get_profile(&self, id: Uuid) -> Result<Option<ConnectionProfile>> {
        let row = sqlx::query(
            "SELECT id, name, engine, environment, ssh_config, db_config, tool_overrides, \
             created_at, updated_at FROM profiles WHERE id = ?1",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(row_to_profile).transpose()
    }

    /// Like [`get_profile`] but errors when absent, for callers that require one.
    pub async fn require_profile(&self, id: Uuid) -> Result<ConnectionProfile> {
        self.get_profile(id)
            .await?
            .ok_or(StoreError::ProfileNotFound(id))
    }

    pub async fn create_profile(&self, input: ProfileCreate) -> Result<ConnectionProfile> {
        let now = Utc::now();
        let profile = ConnectionProfile {
            id: Uuid::new_v4(),
            name: input.name,
            engine: input.engine,
            environment: input.environment,
            ssh: input.ssh,
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
            "INSERT INTO profiles (id, name, engine, environment, ssh_config, db_config, \
             tool_overrides, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(p.id.to_string())
        .bind(&p.name)
        .bind(p.engine.as_str())
        .bind(p.environment.as_str())
        .bind(
            p.ssh
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| corrupt("ssh_config", e))?,
        )
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
        // Doubly-optional: Some(None) clears, None leaves alone.
        if let Some(v) = patch.ssh {
            p.ssh = v;
        }
        if let Some(v) = patch.db {
            p.db = v;
        }
        if let Some(v) = patch.tool_overrides {
            p.tool_overrides = v;
        }
        p.updated_at = Utc::now();

        let result = sqlx::query(
            "UPDATE profiles SET name = ?2, engine = ?3, environment = ?4, ssh_config = ?5, \
             db_config = ?6, tool_overrides = ?7, updated_at = ?8 WHERE id = ?1",
        )
        .bind(p.id.to_string())
        .bind(&p.name)
        .bind(p.engine.as_str())
        .bind(p.environment.as_str())
        .bind(
            p.ssh
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| corrupt("ssh_config", e))?,
        )
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

fn row_to_profile(row: sqlx::sqlite::SqliteRow) -> Result<ConnectionProfile> {
    let ssh_raw: Option<String> = row.get("ssh_config");
    let tools_raw: String = row.get("tool_overrides");
    let db_raw: String = row.get("db_config");

    Ok(ConnectionProfile {
        id: parse_uuid(&row.get::<String, _>("id"), "id")?,
        name: row.get("name"),
        engine: Engine::from_str(&row.get::<String, _>("engine"))
            .map_err(|e| corrupt("engine", e))?,
        environment: EnvironmentTag::from_str(&row.get::<String, _>("environment"))
            .map_err(|e| corrupt("environment", e))?,
        // A malformed ssh_config is corruption, not "no SSH" — surfacing it
        // beats silently turning a tunnelled profile into a direct connection.
        ssh: ssh_raw
            .map(|s| serde_json::from_str::<SshConfig>(&s))
            .transpose()
            .map_err(|e| corrupt("ssh_config", e))?,
        db: serde_json::from_str::<DbConfig>(&db_raw).map_err(|e| corrupt("db_config", e))?,
        tool_overrides: serde_json::from_str::<ToolOverrides>(&tools_raw)
            .map_err(|e| corrupt("tool_overrides", e))?,
        created_at: parse_ts(&row.get::<String, _>("created_at"), "created_at")?,
        updated_at: parse_ts(&row.get::<String, _>("updated_at"), "updated_at")?,
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
        .await?;

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
const SCHEDULE_COLUMNS: &str = "id, name, kind, sync_plan_id, dest_profile_id, cron_expression, \
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

    pub async fn create_schedule(&self, input: ScheduleCreate) -> Result<Schedule> {
        // Refuse to persist something that could never safely run. Validating
        // only in the UI would leave the CLI able to write an unattended
        // drop-and-recreate straight into the table.
        input.validate().map_err(StoreError::InvalidSchedule)?;

        let now = Utc::now();
        let schedule = Schedule {
            id: Uuid::new_v4(),
            name: input.name,
            kind: input.kind,
            plan_id: input.plan_id,
            dest_profile_id: input.dest_profile_id,
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
            "INSERT INTO schedules (id, name, kind, sync_plan_id, dest_profile_id, \
             cron_expression, timezone, enabled, action_json, webhook_url, notify, catch_up, \
             created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        )
        .bind(schedule.id.to_string())
        .bind(&schedule.name)
        .bind(schedule.kind.as_str())
        .bind(schedule.plan_id.map(|u| u.to_string()))
        .bind(schedule.dest_profile_id.map(|u| u.to_string()))
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
