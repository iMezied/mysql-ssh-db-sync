//! Scheduler behaviour that needs no database server.
//!
//! Everything here is about what happens when a schedule outlives the things it
//! points at, or is asked to run something it cannot. Those paths never reach a
//! database, so they can be driven for real — the run genuinely executes, fails,
//! records history and reports — without any container.
//!
//! The happy path (a scheduled run that actually moves data) is in
//! `roundtrip.rs`, behind the container gate.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use db_sync_engine::backup::{EngineBackupOptions, MysqlBackupOptions, TableMode, TableSelection};
use db_sync_engine::cron::ScheduleTimezone;
use db_sync_engine::events::{EVENT_CHANNEL_CAPACITY, create_event_channel};
use db_sync_engine::job::{JobOutcome, JobRegistry};
use db_sync_engine::notify::RunReport;
use db_sync_engine::plan::SyncPlanCreate;
use db_sync_engine::profile::{DbConfig, ProfileCreate, ToolOverrides};
use db_sync_engine::restore::{EngineRestoreOptions, MysqlRestoreOptions, TargetNaming};
use db_sync_engine::schedule::{NotifyPolicy, ScheduleAction, ScheduleCreate, ScheduleRestore};
use db_sync_engine::scheduler::{Scheduler, SchedulerHooks};
use db_sync_engine::store::Store;
use db_sync_engine::types::{Engine, EnvironmentTag};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Captures whatever the scheduler reports, so assertions can be made about the
/// message a user would actually see.
#[derive(Default)]
struct RecordingHooks {
    reports: Mutex<Vec<RunReport>>,
    count: AtomicUsize,
}

/// A newtype, because the orphan rule forbids implementing the engine's trait
/// directly for `Arc<T>`.
struct Recorder(Arc<RecordingHooks>);

#[async_trait::async_trait]
impl SchedulerHooks for Recorder {
    async fn run_finished(&self, report: &RunReport) {
        self.0.reports.lock().await.push(report.clone());
        self.0.count.fetch_add(1, Ordering::SeqCst);
    }
}

struct Harness {
    store: Store,
    scheduler: Scheduler,
    hooks: Arc<RecordingHooks>,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).await.unwrap();
    let (tx, _rx) = create_event_channel(EVENT_CHANNEL_CAPACITY);

    let hooks = Arc::new(RecordingHooks::default());
    let scheduler = Scheduler::new(store.clone(), JobRegistry::new(), tx)
        .with_hooks(Arc::new(Recorder(hooks.clone())));

    Harness {
        store,
        scheduler,
        hooks,
        _dir: dir,
    }
}

