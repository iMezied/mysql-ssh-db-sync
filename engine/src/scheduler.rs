//! The scheduler: what actually runs a [`Schedule`] when its time comes.
//!
//! It lives in the engine, not in the desktop app, so `dbsync` can run the
//! identical code path — the app's scheduler and a systemd timer calling the
//! CLI produce the same job, the same history row and the same webhook.
//!
//! # It does not reimplement anything
//!
//! A scheduled run goes through [`ops::sync`] or [`ops::backup`], the same
//! functions the buttons in the UI call. There is deliberately no "scheduled
//! backup" code path that could drift from the interactive one; the only thing
//! this module adds is deciding *when*, resolving the profiles, and reporting
//! afterwards.
//!
//! # Failing loudly
//!
//! A schedule outlives the things it points at. Plans get deleted, destination
//! profiles get removed, table selections go stale. Every one of those is
//! reported as a failed run with an explicit message and a notification — never
//! as a silent skip, because a backup that silently stops happening is
//! indistinguishable from one that is working right up until it is needed.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::events::{EventSender, JobKind, JobPhase};
use crate::job::{JobContext, JobOutcome, JobRegistry};
use crate::notify::{self, RunOutcome, RunReport};
use crate::ops;
use crate::schedule::{DEFAULT_GRACE, Schedule};
use crate::store::Store;

/// How often the scheduler asks "is anything due?".
///
/// Cron's resolution is one minute, so this only has to be comfortably under
/// that. Thirty seconds means a schedule is picked up within its own minute
/// even if a tick lands late, which is what
/// [`DEFAULT_GRACE`](crate::schedule::DEFAULT_GRACE) is sized against.
pub const DEFAULT_TICK: Duration = Duration::from_secs(30);

/// Somewhere for the host application to react to a finished run.
///
/// This is how a native notification gets shown without the engine depending on
/// Tauri. The CLI supplies [`NoHooks`].
#[async_trait::async_trait]
pub trait SchedulerHooks: Send + Sync + 'static {
    async fn run_finished(&self, report: &RunReport);
}

/// Hooks that do nothing, for headless use.
pub struct NoHooks;

#[async_trait::async_trait]
impl SchedulerHooks for NoHooks {
    async fn run_finished(&self, _report: &RunReport) {}
}

#[derive(Clone)]
pub struct Scheduler {
    store: Store,
    jobs: JobRegistry,
    event_tx: EventSender,
    hooks: Arc<dyn SchedulerHooks>,
    /// Schedules with a run in flight.
    ///
    /// A backup that takes longer than its own interval must not start a second
    /// copy of itself: two `mysqldump`s against the same source, writing to the
    /// same directory, is how a backup set gets corrupted.
    in_flight: Arc<Mutex<HashSet<Uuid>>>,
    tick: Duration,
}

impl Scheduler {
    pub fn new(store: Store, jobs: JobRegistry, event_tx: EventSender) -> Self {
        Self {
            store,
            jobs,
            event_tx,
            hooks: Arc::new(NoHooks),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            tick: DEFAULT_TICK,
        }
    }

