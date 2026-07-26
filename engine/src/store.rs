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
use crate::events::JobKind;
use crate::job::{JobOutcome, JobRecord};
use crate::plan::{SyncPlan, SyncPlanCreate};
use crate::profile::{
    ConnectionProfile, DbConfig, ProfileCreate, ProfileUpdate, SshConfig, ToolOverrides,
};
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
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path.as_ref())
            .create_if_missing(true)
            // Referential integrity is off by default in SQLite; sync_plans
            // depends on it to clean up when a profile is deleted.
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
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
            "SELECT id, profile_id, name, database_name, table_selections, revision, \
             created_at, updated_at FROM sync_plans WHERE profile_id = ?1 ORDER BY name",
        )
        .bind(profile_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(row_to_plan).collect()
    }

    pub async fn get_sync_plan(&self, id: Uuid) -> Result<Option<SyncPlan>> {
        let row = sqlx::query(
            "SELECT id, profile_id, name, database_name, table_selections, revision, \
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
            revision: 1,
            created_at: now,
            updated_at: now,
        };

        sqlx::query(
            "INSERT INTO sync_plans (id, profile_id, name, database_name, table_selections, \
             revision, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(plan.id.to_string())
        .bind(plan.profile_id.to_string())
        .bind(&plan.name)
        .bind(&plan.database)
        .bind(serde_json::to_string(&plan.selections).map_err(|e| corrupt("table_selections", e))?)
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

    pub async fn delete_sync_plan(&self, id: Uuid) -> Result<bool> {
        let res = sqlx::query("DELETE FROM sync_plans WHERE id = ?1")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

fn row_to_plan(row: sqlx::sqlite::SqliteRow) -> Result<SyncPlan> {
    let selections_raw: String = row.get("table_selections");

    Ok(SyncPlan {
        id: parse_uuid(&row.get::<String, _>("id"), "id")?,
        profile_id: parse_uuid(&row.get::<String, _>("profile_id"), "profile_id")?,
        name: row.get("name"),
        database: row.get("database_name"),
        selections: serde_json::from_str(&selections_raw)
            .map_err(|e| corrupt("table_selections", e))?,
        revision: row.get::<i64, _>("revision").max(0) as u32,
        created_at: parse_ts(&row.get::<String, _>("created_at"), "created_at")?,
        updated_at: parse_ts(&row.get::<String, _>("updated_at"), "updated_at")?,
    })
}
