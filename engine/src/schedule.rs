//! Scheduled, unattended runs.
//!
//! A schedule binds a [`SyncPlan`](crate::plan::SyncPlan) to a cron expression.
//! The plan already carries the source profile, the database and the table
//! selection, so a schedule only has to add *when*, *where to*, and *who to
//! tell*.
//!
//! # Unattended means no confirmation is possible
//!
//! Every destructive path in this application is gated on the user typing the
//! name of the thing being destroyed. Nobody is at the keyboard at 03:00, so a
//! schedule cannot carry that confirmation — and a schedule that could drop a
//! database would be a standing instruction to destroy production on a timer.
//! [`Schedule::validate`] therefore rejects a destructive naming strategy
//! outright, and [`Schedule::sync_request`] never populates the confirmation
//! field. The restore layer would refuse it anyway; this makes it impossible to
//! even ask.

use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

use crate::backup::{BackupRequest, CommonBackupOptions, EngineBackupOptions};
use crate::cron::{CronExpression, ScheduleTimezone};
use crate::job::JobOutcome;
use crate::ops::SyncRequest;
use crate::plan::SyncPlan;
use crate::restore::{EngineRestoreOptions, TargetNaming};
use crate::retention::RetentionPolicy;

/// When to raise a notification for a scheduled run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum NotifyPolicy {
    Never,
    /// The default. A nightly backup that worked is not news; one that failed
    /// is the only thing the user needs to see.
    #[default]
    OnFailure,
    Always,
}

impl NotifyPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            NotifyPolicy::Never => "never",
            NotifyPolicy::OnFailure => "on_failure",
            NotifyPolicy::Always => "always",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "never" => Some(NotifyPolicy::Never),
            "on_failure" => Some(NotifyPolicy::OnFailure),
            "always" => Some(NotifyPolicy::Always),
            _ => None,
        }
    }

    pub const fn wants(self, outcome: JobOutcome) -> bool {
        match self {
            NotifyPolicy::Never => false,
            NotifyPolicy::Always => true,
            NotifyPolicy::OnFailure => !matches!(outcome, JobOutcome::Success),
        }
    }
}

/// What a schedule does when it fires.
///
/// The two kinds answer different questions. A sync asks "is today's data
/// somewhere else"; a drill asks "does the thing we have actually restore".
/// Only the second one can tell you your backups are worth having, and it is
/// the one nobody remembers to run by hand — which is the whole argument for
/// it being schedulable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    /// Back up a plan's tables, and optionally restore them to a destination.
    ///
    /// The default, and what every schedule written before drills existed is.
    #[default]
    Sync,
    /// Restore the newest artifact into a scratch database, check it, drop it.
    Drill,
}

impl ScheduleKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ScheduleKind::Sync => "sync",
            ScheduleKind::Drill => "drill",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sync" => Some(ScheduleKind::Sync),
            "drill" => Some(ScheduleKind::Drill),
            _ => None,
        }
    }
}

/// The restore half of a scheduled sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ScheduleRestore {
    pub naming: TargetNaming,
    pub options: EngineRestoreOptions,
}

