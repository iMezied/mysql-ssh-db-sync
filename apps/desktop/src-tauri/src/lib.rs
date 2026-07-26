//! Desktop shell.
//!
//! Owns the Tauri runtime, the shared application state, and the bridge that
//! forwards engine progress events into the webview. All domain logic lives in
//! `db_sync_engine`.

use std::path::PathBuf;

use db_sync_engine::events::{EVENT_CHANNEL_CAPACITY, EventSender, create_event_channel};
use db_sync_engine::job::JobRegistry;
use db_sync_engine::store::Store;
use tauri::Manager;
use tauri_specta::{Event as _, collect_commands, collect_events};

mod commands;
mod events;

use events::{JobFinished, JobProgress};

pub struct AppState {
    pub store: Store,
    pub store_path: PathBuf,
    pub jobs: JobRegistry,
    /// Fan-out channel every job publishes onto.
    pub event_tx: EventSender,
}

fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            commands::list_profiles,
            commands::get_profile,
            commands::create_profile,
            commands::update_profile,
            commands::delete_profile,
            commands::set_profile_secret,
            commands::profile_secret_status,
            commands::test_connection,
            commands::trust_host_key,
            commands::list_databases,
            commands::list_tables,
            commands::start_backup,
            commands::start_sync,
            commands::list_sync_plans,
            commands::create_sync_plan,
            commands::update_sync_plan,
            commands::delete_sync_plan,
            commands::import_tables_conf,
            commands::start_restore,
            commands::backup_directory,
            commands::list_artifacts,
            commands::check_artifact,
            commands::delete_artifact,
            commands::list_jobs,
            commands::cancel_job,
            commands::active_job_ids,
            commands::app_info,
        ])
        .events(collect_events![JobProgress, JobFinished])
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let builder = specta_builder();

    // Regenerate bindings on every debug run so the TypeScript can never drift
    // from the Rust command signatures.
    #[cfg(debug_assertions)]
    builder
        .export(
            specta_typescript::Typescript::default(),
            "../src/bindings.ts",
        )
        .expect("failed to export TypeScript bindings");

    tauri::Builder::default()
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            // Registers the typed event channels. Without this, `JobProgress`
            // listeners on the frontend never fire.
            builder.mount_events(app);

            // Resolved by the engine, not by `app.path().app_data_dir()`, so
            // the GUI and `dbsync` are guaranteed to open the same file. They
            // agree today, but two independent derivations would be free to
            // drift apart on any platform or Tauri upgrade — and the symptom
            // ("the CLI can't see my connections") is far from the cause.
            let store_path = db_sync_engine::paths::default_store_path()?;
            if let Some(parent) = store_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            // Use Tauri's runtime rather than constructing our own. A local
            // `tokio::runtime::Runtime` would be dropped at the end of setup,
            // taking the connection pool's reactor with it and breaking every
            // later query.
            let store = tauri::async_runtime::block_on(Store::open(&store_path))?;

            let (event_tx, _rx) = create_event_channel(EVENT_CHANNEL_CAPACITY);

            app.manage(AppState {
                store,
                store_path,
                jobs: JobRegistry::new(),
                event_tx: event_tx.clone(),
            });

            let handle = app.handle().clone();

            // Prove the connection pool still works once `setup` has returned.
            //
            // Not ceremony: building a local tokio runtime here and dropping it
            // at the end of setup leaves the pool bound to a dead reactor, and
            // the app still *starts* perfectly — the failure only appears on
            // the first query the user triggers. This turns that class of bug
            // into a log line at startup instead of a mystery in the field.
            let health_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                let state = health_handle.state::<AppState>();
                match state.store.list_profiles().await {
                    Ok(profiles) => tracing::info!(
                        "store ready after setup: {} profile(s) at {}",
                        profiles.len(),
                        state.store_path.display()
                    ),
                    Err(e) => tracing::error!("store is unusable after setup: {e}"),
                }
            });

            // Bridge engine events to the webview. Must use Tauri's spawn:
            // a bare `tokio::spawn` here has no reactor and panics.
            tauri::async_runtime::spawn(async move {
                let mut rx = event_tx.subscribe();
                loop {
                    match rx.recv().await {
                        Ok(event) => {
                            if let Err(e) = JobProgress(event).emit(&handle) {
                                tracing::warn!("failed to emit progress event: {e}");
                            }
                        }
                        // The channel is lossy by design. Dropping messages
                        // must not tear down the bridge — that would freeze
                        // the UI for the rest of the session.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(
                                "progress consumer lagged; {skipped} events dropped from the live \
                                 view (the job log is unaffected)"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tracing::debug!("event bridge closed");
                            break;
                        }
                    }
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running DBSync Studio");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regenerate `bindings.ts`.
    ///
    /// Deliberately a test rather than only a side effect of `run()`: CI
    /// typechecks the frontend without launching the app, and a stale or
    /// missing bindings file would otherwise pass unnoticed.
    #[test]
    fn export_typescript_bindings() {
        specta_builder()
            .export(
                specta_typescript::Typescript::default(),
                "../src/bindings.ts",
            )
            .expect("bindings export must succeed");
    }
}
