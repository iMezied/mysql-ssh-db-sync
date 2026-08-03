//! Tauri command surface.
//!
//! This layer is deliberately thin: it validates, delegates to the engine, and
//! maps errors. Any logic that lives here instead of in the engine is invisible
//! to `dbsync` (the CLI) and therefore a layering bug.
//!
//! SECURITY: no command returns a secret. Passwords can be written to the
//! keychain and their presence can be queried, but there is deliberately no
//! "get password" command — a value returned here would be readable by any
//! script running in the webview.

use std::path::PathBuf;

use db_sync_engine::audit::AuditAction;
use db_sync_engine::backup::{BackupRequest, TableSelection};
use db_sync_engine::backupkey::{self, KeyStatus};
use db_sync_engine::connect::{self, ConnectionReport, SshReport};
use db_sync_engine::cron::{CronExpression, ScheduleTimezone};
use db_sync_engine::db::{DatabaseInfo, TableInfo};
use db_sync_engine::destination::{Destination, DestinationCreate, DestinationUpdate};
use db_sync_engine::events::{JobKind, JobPhase};
use db_sync_engine::job::JobRecord;
use db_sync_engine::job::{JobContext, JobOutcome};
use db_sync_engine::tools::{DockerContainer, ToolSource, ToolStatus};
use db_sync_engine::library::{self, Artifact, IntegrityCheck};
use db_sync_engine::mask::MaskRule;
use db_sync_engine::ops::{self, SyncRequest};
use db_sync_engine::pipeline::{Pipeline, PipelineCreate, PipelineUpdate};
use db_sync_engine::plan::{self, SyncPlan, SyncPlanCreate};
use db_sync_engine::profile::{ConnectionProfile, ProfileCreate, ProfileUpdate};
use db_sync_engine::restore::RestoreRequest;
use db_sync_engine::schedule::{Schedule, ScheduleCreate, ScheduleUpdate};
use db_sync_engine::secrets::{self, SecretKind};
use db_sync_engine::sshconn::{SshConnection, SshConnectionCreate, SshConnectionUpdate};
use db_sync_engine::settings::{self, AppSettings};
use db_sync_engine::step::JobStep;
use db_sync_engine::store::StoreError;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;
use tauri_plugin_autostart::ManagerExt;
use uuid::Uuid;

use crate::events::JobFinished;
use crate::{AppState, cli_tool};
use tauri_specta::Event as _;

/// Error shape crossing into the webview.
///
/// Carries a human-readable message and a machine-readable kind so the UI can
/// react (e.g. highlight a duplicate name field) without string matching.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct CommandError {
    pub kind: String,
    pub message: String,
}

impl CommandError {
    fn new(kind: &str, message: impl Into<String>) -> Self {
        Self {
            kind: kind.to_string(),
            message: message.into(),
        }
    }
}

impl From<StoreError> for CommandError {
    fn from(e: StoreError) -> Self {
        let kind = match &e {
            StoreError::DuplicateName(_) => "duplicate_name",
            StoreError::ProfileNotFound(_)
            | StoreError::SyncPlanNotFound(_)
            | StoreError::ScheduleNotFound(_)
            | StoreError::DestinationNotFound(_)
            | StoreError::PipelineNotFound(_) => "not_found",
            StoreError::Corrupt { .. } => "corrupt",
            StoreError::InvalidSchedule(_)
            | StoreError::InvalidDestination(_)
            | StoreError::InvalidSshConnection(_)
            | StoreError::InvalidPipeline(_) => "invalid",
            StoreError::Secrets(_) => "keychain",
            StoreError::Sqlx(_) | StoreError::Migrate(_) => "storage",
        };
        Self::new(kind, e.to_string())
    }
}

impl From<secrets::SecretError> for CommandError {
    fn from(e: secrets::SecretError) -> Self {
        Self::new("keychain", e.to_string())
    }
}

type CmdResult<T> = Result<T, CommandError>;

/// What the UI is allowed to know about stored secrets: whether they exist.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SecretStatus {
    pub has_db_password: bool,
}

/// An SSH connection plus the two things the UI cannot derive from the record:
/// whether its passphrase is stored, and what would break if it were deleted.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SshConnectionStatus {
    pub has_passphrase: bool,
    /// Names of the profiles tunnelling through it.
    pub used_by_profiles: Vec<String>,
    /// Names of the SSH connections jumping through it.
    pub used_by_jump: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct AppInfo {
    pub engine_version: String,
    pub store_path: String,
}

/// How long a single introspection call may take before we give up.
///
/// A wedged tunnel would otherwise leave the UI spinning forever with no way
/// to tell "slow" from "never".
const INTROSPECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

// ── Profiles ────────────────────────────────────────────────────────────

#[tauri::command]
#[specta::specta]
pub async fn list_profiles(state: State<'_, AppState>) -> CmdResult<Vec<ConnectionProfile>> {
    Ok(state.store.list_profiles().await?)
}

#[tauri::command]
#[specta::specta]
pub async fn get_profile(
    state: State<'_, AppState>,
    id: Uuid,
) -> CmdResult<Option<ConnectionProfile>> {
    Ok(state.store.get_profile(id).await?)
}

/// Create a profile, optionally storing its database password in the keychain.
///
/// The password is consumed here and never echoed back.
#[tauri::command]
#[specta::specta]
pub async fn create_profile(
    state: State<'_, AppState>,
    input: ProfileCreate,
    db_password: Option<String>,
) -> CmdResult<ConnectionProfile> {
    if input.name.trim().is_empty() {
        return Err(CommandError::new("invalid", "profile name cannot be empty"));
    }

    let profile = state.store.create_profile(input).await?;

    if let Some(password) = db_password
        && !password.is_empty()
    {
        // If the keychain write fails the profile would exist without its
        // credential, which is confusing; roll back so the user retries cleanly.
        if let Err(e) = secrets::set_secret(profile.id, SecretKind::DbPassword, &password) {
            let _ = state.store.delete_profile(profile.id).await;
            return Err(e.into());
        }
    }

    state
        .store
        .audit(
            AuditAction::ProfileCreated,
            &profile.name,
            format!("{:?} at {}", profile.engine, profile.db.host),
        )
        .await;
    Ok(profile)
}

#[tauri::command]
#[specta::specta]
pub async fn update_profile(
    state: State<'_, AppState>,
    id: Uuid,
    patch: ProfileUpdate,
) -> CmdResult<ConnectionProfile> {
    // Read first, so the record can say what actually moved. "updated" alone
    // does not answer the question this log exists for.
    let before = state.store.get_profile(id).await?;
    let after = state.store.update_profile(id, patch).await?;

    if let Some(before) = before {
        let mut changes = Vec::new();
        if before.db.host != after.db.host || before.db.port != after.db.port {
            changes.push(format!(
                "{}:{} -> {}:{}",
                before.db.host, before.db.port, after.db.host, after.db.port
            ));
        }
        if before.environment != after.environment {
            changes.push(format!(
                "{} -> {}",
                before.environment.as_str(),
                after.environment.as_str()
            ));
        }
        if before.name != after.name {
            changes.push(format!("renamed from {}", before.name));
        }
        if !changes.is_empty() {
            state
                .store
                .audit(AuditAction::ProfileUpdated, &after.name, changes.join(", "))
                .await;
        }
    }

    Ok(after)
}

/// Delete a profile and purge every secret belonging to it.
#[tauri::command]
#[specta::specta]
pub async fn delete_profile(state: State<'_, AppState>, id: Uuid) -> CmdResult<bool> {
    let name = state
        .store
        .get_profile(id)
        .await?
        .map(|p| p.name)
        .unwrap_or_else(|| id.to_string());

    let removed = state.store.delete_profile(id).await?;
    if removed {
        state
            .store
            .audit(AuditAction::ProfileDeleted, name, "backups for it stop")
            .await;
    }
    if removed {
        // Orphaned keychain entries would outlive the app otherwise.
        secrets::delete_all_for_profile(id)?;
    }
    Ok(removed)
}

