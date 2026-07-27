//! PostgreSQL backup and restore round-trip.
//!
//!     docker compose -f docker-compose.test.yml up -d --wait
//!     cargo test -p db-sync-engine --test roundtrip_pg -- --ignored

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use db_sync_engine::backup::{
    BackupRequest, CommonBackupOptions, EngineBackupOptions, PgDumpFormat, PostgresBackupOptions,
    TableSelection,
};
use db_sync_engine::job::JobContext;
use db_sync_engine::manifest::{ArtifactFormat, BackupManifest};
use db_sync_engine::ops;
use db_sync_engine::profile::{
    ConnectionProfile, DbConfig, ProfileCreate, SshAuth, SshConfig, SshEndpoint, ToolOverrides,
};
use db_sync_engine::restore::{
    EngineRestoreOptions, PostgresRestoreOptions, RestoreRequest, TargetNaming,
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

async fn profile(store: &Store, name: &str) -> ConnectionProfile {
    let profile = store
        .create_profile(ProfileCreate {
            name: name.into(),
            engine: Engine::Postgres,
            environment: EnvironmentTag::Dev,
            ssh: Some(ssh_config()),
            db: DbConfig {
                // Resolved from the SSH host, over the compose network.
                host: "postgres".into(),
                port: 5432,
                user: "dbsync".into(),
                database: Some("fixture".into()),
            },
            tool_overrides: ToolOverrides::default(),
        })
        .await
        .unwrap();

    secrets::set_secret(profile.id, SecretKind::DbPassword, "testpass").unwrap();
    profile
}

fn backup_request(output_dir: PathBuf, format: PgDumpFormat) -> BackupRequest {
    BackupRequest {
        common: CommonBackupOptions {
            database: "fixture".into(),
            selections: vec![
                TableSelection::with_data("public.users"),
                TableSelection::with_data("public.orders"),
                TableSelection::with_data("public.attachments"),
                TableSelection::with_data("public.日本語テーブル"),
                TableSelection::with_data("public.naïve_café"),
                TableSelection::with_data("public.settings"),
                TableSelection::with_data("public.order"),
                TableSelection::with_data("reporting.daily_totals"),
                // Structure travels, rows do not.
                TableSelection::schema_only("public.sessions"),
                TableSelection::schema_only("public.audit_log"),
            ],
            output_dir,
            compress: true,
            encrypt: false,
            record_row_counts: false,
        },
        engine: EngineBackupOptions::Postgres(PostgresBackupOptions {
            format,
            ..Default::default()
        }),
    }
}

/// Query the fixture server directly for assertions.
async fn query_scalar(database: &str, sql: &str) -> String {
    let out = tokio::process::Command::new("docker")
        .args([
            "exec",
            "db-sync-postgres-1",
            "psql",
            "-U",
            "dbsync",
            "-d",
            database,
            "-t",
            "-A",
            "-c",
            sql,
        ])
        .output()
        .await
        .expect("docker exec");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

async fn drop_database(name: &str) {
    let _ = query_scalar("postgres", &format!("DROP DATABASE IF EXISTS \"{name}\";")).await;
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn custom_archive_round_trip_preserves_the_data() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let p = profile(&store, "pg-rt").await;
        let _cleanup = Cleanup(vec![p.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let request = backup_request(out.path().to_path_buf(), PgDumpFormat::Custom);
        let artifact = ops::backup(&p, &request, &store, &ctx)
            .await
            .expect("backup should succeed");

        assert!(artifact.exists());
        assert!(artifact.extension().is_some_and(|e| e == "dump"));

        let manifest = BackupManifest::read(&artifact).expect("manifest");
        manifest.verify_artifact(&artifact).expect("checksum");
        assert_eq!(manifest.engine, Engine::Postgres);
        assert_eq!(manifest.format, ArtifactFormat::PgCustom);
        assert!(
            manifest.format.supports_selective_restore(),
            "custom is the default precisely because it can do this"
        );
        assert!(manifest.server_version.starts_with("18"));

        let target = ops::restore(
            &p,
            &RestoreRequest {
                artifact_path: artifact,
                naming: TargetNaming::NewTimestamped {
                    prefix: "pg_rt".into(),
                },
                engine: EngineRestoreOptions::Postgres(PostgresRestoreOptions::default()),
                verify_checksum: true,
                typed_confirmation: None,
            },
            &store,
            &ctx,
        )
        .await
        .expect("restore should succeed");

        assert!(target.starts_with("pg_rt_"));

        // Data for selected tables.
        assert_eq!(query_scalar(&target, "SELECT COUNT(*) FROM users;").await, "3");
        assert_eq!(query_scalar(&target, "SELECT COUNT(*) FROM orders;").await, "2");

        // Schema-only tables exist and are empty.
        assert_eq!(
            query_scalar(&target, "SELECT COUNT(*) FROM sessions;").await,
            "0",
            "a schema-only table must exist but carry no rows"
        );
        assert_eq!(
            query_scalar(&target, "SELECT COUNT(*) FROM audit_log;").await,
            "0"
        );

        // bytea with invalid-UTF8 bytes.
        assert_eq!(
            query_scalar(
                &target,
                "SELECT encode(payload,'hex') FROM attachments WHERE filename='binary.bin';"
            )
            .await,
            "deadbeef00ff00ffc3289f"
        );

        // Unicode identifiers, and a reserved word as a table name.
        assert_eq!(
            query_scalar(&target, "SELECT \"名前\" FROM \"日本語テーブル\" LIMIT 1;").await,
            "テスト"
        );
        assert_eq!(
            query_scalar(&target, "SELECT COUNT(*) FROM \"naïve_café\";").await,
            "2"
        );
        assert_eq!(
            query_scalar(&target, "SELECT COUNT(*) FROM \"order\";").await,
            "1"
        );

        // The non-public schema and its data.
        assert_eq!(
            query_scalar(&target, "SELECT COUNT(*) FROM reporting.daily_totals;").await,
            "1"
        );

        // The enum type came across.
        assert_eq!(
            query_scalar(
                &target,
                "SELECT COUNT(*) FROM pg_type WHERE typname='order_status';"
            )
            .await,
            "1"
        );

        // Views and functions, including the SECURITY DEFINER one.
        assert_eq!(
            query_scalar(
                &target,
                "SELECT COUNT(*) FROM information_schema.views WHERE table_schema='public';"
            )
            .await,
            "2"
        );
        assert_eq!(
            query_scalar(
                &target,
                "SELECT COUNT(*) FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace \
                 WHERE n.nspname='public' AND p.prosecdef;"
            )
            .await,
            "1"
        );

        // The deferrable foreign-key cycle restored.
        assert_eq!(
            query_scalar(&target, "SELECT head_id FROM departments WHERE id=1;").await,
            "1"
        );

        drop_database(&target).await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn plain_format_round_trips_through_psql() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let p = profile(&store, "pg-plain").await;
        let _cleanup = Cleanup(vec![p.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let request = backup_request(out.path().to_path_buf(), PgDumpFormat::Plain);
        let artifact = ops::backup(&p, &request, &store, &ctx).await.expect("backup");

        // Plain output is gzipped on the way past, like the MySQL path.
        assert!(artifact.to_string_lossy().ends_with(".sql.gz"));
        let manifest = BackupManifest::read(&artifact).expect("manifest");
        assert_eq!(manifest.format, ArtifactFormat::SqlGz);
        assert!(
            !manifest.format.supports_selective_restore(),
            "plain SQL gives up selective and parallel restore"
        );

        let target = ops::restore(
            &p,
            &RestoreRequest {
                artifact_path: artifact,
                naming: TargetNaming::NewTimestamped {
                    prefix: "pg_plain".into(),
                },
                engine: EngineRestoreOptions::Postgres(PostgresRestoreOptions::default()),
                verify_checksum: true,
                typed_confirmation: None,
            },
            &store,
            &ctx,
        )
        .await
        .expect("restore");

        assert_eq!(query_scalar(&target, "SELECT COUNT(*) FROM users;").await, "3");
        assert_eq!(
            query_scalar(&target, "SELECT COUNT(*) FROM sessions;").await,
            "0"
        );

        drop_database(&target).await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn directory_format_dumps_in_parallel() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let p = profile(&store, "pg-dir").await;
        let _cleanup = Cleanup(vec![p.id]);

        let mut request = backup_request(out.path().to_path_buf(), PgDumpFormat::Directory);
        if let EngineBackupOptions::Postgres(o) = &mut request.engine {
            o.parallel_jobs = Some(3);
        }

        let ctx = JobContext::new(Uuid::new_v4());
        let artifact = ops::backup(&p, &request, &store, &ctx).await.expect("backup");

        assert!(artifact.is_dir(), "a directory archive is a directory");
        assert!(
            artifact.join("toc.dat").exists(),
            "pg_dump writes a table of contents"
        );

        let manifest = BackupManifest::read(&artifact).expect("manifest");
        assert_eq!(manifest.format, ArtifactFormat::PgDirectory);
        assert!(manifest.size_bytes > 0, "size walks the directory");

        let target = ops::restore(
            &p,
            &RestoreRequest {
                artifact_path: artifact,
                naming: TargetNaming::NewTimestamped {
                    prefix: "pg_dir".into(),
                },
                engine: EngineRestoreOptions::Postgres(PostgresRestoreOptions {
                    parallel_jobs: Some(3),
                    ..Default::default()
                }),
                verify_checksum: true,
                typed_confirmation: None,
            },
            &store,
            &ctx,
        )
        .await
        .expect("parallel restore");

        assert_eq!(query_scalar(&target, "SELECT COUNT(*) FROM users;").await, "3");
        drop_database(&target).await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn selective_restore_loads_only_the_chosen_tables_data() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let p = profile(&store, "pg-sel").await;
        let _cleanup = Cleanup(vec![p.id]);

        let ctx = JobContext::new(Uuid::new_v4());
        let request = backup_request(out.path().to_path_buf(), PgDumpFormat::Custom);
        let artifact = ops::backup(&p, &request, &store, &ctx).await.expect("backup");

        let target = ops::restore(
            &p,
            &RestoreRequest {
                artifact_path: artifact,
                naming: TargetNaming::NewTimestamped {
                    prefix: "pg_sel".into(),
                },
                engine: EngineRestoreOptions::Postgres(PostgresRestoreOptions {
                    only_tables: vec!["public.users".into()],
                    ..Default::default()
                }),
                verify_checksum: true,
                typed_confirmation: None,
            },
            &store,
            &ctx,
        )
        .await
        .expect("selective restore");

        // The chosen table carries its rows.
        assert_eq!(query_scalar(&target, "SELECT COUNT(*) FROM users;").await, "3");

        // Every other table still exists — the full schema is always restored,
        // because a TOC entry for an index names the index, not its table.
        assert_eq!(
            query_scalar(&target, "SELECT COUNT(*) FROM orders;").await,
            "0",
            "an unselected table should be present but empty"
        );
        assert_eq!(
            query_scalar(
                &target,
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_schema='public' AND table_name='orders';"
            )
            .await,
            "1",
            "the table definition must survive selective restore"
        );

        drop_database(&target).await;
    }
}
