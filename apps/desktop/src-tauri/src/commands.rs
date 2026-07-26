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

use db_sync_engine::connect::{self, ConnectionReport};
use db_sync_engine::db::{DatabaseInfo, TableInfo};
use db_sync_engine::job::JobRecord;
use db_sync_engine::profile::{ConnectionProfile, ProfileCreate, ProfileUpdate};
use db_sync_engine::secrets::{self, SecretKind};
use db_sync_engine::store::StoreError;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;
use uuid::Uuid;

use crate::AppState;

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
            StoreError::ProfileNotFound(_) => "not_found",
            StoreError::Corrupt { .. } => "corrupt",
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
    pub has_ssh_passphrase: bool,
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

    Ok(profile)
}

#[tauri::command]
#[specta::specta]
pub async fn update_profile(
    state: State<'_, AppState>,
    id: Uuid,
    patch: ProfileUpdate,
) -> CmdResult<ConnectionProfile> {
    Ok(state.store.update_profile(id, patch).await?)
}

/// Delete a profile and purge every secret belonging to it.
#[tauri::command]
#[specta::specta]
pub async fn delete_profile(state: State<'_, AppState>, id: Uuid) -> CmdResult<bool> {
    let removed = state.store.delete_profile(id).await?;
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
        "ssh_passphrase" => SecretKind::SshKeyPassphrase,
        other => {
            return Err(CommandError::new(
                "invalid",
                format!("unknown secret kind {other:?}"),
            ));
        }
    };

    secrets::set_secret(id, kind, &value)?;
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
        has_ssh_passphrase: secrets::has_secret(id, SecretKind::SshKeyPassphrase)?,
    })
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

#[tauri::command]
#[specta::specta]
pub async fn app_info(state: State<'_, AppState>) -> CmdResult<AppInfo> {
    Ok(AppInfo {
        engine_version: db_sync_engine::ENGINE_VERSION.to_string(),
        store_path: state.store_path.display().to_string(),
    })
}
