//! Database introspection.
//!
//! Used to populate the table picker and to verify restores. Connections here
//! are for *queries only* — bulk dump and restore go through the vendor tools.

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use specta::Type;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{MySqlPool, PgPool, Row};

use crate::types::Engine;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DatabaseInfo {
    pub name: String,
    pub charset: Option<String>,
    pub collation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TableInfo {
    pub schema: Option<String>,
    pub name: String,
    /// InnoDB/MyISAM for MySQL; `None` for PostgreSQL.
    pub storage_engine: Option<String>,
    /// Planner estimate. Cheap but approximate — never use it to verify a
    /// restore. See [`crate::verify`].
    #[specta(type = Option<f64>)]
    pub estimated_rows: Option<u64>,
    #[specta(type = Option<f64>)]
    pub data_bytes: Option<u64>,
    #[specta(type = Option<f64>)]
    pub index_bytes: Option<u64>,
    /// Whether a consistent snapshot is possible for this table.
    ///
    /// Serialised rather than left as a method so the UI cannot re-derive the
    /// rule and drift from it — this decides whether we warn that
    /// `--single-transaction` does not cover a selected table.
    pub transactional: bool,
}

/// A MongoDB collection is a table, a document is a row, a field is a column.
///
/// The mapping is stated once, here, because it is the assumption that lets
/// [`Introspector`] stay a single trait rather than splitting into a relational
/// contract and a document one. Where the analogy genuinely breaks — a schema
/// that is observed rather than declared, masking that is a pipeline rather
/// than an `UPDATE` — the code says so at the point it breaks.
pub const MONGO_TERMINOLOGY: &str = "collection = table, document = row, field = column";

impl TableInfo {
    /// Build a `TableInfo`, deriving `transactional` from the storage engine.
    pub fn new(
        schema: Option<String>,
        name: String,
        storage_engine: Option<String>,
        estimated_rows: Option<u64>,
        data_bytes: Option<u64>,
        index_bytes: Option<u64>,
    ) -> Self {
        let transactional = match storage_engine.as_deref() {
            Some(e) => e.eq_ignore_ascii_case("innodb"),
            // PostgreSQL always is; so is MongoDB on WiredTiger, which has been
            // the only storage engine since 4.2. Neither reports one here.
            None => true,
        };
        Self {
            schema,
            name,
            storage_engine,
            estimated_rows,
            data_bytes,
            index_bytes,
            transactional,
        }
    }
    /// Fully-qualified name for use in generated SQL.
    pub fn qualified_name(&self) -> String {
        match &self.schema {
            Some(s) => format!("{s}.{}", self.name),
            None => self.name.clone(),
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.data_bytes.unwrap_or(0) + self.index_bytes.unwrap_or(0)
    }

    /// MyISAM tables are not covered by `--single-transaction`; the UI warns
    /// when one is selected for a production dump.
    pub fn is_transactional(&self) -> bool {
        self.transactional
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ServerInfo {
    pub engine: Engine,
    pub version: String,
    /// Whether the connected user can read the catalog well enough to
    /// introspect. Surfaced by the test-connection flow.
    pub can_read_catalog: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("query failed: {0}")]
    Query(String),
    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),
    /// A SQL-shaped entry point was handed a document store.
    ///
    /// Returned rather than ignored, and never reachable in normal operation:
    /// every caller of the statement helpers dispatches on the engine first.
    /// This exists so that a future caller that forgets fails loudly here
    /// instead of quietly running no statements and reporting success — which,
    /// for masking, would mean reporting a database as masked without touching
    /// it.
    #[error("{0} does not speak SQL; this path takes statements")]
    NotSql(Engine),
}

/// Where to connect. `host`/`port` are usually a tunnel's local endpoint.
#[derive(Debug, Clone)]
pub struct ConnectParams {
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<SecretString>,
    pub database: Option<String>,
}

/// Read-only catalog access, implemented per engine.
#[async_trait]
pub trait Introspector: Send + Sync {
    async fn server_info(&self) -> Result<ServerInfo, DbError>;
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError>;
    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>, DbError>;
    /// Exact `COUNT(*)`. Slow on large tables by design — estimates are not
    /// acceptable for verification.
    async fn exact_row_count(&self, database: &str, table: &str) -> Result<u64, DbError>;
    /// An order-independent digest of a table's contents.
    ///
    /// `COUNT(*)` proves a table has the right number of rows; it says nothing
    /// about whether they hold the right bytes. Truncated text, a mangled
    /// character set, a column restored as NULL — all pass a row count and all
    /// are exactly what a restore gets wrong.
    ///
    /// The digest only ever has to be comparable between a source and a
    /// destination of the *same* engine, because cross-engine sync is refused
    /// upstream. That frees each implementation to use whatever its own server
    /// computes fastest, rather than a lowest-common-denominator scheme.
    ///
    /// `None` means the table could not be digested (no columns, an exotic
    /// type). That is reported as "not compared", never as a match.
    async fn table_digest(&self, database: &str, table: &str) -> Result<Option<String>, DbError>;
    /// Column names, in ordinal order, for schema comparison.
    ///
    /// # What this means where there is no declared schema
    ///
    /// MongoDB has no column list to read, so its implementation returns the
    /// **union of field names actually present**, in name order. Two
    /// consequences follow, and both are deliberate:
    ///
    /// * A field that no document carries is indistinguishable from a field
    ///   that does not exist. For the callers that matter — schema comparison
    ///   and masking coverage — those are the same thing: there is nothing to
    ///   compare and nothing to mask.
    /// * It is a full scan, not a catalog lookup. That is the price of the
    ///   answer being exact rather than sampled; a sample would make the same
    ///   collection report different fields on the source and the destination
    ///   and turn a correct restore into a schema mismatch.
    ///
    /// Only top-level fields are listed. A masking rule may still address a
    /// nested field by dotted path — see [`crate::mask`], which checks the root
    /// of the path against this list.
    async fn column_names(&self, database: &str, table: &str) -> Result<Vec<String>, DbError>;
    async fn close(&self);
}

/// The database a connection should open when the target database must not be
/// held open — dropping it, or listing databases while creating it.
///
/// * PostgreSQL cannot connect without naming a database, so it borrows the
///   conventional `postgres` bootstrap database.
/// * MySQL and MongoDB both connect to the server rather than to a database.
pub const fn bootstrap_database(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::Postgres => Some("postgres"),
        Engine::Mysql | Engine::Mongo => None,
    }
}

/// Quote a MySQL identifier, escaping embedded backticks.
///
/// Table names come from the catalog, but they are still attacker-influenced
/// data in a shared database: a table literally named `` a`; DROP DATABASE x;-- ``
/// is legal. Interpolating one unquoted into `COUNT(*)` would execute it.
pub fn quote_mysql_ident(ident: &str) -> Result<String, DbError> {
    if ident.contains('\0') {
        return Err(DbError::InvalidIdentifier(
            "identifier contains a null byte".into(),
        ));
    }
    Ok(format!("`{}`", ident.replace('`', "``")))
}

/// Quote a PostgreSQL identifier, escaping embedded double quotes.
pub fn quote_pg_ident(ident: &str) -> Result<String, DbError> {
    if ident.contains('\0') {
        return Err(DbError::InvalidIdentifier(
            "identifier contains a null byte".into(),
        ));
    }
    Ok(format!("\"{}\"", ident.replace('"', "\"\"")))
}

/// Split `schema.table` into parts, defaulting to `public` for PostgreSQL.
fn split_qualified(name: &str, default_schema: &str) -> (String, String) {
    match name.split_once('.') {
        Some((schema, table)) => (schema.to_string(), table.to_string()),
        None => (default_schema.to_string(), name.to_string()),
    }
}

// ── MySQL ───────────────────────────────────────────────────────────────

pub struct MysqlIntrospector {
    pool: MySqlPool,
}

impl MysqlIntrospector {
    pub async fn connect(params: &ConnectParams) -> Result<Self, DbError> {
        // Built programmatically rather than as a URL so a password containing
        // `@`, `/` or `#` cannot corrupt the connection string.
        // utf8mb4 is not the client default on every server build. Without it,
        // a lookup of a table whose name contains non-ASCII characters fails
        // with "table doesn't exist".
        let mut options = MySqlConnectOptions::new()
            .host(&params.host)
            .port(params.port)
            .username(&params.user)
            .charset("utf8mb4");

        if let Some(pw) = &params.password {
            options = options.password(pw.expose_secret());
        }
        if let Some(db) = &params.database {
            options = options.database(db);
        }

        let pool = MySqlPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(std::time::Duration::from_secs(15))
            .connect_with(options)
            .await
            .map_err(|e| DbError::Connect(e.to_string()))?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl Introspector for MysqlIntrospector {
    async fn server_info(&self) -> Result<ServerInfo, DbError> {
        let version: String = sqlx::query_scalar("SELECT VERSION()")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;

        // Probe the catalog separately: a user can often connect but not read
        // information_schema, and finding that out now beats failing mid-dump.
        let can_read_catalog =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM information_schema.TABLES LIMIT 1")
                .fetch_one(&self.pool)
                .await
                .is_ok();

        Ok(ServerInfo {
            engine: Engine::Mysql,
            version,
            can_read_catalog,
        })
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        // MySQL 8's data-dictionary columns report as VARBINARY. CAST(.. AS CHAR)
        // transcodes them (producing mojibake for non-ASCII identifiers);
        // CONVERT(.. USING utf8mb4) reinterprets the bytes, which is what we want.
        let rows = sqlx::query(
            "SELECT CONVERT(SCHEMA_NAME USING utf8mb4) AS name, \
                    CONVERT(DEFAULT_CHARACTER_SET_NAME USING utf8mb4) AS charset, \
                    CONVERT(DEFAULT_COLLATION_NAME USING utf8mb4) AS collation \
             FROM information_schema.SCHEMATA \
             WHERE SCHEMA_NAME NOT IN ('information_schema','mysql','performance_schema','sys') \
             ORDER BY SCHEMA_NAME",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|r| DatabaseInfo {
                name: r.get("name"),
                charset: r.try_get("charset").ok(),
                collation: r.try_get("collation").ok(),
            })
            .collect())
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>, DbError> {
        let rows = sqlx::query(
            "SELECT CONVERT(TABLE_NAME USING utf8mb4) AS name, \
                    CONVERT(ENGINE USING utf8mb4) AS storage_engine, \
                    TABLE_ROWS, DATA_LENGTH, INDEX_LENGTH \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' \
             ORDER BY TABLE_NAME",
        )
        .bind(database)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|r| {
                TableInfo::new(
                    None,
                    r.get("name"),
                    r.try_get::<Option<String>, _>("storage_engine")
                        .ok()
                        .flatten(),
                    r.try_get::<Option<u64>, _>("TABLE_ROWS").ok().flatten(),
                    r.try_get::<Option<u64>, _>("DATA_LENGTH").ok().flatten(),
                    r.try_get::<Option<u64>, _>("INDEX_LENGTH").ok().flatten(),
                )
            })
            .collect())
    }

    async fn exact_row_count(&self, database: &str, table: &str) -> Result<u64, DbError> {
        // Identifiers cannot be bound as parameters, so they are quoted.
        let sql = format!(
            "SELECT COUNT(*) FROM {}.{}",
            quote_mysql_ident(database)?,
            quote_mysql_ident(table)?
        );
        let count: i64 = sqlx::query_scalar(&sql)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Query(format!("counting {database}.{table}: {e}")))?;
        Ok(count.max(0) as u64)
    }

    async fn table_digest(&self, database: &str, table: &str) -> Result<Option<String>, DbError> {
        let columns = self.column_names(database, table).await?;
        if columns.is_empty() {
            return Ok(None);
        }

        // Per-row MD5 folded together with BIT_XOR. XOR is commutative, so the
        // result does not depend on the order rows come back in — which matters
        // because a restored table has no reason to share the source's physical
        // ordering.
        //
        // CONCAT_WS skips NULLs, which would make ('a', NULL) and (NULL, 'a')
        // collide, so each value is wrapped in COALESCE with a sentinel that
        // cannot appear in real data.
        let projection = columns
            .iter()
            .map(|c| {
                quote_mysql_ident(c).map(|q| format!("COALESCE(CAST({q} AS CHAR), '\\0NULL')"))
            })
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");

        let sql = format!(
            "SELECT COALESCE(CAST(BIT_XOR(CAST(CONV(SUBSTRING(MD5(CONCAT_WS('\\0', {projection})), 1, 16), 16, 10) AS UNSIGNED)) AS CHAR), '0')              FROM {}.{}",
            quote_mysql_ident(database)?,
            quote_mysql_ident(table)?
        );

        let digest: String = sqlx::query_scalar(&sql)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Query(format!("digesting {database}.{table}: {e}")))?;
        Ok(Some(digest))
    }

    async fn column_names(&self, database: &str, table: &str) -> Result<Vec<String>, DbError> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT CONVERT(COLUMN_NAME USING utf8mb4) FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
        )
        .bind(database)
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Query(format!("reading columns of {database}.{table}: {e}")))?;
        Ok(rows)
    }

    async fn close(&self) {
        self.pool.close().await;
    }
}

