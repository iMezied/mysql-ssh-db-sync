//! Cross-server sync: backup, restore, verify and retention as one job.
//!
//!     docker compose -f docker-compose.test.yml up -d --wait
//!     cargo test -p db-sync-engine --test sync -- --ignored

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use db_sync_engine::backup::{
    BackupRequest, CommonBackupOptions, EngineBackupOptions, MysqlBackupOptions, TableSelection,
};
use db_sync_engine::job::JobContext;
use db_sync_engine::mask::{MaskRule, MaskTransform};
use db_sync_engine::ops::{self, SyncRequest};
use db_sync_engine::plan::{SyncPlanCreate, parse_tables_conf};
use db_sync_engine::profile::{
    ConnectionProfile, DbConfig, ProfileCreate, SshAuth, SshConfig, SshEndpoint, ToolOverrides,
};
use db_sync_engine::restore::{EngineRestoreOptions, MysqlRestoreOptions, TargetNaming};
use db_sync_engine::retention::RetentionPolicy;
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
    (async fn $name:ident() $body:block) => {
        #[test]
        fn $name() {
            rt().block_on(async move $body);
        }
    };
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

async fn profile(
    store: &Store,
    name: &str,
    engine: Engine,
    user: &str,
    password: &str,
) -> ConnectionProfile {
    let (host, port, database) = match engine {
        Engine::Mysql => ("mysql", 3306, "fixture"),
        Engine::Postgres => ("postgres", 5432, "fixture"),
    };

    let profile = store
        .create_profile(ProfileCreate {
            name: name.into(),
            engine,
            environment: EnvironmentTag::Dev,
            ssh: Some(ssh_config()),
            db: DbConfig {
                host: host.into(),
                port,
                user: user.into(),
                database: Some(database.into()),
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
                TableSelection::with_data("departments"),
                TableSelection::with_data("employees"),
                TableSelection::schema_only("sessions"),
            ],
            output_dir,
            compress: true,
            encrypt: false,
        },
        engine: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
    }
}

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

// ── Sync plans ──────────────────────────────────────────────────────────

db_test! {
    async fn sync_plans_round_trip_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).await.unwrap();

        let profile = store
            .create_profile(ProfileCreate {
                name: "src".into(),
                engine: Engine::Mysql,
                environment: EnvironmentTag::Dev,
                ssh: None,
                db: DbConfig {
                    host: "127.0.0.1".into(),
                    port: 3306,
                    user: "root".into(),
                    database: None,
                },
                tool_overrides: ToolOverrides::default(),
            })
            .await
            .unwrap();

        let plan = store
            .create_sync_plan(SyncPlanCreate {
                profile_id: profile.id,
                name: "nightly".into(),
                database: "app".into(),
                selections: vec![
                    TableSelection::with_data("orders"),
                    TableSelection::schema_only("audit_log"),
                ],
            masking: Vec::new(),
        })
            .await
            .unwrap();

        assert_eq!(plan.revision, 1);
        assert_eq!(plan.tables_with_data(), vec!["orders"]);
        assert_eq!(plan.schema_only_tables(), vec!["audit_log"]);

        let fetched = store.get_sync_plan(plan.id).await.unwrap().unwrap();
        assert_eq!(fetched, plan, "a plan must read back identically");

        // Saving bumps the revision, so a plan that changed under a schedule
        // is visible rather than silent.
        let updated = store
            .update_sync_plan(plan.id, vec![TableSelection::with_data("orders")])
            .await
            .unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.selections.len(), 1);

        assert_eq!(store.list_sync_plans(profile.id).await.unwrap().len(), 1);
        assert!(store.delete_sync_plan(plan.id).await.unwrap());
        assert!(store.get_sync_plan(plan.id).await.unwrap().is_none());
    }
}

