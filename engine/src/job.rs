//! Job lifecycle: context, cancellation, and the durable record.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::events::{
    EVENT_CHANNEL_CAPACITY, EventSender, JobKind, JobPhase, LogLevel, ProgressEvent,
    create_event_channel,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    Success,
    Failed,
    Cancelled,
}

impl JobOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            JobOutcome::Success => "success",
            JobOutcome::Failed => "failed",
            JobOutcome::Cancelled => "cancelled",
        }
    }
}

/// The persisted record of one job run.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct JobRecord {
    pub id: Uuid,
    pub kind: JobKind,
    pub source_profile_id: Uuid,
    pub dest_profile_id: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub outcome: Option<JobOutcome>,
    pub artifact_path: Option<String>,
    /// Serialised options, kept opaque so option shapes can evolve without a
    /// schema migration.
    pub options_json: String,
    /// Durable log. Accumulated from the event stream by [`JobContext`] — the
    /// broadcast channel is lossy and must never be the only record.
    pub log: String,
}

/// Which step of a composite job is currently running.
///
/// `index` is 1-based, because it is read by a human as "step 2 of 5".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepMark {
    pub index: u32,
    pub total: u32,
}

/// Handle passed into every long-running operation.
///
/// Cloning is cheap and shares cancellation state, so child tasks observe the
/// same token as the parent.
#[derive(Clone)]
pub struct JobContext {
    pub job_id: Uuid,
    pub event_tx: EventSender,
    cancel: CancellationToken,
    log: Arc<Mutex<String>>,
    /// Stamped onto every event emitted while it is set. Shared with clones so
    /// a child task spawned inside a step reports the same step.
    step: Arc<Mutex<Option<StepMark>>>,
    /// The most recent error message, kept so the shell can attribute a
    /// failure to the step it happened in without threading the error back
    /// through every early return.
    last_error: Arc<Mutex<Option<String>>>,
}

impl JobContext {
    pub fn new(job_id: Uuid) -> Self {
        let (tx, _rx) = create_event_channel(EVENT_CHANNEL_CAPACITY);
        Self::build(job_id, tx)
    }

    /// Build a context that publishes onto an existing fan-out channel, so the
    /// desktop app can bridge every job through one subscription.
    pub fn with_sender(job_id: Uuid, event_tx: EventSender) -> Self {
        Self::build(job_id, event_tx)
    }

