//! Integration tests for the persistence layer.
//!
//! These run against a real SQLite file rather than a mock: migrations, JSON
//! round-tripping and the unique-name constraint are exactly the things a mock
//! would paper over.

use chrono::Utc;
use db_sync_engine::backup::TableSelection;
use db_sync_engine::destination::{
    DestinationCreate, DestinationKind, DestinationUpdate, S3Destination,
};
use db_sync_engine::events::JobKind;
use db_sync_engine::job::{JobOutcome, JobRecord};
use db_sync_engine::mask::{MaskRule, MaskTransform};
use db_sync_engine::plan::SyncPlanCreate;
use db_sync_engine::profile::{
    DbConfig, ProfileCreate, ProfileUpdate, SshAuth, SshConfig, SshEndpoint, ToolOverrides,
};
use db_sync_engine::retention::RetentionPolicy;
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

// ── Masking rules ───────────────────────────────────────────────────────

async fn plan_with_masking(
    store: &Store,
    masking: Vec<MaskRule>,
) -> db_sync_engine::plan::SyncPlan {
    let profile = store
        .create_profile(profile_input(&Uuid::new_v4().to_string(), Engine::Mysql))
        .await
        .unwrap();

    store
        .create_sync_plan(SyncPlanCreate {
            profile_id: profile.id,
            name: "nightly".into(),
            database: "app".into(),
            selections: vec![
                TableSelection::with_data("users"),
                TableSelection::schema_only("audit_log"),
            ],
            masking,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn masking_rules_survive_a_round_trip() {
    let (store, _dir) = store().await;
    let rules = vec![
        MaskRule::email("users", "email"),
        MaskRule {
            table: "users".into(),
            column: "note".into(),
            transform: MaskTransform::Constant {
                value: "redacted".into(),
            },
        },
    ];

    let created = plan_with_masking(&store, rules.clone()).await;
    let read_back = store.get_sync_plan(created.id).await.unwrap().unwrap();
    assert_eq!(read_back.masking, rules);
}

#[tokio::test]
async fn a_plan_written_before_masking_existed_reads_as_unmasked() {
    // The migration adds the column with a default. A plan row that predates
    // it must come back as "no masking" rather than as corruption.
    let (store, _dir) = store().await;
    let plan = plan_with_masking(&store, Vec::new()).await;

    sqlx::query("UPDATE sync_plans SET masking = '[]' WHERE id = ?1")
        .bind(plan.id.to_string())
        .execute(store.pool())
        .await
        .unwrap();

    let read_back = store.get_sync_plan(plan.id).await.unwrap().unwrap();
    assert!(read_back.masking.is_empty());
}

#[tokio::test]
async fn corrupt_masking_rules_are_an_error_not_an_empty_list() {
    // The dangerous shape: reading unparseable rules as "no masking" would
    // hand somebody an unmasked destination while the plan still claims the
    // column is protected. Failing the run is the only safe reading.
    let (store, _dir) = store().await;
    let plan = plan_with_masking(&store, vec![MaskRule::email("users", "email")]).await;

    sqlx::query("UPDATE sync_plans SET masking = '{not json' WHERE id = ?1")
        .bind(plan.id.to_string())
        .execute(store.pool())
        .await
        .unwrap();

    let err = store.get_sync_plan(plan.id).await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Corrupt {
                field: "masking",
                ..
            }
        ),
        "got: {err}"
    );
}

#[tokio::test]
async fn changing_masking_bumps_the_revision() {
    // A schedule whose masking changed underneath it must be as visible as one
    // whose table selection did.
    let (store, _dir) = store().await;
    let plan = plan_with_masking(&store, Vec::new()).await;
    assert_eq!(plan.revision, 1);

    let updated = store
        .set_sync_plan_masking(plan.id, vec![MaskRule::email("users", "email")])
        .await
        .unwrap();

    assert_eq!(updated.revision, 2);
    assert_eq!(updated.masking.len(), 1);
}

#[tokio::test]
async fn a_rule_on_a_schema_only_table_is_not_active() {
    // Nothing reaches the destination, so nothing is exposed — but it is not
    // the same as a rule that runs, and the two must not read alike.
    let (store, _dir) = store().await;
    let plan = plan_with_masking(
        &store,
        vec![
            MaskRule::email("users", "email"),
            MaskRule::hash("audit_log", "actor"),
        ],
    )
    .await;

    assert_eq!(plan.masking.len(), 2);
    let active = plan.active_masking();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].table, "users");
}

