//! Integration tests for schedule persistence.
//!
//! Against a real SQLite file, because the things most likely to be wrong here
//! are exactly the things a mock hides: whether migration 0002 actually applies
//! on top of 0001, whether an opaque options blob survives a round trip, and
//! whether the safety checks are enforced by the *store* rather than only by
//! the UI that happens to call it.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use db_sync_engine::backup::{EngineBackupOptions, MysqlBackupOptions, TableSelection};
use db_sync_engine::cron::ScheduleTimezone;
use db_sync_engine::job::JobOutcome;
use db_sync_engine::plan::SyncPlanCreate;
use db_sync_engine::profile::{DbConfig, ProfileCreate, ToolOverrides};
use db_sync_engine::restore::{EngineRestoreOptions, MysqlRestoreOptions, TargetNaming};
use db_sync_engine::retention::RetentionPolicy;
use db_sync_engine::schedule::{
    DEFAULT_GRACE, NotifyPolicy, ScheduleAction, ScheduleCreate, ScheduleError, ScheduleRestore,
    ScheduleUpdate,
};
use db_sync_engine::store::{Store, StoreError};
use db_sync_engine::types::{Engine, EnvironmentTag};
use uuid::Uuid;

async fn store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("test.db"))
        .await
        .expect("open store");
    (store, dir)
}

/// A profile and a plan for the schedule to point at.
async fn plan_and_profiles(store: &Store) -> (Uuid, Uuid) {
    let source = store
        .create_profile(ProfileCreate {
            name: "prod".into(),
            engine: Engine::Mysql,
            environment: EnvironmentTag::Prod,
            ssh: None,
            db: DbConfig {
                host: "10.0.0.1".into(),
                port: 3306,
                user: "backup".into(),
                database: Some("app".into()),
            },
            tool_overrides: ToolOverrides::default(),
        })
        .await
        .unwrap();

    let dest = store
        .create_profile(ProfileCreate {
            name: "staging".into(),
            engine: Engine::Mysql,
            environment: EnvironmentTag::Staging,
            ssh: None,
            db: DbConfig {
                host: "10.0.0.2".into(),
                port: 3306,
                user: "restore".into(),
                database: None,
            },
            tool_overrides: ToolOverrides::default(),
        })
        .await
        .unwrap();

    let plan = store
        .create_sync_plan(SyncPlanCreate {
            profile_id: source.id,
            name: "nightly".into(),
            database: "app".into(),
            selections: vec![
                TableSelection::with_data("orders"),
                TableSelection::schema_only("audit_log"),
            ],
        })
        .await
        .unwrap();

    (plan.id, dest.id)
}

fn action(restore: Option<ScheduleRestore>) -> ScheduleAction {
    ScheduleAction {
        output_dir: PathBuf::from("/backups/nightly"),
        compress: true,
        encrypt: false,
        backup: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
        restore,
        verify: true,
        retention: Some(RetentionPolicy {
            keep_last: Some(7),
            max_age_days: Some(30),
        }),
    }
}

fn safe_restore() -> ScheduleRestore {
    ScheduleRestore {
        naming: TargetNaming::NewTimestamped {
            prefix: "app_staging".into(),
        },
        options: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
    }
}

fn backup_only(plan_id: Uuid, cron: &str) -> ScheduleCreate {
    ScheduleCreate {
        name: "nightly backup".into(),
        plan_id,
        dest_profile_id: None,
        cron: cron.parse().unwrap(),
        timezone: ScheduleTimezone::Local,
        action: action(None),
        webhook_url: None,
        notify: NotifyPolicy::OnFailure,
        catch_up: false,
        enabled: true,
    }
}

// ── The migration ───────────────────────────────────────────────────────

#[tokio::test]
async fn migration_0002_applies_on_top_of_0001() {
    // 0001 created a placeholder schedules table; 0002 alters it. If the two
    // disagreed, every query below would fail with "no such column".
    let (store, _dir) = store().await;
    assert!(store.list_schedules().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_store_reopens_without_reapplying_migrations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let first = Store::open(&path).await.unwrap();
    let (plan_id, _) = plan_and_profiles(&first).await;
    first
        .create_schedule(backup_only(plan_id, "0 3 * * *"))
        .await
        .unwrap();
    first.close().await;

    let second = Store::open(&path).await.unwrap();
    assert_eq!(second.list_schedules().await.unwrap().len(), 1);
}

// ── Round-tripping ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_schedule_survives_a_round_trip_intact() {
    let (store, _dir) = store().await;
    let (plan_id, dest_id) = plan_and_profiles(&store).await;

    let created = store
        .create_schedule(ScheduleCreate {
            name: "staging refresh".into(),
            plan_id,
            dest_profile_id: Some(dest_id),
            cron: "30 2 * * 1-5".parse().unwrap(),
            timezone: ScheduleTimezone::Utc,
            action: action(Some(safe_restore())),
            webhook_url: Some("https://hooks.example.com/abc".into()),
            notify: NotifyPolicy::Always,
            catch_up: true,
            enabled: true,
        })
        .await
        .unwrap();

    let loaded = store.get_schedule(created.id).await.unwrap().unwrap();
    assert_eq!(loaded, created, "every field must survive storage");

    // Spot-check the parts that go through JSON or a string column.
    assert_eq!(loaded.cron.as_str(), "30 2 * * 1-5");
    assert_eq!(loaded.timezone, ScheduleTimezone::Utc);
    assert_eq!(loaded.notify, NotifyPolicy::Always);
    assert!(loaded.catch_up);
    assert_eq!(
        loaded.action.retention,
        Some(RetentionPolicy {
            keep_last: Some(7),
            max_age_days: Some(30)
        })
    );
    assert_eq!(loaded.action.restore.unwrap().naming, safe_restore().naming);
}

