//! Introspection integration tests.
//!
//! Every case runs *through an SSH tunnel*, because that is how the app will
//! use it — a direct-connection test would not exercise the path that matters.
//!
//!     docker compose -f docker-compose.test.yml up -d --wait
//!
//! ## Why these do not use `#[tokio::test]`
//!
//! `#[tokio::test]` builds a fresh runtime per test and drops it when the test
//! returns. A tunnel's accept loop and its forwarding tasks live on the runtime
//! that opened it, so the first test to finish would take the shared tunnel
//! down with it and every later test would see `0 bytes at EOF`.
//!
//! One process-lifetime runtime keeps the tunnel alive across all of them. This
//! mirrors the application, where tunnels run on the Tauri runtime and outlive
//! any single query.

use std::sync::Arc;
use std::time::Duration;

use db_sync_engine::db::{ConnectParams, Introspector, connect};
use db_sync_engine::sshconn::{SshAuth, SshConfig, SshEndpoint};
use db_sync_engine::ssh::{
    AcceptAllHostKeys, RusshTunnelProvider, SshCredentials, TunnelHandle, TunnelProvider,
};
use db_sync_engine::types::Engine;
use secrecy::SecretString;
use tokio::net::TcpStream;

const SSH_PORT: u16 = 12222;

/// Runtime shared by every test in this binary.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    })
}

/// Declare an async test that runs on the shared runtime.
macro_rules! db_test {
    (async fn $name:ident() $body:block) => {
        #[test]
        fn $name() {
            rt().block_on(async move $body);
        }
    };
}

fn key_path() -> String {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/fixtures/ssh/id_ed25519"
    )
    .to_string()
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

fn ssh_config() -> SshConfig {
    SshConfig {
        endpoint: SshEndpoint {
            host: "127.0.0.1".into(),
            port: SSH_PORT,
            user: "tunnel".into(),
            auth: SshAuth::KeyFile {
                path: key_path(),
                passphrase_in_keychain: false,
            },
        },
        jump_host: None,
    }
}

/// One shared tunnel per engine, matching how the app works: a tunnel per job,
/// many queries over it.
static MYSQL_TUNNEL: tokio::sync::OnceCell<TunnelHandle> = tokio::sync::OnceCell::const_new();
static PG_TUNNEL: tokio::sync::OnceCell<TunnelHandle> = tokio::sync::OnceCell::const_new();
static MONGO_TUNNEL: tokio::sync::OnceCell<TunnelHandle> = tokio::sync::OnceCell::const_new();

async fn tunnel_for(engine: Engine) -> &'static TunnelHandle {
    let (cell, service, port) = match engine {
        Engine::Mysql => (&MYSQL_TUNNEL, "mysql", 3306u16),
        Engine::Postgres => (&PG_TUNNEL, "postgres", 5432u16),
        // Worth tunnelling rather than connecting directly: it is the one
        // configuration where the MongoDB driver's own discovery would learn
        // the server's advertised hostname and try to dial it. See the
        // `direct_connection` note in db.rs.
        Engine::Mongo => (&MONGO_TUNNEL, "mongo", 27017u16),
    };

    cell.get_or_init(|| async {
        RusshTunnelProvider::new(Arc::new(AcceptAllHostKeys))
            .open(&ssh_config(), &SshCredentials::default(), service, port)
            .await
            .expect("tunnel should open")
    })
    .await
}

/// Connect an introspector through the shared tunnel for its engine.
///
/// Callers must `close()` the result. Letting pools fall out of scope leaves
/// live forwarded channels to be torn down abruptly at process exit, which
/// leaves the SSH server unhappy for the *next* test binary.
async fn introspector_for(engine: Engine, database: Option<&str>) -> Box<dyn Introspector> {
    let handle = tunnel_for(engine).await;
    let (user, password) = match engine {
        Engine::Mysql => ("root", "testroot"),
        Engine::Postgres => ("dbsync", "testpass"),
        Engine::Mongo => ("root", "testroot"),
    };

    let params = ConnectParams {
        engine,
        host: "127.0.0.1".into(),
        port: handle.local_port(),
        user: user.into(),
        password: Some(SecretString::from(password)),
        database: database.map(str::to_string),
    };

    connect(&params)
        .await
        .expect("should connect through tunnel")
}

// ── MySQL ───────────────────────────────────────────────────────────────

db_test! {
    async fn mysql_server_info_through_a_tunnel() {
        require_containers!();
        let db = introspector_for(Engine::Mysql, Some("fixture")).await;

        let info = db.server_info().await.expect("server info");
        assert_eq!(info.engine, Engine::Mysql);
        assert!(info.version.starts_with('8'), "expected MySQL 8, got {}", info.version);
        assert!(info.can_read_catalog, "root must be able to read the catalog");
        db.close().await;
    }
}

db_test! {
    async fn mysql_lists_the_fixture_database() {
        require_containers!();
        let db = introspector_for(Engine::Mysql, Some("fixture")).await;

        let databases = db.list_databases().await.expect("list databases");
        let names: Vec<&str> = databases.iter().map(|d| d.name.as_str()).collect();

        assert!(names.contains(&"fixture"), "got {names:?}");
        // System schemas are noise in a picker and must be filtered out.
        assert!(!names.contains(&"information_schema"));
        assert!(!names.contains(&"performance_schema"));
        db.close().await;
    }
}