/// Store or clear a profile secret. An empty value clears it.
#[tauri::command]
#[specta::specta]
pub async fn set_profile_secret(
    state: State<'_, AppState>,
    id: Uuid,
    kind: String,
    value: String,
) -> CmdResult<()> {
    // Refuse to write secrets for a profile that does not exist, so a typo in
    // the id cannot silently create an unreachable keychain entry.
    state.store.require_profile(id).await?;

    let kind = match kind.as_str() {
        "db_password" => SecretKind::DbPassword,
        // Deliberately no "ssh_passphrase": it belongs to the SSH connection,
        // not the profile. See `set_ssh_connection_passphrase`.
        other => {
            return Err(CommandError::new(
                "invalid",
                format!("unknown secret kind {other:?}"),
            ));
        }
    };

    secrets::set_secret(id, kind, &value)?;
    // That one was set, never what it was.
    state
        .store
        .audit(
            AuditAction::SecretSet,
            state
                .store
                .get_profile(id)
                .await?
                .map(|p| p.name)
                .unwrap_or_else(|| id.to_string()),
            if value.is_empty() {
                format!("{kind:?} cleared")
            } else {
                format!("{kind:?} stored")
            },
        )
        .await;
    Ok(())
}

/// Report whether secrets exist — never their values.
#[tauri::command]
#[specta::specta]
pub async fn profile_secret_status(
    state: State<'_, AppState>,
    id: Uuid,
) -> CmdResult<SecretStatus> {
    state.store.require_profile(id).await?;
    Ok(SecretStatus {
        has_db_password: secrets::has_secret(id, SecretKind::DbPassword)?,
    })
}

// ── SSH connections ─────────────────────────────────────────────────────

#[tauri::command]
#[specta::specta]
pub async fn list_ssh_connections(state: State<'_, AppState>) -> CmdResult<Vec<SshConnection>> {
    Ok(state.store.list_ssh_connections().await?)
}

#[tauri::command]
#[specta::specta]
pub async fn create_ssh_connection(
    state: State<'_, AppState>,
    input: SshConnectionCreate,
    passphrase: Option<String>,
) -> CmdResult<SshConnection> {
    let connection = state.store.create_ssh_connection(input).await?;

    if let Some(passphrase) = passphrase
        && !passphrase.is_empty()
    {
        // Rolled back on a keychain failure for the same reason a profile is:
        // a record that exists without the credential it needs looks configured
        // and is not.
        if let Err(e) = secrets::set_secret(
            connection.id,
            SecretKind::SshKeyPassphrase,
            &passphrase,
        ) {
            let _ = state.store.delete_ssh_connection(connection.id).await;
            return Err(e.into());
        }
    }

    state
        .store
        .audit(
            AuditAction::SshConnectionCreated,
            &connection.name,
            connection.describe(),
        )
        .await;
    Ok(connection)
}

