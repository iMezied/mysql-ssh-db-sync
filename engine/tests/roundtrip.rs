//! MySQL backup and restore round-trip.
//!
//! The point of the whole project, exercised end to end: dump the fixture
//! through an SSH tunnel, restore it into a fresh database *as a user without
//! SUPER*, and confirm the data came back intact.
//!
//!     docker compose -f docker-compose.test.yml up -d --wait
//!
//! Needs the OS keychain (the engine reads the database password from it), so
//! these are `#[ignore]`d like the other credential-touching suites:
//!
//!     cargo test -p db-sync-engine --test roundtrip -- --ignored

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use db_sync_engine::backup::{
    BackupRequest, CommonBackupOptions, EngineBackupOptions, MysqlBackupOptions, TableSelection,
};
use db_sync_engine::events::JobKind;
use db_sync_engine::job::{JobContext, JobOutcome};
use db_sync_engine::manifest::BackupManifest;
use db_sync_engine::ops;
use db_sync_engine::profile::{
    ConnectionProfile, DbConfig, ProfileCreate, SshAuth, SshConfig, SshEndpoint, ToolOverrides,
};
use db_sync_engine::restore::{
    EngineRestoreOptions, MysqlRestoreOptions, RestoreRequest, TargetNaming,
};
use db_sync_engine::secrets::{self, SecretKind};
use db_sync_engine::ssh::{AcceptAllHostKeys, RusshTunnelProvider, SshCredentials, TunnelProvider};
use db_sync_engine::store::Store;
use db_sync_engine::types::{Engine, EnvironmentTag};
use tokio::net::TcpStream;
use uuid::Uuid;

const SSH_PORT: u16 = 12222;

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    })
}

macro_rules! db_test {
    (#[ignore = $reason:literal] async fn $name:ident() $body:block) => {
        #[test]
        #[ignore = $reason]
        fn $name() {
            rt().block_on(async move $body);
        }
    };
}

fn key_path() -> String {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/fixtures/ssh/id_ed25519"
    )
    .to_string()
}

