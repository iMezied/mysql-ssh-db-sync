//! Desktop shell.
//!
//! Owns the Tauri runtime, the shared application state, and the bridge that
//! forwards engine progress events into the webview. All domain logic lives in
//! `db_sync_engine`.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use db_sync_engine::events::{EVENT_CHANNEL_CAPACITY, EventSender, create_event_channel};
use db_sync_engine::job::JobRegistry;
use db_sync_engine::scheduler::Scheduler;
use db_sync_engine::settings;
use db_sync_engine::store::Store;
use tauri::{Manager, RunEvent, WindowEvent};
use tauri_specta::{Event as _, collect_commands, collect_events};
use tokio_util::sync::CancellationToken;

mod cli_tool;
// Public so the IPC tests can register the handlers through
// `generate_handler!`. Nothing outside this crate calls them directly.
pub mod commands;
mod events;
mod hooks;
mod tray;

use events::{JobFinished, JobProgress, NavigateTo, ScheduledRunFinished};

pub struct AppState {
    pub store: Store,
    pub store_path: PathBuf,
    pub jobs: JobRegistry,
    /// Fan-out channel every job publishes onto.
    pub event_tx: EventSender,
    pub scheduler: Scheduler,
    /// Cancels the running scheduler loop, if one is running.
    ///
    /// A `std::sync::Mutex` rather than tokio's: it is only ever held for the
    /// length of a swap, never across an await, and a synchronous lock can be
    /// taken from the Tauri run-event handler where there is no async context.
    pub scheduler_loop: Mutex<Option<CancellationToken>>,
    /// Set when the user picks Quit, so closing the window and quitting the
    /// app stay distinguishable.
    pub quitting: AtomicBool,
    /// Cached copies of the two settings the window-close handler needs.
    ///
    /// That handler runs on the main thread, and reading them from SQLite there
    /// would mean blocking the UI on a connection pool that a running backup
    /// may be holding. Closing a window must never be able to hang behind a
    /// dump, so the values are mirrored here and updated when they change.
    pub close_to_tray: AtomicBool,
    pub background_notice_shown: AtomicBool,
}

impl AppState {
    /// Start the scheduler loop, replacing any loop already running.
    pub fn start_scheduler(&self) {
        let token = CancellationToken::new();

        if let Some(previous) = self
            .scheduler_loop
            .lock()
            .expect("scheduler lock poisoned")
            .replace(token.clone())
        {
            previous.cancel();
        }

        let scheduler = self.scheduler.clone();
        tauri::async_runtime::spawn(scheduler.run(token));
    }

    /// Stop the scheduler loop. In-flight jobs are left to finish.
    pub fn stop_scheduler(&self) {
        if let Some(token) = self
            .scheduler_loop
            .lock()
            .expect("scheduler lock poisoned")
            .take()
        {
            token.cancel();
        }
    }

    pub fn scheduler_running(&self) -> bool {
        self.scheduler_loop
            .lock()
            .expect("scheduler lock poisoned")
            .is_some()
    }
}

