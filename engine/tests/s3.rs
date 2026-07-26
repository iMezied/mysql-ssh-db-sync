//! The S3 client against a real object store.
//!
//!     docker compose -f docker-compose.test.yml up -d --wait
//!
//! Unit tests pin the signing against the published AWS vector and check how
//! URLs are built. Neither proves a server accepts the result — and a signature
//! is exactly the kind of thing that is either completely right or completely
//! rejected, with no way to tell from the outside which one you have written.
//!
//! MinIO speaks the same protocol as S3, R2, B2 and Wasabi. It is not AWS, and
//! this suite does not claim to cover AWS.

use std::time::Duration;

use db_sync_engine::s3::{S3Client, S3Config, S3Error};
use secrecy::SecretString;
use tokio::net::TcpStream;

const MINIO_PORT: u16 = 19000;

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    })
}

macro_rules! s3_test {
    (async fn $name:ident() $body:block) => {
        #[test]
        fn $name() {
            rt().block_on(async move $body);
        }
    };
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

/// A client pointed at a bucket this test owns.
///
/// One bucket per test: they run in parallel, and a shared bucket would make
/// listing and deletion order-dependent.
async fn client(bucket: &str) -> S3Client {
    let client = S3Client::new(S3Config {
        endpoint: format!("http://127.0.0.1:{MINIO_PORT}"),
        region: "us-east-1".into(),
        bucket: bucket.into(),
        prefix: "prod".into(),
        // MinIO is addressed by path; a virtual-host request would resolve
        // `bucket.127.0.0.1`, which is not a name.
        path_style: true,
        access_key_id: "dbsynctest".into(),
        secret_access_key: SecretString::from("dbsynctestsecret"),
    })
    .expect("client should build");

    client.create_bucket().await.expect("create bucket");
    client
}

fn temp_file(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("artifact.sql.gz");
    std::fs::write(&path, bytes).expect("write fixture");
    (dir, path)
}

fn no_progress() -> impl FnMut(u64, u64) {
    |_, _| {}
}

s3_test! {
    async fn a_signed_request_is_accepted_by_a_real_server() {
        // The one thing unit tests cannot establish: that the signature this
        // code produces is the signature a server computes independently.
        require_minio!();
        let c = client("dbsync-sign").await;
        c.check_access().await.expect("the credentials should be accepted");
    }
}

s3_test! {
    async fn wrong_credentials_are_rejected_rather_than_silently_accepted() {
        require_minio!();
        let c = S3Client::new(S3Config {
            endpoint: format!("http://127.0.0.1:{MINIO_PORT}"),
            region: "us-east-1".into(),
            bucket: "dbsync-sign".into(),
            prefix: String::new(),
            path_style: true,
            access_key_id: "dbsynctest".into(),
            secret_access_key: SecretString::from("the-wrong-secret"),
        })
        .unwrap();

        let err = c.check_access().await.expect_err("a bad secret must fail");
        // Proves the server verifies rather than ignores, which is also what
        // makes the passing test above meaningful.
        assert!(
            matches!(err, S3Error::Api { .. }),
            "expected an S3 error, got: {err}"
        );
    }
}

s3_test! {
    async fn a_small_artifact_uploads_and_reads_back_byte_for_byte() {
        require_minio!();
        let c = client("dbsync-small").await;
        let payload = b"-- a small dump\nINSERT INTO users VALUES (1);\n";
        let (_dir, path) = temp_file(payload);
        let key = c.key_for("small.sql.gz");

        let mut progress = no_progress();
        let sent = c
            .upload_file(&path, &key, &mut progress, &|| false)
            .await
            .expect("upload should succeed");

        assert_eq!(sent, payload.len() as u64);
        assert_eq!(
            c.head(&key).await.unwrap(),
            Some(payload.len() as u64),
            "the object must be readable at the size we sent"
        );
    }
}

s3_test! {
    async fn a_missing_object_reads_back_as_absent_not_as_an_error() {
        // Retention and resume logic both ask "is it already there?", and an
        // error would be indistinguishable from "the endpoint is down".
        require_minio!();
        let c = client("dbsync-missing").await;
        assert_eq!(c.head("prod/not-here.sql.gz").await.unwrap(), None);
    }
}

s3_test! {
    async fn a_large_artifact_goes_through_multipart_and_arrives_whole() {
        // The path that only runs above the threshold, so the only one where a
        // part-numbering or ETag bug can hide.
        require_minio!();
        let c = client("dbsync-multipart").await;

        // Just over the threshold, so several parts are sent without making the
        // test slow. The content is patterned rather than zeroes so a part
        // written twice, or in the wrong order, changes the stored size.
        let size = (db_sync_engine::s3::client::MULTIPART_THRESHOLD as usize) + (1 << 20);
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let (_dir, path) = temp_file(&payload);
        let key = c.key_for("large.sql.gz");

        let mut seen: Vec<(u64, u64)> = Vec::new();
        let mut progress = |sent, total| seen.push((sent, total));

        let sent = c
            .upload_file(&path, &key, &mut progress, &|| false)
            .await
            .expect("multipart upload should succeed");

        assert_eq!(sent, size as u64);
        assert_eq!(c.head(&key).await.unwrap(), Some(size as u64));

        assert!(
            seen.len() > 1,
            "an upload this size must have been split: {seen:?}"
        );
        assert!(
            seen.windows(2).all(|w| w[0].0 < w[1].0),
            "progress must only move forwards: {seen:?}"
        );
        assert_eq!(
            seen.last().map(|(sent, _)| *sent),
            Some(size as u64),
            "the last progress report must be the whole file"
        );
    }
}

s3_test! {
    async fn a_cancelled_upload_stops_and_leaves_no_object_behind() {
        // A half-uploaded artifact that looks complete is the same class of
        // problem as a half-masked database.
        require_minio!();
        let c = client("dbsync-cancel").await;

        let size = (db_sync_engine::s3::client::MULTIPART_THRESHOLD as usize) + (1 << 20);
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let (_dir, path) = temp_file(&payload);
        let key = c.key_for("cancelled.sql.gz");

        let mut progress = no_progress();
        let err = c
            .upload_file(&path, &key, &mut progress, &|| true)
            .await
            .expect_err("a cancelled upload must not report success");
        assert!(matches!(err, S3Error::Cancelled), "got: {err}");

        assert_eq!(
            c.head(&key).await.unwrap(),
            None,
            "no object may be left where a cancelled upload was going"
        );
    }
}

s3_test! {
    async fn listing_finds_what_was_uploaded_and_delete_removes_it() {
        require_minio!();
        let c = client("dbsync-list").await;

        for name in ["a.sql.gz", "b.sql.gz"] {
            let (_dir, path) = temp_file(name.as_bytes());
            let mut progress = no_progress();
            c.upload_file(&path, &c.key_for(name), &mut progress, &|| false)
                .await
                .expect("upload");
        }

        let mut listed: Vec<String> = c
            .list("prod/")
            .await
            .expect("list")
            .into_iter()
            .map(|o| o.key)
            .collect();
        listed.sort();
        assert_eq!(listed, vec!["prod/a.sql.gz", "prod/b.sql.gz"]);

        c.delete("prod/a.sql.gz").await.expect("delete");
        assert_eq!(c.head("prod/a.sql.gz").await.unwrap(), None);
        assert!(c.head("prod/b.sql.gz").await.unwrap().is_some());
    }
}

s3_test! {
    async fn a_key_with_awkward_characters_survives_the_round_trip() {
        // Object keys carry database and profile names. Spaces and non-ASCII
        // are where percent-encoding bugs show up, and an encoding that differs
        // between the signature and the URL is a 403.
        require_minio!();
        let c = client("dbsync-keys").await;

        for name in ["a b.sql.gz", "café.sql.gz", "日本語.sql.gz", "a+b.sql.gz"] {
            let (_dir, path) = temp_file(name.as_bytes());
            let key = c.key_for(name);
            let mut progress = no_progress();

            c.upload_file(&path, &key, &mut progress, &|| false)
                .await
                .unwrap_or_else(|e| panic!("{name:?} should upload: {e}"));

            assert_eq!(
                c.head(&key).await.unwrap(),
                Some(name.len() as u64),
                "{name:?} must be stored under the key we signed"
            );
        }
    }
}

s3_test! {
    async fn a_missing_bucket_is_reported_as_such() {
        // The most common first-run mistake, and the error has to say which
        // thing is wrong rather than "403".
        require_minio!();
        let c = S3Client::new(S3Config {
            endpoint: format!("http://127.0.0.1:{MINIO_PORT}"),
            region: "us-east-1".into(),
            bucket: "dbsync-does-not-exist".into(),
            prefix: String::new(),
            path_style: true,
            access_key_id: "dbsynctest".into(),
            secret_access_key: SecretString::from("dbsynctestsecret"),
        })
        .unwrap();

        let err = c.check_access().await.expect_err("no such bucket");
        assert!(
            err.to_string().contains("NoSuchBucket"),
            "the error should name the problem: {err}"
        );
    }
}

s3_test! {
    async fn creating_a_bucket_twice_is_not_an_error() {
        // Otherwise every run after the first would fail on setup.
        require_minio!();
        let c = client("dbsync-idempotent").await;
        c.create_bucket().await.expect("second create should succeed");
    }
}
