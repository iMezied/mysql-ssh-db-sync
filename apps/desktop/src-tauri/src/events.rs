//! Typed event bridge between the engine and the webview.
//!
//! Using `tauri_specta::Event` rather than a bare `emit("progress-event", ..)`
//! means the payload type is generated into `bindings.ts` alongside the
//! commands, so a change to `ProgressEvent` is a frontend compile error instead
//! of a runtime surprise.

use db_sync_engine::events::ProgressEvent;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri_specta::Event;

/// One progress record from a running job.
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct JobProgress(pub ProgressEvent);

/// Emitted when a job reaches a terminal state, so the UI can refresh history
/// without polling.
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct JobFinished {
    pub job_id: String,
    pub outcome: String,
}
