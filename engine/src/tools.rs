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
    Mongodump,
    Mongorestore,
}

impl Tool {
    /// Every tool, so discovery and the settings page cannot drift from the
    /// enum by forgetting to list one.
    pub const ALL: [Tool; 10] = [
        Tool::Mysqldump,
        Tool::Mysql,
        Tool::Mydumper,
        Tool::Myloader,
        Tool::PgDump,
        Tool::PgDumpall,
        Tool::PgRestore,
        Tool::Psql,
        Tool::Mongodump,
        Tool::Mongorestore,
    ];

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
            Tool::Mongodump => "mongodump",
            Tool::Mongorestore => "mongorestore",
        }
    }

    pub const fn engine(self) -> Engine {
        match self {
            Tool::Mysqldump | Tool::Mysql | Tool::Mydumper | Tool::Myloader => Engine::Mysql,
            Tool::PgDump | Tool::PgDumpall | Tool::PgRestore | Tool::Psql => Engine::Postgres,
            Tool::Mongodump | Tool::Mongorestore => Engine::Mongo,
        }
    }

    /// Tools without which the engine cannot function at all.
    pub const fn is_required(self) -> bool {
        matches!(
            self,
            Tool::Mysqldump
                | Tool::Mysql
                | Tool::PgDump
                | Tool::PgRestore
                | Tool::Psql
                | Tool::Mongodump
                | Tool::Mongorestore
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

/// Check `mongodump` against the server it will read.
///
/// The interesting thing here is what this deliberately does **not** do.
///
/// `mongodump` ships in the MongoDB Database Tools, which are versioned in
/// their own `100.x` series, unrelated to the server's. A client reporting
/// `100.9.4` against a `7.0.5` server is the *normal* case, not a client 93
/// majors ahead — so the comparison [`check_pg_dump_compatibility`] makes is
/// meaningless here, and making it would block every correctly-installed
/// MongoDB setup on this planet.
///
/// What is worth saying is the reverse: a `mongodump` whose major version is
/// *not* in the 100 series is one of the old server-bundled builds that were
/// retired at MongoDB 4.4, and those really do fail against a modern server.
/// That gets a warning rather than a block, because a user pointing an
/// override at a deliberately old binary for a deliberately old server is
/// making a choice this app has no business overruling.
pub fn check_mongodump_compatibility(client: Version, server: Version) -> CompatibilityVerdict {
    if client.major < 100 {
        return CompatibilityVerdict::Warn(format!(
            "mongodump {client} predates the MongoDB Database Tools, which were split out \
             at server 4.4 and are versioned separately from the server ({server}). \
             Install the Database Tools (a 100.x mongodump) unless this old binary is \
             deliberate."
        ));
    }
    CompatibilityVerdict::Ok
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
    fn every_engine_has_the_required_tools_to_dump_and_restore() {
        let required: Vec<Tool> = Tool::ALL.into_iter().filter(|t| t.is_required()).collect();

        for engine in Engine::ALL {
            assert!(
                required.iter().any(|t| t.engine() == engine),
                "{engine} has no required tool, so nothing would be discovered for it"
            );
        }
        assert!(!Tool::Mydumper.is_required(), "parallel mode is optional");
    }

    #[test]
    fn tool_binary_names_are_unique() {
        let mut names: Vec<&str> = Tool::ALL.iter().map(|t| t.binary_name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "two tools resolve to one binary");
    }

    #[test]
    fn parses_mongodump_version_banner() {
        let v = Version::parse_first("mongodump version: 100.9.4").unwrap();
        assert_eq!(v, Version::new(100, 9, 4));
    }

    #[test]
    fn mongodump_is_not_version_matched_against_the_server() {
        // The Database Tools are versioned 100.x independently of the server.
        // Applying pg_dump's rule here would block every correct install: a
        // 100.9.4 client against a 7.0.5 server is normal, not 93 majors ahead.
        let verdict =
            check_mongodump_compatibility(Version::new(100, 9, 4), Version::new(7, 0, 5));
        assert!(verdict.is_ok(), "got: {verdict:?}");

        // And the same pairing under pg_dump's rule would be refused — which is
        // exactly why the two checks cannot share an implementation.
        assert!(matches!(
            check_pg_dump_compatibility(Version::new(100, 9, 4), Version::new(7, 0, 5)),
            CompatibilityVerdict::Warn(_)
        ));
    }

    #[test]
    fn a_pre_database_tools_mongodump_is_flagged() {
        let verdict = check_mongodump_compatibility(Version::new(4, 2, 3), Version::new(7, 0, 5));
        assert!(matches!(verdict, CompatibilityVerdict::Warn(_)));
    }
}