/// Everything a scheduled run does, beyond what the plan already says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ScheduleAction {
    pub output_dir: PathBuf,
    pub compress: bool,
    pub encrypt: bool,
    pub backup: EngineBackupOptions,
    /// Present exactly when the schedule has a destination profile.
    pub restore: Option<ScheduleRestore>,
    pub verify: bool,
    /// Compare table contents, not only row counts.
    ///
    /// Off by default and defaulted on deserialise: it costs a full scan of
    /// every table on both sides, and an existing schedule must not silently
    /// acquire that cost on upgrade.
    #[serde(default)]
    pub deep_verify: bool,
    pub retention: Option<RetentionPolicy>,
    /// Count rows before dumping, so a drill can compare exactly.
    ///
    /// See [`crate::backup::CommonBackupOptions::record_row_counts`]. Defaulted
    /// off so an existing schedule does not silently acquire the scan.
    #[serde(default)]
    pub record_row_counts: bool,
    /// Drills only: leave the scratch database behind when the drill fails, so
    /// the wreckage can be inspected in the morning.
    ///
    /// A drill that *passes* always cleans up, whatever this says.
    #[serde(default)]
    pub keep_on_failure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("a schedule needs a name")]
    NameRequired,
    #[error(
        "a schedule cannot use the {0:?} strategy: it destroys an existing database, and there is \
         nobody present at the scheduled time to confirm that"
    )]
    DestructiveTarget(String),
    #[error("a schedule with a destination must say how to restore, and one without must not")]
    RestoreMismatch,
    #[error("a sync schedule needs a plan; it is where the tables to back up come from")]
    PlanRequired,
    #[error(
        "a drill has no sync plan: it restores whatever artifact is newest, and the artifact \
         already fixes what it contains"
    )]
    DrillTakesNoPlan,
    #[error("a drill needs a connection to restore into")]
    DrillNeedsProfile,
    #[error(
        "a drill chooses its own scratch database name and drops it afterwards, so it cannot be \
         given restore target options"
    )]
    DrillTakesNoTarget,
    #[error("the plan is for a {plan_engine:?} source but the restore options are {options:?}")]
    EngineMismatch {
        plan_engine: crate::types::Engine,
        options: crate::types::Engine,
    },
    #[error("{0:?} is not a usable webhook URL: {1}")]
    BadWebhook(String, String),
}

/// A named, recurring job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Schedule {
    pub id: Uuid,
    pub name: String,
    /// Defaulted, so every schedule written before drills existed reads as the
    /// sync it has always been.
    #[serde(default)]
    pub kind: ScheduleKind,
    /// The plan supplies the source profile, database and table selection.
    ///
    /// Required for a sync and always absent for a drill, which restores
    /// whatever artifact is newest rather than selecting anything.
    pub plan_id: Option<Uuid>,
    /// For a sync, `None` makes this a backup-only schedule. For a drill this
    /// is the connection being drilled, and it is required.
    ///
    /// Deliberately not a foreign key. If the destination profile is deleted,
    /// the schedule must fail loudly at its next run rather than have the
    /// column quietly set to NULL — which would silently downgrade a
    /// replication job to a local backup and nobody would notice for months.
    pub dest_profile_id: Option<Uuid>,
    #[specta(type = String)]
    pub cron: CronExpression,
    pub timezone: ScheduleTimezone,
    pub enabled: bool,
    pub action: ScheduleAction,
    pub webhook_url: Option<String>,
    pub notify: NotifyPolicy,
    /// Run an occurrence that was missed while the machine was asleep or the
    /// app was closed. At most one make-up run, however many were missed.
    pub catch_up: bool,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_outcome: Option<JobOutcome>,
    pub last_job_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ScheduleCreate {
    pub name: String,
    #[serde(default)]
    pub kind: ScheduleKind,
    pub plan_id: Option<Uuid>,
    pub dest_profile_id: Option<Uuid>,
    #[specta(type = String)]
    pub cron: CronExpression,
    #[serde(default)]
    pub timezone: ScheduleTimezone,
    pub action: ScheduleAction,
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub notify: NotifyPolicy,
    #[serde(default)]
    pub catch_up: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

const fn default_true() -> bool {
    true
}

/// A partial update. Absent fields are left as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ScheduleUpdate {
    pub name: Option<String>,
    #[specta(type = Option<String>)]
    pub cron: Option<CronExpression>,
    pub timezone: Option<ScheduleTimezone>,
    pub enabled: Option<bool>,
    pub action: Option<ScheduleAction>,
    /// Doubly optional so the webhook can be cleared as well as changed.
    #[serde(default, deserialize_with = "crate::profile::double_option")]
    #[specta(type = Option<String>)]
    pub webhook_url: Option<Option<String>>,
    pub notify: Option<NotifyPolicy>,
    pub catch_up: Option<bool>,
    #[serde(default, deserialize_with = "crate::profile::double_option")]
    #[specta(type = Option<String>)]
    pub dest_profile_id: Option<Option<Uuid>>,
}

