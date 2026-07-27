//! The backup library: artifacts on disk and what is known about them.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::manifest::BackupManifest;
use crate::retention::{RetentionCandidate, RetentionPlan, RetentionPolicy, plan_retention};
use crate::types::Engine;

/// One artifact, as the library lists it.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct Artifact {
    pub path: String,
    pub filename: String,
    #[specta(type = f64)]
    pub size_bytes: u64,
    pub modified_at: DateTime<Utc>,
    /// Populated when a readable manifest sits beside the artifact.
    pub database: Option<String>,
    pub engine: Option<Engine>,
    pub source_profile_name: Option<String>,
    pub table_count: Option<u32>,
    pub tables_with_data: Option<u32>,
    /// `None` when there is no manifest to check against.
    pub has_manifest: bool,
}

/// List artifacts in a directory, newest first.
///
/// An artifact without a manifest is still listed: it may predate the app or
/// have been copied in by hand, and hiding it would be worse than showing it
/// with unknown metadata.
pub fn list_artifacts(dir: impl AsRef<Path>) -> Vec<Artifact> {
    let Ok(entries) = std::fs::read_dir(dir.as_ref()) else {
        return Vec::new();
    };

    let mut artifacts: Vec<Artifact> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_artifact(p))
        .filter_map(|path| describe(&path))
        .collect();

    artifacts.sort_by_key(|a| std::cmp::Reverse(a.modified_at));
    artifacts
}

fn is_artifact(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // The manifest lives alongside the artifact and is not one itself.
    !name.ends_with(".manifest.json") && (name.ends_with(".sql.gz") || name.ends_with(".dump"))
}

fn describe(path: &Path) -> Option<Artifact> {
    let meta = std::fs::metadata(path).ok()?;
    let modified: DateTime<Utc> = meta.modified().ok()?.into();
    let manifest = BackupManifest::read(path).ok();

    Some(Artifact {
        path: path.display().to_string(),
        filename: path.file_name()?.to_string_lossy().into_owned(),
        size_bytes: meta.len(),
        modified_at: modified,
        database: manifest.as_ref().map(|m| m.database.clone()),
        engine: manifest.as_ref().map(|m| m.engine),
        source_profile_name: manifest.as_ref().map(|m| m.source_profile_name.clone()),
        table_count: manifest.as_ref().map(|m| m.tables.len() as u32),
        tables_with_data: manifest.as_ref().map(|m| m.tables_with_data.len() as u32),
        has_manifest: manifest.is_some(),
    })
}

// ── Analytics ───────────────────────────────────────────────────────────

/// How much smaller than its predecessor an artifact may be before it is
/// called out.
///
/// Not a guess at a "normal" growth rate — backups do legitimately shrink when
/// rows are archived or a table is dropped. It is set where a *halving* trips
/// it, because the failures worth catching are categorical rather than
/// gradual: a table that stopped being selected, a dump that was truncated, a
/// `--where` filter that started matching nothing. Those roughly halve a file
/// or worse; a month of deletions does not.
pub const SHRINK_RATIO: f64 = 0.5;

/// Artifacts smaller than this are not compared at all.
///
/// A 300-byte schema-only dump next to a 200-byte one is a 33% "shrink" and
/// means nothing. Comparing them would produce warnings nobody can act on,
/// which is how a warning stops being read.
pub const SHRINK_FLOOR_BYTES: u64 = 64 * 1024;

/// One artifact's size at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SizePoint {
    pub at: DateTime<Utc>,
    #[specta(type = f64)]
    pub bytes: u64,
    pub filename: String,
}

/// A backup that came out dramatically smaller than the one before it.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ShrinkWarning {
    pub filename: String,
    pub at: DateTime<Utc>,
    #[specta(type = f64)]
    pub bytes: u64,
    pub previous_filename: String,
    #[specta(type = f64)]
    pub previous_bytes: u64,
}

impl ShrinkWarning {
    /// How much of the previous artifact's size this one is, as a percentage.
    pub fn percent_of_previous(&self) -> f64 {
        if self.previous_bytes == 0 {
            return 100.0;
        }
        (self.bytes as f64 / self.previous_bytes as f64) * 100.0
    }
}

