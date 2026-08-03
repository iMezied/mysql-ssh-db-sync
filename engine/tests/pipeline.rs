//! Saved chains of actions, run end to end.
//!
//!     docker compose -f docker-compose.test.yml up -d --wait
//!     cargo test -p db-sync-engine --test pipeline -- --ignored
//!
//! What these cover that the unit tests in `pipeline.rs` cannot: that the plan
//! written down before a run matches the order the runner actually executes in,
//! that state really flows from a backup step to the restore that consumes it,
//! and — the one that matters most — that a destructive step without a typed
//! confirmation is refused by the engine rather than by the page in front of
//! it. The same pattern `sync.rs` uses for `ops::sync`, and for the same
//! reason: the guarantee must not depend on the UI.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use db_sync_engine::backup::{EngineBackupOptions, MysqlBackupOptions, TableSelection};
use db_sync_engine::job::{JobContext, JobOutcome};
use db_sync_engine::ops::{self, PipelineRunRequest};
use db_sync_engine::pipeline::{ArtifactSource, Pipeline, PipelineCreate, PipelineStep};
use db_sync_engine::profile::{ConnectionProfile, DbConfig, ProfileCreate, ToolOverrides};
use db_sync_engine::restore::{EngineRestoreOptions, MysqlRestoreOptions, TargetNaming};
use db_sync_engine::secrets::{self, SecretKind};
use db_sync_engine::ssh::{AcceptAllHostKeys, RusshTunnelProvider, SshCredentials, TunnelProvider};
use db_sync_engine::sshconn::{SshAuth, SshConfig, SshConnectionCreate, SshEndpoint};
use db_sync_engine::step::{JobStepKind, JobStepOutcome};
use db_sync_engine::store::Store;
use db_sync_engine::tools::ToolSource;
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

/// Reused rather than recreated, for the reason `sync.rs` documents: a test
/// building two profiles on one store would otherwise hit the unique name index.
async fn saved_ssh(store: &Store) -> Uuid {
    if let Some(existing) = store
        .list_ssh_connections()
        .await
        .expect("list ssh connections")
        .into_iter()
        .find(|c| c.name == "fixture-ssh")
    {
        return existing.id;
    }
    store
        .create_ssh_connection(SshConnectionCreate {
            name: "fixture-ssh".into(),
            endpoint: ssh_endpoint(),
            jump_host_id: None,
        })
        .await
        .expect("save the ssh connection")
        .id
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

    (store, dir)
}

