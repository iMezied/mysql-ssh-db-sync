//! The steps a composite job is made of.
//!
//! A sync is a backup, then an off-site copy, then a restore, then masking,
//! then verification, then retention. Until now that structure existed only as
//! banner comments in [`crate::ops::sync`] and as a flat stream of
//! [`crate::events::ProgressEvent`]s distinguished by phase — which meant the
//! app could show a scrolling log but could not answer "which part are we on,
//! and how long did the restore take".
//!
//! # Planned first, then executed
//!
//! Every step is written down as `pending` before the first one starts. The
//! alternative — inserting a row when a step begins — cannot distinguish "step
//! 4 has not happened yet" from "step 4 never will", so a run that dies at
//! step 2 would look like a two-step run that succeeded and stopped. Planning
//! up front costs one extra write and makes the failure honest.
//!
//! # Recording never fails a job
//!
//! These rows are diagnostics. A store error while writing one is reported on
//! the event stream and otherwise swallowed: losing the record of a forty
//! minute restore is bad, and aborting the restore because the record could
//! not be written is worse.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::events::JobPhase;
use crate::job::{JobContext, StepMark};
use crate::store::Store;

/// What a step does. A closed set, so the UI can label every one it meets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum JobStepKind {
    Backup,
    Restore,
    Verify,
    Mask,
    Offsite,
    Retention,
    Drill,
    Cleanup,
}

impl JobStepKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            JobStepKind::Backup => "backup",
            JobStepKind::Restore => "restore",
            JobStepKind::Verify => "verify",
            JobStepKind::Mask => "mask",
            JobStepKind::Offsite => "offsite",
            JobStepKind::Retention => "retention",
            JobStepKind::Drill => "drill",
            JobStepKind::Cleanup => "cleanup",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "backup" => Some(JobStepKind::Backup),
            "restore" => Some(JobStepKind::Restore),
            "verify" => Some(JobStepKind::Verify),
            "mask" => Some(JobStepKind::Mask),
            "offsite" => Some(JobStepKind::Offsite),
            "retention" => Some(JobStepKind::Retention),
            "drill" => Some(JobStepKind::Drill),
            "cleanup" => Some(JobStepKind::Cleanup),
            _ => None,
        }
    }
}

/// How a step ended.
///
/// Distinct from [`crate::job::JobOutcome`] because of `Skipped`, which is the
/// whole reason these rows are planned up front: a step the run never reached
/// is not a success, not a failure, and not still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum JobStepOutcome {
    Success,
    Failed,
    Skipped,
    Cancelled,
}

impl JobStepOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            JobStepOutcome::Success => "success",
            JobStepOutcome::Failed => "failed",
            JobStepOutcome::Skipped => "skipped",
            JobStepOutcome::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "success" => Some(JobStepOutcome::Success),
            "failed" => Some(JobStepOutcome::Failed),
            "skipped" => Some(JobStepOutcome::Skipped),
            "cancelled" => Some(JobStepOutcome::Cancelled),
            _ => None,
        }
    }
}

/// What one step produced, in the few terms worth showing next to it.
///
/// Every field is optional and defaulted. It is stored in an opaque JSON
/// column, so a field added later still reads back against rows written before
/// it existed — and a blob that cannot be parsed at all degrades to this
/// default rather than making the job page fail to load.
///
/// No `skip_serializing_if`, deliberately. It would make the serialised and
/// deserialised shapes differ, and specta answers that by exporting two
/// TypeScript types with a union over them — so the page consuming this would
/// name `JobStep_Serialize`. A few null fields in a tiny blob is the cheaper
/// side of that trade.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct JobStepDetail {
    /// Path of the artifact this step wrote or read.
    #[serde(default)]
    pub artifact: Option<String>,
    /// Database this step wrote to.
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub tables_checked: Option<u32>,
    /// Why this step failed, as it was reported to the user.
    #[serde(default)]
    pub error: Option<String>,
    /// Anything else worth one line — "3 artifacts removed", "2 of 2 copies
    /// pushed", "verification found 1 mismatch".
    #[serde(default)]
    pub notes: Vec<String>,
}

impl JobStepDetail {
    pub fn artifact(path: impl Into<String>) -> Self {
        Self {
            artifact: Some(path.into()),
            ..Self::default()
        }
    }

    pub fn database(name: impl Into<String>) -> Self {
        Self {
            database: Some(name.into()),
            ..Self::default()
        }
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

/// One planned or completed step of a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct JobStep {
    pub job_id: Uuid,
    /// 1-based, because it is read as "step 2 of 5".
    pub index: u32,
    pub kind: JobStepKind,
    /// Human label decided when the step was planned, so it can name the
    /// actual database or destination rather than repeating the kind.
    pub label: String,
    /// `None` while the step is still pending.
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// `None` means pending if `started_at` is also none, running otherwise.
    pub outcome: Option<JobStepOutcome>,
    pub detail: JobStepDetail,
}

impl JobStep {
    pub fn is_running(&self) -> bool {
        self.started_at.is_some() && self.outcome.is_none()
    }
}

/// The steps a job intends to run, in order.
#[derive(Debug, Clone, Default)]
pub struct StepPlan(Vec<(JobStepKind, String)>);

impl StepPlan {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn add(mut self, kind: JobStepKind, label: impl Into<String>) -> Self {
        self.0.push((kind, label.into()));
        self
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn entries(&self) -> &[(JobStepKind, String)] {
        &self.0
    }
}

/// Walks a [`StepPlan`], stamping the context and writing each row as it goes.
///
/// Held by reference through a straight-line operation: `begin` advances to the
/// next planned step, `done` closes it. Nothing here reports failure — an
/// operation that fails returns early through `?`, and
/// [`crate::ops::record_finish`] closes whatever was left open. That is one
/// place instead of one per early return.
pub struct StepRecorder<'a> {
    store: &'a Store,
    ctx: &'a JobContext,
    plan: StepPlan,
    /// 0-based index of the step `begin` will start next.
    cursor: Mutex<usize>,
}

impl<'a> StepRecorder<'a> {
    /// Write the plan down, then hand back a recorder positioned before the
    /// first step.
    pub async fn start(store: &'a Store, ctx: &'a JobContext, plan: StepPlan) -> Self {
        let recorder = Self {
            store,
            ctx,
            plan,
            cursor: Mutex::new(0),
        };
        if let Err(e) = store
            .plan_job_steps(ctx.job_id, recorder.plan.entries())
            .await
        {
            recorder
                .warn(format!("could not record the step plan: {e}"))
                .await;
        }
        recorder
    }

