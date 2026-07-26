//! Integration tests for the persistence layer.
//!
//! These run against a real SQLite file rather than a mock: migrations, JSON
//! round-tripping and the unique-name constraint are exactly the things a mock
//! would paper over.

use chrono::Utc;
use db_sync_engine::events::JobKind;
use db_sync_engine::job::{JobOutcome, JobRecord};
use db_sync_engine::profile::{
    DbConfig, ProfileCreate, ProfileUpdate, SshAuth, SshConfig, SshEndpoint, ToolOverrides,
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

fn profile_input(name: &str, engine: Engine) -> ProfileCreate {
    ProfileCreate {
        name: name.to_string(),
        engine,
        environment: EnvironmentTag::Dev,
        ssh: None,
        db: DbConfig {
            host: "127.0.0.1".into(),
            port: engine.default_port(),
            user: "app".into(),
            database: Some("appdb".into()),
        },
        tool_overrides: ToolOverrides::default(),
    }
}

fn ssh_config() -> SshConfig {
    SshConfig {
        endpoint: SshEndpoint {
            host: "bastion.example.com".into(),
            port: 22,
            user: "ubuntu".into(),
            auth: SshAuth::KeyFile {
                path: "~/.ssh/id_ed25519".into(),
                passphrase_in_keychain: true,
            },
        },
        jump_host: Some(SshEndpoint {
            host: "jump.example.com".into(),
            port: 2222,
            user: "ops".into(),
            auth: SshAuth::Agent,
        }),
    }
}

#[tokio::test]
async fn migrations_run_on_a_fresh_database() {
    let (store, _dir) = store().await;
    assert!(store.list_profiles().await.unwrap().is_empty());
    assert!(store.list_jobs(10).await.unwrap().is_empty());
}

#[tokio::test]
async fn opening_an_existing_store_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let first = Store::open(&path).await.unwrap();
    first
        .create_profile(profile_input("a", Engine::Mysql))
        .await
        .unwrap();
    first.close().await;

    // Re-running migrations against a populated store must not fail or wipe.
    let second = Store::open(&path).await.unwrap();
    assert_eq!(second.list_profiles().await.unwrap().len(), 1);
}

#[tokio::test]
async fn profile_survives_a_full_round_trip() {
    let (store, _dir) = store().await;

    let mut input = profile_input("prod-germany", Engine::Mysql);
    input.environment = EnvironmentTag::Prod;
    input.ssh = Some(ssh_config());
    input.tool_overrides = ToolOverrides {
        mysqldump: Some("/opt/homebrew/bin/mysqldump".into()),
        ..Default::default()
    };

    let created = store.create_profile(input).await.unwrap();
    let fetched = store.get_profile(created.id).await.unwrap().unwrap();

    assert_eq!(
        fetched, created,
        "stored profile must read back identically"
    );
    assert_eq!(fetched.engine, Engine::Mysql);
    assert_eq!(fetched.environment, EnvironmentTag::Prod);

    // Nested SSH config, including the jump host, must survive serialisation.
    let ssh = fetched.ssh.expect("ssh config");
    assert_eq!(ssh.endpoint.host, "bastion.example.com");
    let jump = ssh.jump_host.expect("jump host");
    assert_eq!(jump.port, 2222);
    assert_eq!(jump.auth, SshAuth::Agent);

    assert_eq!(
        fetched.tool_overrides.mysqldump.as_deref(),
        Some("/opt/homebrew/bin/mysqldump")
    );
}

#[tokio::test]
async fn postgres_profiles_default_to_the_right_port() {
    let (store, _dir) = store().await;
    let p = store
        .create_profile(profile_input("pg", Engine::Postgres))
        .await
        .unwrap();
    assert_eq!(p.db.port, 5432);

    let back = store.get_profile(p.id).await.unwrap().unwrap();
    assert_eq!(back.engine, Engine::Postgres);
}

#[tokio::test]
async fn duplicate_profile_names_are_rejected() {
    let (store, _dir) = store().await;
    store
        .create_profile(profile_input("dup", Engine::Mysql))
        .await
        .unwrap();

    let err = store
        .create_profile(profile_input("dup", Engine::Postgres))
        .await
        .unwrap_err();

    assert!(
        matches!(err, StoreError::DuplicateName(n) if n == "dup"),
        "a clashing name must be a typed error, not a raw SQL failure"
    );
}