/// Edit a connection. Every profile tunnelling through it follows.
#[tauri::command]
#[specta::specta]
pub async fn update_ssh_connection(
    state: State<'_, AppState>,
    id: Uuid,
    patch: SshConnectionUpdate,
) -> CmdResult<SshConnection> {
    let before = state.store.get_ssh_connection(id).await?;
    let after = state.store.update_ssh_connection(id, patch).await?;

    if let Some(before) = before {
        let mut changes = Vec::new();
        if before.endpoint != after.endpoint {
            changes.push(format!("{} -> {}", before.describe(), after.describe()));
        }
        if before.jump_host_id != after.jump_host_id {
            changes.push("jump host changed".to_string());
        }
        if before.name != after.name {
            changes.push(format!("renamed from {}", before.name));
        }
        if !changes.is_empty() {
            // Named individually because this is the record whose blast radius
            // is not visible from the thing being edited.
            let affected = state.store.profiles_using_ssh_connection(id).await?;
            let detail = if affected.is_empty() {
                changes.join(", ")
            } else {
                format!(
                    "{} (affects {})",
                    changes.join(", "),
                    affected
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            state
                .store
                .audit(AuditAction::SshConnectionUpdated, &after.name, detail)
                .await;
        }
    }

    Ok(after)
}

/// Delete a connection and its stored passphrase.
///
/// Refused while any profile or jump host still points at it — the error names
/// them, so the answer to "why not" does not need a second look.
#[tauri::command]
#[specta::specta]
pub async fn delete_ssh_connection(state: State<'_, AppState>, id: Uuid) -> CmdResult<bool> {
    let name = state
        .store
        .get_ssh_connection(id)
        .await?
        .map(|c| c.name)
        .unwrap_or_else(|| id.to_string());

    let removed = ops::forget_ssh_connection(&state.store, id)
        .await
        .map_err(|e| CommandError::new("invalid", e.to_string()))?;

    if removed {
        state
            .store
            .audit(
                AuditAction::SshConnectionDeleted,
                name,
                "nothing can tunnel through it any more",
            )
            .await;
    }
    Ok(removed)
}

/// Store or clear the passphrase protecting an SSH connection's key.
#[tauri::command]
#[specta::specta]
pub async fn set_ssh_connection_passphrase(
    state: State<'_, AppState>,
    id: Uuid,
    value: String,
) -> CmdResult<()> {
    let connection = state.store.require_ssh_connection(id).await?;
    secrets::set_secret(id, SecretKind::SshKeyPassphrase, &value)?;

    state
        .store
        .audit(
            AuditAction::SecretSet,
            &connection.name,
            if value.is_empty() {
                "SSH key passphrase cleared"
            } else {
                "SSH key passphrase stored"
            },
        )
        .await;
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn ssh_connection_status(
    state: State<'_, AppState>,
    id: Uuid,
) -> CmdResult<SshConnectionStatus> {
    state.store.require_ssh_connection(id).await?;
    Ok(SshConnectionStatus {
        has_passphrase: secrets::has_secret(id, SecretKind::SshKeyPassphrase)?,
        used_by_profiles: state
            .store
            .profiles_using_ssh_connection(id)
            .await?
            .into_iter()
            .map(|p| p.name)
            .collect(),
        used_by_jump: state
            .store
            .ssh_connections_jumping_through(id)
            .await?
            .into_iter()
            .map(|c| c.name)
            .collect(),
    })
}

/// Connect and authenticate an SSH connection on its own, without a database.
///
/// Never returns `Err` for an unreachable server, for the same reason
/// [`test_connection`] does not: a failed connection is a successful test.
#[tauri::command]
#[specta::specta]
pub async fn test_ssh_connection(state: State<'_, AppState>, id: Uuid) -> CmdResult<SshReport> {
    Ok(connect::test_ssh_connection(id, &state.store).await)
}

/// Test a profile, reporting each step separately.
///
/// Never returns `Err` for an unreachable server: a failed *connection* is a
/// successful *test*, and the per-step detail is the whole point.
#[tauri::command]
#[specta::specta]
pub async fn test_connection(state: State<'_, AppState>, id: Uuid) -> CmdResult<ConnectionReport> {
    let profile = state.store.require_profile(id).await?;
    Ok(connect::test_connection(&profile, &state.store).await)
}

/// Pin a host key after the user has verified its fingerprint.
///
/// `replace` must be set explicitly when a *different* key was already pinned —
/// silently overwriting is indistinguishable from accepting a MITM.
#[tauri::command]
#[specta::specta]
pub async fn trust_host_key(
    state: State<'_, AppState>,
    host_port: String,
    algorithm: String,
    fingerprint: String,
    replace: bool,
) -> CmdResult<()> {
    let existing = state
        .store
        .get_known_host(&host_port)
        .await?
        .map(|(_, fp)| fp);

    match existing {
        Some(current) if current != fingerprint && !replace => Err(CommandError::new(
            "host_key_changed",
            format!(
                "{host_port} already has a different key pinned ({current}); \
                 confirm the replacement explicitly"
            ),
        )),
        Some(_) => {
            state
                .store
                .replace_host_key(&host_port, &algorithm, &fingerprint)
                .await?;
            Ok(())
        }
        None => {
            state
                .store
                .remember_host(&host_port, &algorithm, &fingerprint)
                .await?;
            Ok(())
        }
    }
}

/// List the databases visible to a profile's user.
#[tauri::command]
#[specta::specta]
pub async fn list_databases(state: State<'_, AppState>, id: Uuid) -> CmdResult<Vec<DatabaseInfo>> {
    let profile = state.store.require_profile(id).await?;

    let connection = tokio::time::timeout(
        INTROSPECT_TIMEOUT,
        connect::open(&profile, &state.store, None),
    )
    .await
    .map_err(|_| CommandError::new("timeout", "connecting timed out"))?
    .map_err(|e| CommandError::new("connect", e.to_string()))?;

    let result = connection.introspector.list_databases().await;
    connection.close().await;

    result.map_err(|e| CommandError::new("query", e.to_string()))
}

/// List the tables in one database, with the metadata the picker needs.
#[tauri::command]
#[specta::specta]
pub async fn list_tables(
    state: State<'_, AppState>,
    id: Uuid,
    database: String,
) -> CmdResult<Vec<TableInfo>> {
    let profile = state.store.require_profile(id).await?;

    // PostgreSQL can only introspect the database it is connected to, so the
    // target database is part of opening the connection, not just the query.
    let connection = tokio::time::timeout(
        INTROSPECT_TIMEOUT,
        connect::open(&profile, &state.store, Some(&database)),
    )
    .await
    .map_err(|_| CommandError::new("timeout", "connecting timed out"))?
    .map_err(|e| CommandError::new("connect", e.to_string()))?;

    let result = connection.introspector.list_tables(&database).await;
    connection.close().await;

    result.map_err(|e| CommandError::new("query", e.to_string()))
}

// ── Jobs ────────────────────────────────────────────────────────────────

#[tauri::command]
#[specta::specta]
pub async fn list_jobs(state: State<'_, AppState>, limit: u32) -> CmdResult<Vec<JobRecord>> {
    Ok(state
        .store
        .list_jobs(i64::from(limit.clamp(1, 500)))
        .await?)
}

/// The steps one job is made of, in order.
///
/// Empty for a single-step job — a plain backup or restore has no shape worth
/// drawing — and for anything that ran before this was recorded.
#[tauri::command]
#[specta::specta]
pub async fn list_job_steps(state: State<'_, AppState>, job_id: Uuid) -> CmdResult<Vec<JobStep>> {
    Ok(state.store.list_job_steps(job_id).await?)
}

// ── Pipelines ───────────────────────────────────────────────────────────

#[tauri::command]
#[specta::specta]
pub async fn list_pipelines(state: State<'_, AppState>) -> CmdResult<Vec<Pipeline>> {
    Ok(state.store.list_pipelines().await?)
}

#[tauri::command]
#[specta::specta]
pub async fn get_pipeline(state: State<'_, AppState>, id: Uuid) -> CmdResult<Option<Pipeline>> {
    Ok(state.store.get_pipeline(id).await?)
}

/// Create a pipeline. Refused unless the whole chain is runnable.
#[tauri::command]
#[specta::specta]
pub async fn create_pipeline(
    state: State<'_, AppState>,
    input: PipelineCreate,
) -> CmdResult<Pipeline> {
    let pipeline = state.store.create_pipeline(input).await?;
    state
        .store
        .audit(
            AuditAction::PipelineCreated,
            &pipeline.name,
            describe_pipeline(&pipeline),
        )
        .await;
    Ok(pipeline)
}

#[tauri::command]
#[specta::specta]
pub async fn update_pipeline(
    state: State<'_, AppState>,
    id: Uuid,
    patch: PipelineUpdate,
) -> CmdResult<Pipeline> {
    let before = state.store.get_pipeline(id).await?;
    let after = state.store.update_pipeline(id, patch).await?;

    // Worth recording specifically: an edit that revokes an unattended
    // authorisation is the change somebody will be looking for later.
    let disarmed = before.is_some_and(|b| b.unattended_ack.is_some())
        && after.unattended_ack.is_none();
    state
        .store
        .audit(
            AuditAction::PipelineUpdated,
            &after.name,
            match disarmed {
                true => format!(
                    "{}; no longer authorised to run unattended",
                    describe_pipeline(&after)
                ),
                false => describe_pipeline(&after),
            },
        )
        .await;
    Ok(after)
}

/// Authorise a destructive pipeline to run with nobody present, or withdraw it.
///
/// `typed` is the target names as the user typed them back, newline-separated
/// when there is more than one. The engine compares it against what the
/// pipeline currently drops, so a typo authorises nothing.
#[tauri::command]
#[specta::specta]
pub async fn arm_pipeline(
    state: State<'_, AppState>,
    id: Uuid,
    typed: Option<String>,
) -> CmdResult<Pipeline> {
    let pipeline = state.store.arm_pipeline(id, typed.as_deref()).await?;
    state
        .store
        .audit(
            AuditAction::PipelineArmed,
            &pipeline.name,
            match &pipeline.unattended_ack {
                Some(targets) => format!("may now drop {} unattended", targets.replace('\n', ", ")),
                None => "unattended authorisation withdrawn".to_string(),
            },
        )
        .await;
    Ok(pipeline)
}

#[tauri::command]
#[specta::specta]
pub async fn delete_pipeline(state: State<'_, AppState>, id: Uuid) -> CmdResult<bool> {
    let pipeline = state.store.get_pipeline(id).await?;
    let deleted = state.store.delete_pipeline(id).await?;
    if let (true, Some(pipeline)) = (deleted, pipeline) {
        state
            .store
            .audit(
                AuditAction::PipelineDeleted,
                &pipeline.name,
                describe_pipeline(&pipeline),
            )
            .await;
    }
    Ok(deleted)
}

/// Start a saved pipeline and return its job id immediately.
///
/// `typed_confirmations` carries one name per destructive step, in the order
/// those steps appear. They are not checked here: each is handed to the restore
/// it belongs to and validated by the engine, which is the same check a
/// hand-built restore goes through.
#[tauri::command]
#[specta::specta]
pub async fn start_pipeline(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: Uuid,
    typed_confirmations: Vec<String>,
) -> CmdResult<Uuid> {
    let pipeline = state.store.require_pipeline(id).await?;
    let profiles = state.store.list_profiles().await?;
    // Fail before a job appears in history, the way start_backup does.
    pipeline
        .validate_against(&profiles)
        .map_err(|e| CommandError::new("invalid", e.to_string()))?;

    // History records what a *connection* did, so the job is attributed to the
    // first step that names one. A pipeline of pure retention names none.
    let source = pipeline
        .steps
        .iter()
        .find_map(|s| s.profile_id())
        .ok_or_else(|| {
            CommandError::new("invalid", "this pipeline touches no connection")
        })?;
    let dest = pipeline
        .steps
        .iter()
        .rev()
        .find_map(|s| s.profile_id());

    let request = ops::PipelineRunRequest {
        typed_confirmations,
        default_output_dir: default_backup_dir(),
    };

    let job_id = Uuid::new_v4();
    let ctx = JobContext::with_sender(job_id, state.event_tx.clone());
    state.jobs.register(&ctx).await;

    let options_json = serde_json::to_string(&PipelineJobOptions {
        pipeline: pipeline.name.clone(),
        steps: pipeline.steps.len(),
    })
    .unwrap_or_else(|_| "{}".into());
    ops::record_start(
        &state.store,
        &ctx,
        JobKind::Sync,
        source,
        dest,
        options_json,
    )
    .await
    .map_err(|e| CommandError::new("storage", e.to_string()))?;

    let store = state.store.clone();
    let jobs = state.jobs.clone();

    tauri::async_runtime::spawn(async move {
        let tools = store.tool_source().await;
        let result = ops::run_pipeline(&pipeline, &request, &store, &tools, &ctx).await;

        let (outcome, artifact) = match &result {
            // A chain that restored but failed its own verification is not a
            // success: the question asked of the history is "did it work".
            Ok(done) if done.fully_succeeded() => {
                (JobOutcome::Success, done.artifacts.last().cloned())
            }
            Ok(done) => {
                ctx.emit_error(
                    JobPhase::Done,
                    "the pipeline ran but not every step did what it claimed",
                )
                .await;
                (JobOutcome::Failed, done.artifacts.last().cloned())
            }
            Err(e) => {
                ctx.emit_error(JobPhase::Done, e.to_string()).await;
                let outcome = if ctx.is_cancelled() {
                    JobOutcome::Cancelled
                } else {
                    JobOutcome::Failed
                };
                (outcome, None)
            }
        };

        let _ = ops::record_finish(&store, &ctx, outcome, artifact).await;
        jobs.unregister(job_id).await;

        let _ = JobFinished {
            job_id: job_id.to_string(),
            outcome: outcome.as_str().to_string(),
        }
        .emit(&app);
    });

    Ok(job_id)
}

/// What the job history records about a pipeline run.
///
/// The whole definition would be a large blob nobody reads back; the name and
/// the shape are what a person scanning history wants.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
struct PipelineJobOptions {
    pipeline: String,
    steps: usize,
}

/// One line saying what a pipeline does, for the audit trail.
fn describe_pipeline(pipeline: &Pipeline) -> String {
    let shape = pipeline
        .steps
        .iter()
        .map(|s| s.kind().as_str())
        .collect::<Vec<_>>()
        .join(" → ");
    match pipeline.destructive_signature() {
        Some(targets) => format!("{shape}; replaces {}", targets.replace('\n', ", ")),
        None => shape,
    }
}

/// Cancel a running job.
///
/// This signals the job's `CancellationToken`, which propagates into its child
/// processes and tunnels. Returns false when the job is not running.
#[tauri::command]
#[specta::specta]
pub async fn cancel_job(state: State<'_, AppState>, job_id: Uuid) -> CmdResult<bool> {
    Ok(state.jobs.cancel(job_id).await)
}

#[tauri::command]
#[specta::specta]
pub async fn active_job_ids(state: State<'_, AppState>) -> CmdResult<Vec<Uuid>> {
    Ok(state.jobs.active_ids().await)
}

/// Look for every client binary through the configured source.
///
/// On a blocking thread: probing versions runs each tool, and with a container
/// source that means starting a container per tool. Holding the async runtime
/// for that would freeze every other command, including the one drawing the
/// spinner the user is looking at.
#[tauri::command]
#[specta::specta]
pub async fn discover_tools(state: State<'_, AppState>) -> CmdResult<Vec<ToolStatus>> {
    let source = state.store.tool_source().await;
    tauri::async_runtime::spawn_blocking(move || db_sync_engine::tools::discover(&source))
        .await
        .map_err(|e| CommandError::new("tools", format!("tool discovery did not finish: {e}")))
}

/// The same, for a source the user is considering but has not saved.
///
/// Separate from [`discover_tools`] so the settings page can answer "would
/// this container work?" before committing to it — finding out by saving and
/// then watching a backup fail is a much worse way to learn.
#[tauri::command]
#[specta::specta]
pub async fn test_tool_source(source: ToolSource) -> CmdResult<Vec<ToolStatus>> {
    tauri::async_runtime::spawn_blocking(move || db_sync_engine::tools::discover(&source))
        .await
        .map_err(|e| CommandError::new("tools", format!("tool discovery did not finish: {e}")))
}

/// Running containers, for the exec source picker.
#[tauri::command]
#[specta::specta]
pub async fn list_docker_containers() -> CmdResult<Vec<DockerContainer>> {
    tauri::async_runtime::spawn_blocking(db_sync_engine::tools::docker_containers)
        .await
        .map_err(|e| CommandError::new("docker", format!("could not reach Docker: {e}")))?
        .map_err(|m| CommandError::new("tools", m))
}

/// Install a client with Homebrew.
///
/// The formula is validated in the engine against the app's own list, so this
/// cannot be talked into installing something else.
#[tauri::command]
#[specta::specta]
pub async fn install_tool_with_brew(formula: String) -> CmdResult<String> {
    tauri::async_runtime::spawn_blocking(move || db_sync_engine::tools::brew_install(&formula))
        .await
        .map_err(|e| CommandError::new("brew", format!("the install did not finish: {e}")))?
        .map_err(|m| CommandError::new("tools", m))
}

#[tauri::command]
#[specta::specta]
pub async fn app_info(state: State<'_, AppState>) -> CmdResult<AppInfo> {
    Ok(AppInfo {
        engine_version: db_sync_engine::ENGINE_VERSION.to_string(),
        store_path: state.store_path.display().to_string(),
    })
}

// ── Running jobs ────────────────────────────────────────────────────────

/// Where backups are written when the caller does not say otherwise.
fn default_backup_dir() -> PathBuf {
    db_sync_engine::paths::app_data_dir()
        .map(|d| d.join("backups"))
        .unwrap_or_else(|_| PathBuf::from("backups"))
}

#[tauri::command]
#[specta::specta]
pub async fn backup_directory() -> CmdResult<String> {
    Ok(default_backup_dir().display().to_string())
}

/// Start a backup and return its job id immediately.
///
/// The work runs in the background; progress arrives as `JobProgress` events
/// and the terminal state as `JobFinished`. Blocking the command until the dump
/// finished would freeze the UI for the length of the job.
#[tauri::command]
#[specta::specta]
pub async fn start_backup(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    profile_id: Uuid,
    request: BackupRequest,
) -> CmdResult<Uuid> {
    let profile = state.store.require_profile(profile_id).await?;
    // Fail fast on an invalid plan, before a job appears in history.
    request
        .validate(&profile)
        .map_err(|e| CommandError::new("invalid", e.to_string()))?;

    let job_id = Uuid::new_v4();
    let ctx = JobContext::with_sender(job_id, state.event_tx.clone());
    state.jobs.register(&ctx).await;

    let options_json = serde_json::to_string(&request).unwrap_or_else(|_| "{}".into());
    ops::record_start(
        &state.store,
        &ctx,
        JobKind::Backup,
        profile_id,
        None,
        options_json,
    )
    .await
    .map_err(|e| CommandError::new("storage", e.to_string()))?;

    let store = state.store.clone();
    let jobs = state.jobs.clone();

    tauri::async_runtime::spawn(async move {
        let tools = store.tool_source().await;
        let result = ops::backup(&profile, &request, &store, &tools, &ctx).await;

        let (outcome, artifact) = match &result {
            Ok(path) => (JobOutcome::Success, Some(path.display().to_string())),
            Err(e) => {
                // The full error, including the tool's stderr, belongs in the log.
                ctx.emit_error(JobPhase::Done, e.to_string()).await;
                let outcome = if ctx.is_cancelled() {
                    JobOutcome::Cancelled
                } else {
                    JobOutcome::Failed
                };
                (outcome, None)
            }
        };

        let _ = ops::record_finish(&store, &ctx, outcome, artifact).await;
        jobs.unregister(job_id).await;

        let _ = JobFinished {
            job_id: job_id.to_string(),
            outcome: outcome.as_str().to_string(),
        }
        .emit(&app);
    });

    Ok(job_id)
}

/// Start a restore and return its job id immediately.
#[tauri::command]
#[specta::specta]
pub async fn start_restore(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    profile_id: Uuid,
    request: RestoreRequest,
) -> CmdResult<Uuid> {
    let profile = state.store.require_profile(profile_id).await?;

    // Typed confirmation and format rules are checked here so a destructive
    // restore cannot even start without them.
    let manifest = db_sync_engine::manifest::BackupManifest::read(&request.artifact_path).ok();
    request
        .validate(&profile, manifest.as_ref())
        .map_err(|e| CommandError::new("invalid", e.to_string()))?;

    let job_id = Uuid::new_v4();
    let ctx = JobContext::with_sender(job_id, state.event_tx.clone());
    state.jobs.register(&ctx).await;

    let options_json = serde_json::to_string(&request).unwrap_or_else(|_| "{}".into());
    ops::record_start(
        &state.store,
        &ctx,
        JobKind::Restore,
        profile_id,
        Some(profile_id),
        options_json,
    )
    .await
    .map_err(|e| CommandError::new("storage", e.to_string()))?;

    let store = state.store.clone();
    let jobs = state.jobs.clone();

    tauri::async_runtime::spawn(async move {
        let tools = store.tool_source().await;
        let result = ops::restore(&profile, &request, &store, &tools, &ctx).await;

        let outcome = match &result {
            Ok(target) => {
                ctx.emit(JobPhase::Done, format!("restored into {target}"))
                    .await;
                JobOutcome::Success
            }
            Err(e) => {
                ctx.emit_error(JobPhase::Done, e.to_string()).await;
                if ctx.is_cancelled() {
                    JobOutcome::Cancelled
                } else {
                    JobOutcome::Failed
                }
            }
        };

        let _ = ops::record_finish(&store, &ctx, outcome, None).await;
        jobs.unregister(job_id).await;

        let _ = JobFinished {
            job_id: job_id.to_string(),
            outcome: outcome.as_str().to_string(),
        }
        .emit(&app);
    });

    Ok(job_id)
}

// ── Library ─────────────────────────────────────────────────────────────

#[tauri::command]
#[specta::specta]
pub async fn list_artifacts(directory: Option<String>) -> CmdResult<Vec<Artifact>> {
    let dir = directory
        .map(PathBuf::from)
        .unwrap_or_else(default_backup_dir);
    Ok(library::list_artifacts(dir))
}

/// Hash an artifact and compare it against its manifest.
#[tauri::command]
#[specta::specta]
pub async fn check_artifact(path: String) -> CmdResult<IntegrityCheck> {
    // Hashing a multi-gigabyte file blocks; keep it off the async runtime.
    tokio::task::spawn_blocking(move || library::check_integrity(path))
        .await
        .map_err(|e| CommandError::new("io", e.to_string()))
}

/// Delete an artifact and its manifest.
#[tauri::command]
#[specta::specta]
pub async fn delete_artifact(path: String) -> CmdResult<()> {
    let artifact = PathBuf::from(&path);
    if !artifact.is_file() {
        return Err(CommandError::new(
            "not_found",
            format!("no such file: {path}"),
        ));
    }
    std::fs::remove_file(&artifact).map_err(|e| CommandError::new("io", e.to_string()))?;
    // Leaving the manifest behind would show a phantom entry in the library.
    let _ = std::fs::remove_file(db_sync_engine::manifest::BackupManifest::path_for(
        &artifact,
    ));
    Ok(())
}

// ── Sync plans ──────────────────────────────────────────────────────────

#[tauri::command]
#[specta::specta]
pub async fn list_sync_plans(
    state: State<'_, AppState>,
    profile_id: Uuid,
) -> CmdResult<Vec<SyncPlan>> {
    Ok(state.store.list_sync_plans(profile_id).await?)
}

#[tauri::command]
#[specta::specta]
pub async fn create_sync_plan(
    state: State<'_, AppState>,
    input: SyncPlanCreate,
) -> CmdResult<SyncPlan> {
    if input.name.trim().is_empty() {
        return Err(CommandError::new("invalid", "plan name cannot be empty"));
    }
    state.store.require_profile(input.profile_id).await?;
    Ok(state.store.create_sync_plan(input).await?)
}

#[tauri::command]
#[specta::specta]
pub async fn update_sync_plan(
    state: State<'_, AppState>,
    id: Uuid,
    selections: Vec<TableSelection>,
) -> CmdResult<SyncPlan> {
    Ok(state.store.update_sync_plan(id, selections).await?)
}

/// Rename a table set.
///
/// Separate from `update_sync_plan` because it must not bump the revision: a
/// revision means "what this set backs up changed", and a typo fix changes
/// nothing a schedule needs to re-examine.
#[tauri::command]
#[specta::specta]
pub async fn rename_sync_plan(
    state: State<'_, AppState>,
    id: Uuid,
    name: String,
) -> CmdResult<SyncPlan> {
    Ok(state.store.rename_sync_plan(id, name).await?)
}

#[tauri::command]
#[specta::specta]
pub async fn delete_sync_plan(state: State<'_, AppState>, id: Uuid) -> CmdResult<bool> {
    Ok(state.store.delete_sync_plan(id).await?)
}

#[tauri::command]
#[specta::specta]
pub async fn set_sync_plan_masking(
    state: State<'_, AppState>,
    id: Uuid,
    masking: Vec<MaskRule>,
) -> CmdResult<SyncPlan> {
    let before = state.store.get_sync_plan(id).await?;
    let count = masking.len();
    let plan = state.store.set_sync_plan_masking(id, masking).await?;

    state
        .store
        .audit(
            AuditAction::MaskingChanged,
            &plan.name,
            format!(
                "{} rule(s), was {}",
                count,
                before.map(|p| p.masking.len()).unwrap_or(0)
            ),
        )
        .await;
    Ok(plan)
}

/// The SQL a masking run would send to the destination.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct MaskingPreview {
    /// One `UPDATE` per table.
    pub updates: Vec<String>,
    /// The read-back; every count must be zero or the sync aborts.
    pub checks: Vec<String>,
    /// Rules that would not run, with the reason.
    pub inert: Vec<db_sync_engine::mask::InertRule>,
}

/// Show what masking would do, without running it.
///
/// The salt appears as a bound placeholder rather than a literal, here and in
/// the real statements, so this output is safe to paste into a ticket.
#[tauri::command]
#[specta::specta]
pub async fn masking_preview(
    state: State<'_, AppState>,
    plan_id: Uuid,
) -> CmdResult<MaskingPreview> {
    use db_sync_engine::mask;

    let plan = state
        .store
        .get_sync_plan(plan_id)
        .await?
        .ok_or_else(|| CommandError::new("not_found", "no such sync plan"))?;
    let profile = state.store.require_profile(plan.profile_id).await?;

    let active: Vec<MaskRule> = plan.active_masking().into_iter().cloned().collect();
    let with_data = plan.tables_with_data();
    let inert = plan
        .masking
        .iter()
        .filter(|r| !with_data.contains(&r.table))
        .map(|rule| db_sync_engine::mask::InertRule {
            rule: rule.clone(),
            reason: format!(
                "{} is not in this plan as a table carrying data, so no rows reach the \
                 destination to mask",
                rule.table
            ),
        })
        .collect();

    let placeholder = "<salt bound at run time>";
    Ok(MaskingPreview {
        updates: mask::update_statements(profile.engine, &active, placeholder)
            .map_err(|e| CommandError::new("invalid", e.to_string()))?
            .into_iter()
            .map(|u| u.statement.sql)
            .collect(),
        checks: mask::check_statements(profile.engine, &active)
            .map_err(|e| CommandError::new("invalid", e.to_string()))?
            .into_iter()
            .map(|c| c.statement.sql)
            .collect(),
        inert,
    })
}

// ── Backup encryption key ───────────────────────────────────────────────

#[tauri::command]
#[specta::specta]
pub async fn backup_key_status(state: State<'_, AppState>) -> CmdResult<KeyStatus> {
    backupkey::status(&state.store)
        .await
        .map_err(|e| CommandError::new("key", e.to_string()))
}

#[tauri::command]
#[specta::specta]
pub async fn generate_backup_key(state: State<'_, AppState>) -> CmdResult<KeyStatus> {
    backupkey::ensure_exists(&state.store)
        .await
        .map_err(|e| CommandError::new("key", e.to_string()))
}

#[tauri::command]
#[specta::specta]
pub async fn set_backup_key_recipients(
    state: State<'_, AppState>,
    keys: Vec<String>,
) -> CmdResult<KeyStatus> {
    backupkey::set_extra_recipients(&state.store, &keys)
        .await
        .map_err(|e| CommandError::new("key", e.to_string()))?;
    backup_key_status(state).await
}

/// Write the secret key to a file and return where it went.
///
/// The secret is deliberately **not** returned. The webview can ask for the
/// key to be escrowed and can be told where it landed, but there is no command
/// anywhere in this app that hands a secret to the frontend — the same rule
/// that governs database passwords. Copying it out of a file is the user's
/// job, and it means the value never sits in a JS string, a React state atom,
/// or a devtools console.
#[tauri::command]
#[specta::specta]
pub async fn export_backup_key_to_file(state: State<'_, AppState>) -> CmdResult<String> {
    let secret = backupkey::export(&state.store)
        .await
        .map_err(|e| CommandError::new("key", e.to_string()))?;

    let dir = db_sync_engine::paths::app_data_dir()
        .map_err(|e| CommandError::new("io", e.to_string()))?;
    let path = dir.join("backup-key.txt");

    write_secret_file(&path, secret.expose_secret())
        .map_err(|e| CommandError::new("io", e.to_string()))?;

    // Worth recording: from this moment the key exists somewhere this
    // application does not control. The path, never the key.
    state
        .store
        .audit(
            AuditAction::BackupKeyExported,
            path.display().to_string(),
            "the key now exists outside this application",
        )
        .await;

    Ok(path.display().to_string())
}

/// Write a secret to a file only its owner can read.
///
/// The mode is set as part of *creating* the file rather than afterwards, so
/// there is no window in which the secret exists at the default umask and
/// another user on a shared machine could read it.
fn write_secret_file(path: &std::path::Path, secret: &str) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    writeln!(file, "{secret}")
}

/// Turn a legacy `tables.conf` into a complete selection list.
///
/// Lets an existing Bash-tool setup be carried over without retyping a couple
/// of hundred table names.
///
/// `available` is the source's table list, and the file cannot be honoured
/// without it: the file names only what carries data, while a selection that
/// stays silent about a table gives it data at run time. See
/// [`plan::selections_from_tables_conf`]. The caller passes the introspection
/// it has already made rather than this connecting again, so an import is still
/// one click on a list the user is looking at.
#[tauri::command]
#[specta::specta]
pub async fn import_tables_conf(
    contents: String,
    available: Vec<String>,
) -> CmdResult<Vec<TableSelection>> {
    if available.is_empty() {
        return Err(CommandError::new(
            "no_tables",
            "the source's table list is not loaded, so the file cannot be \
             completed; without it every table the file omits would be backed \
             up with its data",
        ));
    }
    Ok(plan::selections_from_tables_conf(&contents, &available))
}

// ── Sync ────────────────────────────────────────────────────────────────

/// Start a source-to-destination sync and return its job id.
#[tauri::command]
#[specta::specta]
pub async fn start_sync(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    source_id: Uuid,
    dest_id: Uuid,
    request: SyncRequest,
) -> CmdResult<Uuid> {
    let source = state.store.require_profile(source_id).await?;
    let dest = state.store.require_profile(dest_id).await?;

    // Everything checkable offline is checked before a job appears in history.
    request
        .backup
        .validate(&source)
        .map_err(|e| CommandError::new("invalid", e.to_string()))?;
    if source.engine != dest.engine {
        return Err(CommandError::new(
            "engine_mismatch",
            format!(
                "cannot sync a {:?} source to a {:?} destination",
                source.engine, dest.engine
            ),
        ));
    }

    let job_id = Uuid::new_v4();
    let ctx = JobContext::with_sender(job_id, state.event_tx.clone());
    state.jobs.register(&ctx).await;

    let options_json = serde_json::to_string(&request).unwrap_or_else(|_| "{}".into());
    ops::record_start(
        &state.store,
        &ctx,
        JobKind::Sync,
        source_id,
        Some(dest_id),
        options_json,
    )
    .await
    .map_err(|e| CommandError::new("storage", e.to_string()))?;

    let store = state.store.clone();
    let jobs = state.jobs.clone();

    tauri::async_runtime::spawn(async move {
        let tools = store.tool_source().await;
        let result = ops::sync(&source, &dest, &request, &store, &tools, &ctx).await;

        let (outcome, artifact) = match &result {
            Ok(o) => {
                // A sync whose verification failed is not a success, even
                // though every individual step reported one.
                let verified = o.verification.as_ref().is_none_or(|r| r.passed());
                let outcome = if verified {
                    JobOutcome::Success
                } else {
                    ctx.emit_error(
                        JobPhase::Done,
                        "the data was restored but verification found discrepancies",
                    )
                    .await;
                    JobOutcome::Failed
                };
                (outcome, Some(o.artifact_path.clone()))
            }
            Err(e) => {
                ctx.emit_error(JobPhase::Done, e.to_string()).await;
                let outcome = if ctx.is_cancelled() {
                    JobOutcome::Cancelled
                } else {
                    JobOutcome::Failed
                };
                (outcome, None)
            }
        };

        let _ = ops::record_finish(&store, &ctx, outcome, artifact).await;
        jobs.unregister(job_id).await;

        let _ = JobFinished {
            job_id: job_id.to_string(),
            outcome: outcome.as_str().to_string(),
        }
        .emit(&app);
    });

    Ok(job_id)
}

// ── Schedules ───────────────────────────────────────────────────────────

/// A schedule plus the things the list view needs but the record does not hold:
/// when it next runs, and what the cron expression means in English.
///
/// Computed here rather than in TypeScript so there is exactly one cron
/// implementation. A second one in the frontend would eventually disagree with
/// the scheduler, and the UI would confidently show a time nothing happens at.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ScheduleView {
    pub schedule: Schedule,
    pub description: String,
    pub next_run_at: Option<chrono::DateTime<chrono::Utc>>,
    /// True while a run started by this schedule is still going.
    pub running: bool,
}