#[tokio::test]
async fn a_shorthand_expression_is_stored_as_the_user_wrote_it() {
    // Storing the expansion would mean the form redisplays "0 0 * * *" after a
    // save, which reads as the app having changed what the user typed.
    let (store, _dir) = store().await;
    let (plan_id, _) = plan_and_profiles(&store).await;

    let created = store
        .create_schedule(backup_only(plan_id, "@daily"))
        .await
        .unwrap();
    let loaded = store.get_schedule(created.id).await.unwrap().unwrap();
    assert_eq!(loaded.cron.as_str(), "@daily");
}

#[tokio::test]
async fn a_backup_only_schedule_has_no_destination() {
    let (store, _dir) = store().await;
    let (plan_id, _) = plan_and_profiles(&store).await;

    let created = store
        .create_schedule(backup_only(plan_id, "0 3 * * *"))
        .await
        .unwrap();

    assert!(!created.is_sync());
    assert!(created.dest_profile_id.is_none());
    assert!(created.action.restore.is_none());
}

// ── Safety, enforced by the store ───────────────────────────────────────

#[tokio::test]
async fn a_destructive_schedule_cannot_be_created() {
    // The check has to live here, not in the UI: the CLI writes to the same
    // table, and a standing instruction to drop a database on a timer must be
    // impossible to persist by any route.
    let (store, _dir) = store().await;
    let (plan_id, dest_id) = plan_and_profiles(&store).await;

    let result = store
        .create_schedule(ScheduleCreate {
            name: "dangerous".into(),
            plan_id,
            dest_profile_id: Some(dest_id),
            cron: "0 3 * * *".parse().unwrap(),
            timezone: ScheduleTimezone::Local,
            action: action(Some(ScheduleRestore {
                naming: TargetNaming::DropAndRecreate {
                    name: "production".into(),
                },
                options: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            })),
            webhook_url: None,
            notify: NotifyPolicy::OnFailure,
            catch_up: false,
            enabled: true,
        })
        .await;

    assert!(matches!(
        result,
        Err(StoreError::InvalidSchedule(
            ScheduleError::DestructiveTarget(_)
        ))
    ));
    assert!(
        store.list_schedules().await.unwrap().is_empty(),
        "nothing may be written when validation fails"
    );
}

#[tokio::test]
async fn a_schedule_cannot_be_edited_into_a_destructive_one() {
    // The gap a create-time-only check would leave: make it safe, save it,
    // then edit the naming strategy.
    let (store, _dir) = store().await;
    let (plan_id, dest_id) = plan_and_profiles(&store).await;

    let created = store
        .create_schedule(ScheduleCreate {
            name: "staging refresh".into(),
            plan_id,
            dest_profile_id: Some(dest_id),
            cron: "0 3 * * *".parse().unwrap(),
            timezone: ScheduleTimezone::Local,
            action: action(Some(safe_restore())),
            webhook_url: None,
            notify: NotifyPolicy::OnFailure,
            catch_up: false,
            enabled: true,
        })
        .await
        .unwrap();

    let result = store
        .update_schedule(
            created.id,
            ScheduleUpdate {
                action: Some(action(Some(ScheduleRestore {
                    naming: TargetNaming::DropAndRecreate {
                        name: "production".into(),
                    },
                    options: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
                }))),
                ..Default::default()
            },
        )
        .await;

    assert!(matches!(
        result,
        Err(StoreError::InvalidSchedule(
            ScheduleError::DestructiveTarget(_)
        ))
    ));

    let unchanged = store.get_schedule(created.id).await.unwrap().unwrap();
    assert_eq!(
        unchanged.action.restore.unwrap().naming,
        safe_restore().naming
    );
}

