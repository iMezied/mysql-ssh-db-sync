//! Commands driven through the real IPC path.
//!
//! The unit tests in `commands.rs` cover helpers, and the engine underneath is
//! covered thoroughly by its own suite. What neither reaches is the layer Tauri
//! generates: command-name registration, argument deserialisation from the
//! camelCase the webview sends, `State` extraction, and response serialisation.
//!
//! That layer is generated from the same signatures `bindings.ts` is, so it is
//! structurally hard to get wrong — but "hard to get wrong" is what was said
//! about every other thing in this project that turned out to be wrong. A
//! command renamed without its call site, or a handler left out of
//! `generate_handler!`, fails here rather than in front of a user.
//!
//! `mock_context(noop_assets())` is used rather than `generate_context!` so the
//! test does not depend on the frontend having been built.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use db_sync_desktop::AppState;
use db_sync_engine::backup::TableSelection;
use db_sync_engine::events::{EVENT_CHANNEL_CAPACITY, create_event_channel};
use db_sync_engine::job::JobRegistry;
use db_sync_engine::mask::MaskRule;
use db_sync_engine::plan::{SyncPlan, SyncPlanCreate};
use db_sync_engine::profile::{DbConfig, ProfileCreate, ToolOverrides};
use db_sync_engine::scheduler::Scheduler;
use db_sync_engine::store::Store;
use db_sync_engine::types::{Engine, EnvironmentTag};
use tauri::Manager;
use tauri::ipc::{CallbackFn, InvokeBody, InvokeResponseBody};
use tauri::test::{INVOKE_KEY, mock_builder, mock_context, noop_assets};
use tauri::webview::InvokeRequest;

type MockApp = tauri::App<tauri::test::MockRuntime>;

/// An app carrying real state backed by a temporary store.
fn app_with(store: Store, store_path: PathBuf) -> MockApp {
    let (event_tx, _rx) = create_event_channel(EVENT_CHANNEL_CAPACITY);
    let jobs = JobRegistry::new();
    let scheduler = Scheduler::new(store.clone(), jobs.clone(), event_tx.clone());

    let app = mock_builder()
        .invoke_handler(tauri::generate_handler![
            db_sync_desktop::commands::set_sync_plan_masking,
            db_sync_desktop::commands::masking_preview,
            db_sync_desktop::commands::backup_key_status,
            db_sync_desktop::commands::generate_backup_key,
            db_sync_desktop::commands::set_backup_key_recipients,
            db_sync_desktop::commands::list_destinations,
            db_sync_desktop::commands::create_destination,
            db_sync_desktop::commands::update_destination,
            db_sync_desktop::commands::delete_destination,
            db_sync_desktop::commands::list_ssh_connections,
            db_sync_desktop::commands::create_ssh_connection,
            db_sync_desktop::commands::update_ssh_connection,
            db_sync_desktop::commands::delete_ssh_connection,
            db_sync_desktop::commands::update_profile,
            db_sync_desktop::commands::list_job_steps,
        ])
        .build(mock_context(noop_assets()))
        .expect("mock app should build");

    app.manage(AppState {
        store,
        store_path,
        jobs,
        event_tx,
        scheduler,
        scheduler_loop: Mutex::new(None),
        quitting: AtomicBool::new(false),
        close_to_tray: AtomicBool::new(true),
        background_notice_shown: AtomicBool::new(true),
    });

    app
}

/// A webview label no other call in this process has used.
///
/// Tauri rejects a duplicate label, and every `invoke` here needs its own
/// window — a shared one would make the tests order-dependent.
fn next_label() -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    format!("test-{}", N.fetch_add(1, Ordering::Relaxed))
}

/// Invoke a command exactly as the webview would.
fn invoke(app: &MockApp, cmd: &str, args: serde_json::Value) -> serde_json::Value {
    let webview = tauri::WebviewWindowBuilder::new(app, next_label(), Default::default())
        .build()
        .expect("mock webview");

    let response = tauri::test::get_ipc_response(
        &webview,
        InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: InvokeBody::Json(args),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    );

    match response {
        Ok(InvokeResponseBody::Json(raw)) => serde_json::from_str(&raw).expect("a JSON response"),
        Ok(other) => panic!("{cmd} returned a non-JSON body: {other:?}"),
        Err(e) => panic!("{cmd} failed: {e}"),
    }
}

