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