#[tokio::test]
async fn update_applies_only_the_supplied_fields() {
    let (store, _dir) = store().await;
    let created = store
        .create_profile(profile_input("staging", Engine::Mysql))
        .await
        .unwrap();

    let updated = store
        .update_profile(
            created.id,
            ProfileUpdate {
                environment: Some(EnvironmentTag::Staging),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.environment, EnvironmentTag::Staging);
    assert_eq!(updated.name, "staging", "unset fields must be preserved");
    assert_eq!(updated.db, created.db);
    assert!(updated.updated_at >= created.updated_at);
}

#[tokio::test]
async fn update_can_add_and_clear_ssh_config() {
    let (store, _dir) = store().await;
    let created = store
        .create_profile(profile_input("s", Engine::Mysql))
        .await
        .unwrap();
    assert!(created.ssh.is_none());

    let with_ssh = store
        .update_profile(
            created.id,
            ProfileUpdate {
                ssh: Some(Some(ssh_config())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(with_ssh.ssh.is_some());

    // Some(None) clears; None would have left it alone.
    let cleared = store
        .update_profile(
            created.id,
            ProfileUpdate {
                ssh: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        cleared.ssh.is_none(),
        "Some(None) must clear the SSH config"
    );

    let untouched = store
        .update_profile(created.id, ProfileUpdate::default())
        .await
        .unwrap();
    assert!(untouched.ssh.is_none());
}

#[tokio::test]
async fn updating_a_missing_profile_reports_not_found() {
    let (store, _dir) = store().await;
    let err = store
        .update_profile(Uuid::new_v4(), ProfileUpdate::default())
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ProfileNotFound(_)));
}

#[tokio::test]
async fn delete_removes_the_profile_and_reports_whether_it_existed() {
    let (store, _dir) = store().await;
    let created = store
        .create_profile(profile_input("gone", Engine::Mysql))
        .await
        .unwrap();

    assert!(store.delete_profile(created.id).await.unwrap());
    assert!(store.get_profile(created.id).await.unwrap().is_none());
    assert!(
        !store.delete_profile(created.id).await.unwrap(),
        "deleting twice must report that nothing was removed"
    );
}

#[tokio::test]
async fn profiles_are_listed_alphabetically() {
    let (store, _dir) = store().await;
    for name in ["zulu", "alpha", "mike"] {
        store
            .create_profile(profile_input(name, Engine::Mysql))
            .await
            .unwrap();
    }

    let names: Vec<String> = store
        .list_profiles()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, vec!["alpha", "mike", "zulu"]);
}

#[tokio::test]
async fn job_history_records_a_full_lifecycle() {
    let (store, _dir) = store().await;
    let profile = store
        .create_profile(profile_input("src", Engine::Mysql))
        .await
        .unwrap();

    let job = JobRecord {
        id: Uuid::new_v4(),
        kind: JobKind::Backup,
        source_profile_id: profile.id,
        dest_profile_id: None,
        started_at: Utc::now(),
        finished_at: None,
        outcome: None,
        artifact_path: None,
        options_json: r#"{"single_transaction":true}"#.into(),
        log: String::new(),
    };
    store.insert_job(&job).await.unwrap();

    let running = store.get_job(job.id).await.unwrap().unwrap();
    assert!(
        running.outcome.is_none(),
        "a running job has no outcome yet"
    );

    store
        .finish_job(
            job.id,
            JobOutcome::Success,
            Some("/backups/app.sql.gz".into()),
            "line one\nline two\n".into(),
        )
        .await
        .unwrap();

    let done = store.get_job(job.id).await.unwrap().unwrap();
    assert_eq!(done.outcome, Some(JobOutcome::Success));
    assert!(done.finished_at.is_some());
    assert_eq!(done.artifact_path.as_deref(), Some("/backups/app.sql.gz"));
    assert!(
        done.log.contains("line two"),
        "the durable log must be persisted, not just streamed"
    );
    assert_eq!(done.options_json, r#"{"single_transaction":true}"#);
}

#[tokio::test]
async fn failed_and_cancelled_outcomes_round_trip() {
    let (store, _dir) = store().await;
    let profile = store
        .create_profile(profile_input("src", Engine::Mysql))
        .await
        .unwrap();

    for (kind, outcome) in [
        (JobKind::Restore, JobOutcome::Failed),
        (JobKind::Sync, JobOutcome::Cancelled),
        (JobKind::Verify, JobOutcome::Success),
    ] {
        let id = Uuid::new_v4();
        store
            .insert_job(&JobRecord {
                id,
                kind,
                source_profile_id: profile.id,
                dest_profile_id: Some(profile.id),
                started_at: Utc::now(),
                finished_at: None,
                outcome: None,
                artifact_path: None,
                options_json: "{}".into(),
                log: String::new(),
            })
            .await
            .unwrap();
        store
            .finish_job(id, outcome, None, String::new())
            .await
            .unwrap();

        let back = store.get_job(id).await.unwrap().unwrap();
        assert_eq!(back.outcome, Some(outcome));
        assert_eq!(back.kind, kind);
        assert_eq!(back.dest_profile_id, Some(profile.id));
    }
}

#[tokio::test]
async fn jobs_are_listed_newest_first_and_respect_the_limit() {
    let (store, _dir) = store().await;
    let profile = store
        .create_profile(profile_input("src", Engine::Mysql))
        .await
        .unwrap();

    for i in 0..5 {
        store
            .insert_job(&JobRecord {
                id: Uuid::new_v4(),
                kind: JobKind::Backup,
                source_profile_id: profile.id,
                dest_profile_id: None,
                started_at: Utc::now() + chrono::Duration::seconds(i),
                finished_at: None,
                outcome: None,
                artifact_path: None,
                options_json: "{}".into(),
                log: String::new(),
            })
            .await
            .unwrap();
    }

    let jobs = store.list_jobs(3).await.unwrap();
    assert_eq!(jobs.len(), 3);
    assert!(
        jobs[0].started_at > jobs[1].started_at,
        "history must read newest first"
    );
}

#[tokio::test]
async fn known_hosts_pin_on_first_sight_and_do_not_silently_change() {
    let (store, _dir) = store().await;

    assert!(
        store
            .get_known_host("db.example.com:22")
            .await
            .unwrap()
            .is_none()
    );

    store
        .remember_host("db.example.com:22", "ssh-ed25519", "SHA256:aaa")
        .await
        .unwrap();

    let (kind, fp) = store
        .get_known_host("db.example.com:22")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kind, "ssh-ed25519");
    assert_eq!(fp, "SHA256:aaa");

    // A second sighting must not overwrite the pin — that is what makes a
    // changed host key detectable.
    store
        .remember_host("db.example.com:22", "ssh-ed25519", "SHA256:bbb")
        .await
        .unwrap();
    let (_, fp) = store
        .get_known_host("db.example.com:22")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fp, "SHA256:aaa", "an existing pin must not be overwritten");
}

#[tokio::test]
async fn host_key_can_be_repinned_explicitly() {
    let (store, _dir) = store().await;
    store
        .remember_host("db.example.com:22", "ssh-ed25519", "SHA256:aaa")
        .await
        .unwrap();

    store
        .replace_host_key("db.example.com:22", "ssh-rsa", "SHA256:ccc")
        .await
        .unwrap();

    let (kind, fp) = store
        .get_known_host("db.example.com:22")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kind, "ssh-rsa");
    assert_eq!(fp, "SHA256:ccc");
}

#[tokio::test]
async fn corrupt_rows_surface_as_errors_rather_than_wrong_data() {
    let (store, _dir) = store().await;
    let created = store
        .create_profile(profile_input("c", Engine::Mysql))
        .await
        .unwrap();

    // Simulate a hand-edited or partially-written database.
    sqlx::query("UPDATE profiles SET engine = 'mariadb' WHERE id = ?1")
        .bind(created.id.to_string())
        .execute(store.pool())
        .await
        .unwrap();

    let err = store.get_profile(created.id).await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Corrupt {
                field: "engine",
                ..
            }
        ),
        "an unknown engine must not be silently coerced"
    );
}

#[tokio::test]
async fn malformed_ssh_json_is_corruption_not_a_direct_connection() {
    let (store, _dir) = store().await;
    let created = store
        .create_profile(profile_input("c", Engine::Mysql))
        .await
        .unwrap();

    sqlx::query("UPDATE profiles SET ssh_config = '{not json' WHERE id = ?1")
        .bind(created.id.to_string())
        .execute(store.pool())
        .await
        .unwrap();

    let err = store.get_profile(created.id).await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Corrupt {
                field: "ssh_config",
                ..
            }
        ),
        "a tunnelled profile must never degrade into a direct connection"
    );
}