#[tokio::test]
async fn a_bad_webhook_url_is_refused_at_save_time() {
    let (store, _dir) = store().await;
    let (plan_id, _) = plan_and_profiles(&store).await;

    let mut input = backup_only(plan_id, "0 3 * * *");
    input.webhook_url = Some("hooks.example.com/no-scheme".into());

    assert!(matches!(
        store.create_schedule(input).await,
        Err(StoreError::InvalidSchedule(ScheduleError::BadWebhook(..)))
    ));
}

// ── Updating ────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_update_leaves_absent_fields_alone() {
    let (store, _dir) = store().await;
    let (plan_id, _) = plan_and_profiles(&store).await;

    let created = store
        .create_schedule(backup_only(plan_id, "0 3 * * *"))
        .await
        .unwrap();

    let updated = store
        .update_schedule(
            created.id,
            ScheduleUpdate {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(!updated.enabled);
    assert_eq!(updated.name, created.name, "name must be untouched");
    assert_eq!(updated.cron, created.cron);
    assert_eq!(updated.action, created.action);
}

#[tokio::test]
async fn a_webhook_can_be_cleared_as_well_as_changed() {
    // The distinction `Option<Option<T>>` exists for: "leave it alone" and
    // "remove it" must not be the same request.
    let (store, _dir) = store().await;
    let (plan_id, _) = plan_and_profiles(&store).await;

    let mut input = backup_only(plan_id, "0 3 * * *");
    input.webhook_url = Some("https://hooks.example.com/abc".into());
    let created = store.create_schedule(input).await.unwrap();

    // Absent: untouched.
    let untouched = store
        .update_schedule(created.id, ScheduleUpdate::default())
        .await
        .unwrap();
    assert_eq!(
        untouched.webhook_url.as_deref(),
        Some("https://hooks.example.com/abc")
    );

    // Some(None): cleared.
    let cleared = store
        .update_schedule(
            created.id,
            ScheduleUpdate {
                webhook_url: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(cleared.webhook_url.is_none());
}

#[tokio::test]
async fn changing_the_cron_expression_takes_effect() {
    let (store, _dir) = store().await;
    let (plan_id, _) = plan_and_profiles(&store).await;

    let created = store
        .create_schedule(backup_only(plan_id, "0 3 * * *"))
        .await
        .unwrap();

    let updated = store
        .update_schedule(
            created.id,
            ScheduleUpdate {
                cron: Some("*/15 * * * *".parse().unwrap()),
                timezone: Some(ScheduleTimezone::Utc),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.cron.as_str(), "*/15 * * * *");
    assert_eq!(updated.timezone, ScheduleTimezone::Utc);

    let reloaded = store.get_schedule(created.id).await.unwrap().unwrap();
    assert_eq!(reloaded.cron.as_str(), "*/15 * * * *");
}

// ── Run bookkeeping ─────────────────────────────────────────────────────

#[tokio::test]
async fn marking_a_run_records_the_occurrence_not_the_start_time() {
    // Stamping "now" would push the high-water mark past the occurrence, and a
    // schedule finer than the lateness would then skip its next run.
    let (store, _dir) = store().await;
    let (plan_id, _) = plan_and_profiles(&store).await;

    let created = store
        .create_schedule(backup_only(plan_id, "*/5 * * * *"))
        .await
        .unwrap();
    assert!(created.last_run_at.is_none());

    let occurrence = Utc::now() - Duration::seconds(40);
    let job_id = Uuid::new_v4();
    store
        .mark_schedule_started(created.id, occurrence, job_id)
        .await
        .unwrap();

    let started = store.get_schedule(created.id).await.unwrap().unwrap();
    assert_eq!(
        started.last_run_at.unwrap().timestamp(),
        occurrence.timestamp()
    );
    assert_eq!(started.last_job_id, Some(job_id));
    assert!(
        started.last_outcome.is_none(),
        "a run in flight has no outcome yet"
    );

    store
        .mark_schedule_finished(created.id, JobOutcome::Success)
        .await
        .unwrap();

    let finished = store.get_schedule(created.id).await.unwrap().unwrap();
    assert_eq!(finished.last_outcome, Some(JobOutcome::Success));
}

#[tokio::test]
async fn a_new_run_clears_the_previous_outcome() {
    // Otherwise a running job would still display last night's green tick.
    let (store, _dir) = store().await;
    let (plan_id, _) = plan_and_profiles(&store).await;

    let created = store
        .create_schedule(backup_only(plan_id, "0 3 * * *"))
        .await
        .unwrap();

    store
        .mark_schedule_started(created.id, Utc::now(), Uuid::new_v4())
        .await
        .unwrap();
    store
        .mark_schedule_finished(created.id, JobOutcome::Failed)
        .await
        .unwrap();
    store
        .mark_schedule_started(created.id, Utc::now(), Uuid::new_v4())
        .await
        .unwrap();

    let s = store.get_schedule(created.id).await.unwrap().unwrap();
    assert!(s.last_outcome.is_none());
}

#[tokio::test]
async fn a_stored_schedule_becomes_due_after_its_recorded_run() {
    // The full loop through storage: fire, record, and the same occurrence
    // must not fire again.
    let (store, _dir) = store().await;
    let (plan_id, _) = plan_and_profiles(&store).await;

    let mut input = backup_only(plan_id, "*/5 * * * *");
    input.timezone = ScheduleTimezone::Utc;
    let created = store.create_schedule(input).await.unwrap();

    // Far enough ahead that an occurrence has definitely passed.
    let later = created.created_at + Duration::minutes(11);
    let due = created.due_at(later, DEFAULT_GRACE + Duration::minutes(15));
    let due = due.expect("an occurrence should have passed");

    store
        .mark_schedule_started(created.id, due, Uuid::new_v4())
        .await
        .unwrap();

    let reloaded = store.get_schedule(created.id).await.unwrap().unwrap();
    assert!(
        reloaded
            .due_at(later, DEFAULT_GRACE + Duration::minutes(15))
            .is_none(),
        "the recorded occurrence must not fire a second time"
    );
}

// ── Listing and deletion ────────────────────────────────────────────────

#[tokio::test]
async fn only_enabled_schedules_are_offered_to_the_scheduler() {
    let (store, _dir) = store().await;
    let (plan_id, _) = plan_and_profiles(&store).await;

    let mut on = backup_only(plan_id, "0 3 * * *");
    on.name = "on".into();
    store.create_schedule(on).await.unwrap();

    let mut off = backup_only(plan_id, "0 4 * * *");
    off.name = "off".into();
    off.enabled = false;
    store.create_schedule(off).await.unwrap();

    assert_eq!(store.list_schedules().await.unwrap().len(), 2);

    let enabled = store.list_enabled_schedules().await.unwrap();
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].name, "on");
}

#[tokio::test]
async fn deleting_a_plan_deletes_the_schedules_that_use_it() {
    // A schedule whose plan is gone can never run, and leaving it to fail
    // nightly would be noise rather than information. The plan is a hard
    // dependency, unlike the destination profile.
    let (store, _dir) = store().await;
    let (plan_id, _) = plan_and_profiles(&store).await;

    store
        .create_schedule(backup_only(plan_id, "0 3 * * *"))
        .await
        .unwrap();
    assert_eq!(store.list_schedules().await.unwrap().len(), 1);

    assert!(store.delete_sync_plan(plan_id).await.unwrap());
    assert!(
        store.list_schedules().await.unwrap().is_empty(),
        "the cascade must reach schedules"
    );
}

#[tokio::test]
async fn deleting_a_destination_profile_leaves_the_schedule_in_place() {
    // Deliberate: the schedule must survive and fail loudly at its next run.
    // ON DELETE SET NULL would silently downgrade a replication job to a local
    // backup, and nobody would find out until they needed the replica.
    let (store, _dir) = store().await;
    let (plan_id, dest_id) = plan_and_profiles(&store).await;

    let created = store
        .create_schedule(ScheduleCreate {
            name: "staging refresh".into(),
            plan_id,
            dest_profile_id: Some(dest_id),
            cron: "0 3 * * *".parse().unwrap(),
            timezone: ScheduleTimezone::Local,
            action: action(Some(safe_restore())),
            webhook_url: None,
            notify: NotifyPolicy::OnFailure,
            catch_up: false,
            enabled: true,
        })
        .await
        .unwrap();

    assert!(store.delete_profile(dest_id).await.unwrap());

    let survivor = store.get_schedule(created.id).await.unwrap().unwrap();
    assert_eq!(
        survivor.dest_profile_id,
        Some(dest_id),
        "the destination must still be named so the failure can explain itself"
    );
}

#[tokio::test]
async fn deleting_a_schedule_reports_whether_it_existed() {
    let (store, _dir) = store().await;
    let (plan_id, _) = plan_and_profiles(&store).await;

    let created = store
        .create_schedule(backup_only(plan_id, "0 3 * * *"))
        .await
        .unwrap();

    assert!(store.delete_schedule(created.id).await.unwrap());
    assert!(!store.delete_schedule(created.id).await.unwrap());
    assert!(store.get_schedule(created.id).await.unwrap().is_none());
}

#[tokio::test]
async fn requiring_a_missing_schedule_names_the_id() {
    let (store, _dir) = store().await;
    let id = Uuid::new_v4();
    match store.require_schedule(id).await {
        Err(StoreError::ScheduleNotFound(got)) => assert_eq!(got, id),
        other => panic!("expected ScheduleNotFound, got {other:?}"),
    }
}