impl From<db_sync_engine::schedule::ScheduleError> for CommandError {
    fn from(e: db_sync_engine::schedule::ScheduleError) -> Self {
        use db_sync_engine::schedule::ScheduleError as E;
        let kind = match &e {
            E::DestructiveTarget(_) => "destructive_schedule",
            E::BadWebhook(..) => "bad_webhook",
            E::NameRequired
            | E::RestoreMismatch
            | E::PlanRequired
            | E::DrillTakesNoPlan
            | E::DrillNeedsProfile
            | E::DrillTakesNoTarget
            | E::EngineMismatch { .. } => "invalid",
        };
        Self::new(kind, e.to_string())
    }
}

async fn view(state: &AppState, schedule: Schedule) -> ScheduleView {
    let running = state.scheduler.in_flight_ids().await.contains(&schedule.id);
    let now = chrono::Utc::now();
    ScheduleView {
        description: schedule.cron.describe(),
        next_run_at: schedule.next_run_at(now),
        running,
        schedule,
    }
}

#[tauri::command]
#[specta::specta]
pub async fn list_schedules(state: State<'_, AppState>) -> CmdResult<Vec<ScheduleView>> {
    let schedules = state.store.list_schedules().await?;
    let mut out = Vec::with_capacity(schedules.len());
    for s in schedules {
        out.push(view(&state, s).await);
    }
    Ok(out)
}