fn invoke_err(app: &MockApp, cmd: &str, args: serde_json::Value) -> serde_json::Value {
    let webview = tauri::WebviewWindowBuilder::new(app, next_label(), Default::default())
        .build()
        .expect("mock webview");

    tauri::test::get_ipc_response(
        &webview,
        InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: InvokeBody::Json(args),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    )
    .expect_err("this command was expected to fail")
}

async fn seeded() -> (Store, PathBuf, tempfile::TempDir, SyncPlan) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.db");
    let store = Store::open(&path).await.expect("open store");

    let profile = store
        .create_profile(ProfileCreate {
            name: "src".into(),
            engine: Engine::Mysql,
            environment: EnvironmentTag::Dev,
            ssh_connection_id: None,
            db: DbConfig {
                host: "127.0.0.1".into(),
                port: 3306,
                user: "root".into(),
                database: None,
            },
            tool_overrides: ToolOverrides::default(),
        })
        .await
        .expect("create profile");

    let plan = store
        .create_sync_plan(SyncPlanCreate {
            profile_id: profile.id,
            name: "nightly".into(),
            database: "app".into(),
            selections: vec![
                TableSelection::with_data("users"),
                TableSelection::schema_only("audit_log"),
            ],
            masking: Vec::new(),
        })
        .await
        .expect("create plan");

    (store, path, dir, plan)
}

#[test]
fn masking_rules_round_trip_through_the_ipc_boundary() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, plan) = rt.block_on(seeded());
    let app = app_with(store, path);

    let rules = vec![
        MaskRule::email("users", "email"),
        MaskRule::hash("users", "surname"),
    ];

    // `planId`/`masking` are the names the generated bindings send. A mismatch
    // here is exactly the failure this test exists to catch.
    let updated = invoke(
        &app,
        "set_sync_plan_masking",
        serde_json::json!({ "id": plan.id, "masking": rules }),
    );

    assert_eq!(
        updated["masking"].as_array().map(Vec::len),
        Some(2),
        "got {updated}"
    );
    assert_eq!(
        updated["revision"], 2,
        "saving masking must bump the revision so a schedule sees the change"
    );
    assert_eq!(updated["masking"][0]["column"], "email");
}

#[test]
fn the_preview_returns_sql_without_the_salt_in_it() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, plan) = rt.block_on(seeded());

    rt.block_on(async {
        store
            .set_sync_plan_masking(plan.id, vec![MaskRule::email("users", "email")])
            .await
            .expect("set masking");
    });

    let app = app_with(store, path);
    let preview = invoke(
        &app,
        "masking_preview",
        serde_json::json!({ "planId": plan.id }),
    );

    let updates = preview["updates"].as_array().expect("updates");
    assert_eq!(updates.len(), 1, "one UPDATE per table: {preview}");
    let sql = updates[0].as_str().unwrap();
    assert!(sql.contains("@example.invalid"), "{sql}");
    assert!(
        sql.contains('?'),
        "the salt must be a bound placeholder, not a literal: {sql}"
    );

    assert_eq!(
        preview["checks"].as_array().map(Vec::len),
        Some(1),
        "every rule needs a read-back"
    );
}

#[test]
fn a_rule_on_a_schema_only_table_is_reported_as_inert_not_silently_dropped() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, plan) = rt.block_on(seeded());

    rt.block_on(async {
        store
            .set_sync_plan_masking(plan.id, vec![MaskRule::hash("audit_log", "actor")])
            .await
            .expect("set masking");
    });

    let app = app_with(store, path);
    let preview = invoke(
        &app,
        "masking_preview",
        serde_json::json!({ "planId": plan.id }),
    );

    assert!(
        preview["updates"].as_array().unwrap().is_empty(),
        "a schema-only table has no rows to mask"
    );
    let inert = preview["inert"].as_array().expect("inert");
    assert_eq!(inert.len(), 1, "and the user must be told why: {preview}");
    assert_eq!(inert[0]["rule"]["table"], "audit_log");
}

