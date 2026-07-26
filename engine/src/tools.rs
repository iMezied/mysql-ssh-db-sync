//! Discovery and version checking for external client binaries.
//!
//! We shell out to the vendors' own dump/restore tools rather than
//! reimplementing their formats. We do NOT bundle Oracle's `mysqldump`: it is
//! GPLv2 and bundling would impose that licence on the whole app. Binaries are
//! discovered on the host and may be overridden per profile.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::types::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Mysqldump,
    Mysql,
    Mydumper,
    Myloader,
    PgDump,
    PgDumpall,
    PgRestore,
    Psql,
}

impl Tool {
    pub const fn binary_name(self) -> &'static str {
        match self {
            Tool::Mysqldump => "mysqldump",
            Tool::Mysql => "mysql",
            Tool::Mydumper => "mydumper",
            Tool::Myloader => "myloader",
            Tool::PgDump => "pg_dump",
            Tool::PgDumpall => "pg_dumpall",
            Tool::PgRestore => "pg_restore",
            Tool::Psql => "psql",
        }
    }

    pub const fn engine(self) -> Engine {
        match self {
            Tool::Mysqldump | Tool::Mysql | Tool::Mydumper | Tool::Myloader => Engine::Mysql,
            Tool::PgDump | Tool::PgDumpall | Tool::PgRestore | Tool::Psql => Engine::Postgres,
        }
    }

    /// Tools without which the engine cannot function at all.
    pub const fn is_required(self) -> bool {
        matches!(
            self,
            Tool::Mysqldump | Tool::Mysql | Tool::PgDump | Tool::PgRestore | Tool::Psql
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DiscoveredTool {
    pub tool: Tool,
    pub path: PathBuf,
    pub version: Option<Version>,
}

/// A `major.minor.patch` version, with unknown components treated as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Type)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Extract the first `N[.N[.N]]` found in a tool's `--version` output.
    ///
    /// Formats vary: `pg_dump (PostgreSQL) 16.2`, `mysqldump  Ver 8.0.42 for
    /// osx10.19`, `mysqldump Ver 10.19 Distrib 10.11.6-MariaDB`.
    pub fn parse_first(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_digit() {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                let token = &text[start..i];
                let mut parts = token.split('.').filter(|p| !p.is_empty());
                let major = parts.next()?.parse().ok()?;
                let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
                let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
                return Some(Self::new(major, minor, patch));
            }
            i += 1;
        }
        None
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum CompatibilityVerdict {
    Ok,
    /// Allowed, but the user should know.
    Warn(String),
    /// Refused unless the user explicitly overrides.
    Blocked(String),
}

impl CompatibilityVerdict {
    pub const fn is_ok(&self) -> bool {
        matches!(self, CompatibilityVerdict::Ok)
    }
}

/// Check `pg_dump` against the server it will read.
///
/// Two distinct hazards, and they point in opposite directions:
///
/// * **Client older than the server** — `pg_dump` refuses outright, part-way
///   through, with a confusing message. Blocked.
/// * **Client newer than the server** — the dump *succeeds*, but embeds
///   directives the older server does not understand, so it fails on restore.
///   pg_dump 18 emits `SET transaction_timeout = 0`, a parameter introduced in
///   PostgreSQL 17; restoring that into a 16 server aborts with "unrecognized
///   configuration parameter". Warned, with the fix named, because dumping with
///   a newer client is fine when the destination is equally new.
pub fn check_pg_dump_compatibility(client: Version, server: Version) -> CompatibilityVerdict {
    if client.major < server.major {
        return CompatibilityVerdict::Blocked(format!(
            "pg_dump {client} is older than the server ({server}); \
             pg_dump cannot dump from a newer server. Install PostgreSQL {} client tools.",
            server.major
        ));
    }
    if client.major > server.major {
        return CompatibilityVerdict::Warn(format!(
            "pg_dump {client} is newer than the server ({server}). The dump will \
             succeed, but it may contain directives this server version does not \
             understand, so restoring it back into a PostgreSQL {} server can fail. \
             Use PostgreSQL {} client tools to match the server.",
            server.major, server.major
        ));
    }
    CompatibilityVerdict::Ok
}

/// An 8.0+ `mysqldump` queries `information_schema.COLUMN_STATISTICS`, which
/// does not exist before 8.0 — the dump fails unless `--column-statistics=0`
/// is passed.
pub fn mysql_needs_column_statistics_flag(client: Version, server: Version) -> bool {
    client.major >= 8 && server.major < 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_postgres_version_banner() {
        let v = Version::parse_first("pg_dump (PostgreSQL) 16.2").unwrap();
        assert_eq!(v, Version::new(16, 2, 0));
    }

    #[test]
    fn parses_mysql_version_banner() {
        let v = Version::parse_first("mysqldump  Ver 8.0.42 for macos14 on arm64").unwrap();
        assert_eq!(v, Version::new(8, 0, 42));
    }

    #[test]
    fn parses_bare_major_version() {
        assert_eq!(
            Version::parse_first("psql (PostgreSQL) 17").unwrap(),
            Version::new(17, 0, 0)
        );
    }

    #[test]
    fn returns_none_when_no_digits_present() {
        assert!(Version::parse_first("command not found").is_none());
    }

    #[test]
    fn versions_order_numerically_not_lexically() {
        assert!(Version::new(9, 0, 0) < Version::new(10, 0, 0));
        assert!(Version::new(8, 0, 42) > Version::new(8, 0, 9));
    }

    #[test]
    fn older_pg_dump_against_newer_server_is_blocked() {
        let verdict = check_pg_dump_compatibility(Version::new(15, 6, 0), Version::new(16, 2, 0));
        assert!(matches!(verdict, CompatibilityVerdict::Blocked(_)));
    }

    #[test]
    fn matching_major_versions_are_ok() {
        assert!(
            check_pg_dump_compatibility(Version::new(16, 0, 0), Version::new(16, 2, 0)).is_ok()
        );
    }

    #[test]
    fn even_one_major_newer_is_flagged() {
        // Not "close enough": PostgreSQL 17 added transaction_timeout, which a
        // 16 server rejects when restoring a dump taken with a 17 client.
        let verdict = check_pg_dump_compatibility(Version::new(17, 0, 0), Version::new(16, 2, 0));
        assert!(matches!(verdict, CompatibilityVerdict::Warn(_)));
    }

    #[test]
    fn column_statistics_flag_needed_only_for_new_client_old_server() {
        assert!(mysql_needs_column_statistics_flag(
            Version::new(8, 0, 42),
            Version::new(5, 7, 40)
        ));
        assert!(!mysql_needs_column_statistics_flag(
            Version::new(8, 0, 42),
            Version::new(8, 0, 30)
        ));
        assert!(!mysql_needs_column_statistics_flag(
            Version::new(5, 7, 40),
            Version::new(5, 7, 40)
        ));
    }

    #[test]
    fn required_tools_cover_both_engines() {
        let required: Vec<Tool> = [
            Tool::Mysqldump,
            Tool::Mysql,
            Tool::Mydumper,
            Tool::Myloader,
            Tool::PgDump,
            Tool::PgDumpall,
            Tool::PgRestore,
            Tool::Psql,
        ]
        .into_iter()
        .filter(|t| t.is_required())
        .collect();

        assert!(required.iter().any(|t| t.engine() == Engine::Mysql));
        assert!(required.iter().any(|t| t.engine() == Engine::Postgres));
        assert!(!Tool::Mydumper.is_required(), "parallel mode is optional");
    }
}
