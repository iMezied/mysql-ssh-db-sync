//! Masking against real MySQL and PostgreSQL servers.
//!
//!     docker compose -f docker-compose.test.yml up -d --wait
//!
//! Unit tests prove the generated SQL has the right *shape*. They cannot prove
//! a server accepts it, and the SQL here is not portable boilerplate — it is
//! `SHA2`/`CONV`/`LPAD` on one engine and `encode(sha256(...))`/`bit(28)` on the
//! other. A masking expression that a server rejects is a failed sync; one a
//! server *accepts* while quietly not masking is a data leak. Only a real
//! server can tell those apart.
//!
//! These connect straight to the mapped ports rather than through the SSH
//! tunnel, and never invoke `mysqldump`/`pg_dump`: masking is executed by the
//! destination server over a sqlx connection, so the tunnel and the dump tools
//! are not part of what is under test here.

use std::time::Duration;

use db_sync_engine::db::{ConnectParams, Statement, execute_batch, execute_raw, fetch_count_rows};
use db_sync_engine::mask::{
    MaskRule, MaskTransform, check_statements, derive_salt, update_statements,
};
use db_sync_engine::types::Engine;
use secrecy::SecretString;
use sqlx::Row;
use tokio::net::TcpStream;

const MYSQL_PORT: u16 = 13306;
const PG_PORT: u16 = 15432;

/// Prefix for the namespace each test owns outright.
///
/// One per test, not one shared: these run in parallel, and a shared scratch
/// database means the tests race to create and drop it underneath each other.
const SCRATCH_PREFIX: &str = "dbsync_mask";

fn ns(test: &str) -> String {
    format!("{SCRATCH_PREFIX}_{test}")
}

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
    ($port:expr) => {
        if !reachable($port).await {
            if std::env::var("DBSYNC_REQUIRE_CONTAINERS").is_ok() {
                panic!("test containers are required but not reachable");
            }
            eprintln!("skipping: test containers not running");
            return;
        }
    };
}

fn params(engine: Engine, database: Option<&str>) -> ConnectParams {
    let (port, user, password) = match engine {
        Engine::Mysql => (MYSQL_PORT, "root", "testroot"),
        Engine::Postgres => (PG_PORT, "dbsync", "testpass"),
    };
    ConnectParams {
        engine,
        host: "127.0.0.1".into(),
        port,
        user: user.into(),
        password: Some(SecretString::from(password)),
        database: database.map(str::to_string),
    }
}

/// The connection masking would run over, against a scratch namespace.
///
/// MySQL gets its own database; PostgreSQL gets its own schema inside the
/// fixture database, which also exercises the `schema.table` path that a bare
/// name never reaches.
async fn scratch(engine: Engine, ns: &str) -> ConnectParams {
    match engine {
        Engine::Mysql => {
            let admin = params(engine, None);
            execute_raw(&admin, &format!("DROP DATABASE IF EXISTS {ns}"))
                .await
                .expect("drop scratch database");
            execute_raw(&admin, &format!("CREATE DATABASE {ns}"))
                .await
                .expect("create scratch database");
            params(engine, Some(ns))
        }
        Engine::Postgres => {
            let p = params(engine, Some("fixture"));
            execute_raw(&p, &format!("DROP SCHEMA IF EXISTS {ns} CASCADE"))
                .await
                .expect("drop scratch schema");
            execute_raw(&p, &format!("CREATE SCHEMA {ns}"))
                .await
                .expect("create scratch schema");
            p
        }
    }
}

/// Table name as a masking rule would spell it.
fn table_name(engine: Engine, ns: &str, table: &str) -> String {
    match engine {
        Engine::Mysql => table.to_string(),
        Engine::Postgres => format!("{ns}.{table}"),
    }
}

