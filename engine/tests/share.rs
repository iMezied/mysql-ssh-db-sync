//! Sharing configuration between machines, against a real store.
//!
//! The property under test is negative and therefore easy to lose: an export
//! must contain no secret, and an import must never write one. A unit test can
//! check the *shape* has nowhere to put a password; only a round trip through
//! the store shows that nothing leaks in on the way.

use db_sync_engine::backup::TableSelection;
use db_sync_engine::destination::{DestinationCreate, DestinationKind, S3Destination};
use db_sync_engine::mask::MaskRule;
use db_sync_engine::plan::SyncPlanCreate;
use db_sync_engine::profile::{DbConfig, ProfileCreate, ToolOverrides};
use db_sync_engine::retention::RetentionPolicy;
use db_sync_engine::share::{self, ConfigBundle};
use db_sync_engine::store::Store;
use db_sync_engine::types::{Engine, EnvironmentTag};

async fn store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("test.db"))
        .await
        .expect("open store");
    (store, dir)
}

async fn seed(store: &Store) {
    let profile = store
        .create_profile(ProfileCreate {
            name: "prod-eu".into(),
            engine: Engine::Mysql,
            environment: EnvironmentTag::Prod,
            ssh: None,
            db: DbConfig {
                host: "db.internal".into(),
                port: 3306,
                user: "backup".into(),
                database: Some("app".into()),
            },
            tool_overrides: ToolOverrides::default(),
        })
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
            masking: vec![MaskRule::email("users", "email")],
        })
        .await
        .unwrap();

    store
        .create_destination(DestinationCreate {
            name: "off-site".into(),
            kind: DestinationKind::S3(S3Destination {
                endpoint: "https://s3.eu-west-1.amazonaws.com".into(),
                region: "eu-west-1".into(),
                bucket: "acme-backups".into(),
                prefix: "prod".into(),
                path_style: false,
                access_key_id: "AKIDEXAMPLE".into(),
            }),
            enabled: true,
            retention: RetentionPolicy {
                keep_last: Some(30),
                max_age_days: None,
            },
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn a_bundle_carries_the_configuration_and_none_of_the_access() {
    let (store, _dir) = store().await;
    seed(&store).await;

    let json = share::export(&store).await.unwrap().to_json().unwrap();

    // The configuration is there.
    assert!(json.contains("prod-eu"));
    assert!(json.contains("db.internal"));
    assert!(json.contains("nightly"));
    assert!(json.contains("audit_log"));
    assert!(json.contains("acme-backups"));
    assert!(
        json.contains("AKIDEXAMPLE"),
        "an access key id identifies which credential to use and is not one"
    );

    // The access is not.
    let lowered = json.to_lowercase();
    for forbidden in [
        "password",
        "\"secret",
        "secret_access_key",
        "passphrase\":\"",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "{forbidden:?} appears in an export"
        );
    }
}

#[tokio::test]
async fn importing_onto_a_fresh_machine_recreates_everything_and_says_what_is_missing() {
    let (source, _a) = store().await;
    seed(&source).await;
    let bundle = share::export(&source).await.unwrap();

    let (target, _b) = store().await;
    let report = share::import(&target, &bundle).await.unwrap();

    assert_eq!(report.profiles_created, vec!["prod-eu"]);
    assert_eq!(report.plans_created, vec!["nightly"]);
    assert_eq!(report.destinations_created, vec!["off-site"]);

    // Named individually. "Some of these need credentials" is not something
    // anyone acts on.
    assert_eq!(report.needs_credentials, vec!["prod-eu"]);
    assert_eq!(report.destinations_needing_keys, vec!["off-site"]);

    let profiles = target.list_profiles().await.unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].db.host, "db.internal");
    assert_eq!(profiles[0].environment, EnvironmentTag::Prod);

    let plans = target.list_sync_plans(profiles[0].id).await.unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].selections.len(), 2);
    assert_eq!(
        plans[0].masking.len(),
        1,
        "which columns are sensitive is knowledge worth sharing"
    );
}