db_test! {
    async fn mysql_lists_tables_with_metadata() {
        require_containers!();
        let db = introspector_for(Engine::Mysql, Some("fixture")).await;

        let tables = db.list_tables("fixture").await.expect("list tables");
        assert_eq!(tables.len(), 20, "fixture defines 20 base tables");

        let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"orders"));
        // Views must not appear in a table picker.
        assert!(!names.contains(&"active_users"), "views are not base tables");
        // Reserved-word and unicode identifiers must survive introspection.
        assert!(names.contains(&"order"));
        assert!(names.contains(&"日本語テーブル"));

        let legacy = tables.iter().find(|t| t.name == "legacy_stats").expect("MyISAM table");
        assert!(
            !legacy.is_transactional(),
            "MyISAM must be flagged so the UI can warn about --single-transaction"
        );

        let orders = tables.iter().find(|t| t.name == "orders").unwrap();
        assert!(orders.is_transactional());
        assert!(orders.total_bytes() > 0, "size metadata should be populated");
        db.close().await;
    }
}

db_test! {
    async fn mysql_exact_row_counts_are_correct() {
        require_containers!();
        let db = introspector_for(Engine::Mysql, Some("fixture")).await;

        assert_eq!(db.exact_row_count("fixture", "users").await.unwrap(), 3);
        assert_eq!(db.exact_row_count("fixture", "orders").await.unwrap(), 2);
        assert_eq!(db.exact_row_count("fixture", "roles").await.unwrap(), 3);
        db.close().await;
    }
}

db_test! {
    async fn mysql_counts_reserved_word_and_unicode_tables() {
        require_containers!();
        let db = introspector_for(Engine::Mysql, Some("fixture")).await;

        // These fail immediately without correct identifier quoting and a
        // utf8mb4 connection charset.
        assert_eq!(db.exact_row_count("fixture", "order").await.unwrap(), 1);
        assert_eq!(db.exact_row_count("fixture", "日本語テーブル").await.unwrap(), 1);
        assert_eq!(db.exact_row_count("fixture", "naïve_café").await.unwrap(), 2);
        db.close().await;
    }
}

db_test! {
    async fn mysql_exact_count_is_authoritative_over_the_estimate() {
        require_containers!();
        let db = introspector_for(Engine::Mysql, Some("fixture")).await;

        let tables = db.list_tables("fixture").await.unwrap();
        let orders = tables.iter().find(|t| t.name == "orders").unwrap();
        let exact = db.exact_row_count("fixture", "orders").await.unwrap();

        assert_eq!(exact, 2, "the exact count is authoritative");
        // The estimate is a separate, non-authoritative number. This is exactly
        // why verification never uses it.
        eprintln!("InnoDB estimate: {:?}, exact: {exact}", orders.estimated_rows);
        db.close().await;
    }
}

db_test! {
    async fn mysql_rejects_a_missing_table_rather_than_returning_zero() {
        require_containers!();
        let db = introspector_for(Engine::Mysql, Some("fixture")).await;

        assert!(
            db.exact_row_count("fixture", "no_such_table").await.is_err(),
            "a missing table must error, not silently count as 0"
        );
        db.close().await;
    }
}

// ── PostgreSQL ──────────────────────────────────────────────────────────

db_test! {
    async fn postgres_server_info_through_a_tunnel() {
        require_containers!();
        let db = introspector_for(Engine::Postgres, Some("fixture")).await;

        let info = db.server_info().await.expect("server info");
        assert_eq!(info.engine, Engine::Postgres);
        assert!(
            info.version.starts_with("18"),
            "expected PostgreSQL 18, got {}",
            info.version
        );
        assert!(info.can_read_catalog);
        db.close().await;
    }
}

db_test! {
    async fn postgres_lists_the_fixture_database() {
        require_containers!();
        let db = introspector_for(Engine::Postgres, Some("fixture")).await;

        let databases = db.list_databases().await.expect("list databases");
        let names: Vec<&str> = databases.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"fixture"), "got {names:?}");
        db.close().await;
    }
}

db_test! {
    async fn postgres_lists_tables_across_schemas() {
        require_containers!();
        let db = introspector_for(Engine::Postgres, Some("fixture")).await;

        let tables = db.list_tables("fixture").await.expect("list tables");

        let public: Vec<&str> = tables
            .iter()
            .filter(|t| t.schema.as_deref() == Some("public"))
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(public.len(), 20, "fixture defines 20 public tables");

        // The non-public schema must be discoverable, not silently dropped.
        assert!(
            tables.iter().any(|t| t.schema.as_deref() == Some("reporting")
                && t.name == "daily_totals"),
            "reporting.daily_totals should be listed"
        );

        // Catalog schemas are noise.
        assert!(!tables.iter().any(|t| t.schema.as_deref() == Some("pg_catalog")));

        assert!(public.contains(&"order"));
        assert!(public.contains(&"select"));
        assert!(public.contains(&"日本語テーブル"));
        assert!(!public.contains(&"active_users"), "views are not base tables");

        assert!(
            tables.iter().all(|t| t.is_transactional()),
            "postgres tables are always transactional"
        );
        db.close().await;
    }
}