async fn profile(store: &Store, name: &str, user: &str, password: &str) -> ConnectionProfile {
    let profile = store
        .create_profile(ProfileCreate {
            name: name.into(),
            engine: Engine::Mysql,
            environment: EnvironmentTag::Dev,
            ssh_connection_id: Some(saved_ssh(store).await),
            db: DbConfig {
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

fn backup_step(source: Uuid) -> PipelineStep {
    PipelineStep::Backup {
        profile_id: source,
        database: "fixture".into(),
        plan_id: None,
        selections: vec![
            TableSelection::with_data("users"),
            TableSelection::with_data("orders"),
            TableSelection::schema_only("sessions"),
        ],
        output_dir: None,
        compress: true,
        encrypt: false,
        record_row_counts: false,
        engine: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
    }
}

fn restore_step(dest: Uuid, naming: TargetNaming) -> PipelineStep {
    PipelineStep::Restore {
        profile_id: dest,
        source: ArtifactSource::PreviousStep,
        naming,
        engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
        verify_checksum: true,
    }
}

async fn saved(store: &Store, name: &str, steps: Vec<PipelineStep>) -> Pipeline {
    store
        .create_pipeline(PipelineCreate {
            name: name.into(),
            steps,
        })
        .await
        .expect("create pipeline")
}

fn run_request(out: &tempfile::TempDir, confirmations: &[&str]) -> PipelineRunRequest {
    PipelineRunRequest {
        typed_confirmations: confirmations.iter().map(|s| s.to_string()).collect(),
        default_output_dir: out.path().to_path_buf(),
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

async fn drop_database(name: &str) {
    let _ = tokio::process::Command::new("docker")
        .args([
            "exec",
            "db-sync-mysql-1",
            "mysql",
            "-uroot",
            "-ptestroot",
            "-e",
            &format!("DROP DATABASE IF EXISTS `{name}`"),
        ])
        .output()
        .await;
}

// ── The whole chain ─────────────────────────────────────────────────────

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_pipeline_that_replaces_a_database_runs_and_records_every_step() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "pl-src", "root", "testroot").await;
        let dest = profile(&store, "pl-dst", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        // The target has to exist for a replace to be a replace rather than a
        // create, which is the case worth proving.
        drop_database("pl_target").await;
        query_scalar("mysql", "CREATE DATABASE `pl_target`").await;
        query_scalar("pl_target", "CREATE TABLE leftover (id INT)").await;

        let pipeline = saved(
            &store,
            "refresh pl_target",
            vec![
                backup_step(source.id),
                restore_step(dest.id, TargetNaming::DropAndRecreate { name: "pl_target".into() }),
                PipelineStep::Verify { deep: false },
            ],
        )
        .await;

        let ctx = JobContext::new(Uuid::new_v4());
        let outcome = ops::run_pipeline(
            &pipeline,
            &run_request(&out, &["pl_target"]),
            &store,
            &ToolSource::Local,
            &ctx,
        )
        .await
        .expect("the pipeline should run");

        assert_eq!(outcome.databases, vec!["pl_target".to_string()]);
        assert_eq!(outcome.artifacts.len(), 1);
        assert!(PathBuf::from(&outcome.artifacts[0]).exists());
        assert!(outcome.fully_succeeded(), "{outcome:?}");

        // The data really moved, and the table that was there before is gone —
        // this replaced the database rather than merging into it.
        assert_eq!(
            query_scalar("pl_target", "SELECT COUNT(*) FROM users").await,
            query_scalar("fixture", "SELECT COUNT(*) FROM users").await,
        );
        assert_eq!(
            query_scalar(
                "information_schema",
                "SELECT COUNT(*) FROM tables WHERE table_schema='pl_target' \
                 AND table_name='leftover'"
            )
            .await,
            "0",
            "a replace must not leave the old contents behind"
        );

        let steps = store.list_job_steps(ctx.job_id).await.unwrap();
        assert_eq!(
            steps.iter().map(|s| s.kind).collect::<Vec<_>>(),
            vec![JobStepKind::Backup, JobStepKind::Restore, JobStepKind::Verify],
            "the plan written up front matches the order it ran in"
        );
        assert!(
            steps.iter().all(|s| s.outcome == Some(JobStepOutcome::Success)),
            "{steps:#?}"
        );
        assert_eq!(
            steps[1].detail.database.as_deref(),
            Some("pl_target"),
            "the restore step names what it wrote"
        );

        drop_database("pl_target").await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_replace_with_no_typed_confirmation_is_refused_by_the_engine() {
        // Driven through `ops::run_pipeline`, not through a page, so the
        // guarantee does not depend on the UI remembering to ask. Same shape as
        // the equivalent test for `ops::sync`.
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "nc-src", "root", "testroot").await;
        let dest = profile(&store, "nc-dst", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        drop_database("nc_target").await;
        query_scalar("mysql", "CREATE DATABASE `nc_target`").await;
        query_scalar("nc_target", "CREATE TABLE keep_me (id INT)").await;

        let pipeline = saved(
            &store,
            "unconfirmed replace",
            vec![
                backup_step(source.id),
                restore_step(dest.id, TargetNaming::DropAndRecreate { name: "nc_target".into() }),
            ],
        )
        .await;

        let ctx = JobContext::new(Uuid::new_v4());
        let err = ops::run_pipeline(
            &pipeline,
            &run_request(&out, &[]),
            &store,
            &ToolSource::Local,
            &ctx,
        )
        .await
        .expect_err("a drop with nothing typed back must be refused");

        let message = err.to_string();
        assert!(
            message.contains("nc_target"),
            "the refusal must name what it was protecting: {message}"
        );

        assert_eq!(
            query_scalar(
                "information_schema",
                "SELECT COUNT(*) FROM tables WHERE table_schema='nc_target' \
                 AND table_name='keep_me'"
            )
            .await,
            "1",
            "the database must be exactly as it was"
        );

        drop_database("nc_target").await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_wrong_confirmation_is_refused_as_firmly_as_a_missing_one() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "wc-src", "root", "testroot").await;
        let dest = profile(&store, "wc-dst", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let pipeline = saved(
            &store,
            "mistyped replace",
            vec![
                backup_step(source.id),
                restore_step(dest.id, TargetNaming::DropAndRecreate { name: "wc_target".into() }),
            ],
        )
        .await;

        let ctx = JobContext::new(Uuid::new_v4());
        let err = ops::run_pipeline(
            &pipeline,
            // One character out. Typing the name back is the whole check.
            &run_request(&out, &["wc_targt"]),
            &store,
            &ToolSource::Local,
            &ctx,
        )
        .await
        .expect_err("a typo must not authorise a drop");
        assert!(err.to_string().contains("wc_target"), "got {err}");
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_failing_step_stops_the_chain_and_the_rest_are_marked_skipped() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "fs-src", "root", "testroot").await;
        let dest = profile(&store, "fs-dst", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        // Restoring into a database that does not exist fails at step two, so
        // the verify after it never runs.
        let pipeline = saved(
            &store,
            "restore into nothing",
            vec![
                backup_step(source.id),
                restore_step(
                    dest.id,
                    TargetNaming::IntoExisting { name: "fs_does_not_exist".into() },
                ),
                PipelineStep::Verify { deep: false },
            ],
        )
        .await;

        let ctx = JobContext::new(Uuid::new_v4());
        ops::record_start(
            &store,
            &ctx,
            db_sync_engine::events::JobKind::Sync,
            source.id,
            Some(dest.id),
            "{}".into(),
        )
        .await
        .unwrap();

        let err = ops::run_pipeline(
            &pipeline,
            &run_request(&out, &[]),
            &store,
            &ToolSource::Local,
            &ctx,
        )
        .await
        .expect_err("there is nothing to restore into");

        ctx.emit_error(db_sync_engine::events::JobPhase::Done, err.to_string())
            .await;
        ops::record_finish(&store, &ctx, JobOutcome::Failed, None)
            .await
            .unwrap();

        let steps = store.list_job_steps(ctx.job_id).await.unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].outcome, Some(JobStepOutcome::Success), "the backup ran");
        assert_eq!(steps[1].outcome, Some(JobStepOutcome::Failed));
        assert!(
            steps[1].detail.error.as_deref().is_some_and(|e| e.contains("fs_does_not_exist")),
            "the failed step names the reason: {:?}",
            steps[1].detail
        );
        assert_eq!(
            steps[2].outcome,
            Some(JobStepOutcome::Skipped),
            "a step the chain never reached is not a success"
        );
        assert!(steps[2].started_at.is_none());
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_pipeline_can_restore_a_file_no_step_of_it_produced() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "fa-src", "root", "testroot").await;
        let dest = profile(&store, "fa-dst", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        // Make an artifact with one pipeline, then restore it with another.
        let maker = saved(&store, "just a backup", vec![backup_step(source.id)]).await;
        let made = ops::run_pipeline(
            &maker,
            &run_request(&out, &[]),
            &store,
            &ToolSource::Local,
            &JobContext::new(Uuid::new_v4()),
        )
        .await
        .expect("backup");

        let reader = saved(
            &store,
            "restore that file",
            vec![PipelineStep::Restore {
                profile_id: dest.id,
                source: ArtifactSource::Path {
                    path: PathBuf::from(&made.artifacts[0]),
                },
                naming: TargetNaming::NewTimestamped { prefix: "fa".into() },
                engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
                verify_checksum: true,
            }],
        )
        .await;

        let outcome = ops::run_pipeline(
            &reader,
            &run_request(&out, &[]),
            &store,
            &ToolSource::Local,
            &JobContext::new(Uuid::new_v4()),
        )
        .await
        .expect("restore from a path");

        let target = &outcome.databases[0];
        assert!(target.starts_with("fa_"));
        assert_eq!(
            query_scalar(target, "SELECT COUNT(*) FROM users").await,
            query_scalar("fixture", "SELECT COUNT(*) FROM users").await,
        );

        drop_database(target).await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_pipeline_naming_a_deleted_connection_refuses_before_touching_anything() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "dp-src", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id]);

        let pipeline = saved(&store, "orphaned", vec![backup_step(source.id)]).await;
        store.delete_profile(source.id).await.unwrap();

        let err = ops::run_pipeline(
            &pipeline,
            &run_request(&out, &[]),
            &store,
            &ToolSource::Local,
            &JobContext::new(Uuid::new_v4()),
        )
        .await
        .expect_err("a step pointing at nothing cannot run");
        assert!(
            err.to_string().contains("step 1"),
            "the refusal must say which step: {err}"
        );
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn a_drill_inside_a_pipeline_does_not_replace_the_pipelines_own_plan() {
        // `ops::drill` plans three steps of its own. Nested, its recorder has
        // to go quiet — the step write is a delete-then-insert, so an enabled
        // one would leave the run reporting the drill's shape instead of the
        // chain's, and the two steps either side of it would vanish.
        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "nd-src", "root", "testroot").await;
        let dest = profile(&store, "nd-dst", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let pipeline = saved(
            &store,
            "back up then prove it restores",
            vec![
                backup_step(source.id),
                PipelineStep::Drill {
                    profile_id: dest.id,
                    artifact_dir: None,
                    deep: false,
                    keep_on_failure: false,
                },
            ],
        )
        .await;

        let ctx = JobContext::new(Uuid::new_v4());
        let outcome = ops::run_pipeline(
            &pipeline,
            &run_request(&out, &[]),
            &store,
            &ToolSource::Local,
            &ctx,
        )
        .await
        .expect("the drill should pass against the backup just taken");

        assert!(
            outcome.verification.as_ref().is_some_and(|r| r.passed()),
            "{outcome:?}"
        );

        let steps = store.list_job_steps(ctx.job_id).await.unwrap();
        assert_eq!(
            steps.iter().map(|s| s.kind).collect::<Vec<_>>(),
            vec![JobStepKind::Backup, JobStepKind::Drill],
            "the pipeline's two steps, not the drill's three: {steps:#?}"
        );
        assert!(steps.iter().all(|s| s.outcome == Some(JobStepOutcome::Success)));
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn masking_in_a_pipeline_rewrites_the_destination_but_never_the_artifact() {
        use db_sync_engine::mask::MaskRule;

        require_containers!();
        let (store, _dir) = temp_store().await;
        let out = tempfile::tempdir().unwrap();

        let source = profile(&store, "mk-src", "root", "testroot").await;
        let dest = profile(&store, "mk-dst", "root", "testroot").await;
        let _cleanup = Cleanup(vec![source.id, dest.id]);

        let pipeline = saved(
            &store,
            "masked refresh",
            vec![
                backup_step(source.id),
                restore_step(dest.id, TargetNaming::NewTimestamped { prefix: "mk".into() }),
                PipelineStep::Mask {
                    rules: vec![MaskRule::email("users", "email")],
                },
            ],
        )
        .await;

        let ctx = JobContext::new(Uuid::new_v4());
        let outcome = ops::run_pipeline(
            &pipeline,
            &run_request(&out, &[]),
            &store,
            &ToolSource::Local,
            &ctx,
        )
        .await
        .expect("a masked pipeline should run");

        let target = &outcome.databases[0];
        assert_eq!(
            query_scalar(
                target,
                "SELECT COUNT(*) FROM users WHERE email NOT LIKE '%@example.invalid'"
            )
            .await,
            "0",
            "every address on the destination must be masked"
        );

        // The documented limitation, asserted rather than trusted: masking
        // protects the destination, not the backup file. If this ever starts
        // failing because the artifact *is* masked, the module docs and the
        // README are wrong and have to change with it.
        let dump = tokio::process::Command::new("gzip")
            .args(["-dc", &outcome.artifacts[0]])
            .output()
            .await
            .expect("gunzip the artifact");
        let dump = String::from_utf8_lossy(&dump.stdout);
        assert!(
            dump.contains("ada@example.com"),
            "the artifact holds the real data; if this changed, the docs must too"
        );

        let steps = store.list_job_steps(ctx.job_id).await.unwrap();
        assert_eq!(steps[2].kind, JobStepKind::Mask);
        assert_eq!(steps[2].outcome, Some(JobStepOutcome::Success));
        assert!(
            steps[2].detail.notes.iter().any(|n| n.contains("column")),
            "the mask step says how much it changed: {:?}",
            steps[2].detail
        );

        drop_database(target).await;
    }
}