#[test]
fn previewing_a_plan_that_does_not_exist_is_an_error_not_an_empty_preview() {
    // An empty preview would read as "nothing will be masked", which is the
    // same thing this page says when masking is genuinely off.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, _plan) = rt.block_on(seeded());
    let app = app_with(store, path);

    let error = invoke_err(
        &app,
        "masking_preview",
        serde_json::json!({ "planId": uuid::Uuid::new_v4() }),
    );
    assert_eq!(error["kind"], "not_found", "got {error}");
}

/// Key commands over IPC — the two that cannot touch the real keychain.
///
/// # Why `generate_backup_key` is not exercised here
///
/// Unlike a profile password, the backup key is stored under a **fixed**
/// keychain account (`secrets::APP_SCOPE`, the nil UUID) and a fixed service
/// name. There is exactly one per machine, by design: it has to be findable
/// without a profile. The consequence for tests is that it is *not* isolated
/// by a temporary store — calling `generate_backup_key` from a test would
/// create a key in the developer's own login keychain and leave it there.
///
/// So this covers the read path and the store-backed recipients list, both of
/// which are safe. Key creation is covered by the engine's own tests, which
/// own that decision.
#[test]
#[ignore = "requires an unlocked OS keychain"]
fn key_commands_answer_over_ipc_without_returning_a_secret() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, _plan) = rt.block_on(seeded());
    let app = app_with(store, path);

    // Read-only. Deliberately no assertion on `exists`: whether this machine
    // has a key is not this test's business, and asserting either way would
    // make the result depend on the developer's keychain.
    let status = invoke(&app, "backup_key_status", serde_json::json!({}));
    assert!(status["exists"].is_boolean(), "got {status}");
    assert!(status["extra_recipients"].is_array(), "got {status}");

    // Recipients live in the store, so this one is isolated by the temp file.
    // The key is generated here rather than hardcoded: `set_extra_recipients`
    // parses what it is given, and a made-up string is rejected.
    let (_secret, public) = db_sync_engine::crypto::generate_identity();
    let updated = invoke(
        &app,
        "set_backup_key_recipients",
        serde_json::json!({ "keys": [public] }),
    );
    assert_eq!(updated["extra_recipients"], serde_json::json!([public]));

    // A recipient that is not a real age key is refused rather than stored.
    // Accepting it would produce a manifest naming somebody who can never
    // decrypt, discovered only when they tried to.
    let rejected = invoke_err(
        &app,
        "set_backup_key_recipients",
        serde_json::json!({ "keys": ["age1definitelynotakey"] }),
    );
    assert_eq!(rejected["kind"], "key", "got {rejected}");

    // The rule this whole command surface exists to keep: nothing that crosses
    // the boundary carries the secret half.
    for response in [&status, &updated] {
        let text = response.to_string();
        assert!(
            !text.contains("AGE-SECRET-KEY"),
            "a secret crossed the IPC boundary: {text}"
        );
    }
}

// ── Off-site destinations ───────────────────────────────────────────────

fn s3_kind() -> serde_json::Value {
    serde_json::json!({
        "kind": "s3",
        "endpoint": "https://s3.eu-west-1.amazonaws.com",
        "region": "eu-west-1",
        "bucket": "acme-backups",
        "prefix": "prod",
        "path_style": false,
        "access_key_id": "AKIDEXAMPLE"
    })
}

#[test]
fn a_destination_with_no_credential_is_refused_before_it_is_stored() {
    // A destination without a key looks configured and cannot upload. Refusing
    // at creation is the only point at which the user is still looking at the
    // form; the alternative surfaces at 3am as a failed backup.
    //
    // Runs without a keychain because the validation happens first.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, _plan) = rt.block_on(seeded());
    let app = app_with(store, path);

    let error = invoke_err(
        &app,
        "create_destination",
        serde_json::json!({
            "input": { "name": "off-site", "kind": s3_kind(), "enabled": true },
            "secretAccessKey": "   "
        }),
    );
    assert_eq!(error["kind"], "invalid", "got {error}");

    let listed = invoke(&app, "list_destinations", serde_json::json!({}));
    assert!(
        listed.as_array().unwrap().is_empty(),
        "nothing may have been stored: {listed}"
    );
}

