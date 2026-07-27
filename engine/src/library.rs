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
}