    pub fn with_hooks(mut self, hooks: Arc<dyn SchedulerHooks>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Override the tick interval. Tests use this; nothing else should.
    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// Run until `shutdown` is cancelled.
    ///
    /// The token is a parameter rather than a field because a cancelled token
    /// stays cancelled: the desktop app needs to stop and start this loop as
    /// the user toggles the scheduler, and that means a fresh token each time
    /// rather than a stale one that makes the loop exit immediately.
    pub async fn run(self, shutdown: CancellationToken) {
        tracing::info!(
            "scheduler started, checking every {} seconds",
            self.tick.as_secs()
        );

        let mut ticker = tokio::time::interval(self.tick);
        // A machine waking from sleep has a pile of missed ticks. Firing them
        // all back to back would ask the same question several times in a few
        // milliseconds; one catch-up tick answers it just as well.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("scheduler stopping");
                    break;
                }
                _ = ticker.tick() => {
                    self.tick_once(Utc::now()).await;
                }
            }
        }
    }

    /// One pass over the enabled schedules. Public so tests can drive time.
    pub async fn tick_once(&self, now: DateTime<Utc>) {
        let schedules = match self.store.list_enabled_schedules().await {
            Ok(s) => s,
            Err(e) => {
                // Losing the store is not a reason to stop scheduling; it may
                // come back, and the alternative is silently never running
                // again for the rest of the session.
                tracing::error!("scheduler could not read schedules: {e}");
                return;
            }
        };

        for schedule in schedules {
            let Some(due) = schedule.due_at(now, DEFAULT_GRACE) else {
                continue;
            };

            if !self.claim(schedule.id).await {
                tracing::warn!(
                    "schedule {:?} is still running from a previous occurrence; skipping the run \
                     due at {due}",
                    schedule.name
                );
                continue;
            }

            self.spawn_run(schedule, Some(due));
        }
    }

    /// Start a schedule immediately, outside its cron timing.
    ///
    /// Returns the job id, or `None` if a run is already in flight.
    ///
    /// A manual run deliberately does *not* move the schedule's high-water
    /// mark: testing a schedule at 14:00 must not cancel the occurrence it was
    /// created for, nor consume a pending catch-up.
    pub async fn run_now(
        &self,
        schedule_id: Uuid,
    ) -> Result<Option<Uuid>, crate::store::StoreError> {
        let schedule = self.store.require_schedule(schedule_id).await?;

        if !self.claim(schedule.id).await {
            return Ok(None);
        }

        Ok(Some(self.spawn_run(schedule, None)))
    }

    /// Reserve a schedule, returning false if it was already reserved.
    async fn claim(&self, id: Uuid) -> bool {
        self.in_flight.lock().await.insert(id)
    }

    async fn release(&self, id: Uuid) {
        self.in_flight.lock().await.remove(&id);
    }

    pub async fn in_flight_ids(&self) -> Vec<Uuid> {
        self.in_flight.lock().await.iter().copied().collect()
    }

    /// Launch a run in the background, returning its job id.
    fn spawn_run(&self, schedule: Schedule, occurrence: Option<DateTime<Utc>>) -> Uuid {
        let job_id = Uuid::new_v4();
        let this = self.clone();

        tokio::spawn(async move {
            let schedule_id = schedule.id;
            this.execute(schedule, occurrence, job_id).await;
            this.release(schedule_id).await;
        });

        job_id
    }

    async fn execute(&self, schedule: Schedule, occurrence: Option<DateTime<Utc>>, job_id: Uuid) {
        let started_at = Utc::now();
        // The occurrence, not `now`: a run that started 40 seconds late must
        // not leave a high-water mark 40 seconds past its own occurrence.
        let scheduled_for = occurrence.unwrap_or(started_at);

        // Stamped before the work begins, so a crash mid-run cannot cause the
        // same occurrence to be attempted again on the next start.
        if let Some(due) = occurrence
            && let Err(e) = self
                .store
                .mark_schedule_started(schedule.id, due, job_id)
                .await
        {
            tracing::error!("could not record the start of {:?}: {e}", schedule.name);
        }

        let ctx = JobContext::with_sender(job_id, self.event_tx.clone());
        self.jobs.register(&ctx).await;

        let result = self.perform(&schedule, &ctx, job_id).await;
        let finished_at = Utc::now();

        let (outcome, artifact, target_db, verification, removed, error) = match result {
            Ok(run) => (
                run.outcome,
                run.artifact,
                run.target_database,
                run.verification,
                run.removed_artifacts,
                run.error,
            ),
            Err(message) => {
                ctx.emit_error(JobPhase::Done, &message).await;
                let outcome = if ctx.is_cancelled() {
                    JobOutcome::Cancelled
                } else {
                    JobOutcome::Failed
                };
                (outcome, None, None, None, 0, Some(message))
            }
        };

        let report = RunReport::new(RunOutcome {
            schedule_id: schedule.id,
            schedule_name: &schedule.name,
            job_id,
            kind: if schedule.is_sync() {
                JobKind::Sync
            } else {
                JobKind::Backup
            },
            outcome,
            scheduled_for,
            started_at,
            finished_at,
            source_profile: &self.source_name(&schedule).await,
            dest_profile: self.dest_name(&schedule).await.as_deref(),
            database: &self.database_name(&schedule).await,
            target_database: target_db.as_deref(),
            artifact_path: artifact.as_deref(),
            verification: verification.as_ref(),
            removed_artifacts: removed,
            error,
        });

        // Before the log is snapshotted, so whether the webhook landed is part
        // of the job's durable record rather than only a tracing line.
        if let Some(url) = &schedule.webhook_url {
            self.deliver_webhook(url, &report, &ctx).await;
        }

        let _ = ops::record_finish(
            &self.store,
            &ctx,
            outcome,
            artifact.as_ref().map(|p| p.display().to_string()),
        )
        .await;

        if let Err(e) = self
            .store
            .mark_schedule_finished(schedule.id, outcome)
            .await
        {
            tracing::error!("could not record the outcome of {:?}: {e}", schedule.name);
        }

        self.jobs.unregister(job_id).await;

        if schedule.notify.wants(outcome) {
            self.hooks.run_finished(&report).await;
        }
    }

    async fn deliver_webhook(&self, url: &str, report: &RunReport, ctx: &JobContext) {
        match notify::post_webhook(url, report).await {
            Ok(status) => {
                ctx.emit(JobPhase::Done, format!("webhook accepted ({status})"))
                    .await;
            }
            Err(e) => {
                // A failed webhook never fails the run. The backup either
                // happened or it did not, and that is already recorded; losing
                // the courtesy copy must not turn a good backup into a bad one.
                ctx.emit_warn(JobPhase::Done, format!("webhook delivery failed: {e}"))
                    .await;
            }
        }
    }

    /// The work itself. Errors are returned as messages fit for a user.
    async fn perform(
        &self,
        schedule: &Schedule,
        ctx: &JobContext,
        job_id: Uuid,
    ) -> Result<RunResult, String> {
        ctx.emit(
            JobPhase::Initializing,
            format!("scheduled run of {:?}", schedule.name),
        )
        .await;

        // Every one of these lookups can fail because something the schedule
        // points at was deleted. Each gets its own message naming what is
        // missing — "not found" alone would send the user hunting.
        let plan = self
            .store
            .get_sync_plan(schedule.plan_id)
            .await
            .map_err(|e| format!("could not read the sync plan: {e}"))?
            .ok_or_else(|| {
                format!(
                    "the sync plan this schedule uses no longer exists; re-point or delete the \
                     schedule {:?}",
                    schedule.name
                )
            })?;

        let source = self
            .store
            .get_profile(plan.profile_id)
            .await
            .map_err(|e| format!("could not read the source profile: {e}"))?
            .ok_or_else(|| {
                format!(
                    "the source profile for plan {:?} no longer exists; this schedule cannot run",
                    plan.name
                )
            })?;

        let request = schedule.backup_request(&plan);
        request
            .validate(&source)
            .map_err(|e| format!("this schedule's options are not valid: {e}"))?;

        // Drift is reported, not fatal: a table dropped last week should not
        // stop tonight's backup of everything else — but it must be visible.
        let available = plan
            .selections
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>();
        let missing = plan.missing_from(&available);
        if !missing.is_empty() {
            ctx.emit_warn(
                JobPhase::Introspect,
                format!(
                    "the plan lists tables that are no longer selected: {}",
                    missing.join(", ")
                ),
            )
            .await;
        }

        match schedule.dest_profile_id {
            None => {
                ops::record_start(
                    &self.store,
                    ctx,
                    JobKind::Backup,
                    source.id,
                    None,
                    serde_json::to_string(&request).unwrap_or_else(|_| "{}".into()),
                )
                .await
                .map_err(|e| format!("could not record the job: {e}"))?;

                let artifact = ops::backup(&source, &request, &self.store, ctx)
                    .await
                    .map_err(|e| e.to_string())?;

                let removed = match schedule.action.retention {
                    Some(policy) => {
                        ops::apply_retention(&schedule.action.output_dir, policy, ctx).await
                    }
                    None => Vec::new(),
                };

                Ok(RunResult {
                    outcome: JobOutcome::Success,
                    artifact: Some(artifact),
                    target_database: None,
                    verification: None,
                    removed_artifacts: removed.len(),
                    error: None,
                })
            }

            Some(dest_id) => {
                let dest = self
                    .store
                    .get_profile(dest_id)
                    .await
                    .map_err(|e| format!("could not read the destination profile: {e}"))?
                    // The documented consequence of not making this a foreign
                    // key: the schedule survives, and says so, loudly.
                    .ok_or_else(|| {
                        format!(
                            "the destination profile for schedule {:?} has been deleted; nothing \
                             was backed up or restored",
                            schedule.name
                        )
                    })?;

                let sync_request = schedule
                    .sync_request(&plan)
                    .ok_or("this schedule has a destination but no restore options")?;

                ops::record_start(
                    &self.store,
                    ctx,
                    JobKind::Sync,
                    source.id,
                    Some(dest.id),
                    serde_json::to_string(&sync_request).unwrap_or_else(|_| "{}".into()),
                )
                .await
                .map_err(|e| format!("could not record the job: {e}"))?;

                let out = ops::sync(&source, &dest, &sync_request, &self.store, ctx)
                    .await
                    .map_err(|e| e.to_string())?;

                // Every step can exit zero while the data is wrong. The
                // question job history has to answer is "did it work".
                let verified = out.verification.as_ref().is_none_or(|r| r.passed());
                if !verified {
                    ctx.emit_error(
                        JobPhase::Done,
                        "the data was restored but verification found discrepancies",
                    )
                    .await;
                }

                Ok(RunResult {
                    outcome: if verified {
                        JobOutcome::Success
                    } else {
                        JobOutcome::Failed
                    },
                    artifact: Some(out.artifact_path.clone().into()),
                    target_database: Some(out.target_database.clone()),
                    verification: out.verification.clone(),
                    removed_artifacts: out.removed_artifacts.len(),
                    error: (!verified).then(|| "verification found discrepancies".to_string()),
                })
            }
        }
        .inspect(|_| tracing::debug!("scheduled job {job_id} finished"))
    }

    // Names for the report. A missing profile is not an error here — the run
    // has already failed for that reason and been reported; this is only the
    // label on the notification.

    async fn source_name(&self, schedule: &Schedule) -> String {
        let Ok(Some(plan)) = self.store.get_sync_plan(schedule.plan_id).await else {
            return "(deleted plan)".into();
        };
        match self.store.get_profile(plan.profile_id).await {
            Ok(Some(p)) => p.name,
            _ => "(deleted profile)".into(),
        }
    }

    async fn dest_name(&self, schedule: &Schedule) -> Option<String> {
        let id = schedule.dest_profile_id?;
        Some(match self.store.get_profile(id).await {
            Ok(Some(p)) => p.name,
            _ => "(deleted profile)".into(),
        })
    }

    async fn database_name(&self, schedule: &Schedule) -> String {
        match self.store.get_sync_plan(schedule.plan_id).await {
            Ok(Some(plan)) => plan.database,
            _ => "(unknown)".into(),
        }
    }
}