#[tauri::command]
#[specta::specta]
pub async fn get_schedule(state: State<'_, AppState>, id: Uuid) -> CmdResult<Option<ScheduleView>> {
    match state.store.get_schedule(id).await? {
        Some(s) => Ok(Some(view(&state, s).await)),
        None => Ok(None),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn create_schedule(
    state: State<'_, AppState>,
    input: ScheduleCreate,
) -> CmdResult<ScheduleView> {
    // Checked here as well as in the store so the message names the field the
    // form should highlight, rather than arriving as a generic storage error.
    input.validate()?;
    let created = state.store.create_schedule(input).await?;
    Ok(view(&state, created).await)
}

#[tauri::command]
#[specta::specta]
pub async fn update_schedule(
    state: State<'_, AppState>,
    id: Uuid,
    patch: ScheduleUpdate,
) -> CmdResult<ScheduleView> {
    let updated = state.store.update_schedule(id, patch).await?;
    Ok(view(&state, updated).await)
}

#[tauri::command]
#[specta::specta]
pub async fn delete_schedule(state: State<'_, AppState>, id: Uuid) -> CmdResult<bool> {
    let name = state
        .store
        .get_schedule(id)
        .await?
        .map(|s| s.name)
        .unwrap_or_else(|| id.to_string());
    state
        .store
        .audit(
            AuditAction::ScheduleDeleted,
            name,
            "an unattended job stops running",
        )
        .await;
    Ok(state.store.delete_schedule(id).await?)
}

/// Run a schedule immediately, returning its job id.
///
/// Deliberately does not move the schedule's high-water mark: testing a
/// schedule now must not cancel the occurrence it was created for.
#[tauri::command]
#[specta::specta]
pub async fn run_schedule_now(state: State<'_, AppState>, id: Uuid) -> CmdResult<Uuid> {
    match state.scheduler.run_now(id).await? {
        Some(job_id) => Ok(job_id),
        None => Err(CommandError::new(
            "already_running",
            "that schedule is already running",
        )),
    }
}

/// What a cron expression means, and the next few times it fires.
///
/// This is what the schedule form shows as the user types. Seeing the next five
/// real timestamps is the only reliable way to catch a mistyped expression
/// before it silently backs up at the wrong time for a month.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct CronPreview {
    pub description: String,
    pub next_runs: Vec<chrono::DateTime<chrono::Utc>>,
}

#[tauri::command]
#[specta::specta]
pub async fn preview_cron(
    expression: String,
    timezone: ScheduleTimezone,
) -> CmdResult<CronPreview> {
    let parsed: CronExpression =
        expression
            .parse()
            .map_err(|e: db_sync_engine::cron::CronError| {
                CommandError::new("invalid_cron", e.to_string())
            })?;

    let mut next_runs = Vec::new();
    let mut cursor = chrono::Utc::now();
    for _ in 0..5 {
        match parsed.next_after(timezone, cursor) {
            Some(t) => {
                next_runs.push(t);
                cursor = t;
            }
            // An expression that resolves a few times and then stops is
            // possible (`0 0 29 2 *` runs out of the scan window); showing
            // what there is beats showing nothing.
            None => break,
        }
    }

    Ok(CronPreview {
        description: parsed.describe(),
        next_runs,
    })
}

/// The `dbsync` invocation that runs this schedule from system cron.
///
/// Offered because plenty of DBAs would rather their backups were driven by the
/// same cron that runs everything else on the box than by a desktop app that
/// has to stay open.
#[tauri::command]
#[specta::specta]
pub async fn crontab_line(state: State<'_, AppState>, id: Uuid) -> CmdResult<String> {
    let schedule = state.store.require_schedule(id).await?;

    // An absolute path, whenever one is known. `cron` runs with a bare `PATH`
    // that usually contains neither ~/.local/bin nor the inside of an
    // application bundle, so a line saying just `dbsync` is a line that fails
    // at 03:00 with "command not found". Prefer whatever is already on the
    // user's PATH, fall back to the copy shipped in the bundle, and only emit
    // a bare name when neither exists.
    let status = cli_tool::status();
    let program = status
        .installed_path
        .or(status.bundled_path)
        .unwrap_or_else(|| "dbsync".into());

    Ok(format!(
        "{} {} --store {} schedule run {} >> {} 2>&1",
        schedule.cron.as_str(),
        shell_quote(&program),
        shell_quote(&state.store_path.display().to_string()),
        schedule.id,
        shell_quote(
            &schedule
                .action
                .output_dir
                .join("dbsync-cron.log")
                .display()
                .to_string()
        ),
    ))
}

/// Quote a path for a crontab line, which `/bin/sh` interprets.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SchedulerStatus {
    /// Whether the in-app scheduler loop is running right now.
    pub running: bool,
    pub in_flight: Vec<Uuid>,
}