/// What the library holds for one database.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DatabaseStats {
    pub database: String,
    pub engine: Option<Engine>,
    #[specta(type = f64)]
    pub artifacts: usize,
    #[specta(type = f64)]
    pub total_bytes: u64,
    #[specta(type = f64)]
    pub newest_bytes: u64,
    pub newest_at: DateTime<Utc>,
    pub oldest_at: DateTime<Utc>,
    /// Oldest first, so a chart reads left to right.
    pub series: Vec<SizePoint>,
    /// Average change per day across the whole span.
    ///
    /// `None` with fewer than two artifacts, or when they share a timestamp —
    /// there is no rate to report, and inventing one from a single point is
    /// the kind of number that gets quoted back later as if it meant
    /// something.
    pub bytes_per_day: Option<f64>,
    pub shrinks: Vec<ShrinkWarning>,
}

/// Everything the library page reports.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct LibraryStats {
    #[specta(type = f64)]
    pub total_artifacts: usize,
    #[specta(type = f64)]
    pub total_bytes: u64,
    /// Artifacts with no readable manifest, so no database to group under.
    ///
    /// Reported rather than dropped: they still take up space, and a library
    /// whose totals silently exclude them would understate what is on disk.
    #[specta(type = f64)]
    pub unattributed: usize,
    #[specta(type = f64)]
    pub unattributed_bytes: u64,
    /// Largest total first — the one filling the disk is the one to look at.
    pub databases: Vec<DatabaseStats>,
}

impl LibraryStats {
    /// Every shrink warning across every database, newest first.
    pub fn all_shrinks(&self) -> Vec<&ShrinkWarning> {
        let mut all: Vec<&ShrinkWarning> = self
            .databases
            .iter()
            .flat_map(|d| d.shrinks.iter())
            .collect();
        all.sort_by_key(|s| std::cmp::Reverse(s.at));
        all
    }
}

/// Summarise a directory of artifacts.
pub fn stats(dir: impl AsRef<Path>) -> LibraryStats {
    summarise(list_artifacts(dir))
}

/// The pure half, so the arithmetic is testable without a filesystem.
pub fn summarise(artifacts: Vec<Artifact>) -> LibraryStats {
    let total_artifacts = artifacts.len();
    let total_bytes = artifacts.iter().map(|a| a.size_bytes).sum();

    let mut grouped: std::collections::BTreeMap<String, Vec<Artifact>> =
        std::collections::BTreeMap::new();
    let mut unattributed = 0usize;
    let mut unattributed_bytes = 0u64;

    for artifact in artifacts {
        match artifact.database.clone() {
            Some(database) => grouped.entry(database).or_default().push(artifact),
            None => {
                unattributed += 1;
                unattributed_bytes += artifact.size_bytes;
            }
        }
    }

    let mut databases: Vec<DatabaseStats> = grouped
        .into_iter()
        .map(|(database, group)| database_stats(database, group))
        .collect();

    // Largest first: the question this answers is "what is filling the disk".
    databases.sort_by_key(|d| std::cmp::Reverse(d.total_bytes));

    LibraryStats {
        total_artifacts,
        total_bytes,
        unattributed,
        unattributed_bytes,
        databases,
    }
}

fn database_stats(database: String, mut group: Vec<Artifact>) -> DatabaseStats {
    // Oldest first for both the series and the shrink comparison: "smaller
    // than the one before it" only means anything in chronological order.
    group.sort_by_key(|a| a.modified_at);

    let series: Vec<SizePoint> = group
        .iter()
        .map(|a| SizePoint {
            at: a.modified_at,
            bytes: a.size_bytes,
            filename: a.filename.clone(),
        })
        .collect();

    let mut shrinks = Vec::new();
    for pair in group.windows(2) {
        let (previous, current) = (&pair[0], &pair[1]);
        if previous.size_bytes < SHRINK_FLOOR_BYTES {
            continue;
        }
        if (current.size_bytes as f64) < previous.size_bytes as f64 * SHRINK_RATIO {
            shrinks.push(ShrinkWarning {
                filename: current.filename.clone(),
                at: current.modified_at,
                bytes: current.size_bytes,
                previous_filename: previous.filename.clone(),
                previous_bytes: previous.size_bytes,
            });
        }
    }

    let oldest = group.first().expect("a group is never empty");
    let newest = group.last().expect("a group is never empty");

    let span_days = (newest.modified_at - oldest.modified_at).num_seconds() as f64 / 86_400.0;
    let bytes_per_day = (group.len() > 1 && span_days > 0.0)
        .then(|| (newest.size_bytes as f64 - oldest.size_bytes as f64) / span_days);

    DatabaseStats {
        database,
        engine: newest.engine,
        artifacts: group.len(),
        total_bytes: group.iter().map(|a| a.size_bytes).sum(),
        newest_bytes: newest.size_bytes,
        newest_at: newest.modified_at,
        oldest_at: oldest.modified_at,
        series,
        bytes_per_day,
        shrinks,
    }
}

