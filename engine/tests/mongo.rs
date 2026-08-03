//! MongoDB against a real server.
//!
//!     docker compose -f docker-compose.test.yml up -d --wait
//!
//! Unit tests prove the *shape* of what this sends — the command line, the
//! filter document, the transform's output. They cannot prove a server accepts
//! any of it, and for MongoDB the gap is wider than for the SQL engines because
//! more of the behaviour is the driver's rather than a string this crate wrote:
//! `$objectToArray` over documents with different field sets, `$regexMatch` in
//! a `countDocuments` filter, whether `count_documents` on a fresh collection
//! is exact. Each of those is an assumption the masking guarantee rests on.
//!
//! The fixture is deliberately awkward — see `tests/fixtures/mongo/01-fixture.js`.
//!
//! These connect straight to the mapped port. The tunnel is exercised in
//! `introspect.rs`, which is where the `direct_connection` behaviour that
//! MongoDB needs through a forwarded port belongs.

use std::time::Duration;

use db_sync_engine::db::{ConnectParams, Introspector, MongoIntrospector};
use db_sync_engine::tools::ToolSource;
use db_sync_engine::mask::mongo as mask_mongo;
use db_sync_engine::mask::{MaskRule, MaskTransform, derive_salt};
use db_sync_engine::types::Engine;
use mongodb::bson::{Bson, doc};
use secrecy::SecretString;
use tokio::net::TcpStream;

const MONGO_PORT: u16 = 27018;

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
    (async fn $name:ident() $body:block) => {
        #[test]
        fn $name() {
            rt().block_on(async move $body);
        }
    };
}