#[test]
fn a_plaintext_http_destination_is_refused_over_ipc() {
    // The engine refuses it; this proves the refusal survives the boundary as
    // an error the form can show rather than as a panic or a silent success.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, _plan) = rt.block_on(seeded());
    let app = app_with(store, path);

    let mut kind = s3_kind();
    kind["endpoint"] = serde_json::json!("http://s3.example.com");

    let error = invoke_err(
        &app,
        "create_destination",
        serde_json::json!({
            "input": { "name": "insecure", "kind": kind, "enabled": true },
            "secretAccessKey": "a-real-looking-secret"
        }),
    );
    assert_eq!(error["kind"], "invalid", "got {error}");
    assert!(
        error["message"].as_str().unwrap().contains("https://"),
        "the error must carry the fix: {error}"
    );
}

// ── SSH connections ─────────────────────────────────────────────────────

fn endpoint(host: &str) -> serde_json::Value {
    serde_json::json!({
        "host": host,
        "port": 22,
        "user": "ubuntu",
        "auth": { "kind": "agent" }
    })
}

/// Create one over IPC. Passing no passphrase keeps this keychain-free.
fn create_ssh(app: &MockApp, name: &str, jump_host_id: Option<uuid::Uuid>) -> serde_json::Value {
    invoke(
        app,
        "create_ssh_connection",
        serde_json::json!({
            "input": {
                "name": name,
                "endpoint": endpoint(&format!("{name}.example.com")),
                "jump_host_id": jump_host_id
            },
            "passphrase": null
        }),
    )
}

#[test]
fn an_ssh_connection_is_created_once_and_referenced_by_id() {
    // The point of the whole milestone: the tunnel is a record of its own, and
    // a profile carries a reference rather than a copy.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, plan) = rt.block_on(seeded());
    let app = app_with(store, path);

    let created = create_ssh(&app, "bastion", None);
    let id: uuid::Uuid = serde_json::from_value(created["id"].clone()).unwrap();
    assert_eq!(created["endpoint"]["host"], "bastion.example.com");
    assert_eq!(created["jump_host_id"], serde_json::Value::Null);

    let listed = invoke(&app, "list_ssh_connections", serde_json::json!({}));
    assert_eq!(listed.as_array().map(Vec::len), Some(1), "got {listed}");

    let attached = invoke(
        &app,
        "update_profile",
        serde_json::json!({
            "id": plan.profile_id,
            "patch": { "ssh_connection_id": id }
        }),
    );
    assert_eq!(
        attached["ssh_connection_id"], created["id"],
        "the profile must carry the reference, not the endpoint: {attached}"
    );
    assert!(
        attached.get("ssh").is_none(),
        "no embedded copy may survive the boundary: {attached}"
    );
}

#[test]
fn omitting_the_jump_host_keeps_it_and_an_explicit_null_removes_it() {
    // Presence versus value is decided by serde at exactly this boundary, and
    // it is the difference between "leave the bastion alone" and "drop it".
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, _plan) = rt.block_on(seeded());
    let app = app_with(store, path);

    let jump = create_ssh(&app, "jump", None);
    let jump_id: uuid::Uuid = serde_json::from_value(jump["id"].clone()).unwrap();
    let conn = create_ssh(&app, "db-host", Some(jump_id));
    let id: uuid::Uuid = serde_json::from_value(conn["id"].clone()).unwrap();
    assert_eq!(conn["jump_host_id"], jump["id"]);

    // A rename that says nothing about the bastion must not silently drop it.
    let renamed = invoke(
        &app,
        "update_ssh_connection",
        serde_json::json!({ "id": id, "patch": { "name": "db-host-2" } }),
    );
    assert_eq!(renamed["name"], "db-host-2");
    assert_eq!(
        renamed["jump_host_id"], jump["id"],
        "an omitted key means unchanged: {renamed}"
    );

    let detached = invoke(
        &app,
        "update_ssh_connection",
        serde_json::json!({ "id": id, "patch": { "jump_host_id": null } }),
    );
    assert_eq!(
        detached["jump_host_id"],
        serde_json::Value::Null,
        "an explicit null means remove: {detached}"
    );
}