/// Result of checking an artifact against its manifest.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum IntegrityCheck {
    Ok,
    Mismatch { expected: String, actual: String },
    NoManifest,
    Unreadable { detail: String },
}

/// Hash an artifact and compare it against its manifest.
pub fn check_integrity(path: impl AsRef<Path>) -> IntegrityCheck {
    let path = path.as_ref();
    let Ok(manifest) = BackupManifest::read(path) else {
        return IntegrityCheck::NoManifest;
    };

    match crate::manifest::sha256_file(path) {
        Ok(actual) if actual == manifest.sha256 => IntegrityCheck::Ok,
        Ok(actual) => IntegrityCheck::Mismatch {
            expected: manifest.sha256,
            actual,
        },
        Err(e) => IntegrityCheck::Unreadable {
            detail: e.to_string(),
        },
    }
}

/// Work out which artifacts a retention policy would remove.
///
/// Deliberately separate from applying it: the plan is shown, and only then
/// acted on. Deleting backups is not something to do invisibly.
pub fn plan_cleanup(dir: impl AsRef<Path>, policy: RetentionPolicy) -> RetentionPlan {
    let candidates: Vec<RetentionCandidate> = list_artifacts(dir)
        .into_iter()
        .map(|a| RetentionCandidate {
            path: a.path,
            created_at: a.modified_at,
            size_bytes: a.size_bytes,
        })
        .collect();

    plan_retention(candidates, policy, Utc::now())
}