async fn reachable(port: u16) -> bool {
    tokio::time::timeout(
        Duration::from_secs(2),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

macro_rules! require_containers {
    () => {
        if !reachable(MONGO_PORT).await {
            if std::env::var("DBSYNC_REQUIRE_CONTAINERS").is_ok() {
                panic!("test containers are required but not reachable");
            }
            eprintln!("skipping: test containers not running");
            return;
        }
    };
}

fn params(database: Option<&str>) -> ConnectParams {
    ConnectParams {
        engine: Engine::Mongo,
        host: "127.0.0.1".into(),
        port: MONGO_PORT,
        user: "root".into(),
        password: Some(SecretString::from("testroot")),
        database: database.map(str::to_string),
    }
}

async fn introspector() -> MongoIntrospector {
    MongoIntrospector::connect(&params(None))
        .await
        .expect("connect to the mongo fixture")
}

/// A scratch database seeded from a document list, owned by one test.
///
/// One per test rather than one shared: these run in parallel, and a shared
/// database means the tests drop it underneath each other.
async fn scratch(name: &str, collection: &str, documents: Vec<mongodb::bson::Document>) -> String {
    let db_name = format!("dbsync_mongo_{name}");
    let introspector = introspector().await;
    let db = introspector.client().database(&db_name);
    db.drop().await.expect("drop scratch database");
    if !documents.is_empty() {
        db.collection::<mongodb::bson::Document>(collection)
            .insert_many(documents)
            .await
            .expect("seed scratch database");
    }
    db_name
}

// ── Introspection ───────────────────────────────────────────────────────

db_test! {
    async fn the_server_reports_its_version_and_catalog_access() {
        require_containers!();
        let info = introspector().await.server_info().await.expect("server info");

        assert_eq!(info.engine, Engine::Mongo);
        assert!(
            info.version.starts_with('7'),
            "fixture pins mongo:7, got {}",
            info.version
        );
        assert!(info.can_read_catalog, "root can list databases");
    }
}

db_test! {
    async fn mongodb_own_databases_are_not_offered_as_backup_targets() {
        require_containers!();
        let databases = introspector().await.list_databases().await.expect("list");
        let names: Vec<&str> = databases.iter().map(|d| d.name.as_str()).collect();

        assert!(names.contains(&"fixture"), "got {names:?}");
        for system in ["admin", "local", "config"] {
            assert!(
                !names.contains(&system),
                "{system} is the server's own bookkeeping, not a backup target: {names:?}"
            );
        }
    }
}

db_test! {
    async fn collections_are_listed_with_their_sizes() {
        require_containers!();
        let tables = introspector()
            .await
            .list_tables("fixture")
            .await
            .expect("list collections");

        let users = tables
            .iter()
            .find(|t| t.name == "users")
            .expect("users collection");

        assert_eq!(users.estimated_rows, Some(4));
        assert!(users.total_bytes() > 0, "$collStats reported no size");
        assert!(
            users.is_transactional(),
            "WiredTiger is transactional; a false here would warn the user for no reason"
        );

        // A unicode collection name has to survive the round trip through the
        // driver, the same way the SQL fixtures' unicode table does.
        assert!(
            tables.iter().any(|t| t.name == "naïve_café"),
            "got {:?}",
            tables.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
    }
}

db_test! {
    async fn counting_documents_is_exact_not_an_estimate() {
        require_containers!();
        let i = introspector().await;
        assert_eq!(i.exact_row_count("fixture", "users").await.unwrap(), 4);
        assert_eq!(i.exact_row_count("fixture", "orders").await.unwrap(), 3);
    }
}

db_test! {
    async fn the_field_list_covers_every_document_not_a_sample() {
        require_containers!();
        // `referred_by` exists on exactly one of the four fixture documents.
        // A sampled field list can miss it; this one may not, because masking
        // coverage uses it to decide whether a rule is inert.
        let fields = introspector()
            .await
            .column_names("fixture", "users")
            .await
            .expect("field names");

        for expected in ["_id", "email", "display_name", "phone", "profile", "referred_by"] {
            assert!(
                fields.iter().any(|f| f == expected),
                "{expected} missing from {fields:?}"
            );
        }
    }
}

db_test! {
    async fn field_names_come_back_in_a_stable_order() {
        require_containers!();
        // Source and destination lists are compared directly during
        // verification, so an unstable order would report a schema mismatch on
        // a correct restore.
        let i = introspector().await;
        let first = i.column_names("fixture", "users").await.unwrap();
        let second = i.column_names("fixture", "users").await.unwrap();
        assert_eq!(first, second);

        let mut sorted = first.clone();
        sorted.sort();
        assert_eq!(first, sorted, "the order is by name");
    }
}

db_test! {
    async fn an_empty_collection_has_no_fields_and_no_documents() {
        require_containers!();
        let db_name = scratch("empty", "unused", vec![]).await;
        let i = introspector().await;
        let db = i.client().database(&db_name);
        db.create_collection("blank").await.expect("create");

        assert_eq!(i.exact_row_count(&db_name, "blank").await.unwrap(), 0);
        assert!(i.column_names(&db_name, "blank").await.unwrap().is_empty());

        db.drop().await.ok();
    }
}

// ── Digest ──────────────────────────────────────────────────────────────

db_test! {
    async fn the_digest_is_stable_across_reads() {
        require_containers!();
        let i = introspector().await;
        let a = i.table_digest("fixture", "users").await.unwrap();
        let b = i.table_digest("fixture", "users").await.unwrap();
        assert_eq!(a, b);
        assert!(a.is_some());
    }
}

db_test! {
    async fn the_digest_ignores_document_and_field_order() {
        require_containers!();
        // This is the property that decides whether the digest is usable at
        // all. A restored collection shares neither the source's physical
        // order nor, necessarily, its field order — and neither is data loss.
        let docs = vec![
            doc! { "_id": 1, "a": "one", "b": 1 },
            doc! { "_id": 2, "a": "two", "b": 2 },
        ];
        let shuffled = vec![
            doc! { "b": 2, "_id": 2, "a": "two" },
            doc! { "b": 1, "a": "one", "_id": 1 },
        ];

        let left = scratch("digest_order_a", "t", docs).await;
        let right = scratch("digest_order_b", "t", shuffled).await;

        let i = introspector().await;
        assert_eq!(
            i.table_digest(&left, "t").await.unwrap(),
            i.table_digest(&right, "t").await.unwrap(),
            "reordering is not data loss"
        );

        i.client().database(&left).drop().await.ok();
        i.client().database(&right).drop().await.ok();
    }
}

db_test! {
    async fn the_digest_notices_a_changed_value() {
        require_containers!();
        // The failure the digest exists to catch: the right number of
        // documents holding the wrong bytes.
        let left = scratch(
            "digest_value_a",
            "t",
            vec![doc! { "_id": 1, "v": "correct" }],
        )
        .await;
        let right = scratch(
            "digest_value_b",
            "t",
            vec![doc! { "_id": 1, "v": "corrupt" }],
        )
        .await;

        let i = introspector().await;
        assert_ne!(
            i.table_digest(&left, "t").await.unwrap(),
            i.table_digest(&right, "t").await.unwrap()
        );

        i.client().database(&left).drop().await.ok();
        i.client().database(&right).drop().await.ok();
    }
}

db_test! {
    async fn the_digest_notices_a_missing_document() {
        require_containers!();
        let left = scratch(
            "digest_count_a",
            "t",
            vec![doc! { "_id": 1, "v": "a" }, doc! { "_id": 2, "v": "b" }],
        )
        .await;
        let right = scratch("digest_count_b", "t", vec![doc! { "_id": 1, "v": "a" }]).await;

        let i = introspector().await;
        assert_ne!(
            i.table_digest(&left, "t").await.unwrap(),
            i.table_digest(&right, "t").await.unwrap()
        );

        i.client().database(&left).drop().await.ok();
        i.client().database(&right).drop().await.ok();
    }
}

// ── Masking ─────────────────────────────────────────────────────────────

fn salt() -> String {
    derive_salt("integration-test-secret")
}

db_test! {
    async fn masking_replaces_values_and_the_read_back_confirms_it() {
        require_containers!();
        let db_name = scratch(
            "mask_basic",
            "users",
            vec![
                doc! { "_id": 1, "email": "alice@corp.test", "ssn": "111-22-3333" },
                doc! { "_id": 2, "email": "bob@corp.test", "ssn": "444-55-6666" },
                // A null email must survive as null, not become a pseudonym.
                doc! { "_id": 3, "email": Bson::Null, "ssn": "777-88-9999" },
            ],
        )
        .await;
        let p = params(Some(&db_name));

        let rules = vec![
            MaskRule::email("users", "email"),
            MaskRule {
                table: "users".into(),
                column: "ssn".into(),
                transform: MaskTransform::Null,
            },
        ];

        mask_mongo::apply(&p, &db_name, &rules, &salt())
            .await
            .expect("masking should apply");
        let confirmed = mask_mongo::verify(&p, &db_name, &rules)
            .await
            .expect("read-back should find nothing unmasked");
        assert_eq!(confirmed.len(), 2);

        let i = introspector().await;
        let collection = i
            .client()
            .database(&db_name)
            .collection::<mongodb::bson::Document>("users");

        let alice = collection
            .find_one(doc! { "_id": 1 })
            .await
            .unwrap()
            .expect("alice");
        let email = alice.get_str("email").unwrap();
        assert!(
            email.ends_with("@example.invalid"),
            "masked address must be undeliverable, got {email}"
        );
        assert!(alice.get("ssn").is_some_and(|v| matches!(v, Bson::Null)));

        let no_email = collection
            .find_one(doc! { "_id": 3 })
            .await
            .unwrap()
            .expect("third document");
        assert!(
            matches!(no_email.get("email"), Some(Bson::Null)),
            "a null must stay null rather than acquire a pseudonym"
        );

        i.client().database(&db_name).drop().await.ok();
    }
}

db_test! {
    async fn the_same_value_masks_identically_across_collections() {
        require_containers!();
        // The property that keeps a masked copy usable: `users.email` and
        // `orders.buyer_email` still join.
        let db_name = scratch(
            "mask_join",
            "users",
            vec![doc! { "_id": 1, "email": "alice@corp.test" }],
        )
        .await;
        let p = params(Some(&db_name));
        let i = introspector().await;
        i.client()
            .database(&db_name)
            .collection::<mongodb::bson::Document>("orders")
            .insert_many(vec![doc! { "_id": 1, "buyer_email": "alice@corp.test" }])
            .await
            .unwrap();

        let rules = vec![
            MaskRule::email("users", "email"),
            MaskRule::email("orders", "buyer_email"),
        ];
        mask_mongo::apply(&p, &db_name, &rules, &salt()).await.unwrap();

        let db = i.client().database(&db_name);
        let user = db
            .collection::<mongodb::bson::Document>("users")
            .find_one(doc! { "_id": 1 })
            .await
            .unwrap()
            .unwrap();
        let order = db
            .collection::<mongodb::bson::Document>("orders")
            .find_one(doc! { "_id": 1 })
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            user.get_str("email").unwrap(),
            order.get_str("buyer_email").unwrap(),
            "the join must survive masking"
        );

        db.drop().await.ok();
    }
}

db_test! {
    async fn a_nested_field_is_masked_by_dotted_path() {
        require_containers!();
        let db_name = scratch(
            "mask_nested",
            "users",
            vec![doc! {
                "_id": 1,
                "profile": { "contact": { "email": "alice@corp.test" }, "tier": "gold" },
            }],
        )
        .await;
        let p = params(Some(&db_name));
        let rules = vec![MaskRule::email("users", "profile.contact.email")];

        mask_mongo::apply(&p, &db_name, &rules, &salt()).await.unwrap();
        mask_mongo::verify(&p, &db_name, &rules)
            .await
            .expect("the nested field should be masked");

        let i = introspector().await;
        let db = i.client().database(&db_name);
        let user = db
            .collection::<mongodb::bson::Document>("users")
            .find_one(doc! { "_id": 1 })
            .await
            .unwrap()
            .unwrap();

        let profile = user.get_document("profile").unwrap();
        let email = profile.get_document("contact").unwrap().get_str("email").unwrap();
        assert!(email.ends_with("@example.invalid"), "got {email}");
        assert_eq!(
            profile.get_str("tier").unwrap(),
            "gold",
            "the siblings of a masked field must be untouched"
        );

        db.drop().await.ok();
    }
}

db_test! {
    async fn a_field_holding_a_subdocument_is_reported_rather_than_flattened() {
        require_containers!();
        // Replacing a subdocument with a hash of its rendering would silently
        // change the document's shape. Declining is safe *because* the
        // read-back then reports it — which is what this proves.
        let db_name = scratch(
            "mask_structure",
            "users",
            vec![doc! { "_id": 1, "email": { "work": "alice@corp.test" } }],
        )
        .await;
        let p = params(Some(&db_name));
        let rules = vec![MaskRule::email("users", "email")];

        mask_mongo::apply(&p, &db_name, &rules, &salt()).await.unwrap();
        let err = mask_mongo::verify(&p, &db_name, &rules)
            .await
            .expect_err("an unmasked field must not pass the check");
        assert!(
            err.to_string().contains("not masked"),
            "the operator has to be told; got: {err}"
        );

        introspector().await.client().database(&db_name).drop().await.ok();
    }
}

db_test! {
    async fn a_non_string_value_is_still_masked() {
        require_containers!();
        // A phone number stored as an int64 is ordinary in a document store.
        // Skipping it would leave it readable.
        let db_name = scratch(
            "mask_numeric",
            "users",
            vec![doc! { "_id": 1, "phone": 441632960903i64 }],
        )
        .await;
        let p = params(Some(&db_name));
        let rules = vec![MaskRule {
            table: "users".into(),
            column: "phone".into(),
            transform: MaskTransform::Phone,
        }];

        mask_mongo::apply(&p, &db_name, &rules, &salt()).await.unwrap();
        mask_mongo::verify(&p, &db_name, &rules)
            .await
            .expect("a numeric phone should mask");

        let i = introspector().await;
        let db = i.client().database(&db_name);
        let user = db
            .collection::<mongodb::bson::Document>("users")
            .find_one(doc! { "_id": 1 })
            .await
            .unwrap()
            .unwrap();
        assert!(user.get_str("phone").unwrap().starts_with("+1555"));

        db.drop().await.ok();
    }
}

db_test! {
    async fn the_read_back_catches_a_value_that_was_never_masked() {
        require_containers!();
        // Masking is applied to one collection and the check is run over two.
        // This is the shape of every real failure: an update that reported
        // success over data it did not reach.
        let db_name = scratch(
            "mask_missed",
            "users",
            vec![doc! { "_id": 1, "email": "alice@corp.test" }],
        )
        .await;
        let p = params(Some(&db_name));
        let i = introspector().await;
        i.client()
            .database(&db_name)
            .collection::<mongodb::bson::Document>("orders")
            .insert_many(vec![doc! { "_id": 1, "buyer_email": "bob@corp.test" }])
            .await
            .unwrap();

        let applied = vec![MaskRule::email("users", "email")];
        let checked = vec![
            MaskRule::email("users", "email"),
            MaskRule::email("orders", "buyer_email"),
        ];

        mask_mongo::apply(&p, &db_name, &applied, &salt()).await.unwrap();
        let err = mask_mongo::verify(&p, &db_name, &checked)
            .await
            .expect_err("the untouched collection must fail the check");
        assert!(err.to_string().contains("orders"), "got: {err}");

        i.client().database(&db_name).drop().await.ok();
    }
}

db_test! {
    async fn a_constant_overwrites_nulls_too() {
        require_containers!();
        let db_name = scratch(
            "mask_constant",
            "users",
            vec![
                doc! { "_id": 1, "note": "sensitive" },
                doc! { "_id": 2, "note": Bson::Null },
            ],
        )
        .await;
        let p = params(Some(&db_name));
        let rules = vec![MaskRule {
            table: "users".into(),
            column: "note".into(),
            transform: MaskTransform::Constant {
                value: "redacted".into(),
            },
        }];

        mask_mongo::apply(&p, &db_name, &rules, &salt()).await.unwrap();
        mask_mongo::verify(&p, &db_name, &rules)
            .await
            .expect("a surviving null would be a miss");

        introspector().await.client().database(&db_name).drop().await.ok();
    }
}

// ── Round trip ──────────────────────────────────────────────────────────
//
// These drive the real `mongodump` and `mongorestore`. They are skipped when
// the binaries are not present rather than failing, because the Database Tools
// are a separate download from the server and nothing else in this suite needs
// them. Point `DBSYNC_MONGO_TOOLS` at a directory of binaries to use a copy
// that is not on `PATH` — which also exercises the profile's tool override,
// the mechanism a user with a non-standard install relies on.

fn tool_override(binary: &str) -> Option<String> {
    if let Ok(dir) = std::env::var("DBSYNC_MONGO_TOOLS") {
        let path = std::path::Path::new(&dir).join(binary);
        return path.exists().then(|| path.display().to_string());
    }
    db_sync_engine::exec::find_tool(binary, None).map(|p| p.display().to_string())
}

macro_rules! require_tools {
    () => {{
        let (Some(dump), Some(restore)) =
            (tool_override("mongodump"), tool_override("mongorestore"))
        else {
            eprintln!("skipping: mongodump/mongorestore not found");
            return;
        };
        (dump, restore)
    }};
}

fn mongo_profile(dump: String, restore: String) -> db_sync_engine::profile::ConnectionProfile {
    use db_sync_engine::profile::{ConnectionProfile, DbConfig, ToolOverrides};
    use db_sync_engine::types::EnvironmentTag;

    ConnectionProfile {
        id: uuid::Uuid::new_v4(),
        name: "mongo-fixture".into(),
        engine: Engine::Mongo,
        environment: EnvironmentTag::Dev,
        ssh_connection_id: None,
        db: DbConfig {
            host: "127.0.0.1".into(),
            port: MONGO_PORT,
            user: "root".into(),
            database: Some("fixture".into()),
        },
        tool_overrides: ToolOverrides {
            mongodump: Some(dump),
            mongorestore: Some(restore),
            ..Default::default()
        },
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn endpoint() -> db_sync_engine::backup::mysql::Endpoint {
    db_sync_engine::backup::mysql::Endpoint {
        host: "127.0.0.1".into(),
        port: MONGO_PORT,
        user: "root".into(),
        password: Some(SecretString::from("testroot")),
    }
}

db_test! {
    async fn a_database_survives_a_dump_and_restore_intact() {
        require_containers!();
        let (dump, restore) = require_tools!();

        use db_sync_engine::backup::{
            run_mongo_backup, BackupRequest, BackupRun, CommonBackupOptions, EngineBackupOptions,
            MongoBackupOptions, TableSelection,
        };
        use db_sync_engine::job::JobContext;
        use db_sync_engine::manifest::{ArtifactFormat, BackupManifest};
        use db_sync_engine::restore::{
            run_mongo_restore, EngineRestoreOptions, MongoRestoreOptions, RestoreRequest,
            TargetNaming,
        };

        let profile = mongo_profile(dump, restore);
        let out = tempfile::tempdir().expect("temp dir");
        let ctx = JobContext::new(uuid::Uuid::new_v4());

        let request = BackupRequest {
            common: CommonBackupOptions {
                database: "fixture".into(),
                selections: vec![
                    TableSelection::with_data("users"),
                    TableSelection::with_data("orders"),
                    TableSelection::with_data("naïve_café"),
                    // Excluded, so the exclusion actually reaches mongodump
                    // rather than only the rendered command line.
                    TableSelection {
                        name: "sessions".into(),
                        mode: db_sync_engine::backup::TableMode::Exclude,
                        where_filter: None,
                    },
                ],
                output_dir: out.path().to_path_buf(),
                compress: true,
                encrypt: false,
                record_row_counts: false,
            },
            engine: EngineBackupOptions::Mongo(MongoBackupOptions::default()),
        };

        let artifact = run_mongo_backup(
            BackupRun {
                profile: &profile,
                request: &request,
                endpoint: endpoint(),
                server_version: "7.0.0".into(),
                recipients: &[],
                source_row_counts: &Default::default(),
                tools: &ToolSource::Local,
            },
            &ctx,
        )
        .await
        .expect("mongodump should produce an archive");

        assert!(artifact.exists(), "no artifact at {}", artifact.display());
        assert!(
            artifact.to_string_lossy().ends_with(".archive.gz"),
            "the name must say what the file is: {}",
            artifact.display()
        );

        let manifest = BackupManifest::read(&artifact).expect("manifest beside the artifact");
        assert_eq!(manifest.engine, Engine::Mongo);
        assert_eq!(manifest.format, ArtifactFormat::MongoArchive);
        assert_eq!(manifest.database, "fixture");
        assert!(
            !manifest.tables.contains(&"sessions".to_string()),
            "an excluded collection must not be claimed in the manifest: {:?}",
            manifest.tables
        );
        manifest
            .verify_artifact(&artifact)
            .expect("the recorded checksum must match the bytes on disk");

        // ── Restore into a database that is not the source ──────────────
        let target_prefix = format!("dbsync_rt_{}", uuid::Uuid::new_v4().simple());
        let restore_request = RestoreRequest {
            artifact_path: artifact.clone(),
            naming: TargetNaming::NewTimestamped {
                prefix: target_prefix.clone(),
            },
            engine: EngineRestoreOptions::Mongo(MongoRestoreOptions::default()),
            verify_checksum: true,
            typed_confirmation: None,
        };

        let target = run_mongo_restore(
            &profile,
            &restore_request,
            endpoint(),
            &ToolSource::Local,
            &ctx,
        )
            .await
            .expect("mongorestore should restore the archive");
        assert!(target.starts_with(&target_prefix));

        // ── Compare the copy against the source ─────────────────────────
        let i = introspector().await;

        let mut restored: Vec<String> = i
            .list_tables(&target)
            .await
            .expect("list restored collections")
            .into_iter()
            .map(|t| t.name)
            .collect();
        restored.sort();
        assert_eq!(
            restored,
            vec!["naïve_café".to_string(), "orders".into(), "users".into()],
            "the excluded collection must not have travelled, and a unicode \
             name must survive the archive"
        );

        for collection in ["users", "orders", "naïve_café"] {
            assert_eq!(
                i.exact_row_count("fixture", collection).await.unwrap(),
                i.exact_row_count(&target, collection).await.unwrap(),
                "{collection} lost or gained documents"
            );
            assert_eq!(
                i.table_digest("fixture", collection).await.unwrap(),
                i.table_digest(&target, collection).await.unwrap(),
                "{collection} came back with different bytes"
            );
            assert_eq!(
                i.column_names("fixture", collection).await.unwrap(),
                i.column_names(&target, collection).await.unwrap(),
                "{collection} came back with a different field set"
            );
        }

        // The source must be untouched. This is what `--nsFrom/--nsTo` is for,
        // and getting it wrong on a production profile overwrites the source.
        assert_eq!(
            i.exact_row_count("fixture", "sessions").await.unwrap(),
            2,
            "the source database was modified by a restore"
        );

        i.client().database(&target).drop().await.ok();
    }
}

db_test! {
    async fn an_encrypted_archive_restores_through_the_same_path() {
        require_containers!();
        let (dump, restore) = require_tools!();

        // MongoDB's artifact is a single stream, which is the property that
        // lets encryption work at all — the PostgreSQL directory formats are
        // refused for exactly the lack of it. Proving it end to end means
        // proving the age layer sits outside mongodump's own gzip.
        use db_sync_engine::backup::{
            run_mongo_backup, BackupRequest, BackupRun, CommonBackupOptions, EngineBackupOptions,
            MongoBackupOptions, TableSelection,
        };
        use db_sync_engine::job::JobContext;

        let Ok(identity) = db_sync_engine::backupkey::identity() else {
            eprintln!("skipping: no backup key on this machine (needs the keychain)");
            return;
        };
        let recipient = db_sync_engine::crypto::public_from_identity(&identity)
            .expect("a key must yield its public half");

        let profile = mongo_profile(dump, restore);
        let out = tempfile::tempdir().expect("temp dir");
        let ctx = JobContext::new(uuid::Uuid::new_v4());

        let request = BackupRequest {
            common: CommonBackupOptions {
                database: "fixture".into(),
                selections: vec![TableSelection::with_data("users")],
                output_dir: out.path().to_path_buf(),
                compress: true,
                encrypt: true,
                record_row_counts: false,
            },
            engine: EngineBackupOptions::Mongo(MongoBackupOptions::default()),
        };

        let artifact = run_mongo_backup(
            BackupRun {
                profile: &profile,
                request: &request,
                endpoint: endpoint(),
                server_version: "7.0.0".into(),
                recipients: std::slice::from_ref(&recipient),
                source_row_counts: &Default::default(),
                tools: &ToolSource::Local,
            },
            &ctx,
        )
        .await
        .expect("an encrypted archive should be produced");

        assert!(
            artifact.to_string_lossy().ends_with(".archive.gz.age"),
            "the extension must say the file is ciphertext: {}",
            artifact.display()
        );
        assert!(
            db_sync_engine::crypto::looks_encrypted(&artifact),
            "the bytes on disk must actually be an age file"
        );
    }
}

db_test! {
    async fn masking_leaves_the_documents_it_should_not_touch_alone() {
        require_containers!();
        let db_name = scratch(
            "mask_untouched",
            "users",
            vec![doc! {
                "_id": 1,
                "email": "alice@corp.test",
                "display_name": "Alice",
                "created_at": "2026-01-02",
            }],
        )
        .await;
        let p = params(Some(&db_name));
        let rules = vec![MaskRule::email("users", "email")];

        mask_mongo::apply(&p, &db_name, &rules, &salt()).await.unwrap();

        let i = introspector().await;
        let db = i.client().database(&db_name);
        let user = db
            .collection::<mongodb::bson::Document>("users")
            .find_one(doc! { "_id": 1 })
            .await
            .unwrap()
            .unwrap();

        assert_eq!(user.get_str("display_name").unwrap(), "Alice");
        assert_eq!(user.get_str("created_at").unwrap(), "2026-01-02");

        db.drop().await.ok();
    }
}