db_test! {
    async fn deleting_a_profile_removes_its_plans() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).await.unwrap();

        let profile = store
            .create_profile(ProfileCreate {
                name: "src".into(),
                engine: Engine::Mysql,
                environment: EnvironmentTag::Dev,
                ssh: None,
                db: DbConfig {
                    host: "127.0.0.1".into(),
                    port: 3306,
                    user: "root".into(),
                    database: None,
                },
                tool_overrides: ToolOverrides::default(),
            })
            .await
            .unwrap();

        let plan = store
            .create_sync_plan(SyncPlanCreate {
                profile_id: profile.id,
                name: "nightly".into(),
                database: "app".into(),
                selections: vec![TableSelection::with_data("orders")],
            masking: Vec::new(),
        })
            .await
            .unwrap();

        store.delete_profile(profile.id).await.unwrap();

        // The foreign key cascades; an orphaned plan would point at nothing.
        assert!(
            store.get_sync_plan(plan.id).await.unwrap().is_none(),
            "plans must not outlive their profile"
        );
    }
}

db_test! {
    async fn a_legacy_tables_conf_imports_as_a_plan() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).await.unwrap();

        let profile = store
            .create_profile(ProfileCreate {
                name: "src".into(),
                engine: Engine::Mysql,
                environment: EnvironmentTag::Dev,
                ssh: None,
                db: DbConfig {
                    host: "127.0.0.1".into(),
                    port: 3306,
                    user: "root".into(),
                    database: None,
                },
                tool_overrides: ToolOverrides::default(),
            })
            .await
            .unwrap();

        // The real file shipped with the Bash tool looks like this.
        let conf = "# Orders\norders\norder_items\n\n# Users\nusers\n";
        let plan = store
            .create_sync_plan(SyncPlanCreate {
                profile_id: profile.id,
                name: "imported".into(),
                database: "app".into(),
                selections: parse_tables_conf(conf),
            masking: Vec::new(),
        })
            .await
            .unwrap();

        assert_eq!(plan.tables_with_data(), vec!["orders", "order_items", "users"]);
    }
}