async fn containers_up() -> bool {
    tokio::time::timeout(
        Duration::from_secs(2),
        TcpStream::connect(("127.0.0.1", SSH_PORT)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

macro_rules! require_containers {
    () => {
        if !containers_up().await {
            if std::env::var("DBSYNC_REQUIRE_CONTAINERS").is_ok() {
                panic!("test containers are required but not reachable");
            }
            eprintln!("skipping: test containers not running");
            return;
        }
    };
}

fn ssh_config() -> SshConfig {
    SshConfig {
        endpoint: SshEndpoint {
            host: "127.0.0.1".into(),
            port: SSH_PORT,
            user: "tunnel".into(),
            auth: SshAuth::KeyFile {
                path: key_path(),
                passphrase_in_keychain: false,
            },
        },
        jump_host: None,
    }
}

/// Purges keychain entries even when a test fails.
struct Cleanup(Vec<Uuid>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for id in &self.0 {
            let _ = secrets::delete_all_for_profile(*id);
        }
    }
}

/// Redirect app-scoped secrets to a disposable scope, and remove them after.
///
/// Without this, `backupkey::ensure_exists` writes a real backup key into the
/// developer's own login keychain at the fixed app scope and leaves it there —
/// and on a machine that already has one, the test would encrypt its fixtures
/// to the developer's actual key rather than to one of its own.
/// The override is an environment variable, and those are per-*process*, not
/// per-thread — so two of these alive at once would silently share, and
/// whichever finished first would pull the scope out from under the other.
/// (That is not hypothetical: it is what happened the first time this existed
/// without the lock, and it surfaced as an artifact that would not decrypt.)
/// Holding a lock for the life of the guard makes the key-touching tests run
/// one at a time; there are three of them and they are container-bound anyway.
static KEY_SCOPE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct ScopedBackupKey {
    id: Uuid,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl ScopedBackupKey {
    fn new() -> Self {
        let guard = KEY_SCOPE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let id = Uuid::new_v4();
        // Safe: nothing else in this process reads the variable while the lock
        // is held, which is the whole point of holding it.
        unsafe { std::env::set_var(secrets::APP_SCOPE_OVERRIDE, id.to_string()) };
        Self { id, _guard: guard }
    }
}

impl Drop for ScopedBackupKey {
    fn drop(&mut self) {
        // NOT `delete_all_for_profile`: it deliberately skips BackupKey, so it
        // would leave exactly the entry this guard exists to remove. Writing an
        // empty value is how the secrets layer deletes.
        let _ = secrets::set_secret(self.id, SecretKind::BackupKey, "");
        unsafe { std::env::remove_var(secrets::APP_SCOPE_OVERRIDE) };
    }
}

async fn temp_store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("t.db")).await.unwrap();

    // Pin the fixture server's host key, as the UI does once the user confirms.
    let probe = RusshTunnelProvider::new(Arc::new(AcceptAllHostKeys))
        .probe(&ssh_config(), &SshCredentials::default())
        .await
        .expect("probe");
    store
        .remember_host(
            &format!("127.0.0.1:{SSH_PORT}"),
            &probe.host_key.algorithm,
            &probe.host_key.fingerprint,
        )
        .await
        .unwrap();

    (store, dir)
}

/// `user` is deliberately a parameter: the restore half runs as `dbsync`, which
/// has no SUPER, to prove DEFINER stripping works in the real pipeline.
async fn profile(store: &Store, name: &str, user: &str, password: &str) -> ConnectionProfile {
    let profile = store
        .create_profile(ProfileCreate {
            name: name.into(),
            engine: Engine::Mysql,
            environment: EnvironmentTag::Dev,
            ssh: Some(ssh_config()),
            db: DbConfig {
                // Resolved from the SSH host, via the compose network.
                host: "mysql".into(),
                port: 3306,
                user: user.into(),
                database: Some("fixture".into()),
            },
            tool_overrides: ToolOverrides::default(),
        })
        .await
        .unwrap();

    secrets::set_secret(profile.id, SecretKind::DbPassword, password).unwrap();
    profile
}

fn backup_request(output_dir: PathBuf) -> BackupRequest {
    BackupRequest {
        common: CommonBackupOptions {
            database: "fixture".into(),
            selections: vec![
                TableSelection::with_data("users"),
                TableSelection::with_data("orders"),
                TableSelection::with_data("attachments"),
                TableSelection::with_data("日本語テーブル"),
                TableSelection::with_data("naïve_café"),
                TableSelection::with_data("settings"),
                TableSelection::with_data("audit_log"),
                TableSelection::with_data("departments"),
                TableSelection::with_data("employees"),
                // Present in the dump, but deliberately without rows.
                TableSelection::schema_only("sessions"),
                TableSelection::schema_only("payments"),
            ],
            output_dir,
            compress: true,
            encrypt: false,
        },
        engine: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
    }
}

/// Run a raw query against the fixture server for assertions.
async fn query_scalar(database: &str, sql: &str) -> String {
    let out = tokio::process::Command::new("docker")
        .args([
            "exec",
            "db-sync-mysql-1",
            "mysql",
            "-uroot",
            "-ptestroot",
            "--default-character-set=utf8mb4",
            "-N",
            "-B",
            database,
            "-e",
            sql,
        ])
        .output()
        .await
        .expect("docker exec");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn backup_then_restore_preserves_the_data() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "rt-source", "root", "testroot").await;
        // The destination user has no SUPER: a dump that still carried DEFINER
        // clauses would be rejected outright.
        let dest = profile(&store, "rt-dest", "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        // ── Backup ──────────────────────────────────────────────────────
        let ctx = JobContext::new(Uuid::new_v4());
        ops::record_start(&store, &ctx, JobKind::Backup, source.id, None, "{}".into())
            .await
            .unwrap();

        let request = backup_request(out.path().to_path_buf());
        let artifact = ops::backup(&source, &request, &store, &ctx)
            .await
            .expect("backup should succeed");

        ops::record_finish(
            &store,
            &ctx,
            JobOutcome::Success,
            Some(artifact.display().to_string()),
        )
        .await
        .unwrap();

        assert!(artifact.exists(), "artifact should be on disk");
        assert!(
            std::fs::metadata(&artifact).unwrap().len() > 0,
            "artifact should not be empty"
        );

        // The manifest must describe what was actually taken, and its checksum
        // must match — that is what a restore trusts.
        let manifest = BackupManifest::read(&artifact).expect("manifest");
        manifest.verify_artifact(&artifact).expect("checksum should match");
        assert_eq!(manifest.database, "fixture");
        assert_eq!(manifest.engine, Engine::Mysql);
        assert!(manifest.server_version.starts_with('8'));
        assert!(manifest.tables_with_data.contains(&"orders".to_string()));
        assert!(manifest.tables.contains(&"sessions".to_string()));
        assert!(!manifest.tables_with_data.contains(&"sessions".to_string()));

        // The durable log must record the work, not just the outcome.
        let recorded = store.get_job(ctx.job_id).await.unwrap().unwrap();
        assert_eq!(recorded.outcome, Some(JobOutcome::Success));
        assert!(recorded.log.contains("mysqldump"), "log should show the command");
        assert!(recorded.artifact_path.is_some());

        // ── Restore, as a user without SUPER ────────────────────────────
        let restore_ctx = JobContext::new(Uuid::new_v4());
        let restore_request = RestoreRequest {
            artifact_path: artifact.clone(),
            naming: TargetNaming::NewTimestamped {
                prefix: "rt_restore".into(),
            },
            engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify_checksum: true,
            typed_confirmation: None,
        };

        let target = ops::restore(&dest, &restore_request, &store, &restore_ctx)
            .await
            .expect("restore should succeed without SUPER");

        assert!(target.starts_with("rt_restore_"));

        // ── The data came back ──────────────────────────────────────────
        assert_eq!(query_scalar(&target, "SELECT COUNT(*) FROM users;").await, "3");
        assert_eq!(query_scalar(&target, "SELECT COUNT(*) FROM orders;").await, "2");

        // Schema-only tables exist but are empty.
        assert_eq!(
            query_scalar(&target, "SELECT COUNT(*) FROM sessions;").await,
            "0",
            "a schema-only table must be present and empty"
        );

        // Binary data survives byte-for-byte.
        assert_eq!(
            query_scalar(
                &target,
                "SELECT HEX(payload) FROM attachments WHERE filename='binary.bin';"
            )
            .await,
            "DEADBEEF00FF00FFC3289F",
            "invalid-UTF8 bytes must round-trip exactly"
        );

        // Unicode identifiers and values survive.
        assert_eq!(
            query_scalar(&target, "SELECT `名前` FROM `日本語テーブル` LIMIT 1;").await,
            "テスト"
        );
        assert_eq!(
            query_scalar(&target, "SELECT COUNT(*) FROM `naïve_café`;").await,
            "2"
        );

        // Row data containing "DEFINER=" must not have been rewritten by the
        // filter — this is the exact bug the old sed had.
        assert_eq!(
            query_scalar(
                &target,
                "SELECT `value` FROM settings WHERE `key`='definer_note';"
            )
            .await,
            "DEFINER=`root`@`localhost`"
        );

        // The foreign-key cycle restored, which needs FOREIGN_KEY_CHECKS=0.
        assert_eq!(
            query_scalar(&target, "SELECT head_id FROM departments WHERE id=1;").await,
            "1"
        );

        // Routines, views and triggers came across, without DEFINER clauses.
        assert_eq!(
            query_scalar(
                "information_schema",
                &format!("SELECT COUNT(*) FROM VIEWS WHERE TABLE_SCHEMA='{target}';")
            )
            .await,
            "2"
        );
        assert_eq!(
            query_scalar(
                "information_schema",
                &format!("SELECT COUNT(*) FROM ROUTINES WHERE ROUTINE_SCHEMA='{target}';")
            )
            .await,
            "2"
        );

        // Cleanup.
        let _ = query_scalar("mysql", &format!("DROP DATABASE IF EXISTS `{target}`;")).await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn verification_confirms_the_restore() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "vf-source", "root", "testroot").await;
        let dest = profile(&store, "vf-dest", "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let request = backup_request(out.path().to_path_buf());
        let artifact = ops::backup(&source, &request, &store, &ctx).await.unwrap();

        let target = ops::restore(
            &dest,
            &RestoreRequest {
                artifact_path: artifact,
                naming: TargetNaming::NewTimestamped {
                    prefix: "vf_restore".into(),
                },
                engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
                verify_checksum: true,
                typed_confirmation: None,
            },
            &store,
            &ctx,
        )
        .await
        .unwrap();

        let with_data: Vec<String> = request
            .common
            .tables_with_data()
            .into_iter()
            .map(|t| t.name.clone())
            .collect();
        let schema_only = vec!["sessions".to_string(), "payments".to_string()];

        let report = ops::verify_restore(
            ops::VerifyRequest {
                deep: true,
                masked_tables: &[],
                source_profile: &source,
                dest_profile: &dest,
                source_database: "fixture",
                dest_database: &target,
                tables_with_data: &with_data,
                schema_only: &schema_only,
            },
            &store,
            &ctx,
        )
        .await
        .expect("verification should run");

        assert!(
            report.passed(),
            "verification should pass, report:\n{}",
            report.to_markdown()
        );
        assert_eq!(report.failures, 0);
        assert!(report.tables_checked >= with_data.len());

        let _ = query_scalar("mysql", &format!("DROP DATABASE IF EXISTS `{target}`;")).await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_corrupted_artifact_is_refused_before_the_destination_is_touched() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "cx-source", "root", "testroot").await;
        let dest = profile(&store, "cx-dest", "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let artifact = ops::backup(&source, &backup_request(out.path().to_path_buf()), &store, &ctx)
            .await
            .unwrap();

        // Simulate a truncated copy.
        let mut bytes = std::fs::read(&artifact).unwrap();
        bytes.truncate(bytes.len() / 2);
        std::fs::write(&artifact, bytes).unwrap();

        let before = query_scalar(
            "information_schema",
            "SELECT COUNT(*) FROM SCHEMATA WHERE SCHEMA_NAME LIKE 'cx_restore%';",
        )
        .await;

        let err = ops::restore(
            &dest,
            &RestoreRequest {
                artifact_path: artifact,
                naming: TargetNaming::NewTimestamped {
                    prefix: "cx_restore".into(),
                },
                engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
                verify_checksum: true,
                typed_confirmation: None,
            },
            &store,
            &ctx,
        )
        .await
        .expect_err("a corrupt artifact must not be restored");

        assert!(
            format!("{err}").contains("checksum"),
            "the failure should name the checksum, got {err}"
        );

        let after = query_scalar(
            "information_schema",
            "SELECT COUNT(*) FROM SCHEMATA WHERE SCHEMA_NAME LIKE 'cx_restore%';",
        )
        .await;
        assert_eq!(
            before, after,
            "no destination database should have been created"
        );
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn cancelling_a_backup_stops_it_and_leaves_no_artifact() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "cn-source", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let cancel_ctx = ctx.clone();

        // Cancel almost immediately; the dump is small, so this races — either
        // outcome is acceptable as long as cancellation is honoured cleanly and
        // no half-written artifact is left behind.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel_ctx.cancel();
        });

        let result = ops::backup(
            &source,
            &backup_request(out.path().to_path_buf()),
            &store,
            &ctx,
        )
        .await;

        match result {
            Err(e) => {
                assert!(
                    format!("{e}").contains("cancel"),
                    "a cancelled backup should say so, got {e}"
                );
                let leftovers: Vec<_> = std::fs::read_dir(out.path())
                    .unwrap()
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(".sql.gz"))
                    .collect();
                assert!(
                    leftovers.is_empty(),
                    "a cancelled backup must not leave a partial artifact that looks restorable, \
                     found {leftovers:?}"
                );
            }
            Ok(artifact) => {
                // Finished before the cancel landed; it must still be valid.
                let manifest = BackupManifest::read(&artifact).expect("manifest");
                manifest.verify_artifact(&artifact).expect("checksum");
            }
        }
    }
}