// ── Off-site destinations ───────────────────────────────────────────────

fn s3_destination(name: &str) -> DestinationCreate {
    DestinationCreate {
        name: name.to_string(),
        kind: DestinationKind::S3(S3Destination {
            endpoint: "https://s3.eu-west-1.amazonaws.com".into(),
            region: "eu-west-1".into(),
            bucket: "backups".into(),
            prefix: "prod".into(),
            path_style: false,
            access_key_id: "AKIDEXAMPLE".into(),
        }),
        enabled: true,
        retention: RetentionPolicy::default(),
    }
}

#[tokio::test]
async fn a_destination_round_trips_through_the_store() {
    let (store, _dir) = store().await;
    let created = store
        .create_destination(s3_destination("off-site"))
        .await
        .expect("create");

    let fetched = store
        .get_destination(created.id)
        .await
        .unwrap()
        .expect("stored");
    assert_eq!(fetched, created);

    let DestinationKind::S3(s3) = &fetched.kind;
    assert_eq!(s3.bucket, "backups");
    assert_eq!(s3.access_key_id, "AKIDEXAMPLE");
}

#[tokio::test]
async fn a_stored_destination_holds_no_credential() {
    // The invariant the schema is built around: nothing in this table is
    // sensitive, so no query against it can leak a credential.
    let (store, _dir) = store().await;
    let created = store
        .create_destination(s3_destination("off-site"))
        .await
        .unwrap();

    let row: (String,) = sqlx::query_as("SELECT kind FROM destinations WHERE id = ?1")
        .bind(created.id.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();

    assert!(row.0.contains("AKIDEXAMPLE"), "the key id is not a secret");
    assert!(
        !row.0.to_lowercase().contains("secret"),
        "no secret material may be persisted: {}",
        row.0
    );
}

#[tokio::test]
async fn destination_names_are_unique() {
    // A job log naming a destination has to be unambiguous.
    let (store, _dir) = store().await;
    store
        .create_destination(s3_destination("off-site"))
        .await
        .unwrap();

    let err = store
        .create_destination(s3_destination("off-site"))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::DuplicateName(_)), "{err}");
}

#[tokio::test]
async fn an_unusable_destination_is_refused_rather_than_stored() {
    // Storing it would make it look configured, and it would only fail at the
    // moment it was being relied on.
    let (store, _dir) = store().await;
    let mut input = s3_destination("insecure");
    let DestinationKind::S3(s3) = &mut input.kind;
    s3.endpoint = "http://s3.example.com".into();

    let err = store.create_destination(input).await.unwrap_err();
    assert!(matches!(err, StoreError::InvalidDestination(_)), "{err}");
    assert!(store.list_destinations().await.unwrap().is_empty());
}

#[tokio::test]
async fn only_enabled_destinations_are_offered_to_a_backup() {
    let (store, _dir) = store().await;
    store
        .create_destination(s3_destination("live"))
        .await
        .unwrap();

    let mut paused = s3_destination("paused");
    paused.enabled = false;
    let paused = store.create_destination(paused).await.unwrap();

    let enabled = store.list_enabled_destinations().await.unwrap();
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].name, "live");

    // Disabled, not forgotten: the configuration and its credential survive.
    assert_eq!(store.list_destinations().await.unwrap().len(), 2);
    assert!(store.get_destination(paused.id).await.unwrap().is_some());
}

