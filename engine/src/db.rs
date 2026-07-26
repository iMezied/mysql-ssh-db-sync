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
            // PostgreSQL is always transactional.
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
    async fn column_names(&self, database: &str, table: &str) -> Result<Vec<String>, DbError>;
    async fn close(&self);
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
    }
    Ok(())
}

/// Connect using whichever driver matches the engine.
pub async fn connect(params: &ConnectParams) -> Result<Box<dyn Introspector>, DbError> {
    match params.engine {
        Engine::Mysql => Ok(Box::new(MysqlIntrospector::connect(params).await?)),
        Engine::Postgres => Ok(Box::new(PostgresIntrospector::connect(params).await?)),
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
