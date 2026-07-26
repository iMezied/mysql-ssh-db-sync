//! Shipping an artifact off-site, against a real object store.
//!
//!     docker compose -f docker-compose.test.yml up -d --wait
//!
//! `s3.rs` proves the transport works. This proves the *feature* works: that a
//! configured destination is actually read from the store, that its credential
//! is resolved from the keychain, that the manifest goes with the artifact, and
//! — the part that matters most — that a destination which cannot be reached is
//! reported as a failure rather than quietly skipped.
//!
//! Needs the OS keychain, because a destination's secret access key lives
//! there. Run locally with:
//!
//!     cargo test -p db-sync-engine --test offsite -- --ignored

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use db_sync_engine::destination::{Destination, DestinationCreate, DestinationKind, S3Destination};
use db_sync_engine::job::JobContext;
use db_sync_engine::manifest::{ArtifactFormat, BackupManifest, MANIFEST_VERSION, sha256_file};
use db_sync_engine::ops;
use db_sync_engine::retention::RetentionPolicy;
use db_sync_engine::s3::{S3Client, S3Config, S3Error};
use db_sync_engine::secrets::{self, SecretKind};
use db_sync_engine::store::Store;
use db_sync_engine::types::Engine;
use secrecy::SecretString;
use tokio::net::TcpStream;
use uuid::Uuid;

const MINIO_PORT: u16 = 19000;
const ACCESS_KEY: &str = "dbsynctest";
const SECRET_KEY: &str = "dbsynctestsecret";

fn endpoint() -> String {
    format!("http://127.0.0.1:{MINIO_PORT}")
}