#[tokio::test]
async fn updating_a_destination_leaves_untouched_fields_alone() {
    let (store, _dir) = store().await;
    let created = store
        .create_destination(s3_destination("off-site"))
        .await
        .unwrap();

    let updated = store
        .update_destination(
            created.id,
            DestinationUpdate {
                enabled: Some(false),
                retention: Some(RetentionPolicy {
                    keep_last: Some(30),
                    max_age_days: None,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(!updated.enabled);
    assert_eq!(updated.retention.keep_last, Some(30));
    assert_eq!(
        updated.name, "off-site",
        "the name was not part of the patch"
    );
    assert_eq!(updated.kind, created.kind);
}

#[tokio::test]
async fn an_update_cannot_make_a_destination_unusable() {
    // The same guard as creation. Editing an endpoint to a plaintext one is
    // exactly as bad as configuring it that way to begin with.
    let (store, _dir) = store().await;
    let created = store
        .create_destination(s3_destination("off-site"))
        .await
        .unwrap();

    let err = store
        .update_destination(
            created.id,
            DestinationUpdate {
                kind: Some(DestinationKind::S3(S3Destination {
                    endpoint: "http://s3.example.com".into(),
                    region: "eu-west-1".into(),
                    bucket: "backups".into(),
                    prefix: String::new(),
                    path_style: false,
                    access_key_id: "AKIDEXAMPLE".into(),
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, StoreError::InvalidDestination(_)), "{err}");

    let unchanged = store.get_destination(created.id).await.unwrap().unwrap();
    assert_eq!(unchanged.kind, created.kind, "the stored row must not move");
}

#[tokio::test]
async fn off_site_retention_defaults_to_keeping_everything() {
    // A destination created without a policy must not start deleting things.
    let (store, _dir) = store().await;
    let created = store
        .create_destination(s3_destination("off-site"))
        .await
        .unwrap();
    assert!(!created.retention.is_enabled());

    let fetched = store.get_destination(created.id).await.unwrap().unwrap();
    assert!(
        !fetched.retention.is_enabled(),
        "and it reads back that way"
    );
}

#[tokio::test]
async fn deleting_a_destination_removes_it() {
    let (store, _dir) = store().await;
    let created = store
        .create_destination(s3_destination("off-site"))
        .await
        .unwrap();

    assert!(store.delete_destination(created.id).await.unwrap());
    assert!(store.get_destination(created.id).await.unwrap().is_none());
    assert!(
        !store.delete_destination(created.id).await.unwrap(),
        "deleting twice reports that there was nothing to delete"
    );
}

#[tokio::test]
async fn a_corrupt_destination_row_is_an_error_not_a_guess() {
    let (store, _dir) = store().await;
    let created = store
        .create_destination(s3_destination("off-site"))
        .await
        .unwrap();

    sqlx::query("UPDATE destinations SET kind = ?2 WHERE id = ?1")
        .bind(created.id.to_string())
        .bind("{not json")
        .execute(store.pool())
        .await
        .unwrap();

    let err = store.get_destination(created.id).await.unwrap_err();
    assert!(matches!(err, StoreError::Corrupt { .. }), "{err}");
}

// ── Audit log ───────────────────────────────────────────────────────────

#[tokio::test]
async fn configuration_changes_are_recorded_newest_first() {
    use db_sync_engine::audit::AuditAction;

    let (store, _dir) = store().await;
    store
        .audit(
            AuditAction::ProfileCreated,
            "prod-eu",
            "mysql at db.internal",
        )
        .await;
    // Timestamps have sub-second resolution here, but two writes in the same
    // millisecond would tie; a small gap keeps the assertion about ordering
    // rather than about luck.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    store
        .audit(AuditAction::MaskingChanged, "nightly", "0 rule(s), was 3")
        .await;

    let entries = store.list_audit(10).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].action, "masking.changed");
    assert_eq!(entries[0].subject, "nightly");
    assert_eq!(entries[1].action, "profile.created");
}

#[tokio::test]
async fn the_audit_log_records_that_a_secret_was_set_not_what_it_was() {
    // The rule every other layer follows, held here too: `detail` is free-form
    // and must never become a place a credential ends up.
    use db_sync_engine::audit::AuditAction;

    let (store, _dir) = store().await;
    store
        .audit(AuditAction::SecretSet, "prod-eu", "DbPassword stored")
        .await;

    let entries = store.list_audit(10).await.unwrap();
    assert!(entries[0].detail.contains("stored"));
    assert!(!entries[0].detail.to_lowercase().contains("hunter2"));
}

#[tokio::test]
async fn an_unwritable_audit_row_does_not_abort_the_change_being_audited() {
    // Refusing to delete a profile because the log could not be written would
    // be a worse outcome than an incomplete log. Closing the pool makes every
    // write fail; `audit` must still return.
    use db_sync_engine::audit::AuditAction;

    let (store, _dir) = store().await;
    store.close().await;

    // No panic, no propagated error — the failure is logged and swallowed.
    store
        .audit(AuditAction::ProfileDeleted, "gone", "backups stop")
        .await;
}

#[tokio::test]
async fn the_audit_limit_is_respected_and_never_zero() {
    use db_sync_engine::audit::AuditAction;

    let (store, _dir) = store().await;
    for i in 0..5 {
        store
            .audit(AuditAction::ProfileCreated, format!("p{i}"), "")
            .await;
    }

    assert_eq!(store.list_audit(3).await.unwrap().len(), 3);
    // A zero or negative limit would return nothing and read as "no changes",
    // which is a different claim from "you asked for none".
    assert_eq!(store.list_audit(0).await.unwrap().len(), 1);
}