// ── Scheduled runs ──────────────────────────────────────────────────────
//
// The scheduler is only worth anything if a schedule coming due produces the
// same real backup that pressing the button does. These drive it through
// `tick_once` — the exact path the running app takes — rather than calling
// `ops::sync` directly, so the due-detection, profile resolution, history
// recording and reporting are all exercised together.

/// Captures the reports the scheduler produces, so a test can assert on what
/// the user would actually have been told.
#[derive(Default)]
struct Reports(tokio::sync::Mutex<Vec<db_sync_engine::notify::RunReport>>);

struct Recorder(Arc<Reports>);

#[async_trait::async_trait]
impl db_sync_engine::scheduler::SchedulerHooks for Recorder {
    async fn run_finished(&self, report: &db_sync_engine::notify::RunReport) {
        self.0.0.lock().await.push(report.clone());
    }
}

/// Wait for the scheduler to go idle, failing rather than hanging.
async fn settle(scheduler: &db_sync_engine::scheduler::Scheduler) {
    for _ in 0..600 {
        if scheduler.in_flight_ids().await.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("a scheduled run never finished");
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_due_schedule_runs_a_real_sync_and_reports_it() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "sched-source", "root", "testroot").await;
        let dest = profile(&store, "sched-dest", "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let plan = store
            .create_sync_plan(db_sync_engine::plan::SyncPlanCreate {
                profile_id: source.id,
                name: "nightly".into(),
                database: "fixture".into(),
                selections: backup_request(out.path().to_path_buf()).common.selections,
                masking: Vec::new(),
            })
            .await
            .unwrap();

        let schedule = store
            .create_schedule(db_sync_engine::schedule::ScheduleCreate {
                name: "nightly staging refresh".into(),
                plan_id: plan.id,
                dest_profile_id: Some(dest.id),
                // Every minute, with catch-up on, so a single tick a little
                // later is guaranteed to find an occurrence.
                cron: "* * * * *".parse().unwrap(),
                timezone: db_sync_engine::cron::ScheduleTimezone::Utc,
                action: db_sync_engine::schedule::ScheduleAction {
                    output_dir: out.path().to_path_buf(),
                    compress: true,
                    encrypt: false,
                    backup: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
                    restore: Some(db_sync_engine::schedule::ScheduleRestore {
                        naming: TargetNaming::NewTimestamped {
                            prefix: "scheduled".into(),
                        },
                        options: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
                    }),
                    verify: true,
                    deep_verify: false,
                    retention: None,
                },
                webhook_url: None,
                notify: db_sync_engine::schedule::NotifyPolicy::Always,
                catch_up: true,
                enabled: true,
            })
            .await
            .unwrap();

        let reports = Arc::new(Reports::default());
        let (tx, _rx) = db_sync_engine::events::create_event_channel(
            db_sync_engine::events::EVENT_CHANNEL_CAPACITY,
        );
        let scheduler = db_sync_engine::scheduler::Scheduler::new(
            store.clone(),
            db_sync_engine::job::JobRegistry::new(),
            tx,
        )
        .with_hooks(Arc::new(Recorder(reports.clone())));

        // Two minutes on from creation, so an occurrence has definitely passed.
        scheduler
            .tick_once(schedule.created_at + chrono::Duration::minutes(2))
            .await;
        assert_eq!(
            scheduler.in_flight_ids().await.len(),
            1,
            "the tick should have started exactly one run"
        );
        settle(&scheduler).await;

        // ── What the user was told ──────────────────────────────────────
        let reports = reports.0.lock().await;
        assert_eq!(reports.len(), 1);
        let report = &reports[0];

        assert_eq!(
            report.outcome,
            JobOutcome::Success,
            "the scheduled sync failed: {:?}",
            report.error
        );
        assert_eq!(report.kind, JobKind::Sync);
        assert_eq!(report.source_profile, "sched-source");
        assert_eq!(report.dest_profile.as_deref(), Some("sched-dest"));
        assert!(
            report.artifact_bytes.is_some_and(|b| b > 0),
            "the report should carry a real artifact size"
        );

        let verification = report
            .verification
            .as_ref()
            .expect("verification was requested");
        assert!(verification.passed, "row counts did not match");
        assert!(verification.tables_checked >= 9);

        // ── What actually landed on the destination ─────────────────────
        let target = report
            .target_database
            .as_deref()
            .expect("a sync names the database it wrote");
        assert!(target.starts_with("scheduled_"), "got {target}");

        let users = query_scalar(target, "SELECT COUNT(*) FROM users").await;
        assert_ne!(users, "0", "the scheduled run restored no rows");
        assert_eq!(
            users,
            query_scalar("fixture", "SELECT COUNT(*) FROM users").await,
            "the restored row count must match the source"
        );

        // Non-ASCII table names survive the scheduled path too.
        assert_eq!(
            query_scalar(target, "SELECT COUNT(*) FROM `日本語テーブル`").await,
            query_scalar("fixture", "SELECT COUNT(*) FROM `日本語テーブル`").await
        );

        // ── What the schedule recorded ──────────────────────────────────
        let after = store.get_schedule(schedule.id).await.unwrap().unwrap();
        assert_eq!(after.last_outcome, Some(JobOutcome::Success));
        assert!(
            after.last_run_at.is_some(),
            "a scheduled run must move the high-water mark"
        );
        assert_eq!(after.last_job_id, Some(report.job_id));

        // And a second tick at the same instant must not run it again.
        scheduler
            .tick_once(schedule.created_at + chrono::Duration::minutes(2))
            .await;
        assert!(
            scheduler.in_flight_ids().await.is_empty(),
            "one occurrence must produce exactly one run"
        );

        // Leave the fixture server as we found it.
        let _ = tokio::process::Command::new("docker")
            .args([
                "exec",
                "db-sync-mysql-1",
                "mysql",
                "-uroot",
                "-ptestroot",
                "-e",
                &format!("DROP DATABASE IF EXISTS `{target}`"),
            ])
            .output()
            .await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_scheduled_backup_enforces_its_retention_policy() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "retain-source", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id]);

        let plan = store
            .create_sync_plan(db_sync_engine::plan::SyncPlanCreate {
                profile_id: source.id,
                name: "hourly".into(),
                database: "fixture".into(),
                selections: vec![TableSelection::with_data("users")],
                masking: Vec::new(),
            })
            .await
            .unwrap();

        let schedule = store
            .create_schedule(db_sync_engine::schedule::ScheduleCreate {
                name: "hourly backup".into(),
                plan_id: plan.id,
                dest_profile_id: None,
                cron: "@hourly".parse().unwrap(),
                timezone: db_sync_engine::cron::ScheduleTimezone::Utc,
                action: db_sync_engine::schedule::ScheduleAction {
                    output_dir: out.path().to_path_buf(),
                    compress: true,
                    encrypt: false,
                    backup: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
                    restore: None,
                    verify: false,
                    deep_verify: false,
                    // Keep one. The newest is never deleted, so after two runs
                    // exactly one artifact must remain.
                    retention: Some(db_sync_engine::retention::RetentionPolicy {
                        keep_last: Some(1),
                        max_age_days: None,
                    }),
                },
                webhook_url: None,
                notify: db_sync_engine::schedule::NotifyPolicy::Never,
                catch_up: false,
                enabled: true,
            })
            .await
            .unwrap();

        let (tx, _rx) = db_sync_engine::events::create_event_channel(
            db_sync_engine::events::EVENT_CHANNEL_CAPACITY,
        );
        let scheduler = db_sync_engine::scheduler::Scheduler::new(
            store.clone(),
            db_sync_engine::job::JobRegistry::new(),
            tx,
        );

        let artifacts = || {
            std::fs::read_dir(out.path())
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".sql.gz"))
                .collect::<Vec<_>>()
        };

        scheduler.run_now(schedule.id).await.unwrap().unwrap();
        settle(&scheduler).await;
        assert_eq!(artifacts().len(), 1, "the first run produced no artifact");

        // Artifact names carry a whole-second timestamp; without this the
        // second run would collide with the first rather than replace it.
        tokio::time::sleep(Duration::from_millis(1100)).await;

        scheduler.run_now(schedule.id).await.unwrap().unwrap();
        settle(&scheduler).await;

        let remaining = artifacts();
        assert_eq!(
            remaining.len(),
            1,
            "retention should have left exactly one artifact, found {remaining:?}"
        );

        let after = store.get_schedule(schedule.id).await.unwrap().unwrap();
        assert_eq!(after.last_outcome, Some(JobOutcome::Success));
        assert!(
            after.last_run_at.is_none(),
            "manual runs must not move the high-water mark"
        );
    }
}