/// Conservative restore options for a drill.
///
/// A drill restores into a database it just created, so the destructive
/// options that exist for real restores have nothing to act on and are left
/// off rather than defaulted on.
pub fn default_restore_options(engine: crate::types::Engine) -> EngineRestoreOptions {
    match engine {
        crate::types::Engine::Mysql => {
            EngineRestoreOptions::Mysql(crate::restore::MysqlRestoreOptions::default())
        }
        crate::types::Engine::Postgres => {
            EngineRestoreOptions::Postgres(crate::restore::PostgresRestoreOptions::default())
        }
        crate::types::Engine::Mongo => {
            EngineRestoreOptions::Mongo(crate::restore::MongoRestoreOptions::default())
        }
    }
}

/// How long after its nominal time an occurrence may still start.
///
/// Sized so a scheduler ticking every 30 seconds always catches the minute it
/// is meant to, while an occurrence genuinely slept through is left to the
/// catch-up rule rather than firing hours late by accident.
pub const DEFAULT_GRACE: Duration = Duration::seconds(90);

impl Schedule {
    /// Reject anything that cannot safely run unattended.
    pub fn validate(&self) -> Result<(), ScheduleError> {
        validate_parts(
            &self.name,
            self.kind,
            self.plan_id,
            self.dest_profile_id,
            &self.action,
            self.webhook_url.as_deref(),
        )
    }

    /// The instant this schedule should fire for, if it is due right now.
    ///
    /// Returns the *occurrence* time rather than `now`, so the run is recorded
    /// against the time it was scheduled for.
    pub fn due_at(&self, now: DateTime<Utc>, grace: Duration) -> Option<DateTime<Utc>> {
        if !self.enabled {
            return None;
        }

        let occurrence = self.cron.prev_at_or_before(self.timezone, now)?;

        // Never fire for an occurrence that predates the schedule itself:
        // creating a nightly backup at 09:00 must not immediately run last
        // night's. `last_run_at` then moves the line forward on every run,
        // which is also what stops a single occurrence firing twice.
        let baseline = self.last_run_at.unwrap_or(self.created_at);
        if occurrence <= baseline {
            return None;
        }

        // An occurrence that was slept through is only run if the user asked
        // for that. Otherwise a laptop opened at 09:00 would kick off a backup
        // meant for 02:00, in the middle of the working day.
        if !self.catch_up && now - occurrence > grace {
            return None;
        }

        Some(occurrence)
    }

    /// When this schedule will next fire, for display.
    pub fn next_run_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.enabled
            .then(|| self.cron.next_after(self.timezone, now))
            .flatten()
    }

    /// Whether this run also restores to a destination.
    ///
    /// A drill has a `dest_profile_id` too — it has to restore *somewhere* —
    /// so this deliberately checks the kind as well. Without that, every drill
    /// would report itself as a cross-server sync in job history and in
    /// notifications.
    pub const fn is_sync(&self) -> bool {
        matches!(self.kind, ScheduleKind::Sync) && self.dest_profile_id.is_some()
    }

    pub const fn is_drill(&self) -> bool {
        matches!(self.kind, ScheduleKind::Drill)
    }

    /// What a drill run needs, or `None` when this is not a drill.
    pub fn drill_request(&self) -> Option<crate::ops::DrillRequest> {
        if !self.is_drill() {
            return None;
        }
        Some(crate::ops::DrillRequest {
            // The same directory backups are written to: a drill's job is to
            // check what the backups actually produced.
            artifact_dir: self.action.output_dir.clone(),
            restore: match &self.action.restore {
                // Refused by `validate`, so this is unreachable in a stored
                // schedule; deriving from the backup engine keeps it correct
                // rather than relying on that.
                Some(r) => r.options.clone(),
                None => default_restore_options(self.action.backup.engine()),
            },
            deep_verify: self.action.deep_verify,
            keep_on_failure: self.action.keep_on_failure,
        })
    }

    /// The backup half, built from the plan's table selection.
    pub fn backup_request(&self, plan: &SyncPlan) -> BackupRequest {
        BackupRequest {
            common: CommonBackupOptions {
                database: plan.database.clone(),
                selections: plan.selections.clone(),
                output_dir: self.action.output_dir.clone(),
                compress: self.action.compress,
                encrypt: self.action.encrypt,
                // Scheduled backups are the ones a drill checks, so the
                // numbers that let it check exactly are worth the scan here
                // more than anywhere else.
                record_row_counts: self.action.record_row_counts,
            },
            engine: self.action.backup.clone(),
        }
    }

    /// The full sync request, or `None` for a backup-only schedule.
    pub fn sync_request(&self, plan: &SyncPlan) -> Option<SyncRequest> {
        let restore = self.action.restore.as_ref()?;
        Some(SyncRequest {
            backup: self.backup_request(plan),
            naming: restore.naming.clone(),
            restore: restore.options.clone(),
            verify: self.action.verify,
            deep_verify: self.action.deep_verify,
            // Taken from the plan, never stored on the schedule: an operator
            // who adds a masking rule expects every scheduled run of that plan
            // to pick it up, not just the ones created afterwards.
            masking: plan.masking.clone(),
            retention: self.action.retention,
            // Never populated. See the module docs: an unattended run has
            // nobody to confirm a destructive restore, so it must not be able
            // to supply the confirmation a destructive restore requires.
            typed_confirmation: None,
        })
    }
}