impl Harness {
    /// Wait for every in-flight run to finish, or give up.
    async fn settle(&self) {
        for _ in 0..200 {
            if self.scheduler.in_flight_ids().await.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("a scheduled run never finished");
    }

    async fn only_report(&self) -> RunReport {
        let reports = self.hooks.reports.lock().await;
        assert_eq!(reports.len(), 1, "expected exactly one report");
        reports[0].clone()
    }
}

async fn profile(store: &Store, name: &str) -> Uuid {
    profile_at(store, name, "127.0.0.1").await
}

async fn profile_at(store: &Store, name: &str, host: &str) -> Uuid {
    store
        .create_profile(ProfileCreate {
            name: name.into(),
            engine: Engine::Mysql,
            environment: EnvironmentTag::Dev,
            ssh: None,
            db: DbConfig {
                host: host.into(),
                port: 3306,
                user: "app".into(),
                database: None,
            },
            tool_overrides: ToolOverrides::default(),
        })
        .await
        .unwrap()
        .id
}

/// TEST-NET-1 (RFC 5737). Reserved for documentation and guaranteed not to be
/// routed, so a connection to it hangs rather than failing fast — which is
/// exactly what a test needs when it wants a run to stay in flight.
const UNROUTABLE: &str = "192.0.2.1";

async fn plan(store: &Store, profile_id: Uuid, selections: Vec<TableSelection>) -> Uuid {
    store
        .create_sync_plan(SyncPlanCreate {
            profile_id,
            name: "nightly".into(),
            database: "app".into(),
            selections,
        })
        .await
        .unwrap()
        .id
}

fn action(restore: Option<ScheduleRestore>, output_dir: PathBuf) -> ScheduleAction {
    ScheduleAction {
        output_dir,
        compress: true,
        encrypt: false,
        backup: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
        restore,
        verify: true,
        retention: None,
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

// ── A schedule that outlives what it points at ──────────────────────────

#[tokio::test]
async fn a_deleted_destination_profile_fails_the_run_loudly() {
    // The behaviour the schema deliberately chose over ON DELETE SET NULL:
    // rather than quietly becoming a local-backup-only job, the schedule must
    // fail and say why. Nothing may touch the source before that is known.
    let h = harness().await;

    let source = profile(&h.store, "prod").await;
    let dest = profile(&h.store, "staging").await;
    let plan_id = plan(&h.store, source, vec![TableSelection::with_data("orders")]).await;

    let schedule = h
        .store
        .create_schedule(ScheduleCreate {
            name: "staging refresh".into(),
            plan_id,
            dest_profile_id: Some(dest),
            cron: "0 3 * * *".parse().unwrap(),
            timezone: ScheduleTimezone::Utc,
            action: action(Some(safe_restore()), h._dir.path().to_path_buf()),
            webhook_url: None,
            notify: NotifyPolicy::Always,
            catch_up: false,
            enabled: true,
        })
        .await
        .unwrap();

    h.store.delete_profile(dest).await.unwrap();

    h.scheduler.run_now(schedule.id).await.unwrap().unwrap();
    h.settle().await;

    let report = h.only_report().await;
    assert_eq!(report.outcome, JobOutcome::Failed);

    let error = report.error.expect("a failure must carry a message");
    assert!(
        error.contains("destination profile") && error.contains("deleted"),
        "the message must name the actual cause; got: {error}"
    );
    assert!(
        error.contains("staging refresh"),
        "and name the schedule: {error}"
    );
    assert!(
        error.contains("nothing was backed up"),
        "and say that nothing happened: {error}"
    );

    // And the outcome is durable, not just reported.
    let after = h.store.get_schedule(schedule.id).await.unwrap().unwrap();
    assert_eq!(after.last_outcome, Some(JobOutcome::Failed));
}

#[tokio::test]
async fn an_unrunnable_plan_fails_before_opening_any_connection() {
    // A plan whose every table is excluded backs up nothing. Catching that
    // offline means the run fails in milliseconds rather than after opening a
    // tunnel to production to discover there is nothing to do.
    let h = harness().await;

    let source = profile(&h.store, "prod").await;
    let plan_id = plan(
        &h.store,
        source,
        vec![TableSelection {
            name: "orders".into(),
            mode: TableMode::Exclude,
            where_filter: None,
        }],
    )
    .await;

    let schedule = h
        .store
        .create_schedule(ScheduleCreate {
            name: "empty".into(),
            plan_id,
            dest_profile_id: None,
            cron: "0 3 * * *".parse().unwrap(),
            timezone: ScheduleTimezone::Utc,
            action: action(None, h._dir.path().to_path_buf()),
            webhook_url: None,
            notify: NotifyPolicy::Always,
            catch_up: false,
            enabled: true,
        })
        .await
        .unwrap();

    let started = std::time::Instant::now();
    h.scheduler.run_now(schedule.id).await.unwrap().unwrap();
    h.settle().await;

    let report = h.only_report().await;
    assert_eq!(report.outcome, JobOutcome::Failed);
    assert!(
        report
            .error
            .as_deref()
            .unwrap()
            .contains("every table is excluded"),
        "got: {:?}",
        report.error
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "an offline validation failure must not wait on the network"
    );
}

// ── Concurrency ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_schedule_already_running_is_not_started_again() {
    // A backup slower than its own interval must not run twice at once: two
    // dumps of the same source writing into the same directory is how a backup
    // set gets corrupted.
    let h = harness().await;

    // Pointed at an unroutable address so the first run is reliably still
    // connecting when the second request arrives. Racing a fast failure would
    // make this test pass or fail on how long a keychain lookup happened to
    // take on the day.
    let source = profile_at(&h.store, "prod", UNROUTABLE).await;
    let plan_id = plan(&h.store, source, vec![TableSelection::with_data("orders")]).await;

    let schedule = h
        .store
        .create_schedule(ScheduleCreate {
            name: "slow".into(),
            plan_id,
            dest_profile_id: None,
            cron: "0 3 * * *".parse().unwrap(),
            timezone: ScheduleTimezone::Utc,
            action: action(None, h._dir.path().to_path_buf()),
            webhook_url: None,
            notify: NotifyPolicy::Never,
            catch_up: false,
            enabled: true,
        })
        .await
        .unwrap();

    // Occupy the slot the way a long-running job would.
    assert!(h.scheduler.run_now(schedule.id).await.unwrap().is_some());

    // A second request while the first is in flight is refused rather than
    // queued — running it later would be worse than not running it.
    assert!(
        h.scheduler.run_now(schedule.id).await.unwrap().is_none(),
        "a second concurrent run must be refused while the first is in flight"
    );

    // Deliberately not settled: the in-flight run is waiting on a connection
    // that will never be answered, and waiting out its timeout would add
    // minutes to the suite for no extra assurance. The task is abandoned when
    // the test runtime is dropped.
}

// ── Notification policy ─────────────────────────────────────────────────

#[tokio::test]
async fn the_default_policy_stays_quiet_about_success_but_not_failure() {
    let h = harness().await;

    let source = profile(&h.store, "prod").await;
    let dest = profile(&h.store, "staging").await;
    let plan_id = plan(&h.store, source, vec![TableSelection::with_data("orders")]).await;

    let schedule = h
        .store
        .create_schedule(ScheduleCreate {
            name: "quiet".into(),
            plan_id,
            dest_profile_id: Some(dest),
            cron: "0 3 * * *".parse().unwrap(),
            timezone: ScheduleTimezone::Utc,
            action: action(Some(safe_restore()), h._dir.path().to_path_buf()),
            webhook_url: None,
            notify: NotifyPolicy::OnFailure,
            catch_up: false,
            enabled: true,
        })
        .await
        .unwrap();

    h.store.delete_profile(dest).await.unwrap();
    h.scheduler.run_now(schedule.id).await.unwrap().unwrap();
    h.settle().await;

    assert_eq!(
        h.hooks.count.load(Ordering::SeqCst),
        1,
        "a failure must be reported under the default policy"
    );
}

#[tokio::test]
async fn a_never_policy_reports_nothing_at_all() {
    let h = harness().await;

    let source = profile(&h.store, "prod").await;
    let dest = profile(&h.store, "staging").await;
    let plan_id = plan(&h.store, source, vec![TableSelection::with_data("orders")]).await;

    let schedule = h
        .store
        .create_schedule(ScheduleCreate {
            name: "silent".into(),
            plan_id,
            dest_profile_id: Some(dest),
            cron: "0 3 * * *".parse().unwrap(),
            timezone: ScheduleTimezone::Utc,
            action: action(Some(safe_restore()), h._dir.path().to_path_buf()),
            webhook_url: None,
            notify: NotifyPolicy::Never,
            catch_up: false,
            enabled: true,
        })
        .await
        .unwrap();

    h.store.delete_profile(dest).await.unwrap();
    h.scheduler.run_now(schedule.id).await.unwrap().unwrap();
    h.settle().await;

    assert_eq!(h.hooks.count.load(Ordering::SeqCst), 0);

    // Silence is about notifications only; the record is still written.
    let after = h.store.get_schedule(schedule.id).await.unwrap().unwrap();
    assert_eq!(after.last_outcome, Some(JobOutcome::Failed));
}

// ── Manual runs ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_manual_run_does_not_consume_the_next_scheduled_occurrence() {
    // Pressing "Run now" at 14:00 to test a schedule must not cancel the 03:00
    // run it was created for.
    let h = harness().await;

    let source = profile(&h.store, "prod").await;
    let dest = profile(&h.store, "staging").await;
    let plan_id = plan(&h.store, source, vec![TableSelection::with_data("orders")]).await;

    let schedule = h
        .store
        .create_schedule(ScheduleCreate {
            name: "nightly".into(),
            plan_id,
            dest_profile_id: Some(dest),
            cron: "0 3 * * *".parse().unwrap(),
            timezone: ScheduleTimezone::Utc,
            action: action(Some(safe_restore()), h._dir.path().to_path_buf()),
            webhook_url: None,
            notify: NotifyPolicy::Never,
            catch_up: true,
            enabled: true,
        })
        .await
        .unwrap();

    h.store.delete_profile(dest).await.unwrap();
    h.scheduler.run_now(schedule.id).await.unwrap().unwrap();
    h.settle().await;

    let after = h.store.get_schedule(schedule.id).await.unwrap().unwrap();
    assert!(
        after.last_run_at.is_none(),
        "a manual run must not move the schedule's high-water mark"
    );
    assert_eq!(
        after.last_outcome,
        Some(JobOutcome::Failed),
        "but it is still recorded as the last thing that happened"
    );
}

// ── Ticking ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_tick_ignores_disabled_schedules() {
    let h = harness().await;