// ── Encryption at rest ──────────────────────────────────────────────────

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn an_encrypted_backup_round_trips_and_is_unreadable_without_the_key() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "enc-source", "root", "testroot").await;
        let dest = profile(&store, "enc-dest", "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let _scope = ScopedBackupKey::new();

        // A key must exist and be escrowed before an encrypted backup runs.
        db_sync_engine::backupkey::ensure_exists(&store).await.unwrap();
        let exported = db_sync_engine::backupkey::export(&store).await.unwrap();

        let mut request = backup_request(out.path().to_path_buf());
        request.common.encrypt = true;

        let ctx = JobContext::new(Uuid::new_v4());
        let artifact = ops::backup(&source, &request, &store, &ctx)
            .await
            .expect("encrypted backup");

        // ── The artifact really is ciphertext ────────────────────────────
        assert!(
            artifact.to_string_lossy().ends_with(".sql.gz.age"),
            "the extension must say what the file is, got {}",
            artifact.display()
        );
        assert!(
            db_sync_engine::crypto::looks_encrypted(&artifact),
            "the artifact does not start with an age header"
        );

        let raw = std::fs::read(&artifact).unwrap();
        for probe in [b"CREATE TABLE".as_slice(), b"INSERT INTO".as_slice()] {
            assert!(
                !raw.windows(probe.len()).any(|w| w == probe),
                "plaintext SQL survived into an encrypted artifact"
            );
        }

        // ── The manifest names the key needed to read it ─────────────────
        let manifest = BackupManifest::read(&artifact).expect("manifest");
        assert!(manifest.encrypted);
        assert_eq!(manifest.encryption_recipients.len(), 1);
        manifest.verify_artifact(&artifact).expect("checksum covers the ciphertext");

        // ── And it restores, as a user without SUPER ─────────────────────
        let restore = RestoreRequest {
            artifact_path: artifact.clone(),
            naming: TargetNaming::NewTimestamped { prefix: "enc_restore".into() },
            engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify_checksum: true,
            typed_confirmation: None,
        };
        let target = ops::restore(&dest, &restore, &store, &ctx)
            .await
            .expect("restore of an encrypted artifact");

        assert_eq!(
            query_scalar(&target, "SELECT COUNT(*) FROM users").await,
            query_scalar("fixture", "SELECT COUNT(*) FROM users").await,
            "the decrypted restore must match the source"
        );

        // ── Without the key it is unreadable ─────────────────────────────
        // Replacing the installation key simulates the machine that made the
        // backup being gone. The artifact must refuse to restore, not restore
        // something wrong.
        let (other_secret, _other_public) = db_sync_engine::crypto::generate_identity();
        db_sync_engine::backupkey::import(
            &store,
            secrecy::ExposeSecret::expose_secret(&other_secret),
        )
        .await
        .unwrap();

        let err = ops::restore(&dest, &restore, &store, &ctx)
            .await
            .expect_err("a foreign key must not decrypt this artifact");
        let message = err.to_string();
        assert!(
            message.contains("decrypt") || message.contains("key"),
            "the failure should name the cause, got: {message}"
        );

        // Put the real key back so the keychain is left as we found it.
        db_sync_engine::backupkey::import(
            &store,
            secrecy::ExposeSecret::expose_secret(&exported),
        )
        .await
        .unwrap();

        let _ = tokio::process::Command::new("docker")
            .args([
                "exec", "db-sync-mysql-1", "mysql", "-uroot", "-ptestroot",
                "-e", &format!("DROP DATABASE IF EXISTS `{target}`"),
            ])
            .output()
            .await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn an_encrypted_backup_is_refused_until_the_key_is_escrowed() {
        // The whole reason escrow is enforced: an artifact encrypted to a key
        // nobody has a copy of is worse than no artifact at all.
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "escrow-source", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id]);

        let _scope = ScopedBackupKey::new();
        db_sync_engine::backupkey::ensure_exists(&store).await.unwrap();
        // Deliberately not exported.

        let mut request = backup_request(out.path().to_path_buf());
        request.common.encrypt = true;

        let ctx = JobContext::new(Uuid::new_v4());
        let err = ops::backup(&source, &request, &store, &ctx)
            .await
            .expect_err("an un-escrowed key must block an encrypted backup");
        assert!(
            err.to_string().contains("exported"),
            "the message must say what to do, got: {err}"
        );

        let leftovers: Vec<_> = std::fs::read_dir(out.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            leftovers.is_empty(),
            "nothing may be dumped before the check runs, found {leftovers:?}"
        );
    }
}