// ── PostgreSQL ──────────────────────────────────────────────────────────

pub struct PostgresIntrospector {
    pool: PgPool,
}

impl PostgresIntrospector {
    pub async fn connect(params: &ConnectParams) -> Result<Self, DbError> {
        let mut options = PgConnectOptions::new()
            .host(&params.host)
            .port(params.port)
            .username(&params.user);

        if let Some(pw) = &params.password {
            options = options.password(pw.expose_secret());
        }
        // PostgreSQL requires a database to connect to at all; `postgres` is
        // the conventional bootstrap database for enumerating the others.
        options = options.database(params.database.as_deref().unwrap_or("postgres"));

        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(std::time::Duration::from_secs(15))
            .connect_with(options)
            .await
            .map_err(|e| DbError::Connect(e.to_string()))?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl Introspector for PostgresIntrospector {
    async fn server_info(&self) -> Result<ServerInfo, DbError> {
        let version: String = sqlx::query_scalar("SHOW server_version")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;

        let can_read_catalog =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pg_catalog.pg_class")
                .fetch_one(&self.pool)
                .await
                .is_ok();

        Ok(ServerInfo {
            engine: Engine::Postgres,
            version,
            can_read_catalog,
        })
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        let rows = sqlx::query(
            "SELECT d.datname, pg_encoding_to_char(d.encoding) AS charset, d.datcollate \
             FROM pg_database d \
             WHERE d.datistemplate = false AND d.datallowconn = true \
             ORDER BY d.datname",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|r| DatabaseInfo {
                name: r.get("datname"),
                charset: r.try_get("charset").ok(),
                collation: r.try_get("datcollate").ok(),
            })
            .collect())
    }

    /// Lists tables in the *connected* database.
    ///
    /// PostgreSQL cannot query across databases on one connection, so
    /// `database` must match the connection's database; it is accepted for
    /// symmetry with the MySQL implementation and validated here.
    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>, DbError> {
        let current: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;

        if current != database {
            return Err(DbError::Query(format!(
                "connected to {current:?} but asked for tables in {database:?}; \
                 PostgreSQL needs a separate connection per database"
            )));
        }

        let rows = sqlx::query(
            "SELECT n.nspname AS schema_name, \
                    c.relname AS table_name, \
                    c.reltuples::bigint AS estimated_rows, \
                    pg_table_size(c.oid) AS data_bytes, \
                    pg_indexes_size(c.oid) AS index_bytes \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind = 'r' \
               AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
               AND n.nspname NOT LIKE 'pg_toast%' \
             ORDER BY n.nspname, c.relname",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|r| {
                // reltuples is -1 on a never-analysed table, which is "unknown"
                // rather than a real count.
                let estimated = r
                    .try_get::<i64, _>("estimated_rows")
                    .ok()
                    .filter(|v| *v >= 0);
                TableInfo::new(
                    Some(r.get("schema_name")),
                    r.get("table_name"),
                    None,
                    estimated.map(|v| v as u64),
                    r.try_get::<i64, _>("data_bytes")
                        .ok()
                        .map(|v| v.max(0) as u64),
                    r.try_get::<i64, _>("index_bytes")
                        .ok()
                        .map(|v| v.max(0) as u64),
                )
            })
            .collect())
    }

    async fn exact_row_count(&self, _database: &str, table: &str) -> Result<u64, DbError> {
        let (schema, name) = split_qualified(table, "public");
        let sql = format!(
            "SELECT COUNT(*) FROM {}.{}",
            quote_pg_ident(&schema)?,
            quote_pg_ident(&name)?
        );
        let count: i64 = sqlx::query_scalar(&sql)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Query(format!("counting {table}: {e}")))?;
        Ok(count.max(0) as u64)
    }

    async fn table_digest(&self, _database: &str, table: &str) -> Result<Option<String>, DbError> {
        let (schema, name) = split_qualified(table, "public");

        // `t::text` is PostgreSQL's own canonical rendering of a whole row, so
        // this needs no column list and handles every type the server can
        // print. Summing per-row hashes keeps it order-independent, the same
        // property the MySQL side relies on.
        //
        // The cast chain takes 16 hex digits of the MD5 into a signed 64-bit
        // integer; the sum is taken as numeric so it cannot overflow.
        let sql = format!(
            "SELECT COALESCE(SUM(('x' || substr(md5(t::text), 1, 16))::bit(64)::bigint::numeric), 0)::text \
             FROM {}.{} AS t",
            quote_pg_ident(&schema)?,
            quote_pg_ident(&name)?
        );

        let digest: String = sqlx::query_scalar(&sql)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Query(format!("digesting {table}: {e}")))?;
        Ok(Some(digest))
    }

    async fn column_names(&self, _database: &str, table: &str) -> Result<Vec<String>, DbError> {
        let (schema, name) = split_qualified(table, "public");
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
        )
        .bind(&schema)
        .bind(&name)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Query(format!("reading columns of {table}: {e}")))?;
        Ok(rows)
    }

    async fn close(&self) {
        self.pool.close().await;
    }
}