#[test]
fn a_profile_can_be_re_pointed_and_detached_after_it_exists() {
    // What the tunnel selector on an existing connection sends. A bastion is
    // introduced, replaced or retired long after the database it fronts was
    // configured, and `ProfileUpdate` carries that by presence too — a
    // different struct from `SshConnectionUpdate`, so a different seam.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, plan) = rt.block_on(seeded());
    let app = app_with(store, path);

    let first = create_ssh(&app, "bastion", None);
    let second = create_ssh(&app, "bastion-2", None);

    let attached = invoke(
        &app,
        "update_profile",
        serde_json::json!({
            "id": plan.profile_id,
            "patch": { "ssh_connection_id": first["id"] }
        }),
    );
    assert_eq!(attached["ssh_connection_id"], first["id"]);

    let moved = invoke(
        &app,
        "update_profile",
        serde_json::json!({
            "id": plan.profile_id,
            "patch": { "ssh_connection_id": second["id"] }
        }),
    );
    assert_eq!(moved["ssh_connection_id"], second["id"], "got {moved}");

    // An edit that says nothing about the tunnel must not drop it — this is
    // the patch every *other* form on that page sends.
    let renamed = invoke(
        &app,
        "update_profile",
        serde_json::json!({ "id": plan.profile_id, "patch": { "name": "src-2" } }),
    );
    assert_eq!(renamed["name"], "src-2");
    assert_eq!(
        renamed["ssh_connection_id"], second["id"],
        "an omitted key means unchanged: {renamed}"
    );

    let detached = invoke(
        &app,
        "update_profile",
        serde_json::json!({
            "id": plan.profile_id,
            "patch": { "ssh_connection_id": null }
        }),
    );
    assert_eq!(
        detached["ssh_connection_id"],
        serde_json::Value::Null,
        "an explicit null detaches: {detached}"
    );

    // And with nothing pointing at it, the connection can now be deleted.
    assert_eq!(
        invoke(
            &app,
            "delete_ssh_connection",
            serde_json::json!({ "id": second["id"] })
        ),
        true
    );
}

#[test]
fn a_connection_in_use_is_refused_and_the_error_names_what_uses_it() {
    // "Cannot delete" without saying what holds it is the kind of error that
    // sends someone clicking through every profile to find out.
    //
    // Keychain-free: the refusal happens before any secret is touched.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, plan) = rt.block_on(seeded());
    let app = app_with(store, path);

    let conn = create_ssh(&app, "bastion", None);
    let id: uuid::Uuid = serde_json::from_value(conn["id"].clone()).unwrap();

    invoke(
        &app,
        "update_profile",
        serde_json::json!({
            "id": plan.profile_id,
            "patch": { "ssh_connection_id": id }
        }),
    );

    let error = invoke_err(
        &app,
        "delete_ssh_connection",
        serde_json::json!({ "id": id }),
    );
    assert_eq!(error["kind"], "invalid", "got {error}");
    assert!(
        error["message"].as_str().unwrap().contains("src"),
        "the error must name the profile holding it: {error}"
    );

    // Still there, and still attached.
    let listed = invoke(&app, "list_ssh_connections", serde_json::json!({}));
    assert_eq!(listed.as_array().map(Vec::len), Some(1), "got {listed}");
}

#[test]
fn two_connections_cannot_share_a_name() {
    // Names are how a shared config refers to a tunnel on another machine, so
    // a duplicate is ambiguous rather than merely untidy.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, _plan) = rt.block_on(seeded());
    let app = app_with(store, path);

    create_ssh(&app, "bastion", None);
    let error = invoke_err(
        &app,
        "create_ssh_connection",
        serde_json::json!({
            "input": { "name": "bastion", "endpoint": endpoint("other.example.com") },
            "passphrase": null
        }),
    );
    assert_eq!(error["kind"], "duplicate_name", "got {error}");
}