impl ScheduleCreate {
    pub fn validate(&self) -> Result<(), ScheduleError> {
        validate_parts(
            &self.name,
            self.kind,
            self.plan_id,
            self.dest_profile_id,
            &self.action,
            self.webhook_url.as_deref(),
        )
    }
}

fn validate_parts(
    name: &str,
    kind: ScheduleKind,
    plan: Option<Uuid>,
    dest: Option<Uuid>,
    action: &ScheduleAction,
    webhook: Option<&str>,
) -> Result<(), ScheduleError> {
    if name.trim().is_empty() {
        return Err(ScheduleError::NameRequired);
    }

    if let Some(url) = webhook {
        validate_webhook(url)?;
    }

    // ── Drills ──────────────────────────────────────────────────────────
    //
    // A drill's shape is the inverse of a sync's, and the mismatches are all
    // ways of describing a run that cannot happen. Refusing here means the
    // scheduler never has to decide what a half-specified schedule meant.
    if kind == ScheduleKind::Drill {
        if plan.is_some() {
            return Err(ScheduleError::DrillTakesNoPlan);
        }
        if dest.is_none() {
            return Err(ScheduleError::DrillNeedsProfile);
        }
        // `ops::drill` generates its own scratch name and refuses to drop
        // anything else. Accepting a naming strategy here would either be
        // ignored — silently, which is the failure this project keeps
        // removing — or would let a schedule aim a drill at a real database.
        if action.restore.is_some() {
            return Err(ScheduleError::DrillTakesNoTarget);
        }
        return Ok(());
    }

    // ── Syncs ───────────────────────────────────────────────────────────
    if plan.is_none() {
        return Err(ScheduleError::PlanRequired);
    }

    // A destination without restore options, or restore options without a
    // destination, means the UI and the engine disagree about what this
    // schedule is. Neither is safe to guess at.
    if dest.is_some() != action.restore.is_some() {
        return Err(ScheduleError::RestoreMismatch);
    }

    if let Some(restore) = &action.restore {
        if restore.naming.is_destructive() {
            return Err(ScheduleError::DestructiveTarget(format!(
                "{:?}",
                restore.naming
            )));
        }
        if restore.options.engine() != action.backup.engine() {
            return Err(ScheduleError::EngineMismatch {
                plan_engine: action.backup.engine(),
                options: restore.options.engine(),
            });
        }
    }

    Ok(())
}