// ── MongoDB ─────────────────────────────────────────────────────────────

/// Databases MongoDB creates for itself. Listing them would offer the user a
/// backup of the server's own bookkeeping.
pub const MONGO_SYSTEM_DATABASES: [&str; 3] = ["admin", "local", "config"];

/// The database credentials are checked against.
///
/// Fixed rather than derived from the profile's database, and it has to be:
/// `mongodump` resolves this differently from the driver when it is left
/// unstated, so a backup would authenticate against one database while the
/// introspection that verifies it authenticated against another. The two paths
/// agreeing matters more than either default. `--authenticationDatabase=admin`
/// is passed explicitly to the tools for the same reason.
pub const MONGO_AUTH_SOURCE: &str = "admin";

/// How many documents to hold in memory while digesting. Only ever one at a
/// time is needed; the batch size is what the driver fetches per round trip.
const MONGO_DIGEST_BATCH: u32 = 1000;

pub struct MongoIntrospector {
    client: mongodb::Client,
}

impl MongoIntrospector {
    pub async fn connect(params: &ConnectParams) -> Result<Self, DbError> {
        use mongodb::options::{ClientOptions, Credential, ServerAddress};

        // Built once rather than folded in conditionally: the options builder
        // encodes which fields are set in its *type*, so reassigning it inside
        // an `if` does not compile.
        let credential = match (&params.password, params.user.is_empty()) {
            (Some(pw), _) => Some(
                Credential::builder()
                    .username(params.user.clone())
                    .password(pw.expose_secret().to_string())
                    .source(MONGO_AUTH_SOURCE.to_string())
                    .build(),
            ),
            // A user with no password: a keyfile or certificate setup, or a
            // server with authentication off that still wants a name.
            (None, false) => Some(
                Credential::builder()
                    .username(params.user.clone())
                    .source(MONGO_AUTH_SOURCE.to_string())
                    .build(),
            ),
            (None, true) => None,
        };

        let options = ClientOptions::builder()
            .hosts(vec![ServerAddress::Tcp {
                host: params.host.clone(),
                port: Some(params.port),
            }])
            // Only matters against a replica set, and against one it is the
            // difference between working and failing bafflingly. Left off, the
            // driver runs discovery, learns the members' *own* advertised
            // hostnames, and dials those — which from this side of an SSH
            // tunnel resolve to nothing, or worse, to something else entirely.
            // The endpoint we were handed is the endpoint we talk to.
            //
            // Not covered by a test, and it cannot be with the current
            // fixture: `tests/introspect.rs` tunnels to a *standalone*, which
            // advertises no members, so discovery finds nothing to redirect to
            // and the tunnelled tests pass whether this is set or not. Proving
            // it would need a replica-set fixture — which would in turn stop
            // exercising the standalone-only behaviour `--oplog` depends on.
            // Recorded here rather than left as a test that looks like cover
            // and is not.
            .direct_connection(true)
            .app_name(Some("DBSync Studio".to_string()))
            .connect_timeout(Some(std::time::Duration::from_secs(15)))
            .server_selection_timeout(Some(std::time::Duration::from_secs(15)))
            .max_pool_size(Some(4))
            .credential(credential)
            .build();

        let client =
            mongodb::Client::with_options(options).map_err(|e| DbError::Connect(e.to_string()))?;

        // `with_options` does not dial: it builds a client and connects lazily.
        // A profile that cannot be reached would otherwise "connect" here and
        // fail much later, inside whatever operation the user actually asked
        // for, which is the wrong place to learn the host is wrong.
        client
            .database(MONGO_AUTH_SOURCE)
            .run_command(mongodb::bson::doc! { "ping": 1 })
            .await
            .map_err(|e| DbError::Connect(e.to_string()))?;

        Ok(Self { client })
    }