    pub fn total(&self) -> u32 {
        self.plan.len() as u32
    }

    /// Start the next planned step.
    ///
    /// `kind` is what the caller believes it is about to run, and is checked
    /// against the plan: a plan and an execution order that disagree would
    /// silently mislabel every row from that point on.
    pub async fn begin(&self, kind: JobStepKind) {
        let index = {
            let mut cursor = self.cursor.lock().await;
            let index = *cursor;
            *cursor += 1;
            index
        };

        let Some((planned, _)) = self.plan.entries().get(index) else {
            debug_assert!(false, "began more steps than were planned");
            return;
        };
        debug_assert_eq!(
            *planned,
            kind,
            "step {} was planned as {planned:?} but began as {kind:?}",
            index + 1
        );

        let number = index as u32 + 1;
        self.ctx
            .enter_step(StepMark {
                index: number,
                total: self.total(),
            })
            .await;

        if let Err(e) = self.store.begin_job_step(self.ctx.job_id, number).await {
            self.warn(format!("could not record the start of step {number}: {e}"))
                .await;
        }
    }

    /// Close the step that is currently running as a success.
    pub async fn done(&self, detail: JobStepDetail) {
        self.close(JobStepOutcome::Success, detail).await;
    }

    /// Close the next planned step as skipped without running it.
    ///
    /// For a step the plan could not rule out in advance — retention that a
    /// failed verification has disqualified, a cleanup deliberately left
    /// undone.
    pub async fn skip(&self, kind: JobStepKind, reason: impl Into<String>) {
        self.begin(kind).await;
        self.close(
            JobStepOutcome::Skipped,
            JobStepDetail::default().note(reason),
        )
        .await;
    }

    async fn close(&self, outcome: JobStepOutcome, detail: JobStepDetail) {
        let number = *self.cursor.lock().await as u32;
        if number == 0 {
            debug_assert!(false, "closed a step before beginning one");
            return;
        }

        if let Err(e) = self
            .store
            .finish_job_step(self.ctx.job_id, number, outcome, &detail)
            .await
        {
            self.warn(format!("could not record the end of step {number}: {e}"))
                .await;
        }
        self.ctx.leave_step().await;
    }

    /// Reported on the stream rather than returned: see the module docs on why
    /// a diagnostic write must not be able to fail a job.
    async fn warn(&self, message: String) {
        self.ctx.emit_warn(JobPhase::Initializing, message).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_outcomes_round_trip_through_their_stored_form() {
        for kind in [
            JobStepKind::Backup,
            JobStepKind::Restore,
            JobStepKind::Verify,
            JobStepKind::Mask,
            JobStepKind::Offsite,
            JobStepKind::Retention,
            JobStepKind::Drill,
            JobStepKind::Cleanup,
        ] {
            assert_eq!(JobStepKind::parse(kind.as_str()), Some(kind));
        }
        for outcome in [
            JobStepOutcome::Success,
            JobStepOutcome::Failed,
            JobStepOutcome::Skipped,
            JobStepOutcome::Cancelled,
        ] {
            assert_eq!(JobStepOutcome::parse(outcome.as_str()), Some(outcome));
        }
    }

    #[test]
    fn an_empty_detail_round_trips() {
        let json = serde_json::to_string(&JobStepDetail::default()).unwrap();
        let back: JobStepDetail = serde_json::from_str(&json).unwrap();
        assert_eq!(back, JobStepDetail::default());
    }

    #[test]
    fn a_detail_written_before_a_field_existed_still_reads() {
        let old: JobStepDetail = serde_json::from_str(r#"{"artifact":"/tmp/a.sql.gz"}"#).unwrap();
        assert_eq!(old.artifact.as_deref(), Some("/tmp/a.sql.gz"));
        assert_eq!(old.database, None);
        assert!(old.notes.is_empty());
    }

    #[test]
    fn a_pending_step_is_not_running() {
        let mut step = JobStep {
            job_id: Uuid::nil(),
            index: 1,
            kind: JobStepKind::Backup,
            label: "Back up shop".into(),
            started_at: None,
            finished_at: None,
            outcome: None,
            detail: JobStepDetail::default(),
        };
        assert!(!step.is_running(), "planned but not started");

        step.started_at = Some(Utc::now());
        assert!(step.is_running());

        step.outcome = Some(JobStepOutcome::Success);
        assert!(!step.is_running());
    }

    #[test]
    fn a_plan_keeps_the_order_it_was_built_in() {
        let plan = StepPlan::new()
            .add(JobStepKind::Backup, "Back up shop")
            .add(JobStepKind::Restore, "Restore into shop_copy");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.entries()[0].0, JobStepKind::Backup);
        assert_eq!(plan.entries()[1].1, "Restore into shop_copy");
    }
}