#[tauri::command]
#[specta::specta]
pub async fn scheduler_status(state: State<'_, AppState>) -> CmdResult<SchedulerStatus> {
    Ok(SchedulerStatus {
        running: state.scheduler_running(),
        in_flight: state.scheduler.in_flight_ids().await,
    })
}

// ── Application settings ────────────────────────────────────────────────

#[tauri::command]
#[specta::specta]
pub async fn get_app_settings(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<AppSettings> {
    let mut stored = state.store.app_settings().await?;
    // The OS owns this one: the user can remove the login item outside the app,
    // and reporting our stale copy would show a toggle that lies.
    stored.launch_at_login = app.autolaunch().is_enabled().unwrap_or(false);
    Ok(stored)
}

#[tauri::command]
#[specta::specta]
pub async fn set_app_settings(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    next: AppSettings,
) -> CmdResult<AppSettings> {
    state
        .store
        .set_flag(settings::SCHEDULER_ENABLED, next.scheduler_enabled)
        .await?;
    state
        .store
        .set_flag(settings::CLOSE_TO_TRAY, next.close_to_tray)
        .await?;
    state
        .store
        .set_setting(
            settings::TOOL_SOURCE,
            &serde_json::to_string(&next.tool_source).unwrap_or_default(),
        )
        .await?;
    // Keep the copy the window-close handler reads in step with the stored one.
    state
        .close_to_tray
        .store(next.close_to_tray, std::sync::atomic::Ordering::SeqCst);

    // Applied immediately rather than at next launch: a user who turns the
    // scheduler on expects tonight's backup to run, not tomorrow's.
    if next.scheduler_enabled {
        if !state.scheduler_running() {
            state.start_scheduler();
        }
    } else {
        state.stop_scheduler();
    }

    let autolaunch = app.autolaunch();
    let result = if next.launch_at_login {
        autolaunch.enable()
    } else {
        autolaunch.disable()
    };
    if let Err(e) = result {
        return Err(CommandError::new(
            "autostart",
            format!("could not change the launch-at-login setting: {e}"),
        ));
    }

    get_app_settings(app, state).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_path_with_spaces_is_quoted_for_cron() {
        // "~/Library/Application Support/..." is the normal macOS path, and an
        // unquoted crontab line there runs with the wrong arguments.
        assert_eq!(
            shell_quote("/Users/a/Library/Application Support/DBSync/store.db"),
            "'/Users/a/Library/Application Support/DBSync/store.db'"
        );
        assert_eq!(shell_quote("/opt/dbsync/store.db"), "/opt/dbsync/store.db");
    }

    #[test]
    fn an_embedded_quote_cannot_break_out_of_the_crontab_line() {
        assert_eq!(shell_quote("/tmp/it's here"), r"'/tmp/it'\''s here'");
    }

    #[test]
    fn an_exported_key_is_readable_only_by_its_owner() {
        // The whole reason the export writes a file instead of returning the
        // key: it stays out of the webview. That is only an improvement if the
        // file it lands in is not world-readable.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup-key.txt");
        write_secret_file(&path, "AGE-SECRET-KEY-TEST").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "AGE-SECRET-KEY-TEST\n"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "got {mode:o}");
        }
    }

    #[test]
    fn re_exporting_does_not_leave_the_old_key_behind() {
        // Truncation matters: an age secret is a fixed length, so a shorter
        // second write would leave a readable tail of the first.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup-key.txt");
        write_secret_file(&path, "AGE-SECRET-KEY-A-VERY-LONG-ONE").unwrap();
        write_secret_file(&path, "SHORT").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SHORT\n");
    }
}