/// Delete the artifacts a plan selected, along with their manifests.
///
/// Returns the paths actually removed, so the job log can record them.
pub fn apply_cleanup(plan: &RetentionPlan) -> Vec<String> {
    let mut removed = Vec::new();
    for candidate in &plan.delete {
        let path = PathBuf::from(&candidate.path);
        if std::fs::remove_file(&path).is_ok() {
            // An orphaned manifest would make the library show a phantom entry.
            let _ = std::fs::remove_file(BackupManifest::path_for(&path));
            removed.push(candidate.path.clone());
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::MANIFEST_VERSION;
    use crate::manifest::{ArtifactFormat, sha256_file};
    use uuid::Uuid;

    fn write_artifact(dir: &Path, name: &str, body: &[u8], with_manifest: bool) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();

        if with_manifest {
            let manifest = BackupManifest {
                manifest_version: MANIFEST_VERSION,
                id: Uuid::new_v4(),
                source_profile_id: Uuid::new_v4(),
                source_profile_name: "prod".into(),
                engine: Engine::Mysql,
                server_version: "8.0.42".into(),
                dump_tool: "mysqldump".into(),
                dump_tool_version: "8.0.42".into(),
                database: "app".into(),
                created_at: Utc::now(),
                format: ArtifactFormat::SqlGz,
                tables: vec!["users".into(), "orders".into()],
                tables_with_data: vec!["orders".into()],
                source_row_counts: Default::default(),
                options: serde_json::json!({}),
                artifact_filename: name.into(),
                size_bytes: body.len() as u64,
                sha256: sha256_file(&path).unwrap(),
                encrypted: false,
                encryption_recipients: Vec::new(),
            };
            manifest.write(&path).unwrap();
        }
        path
    }

    #[test]
    fn lists_artifacts_with_manifest_metadata() {
        let dir = tempfile::tempdir().unwrap();
        write_artifact(dir.path(), "app_1.sql.gz", b"one", true);

        let listed = list_artifacts(dir.path());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].database.as_deref(), Some("app"));
        assert_eq!(listed[0].table_count, Some(2));
        assert_eq!(listed[0].tables_with_data, Some(1));
        assert!(listed[0].has_manifest);
    }

    #[test]
    fn artifacts_without_a_manifest_are_still_listed() {
        let dir = tempfile::tempdir().unwrap();
        write_artifact(dir.path(), "orphan.sql.gz", b"x", false);

        let listed = list_artifacts(dir.path());
        assert_eq!(
            listed.len(),
            1,
            "hiding it would be worse than unknown metadata"
        );
        assert!(!listed[0].has_manifest);
        assert!(listed[0].database.is_none());
    }

    #[test]
    fn manifests_are_not_listed_as_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        write_artifact(dir.path(), "app.sql.gz", b"x", true);

        let listed = list_artifacts(dir.path());
        assert_eq!(
            listed.len(),
            1,
            "the .manifest.json must not appear as an artifact"
        );
    }

    #[test]
    fn unrelated_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"hello").unwrap();
        assert!(list_artifacts(dir.path()).is_empty());
    }

    #[test]
    fn a_missing_directory_lists_nothing_rather_than_erroring() {
        assert!(list_artifacts("/nonexistent/backup/dir").is_empty());
    }

    #[test]
    fn integrity_passes_for_an_untouched_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_artifact(dir.path(), "app.sql.gz", b"contents", true);
        assert!(matches!(check_integrity(&path), IntegrityCheck::Ok));
    }

    #[test]
    fn integrity_catches_a_modified_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_artifact(dir.path(), "app.sql.gz", b"contents", true);
        std::fs::write(&path, b"tampered").unwrap();

        match check_integrity(&path) {
            IntegrityCheck::Mismatch { expected, actual } => assert_ne!(expected, actual),
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }

    #[test]
    fn integrity_reports_a_missing_manifest_distinctly() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_artifact(dir.path(), "orphan.sql.gz", b"x", false);
        assert!(matches!(check_integrity(&path), IntegrityCheck::NoManifest));
    }

    #[test]
    fn cleanup_removes_the_manifest_alongside_the_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let old = write_artifact(dir.path(), "app_old.sql.gz", b"old", true);
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_artifact(dir.path(), "app_new.sql.gz", b"new", true);

        let plan = plan_cleanup(
            dir.path(),
            RetentionPolicy {
                keep_last: Some(1),
                max_age_days: None,
            },
        );
        assert_eq!(plan.delete.len(), 1);

        let removed = apply_cleanup(&plan);
        assert_eq!(removed.len(), 1);
        assert!(!old.exists());
        assert!(
            !BackupManifest::path_for(&old).exists(),
            "an orphaned manifest would show as a phantom library entry"
        );
        assert_eq!(list_artifacts(dir.path()).len(), 1);
    }

    #[test]
    fn cleanup_never_removes_the_only_backup() {
        let dir = tempfile::tempdir().unwrap();
        let only = write_artifact(dir.path(), "app.sql.gz", b"x", true);

        let plan = plan_cleanup(
            dir.path(),
            RetentionPolicy {
                keep_last: Some(0),
                max_age_days: Some(0),
            },
        );
        apply_cleanup(&plan);

        assert!(
            only.exists(),
            "a policy must not leave the user with nothing"
        );
    }

    // ── Analytics ───────────────────────────────────────────────────────

    fn artifact_at(database: Option<&str>, name: &str, bytes: u64, days_ago: i64) -> Artifact {
        Artifact {
            path: format!("/backups/{name}"),
            filename: name.into(),
            size_bytes: bytes,
            modified_at: Utc::now() - chrono::Duration::days(days_ago),
            database: database.map(str::to_string),
            engine: Some(Engine::Mysql),
            source_profile_name: Some("prod".into()),
            table_count: Some(10),
            tables_with_data: Some(8),
            has_manifest: database.is_some(),
        }
    }

    const MB: u64 = 1024 * 1024;

    #[test]
    fn artifacts_are_grouped_by_database_and_sorted_by_size() {
        let stats = summarise(vec![
            artifact_at(Some("small"), "small_1.sql.gz", MB, 1),
            artifact_at(Some("big"), "big_1.sql.gz", 10 * MB, 2),
            artifact_at(Some("big"), "big_2.sql.gz", 12 * MB, 1),
        ]);

        assert_eq!(stats.total_artifacts, 3);
        assert_eq!(stats.total_bytes, 23 * MB);
        assert_eq!(stats.databases.len(), 2);
        assert_eq!(
            stats.databases[0].database, "big",
            "largest first — the one filling the disk is the one to look at"
        );
        assert_eq!(stats.databases[0].artifacts, 2);
        assert_eq!(stats.databases[0].total_bytes, 22 * MB);
        assert_eq!(stats.databases[0].newest_bytes, 12 * MB);
    }

    #[test]
    fn artifacts_without_a_manifest_are_counted_not_dropped() {
        // They still take up space. Totals that silently excluded them would
        // understate what is actually on the disk.
        let stats = summarise(vec![
            artifact_at(Some("app"), "app.sql.gz", 5 * MB, 1),
            artifact_at(None, "mystery.sql.gz", 3 * MB, 1),
        ]);

        assert_eq!(stats.total_bytes, 8 * MB);
        assert_eq!(stats.unattributed, 1);
        assert_eq!(stats.unattributed_bytes, 3 * MB);
        assert_eq!(stats.databases.len(), 1);
    }

    #[test]
    fn the_series_runs_oldest_first() {
        // Both the chart and the shrink comparison depend on it: "smaller than
        // the one before" only means anything in chronological order.
        let stats = summarise(vec![
            artifact_at(Some("app"), "newest.sql.gz", 3 * MB, 1),
            artifact_at(Some("app"), "oldest.sql.gz", MB, 30),
            artifact_at(Some("app"), "middle.sql.gz", 2 * MB, 15),
        ]);

        let names: Vec<&str> = stats.databases[0]
            .series
            .iter()
            .map(|p| p.filename.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["oldest.sql.gz", "middle.sql.gz", "newest.sql.gz"]
        );
    }

    #[test]
    fn a_halved_backup_is_flagged() {
        // The failure worth catching: a table stopped being selected, a dump
        // was truncated, a WHERE filter started matching nothing.
        let stats = summarise(vec![
            artifact_at(Some("app"), "full.sql.gz", 100 * MB, 2),
            artifact_at(Some("app"), "truncated.sql.gz", 20 * MB, 1),
        ]);

        let shrinks = &stats.databases[0].shrinks;
        assert_eq!(shrinks.len(), 1);
        assert_eq!(shrinks[0].filename, "truncated.sql.gz");
        assert_eq!(shrinks[0].previous_filename, "full.sql.gz");
        assert_eq!(shrinks[0].percent_of_previous().round(), 20.0);
    }

    #[test]
    fn ordinary_shrinkage_is_not_flagged() {
        // Backups legitimately get smaller when rows are archived. Warning on
        // that is how a warning stops being read.
        let stats = summarise(vec![
            artifact_at(Some("app"), "a.sql.gz", 100 * MB, 3),
            artifact_at(Some("app"), "b.sql.gz", 90 * MB, 2),
            artifact_at(Some("app"), "c.sql.gz", 80 * MB, 1),
        ]);
        assert!(stats.databases[0].shrinks.is_empty());
    }

    #[test]
    fn tiny_artifacts_are_not_compared_at_all() {
        // A 300-byte schema-only dump next to a 200-byte one is a 33% shrink
        // and means nothing.
        let stats = summarise(vec![
            artifact_at(Some("app"), "a.sql.gz", 300, 2),
            artifact_at(Some("app"), "b.sql.gz", 100, 1),
        ]);
        assert!(
            stats.databases[0].shrinks.is_empty(),
            "below the floor there is no signal, only noise"
        );
    }

    #[test]
    fn growth_is_reported_per_day_across_the_span() {
        let stats = summarise(vec![
            artifact_at(Some("app"), "a.sql.gz", 100 * MB, 10),
            artifact_at(Some("app"), "b.sql.gz", 200 * MB, 0),
        ]);

        let per_day = stats.databases[0]
            .bytes_per_day
            .expect("two points, ten days");
        // 100 MB over 10 days.
        assert!(
            (per_day - (10 * MB) as f64).abs() < (MB as f64) * 0.1,
            "got {per_day}"
        );
    }

    #[test]
    fn a_single_artifact_reports_no_growth_rate() {
        // Inventing a rate from one point produces a number that gets quoted
        // back later as if it meant something.
        let stats = summarise(vec![artifact_at(Some("app"), "only.sql.gz", 5 * MB, 1)]);
        assert_eq!(stats.databases[0].bytes_per_day, None);
    }

    #[test]
    fn an_empty_library_summarises_to_nothing_rather_than_panicking() {
        let stats = summarise(Vec::new());
        assert_eq!(stats.total_artifacts, 0);
        assert_eq!(stats.total_bytes, 0);
        assert!(stats.databases.is_empty());
    }

    #[test]
    fn every_shrink_is_reachable_from_the_top_level_newest_first() {
        let stats = summarise(vec![
            artifact_at(Some("a"), "a1.sql.gz", 100 * MB, 4),
            artifact_at(Some("a"), "a2.sql.gz", 10 * MB, 3),
            artifact_at(Some("b"), "b1.sql.gz", 100 * MB, 2),
            artifact_at(Some("b"), "b2.sql.gz", 10 * MB, 1),
        ]);

        let all = stats.all_shrinks();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].filename, "b2.sql.gz", "newest first");
    }
}