async fn reachable() -> bool {
    tokio::time::timeout(
        Duration::from_secs(2),
        TcpStream::connect(("127.0.0.1", MINIO_PORT)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

macro_rules! require_minio {
    () => {
        if !reachable().await {
            if std::env::var("DBSYNC_REQUIRE_CONTAINERS").is_ok() {
                panic!("MinIO is required but not reachable on port {}", MINIO_PORT);
            }
            eprintln!("skipping: MinIO not running");
            return;
        }
    };
}

/// Removes every keychain entry a test created, however the test ended.
///
/// Without this, a failed assertion leaves a credential in the developer's
/// own login keychain with nothing left pointing at it.
#[derive(Default)]
struct Cleanup(Vec<Uuid>);

impl Cleanup {
    fn track(&mut self, id: Uuid) -> Uuid {
        self.0.push(id);
        id
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for id in &self.0 {
            let _ = secrets::delete_for_destination(*id);
        }
    }
}

async fn store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("test.db"))
        .await
        .expect("open store");
    (store, dir)
}

fn s3_kind(bucket: &str, prefix: &str) -> DestinationKind {
    DestinationKind::S3(S3Destination {
        endpoint: endpoint(),
        region: "us-east-1".into(),
        bucket: bucket.into(),
        prefix: prefix.into(),
        // MinIO is addressed by path; a virtual-host request would resolve
        // `bucket.127.0.0.1`, which is not a name.
        path_style: true,
        access_key_id: ACCESS_KEY.into(),
    })
}

/// Create the bucket, store the destination, and file its credential.
async fn destination(
    store: &Store,
    cleanup: &mut Cleanup,
    name: &str,
    bucket: &str,
    retention: RetentionPolicy,
) -> Destination {
    let bootstrap = S3Client::new(S3Config {
        endpoint: endpoint(),
        region: "us-east-1".into(),
        bucket: bucket.into(),
        prefix: String::new(),
        path_style: true,
        access_key_id: ACCESS_KEY.into(),
        secret_access_key: SecretString::from(SECRET_KEY),
    })
    .expect("bootstrap client");
    bootstrap.create_bucket().await.expect("create bucket");

    let created = store
        .create_destination(DestinationCreate {
            name: name.into(),
            kind: s3_kind(bucket, "prod"),
            enabled: true,
            retention,
        })
        .await
        .expect("create destination");

    cleanup.track(created.id);
    secrets::set_secret(created.id, SecretKind::ObjectStoreSecret, SECRET_KEY)
        .expect("store the credential");

    created
}

/// An artifact with a manifest beside it, as a real backup leaves on disk.
fn artifact(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write artifact");

    let manifest = BackupManifest {
        manifest_version: MANIFEST_VERSION,
        id: Uuid::new_v4(),
        source_profile_id: Uuid::new_v4(),
        source_profile_name: "prod".into(),
        engine: Engine::Mysql,
        server_version: "8.0.42".into(),
        dump_tool: "mysqldump".into(),
        dump_tool_version: "8.0.42".into(),
        database: "app".into(),
        created_at: Utc::now(),
        format: ArtifactFormat::SqlGz,
        tables: vec!["users".into()],
        tables_with_data: vec!["users".into()],
        options: serde_json::json!({}),
        artifact_filename: name.into(),
        size_bytes: body.len() as u64,
        sha256: sha256_file(&path).unwrap(),
        encrypted: false,
        encryption_recipients: Vec::new(),
    };
    manifest.write(&path).expect("write manifest");
    path
}

fn reader(bucket: &str) -> S3Client {
    S3Client::new(S3Config {
        endpoint: endpoint(),
        region: "us-east-1".into(),
        bucket: bucket.into(),
        prefix: String::new(),
        path_style: true,
        access_key_id: ACCESS_KEY.into(),
        secret_access_key: SecretString::from(SECRET_KEY),
    })
    .expect("reader")
}

fn ctx() -> JobContext {
    JobContext::new(Uuid::new_v4())
}

#[tokio::test]
#[ignore = "requires MinIO and an unlocked OS keychain"]
async fn an_artifact_and_its_manifest_both_arrive() {
    // The manifest is what makes an off-site copy checkable. An artifact
    // sitting in a bucket with no manifest can still be restored, but nothing
    // can say whether it arrived intact — which is most of the point.
    require_minio!();
    let (store, _dir) = store().await;
    let mut cleanup = Cleanup::default();
    destination(
        &store,
        &mut cleanup,
        "off-site",
        "dbsync-push",
        RetentionPolicy::default(),
    )
    .await;

    let work = tempfile::tempdir().unwrap();
    let payload = b"-- a dump\nINSERT INTO users VALUES (1);\n";
    let path = artifact(work.path(), "app_2026.sql.gz", payload);

    let results = ops::push_offsite(&path, &store, &ctx())
        .await
        .expect("the push should run");

    assert_eq!(results.len(), 1);
    assert!(results[0].succeeded(), "{:?}", results[0].error);
    assert_eq!(results[0].bytes, payload.len() as u64);
    assert_eq!(results[0].key, "prod/app_2026.sql.gz");

    let bucket = reader("dbsync-push");
    assert_eq!(
        bucket.head("prod/app_2026.sql.gz").await.unwrap(),
        Some(payload.len() as u64)
    );
    assert!(
        bucket
            .head("prod/app_2026.sql.gz.manifest.json")
            .await
            .unwrap()
            .is_some(),
        "the manifest must travel with the artifact"
    );
}

#[tokio::test]
#[ignore = "requires MinIO and an unlocked OS keychain"]
async fn a_destination_with_no_stored_credential_fails_rather_than_being_skipped() {
    // The failure this pins is the whole reason the feature is dangerous to
    // get wrong: a destination that is configured but unusable must not read
    // as "no destinations configured". The operator believes there is a second
    // copy either way; only one of those beliefs is true.
    require_minio!();
    let (store, _dir) = store().await;

    let created = store
        .create_destination(DestinationCreate {
            name: "no-credential".into(),
            kind: s3_kind("dbsync-push", "prod"),
            enabled: true,
            retention: RetentionPolicy::default(),
        })
        .await
        .unwrap();
    // Deliberately no secret filed for `created.id`.

    let work = tempfile::tempdir().unwrap();
    let path = artifact(work.path(), "app.sql.gz", b"x");

    let results = ops::push_offsite(&path, &store, &ctx()).await.unwrap();

    assert_eq!(results.len(), 1, "the destination must still be reported");
    assert!(!results[0].succeeded());
    assert_eq!(results[0].destination_id, created.id);
    let error = results[0].error.as_deref().unwrap();
    assert!(
        error.contains("no secret access key"),
        "the error should say what is missing: {error}"
    );
    assert_eq!(
        ops::push_failures(&results).len(),
        1,
        "and it must count as a failure the caller can act on"
    );
}

#[tokio::test]
#[ignore = "requires MinIO and an unlocked OS keychain"]
async fn a_disabled_destination_is_left_alone() {
    require_minio!();
    let (store, _dir) = store().await;
    let mut cleanup = Cleanup::default();

    let created = destination(
        &store,
        &mut cleanup,
        "paused",
        "dbsync-disabled",
        RetentionPolicy::default(),
    )
    .await;
    store
        .update_destination(
            created.id,
            db_sync_engine::destination::DestinationUpdate {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let work = tempfile::tempdir().unwrap();
    let path = artifact(work.path(), "app.sql.gz", b"x");

    let results = ops::push_offsite(&path, &store, &ctx()).await.unwrap();
    assert!(results.is_empty(), "a disabled destination is not tried");
    assert!(
        reader("dbsync-disabled")
            .head("prod/app.sql.gz")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
#[ignore = "requires MinIO and an unlocked OS keychain"]
async fn one_broken_destination_does_not_stop_a_working_one() {
    // Two off-site copies exist precisely so that losing one is survivable.
    // Aborting the second because the first failed would defeat that.
    require_minio!();
    let (store, _dir) = store().await;
    let mut cleanup = Cleanup::default();

    // Points at a bucket that does not exist.
    let broken = store
        .create_destination(DestinationCreate {
            name: "a-broken".into(),
            kind: s3_kind("dbsync-nonexistent-bucket", "prod"),
            enabled: true,
            retention: RetentionPolicy::default(),
        })
        .await
        .unwrap();
    cleanup.track(broken.id);
    secrets::set_secret(broken.id, SecretKind::ObjectStoreSecret, SECRET_KEY).unwrap();

    // Sorted by name, so "a-broken" is attempted first.
    destination(
        &store,
        &mut cleanup,
        "z-working",
        "dbsync-mixed",
        RetentionPolicy::default(),
    )
    .await;

    let work = tempfile::tempdir().unwrap();
    let path = artifact(work.path(), "app.sql.gz", b"payload");

    let results = ops::push_offsite(&path, &store, &ctx()).await.unwrap();
    assert_eq!(results.len(), 2);

    let broken_result = results
        .iter()
        .find(|r| r.destination_id == broken.id)
        .unwrap();
    assert!(!broken_result.succeeded());
    assert!(
        broken_result
            .error
            .as_deref()
            .unwrap()
            .contains("NoSuchBucket"),
        "the error must name the problem: {:?}",
        broken_result.error
    );

    let working = results
        .iter()
        .find(|r| r.destination_name == "z-working")
        .unwrap();
    assert!(working.succeeded(), "{:?}", working.error);
    assert!(
        reader("dbsync-mixed")
            .head("prod/app.sql.gz")
            .await
            .unwrap()
            .is_some(),
        "the working destination received the artifact"
    );

    assert_eq!(ops::push_failures(&results).len(), 1);
}

#[tokio::test]
#[ignore = "requires MinIO and an unlocked OS keychain"]
async fn off_site_retention_removes_old_artifacts_with_their_manifests() {
    require_minio!();
    let (store, _dir) = store().await;
    let mut cleanup = Cleanup::default();
    destination(
        &store,
        &mut cleanup,
        "retained",
        "dbsync-offsite-retention",
        RetentionPolicy {
            keep_last: Some(2),
            max_age_days: None,
        },
    )
    .await;

    let work = tempfile::tempdir().unwrap();
    for i in 0..3 {
        let path = artifact(work.path(), &format!("app_{i}.sql.gz"), b"payload");
        let results = ops::push_offsite(&path, &store, &ctx()).await.unwrap();
        assert!(results[0].succeeded(), "{:?}", results[0].error);
        // Object timestamps have second granularity, and the policy is about
        // which is newest — a tie would make the assertion below a coin flip.
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }

    let bucket = reader("dbsync-offsite-retention");
    let keys: Vec<String> = bucket
        .list("prod/")
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();

    assert!(
        !keys.contains(&"prod/app_0.sql.gz".to_string()),
        "the oldest artifact should have been removed: {keys:?}"
    );
    assert!(
        !keys.contains(&"prod/app_0.sql.gz.manifest.json".to_string()),
        "and its manifest with it, or a listing describes an object that is \
         no longer there: {keys:?}"
    );
    assert!(keys.contains(&"prod/app_2.sql.gz".to_string()), "{keys:?}");
    assert!(keys.contains(&"prod/app_1.sql.gz".to_string()), "{keys:?}");
}

#[tokio::test]
#[ignore = "requires MinIO and an unlocked OS keychain"]
async fn off_site_retention_does_not_count_manifests_as_copies() {
    // `keep_last: 1` with two artifacts must leave one artifact and its
    // manifest. Counting manifests as artifacts would leave half of what was
    // asked for, and the number would look right in the listing.
    require_minio!();
    let (store, _dir) = store().await;
    let mut cleanup = Cleanup::default();
    destination(
        &store,
        &mut cleanup,
        "manifest-aware",
        "dbsync-manifest-count",
        RetentionPolicy {
            keep_last: Some(1),
            max_age_days: None,
        },
    )
    .await;

    let work = tempfile::tempdir().unwrap();
    for i in 0..2 {
        let path = artifact(work.path(), &format!("app_{i}.sql.gz"), b"payload");
        ops::push_offsite(&path, &store, &ctx()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }

    let keys: Vec<String> = reader("dbsync-manifest-count")
        .list("prod/")
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();

    assert!(keys.contains(&"prod/app_1.sql.gz".to_string()), "{keys:?}");
    assert!(
        keys.contains(&"prod/app_1.sql.gz.manifest.json".to_string()),
        "the surviving artifact keeps its manifest: {keys:?}"
    );
    assert!(!keys.contains(&"prod/app_0.sql.gz".to_string()), "{keys:?}");
}

#[tokio::test]
#[ignore = "requires MinIO and an unlocked OS keychain"]
async fn testing_a_destination_accepts_a_good_one_and_names_a_bad_one() {
    require_minio!();
    let (store, _dir) = store().await;
    let mut cleanup = Cleanup::default();

    let good = destination(
        &store,
        &mut cleanup,
        "good",
        "dbsync-test-ok",
        RetentionPolicy::default(),
    )
    .await;
    ops::test_destination(&good)
        .await
        .expect("a reachable bucket with a valid credential");

    let bad = store
        .create_destination(DestinationCreate {
            name: "typo".into(),
            kind: s3_kind("dbsync-typo-bucket", "prod"),
            enabled: true,
            retention: RetentionPolicy::default(),
        })
        .await
        .unwrap();
    cleanup.track(bad.id);
    secrets::set_secret(bad.id, SecretKind::ObjectStoreSecret, SECRET_KEY).unwrap();

    let err = ops::test_destination(&bad)
        .await
        .expect_err("a bucket that does not exist");
    assert!(
        err.to_string().contains("NoSuchBucket"),
        "the most common first-run mistake must be named: {err}"
    );
}

#[tokio::test]
#[ignore = "requires MinIO and an unlocked OS keychain"]
async fn a_wrong_credential_is_reported_as_a_failed_push() {
    require_minio!();
    let (store, _dir) = store().await;
    let mut cleanup = Cleanup::default();

    let created = destination(
        &store,
        &mut cleanup,
        "wrong-key",
        "dbsync-wrong-key",
        RetentionPolicy::default(),
    )
    .await;
    // Replace the good credential with a bad one.
    secrets::set_secret(created.id, SecretKind::ObjectStoreSecret, "not-the-secret").unwrap();

    let work = tempfile::tempdir().unwrap();
    let path = artifact(work.path(), "app.sql.gz", b"x");

    let results = ops::push_offsite(&path, &store, &ctx()).await.unwrap();
    assert!(!results[0].succeeded());
    assert!(
        reader("dbsync-wrong-key")
            .head("prod/app.sql.gz")
            .await
            .unwrap()
            .is_none(),
        "nothing may be left behind by a rejected push"
    );
}

#[tokio::test]
#[ignore = "requires MinIO and an unlocked OS keychain"]
async fn forgetting_a_destination_takes_its_credential_with_it() {
    // A credential with nothing pointing at it is invisible in the app and
    // cannot be removed from inside it.
    require_minio!();
    let (store, _dir) = store().await;
    let mut cleanup = Cleanup::default();

    let created = destination(
        &store,
        &mut cleanup,
        "temporary",
        "dbsync-forget",
        RetentionPolicy::default(),
    )
    .await;
    assert!(secrets::has_secret(created.id, SecretKind::ObjectStoreSecret).unwrap());

    assert!(ops::forget_destination(&store, created.id).await.unwrap());

    assert!(store.get_destination(created.id).await.unwrap().is_none());
    assert!(
        !secrets::has_secret(created.id, SecretKind::ObjectStoreSecret).unwrap(),
        "the keychain entry must go with the row"
    );
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn no_destinations_configured_is_not_a_failure() {
    // The default state of the app. It must not manufacture a problem.
    let (store, _dir) = store().await;
    let work = tempfile::tempdir().unwrap();
    let path = artifact(work.path(), "app.sql.gz", b"x");

    let results = ops::push_offsite(&path, &store, &ctx()).await.unwrap();
    assert!(results.is_empty());
    assert!(ops::push_failures(&results).is_empty());
}

/// Guards against the transport quietly changing shape underneath the feature.
#[tokio::test]
#[ignore = "requires MinIO"]
async fn an_unreachable_endpoint_surfaces_as_a_transport_error() {
    let client = S3Client::new(S3Config {
        // A port nothing is listening on.
        endpoint: "http://127.0.0.1:1".into(),
        region: "us-east-1".into(),
        bucket: "anything".into(),
        prefix: String::new(),
        path_style: true,
        access_key_id: ACCESS_KEY.into(),
        secret_access_key: SecretString::from(SECRET_KEY),
    })
    .unwrap();

    let err = client
        .check_access()
        .await
        .expect_err("nothing is listening");
    assert!(matches!(err, S3Error::Transport { .. }), "{err}");
}
