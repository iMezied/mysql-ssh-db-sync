//! Database introspection.
//!
//! Used to populate the table picker and to verify restores. Connections here
//! are for *queries only* — bulk dump and restore go through the vendor tools.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use specta::Type;

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
}

impl TableInfo {
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
        match self.storage_engine.as_deref() {
            Some(e) => e.eq_ignore_ascii_case("innodb"),
            // PostgreSQL is always transactional.
            None => true,
        }
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
    #[error("not implemented until M1': {0}")]
    NotImplemented(&'static str),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(engine: Option<&str>) -> TableInfo {
        TableInfo {
            schema: None,
            name: "orders".into(),
            storage_engine: engine.map(str::to_string),
            estimated_rows: Some(10),
            data_bytes: Some(100),
            index_bytes: Some(20),
        }
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
        let t = TableInfo {
            schema: None,
            name: "t".into(),
            storage_engine: None,
            estimated_rows: None,
            data_bytes: None,
            index_bytes: None,
        };
        assert_eq!(t.total_bytes(), 0);
    }

    #[test]
    fn myisam_is_flagged_as_non_transactional() {
        assert!(table(Some("InnoDB")).is_transactional());
        assert!(table(Some("innodb")).is_transactional());
        assert!(!table(Some("MyISAM")).is_transactional());
        assert!(table(None).is_transactional(), "postgres is transactional");
    }
}