// ── Content verification ────────────────────────────────────────────────

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_digest_catches_corruption_that_a_row_count_misses() {
        // The case row counts are blind to, and the reason digests exist: the
        // right number of rows holding the wrong bytes. Verified against a real
        // server, by restoring correctly and then editing one value.
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "digest-source", "root", "testroot").await;
        let dest = profile(&store, "digest-dest", "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let artifact = ops::backup(&source, &backup_request(out.path().to_path_buf()), &store, &ctx)
            .await
            .expect("backup");

        let target = ops::restore(
            &dest,
            &RestoreRequest {
                artifact_path: artifact,
                naming: TargetNaming::NewTimestamped { prefix: "digest".into() },
                engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
                verify_checksum: true,
                typed_confirmation: None,
            },
            &store,
            &ctx,
        )
        .await
        .expect("restore");

        let with_data = vec!["users".to_string()];
        let verify_request = || ops::VerifyRequest {
            deep: true,
            masked_tables: &[],
            source_profile: &source,
            dest_profile: &dest,
            source_database: "fixture",
            dest_database: &target,
            tables_with_data: &with_data,
            schema_only: &[],
        };

        // A faithful restore passes both layers.
        let report = ops::verify_restore(verify_request(), &store, &ctx).await.unwrap();
        assert!(report.passed(), "a faithful restore should verify: {}", report.to_markdown());

        // Change one value without changing the row count.
        let _ = tokio::process::Command::new("docker")
            .args([
                "exec", "db-sync-mysql-1", "mysql", "-uroot", "-ptestroot",
                "--default-character-set=utf8mb4", target.as_str(),
                "-e", "UPDATE users SET email = CONCAT(email, '.tampered') LIMIT 1",
            ])
            .output()
            .await
            .expect("docker exec");

        // Row counts still agree...
        assert_eq!(
            query_scalar(&target, "SELECT COUNT(*) FROM users").await,
            query_scalar("fixture", "SELECT COUNT(*) FROM users").await,
            "the tampering must not change the row count, or this proves nothing"
        );

        // ...but the digest does not.
        let report = ops::verify_restore(verify_request(), &store, &ctx).await.unwrap();
        assert!(
            !report.passed(),
            "a single changed value must fail verification: {}",
            report.to_markdown()
        );
        assert!(
            report.to_markdown().contains("CONTENT MISMATCH"),
            "got: {}",
            report.to_markdown()
        );

        // And a shallow run over the same tampered data still passes, which is
        // exactly the gap this milestone closes.
        let mut shallow = verify_request();
        shallow.deep = false;
        let shallow_report = ops::verify_restore(shallow, &store, &ctx).await.unwrap();
        assert!(
            shallow_report.passed(),
            "row counts alone cannot see this, which is the point"
        );

        let _ = tokio::process::Command::new("docker")
            .args([
                "exec", "db-sync-mysql-1", "mysql", "-uroot", "-ptestroot",
                "-e", &format!("DROP DATABASE IF EXISTS `{target}`"),
            ])
            .output()
            .await;
    }
}