    fn collection(&self, database: &str, name: &str) -> mongodb::Collection<mongodb::bson::Document> {
        self.client.database(database).collection(name)
    }

    /// The driver client, for the operations that are not catalog reads.
    ///
    /// [`Introspector`] is documented as read-only, and masking writes — so it
    /// does not go through the trait, the same way [`execute_batch`] does not
    /// for the relational engines. Nothing acquires the ability to modify data
    /// by holding an introspector.
    pub fn client(&self) -> &mongodb::Client {
        &self.client
    }
}

/// Recursively sort a document's keys so that two documents holding the same
/// data hash the same however their fields happen to be ordered.
///
/// BSON preserves field order and treats it as significant, so hashing the raw
/// encoding would report a mismatch for a restore that reordered fields but
/// lost nothing. That is the wrong trade: a digest nobody trusts gets turned
/// off, and then the restores that *did* lose data stop being checked at all.
/// Reordering is not data loss, so it is normalised away.
///
/// Arrays keep their order, because in a document store an array's order is
/// data.
fn canonicalise(doc: &mongodb::bson::Document) -> mongodb::bson::Document {
    use mongodb::bson::{Bson, Document};

    fn canon_value(value: &Bson) -> Bson {
        match value {
            Bson::Document(d) => Bson::Document(canonicalise(d)),
            Bson::Array(items) => Bson::Array(items.iter().map(canon_value).collect()),
            other => other.clone(),
        }
    }

    let mut pairs: Vec<(&str, &Bson)> = doc.iter().map(|(k, v)| (k.as_str(), v)).collect();
    // Duplicate keys are legal in BSON. Sorting by key alone would leave their
    // relative order down to the input, so the value's encoding breaks the tie.
    pairs.sort_by(|a, b| {
        a.0.cmp(b.0)
            .then_with(|| format!("{:?}", a.1).cmp(&format!("{:?}", b.1)))
    });

    let mut out = Document::new();
    for (k, v) in pairs {
        out.insert(k, canon_value(v));
    }
    out
}

