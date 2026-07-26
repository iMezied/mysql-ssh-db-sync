//! Turning a finished scheduled run into something the user sees.
//!
//! The engine decides *what* to say — it builds the title and body — and this
//! decides *how*, because showing a native notification needs Tauri and the
//! engine must stay usable from `dbsync`.

use db_sync_engine::notify::RunReport;
use db_sync_engine::scheduler::SchedulerHooks;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;
use tauri_specta::Event as _;

use crate::events::ScheduledRunFinished;

pub struct DesktopHooks {
    app: AppHandle,
}

impl DesktopHooks {
    pub const fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait::async_trait]
impl SchedulerHooks for DesktopHooks {
    async fn run_finished(&self, report: &RunReport) {
        let notification = report.to_notification();

        if let Err(e) = self
            .app
            .notification()
            .builder()
            .title(&notification.title)
            .body(&notification.body)
            .show()
        {
            // Notifications can be refused by the OS — permission not granted,
            // Do Not Disturb, a locked screen. None of that is a reason to lose
            // the message, so it still reaches the log and the open window.
            tracing::warn!(
                "could not show a notification for {:?}: {e}",
                report.schedule_name
            );
        }

        tracing::info!(
            "scheduled run of {:?} finished: {:?}",
            report.schedule_name,
            report.outcome
        );

        if let Err(e) = ScheduledRunFinished(report.clone()).emit(&self.app) {
            tracing::warn!("could not tell the window about a finished run: {e}");
        }
    }
}