/// Two tables sharing an email, so the join property can be checked.
async fn seed(engine: Engine, ns: &str, p: &ConnectParams) {
    let ddl = match engine {
        Engine::Mysql => vec![
            "CREATE TABLE users (id INT PRIMARY KEY, email VARCHAR(255), \
             phone VARCHAR(64), surname VARCHAR(255), ssn VARCHAR(32), note TEXT)"
                .to_string(),
            "CREATE TABLE orders (id INT PRIMARY KEY, buyer_email VARCHAR(255))".to_string(),
        ],
        Engine::Postgres => vec![
            format!(
                "CREATE TABLE {ns}.users (id INT PRIMARY KEY, email TEXT, \
                 phone TEXT, surname TEXT, ssn TEXT, note TEXT)"
            ),
            format!("CREATE TABLE {ns}.orders (id INT PRIMARY KEY, buyer_email TEXT)"),
        ],
    };
    for statement in ddl {
        execute_raw(p, &statement)
            .await
            .expect("create fixture table");
    }

    let users = match engine {
        Engine::Mysql => "INSERT INTO users VALUES \
             (1, 'alice@example.com', '+441234567890', 'Ashworth', '111-22-3333', 'vip'), \
             (2, 'bob@example.com', '+441234567891', 'Bell', '444-55-6666', 'none'), \
             (3, NULL, NULL, 'Carr', '777-88-9999', NULL)"
            .to_string(),
        Engine::Postgres => format!(
            "INSERT INTO {ns}.users VALUES \
             (1, 'alice@example.com', '+441234567890', 'Ashworth', '111-22-3333', 'vip'), \
             (2, 'bob@example.com', '+441234567891', 'Bell', '444-55-6666', 'none'), \
             (3, NULL, NULL, 'Carr', '777-88-9999', NULL)"
        ),
    };
    execute_raw(p, &users).await.expect("seed users");

    // Alice appears in both tables. If masking is not deterministic, this join
    // breaks and the masked copy is useless for anything that reads across
    // tables — which is most of what a dev database is for.
    let orders = match engine {
        Engine::Mysql => {
            "INSERT INTO orders VALUES (1, 'alice@example.com'), (2, 'carol@example.com')"
                .to_string()
        }
        Engine::Postgres => format!(
            "INSERT INTO {ns}.orders VALUES (1, 'alice@example.com'), \
             (2, 'carol@example.com')"
        ),
    };
    execute_raw(p, &orders).await.expect("seed orders");
}

async fn cleanup(engine: Engine, ns: &str) {
    match engine {
        Engine::Mysql => {
            let _ = execute_raw(
                &params(engine, None),
                &format!("DROP DATABASE IF EXISTS {ns}"),
            )
            .await;
        }
        Engine::Postgres => {
            let _ = execute_raw(
                &params(engine, Some("fixture")),
                &format!("DROP SCHEMA IF EXISTS {ns} CASCADE"),
            )
            .await;
        }
    }
}

/// Read one text column, ordered by id, NULLs included.
async fn read_column(engine: Engine, ns: &str, table: &str, column: &str) -> Vec<Option<String>> {
    let qualified = match engine {
        Engine::Mysql => table.to_string(),
        Engine::Postgres => format!("{ns}.{table}"),
    };
    let sql = format!("SELECT {column} FROM {qualified} ORDER BY id");

    match engine {
        Engine::Mysql => {
            let pool = sqlx::MySqlPool::connect(&format!(
                "mysql://root:testroot@127.0.0.1:{MYSQL_PORT}/{ns}"
            ))
            .await
            .expect("connect");
            let rows = sqlx::query(&sql).fetch_all(&pool).await.expect("read back");
            let out = rows.iter().map(|r| r.get::<Option<String>, _>(0)).collect();
            pool.close().await;
            out
        }
        Engine::Postgres => {
            let pool = sqlx::PgPool::connect(&format!(
                "postgres://dbsync:testpass@127.0.0.1:{PG_PORT}/fixture"
            ))
            .await
            .expect("connect");
            let rows = sqlx::query(&sql).fetch_all(&pool).await.expect("read back");
            let out = rows.iter().map(|r| r.get::<Option<String>, _>(0)).collect();
            pool.close().await;
            out
        }
    }
}

fn rules(engine: Engine, ns: &str) -> Vec<MaskRule> {
    vec![
        MaskRule::email(table_name(engine, ns, "users"), "email"),
        MaskRule {
            table: table_name(engine, ns, "users"),
            column: "phone".into(),
            transform: MaskTransform::Phone,
        },
        MaskRule::hash(table_name(engine, ns, "users"), "surname"),
        MaskRule {
            table: table_name(engine, ns, "users"),
            column: "ssn".into(),
            transform: MaskTransform::Null,
        },
        MaskRule {
            table: table_name(engine, ns, "users"),
            column: "note".into(),
            transform: MaskTransform::Constant {
                value: "redacted".into(),
            },
        },
        MaskRule::email(table_name(engine, ns, "orders"), "buyer_email"),
    ]
}

async fn run_checks(p: &ConnectParams, checks: &[Statement]) -> Vec<Vec<i64>> {
    fetch_count_rows(p, checks)
        .await
        .expect("checks should run")
}