/// Recent configuration changes, newest first.
#[tauri::command]
#[specta::specta]
pub async fn list_audit(
    state: State<'_, AppState>,
    // `u32`, not `i64`: specta forbids exporting BigInt-style types, and a
    // clamp here is better than trusting the caller anyway.
    limit: u32,
) -> CmdResult<Vec<db_sync_engine::audit::AuditEntry>> {
    Ok(state
        .store
        .list_audit(i64::from(limit.clamp(1, 500)))
        .await?)
}

// ── Shareable configuration ─────────────────────────────────────────────

/// Write a shareable bundle to a file and return the path.
///
/// A file rather than a string, for the same reason the backup key export is:
/// the thing the user then does with it is put it somewhere. Unlike the key,
/// this one carries no secret — but returning a path keeps both exports
/// working the same way, and the file is what gets committed or attached.
#[tauri::command]
#[specta::specta]
pub async fn export_config_to_file(state: State<'_, AppState>) -> CmdResult<String> {
    let bundle = db_sync_engine::share::export(&state.store)
        .await
        .map_err(|e| CommandError::new("share", e.to_string()))?;

    let path = default_backup_dir()
        .parent()
        .unwrap_or(&default_backup_dir())
        .join("dbsync-config.json");

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CommandError::new("io", format!("could not create {parent:?}: {e}")))?;
    }
    std::fs::write(
        &path,
        bundle
            .to_json()
            .map_err(|e| CommandError::new("share", e.to_string()))?,
    )
    .map_err(|e| CommandError::new("io", format!("could not write {path:?}: {e}")))?;

    Ok(path.display().to_string())
}

/// Read a bundle and report what it would change, without changing anything.
#[tauri::command]
#[specta::specta]
pub async fn preview_config_import(path: String) -> CmdResult<db_sync_engine::share::ConfigBundle> {
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| CommandError::new("io", format!("could not read {path}: {e}")))?;
    db_sync_engine::share::ConfigBundle::from_json(&raw)
        .map_err(|e| CommandError::new("share", e.to_string()))
}