    let source = profile(&h.store, "prod").await;
    let plan_id = plan(&h.store, source, vec![TableSelection::with_data("orders")]).await;

    let mut input = ScheduleCreate {
        name: "off".into(),
        plan_id,
        dest_profile_id: None,
        // Due every minute, so it would certainly fire if it were enabled.
        cron: "* * * * *".parse().unwrap(),
        timezone: ScheduleTimezone::Utc,
        action: action(None, h._dir.path().to_path_buf()),
        webhook_url: None,
        notify: NotifyPolicy::Always,
        catch_up: true,
        enabled: false,
    };
    input.enabled = false;
    let schedule = h.store.create_schedule(input).await.unwrap();

    h.scheduler
        .tick_once(schedule.created_at + chrono::Duration::hours(2))
        .await;

    assert!(h.scheduler.in_flight_ids().await.is_empty());
    assert_eq!(h.hooks.count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_tick_starts_a_schedule_that_has_come_due() {
    let h = harness().await;

    let source = profile(&h.store, "prod").await;
    let dest = profile(&h.store, "staging").await;
    let plan_id = plan(&h.store, source, vec![TableSelection::with_data("orders")]).await;

    let schedule = h
        .store
        .create_schedule(ScheduleCreate {
            name: "hourly".into(),
            plan_id,
            dest_profile_id: Some(dest),
            cron: "0 * * * *".parse().unwrap(),
            timezone: ScheduleTimezone::Utc,
            action: action(Some(safe_restore()), h._dir.path().to_path_buf()),
            webhook_url: None,
            notify: NotifyPolicy::Always,
            // Two hours have "passed" between creation and the tick, so the
            // run only happens because catch-up is on.
            catch_up: true,
            enabled: true,
        })
        .await
        .unwrap();

    h.store.delete_profile(dest).await.unwrap();
    h.scheduler
        .tick_once(schedule.created_at + chrono::Duration::hours(2))
        .await;
    h.settle().await;

    assert_eq!(h.hooks.count.load(Ordering::SeqCst), 1);

    let after = h.store.get_schedule(schedule.id).await.unwrap().unwrap();
    assert!(
        after.last_run_at.is_some(),
        "a scheduled run does move the high-water mark, unlike a manual one"
    );
}

#[tokio::test]
async fn a_tick_does_not_rerun_an_occurrence_it_already_ran() {
    let h = harness().await;

    let source = profile(&h.store, "prod").await;
    let dest = profile(&h.store, "staging").await;
    let plan_id = plan(&h.store, source, vec![TableSelection::with_data("orders")]).await;

    let schedule = h
        .store
        .create_schedule(ScheduleCreate {
            name: "hourly".into(),
            plan_id,
            dest_profile_id: Some(dest),
            cron: "0 * * * *".parse().unwrap(),
            timezone: ScheduleTimezone::Utc,
            action: action(Some(safe_restore()), h._dir.path().to_path_buf()),
            webhook_url: None,
            notify: NotifyPolicy::Always,
            catch_up: true,
            enabled: true,
        })
        .await
        .unwrap();

    h.store.delete_profile(dest).await.unwrap();

    let now = schedule.created_at + chrono::Duration::hours(2);
    h.scheduler.tick_once(now).await;
    h.settle().await;

    // Same instant, second tick: the occurrence has already been served.
    h.scheduler.tick_once(now).await;
    h.settle().await;

    assert_eq!(
        h.hooks.count.load(Ordering::SeqCst),
        1,
        "one occurrence must produce exactly one run"
    );
}