/// The whole feature, on one engine.
async fn masks_a_real_database(engine: Engine, ns: &str) {
    let p = scratch(engine, ns).await;
    seed(engine, ns, &p).await;

    let rules = rules(engine, ns);
    let salt = derive_salt("integration-test-secret");

    // ── The check must be able to see unmasked data ─────────────────────
    //
    // Run before masking. If this reported zero here it would report zero
    // afterwards too, and the post-mask "verified" would be worthless.
    let checks = check_statements(engine, &rules).unwrap();
    let statements: Vec<Statement> = checks.iter().map(|c| c.statement.clone()).collect();
    let before = run_checks(&p, &statements).await;
    let total_before: i64 = before.iter().flatten().sum();
    assert!(
        total_before > 0,
        "{engine:?}: the check found nothing to mask in unmasked data, so it proves nothing"
    );

    // ── Mask ────────────────────────────────────────────────────────────
    let updates = update_statements(engine, &rules, &salt).unwrap();
    let update_statements_only: Vec<Statement> =
        updates.iter().map(|u| u.statement.clone()).collect();
    let affected = execute_batch(&p, &update_statements_only)
        .await
        .unwrap_or_else(|e| panic!("{engine:?}: masking statements were rejected: {e}"));
    assert_eq!(
        affected.iter().sum::<u64>(),
        5,
        "{engine:?}: 3 users + 2 orders"
    );

    // ── The check must pass afterwards ──────────────────────────────────
    let after = run_checks(&p, &statements).await;
    for (check, counts) in checks.iter().zip(&after) {
        for (column, count) in check.columns.iter().zip(counts) {
            assert_eq!(
                *count, 0,
                "{engine:?}: {}.{} still has {count} unmasked row(s)",
                check.table, column
            );
        }
    }

    // ── And the data must actually be different ─────────────────────────
    let emails = read_column(engine, ns, "users", "email").await;
    assert!(
        !emails.iter().flatten().any(|e| e.contains("alice")),
        "{engine:?}: the original address survived masking: {emails:?}"
    );
    assert!(
        emails
            .iter()
            .flatten()
            .all(|e| e.ends_with("@example.invalid")),
        "{engine:?}: {emails:?}"
    );

    // NULL is not sensitive and must survive as NULL — turning it into a
    // hash would invent data that was never there.
    assert_eq!(
        emails[2], None,
        "{engine:?}: a NULL email became {:?}",
        emails[2]
    );
    let phones = read_column(engine, ns, "users", "phone").await;
    assert_eq!(
        phones[2], None,
        "{engine:?}: a NULL phone became {:?}",
        phones[2]
    );

    // ── The join property ───────────────────────────────────────────────
    //
    // Alice's address appears in both tables. Masking is only useful if it
    // maps them to the same pseudonym.
    let buyers = read_column(engine, ns, "orders", "buyer_email").await;
    assert_eq!(
        emails[0], buyers[0],
        "{engine:?}: the same address masked differently in two tables, so joins are broken"
    );
    assert_ne!(
        buyers[0], buyers[1],
        "{engine:?}: two different addresses collapsed to one pseudonym"
    );

    // ── The remaining transforms ────────────────────────────────────────
    let phones = read_column(engine, ns, "users", "phone").await;
    assert!(
        phones[0].as_deref().unwrap().starts_with("+1555"),
        "{engine:?}: {phones:?}"
    );
    let surnames = read_column(engine, ns, "users", "surname").await;
    assert_eq!(
        surnames[0].as_deref().unwrap().len(),
        64,
        "{engine:?}: full sha256 hex"
    );
    assert!(
        !surnames.iter().flatten().any(|s| s == "Ashworth"),
        "{engine:?}"
    );

    let ssns = read_column(engine, ns, "users", "ssn").await;
    assert!(ssns.iter().all(Option::is_none), "{engine:?}: {ssns:?}");

    let notes = read_column(engine, ns, "users", "note").await;
    assert!(
        notes.iter().all(|n| n.as_deref() == Some("redacted")),
        "{engine:?}: a constant overwrites NULLs too: {notes:?}"
    );

    cleanup(engine, ns).await;
}

db_test! {
    async fn mysql_masks_a_real_database() {
        require_containers!(MYSQL_PORT);
        masks_a_real_database(Engine::Mysql, &ns("my_full")).await;
    }
}

db_test! {
    async fn postgres_masks_a_real_database() {
        require_containers!(PG_PORT);
        masks_a_real_database(Engine::Postgres, &ns("pg_full")).await;
    }
}

/// Masking the same data twice must land in the same place.
///
/// A dev database refreshed weekly keeps stable pseudonyms only if this holds;
/// without it, every refresh reshuffles every identity and nothing downstream
/// can keep a reference.
async fn masking_is_stable_across_runs(engine: Engine, ns: &str) {
    let p = scratch(engine, ns).await;
    seed(engine, ns, &p).await;

    let rules = vec![MaskRule::email(table_name(engine, ns, "users"), "email")];
    let salt = derive_salt("integration-test-secret");

    let updates = update_statements(engine, &rules, &salt).unwrap();
    let stmts: Vec<Statement> = updates.iter().map(|u| u.statement.clone()).collect();

    execute_batch(&p, &stmts).await.expect("first pass");
    let first = read_column(engine, ns, "users", "email").await;

    // Re-seed and mask again from scratch, exactly as next week's sync would.
    cleanup(engine, ns).await;
    let p = scratch(engine, ns).await;
    seed(engine, ns, &p).await;
    execute_batch(&p, &stmts).await.expect("second pass");
    let second = read_column(engine, ns, "users", "email").await;

    assert_eq!(
        first, second,
        "{engine:?}: the same input masked to a different value on a second run"
    );

    cleanup(engine, ns).await;
}