// ── Restore drills ──────────────────────────────────────────────────────

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_drill_restores_the_newest_backup_and_cleans_up_after_itself() {
        // A backup is a belief until it has been restored. This is the only
        // check that turns it into a fact.
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "drill-source", "root", "testroot").await;
        let dest = profile(&store, "drill-dest", "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        ops::backup(&source, &backup_request(out.path().to_path_buf()), &store, &ctx)
            .await
            .expect("backup");

        let outcome = ops::drill(
            &dest,
            &ops::DrillRequest {
                artifact_dir: out.path().to_path_buf(),
                restore: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
                deep_verify: true,
                keep_on_failure: false,
            },
            &store,
            &ctx,
        )
        .await
        .expect("drill");

        assert!(
            outcome.report.passed(),
            "the drill should prove the backup restores: {}",
            outcome.report.to_markdown()
        );
        assert!(
            outcome.scratch_database.starts_with("dbsync_drill_"),
            "got {}",
            outcome.scratch_database
        );
        assert!(outcome.dropped, "a passing drill must clean up after itself");

        // And it really is gone — a drill that left databases behind would
        // fill a server within a month of nightly runs.
        let remaining = query_scalar(
            "information_schema",
            &format!(
                "SELECT COUNT(*) FROM SCHEMATA WHERE SCHEMA_NAME = '{}'",
                outcome.scratch_database
            ),
        )
        .await;
        assert_eq!(remaining, "0", "the scratch database was not dropped");
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_drill_with_no_backups_says_so_rather_than_passing() {
        // An empty directory must never read as "verified".
        require_containers!();
        let (store, _dir) = temp_store().await;
        let empty = tempfile::tempdir().unwrap();

        let dest = profile(&store, "drill-empty", "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let err = ops::drill(
            &dest,
            &ops::DrillRequest {
                artifact_dir: empty.path().to_path_buf(),
                restore: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
                deep_verify: false,
                keep_on_failure: false,
            },
            &store,
            &ctx,
        )
        .await
        .expect_err("a drill with nothing to restore must fail");

        assert!(err.to_string().contains("no backups found"), "got: {err}");
    }
}