#[tokio::test]
async fn an_imported_destination_arrives_switched_off() {
    // It has no credential, and an enabled destination that cannot upload
    // fails every backup until somebody notices.
    let (source, _a) = store().await;
    seed(&source).await;
    let bundle = share::export(&source).await.unwrap();

    let (target, _b) = store().await;
    share::import(&target, &bundle).await.unwrap();

    let destinations = target.list_destinations().await.unwrap();
    assert_eq!(destinations.len(), 1);
    assert!(
        !destinations[0].enabled,
        "an unusable destination must not be armed on arrival"
    );
    assert!(target.list_enabled_destinations().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_second_import_updates_rather_than_duplicating() {
    // Two machines generate different ids for the same server. Matching on id
    // would duplicate everything on every import.
    let (source, _a) = store().await;
    seed(&source).await;
    let bundle = share::export(&source).await.unwrap();

    let (target, _b) = store().await;
    share::import(&target, &bundle).await.unwrap();
    let second = share::import(&target, &bundle).await.unwrap();

    assert_eq!(second.profiles_updated, vec!["prod-eu"]);
    assert!(second.profiles_created.is_empty());
    assert_eq!(target.list_profiles().await.unwrap().len(), 1);

    let profiles = target.list_profiles().await.unwrap();
    assert_eq!(
        target.list_sync_plans(profiles[0].id).await.unwrap().len(),
        1
    );
    assert_eq!(target.list_destinations().await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_import_never_removes_what_the_bundle_omits() {
    // "I shared my config with you" must not be able to delete a connection
    // somebody relies on.
    let (target, _b) = store().await;
    target
        .create_profile(ProfileCreate {
            name: "my-own-box".into(),
            engine: Engine::Postgres,
            environment: EnvironmentTag::Dev,
            ssh: None,
            db: DbConfig {
                host: "127.0.0.1".into(),
                port: 5432,
                user: "me".into(),
                database: None,
            },
            tool_overrides: ToolOverrides::default(),
        })
        .await
        .unwrap();

    let (source, _a) = store().await;
    seed(&source).await;
    let bundle = share::export(&source).await.unwrap();
    share::import(&target, &bundle).await.unwrap();

    let names: Vec<String> = target
        .list_profiles()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert!(names.contains(&"my-own-box".to_string()));
    assert!(names.contains(&"prod-eu".to_string()));
}

#[tokio::test]
async fn a_plan_whose_connection_is_missing_is_reported_not_dropped() {
    // The sender believed they shared a working plan. Silently discarding it
    // means the receiver never finds out it did not arrive.
    let (source, _a) = store().await;
    seed(&source).await;
    let mut bundle = share::export(&source).await.unwrap();
    bundle.profiles.clear();

    let (target, _b) = store().await;
    let report = share::import(&target, &bundle).await.unwrap();

    assert!(report.plans_created.is_empty());
    assert_eq!(report.orphaned_plans.len(), 1);
    assert!(report.orphaned_plans[0].contains("prod-eu"));
}

#[tokio::test]
async fn an_empty_store_exports_an_empty_bundle_rather_than_failing() {
    let (store, _dir) = store().await;
    let bundle = share::export(&store).await.unwrap();
    assert!(bundle.profiles.is_empty());
    assert!(bundle.plans.is_empty());
    assert!(bundle.destinations.is_empty());

    // And it round-trips, so a fresh install can hand one over.
    let parsed = ConfigBundle::from_json(&bundle.to_json().unwrap()).unwrap();
    assert_eq!(parsed.bundle_version, db_sync_engine::share::BUNDLE_VERSION);
}

#[tokio::test]
async fn an_import_is_recorded_whichever_surface_ran_it() {
    // Recorded inside `share::import` rather than at each call site. An entry
    // that only appeared when the import happened to go through the GUI would
    // be worse than none, because its absence would mean nothing.
    let (source, _a) = store().await;
    seed(&source).await;
    let bundle = share::export(&source).await.unwrap();

    let (target, _b) = store().await;
    share::import(&target, &bundle).await.unwrap();

    let entries = target.list_audit(10).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].action, "config.imported");
    assert!(
        entries[0].detail.contains("1 connection"),
        "the record should say what arrived: {:?}",
        entries[0]
    );
}