/// Mark the app as genuinely quitting, then exit.
///
/// Without the flag, the exit handler cannot tell "the user chose Quit" from
/// "the user closed the window", and one of the two would be wrong.
pub fn request_quit(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        state.quitting.store(true, Ordering::SeqCst);
        state.stop_scheduler();
    }
    app.exit(0);
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
            commands::list_ssh_connections,
            commands::create_ssh_connection,
            commands::update_ssh_connection,
            commands::delete_ssh_connection,
            commands::set_ssh_connection_passphrase,
            commands::ssh_connection_status,
            commands::test_ssh_connection,
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
            commands::set_sync_plan_masking,
            commands::masking_preview,
            commands::backup_key_status,
            commands::generate_backup_key,
            commands::set_backup_key_recipients,
            commands::export_backup_key_to_file,
            commands::start_restore,
            commands::backup_directory,
            commands::list_artifacts,
            commands::check_artifact,
            commands::delete_artifact,
            commands::list_jobs,
            commands::cancel_job,
            commands::active_job_ids,
            commands::app_info,
            commands::list_schedules,
            commands::get_schedule,
            commands::create_schedule,
            commands::update_schedule,
            commands::delete_schedule,
            commands::run_schedule_now,
            commands::preview_cron,
            commands::crontab_line,
            commands::scheduler_status,
            commands::get_app_settings,
            commands::set_app_settings,
            commands::list_audit,
            commands::library_stats,
            commands::export_config_to_file,
            commands::preview_config_import,
            commands::import_config,
            commands::list_destinations,
            commands::create_destination,
            commands::update_destination,
            commands::set_destination_credential,
            commands::delete_destination,
            commands::test_destination,
            commands::push_artifact_offsite,
            commands::cli_status,
            commands::install_cli,
        ])
        .events(collect_events![
            JobProgress,
            JobFinished,
            ScheduledRunFinished,
            NavigateTo
        ])
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

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        // LaunchAgent rather than a login item: it survives an app move and can
        // be inspected and removed by the user without going through us.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
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

            // Turn any SSH config left inline on a profile by an older version
            // into a saved connection. A no-op after the first run, and the
            // CLI does the same thing on its own start — whichever of the two
            // the user happens to open first performs the upgrade.
            match tauri::async_runtime::block_on(db_sync_engine::sshconn::adopt_legacy_configs(
                &store,
            )) {
                Ok(adopted) if !adopted.is_empty() => {
                    tracing::info!(
                        "adopted {} inline SSH config(s) into saved connections: {}",
                        adopted.len(),
                        adopted
                            .iter()
                            .map(|a| format!("{} -> {}", a.profile_name, a.ssh_connection_name))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                Ok(_) => {}
                // Not fatal: the profiles still hold their original config, so
                // the next start can try again. Refusing to launch over it
                // would leave the user with no way to reach the data.
                Err(e) => tracing::error!("could not adopt inline SSH configurations: {e}"),
            }

            let (event_tx, _rx) = create_event_channel(EVENT_CHANNEL_CAPACITY);
            let handle = app.handle().clone();

            let scheduler = Scheduler::new(store.clone(), JobRegistry::new(), event_tx.clone())
                .with_hooks(std::sync::Arc::new(hooks::DesktopHooks::new(
                    handle.clone(),
                )));

            let stored = tauri::async_runtime::block_on(store.app_settings())?;

            app.manage(AppState {
                store,
                store_path,
                jobs: JobRegistry::new(),
                event_tx: event_tx.clone(),
                scheduler,
                scheduler_loop: Mutex::new(None),
                quitting: AtomicBool::new(false),
                close_to_tray: AtomicBool::new(stored.close_to_tray),
                background_notice_shown: AtomicBool::new(stored.background_notice_shown),
            });

            tray::build(&handle)?;

            if stored.scheduler_enabled {
                app.state::<AppState>().start_scheduler();
            } else {
                tracing::info!("the in-app scheduler is turned off; no schedules will run here");
            }

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
            let bridge_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                let mut rx = event_tx.subscribe();
                loop {
                    match rx.recv().await {
                        Ok(event) => {
                            if let Err(e) = JobProgress(event).emit(&bridge_handle) {
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
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let Some(state) = app.try_state::<AppState>() else {
                    return;
                };

                if state.quitting.load(Ordering::SeqCst)
                    || !state.close_to_tray.load(Ordering::SeqCst)
                {
                    return;
                }

                api.prevent_close();
                let _ = window.hide();
                announce_background_mode(app);
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building DBSync Studio");

    app.run(|app, event| {
        if let RunEvent::ExitRequested { api, .. } = event {
            // On macOS the app should outlive its window, because that is the
            // whole point of running schedules in the background. Without this
            // the process exits the moment the last window closes and every
            // schedule stops with it.
            if let Some(state) = app.try_state::<AppState>()
                && !state.quitting.load(Ordering::SeqCst)
            {
                api.prevent_exit();
            }
        }
    });
}

/// Tell the user once that closing the window did not stop anything.
///
/// A window that vanishes with no explanation reads as a crash, and the user
/// relaunches — or worse, assumes their backups stopped and stops trusting
/// them. Saying it every time would be nagging, so it is said once.
fn announce_background_mode(app: &tauri::AppHandle) {
    use tauri_plugin_notification::NotificationExt;

    let Some(state) = app.try_state::<AppState>() else {
        return;
    };

    // `swap` rather than load-then-store: closing two windows in quick
    // succession must not show the notice twice.
    if state.background_notice_shown.swap(true, Ordering::SeqCst) {
        return;
    }

    let _ = app
        .notification()
        .builder()
        .title("DBSync Studio is still running")
        .body("Schedules keep running in the background. Quit from the menu bar to stop them.")
        .show();

    let store = state.store.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = store
            .set_flag(settings::BACKGROUND_NOTICE_SHOWN, true)
            .await
        {
            tracing::warn!("could not record that the background notice was shown: {e}");
        }
    });
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