// ── The pipeline ────────────────────────────────────────────────────────

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn sync_backs_up_restores_and_verifies_in_one_job() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "sy-src", Engine::Mysql, "root", "testroot").await;
        let dest = profile(&store, "sy-dst", Engine::Mysql, "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let request = SyncRequest {
            backup: backup_request(out.path().to_path_buf()),
            naming: TargetNaming::NewTimestamped {
                prefix: "sy".into(),
            },
            restore: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify: true,
            deep_verify: false,
            masking: Vec::new(),
            retention: None,
            typed_confirmation: None,
        };

        let outcome = ops::sync(&source, &dest, &request, &store, &ctx)
            .await
            .expect("sync should succeed");

        assert!(outcome.target_database.starts_with("sy_"));
        assert!(PathBuf::from(&outcome.artifact_path).exists());

        let report = outcome.verification.expect("verification was requested");
        assert!(
            report.passed(),
            "verification should pass:\n{}",
            report.to_markdown()
        );

        // And the data really is there.
        assert_eq!(
            query_scalar(&outcome.target_database, "SELECT COUNT(*) FROM users;").await,
            "3"
        );
        assert_eq!(
            query_scalar(&outcome.target_database, "SELECT COUNT(*) FROM sessions;").await,
            "0",
            "schema-only tables arrive empty"
        );

        let _ = query_scalar(
            "mysql",
            &format!("DROP DATABASE IF EXISTS `{}`;", outcome.target_database),
        )
        .await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn sync_applies_retention_after_verifying() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "rt-src", Engine::Mysql, "root", "testroot").await;
        let dest = profile(&store, "rt-dst", Engine::Mysql, "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        // Two older artifacts already sitting in the directory.
        for name in ["old_a.sql.gz", "old_b.sql.gz"] {
            std::fs::write(out.path().join(name), b"stale").unwrap();
        }

        let ctx = JobContext::new(Uuid::new_v4());
        let request = SyncRequest {
            backup: backup_request(out.path().to_path_buf()),
            naming: TargetNaming::NewTimestamped {
                prefix: "rt".into(),
            },
            restore: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify: true,
            deep_verify: false,
            masking: Vec::new(),
            retention: Some(RetentionPolicy {
                keep_last: Some(1),
                max_age_days: None,
            }),
            typed_confirmation: None,
        };

        let outcome = ops::sync(&source, &dest, &request, &store, &ctx)
            .await
            .expect("sync should succeed");

        assert_eq!(
            outcome.removed_artifacts.len(),
            2,
            "keep_last=1 should remove both stale artifacts"
        );

        // The artifact this run just produced must be the survivor.
        let remaining: Vec<String> = std::fs::read_dir(out.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".sql.gz"))
            .collect();
        assert_eq!(remaining.len(), 1, "got {remaining:?}");
        assert!(outcome.artifact_path.ends_with(&remaining[0]));

        let _ = query_scalar(
            "mysql",
            &format!("DROP DATABASE IF EXISTS `{}`;", outcome.target_database),
        )
        .await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn syncing_across_engines_is_refused_before_anything_runs() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "xe-src", Engine::Mysql, "root", "testroot").await;
        let dest = profile(&store, "xe-dst", Engine::Postgres, "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let request = SyncRequest {
            backup: backup_request(out.path().to_path_buf()),
            naming: TargetNaming::NewTimestamped {
                prefix: "xe".into(),
            },
            restore: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify: false,
            deep_verify: false,
            masking: Vec::new(),
            retention: None,
            typed_confirmation: None,
        };

        let err = ops::sync(&source, &dest, &request, &store, &ctx)
            .await
            .expect_err("MySQL to PostgreSQL is a migration, not a copy");

        assert!(
            format!("{err}").contains("cannot sync"),
            "the error should say why, got: {err}"
        );

        // Nothing should have been dumped.
        let artifacts: Vec<_> = std::fs::read_dir(out.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".sql.gz"))
            .collect();
        assert!(
            artifacts.is_empty(),
            "the mismatch must be caught before any work is done"
        );
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_destructive_target_needs_typed_confirmation() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "tc-src", Engine::Mysql, "root", "testroot").await;
        let dest = profile(&store, "tc-dst", Engine::Mysql, "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let request = SyncRequest {
            backup: backup_request(out.path().to_path_buf()),
            naming: TargetNaming::DropAndRecreate {
                name: "tc_target".into(),
            },
            restore: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify: false,
            deep_verify: false,
            masking: Vec::new(),
            retention: None,
            typed_confirmation: None,
        };

        let err = ops::sync(&source, &dest, &request, &store, &ctx)
            .await
            .expect_err("dropping a database must require confirmation");
        assert!(format!("{err}").contains("type its name"), "got: {err}");
    }
}

// ── Masking ─────────────────────────────────────────────────────────────