#[async_trait]
impl Introspector for MongoIntrospector {
    async fn server_info(&self) -> Result<ServerInfo, DbError> {
        use mongodb::bson::doc;

        let build_info = self
            .client
            .database(MONGO_AUTH_SOURCE)
            .run_command(doc! { "buildInfo": 1 })
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;

        let version = build_info
            .get_str("version")
            .map_err(|e| DbError::Query(format!("buildInfo carried no version: {e}")))?
            .to_string();

        // `listDatabases` is a privilege of its own: a user granted readWrite on
        // one database connects happily and cannot enumerate. Finding that out
        // now beats failing once a dump is already running.
        let can_read_catalog = self.client.list_database_names().await.is_ok();

        Ok(ServerInfo {
            engine: Engine::Mongo,
            version,
            can_read_catalog,
        })
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        let names = self
            .client
            .list_database_names()
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;

        let mut out: Vec<DatabaseInfo> = names
            .into_iter()
            .filter(|n| !MONGO_SYSTEM_DATABASES.contains(&n.as_str()))
            .map(|name| DatabaseInfo {
                name,
                // A MongoDB database has neither. Documents carry their own
                // encoding: BSON strings are UTF-8 by definition, and there is
                // no server-side collation to report at this level.
                charset: None,
                collation: None,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>, DbError> {
        let db = self.client.database(database);
        let mut names = db
            .list_collection_names()
            .await
            .map_err(|e| DbError::Query(format!("listing collections in {database}: {e}")))?;
        names.sort();

        let mut out = Vec::with_capacity(names.len());
        for name in names {
            // Sizes come from `$collStats`, the aggregation stage, rather than
            // the `collStats` command, which MongoDB deprecated. Failing to
            // read them is not fatal: the picker can list a collection it
            // cannot size, and the numbers here are only ever a hint.
            let stats = collection_storage_stats(&db, &name).await;

            out.push(TableInfo::new(
                None,
                name,
                // No storage engine is reported per collection; WiredTiger has
                // been the only one since 4.2, and it is transactional.
                None,
                stats.as_ref().and_then(|s| s.count),
                stats.as_ref().and_then(|s| s.size),
                stats.as_ref().and_then(|s| s.index_size),
            ));
        }
        Ok(out)
    }

    async fn exact_row_count(&self, database: &str, table: &str) -> Result<u64, DbError> {
        use mongodb::bson::doc;

        // `count_documents` runs an aggregation that actually counts.
        // `estimated_document_count` reads collection metadata, which is the
        // MongoDB spelling of a planner estimate and drifts after an unclean
        // shutdown — exactly the failure this project replaced.
        self.collection(database, table)
            .count_documents(doc! {})
            .await
            .map_err(|e| DbError::Query(format!("counting {database}.{table}: {e}")))
    }

    /// Hashed here rather than on the server.
    ///
    /// The relational implementations push the digest into the database because
    /// SQL can express one. MongoDB's equivalents are either version-gated
    /// internals or unavailable, so the documents are streamed and hashed
    /// locally instead. That is honest about the cost: this reads the whole
    /// collection over the wire, which is why deep verification is opt-in.
    ///
    /// Order independence is kept the same way MySQL keeps it — a per-document
    /// hash folded in with XOR — so the physical order of a restored collection
    /// does not have to match the source's.
    async fn table_digest(&self, database: &str, table: &str) -> Result<Option<String>, DbError> {
        use mongodb::bson::doc;
        use sha2::{Digest, Sha256};

        let mut cursor = self
            .collection(database, table)
            .find(doc! {})
            .batch_size(MONGO_DIGEST_BATCH)
            .await
            .map_err(|e| DbError::Query(format!("reading {database}.{table}: {e}")))?;

        let mut fold = [0u8; 32];
        let mut seen: u64 = 0;

        while cursor
            .advance()
            .await
            .map_err(|e| DbError::Query(format!("reading {database}.{table}: {e}")))?
        {
            let doc = cursor
                .deserialize_current()
                .map_err(|e| DbError::Query(format!("decoding a document: {e}")))?;

            let bytes = mongodb::bson::to_vec(&canonicalise(&doc))
                .map_err(|e| DbError::Query(format!("encoding a document: {e}")))?;

            let hash = Sha256::digest(&bytes);
            for (slot, byte) in fold.iter_mut().zip(hash.iter()) {
                *slot ^= byte;
            }
            seen += 1;
        }

        // An empty collection folds to all zeros, which is a real answer and
        // not a failure to compute one — the same value the MySQL digest gives
        // for an empty table.
        let _ = seen;
        Ok(Some(fold.iter().map(|b| format!("{b:02x}")).collect()))
    }

    async fn column_names(&self, database: &str, table: &str) -> Result<Vec<String>, DbError> {
        use mongodb::bson::doc;

        // The union of every top-level field name present. See the trait's
        // documentation for why this is exact rather than sampled.
        let pipeline = vec![
            doc! { "$project": { "kv": { "$objectToArray": "$$ROOT" } } },
            doc! { "$unwind": "$kv" },
            doc! { "$group": { "_id": "$kv.k" } },
            doc! { "$sort": { "_id": 1 } },
        ];

        let mut cursor = self
            .collection(database, table)
            .aggregate(pipeline)
            // A wide collection can exceed the 100 MB in-memory limit for
            // `$group`. Spilling is slower than failing, and far more useful.
            .allow_disk_use(true)
            .await
            .map_err(|e| DbError::Query(format!("reading fields of {database}.{table}: {e}")))?;

        let mut names = Vec::new();
        while cursor
            .advance()
            .await
            .map_err(|e| DbError::Query(format!("reading fields of {database}.{table}: {e}")))?
        {
            let doc = cursor
                .deserialize_current()
                .map_err(|e| DbError::Query(format!("decoding a field name: {e}")))?;
            if let Ok(name) = doc.get_str("_id") {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }

    async fn close(&self) {
        // The driver shuts its connection pool down when the client drops.
        // Taking `&self` means there is no client to consume here, and calling
        // the async `shutdown` would need one.
    }
}

/// The three numbers `list_tables` wants out of `$collStats`.
struct MongoStorageStats {
    count: Option<u64>,
    size: Option<u64>,
    index_size: Option<u64>,
}

async fn collection_storage_stats(
    db: &mongodb::Database,
    name: &str,
) -> Option<MongoStorageStats> {
    use mongodb::bson::doc;

    let mut cursor = db
        .collection::<mongodb::bson::Document>(name)
        .aggregate(vec![doc! { "$collStats": { "storageStats": {} } }])
        .await
        .ok()?;

    if !cursor.advance().await.ok()? {
        return None;
    }
    let doc = cursor.deserialize_current().ok()?;
    let stats = doc.get_document("storageStats").ok()?;

    /// `$collStats` reports these as int32 or int64 depending on magnitude, so
    /// asking for one specific width would read zero for half of them.
    fn number(stats: &mongodb::bson::Document, key: &str) -> Option<u64> {
        match stats.get(key)? {
            mongodb::bson::Bson::Int32(v) => Some((*v).max(0) as u64),
            mongodb::bson::Bson::Int64(v) => Some((*v).max(0) as u64),
            mongodb::bson::Bson::Double(v) => Some(v.max(0.0) as u64),
            _ => None,
        }
    }

    Some(MongoStorageStats {
        count: number(stats, "count"),
        size: number(stats, "size"),
        index_size: number(stats, "totalIndexSize"),
    })
}

/// Drop a database, in whichever dialect the engine speaks.
///
/// Exists so that callers with a legitimate reason to drop one — a sync that
/// owns the database it created, a drill cleaning up after itself — do not each
/// have to hold both the SQL spelling and the MongoDB one. Both callers guard
/// *which* name may be passed; this only decides how.
pub async fn drop_database(params: &ConnectParams, name: &str) -> Result<(), DbError> {
    match params.engine {
        Engine::Mysql => {
            let quoted = quote_mysql_ident(name)?;
            execute_raw(params, &format!("DROP DATABASE IF EXISTS {quoted}")).await
        }
        Engine::Postgres => {
            let quoted = quote_pg_ident(name)?;
            execute_raw(params, &format!("DROP DATABASE IF EXISTS {quoted}")).await
        }
        Engine::Mongo => {
            if name.contains('\0') {
                return Err(DbError::InvalidIdentifier(
                    "database name contains a null byte".into(),
                ));
            }
            let introspector = MongoIntrospector::connect(params).await?;
            // Dropping a database that is not there is not an error in
            // MongoDB, which is the `IF EXISTS` the SQL branches ask for.
            introspector
                .client
                .database(name)
                .drop()
                .await
                .map_err(|e| DbError::Query(format!("dropping {name}: {e}")))
        }
    }
}

/// A statement and the values bound into its placeholders.
///
/// Values are always bound, never interpolated. Identifiers cannot be bound in
/// SQL, so those go through [`quote_mysql_ident`] / [`quote_pg_ident`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub sql: String,
    pub binds: Vec<String>,
}

/// Run write statements in order, on one connection.
///
/// Returns the rows each statement affected. Sharing a connection matters: the
/// alternative dials the destination once per statement, and through an SSH
/// tunnel that is the dominant cost of masking a database.
///
/// Not on [`Introspector`], which is documented as read-only catalog access —
/// keeping writes out of the read trait means nothing acquires the ability to
/// modify data by accident.
pub async fn execute_batch(
    params: &ConnectParams,
    statements: &[Statement],
) -> Result<Vec<u64>, DbError> {
    let mut affected = Vec::with_capacity(statements.len());

    match params.engine {
        Engine::Mysql => {
            let introspector = MysqlIntrospector::connect(params).await?;
            for statement in statements {
                let mut q = sqlx::query(&statement.sql);
                for bind in &statement.binds {
                    q = q.bind(bind);
                }
                let result = q
                    .execute(&introspector.pool)
                    .await
                    .map_err(|e| DbError::Query(format!("executing statement: {e}")))?;
                affected.push(result.rows_affected());
            }
            introspector.pool.close().await;
        }
        Engine::Postgres => {
            let introspector = PostgresIntrospector::connect(params).await?;
            for statement in statements {
                let mut q = sqlx::query(&statement.sql);
                for bind in &statement.binds {
                    q = q.bind(bind);
                }
                let result = q
                    .execute(&introspector.pool)
                    .await
                    .map_err(|e| DbError::Query(format!("executing statement: {e}")))?;
                affected.push(result.rows_affected());
            }
            introspector.pool.close().await;
        }
        Engine::Mongo => return Err(DbError::NotSql(Engine::Mongo)),
    }

    Ok(affected)
}

/// Run queries that each return one row of integers.
///
/// The shape masking's read-back needs: one `SELECT` per table projecting a
/// count per masked column.
pub async fn fetch_count_rows(
    params: &ConnectParams,
    queries: &[Statement],
) -> Result<Vec<Vec<i64>>, DbError> {
    let mut out = Vec::with_capacity(queries.len());

    macro_rules! run {
        ($pool:expr) => {{
            for query in queries {
                let mut raw = sqlx::query(&query.sql);
                for bind in &query.binds {
                    raw = raw.bind(bind);
                }
                let row = raw
                    .fetch_one($pool)
                    .await
                    .map_err(|e| DbError::Query(format!("reading counts: {e}")))?;
                let mut counts = Vec::new();
                for i in 0..row.len() {
                    counts.push(row.try_get::<i64, _>(i).map_err(|e| {
                        DbError::Query(format!("count {i} was not an integer: {e}"))
                    })?);
                }
                out.push(counts);
            }
        }};
    }

    match params.engine {
        Engine::Mysql => {
            let introspector = MysqlIntrospector::connect(params).await?;
            run!(&introspector.pool);
            introspector.pool.close().await;
        }
        Engine::Postgres => {
            let introspector = PostgresIntrospector::connect(params).await?;
            run!(&introspector.pool);
            introspector.pool.close().await;
        }
        Engine::Mongo => return Err(DbError::NotSql(Engine::Mongo)),
    }

    Ok(out)
}

/// Run a statement that returns nothing.
///
/// Deliberately narrow and deliberately not on [`Introspector`], which is
/// documented as read-only catalog access. The only caller is the drill's
/// cleanup, and keeping the write path out of the read trait means nothing
/// else acquires the ability to execute DDL by accident.
pub async fn execute_raw(params: &ConnectParams, sql: &str) -> Result<(), DbError> {
    match params.engine {
        Engine::Mysql => {
            let introspector = MysqlIntrospector::connect(params).await?;
            sqlx::query(sql)
                .execute(&introspector.pool)
                .await
                .map_err(|e| DbError::Query(format!("executing statement: {e}")))?;
            introspector.pool.close().await;
        }
        Engine::Postgres => {
            let introspector = PostgresIntrospector::connect(params).await?;
            sqlx::query(sql)
                .execute(&introspector.pool)
                .await
                .map_err(|e| DbError::Query(format!("executing statement: {e}")))?;
            introspector.pool.close().await;
        }
        Engine::Mongo => return Err(DbError::NotSql(Engine::Mongo)),
    }
    Ok(())
}

/// Connect using whichever driver matches the engine.
pub async fn connect(params: &ConnectParams) -> Result<Box<dyn Introspector>, DbError> {
    match params.engine {
        Engine::Mysql => Ok(Box::new(MysqlIntrospector::connect(params).await?)),
        Engine::Postgres => Ok(Box::new(PostgresIntrospector::connect(params).await?)),
        Engine::Mongo => Ok(Box::new(MongoIntrospector::connect(params).await?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(engine: Option<&str>) -> TableInfo {
        TableInfo::new(
            None,
            "orders".into(),
            engine.map(str::to_string),
            Some(10),
            Some(100),
            Some(20),
        )
    }

    #[test]
    fn qualified_name_includes_schema_when_present() {
        let mut t = table(None);
        assert_eq!(t.qualified_name(), "orders");
        t.schema = Some("public".into());
        assert_eq!(t.qualified_name(), "public.orders");
    }

    #[test]
    fn total_bytes_sums_data_and_index() {
        assert_eq!(table(Some("InnoDB")).total_bytes(), 120);
    }

    #[test]
    fn missing_size_fields_do_not_panic() {
        let t = TableInfo::new(None, "t".into(), None, None, None, None);
        assert_eq!(t.total_bytes(), 0);
    }

    #[test]
    fn myisam_is_flagged_as_non_transactional() {
        assert!(table(Some("InnoDB")).is_transactional());
        assert!(table(Some("innodb")).is_transactional());
        assert!(!table(Some("MyISAM")).is_transactional());
        assert!(table(None).is_transactional(), "postgres is transactional");
    }

    #[test]
    fn mysql_identifiers_are_quoted() {
        assert_eq!(quote_mysql_ident("orders").unwrap(), "`orders`");
        assert_eq!(quote_mysql_ident("order").unwrap(), "`order`");
        assert_eq!(
            quote_mysql_ident("日本語テーブル").unwrap(),
            "`日本語テーブル`"
        );
    }

    #[test]
    fn mysql_identifier_injection_is_neutralised() {
        // A table can legally be named this. Interpolated raw, it would run.
        let evil = "a`; DROP DATABASE app; SELECT `1";
        let quoted = quote_mysql_ident(evil).unwrap();
        assert_eq!(quoted, "`a``; DROP DATABASE app; SELECT ``1`");
        // Every embedded backtick is doubled, so the identifier never closes early.
        assert_eq!(quoted.matches("``").count(), 2);
    }

    #[test]
    fn postgres_identifiers_are_quoted() {
        assert_eq!(quote_pg_ident("orders").unwrap(), "\"orders\"");
        assert_eq!(quote_pg_ident("select").unwrap(), "\"select\"");
    }

    #[test]
    fn postgres_identifier_injection_is_neutralised() {
        let evil = "a\"; DROP TABLE users; --";
        let quoted = quote_pg_ident(evil).unwrap();
        assert_eq!(quoted, "\"a\"\"; DROP TABLE users; --\"");
    }

    #[test]
    fn null_bytes_in_identifiers_are_rejected() {
        assert!(quote_mysql_ident("bad\0name").is_err());
        assert!(quote_pg_ident("bad\0name").is_err());
    }

    #[test]
    fn qualified_names_split_with_a_default_schema() {
        assert_eq!(
            split_qualified("public.orders", "public"),
            ("public".to_string(), "orders".to_string())
        );
        assert_eq!(
            split_qualified("orders", "public"),
            ("public".to_string(), "orders".to_string())
        );
        assert_eq!(
            split_qualified("reporting.daily", "public"),
            ("reporting".to_string(), "daily".to_string())
        );
    }
}