struct RunResult {
    outcome: JobOutcome,
    artifact: Option<std::path::PathBuf>,
    target_database: Option<String>,
    verification: Option<crate::verify::VerificationReport>,
    removed_artifacts: usize,
    error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EVENT_CHANNEL_CAPACITY, create_event_channel};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingHooks {
        seen: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl SchedulerHooks for CountingHooks {
        async fn run_finished(&self, _report: &RunReport) {
            self.seen.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn scheduler() -> Scheduler {
        let store = Store::open(":memory:").await.unwrap();
        let (tx, _rx) = create_event_channel(EVENT_CHANNEL_CAPACITY);
        Scheduler::new(store, JobRegistry::new(), tx)
    }

    #[tokio::test]
    async fn a_schedule_can_only_be_claimed_once() {
        // The guard against a slow backup starting a second copy of itself.
        let s = scheduler().await;
        let id = Uuid::new_v4();

        assert!(s.claim(id).await);
        assert!(!s.claim(id).await, "a second claim must be refused");

        s.release(id).await;
        assert!(s.claim(id).await, "and allowed again once released");
    }

    #[tokio::test]
    async fn claims_are_per_schedule() {
        let s = scheduler().await;
        assert!(s.claim(Uuid::new_v4()).await);
        assert!(s.claim(Uuid::new_v4()).await);
        assert_eq!(s.in_flight_ids().await.len(), 2);
    }

    #[tokio::test]
    async fn a_tick_with_no_schedules_does_nothing() {
        let s = scheduler().await;
        s.tick_once(Utc::now()).await;
        assert!(s.in_flight_ids().await.is_empty());
    }

    #[tokio::test]
    async fn run_now_on_an_unknown_schedule_reports_not_found() {
        let s = scheduler().await;
        match s.run_now(Uuid::new_v4()).await {
            Err(crate::store::StoreError::ScheduleNotFound(_)) => {}
            other => panic!("expected ScheduleNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_loop_stops_when_the_shutdown_token_fires() {
        // Without this the desktop app cannot quit: the runtime would wait on
        // a task that never returns.
        let s = scheduler().await.with_tick(Duration::from_millis(20));
        let token = CancellationToken::new();

        let handle = tokio::spawn(s.run(token.clone()));
        tokio::time::sleep(Duration::from_millis(60)).await;
        token.cancel();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("the scheduler must stop promptly")
            .unwrap();
    }

    #[tokio::test]
    async fn a_restarted_loop_is_not_still_cancelled() {
        // The reason the token is a parameter: stopping and restarting the
        // scheduler from the settings toggle must actually restart it, not
        // spawn a loop that exits on its first poll.
        let s = scheduler().await.with_tick(Duration::from_millis(20));

        let first = CancellationToken::new();
        let handle = tokio::spawn(s.clone().run(first.clone()));
        first.cancel();
        handle.await.unwrap();

        let second = CancellationToken::new();
        let handle = tokio::spawn(s.run(second.clone()));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !handle.is_finished(),
            "the second loop must still be running"
        );

        second.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn hooks_are_optional() {
        let seen = Arc::new(AtomicUsize::new(0));
        let s = scheduler()
            .await
            .with_hooks(Arc::new(CountingHooks { seen: seen.clone() }));
        // Nothing due, so nothing reported.
        s.tick_once(Utc::now()).await;
        assert_eq!(seen.load(Ordering::SeqCst), 0);
    }
}