/// Decompress an artifact and return its text, for asserting on what the
/// backup file actually contains.
async fn artifact_text(path: &str) -> String {
    let out = tokio::process::Command::new("gzip")
        .args(["-dc", path])
        .output()
        .await
        .expect("gunzip the artifact");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Databases on the destination whose name starts with `prefix`.
async fn databases_starting_with(prefix: &str) -> Vec<String> {
    let listed = query_scalar(
        "mysql",
        &format!(
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
             WHERE SCHEMA_NAME LIKE '{prefix}%';"
        ),
    )
    .await;
    listed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn masking_rewrites_the_destination_but_never_the_artifact() {
        // The documented limitation, asserted rather than trusted: the backup
        // file is exactly as sensitive as the source. If this ever starts
        // failing because the artifact *is* masked, the README and the module
        // docs are wrong and have to change with it.
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "mk-src", Engine::Mysql, "root", "testroot").await;
        let dest = profile(&store, "mk-dst", Engine::Mysql, "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let request = SyncRequest {
            backup: backup_request(out.path().to_path_buf()),
            naming: TargetNaming::NewTimestamped { prefix: "mkok".into() },
            restore: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify: true,
            deep_verify: true,
            masking: vec![MaskRule::email("users", "email")],
            retention: None,
            typed_confirmation: None,
        };

        let outcome = ops::sync(&source, &dest, &request, &store, &ctx)
            .await
            .expect("a masked sync should succeed");

        // The destination is masked.
        let remaining = query_scalar(
            &outcome.target_database,
            "SELECT COUNT(*) FROM users WHERE email NOT LIKE '%@example.invalid';",
        )
        .await;
        assert_eq!(remaining, "0", "every address should have been rewritten");
        assert_eq!(
            query_scalar(&outcome.target_database, "SELECT COUNT(*) FROM users;").await,
            "3",
            "masking rewrites rows, it does not remove them"
        );

        // The report says so, and only ever says so after the read-back.
        let report = outcome.masking.expect("a masked sync reports what it masked");
        assert!(report.verified);
        assert_eq!(report.columns, vec!["users.email"]);
        assert_eq!(report.rows_rewritten, 3);

        // The artifact is NOT masked. This is the cost of doing the work on
        // the destination, and it is stated everywhere it could matter.
        let dump = artifact_text(&outcome.artifact_path).await;
        assert!(
            dump.contains("ada@example.com"),
            "the artifact holds the real data; if this changed, the docs must too"
        );

        // Deep verification still passes: the masked table is recorded as not
        // compared, and every other table is compared normally.
        let verification = outcome.verification.expect("verification was requested");
        assert!(
            verification.passed(),
            "masking must not make verification fail:\n{}",
            verification.to_markdown()
        );

        let _ = query_scalar(
            "mysql",
            &format!("DROP DATABASE IF EXISTS `{}`;", outcome.target_database),
        )
        .await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_masking_failure_destroys_the_destination() {
        // The guarantee the whole feature rests on. `display_name` is NOT
        // NULL, so the UPDATE is rejected — and a database holding real
        // addresses must not survive that.
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "mf-src", Engine::Mysql, "root", "testroot").await;
        let dest = profile(&store, "mf-dst", Engine::Mysql, "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let request = SyncRequest {
            backup: backup_request(out.path().to_path_buf()),
            naming: TargetNaming::NewTimestamped { prefix: "mkfail".into() },
            restore: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify: true,
            deep_verify: false,
            masking: vec![MaskRule {
                table: "users".into(),
                column: "display_name".into(),
                transform: MaskTransform::Null,
            }],
            retention: None,
            typed_confirmation: None,
        };

        let err = ops::sync(&source, &dest, &request, &store, &ctx)
            .await
            .expect_err("NULLing a NOT NULL column cannot succeed");
        assert!(
            !matches!(err, ops::OpError::UnmaskedDataLeftBehind { .. }),
            "the drop itself must have worked: {err}"
        );

        let survivors = databases_starting_with("mkfail").await;
        assert!(
            survivors.is_empty(),
            "a half-masked database was left standing: {survivors:?}"
        );
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_rule_naming_a_missing_column_fails_before_anything_is_dumped()  {
        // The check exists to be cheap. If it ran after the backup, the remedy
        // would be dropping a database somebody may already be connected to.
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "mp-src", Engine::Mysql, "root", "testroot").await;
        let dest = profile(&store, "mp-dst", Engine::Mysql, "dbsync", "testpass").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let request = SyncRequest {
            backup: backup_request(out.path().to_path_buf()),
            naming: TargetNaming::NewTimestamped { prefix: "mkpre".into() },
            restore: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify: true,
            deep_verify: false,
            // The real column is `email`.
            masking: vec![MaskRule::email("users", "email_address")],
            retention: None,
            typed_confirmation: None,
        };

        let err = ops::sync(&source, &dest, &request, &store, &ctx)
            .await
            .expect_err("a rule that protects nothing must not run");
        assert!(
            err.to_string().contains("email_address"),
            "the error should name the rule: {err}"
        );

        let written: Vec<_> = std::fs::read_dir(out.path()).unwrap().collect();
        assert!(
            written.is_empty(),
            "the check must run before the dump, not after: {written:?}"
        );
        assert!(
            databases_starting_with("mkpre").await.is_empty(),
            "and nothing should have been restored"
        );
    }
}