    fn build(job_id: Uuid, event_tx: EventSender) -> Self {
        Self {
            job_id,
            event_tx,
            cancel: CancellationToken::new(),
            log: Arc::new(Mutex::new(String::new())),
            step: Arc::new(Mutex::new(None)),
            last_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Mark every subsequent event as belonging to this step.
    pub async fn enter_step(&self, mark: StepMark) {
        *self.step.lock().await = Some(mark);
    }

    /// Stop attributing events to a step. Work that happens between steps —
    /// setup, teardown — should not be blamed on the one that just finished.
    pub async fn leave_step(&self) {
        *self.step.lock().await = None;
    }

    pub async fn current_step(&self) -> Option<StepMark> {
        *self.step.lock().await
    }

    /// The last message passed to [`Self::emit_error`], if any.
    pub async fn last_error(&self) -> Option<String> {
        self.last_error.lock().await.clone()
    }

    pub fn subscribe(&self) -> crate::events::EventReceiver {
        self.event_tx.subscribe()
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Fail fast at a cancellation checkpoint.
    pub fn bail_if_cancelled(&self) -> anyhow::Result<()> {
        if self.is_cancelled() {
            anyhow::bail!("job cancelled");
        }
        Ok(())
    }

    pub async fn emit(&self, phase: JobPhase, message: impl Into<String>) {
        self.emit_event(ProgressEvent::new(self.job_id, phase, message))
            .await;
    }

    pub async fn emit_warn(&self, phase: JobPhase, message: impl Into<String>) {
        self.emit_event(ProgressEvent::new(self.job_id, phase, message).with_level(LogLevel::Warn))
            .await;
    }

    pub async fn emit_error(&self, phase: JobPhase, message: impl Into<String>) {
        let message = message.into();
        *self.last_error.lock().await = Some(message.clone());
        self.emit_event(
            ProgressEvent::new(self.job_id, phase, message).with_level(LogLevel::Error),
        )
        .await;
    }

    /// Append to the durable log, then publish to the lossy live channel.
    ///
    /// Order matters: a dropped broadcast message must not also lose the log
    /// line. A send error means "nobody is listening right now", which is
    /// normal and not a failure.
    pub async fn emit_event(&self, mut event: ProgressEvent) {
        if let Some(mark) = *self.step.lock().await {
            event.step = Some(mark.index);
            event.step_total = Some(mark.total);
        }
        {
            let mut log = self.log.lock().await;
            log.push_str(&event.to_log_line());
            log.push('\n');
        }
        let _ = self.event_tx.send(event);
    }

    pub async fn log_snapshot(&self) -> String {
        self.log.lock().await.clone()
    }
}

/// Tracks in-flight jobs so they can actually be cancelled.
///
/// Registering the `CancellationToken` (not the event sender) is the whole
/// point: cancellation must propagate into running child processes, not just
/// print a message.
#[derive(Clone, Default)]
pub struct JobRegistry {
    inner: Arc<Mutex<HashMap<Uuid, CancellationToken>>>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(&self, ctx: &JobContext) {
        self.inner
            .lock()
            .await
            .insert(ctx.job_id, ctx.cancel_token());
    }

    pub async fn unregister(&self, job_id: Uuid) {
        self.inner.lock().await.remove(&job_id);
    }

    /// Returns true if a job was found and signalled.
    pub async fn cancel(&self, job_id: Uuid) -> bool {
        match self.inner.lock().await.get(&job_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    pub async fn active_ids(&self) -> Vec<Uuid> {
        self.inner.lock().await.keys().copied().collect()
    }

    pub async fn cancel_all(&self) {
        for token in self.inner.lock().await.values() {
            token.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_cancels_the_context_token() {
        let registry = JobRegistry::new();
        let ctx = JobContext::new(Uuid::new_v4());
        registry.register(&ctx).await;

        assert!(!ctx.is_cancelled());
        assert!(registry.cancel(ctx.job_id).await);
        assert!(
            ctx.is_cancelled(),
            "cancellation must reach the job context"
        );
    }

    #[tokio::test]
    async fn cancelling_unknown_job_reports_not_found() {
        let registry = JobRegistry::new();
        assert!(!registry.cancel(Uuid::new_v4()).await);
    }

    #[tokio::test]
    async fn cancellation_propagates_to_clones() {
        let ctx = JobContext::new(Uuid::new_v4());
        let child = ctx.clone();
        ctx.cancel();
        assert!(child.is_cancelled());
        assert!(child.bail_if_cancelled().is_err());
    }

    #[tokio::test]
    async fn log_is_retained_even_with_no_subscribers() {
        // The live channel is lossy; the durable log must not be.
        let ctx = JobContext::new(Uuid::new_v4());
        ctx.emit(JobPhase::Initializing, "starting").await;
        ctx.emit_warn(JobPhase::DumpData, "table skipped").await;

        let log = ctx.log_snapshot().await;
        assert!(log.contains("starting"));
        assert!(log.contains("table skipped"));
        assert!(log.contains("WARN"));
    }

    #[tokio::test]
    async fn events_are_stamped_with_the_step_they_happened_in() {
        let ctx = JobContext::new(Uuid::new_v4());
        let mut rx = ctx.subscribe();

        ctx.emit(JobPhase::Initializing, "before any step").await;
        ctx.enter_step(StepMark { index: 2, total: 5 }).await;
        ctx.emit(JobPhase::Restore, "inside").await;
        ctx.leave_step().await;
        ctx.emit(JobPhase::Done, "after").await;

        let before = rx.recv().await.unwrap();
        assert_eq!(before.step, None, "work outside a step belongs to no step");

        let inside = rx.recv().await.unwrap();
        assert_eq!(inside.step, Some(2));
        assert_eq!(inside.step_total, Some(5));

        let after = rx.recv().await.unwrap();
        assert_eq!(after.step, None, "leaving must actually clear the mark");
    }

    #[tokio::test]
    async fn a_clone_reports_the_same_step() {
        // Dumps forward progress from a spawned worker holding a clone. If the
        // mark did not travel, every per-table line would lose its step.
        let ctx = JobContext::new(Uuid::new_v4());
        let child = ctx.clone();
        ctx.enter_step(StepMark { index: 1, total: 3 }).await;
        assert_eq!(
            child.current_step().await,
            Some(StepMark { index: 1, total: 3 })
        );
    }

    #[tokio::test]
    async fn the_last_error_is_remembered_for_the_step_that_failed() {
        let ctx = JobContext::new(Uuid::new_v4());
        assert_eq!(ctx.last_error().await, None);
        ctx.emit_warn(JobPhase::DumpData, "a warning is not an error")
            .await;
        assert_eq!(ctx.last_error().await, None);
        ctx.emit_error(JobPhase::Restore, "target database is not empty")
            .await;
        assert_eq!(
            ctx.last_error().await.as_deref(),
            Some("target database is not empty")
        );
    }

    #[tokio::test]
    async fn unregister_removes_the_job() {
        let registry = JobRegistry::new();
        let ctx = JobContext::new(Uuid::new_v4());
        registry.register(&ctx).await;
        registry.unregister(ctx.job_id).await;
        assert!(registry.active_ids().await.is_empty());
        assert!(!registry.cancel(ctx.job_id).await);
    }
}
