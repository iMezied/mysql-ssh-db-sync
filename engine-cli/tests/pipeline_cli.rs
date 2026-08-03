//! `dbsync pipeline`, driven as the binary.
//!
//!     docker compose -f docker-compose.test.yml up -d --wait
//!     cargo test -p db-sync-cli --test pipeline_cli -- --ignored
//!
//! The engine suite already proves `ops::run_pipeline` works. What only this
//! can reach is the layer above it: resolving a pipeline by a prefix of its
//! name, refusing an ambiguous one rather than guessing, the guard that stops a
//! destructive chain running headlessly with nothing typed and no standing
//! authorisation, and the exit code — which is the entire reason to run any of
//! this from cron.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use db_sync_engine::backup::{EngineBackupOptions, MysqlBackupOptions, TableSelection};
use db_sync_engine::pipeline::{ArtifactSource, PipelineCreate, PipelineStep};
use db_sync_engine::profile::{DbConfig, ProfileCreate, ToolOverrides};
use db_sync_engine::restore::{EngineRestoreOptions, MysqlRestoreOptions, TargetNaming};
use db_sync_engine::secrets::{self, SecretKind};
use db_sync_engine::ssh::{AcceptAllHostKeys, RusshTunnelProvider, SshCredentials, TunnelProvider};
use db_sync_engine::sshconn::{SshAuth, SshConfig, SshConnectionCreate, SshEndpoint};
use db_sync_engine::store::Store;
use db_sync_engine::types::{Engine, EnvironmentTag};
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