db_test! {
    async fn postgres_exact_row_counts_are_correct() {
        require_containers!();
        let db = introspector_for(Engine::Postgres, Some("fixture")).await;

        assert_eq!(db.exact_row_count("fixture", "users").await.unwrap(), 3);
        assert_eq!(db.exact_row_count("fixture", "orders").await.unwrap(), 2);
        db.close().await;
    }
}

db_test! {
    async fn postgres_counts_qualified_reserved_and_unicode_tables() {
        require_containers!();
        let db = introspector_for(Engine::Postgres, Some("fixture")).await;

        assert_eq!(db.exact_row_count("fixture", "public.order").await.unwrap(), 1);
        assert_eq!(db.exact_row_count("fixture", "select").await.unwrap(), 1);
        assert_eq!(db.exact_row_count("fixture", "日本語テーブル").await.unwrap(), 1);
        // A table outside the default schema needs its qualifier honoured.
        assert_eq!(
            db.exact_row_count("fixture", "reporting.daily_totals").await.unwrap(),
            1
        );
        db.close().await;
    }
}

db_test! {
    async fn postgres_refuses_to_list_tables_for_another_database() {
        require_containers!();
        let db = introspector_for(Engine::Postgres, Some("fixture")).await;

        // A silent empty list here would read as "that database has no tables".
        let err = db
            .list_tables("some_other_db")
            .await
            .expect_err("cross-database introspection needs a separate connection");
        assert!(format!("{err}").contains("separate connection"));
        db.close().await;
    }
}

db_test! {
    async fn bad_credentials_fail_to_connect() {
        require_containers!();
        let handle = tunnel_for(Engine::Mysql).await;

        let params = ConnectParams {
            engine: Engine::Mysql,
            host: "127.0.0.1".into(),
            port: handle.local_port(),
            user: "root".into(),
            password: Some(SecretString::from("wrong-password")),
            database: Some("fixture".into()),
        };

        assert!(
            connect(&params).await.is_err(),
            "a wrong password must fail at connect time, not mid-job"
        );
    }
}

db_test! {
    async fn many_queries_share_one_tunnel() {
        require_containers!();
        let db = introspector_for(Engine::Mysql, Some("fixture")).await;

        // A dump opens many connections over the life of one tunnel. If the
        // forwarder mishandled connection teardown this would fail partway.
        for _ in 0..25 {
            assert_eq!(db.exact_row_count("fixture", "users").await.unwrap(), 3);
        }
        db.close().await;
    }
}

// ── MongoDB ─────────────────────────────────────────────────────────────
//
// These prove the driver works over a forwarded port: the handshake, the
// catalog, and a pool held across many queries.
//
// What they do **not** prove is that `direct_connection(true)` is doing
// anything, and it is worth being clear about that rather than letting the
// file imply otherwise. That setting stops the driver rediscovering a replica
// set's members by their own advertised hostnames — which through a tunnel
// resolve to nothing on this side. The fixture is a *standalone*, which
// advertises no members, so discovery has nothing to redirect to and these
// pass with the setting either way. Confirmed by flipping it off and watching
// them still pass.
//
// Covering it would need a replica-set fixture, which would stop exercising
// the standalone behaviour that `--oplog` being off by default exists for.
// The trade is recorded in `db.rs` next to the setting.

db_test! {
    async fn mongo_server_info_through_a_tunnel() {
        require_containers!();
        let db = introspector_for(Engine::Mongo, None).await;

        let info = db.server_info().await.expect("server info");
        assert_eq!(info.engine, Engine::Mongo);
        assert!(
            info.version.starts_with('7'),
            "expected MongoDB 7, got {}",
            info.version
        );
        assert!(info.can_read_catalog);
        db.close().await;
    }
}

db_test! {
    async fn mongo_reads_collections_through_a_tunnel() {
        require_containers!();
        let db = introspector_for(Engine::Mongo, None).await;

        let names: Vec<String> = db
            .list_tables("fixture")
            .await
            .expect("list collections")
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(names.contains(&"users".to_string()), "got {names:?}");

        // Reaching the data, not just the catalog: a tunnel that forwards the
        // handshake and then stalls would pass the check above.
        assert_eq!(db.exact_row_count("fixture", "users").await.unwrap(), 4);
        db.close().await;
    }
}

db_test! {
    async fn mongo_holds_one_tunnel_across_many_queries() {
        require_containers!();
        let db = introspector_for(Engine::Mongo, None).await;

        // The driver keeps a pool and reuses it. If discovery were running, a
        // later query could be sent to a rediscovered address rather than the
        // forwarded one, so this fails partway rather than at the first call.
        for _ in 0..25 {
            assert_eq!(db.exact_row_count("fixture", "orders").await.unwrap(), 3);
        }
        db.close().await;
    }
}