/// Apply a bundle. Never writes a credential; never removes anything.
#[tauri::command]
#[specta::specta]
pub async fn import_config(
    state: State<'_, AppState>,
    path: String,
) -> CmdResult<db_sync_engine::share::ImportReport> {
    let bundle = preview_config_import(path).await?;
    // `share::import` records the audit entry itself, so the CLI and the app
    // produce the same record.
    db_sync_engine::share::import(&state.store, &bundle)
        .await
        .map_err(|e| CommandError::new("share", e.to_string()))
}

/// Size and growth across the backup library.
#[tauri::command]
#[specta::specta]
pub async fn library_stats(directory: Option<String>) -> CmdResult<library::LibraryStats> {
    let dir = match directory {
        Some(d) => PathBuf::from(d),
        None => default_backup_dir(),
    };
    Ok(library::stats(dir))
}

// ── Off-site destinations ───────────────────────────────────────────────

/// A destination as the UI sees it.
///
/// The destination itself carries no secret — see [`db_sync_engine::destination`]
/// — so it crosses the boundary whole. What is added here is the one fact the
/// UI needs and cannot derive: whether a credential has been filed for it. A
/// destination with no key is configured and unusable, which looks identical
/// to a working one in every other respect.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DestinationView {
    #[serde(flatten)]
    pub destination: Destination,
    pub has_credential: bool,
    /// Where this points, e.g. `s3://backups/prod`.
    pub location: String,
}

fn destination_view(destination: Destination) -> DestinationView {
    let has_credential =
        secrets::has_secret(destination.id, SecretKind::ObjectStoreSecret).unwrap_or(false);
    DestinationView {
        location: destination.kind.describe(),
        destination,
        has_credential,
    }
}

#[tauri::command]
#[specta::specta]
pub async fn list_destinations(state: State<'_, AppState>) -> CmdResult<Vec<DestinationView>> {
    Ok(state
        .store
        .list_destinations()
        .await?
        .into_iter()
        .map(destination_view)
        .collect())
}

/// Create a destination and file its credential.
///
/// The secret arrives here and goes straight to the keychain; it is never
/// stored in the database and never returned. This is the only direction a
/// secret is allowed to travel across this boundary.
#[tauri::command]
#[specta::specta]
pub async fn create_destination(
    state: State<'_, AppState>,
    input: DestinationCreate,
    secret_access_key: String,
) -> CmdResult<DestinationView> {
    if secret_access_key.trim().is_empty() {
        return Err(CommandError::new(
            "invalid",
            "a secret access key is required; without one the destination cannot be used",
        ));
    }

    let created = state.store.create_destination(input).await?;
    // Filed after the row exists, so a rejected configuration never leaves a
    // credential behind with nothing pointing at it.
    secrets::set_secret(
        created.id,
        SecretKind::ObjectStoreSecret,
        secret_access_key.trim(),
    )?;
    state
        .store
        .audit(
            AuditAction::DestinationCreated,
            &created.name,
            created.kind.describe(),
        )
        .await;
    Ok(destination_view(created))
}

#[tauri::command]
#[specta::specta]
pub async fn update_destination(
    state: State<'_, AppState>,
    id: Uuid,
    patch: DestinationUpdate,
) -> CmdResult<DestinationView> {
    Ok(destination_view(
        state.store.update_destination(id, patch).await?,
    ))
}

/// Replace a destination's secret access key.
#[tauri::command]
#[specta::specta]
pub async fn set_destination_credential(
    state: State<'_, AppState>,
    id: Uuid,
    secret_access_key: String,
) -> CmdResult<DestinationView> {
    let destination = state.store.require_destination(id).await?;
    if secret_access_key.trim().is_empty() {
        return Err(CommandError::new(
            "invalid",
            "an empty key would leave this destination unusable; remove it instead",
        ));
    }
    secrets::set_secret(id, SecretKind::ObjectStoreSecret, secret_access_key.trim())?;
    Ok(destination_view(destination))
}

/// Delete a destination and the credential belonging to it.
#[tauri::command]
#[specta::specta]
pub async fn delete_destination(state: State<'_, AppState>, id: Uuid) -> CmdResult<bool> {
    let name = state
        .store
        .get_destination(id)
        .await?
        .map(|d| d.name)
        .unwrap_or_else(|| id.to_string());

    let removed = ops::forget_destination(&state.store, id)
        .await
        .map_err(|e| CommandError::new("destination", e.to_string()))?;

    if removed {
        state
            .store
            .audit(
                AuditAction::DestinationDeleted,
                name,
                "off-site copies stop being made",
            )
            .await;
    }
    Ok(removed)
}

/// What a reachability check found.
///
/// A struct rather than a `Result` across the boundary, because "this one is
/// broken" is information the page shows rather than an error that stops it.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DestinationCheck {
    pub ok: bool,
    pub detail: String,
}

/// Check that a destination is reachable and its credential is accepted.
///
/// Narrow on purpose, and the UI says so: this proves the endpoint resolves,
/// the credential is valid and the bucket can be listed. It does not prove the
/// credential can *write* — only a write proves that.
#[tauri::command]
#[specta::specta]
pub async fn test_destination(state: State<'_, AppState>, id: Uuid) -> CmdResult<DestinationCheck> {
    let destination = state.store.require_destination(id).await?;
    Ok(match ops::test_destination(&destination).await {
        Ok(()) => DestinationCheck {
            ok: true,
            detail: format!(
                "{} is reachable and accepts the key",
                destination.kind.describe()
            ),
        },
        Err(e) => DestinationCheck {
            ok: false,
            detail: e.to_string(),
        },
    })
}

/// Upload an artifact that is already on disk to every enabled destination.
///
/// For backfilling artifacts taken before a destination existed, and for
/// retrying one whose upload failed. Returns a job id immediately and follows
/// the same event stream as a backup, because an upload of a large artifact is
/// exactly as long-running as the dump that produced it.
///
/// Deliberately writes no job-history row: history records what a *profile*
/// did, and a manual push has no profile behind it. Inventing one would put a
/// row in the list attributed to a connection that was not involved.
#[tauri::command]
#[specta::specta]
pub async fn push_artifact_offsite(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
) -> CmdResult<Uuid> {
    let artifact = PathBuf::from(&path);
    if !artifact.is_file() {
        return Err(CommandError::new(
            "not_found",
            format!("{path} is not a file"),
        ));
    }

    let job_id = Uuid::new_v4();
    let ctx = JobContext::with_sender(job_id, state.event_tx.clone());
    state.jobs.register(&ctx).await;

    let store = state.store.clone();
    let jobs = state.jobs.clone();

    tauri::async_runtime::spawn(async move {
        let outcome = match ops::push_offsite(&artifact, &store, &ctx).await {
            Ok(results) => {
                let failures = ops::push_failures(&results);
                if failures.is_empty() {
                    JobOutcome::Success
                } else {
                    // Already emitted per destination by `push_offsite`; this
                    // is the line that makes the job as a whole read as failed.
                    ctx.emit_error(
                        JobPhase::Done,
                        format!("{} destination(s) failed", failures.len()),
                    )
                    .await;
                    JobOutcome::Failed
                }
            }
            Err(e) => {
                ctx.emit_error(JobPhase::Done, e.to_string()).await;
                JobOutcome::Failed
            }
        };

        jobs.unregister(job_id).await;
        let _ = JobFinished {
            job_id: job_id.to_string(),
            outcome: outcome.as_str().to_string(),
        }
        .emit(&app);
    });

    Ok(job_id)
}

// ── Command-line tool ───────────────────────────────────────────────────

#[tauri::command]
#[specta::specta]
pub async fn cli_status() -> CmdResult<cli_tool::CliStatus> {
    Ok(cli_tool::status())
}

/// Link the bundled `dbsync` somewhere a terminal will find it.
///
/// Never escalates privileges. When nothing is writable the result carries the
/// exact command for the user to run instead — an app that asks for an
/// administrator password to write into a system directory is asking for more
/// trust than this feature is worth.
#[tauri::command]
#[specta::specta]
pub async fn install_cli() -> CmdResult<cli_tool::CliInstall> {
    cli_tool::install().map_err(|e| CommandError::new("cli_install", e.to_string()))
}