async fn containers_up() -> bool {
    tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(("127.0.0.1", SSH_PORT)),
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

fn ssh_endpoint() -> SshEndpoint {
    SshEndpoint {
        host: "127.0.0.1".into(),
        port: SSH_PORT,
        user: "tunnel".into(),
        auth: SshAuth::KeyFile {
            path: concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../tests/fixtures/ssh/id_ed25519"
            )
            .to_string(),
            passphrase_in_keychain: false,
        },
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

/// A store on disk with one connection and one pipeline, ready for the binary
/// to open with `--store`.
async fn seeded(target: &str) -> (PathBuf, tempfile::TempDir, Cleanup) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cli.db");
    let store = Store::open(&path).await.unwrap();

    let probe = RusshTunnelProvider::new(Arc::new(AcceptAllHostKeys))
        .probe(
            &SshConfig {
                endpoint: ssh_endpoint(),
                jump_host: None,
            },
            &SshCredentials::default(),
        )
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

    let ssh = store
        .create_ssh_connection(SshConnectionCreate {
            name: "fixture-ssh".into(),
            endpoint: ssh_endpoint(),
            jump_host_id: None,
        })
        .await
        .unwrap();

    let profile = store
        .create_profile(ProfileCreate {
            name: "cli-fixture".into(),
            engine: Engine::Mysql,
            environment: EnvironmentTag::Dev,
            ssh_connection_id: Some(ssh.id),
            db: DbConfig {
                host: "mysql".into(),
                port: 3306,
                user: "root".into(),
                database: Some("fixture".into()),
            },
            tool_overrides: ToolOverrides::default(),
        })
        .await
        .unwrap();
    secrets::set_secret(profile.id, SecretKind::DbPassword, "testroot").unwrap();

    store
        .create_pipeline(PipelineCreate {
            name: "refresh cli target".into(),
            steps: vec![
                PipelineStep::Backup {
                    profile_id: profile.id,
                    database: "fixture".into(),
                    plan_id: None,
                    selections: vec![
                        TableSelection::with_data("users"),
                        TableSelection::schema_only("sessions"),
                    ],
                    output_dir: None,
                    compress: true,
                    encrypt: false,
                    record_row_counts: false,
                    engine: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
                },
                PipelineStep::Restore {
                    profile_id: profile.id,
                    source: ArtifactSource::PreviousStep,
                    naming: TargetNaming::DropAndRecreate {
                        name: target.into(),
                    },
                    engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
                    verify_checksum: true,
                },
                PipelineStep::Verify { deep: false },
            ],
        })
        .await
        .unwrap();

    // A second one, so a prefix that matches both is genuinely ambiguous.
    store
        .create_pipeline(PipelineCreate {
            name: "refresh something else".into(),
            steps: vec![PipelineStep::Backup {
                profile_id: profile.id,
                database: "fixture".into(),
                plan_id: None,
                selections: Vec::new(),
                output_dir: None,
                compress: true,
                encrypt: false,
                record_row_counts: false,
                engine: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
            }],
        })
        .await
        .unwrap();

    store.close().await;
    (path, dir, Cleanup(vec![profile.id]))
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn dbsync(store: &Path, args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_dbsync"))
        .arg("--store")
        .arg(store)
        .args(args)
        .output()
        .expect("run dbsync");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

async fn mysql(sql: &str) -> String {
    let out = tokio::process::Command::new("docker")
        .args([
            "exec",
            "db-sync-mysql-1",
            "mysql",
            "-uroot",
            "-ptestroot",
            "-N",
            "-B",
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
    async fn list_and_show_describe_a_pipeline_without_running_it() {
        require_containers!();
        let (store, _dir, _cleanup) = seeded("cli_target").await;

        let listed = dbsync(&store, &["pipeline", "list"]);
        assert_eq!(listed.code, 0, "{}", listed.stderr);
        assert!(listed.stdout.contains("refresh cli target"), "{}", listed.stdout);
        assert!(
            listed.stdout.contains("replaces cli_target"),
            "the listing has to say which ones can destroy something: {}",
            listed.stdout
        );

        let shown = dbsync(&store, &["pipeline", "show", "refresh cli t"]);
        assert_eq!(shown.code, 0, "{}", shown.stderr);
        assert!(shown.stdout.contains("1. Back up fixture from cli-fixture"), "{}", shown.stdout);
        assert!(shown.stdout.contains("2. Restore into cli_target, replacing it"), "{}", shown.stdout);
        assert!(
            shown.stdout.contains("needs --confirm"),
            "show has to say what a run would need: {}",
            shown.stdout
        );
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn an_ambiguous_name_is_refused_rather_than_guessed() {
        // Guessing here could start a chain that drops a database.
        require_containers!();
        let (store, _dir, _cleanup) = seeded("amb_target").await;

        let run = dbsync(&store, &["pipeline", "show", "refresh"]);
        assert_ne!(run.code, 0);
        assert!(
            run.stderr.contains("matches several pipelines"),
            "and it must list them: {}",
            run.stderr
        );

        let missing = dbsync(&store, &["pipeline", "show", "nothing-like-this"]);
        assert_ne!(missing.code, 0);
        assert!(missing.stderr.contains("no pipeline matches"), "{}", missing.stderr);
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_destructive_run_with_nothing_typed_is_refused_and_names_both_ways_out() {
        require_containers!();
        let (store, _dir, _cleanup) = seeded("guard_target").await;
        mysql("DROP DATABASE IF EXISTS `guard_target`").await;
        mysql("CREATE DATABASE `guard_target`").await;
        mysql("CREATE TABLE `guard_target`.keep_me (id INT)").await;

        let run = dbsync(&store, &["pipeline", "run", "refresh cli"]);
        assert_ne!(run.code, 0, "a headless drop with nothing typed must not run");
        assert!(run.stderr.contains("guard_target"), "{}", run.stderr);
        assert!(
            run.stderr.contains("--confirm") && run.stderr.contains("arm it in"),
            "the refusal has to name both ways out: {}",
            run.stderr
        );

        assert_eq!(
            mysql(
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_schema='guard_target' AND table_name='keep_me'"
            )
            .await,
            "1",
            "the database must be exactly as it was"
        );
        mysql("DROP DATABASE IF EXISTS `guard_target`").await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_confirmed_run_replaces_the_target_and_exits_zero() {
        require_containers!();
        let (store, _dir, _cleanup) = seeded("cli_run_target").await;
        let out = tempfile::tempdir().unwrap();

        mysql("DROP DATABASE IF EXISTS `cli_run_target`").await;
        mysql("CREATE DATABASE `cli_run_target`").await;
        mysql("CREATE TABLE `cli_run_target`.leftover (id INT)").await;

        let run = dbsync(
            &store,
            &[
                "pipeline",
                "run",
                "refresh cli",
                "--confirm",
                "cli_run_target",
                "--dir",
                out.path().to_str().unwrap(),
            ],
        );
        assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);

        // The step breakdown is the run's human output.
        assert!(run.stderr.contains("1. Back up fixture"), "{}", run.stderr);
        assert!(run.stderr.contains("success"), "{}", run.stderr);

        assert_eq!(
            mysql("SELECT COUNT(*) FROM cli_run_target.users").await,
            mysql("SELECT COUNT(*) FROM fixture.users").await,
        );
        assert_eq!(
            mysql(
                "SELECT COUNT(*) FROM information_schema.tables \
                 WHERE table_schema='cli_run_target' AND table_name='leftover'"
            )
            .await,
            "0",
            "a replace must not leave the old contents behind"
        );

        mysql("DROP DATABASE IF EXISTS `cli_run_target`").await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn an_armed_pipeline_runs_headlessly_with_nothing_typed() {
        // This is the whole point of arming: cron cannot answer a prompt.
        require_containers!();
        let (store_path, _dir, _cleanup) = seeded("armed_target").await;
        let out = tempfile::tempdir().unwrap();

        mysql("DROP DATABASE IF EXISTS `armed_target`").await;
        mysql("CREATE DATABASE `armed_target`").await;

        let store = Store::open(&store_path).await.unwrap();
        let pipeline = store
            .list_pipelines()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.name == "refresh cli target")
            .unwrap();
        store
            .arm_pipeline(pipeline.id, Some("armed_target"))
            .await
            .expect("arm");
        store.close().await;

        let run = dbsync(
            &store_path,
            &[
                "pipeline",
                "run",
                "refresh cli",
                "--dir",
                out.path().to_str().unwrap(),
            ],
        );
        assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);
        assert_eq!(
            mysql("SELECT COUNT(*) FROM armed_target.users").await,
            mysql("SELECT COUNT(*) FROM fixture.users").await,
        );

        mysql("DROP DATABASE IF EXISTS `armed_target`").await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn json_mode_prints_the_outcome_as_one_object() {
        require_containers!();
        let (store, _dir, _cleanup) = seeded("json_target").await;
        let out = tempfile::tempdir().unwrap();

        mysql("DROP DATABASE IF EXISTS `json_target`").await;
        mysql("CREATE DATABASE `json_target`").await;

        let run = dbsync(
            &store,
            &[
                "--json",
                "pipeline",
                "run",
                "refresh cli",
                "--confirm",
                "json_target",
                "--dir",
                out.path().to_str().unwrap(),
            ],
        );
        assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);

        // Progress is JSON-lines and the outcome is the last value on the same
        // stream, so a log collector reading stdout gets both without needing a
        // second format. Read it as a stream of values rather than by splitting
        // on newlines — the outcome is pretty-printed and spans several.
        let values: Vec<serde_json::Value> = serde_json::Deserializer::from_str(&run.stdout)
            .into_iter::<serde_json::Value>()
            .collect::<Result<_, _>>()
            .unwrap_or_else(|e| panic!("stdout is not a JSON stream ({e}):\n{}", run.stdout));

        let outcome = values.last().expect("at least the outcome");
        assert!(
            outcome.get("databases").is_some(),
            "the last value is the outcome, carrying what it wrote: {outcome}"
        );
        assert!(
            values.len() > 1,
            "progress should have been streamed before it: {} value(s)",
            values.len()
        );

        mysql("DROP DATABASE IF EXISTS `json_target`").await;
    }
}