#[test]
#[ignore = "requires an unlocked OS keychain"]
fn a_destination_round_trips_over_ipc_without_its_secret() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (store, path, _dir, _plan) = rt.block_on(seeded());
    let app = app_with(store, path);

    let created = invoke(
        &app,
        "create_destination",
        serde_json::json!({
            "input": {
                "name": "off-site",
                "kind": s3_kind(),
                "enabled": true,
                "retention": { "keep_last": 30, "max_age_days": null }
            },
            "secretAccessKey": "wJalrXUtnFEMI-TESTVALUE"
        }),
    );

    // Cleaned up however this test ends.
    let id: uuid::Uuid = serde_json::from_value(created["id"].clone()).unwrap();
    struct Cleanup(uuid::Uuid);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = db_sync_engine::secrets::delete_for_destination(self.0);
        }
    }
    let _cleanup = Cleanup(id);

    assert_eq!(created["name"], "off-site");
    assert_eq!(created["location"], "s3://acme-backups/prod");
    assert_eq!(
        created["has_credential"], true,
        "the UI needs to know a key was filed: {created}"
    );

    // The rule the whole command surface exists to keep.
    let text = created.to_string();
    assert!(
        !text.contains("wJalrXUtnFEMI-TESTVALUE"),
        "a secret crossed the IPC boundary: {text}"
    );
    assert!(
        text.contains("AKIDEXAMPLE"),
        "the key id is not secret and says which credential is in use: {text}"
    );

    // Disabling keeps the credential — pausing a destination for an afternoon
    // must not mean setting it up again.
    let disabled = invoke(
        &app,
        "update_destination",
        serde_json::json!({
            "id": id,
            "patch": { "name": null, "kind": null, "enabled": false, "retention": null }
        }),
    );
    assert_eq!(disabled["enabled"], false);
    assert_eq!(disabled["has_credential"], true, "got {disabled}");
    assert_eq!(disabled["name"], "off-site", "an absent field is untouched");

    assert_eq!(
        invoke(&app, "delete_destination", serde_json::json!({ "id": id })),
        true
    );
    assert!(
        !db_sync_engine::secrets::has_secret(
            id,
            db_sync_engine::secrets::SecretKind::ObjectStoreSecret
        )
        .unwrap(),
        "deleting the destination must take its credential with it"
    );
}

#[tokio::test]
async fn a_jobs_steps_come_back_in_order_with_their_state() {
    use db_sync_engine::step::{JobStepKind, JobStepOutcome};

    let (store, path, _dir, _plan) = seeded().await;
    let job = uuid::Uuid::new_v4();

    store
        .plan_job_steps(
            job,
            &[
                (JobStepKind::Backup, "Back up shop".into()),
                (JobStepKind::Restore, "Replace shop_copy".into()),
                (JobStepKind::Verify, "Compare against the source".into()),
            ],
        )
        .await
        .unwrap();
    store.begin_job_step(job, 1).await.unwrap();
    store
        .finish_job_step(job, 1, JobStepOutcome::Success, &Default::default())
        .await
        .unwrap();
    store.begin_job_step(job, 2).await.unwrap();
    store
        .close_open_steps(job, JobStepOutcome::Failed, Some("shop_copy is in use"))
        .await
        .unwrap();

    let app = app_with(store, path);
    let steps = invoke(
        &app,
        "list_job_steps",
        // camelCase on the wire: the layer Tauri generates is what this suite
        // exists to exercise, and `job_id` here would silently arrive as None.
        serde_json::json!({ "jobId": job.to_string() }),
    );

    let steps = steps.as_array().expect("an array of steps");
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0]["index"], 1, "ordered by index, not by insertion");
    assert_eq!(steps[0]["outcome"], "success");
    assert_eq!(steps[1]["outcome"], "failed");
    assert_eq!(
        steps[1]["detail"]["error"], "shop_copy is in use",
        "the failed step names the reason: {}",
        steps[1]
    );
    assert_eq!(
        steps[2]["outcome"], "skipped",
        "a step the run never reached is not a success"
    );
}

#[tokio::test]
async fn a_job_with_no_recorded_steps_returns_an_empty_list() {
    // Every job from before this existed, and every single-step job. An error
    // here would make the detail page fail rather than simply show no steps.
    let (store, path, _dir, _plan) = seeded().await;
    let app = app_with(store, path);

    let steps = invoke(
        &app,
        "list_job_steps",
        serde_json::json!({ "jobId": uuid::Uuid::new_v4().to_string() }),
    );
    assert_eq!(steps.as_array().expect("an array").len(), 0);
}