/// A webhook URL must be one this application will actually send to.
///
/// Checked here rather than at send time so a typo surfaces while the user is
/// still looking at the form, not silently at 03:00 three weeks later.
pub fn validate_webhook(raw: &str) -> Result<(), ScheduleError> {
    let url = url::Url::parse(raw)
        .map_err(|e| ScheduleError::BadWebhook(raw.to_string(), e.to_string()))?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(ScheduleError::BadWebhook(
            raw.to_string(),
            format!("the scheme must be http or https, not {:?}", url.scheme()),
        ));
    }

    if url.host().is_none() {
        return Err(ScheduleError::BadWebhook(
            raw.to_string(),
            "there is no host in the URL".into(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::{MysqlBackupOptions, TableSelection};
    use crate::restore::MysqlRestoreOptions;

    fn action(restore: Option<ScheduleRestore>) -> ScheduleAction {
        ScheduleAction {
            output_dir: PathBuf::from("/backups"),
            compress: true,
            encrypt: false,
            backup: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
            restore,
            verify: true,
            deep_verify: false,
            retention: None,
            record_row_counts: false,
            keep_on_failure: false,
        }
    }

    fn safe_restore() -> ScheduleRestore {
        ScheduleRestore {
            naming: TargetNaming::NewTimestamped {
                prefix: "staging".into(),
            },
            options: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
        }
    }

    fn schedule(cron: &str, created_at: DateTime<Utc>) -> Schedule {
        Schedule {
            id: Uuid::new_v4(),
            name: "nightly".into(),
            kind: ScheduleKind::Sync,
            plan_id: Some(Uuid::new_v4()),
            dest_profile_id: None,
            cron: cron.parse().unwrap(),
            timezone: ScheduleTimezone::Utc,
            enabled: true,
            action: action(None),
            webhook_url: None,
            notify: NotifyPolicy::OnFailure,
            catch_up: false,
            last_run_at: None,
            last_outcome: None,
            last_job_id: None,
            created_at,
            updated_at: created_at,
        }
    }

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 26, h, m, 0).unwrap()
    }
    use chrono::TimeZone;

    // ── Safety ──────────────────────────────────────────────────────────

    #[test]
    fn a_destructive_target_cannot_be_scheduled() {
        // The whole reason this check exists: at 03:00 nobody can be asked.
        let mut a = action(Some(ScheduleRestore {
            naming: TargetNaming::DropAndRecreate {
                name: "production".into(),
            },
            options: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
        }));
        a.verify = true;

        let err = validate_parts(
            "nightly",
            ScheduleKind::Sync,
            Some(Uuid::new_v4()),
            Some(Uuid::new_v4()),
            &a,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, ScheduleError::DestructiveTarget(_)));
    }

    #[test]
    fn a_scheduled_sync_never_carries_a_confirmation() {
        // Belt and braces: even if a destructive naming strategy reached a
        // stored schedule, the request built from it supplies no confirmation,
        // so the restore layer refuses it.
        let mut s = schedule("0 3 * * *", at(0, 0));
        s.dest_profile_id = Some(Uuid::new_v4());
        s.action = action(Some(ScheduleRestore {
            naming: TargetNaming::DropAndRecreate {
                name: "production".into(),
            },
            options: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
        }));

        let plan = plan_for(&s);
        let request = s.sync_request(&plan).unwrap();
        assert!(
            request.typed_confirmation.is_none(),
            "an unattended run must not be able to confirm its own destruction"
        );
    }

    #[test]
    fn a_non_destructive_target_is_accepted() {
        let a = action(Some(safe_restore()));
        assert!(
            validate_parts(
                "nightly",
                ScheduleKind::Sync,
                Some(Uuid::new_v4()),
                Some(Uuid::new_v4()),
                &a,
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn destination_and_restore_options_must_agree() {
        // Destination without restore options.
        assert!(matches!(
            validate_parts(
                "n",
                ScheduleKind::Sync,
                Some(Uuid::new_v4()),
                Some(Uuid::new_v4()),
                &action(None),
                None
            ),
            Err(ScheduleError::RestoreMismatch)
        ));
        // Restore options without a destination.
        assert!(matches!(
            validate_parts(
                "n",
                ScheduleKind::Sync,
                Some(Uuid::new_v4()),
                None,
                &action(Some(safe_restore())),
                None
            ),
            Err(ScheduleError::RestoreMismatch)
        ));
    }

    #[test]
    fn mismatched_engines_are_rejected() {
        let a = action(Some(ScheduleRestore {
            naming: TargetNaming::NewTimestamped { prefix: "s".into() },
            options: EngineRestoreOptions::Postgres(Default::default()),
        }));
        assert!(matches!(
            validate_parts(
                "n",
                ScheduleKind::Sync,
                Some(Uuid::new_v4()),
                Some(Uuid::new_v4()),
                &a,
                None
            ),
            Err(ScheduleError::EngineMismatch { .. })
        ));
    }

    #[test]
    fn a_schedule_needs_a_name() {
        assert!(matches!(
            validate_parts(
                "   ",
                ScheduleKind::Sync,
                Some(Uuid::new_v4()),
                None,
                &action(None),
                None
            ),
            Err(ScheduleError::NameRequired)
        ));
    }

    #[test]
    fn webhook_urls_are_checked_while_the_user_is_still_looking() {
        assert!(validate_webhook("https://hooks.example.com/abc").is_ok());
        assert!(validate_webhook("http://localhost:9000/hook").is_ok());

        assert!(validate_webhook("not a url").is_err());
        assert!(validate_webhook("ftp://example.com/x").is_err());
        // No scheme at all is the commonest paste error.
        assert!(validate_webhook("hooks.example.com/abc").is_err());
    }

    // ── Due detection ───────────────────────────────────────────────────

    #[test]
    fn a_new_schedule_does_not_immediately_run_a_past_occurrence() {
        // Created at 09:00 with a 03:00 daily schedule: the 03:00 that already
        // happened today is not this schedule's to run.
        let s = schedule("0 3 * * *", at(9, 0));
        assert!(s.due_at(at(9, 0), DEFAULT_GRACE).is_none());
        assert!(s.due_at(at(12, 0), DEFAULT_GRACE).is_none());
    }

    #[test]
    fn fires_at_its_occurrence() {
        let s = schedule("0 3 * * *", at(1, 0));
        assert_eq!(s.due_at(at(3, 0), DEFAULT_GRACE), Some(at(3, 0)));
    }

    #[test]
    fn fires_when_the_tick_is_slightly_late() {
        // Ticks are every 30s and the machine is busy; the grace window is
        // what stops a backup being skipped because the tick landed at :17.
        let s = schedule("0 3 * * *", at(1, 0));
        let slightly_late = at(3, 0) + Duration::seconds(45);
        assert_eq!(s.due_at(slightly_late, DEFAULT_GRACE), Some(at(3, 0)));
    }

    #[test]
    fn does_not_fire_twice_for_one_occurrence() {
        let mut s = schedule("0 3 * * *", at(1, 0));
        let due = s.due_at(at(3, 0), DEFAULT_GRACE).unwrap();

        s.last_run_at = Some(due);
        assert!(
            s.due_at(at(3, 0) + Duration::seconds(30), DEFAULT_GRACE)
                .is_none(),
            "the same occurrence must not run twice"
        );
        // The next day's occurrence is still due.
        let tomorrow = at(3, 0) + Duration::days(1);
        assert_eq!(s.due_at(tomorrow, DEFAULT_GRACE), Some(tomorrow));
    }

    #[test]
    fn without_catch_up_a_missed_occurrence_is_left_alone() {
        // The laptop was shut at 03:00 and opened at 09:00. Kicking off a
        // production backup in the middle of the morning is not what the user
        // asked for when they said "at 3am".
        let s = schedule("0 3 * * *", at(0, 0));
        assert!(s.due_at(at(9, 0), DEFAULT_GRACE).is_none());
    }

    #[test]
    fn with_catch_up_a_missed_occurrence_runs_once() {
        let mut s = schedule("0 3 * * *", at(0, 0));
        s.catch_up = true;

        let due = s.due_at(at(9, 0), DEFAULT_GRACE).expect("should catch up");
        assert_eq!(due, at(3, 0), "recorded against its scheduled time");

        // And exactly once, however many were missed.
        s.last_run_at = Some(due);
        assert!(s.due_at(at(9, 5), DEFAULT_GRACE).is_none());
    }

    #[test]
    fn catch_up_makes_up_one_run_not_one_per_missed_occurrence() {
        // Away for a week with an hourly schedule: 168 missed occurrences must
        // produce one run, not 168.
        let mut s = schedule("0 * * * *", at(0, 0));
        s.catch_up = true;
        s.last_run_at = Some(at(1, 0));

        let a_week_later = at(1, 0) + Duration::days(7);
        let due = s.due_at(a_week_later, DEFAULT_GRACE).unwrap();
        assert_eq!(
            due, a_week_later,
            "the most recent occurrence, not the oldest"
        );

        s.last_run_at = Some(due);
        assert!(s.due_at(a_week_later, DEFAULT_GRACE).is_none());
    }

    #[test]
    fn a_disabled_schedule_never_fires() {
        let mut s = schedule("0 3 * * *", at(1, 0));
        s.enabled = false;
        assert!(s.due_at(at(3, 0), DEFAULT_GRACE).is_none());
        assert!(s.next_run_at(at(3, 0)).is_none());
    }

    #[test]
    fn next_run_at_looks_forward() {
        let s = schedule("0 3 * * *", at(1, 0));
        assert_eq!(s.next_run_at(at(1, 0)), Some(at(3, 0)));
        assert_eq!(s.next_run_at(at(4, 0)), Some(at(3, 0) + Duration::days(1)));
    }

    // ── Request construction ────────────────────────────────────────────

    fn plan_for(s: &Schedule) -> SyncPlan {
        SyncPlan {
            id: s.plan_id.expect("a sync schedule always has a plan"),
            profile_id: Uuid::new_v4(),
            name: "nightly".into(),
            database: "app".into(),
            selections: vec![
                TableSelection::with_data("orders"),
                TableSelection::schema_only("audit_log"),
            ],
            masking: vec![crate::mask::MaskRule::email("orders", "buyer_email")],
            revision: 3,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn the_backup_request_comes_from_the_plan() {
        // The plan is the single source of truth for what gets backed up, so a
        // plan edit takes effect on the next run without touching the schedule.
        let s = schedule("0 3 * * *", at(0, 0));
        let plan = plan_for(&s);
        let req = s.backup_request(&plan);

        assert_eq!(req.common.database, "app");
        assert_eq!(req.common.selections, plan.selections);
        assert_eq!(req.common.output_dir, PathBuf::from("/backups"));
    }

    #[test]
    fn masking_rules_reach_a_scheduled_sync_from_the_plan() {
        // An operator who adds a masking rule to a plan expects the schedule
        // that has been running for months to start applying it. Storing the
        // rules on the schedule instead would silently protect only the runs
        // configured after the rule was written.
        let mut s = schedule("0 3 * * *", at(0, 0));
        s.dest_profile_id = Some(Uuid::new_v4());
        s.action = action(Some(safe_restore()));

        let plan = plan_for(&s);
        let req = s
            .sync_request(&plan)
            .expect("a sync schedule has a request");
        assert_eq!(req.masking, plan.masking);
    }

    #[test]
    fn a_backup_only_schedule_has_no_sync_request() {
        let s = schedule("0 3 * * *", at(0, 0));
        assert!(!s.is_sync());
        assert!(s.sync_request(&plan_for(&s)).is_none());
    }

    #[test]
    fn a_sync_schedule_carries_the_restore_options_through() {
        let mut s = schedule("0 3 * * *", at(0, 0));
        s.dest_profile_id = Some(Uuid::new_v4());
        s.action = action(Some(safe_restore()));

        assert!(s.is_sync());
        let req = s.sync_request(&plan_for(&s)).unwrap();
        assert!(req.verify);
        assert_eq!(req.naming, safe_restore().naming);
    }

    // ── Notification policy ─────────────────────────────────────────────

    #[test]
    fn the_default_policy_reports_failures_only() {
        let p = NotifyPolicy::default();
        assert_eq!(p, NotifyPolicy::OnFailure);
        assert!(
            !p.wants(JobOutcome::Success),
            "a working backup is not news"
        );
        assert!(p.wants(JobOutcome::Failed));
        assert!(p.wants(JobOutcome::Cancelled));
    }

    #[test]
    fn notify_policies_round_trip_through_storage() {
        for p in [
            NotifyPolicy::Never,
            NotifyPolicy::OnFailure,
            NotifyPolicy::Always,
        ] {
            assert_eq!(NotifyPolicy::parse(p.as_str()), Some(p));
        }
        assert_eq!(NotifyPolicy::parse("nonsense"), None);
    }

    #[test]
    fn never_and_always_are_absolute() {
        for outcome in [
            JobOutcome::Success,
            JobOutcome::Failed,
            JobOutcome::Cancelled,
        ] {
            assert!(!NotifyPolicy::Never.wants(outcome));
            assert!(NotifyPolicy::Always.wants(outcome));
        }
    }

    // ── Drill schedules ─────────────────────────────────────────────────

    fn drill_schedule() -> Schedule {
        let mut s = schedule("0 4 * * *", Utc::now());
        s.kind = ScheduleKind::Drill;
        s.plan_id = None;
        s.dest_profile_id = Some(Uuid::new_v4());
        s
    }

    #[test]
    fn a_well_formed_drill_validates() {
        assert!(drill_schedule().validate().is_ok());
    }

    #[test]
    fn a_drill_needs_somewhere_to_restore_into() {
        let mut s = drill_schedule();
        s.dest_profile_id = None;
        assert!(matches!(
            s.validate(),
            Err(ScheduleError::DrillNeedsProfile)
        ));
    }

    #[test]
    fn a_drill_refuses_a_sync_plan() {
        // A drill restores whatever artifact is newest. A plan would either be
        // ignored — silently, which is the failure mode this project keeps
        // removing — or would imply a table selection the artifact overrides.
        let mut s = drill_schedule();
        s.plan_id = Some(Uuid::new_v4());
        assert!(matches!(s.validate(), Err(ScheduleError::DrillTakesNoPlan)));
    }

    #[test]
    fn a_drill_refuses_restore_target_options() {
        // The load-bearing one. `ops::drill` only drops databases matching the
        // name it generated itself; letting a schedule supply a naming
        // strategy would be the one way to aim an unattended drill at a real
        // database.
        let mut s = drill_schedule();
        s.action.restore = Some(safe_restore());
        assert!(matches!(
            s.validate(),
            Err(ScheduleError::DrillTakesNoTarget)
        ));
    }

    #[test]
    fn a_sync_still_needs_a_plan() {
        let mut s = schedule("0 3 * * *", Utc::now());
        s.plan_id = None;
        assert!(matches!(s.validate(), Err(ScheduleError::PlanRequired)));
    }

    #[test]
    fn a_drill_is_not_reported_as_a_sync() {
        // A drill has a destination profile too, so a predicate based only on
        // that would label every drill a cross-server sync in job history and
        // in notifications.
        let s = drill_schedule();
        assert!(s.is_drill());
        assert!(!s.is_sync(), "a drill copies nothing between servers");
    }

    #[test]
    fn a_drill_request_carries_the_backup_folder_and_the_flags() {
        let mut s = drill_schedule();
        s.action.deep_verify = true;
        s.action.keep_on_failure = true;

        let request = s.drill_request().expect("a drill builds a request");
        assert_eq!(request.artifact_dir, PathBuf::from("/backups"));
        assert!(request.deep_verify);
        assert!(request.keep_on_failure);
    }

    #[test]
    fn a_sync_schedule_builds_no_drill_request() {
        assert!(schedule("0 3 * * *", Utc::now()).drill_request().is_none());
    }

    #[test]
    fn a_schedule_written_before_drills_existed_reads_as_a_sync() {
        // Every schedule already stored. Built by serialising a current one and
        // deleting the new key, so this stays honest as the shape changes.
        let current = schedule("0 3 * * *", Utc::now());
        let mut json = serde_json::to_value(&current).unwrap();
        json.as_object_mut().unwrap().remove("kind").unwrap();

        let parsed: Schedule = serde_json::from_value(json).expect("an old schedule must load");
        assert_eq!(parsed.kind, ScheduleKind::Sync);
        assert!(parsed.validate().is_ok());
    }

    #[test]
    fn the_kind_round_trips_through_its_stored_string() {
        for kind in [ScheduleKind::Sync, ScheduleKind::Drill] {
            assert_eq!(ScheduleKind::parse(kind.as_str()), Some(kind));
        }
        // An unknown value must not silently become a sync: that would run a
        // drill's row down the sync path and report its absent plan as deleted.
        assert_eq!(ScheduleKind::parse("backup"), None);
    }
}