// ── Restore target pre-flight ───────────────────────────────────────────

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn restoring_into_a_database_that_does_not_exist_says_so() {
        // `IntoExisting` creates nothing, so without this check the dump
        // streams at a database that is not there and the failure arrives as
        // whatever the vendor client prints — "Unknown database", on a stderr
        // line, under a generic "exit status: 1".
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "pf-source", "root", "testroot").await;
        let dest = profile(&store, "pf-dest", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let artifact = ops::backup(&source, &backup_request(out.path().to_path_buf()), &store, &ctx)
            .await
            .expect("backup");

        let err = ops::restore(
            &dest,
            &RestoreRequest {
                artifact_path: artifact,
                naming: TargetNaming::IntoExisting {
                    name: "definitely_not_a_database".into(),
                },
                engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
                verify_checksum: true,
                typed_confirmation: None,
            },
            &store,
            &ctx,
        )
        .await
        .expect_err("there is nothing to restore into");

        let message = err.to_string();
        assert!(
            message.contains("definitely_not_a_database") && message.contains("no database called"),
            "the error must name the missing database: {message}"
        );
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_generated_name_that_already_exists_is_refused_not_merged_into() {
        // The timestamp has one-second resolution, so two restores in the same
        // second resolve to the same name. Writing into the first one's
        // database would silently merge two restores; this pins that it is
        // refused, and that the message explains a collision rather than
        // reporting a generic CREATE failure.
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "cl-source", "root", "testroot").await;
        let dest = profile(&store, "cl-dest", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let artifact = ops::backup(&source, &backup_request(out.path().to_path_buf()), &store, &ctx)
            .await
            .expect("backup");

        let naming = TargetNaming::NewTimestamped {
            prefix: "collision_probe".into(),
        };
        // Create exactly the database the next restore will generate.
        let occupied = naming.resolve(chrono::Utc::now());
        let _ = query_scalar(
            "mysql",
            &format!("CREATE DATABASE IF NOT EXISTS `{occupied}`;"),
        )
        .await;

        let err = ops::restore(
            &dest,
            &RestoreRequest {
                artifact_path: artifact,
                naming,
                engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
                verify_checksum: true,
                typed_confirmation: None,
            },
            &store,
            &ctx,
        )
        .await;

        let _ = query_scalar("mysql", &format!("DROP DATABASE IF EXISTS `{occupied}`;")).await;

        // The second can tick between resolving the name above and resolving it
        // again inside the restore, which is the same one-second granularity
        // this test is about. A restore that landed on a free name is a pass,
        // not a flake — what must never happen is silently reusing the
        // occupied one.
        match err {
            Err(e) => {
                let message = e.to_string();
                assert!(
                    message.contains("already exists"),
                    "a collision must be reported as one: {message}"
                );
            }
            Ok(target) => {
                assert_ne!(
                    target, occupied,
                    "a restore must never write into a database it did not create"
                );
                let _ = query_scalar("mysql", &format!("DROP DATABASE IF EXISTS `{target}`;")).await;
            }
        }
    }
}