db_test! {
    async fn mysql_masking_is_stable_across_runs() {
        require_containers!(MYSQL_PORT);
        masking_is_stable_across_runs(Engine::Mysql, &ns("my_stable")).await;
    }
}

db_test! {
    async fn postgres_masking_is_stable_across_runs() {
        require_containers!(PG_PORT);
        masking_is_stable_across_runs(Engine::Postgres, &ns("pg_stable")).await;
    }
}

/// A different salt must produce different pseudonyms.
///
/// The salt is the only thing standing between deterministic masking and a
/// dictionary attack, so it has to actually reach the hash.
async fn the_salt_changes_the_output(engine: Engine, ns: &str) {
    let p = scratch(engine, ns).await;
    seed(engine, ns, &p).await;

    let rules = vec![MaskRule::email(table_name(engine, ns, "users"), "email")];

    let a: Vec<Statement> = update_statements(engine, &rules, &derive_salt("secret-a"))
        .unwrap()
        .iter()
        .map(|u| u.statement.clone())
        .collect();
    execute_batch(&p, &a).await.expect("mask with salt a");
    let with_a = read_column(engine, ns, "users", "email").await;

    cleanup(engine, ns).await;
    let p = scratch(engine, ns).await;
    seed(engine, ns, &p).await;

    let b: Vec<Statement> = update_statements(engine, &rules, &derive_salt("secret-b"))
        .unwrap()
        .iter()
        .map(|u| u.statement.clone())
        .collect();
    execute_batch(&p, &b).await.expect("mask with salt b");
    let with_b = read_column(engine, ns, "users", "email").await;

    assert_ne!(
        with_a[0], with_b[0],
        "{engine:?}: the salt did not reach the hash"
    );

    cleanup(engine, ns).await;
}

db_test! {
    async fn mysql_salt_changes_the_output() {
        require_containers!(MYSQL_PORT);
        the_salt_changes_the_output(Engine::Mysql, &ns("my_salt")).await;
    }
}

db_test! {
    async fn postgres_salt_changes_the_output() {
        require_containers!(PG_PORT);
        the_salt_changes_the_output(Engine::Postgres, &ns("pg_salt")).await;
    }
}

/// A transform the column cannot accept must fail loudly.
///
/// This is the path that ends in the destination being dropped. If the error
/// were swallowed here, the sync would report success over a database that
/// still holds the real values.
async fn an_impossible_transform_is_an_error(engine: Engine, ns: &str) {
    let p = scratch(engine, ns).await;

    let ddl = match engine {
        Engine::Mysql => {
            "CREATE TABLE required (id INT PRIMARY KEY, ssn VARCHAR(32) NOT NULL)".to_string()
        }
        Engine::Postgres => {
            format!("CREATE TABLE {ns}.required (id INT PRIMARY KEY, ssn TEXT NOT NULL)")
        }
    };
    execute_raw(&p, &ddl).await.expect("create table");

    let insert = match engine {
        Engine::Mysql => "INSERT INTO required VALUES (1, '111-22-3333')".to_string(),
        Engine::Postgres => {
            format!("INSERT INTO {ns}.required VALUES (1, '111-22-3333')")
        }
    };
    execute_raw(&p, &insert).await.expect("seed");

    let rules = vec![MaskRule {
        table: table_name(engine, ns, "required"),
        column: "ssn".into(),
        transform: MaskTransform::Null,
    }];
    let updates = update_statements(engine, &rules, "salt").unwrap();
    let stmts: Vec<Statement> = updates.iter().map(|u| u.statement.clone()).collect();

    let result = execute_batch(&p, &stmts).await;
    assert!(
        result.is_err(),
        "{engine:?}: NULLing a NOT NULL column reported success"
    );

    // And the data is untouched, which is exactly why the caller must drop the
    // database rather than carry on.
    let rows = read_column(engine, ns, "required", "ssn").await;
    assert_eq!(rows[0].as_deref(), Some("111-22-3333"), "{engine:?}");

    cleanup(engine, ns).await;
}

db_test! {
    async fn mysql_impossible_transform_is_an_error() {
        require_containers!(MYSQL_PORT);
        an_impossible_transform_is_an_error(Engine::Mysql, &ns("my_impossible")).await;
    }
}

db_test! {
    async fn postgres_impossible_transform_is_an_error() {
        require_containers!(PG_PORT);
        an_impossible_transform_is_an_error(Engine::Postgres, &ns("pg_impossible")).await;
    }
}
