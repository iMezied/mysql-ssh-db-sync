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
