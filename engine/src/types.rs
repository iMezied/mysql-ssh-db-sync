//! Shared domain types.
//!
//! These live here rather than in `events` so that persistence, profiles and
//! jobs can all reference them without importing the event system.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use specta::Type;

/// A supported database engine.
///
/// The serde representation is snake_case and is the *single* source of truth
/// for how these values are written to SQLite and to JSON manifests. Do not
/// hand-write match arms mapping these to strings — use `Display`/`FromStr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    Mysql,
    Postgres,
    Mongo,
}

impl Engine {
    pub const fn as_str(self) -> &'static str {
        match self {
            Engine::Mysql => "mysql",
            Engine::Postgres => "postgres",
            Engine::Mongo => "mongo",
        }
    }

    /// Default TCP port for this engine.
    pub const fn default_port(self) -> u16 {
        match self {
            Engine::Mysql => 3306,
            Engine::Postgres => 5432,
            Engine::Mongo => 27017,
        }
    }

    /// Whether this engine stores rows in tables with a fixed column list.
    ///
    /// The vocabulary in this codebase — table, row, column — is relational,
    /// and MongoDB maps onto it closely enough that [`crate::db::Introspector`]
    /// stays one trait: a collection is a table, a document is a row, a field
    /// is a column. The places that genuinely cannot be papered over are the
    /// ones that *generate SQL*, and this is what they branch on.
    ///
    /// The other real difference is that a document store has no declared
    /// schema, so a field list is sampled rather than read — see
    /// [`crate::db::Introspector::column_names`].
    pub const fn is_relational(self) -> bool {
        matches!(self, Engine::Mysql | Engine::Postgres)
    }

    /// Every engine, for exhaustive tests and for the CLI's help text.
    pub const ALL: [Engine; 3] = [Engine::Mysql, Engine::Postgres, Engine::Mongo];
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Engine {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mysql" => Ok(Engine::Mysql),
            "postgres" => Ok(Engine::Postgres),
            "mongo" => Ok(Engine::Mongo),
            other => Err(ParseEnumError::new("Engine", other)),
        }
    }
}

/// Environment classification for a connection profile.
///
/// Drives colour-coding in the UI and the strictness of destructive-action
/// confirmations — production targets require typed confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentTag {
    Prod,
    Staging,
    Dev,
}

impl EnvironmentTag {
    pub const fn as_str(self) -> &'static str {
        match self {
            EnvironmentTag::Prod => "prod",
            EnvironmentTag::Staging => "staging",
            EnvironmentTag::Dev => "dev",
        }
    }

    /// Whether destructive operations against this environment require the
    /// user to type the target name to confirm.
    pub const fn requires_typed_confirmation(self) -> bool {
        matches!(self, EnvironmentTag::Prod)
    }
}

impl fmt::Display for EnvironmentTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EnvironmentTag {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "prod" => Ok(EnvironmentTag::Prod),
            "staging" => Ok(EnvironmentTag::Staging),
            "dev" => Ok(EnvironmentTag::Dev),
            other => Err(ParseEnumError::new("EnvironmentTag", other)),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid {kind} value: {value:?}")]
pub struct ParseEnumError {
    pub kind: &'static str,
    pub value: String,
}

impl ParseEnumError {
    fn new(kind: &'static str, value: &str) -> Self {
        Self {
            kind,
            value: value.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_roundtrips_through_string() {
        for e in Engine::ALL {
            assert_eq!(Engine::from_str(&e.to_string()).unwrap(), e);
        }
    }

    #[test]
    fn engine_string_matches_serde_representation() {
        // The DB column and the JSON payload must agree, or profiles written by
        // the GUI become unreadable by the CLI.
        for e in Engine::ALL {
            let json = serde_json::to_string(&e).unwrap();
            assert_eq!(json, format!("\"{}\"", e.as_str()));
        }
    }

    #[test]
    fn every_engine_has_a_distinct_default_port() {
        let mut ports: Vec<u16> = Engine::ALL.iter().map(|e| e.default_port()).collect();
        ports.sort_unstable();
        let before = ports.len();
        ports.dedup();
        assert_eq!(before, ports.len(), "two engines share a default port");
    }

    #[test]
    fn only_the_sql_engines_are_relational() {
        assert!(Engine::Mysql.is_relational());
        assert!(Engine::Postgres.is_relational());
        assert!(
            !Engine::Mongo.is_relational(),
            "masking and identifier quoting branch on this"
        );
    }

    #[test]
    fn environment_roundtrips_through_string() {
        for t in [
            EnvironmentTag::Prod,
            EnvironmentTag::Staging,
            EnvironmentTag::Dev,
        ] {
            assert_eq!(EnvironmentTag::from_str(&t.to_string()).unwrap(), t);
        }
    }

    #[test]
    fn environment_string_matches_serde_representation() {
        for t in [
            EnvironmentTag::Prod,
            EnvironmentTag::Staging,
            EnvironmentTag::Dev,
        ] {
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(json, format!("\"{}\"", t.as_str()));
        }
    }

    #[test]
    fn unknown_values_are_rejected() {
        assert!(Engine::from_str("mariadb").is_err());
        assert!(EnvironmentTag::from_str("production").is_err());
    }
}
