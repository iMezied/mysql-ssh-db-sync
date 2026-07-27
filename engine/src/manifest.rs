//! Backup manifests.
//!
//! Every artifact is written alongside a `<artifact>.manifest.json` describing
//! how it was produced. Restores read this to pick the right tool and to detect
//! corruption before touching a destination server.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use specta::Type;
use uuid::Uuid;

use crate::types::Engine;

/// Bump when the manifest layout changes incompatibly.
pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactFormat {
    /// Single gzipped SQL stream (MySQL, and PostgreSQL `plain` format).
    SqlGz,
    /// `pg_dump -Fc` custom archive. Supports selective and parallel restore.
    PgCustom,
    /// `pg_dump -Fd` directory archive. Supports parallel dump and restore.
    PgDirectory,
    /// `mydumper` output directory.
    MydumperDir,
}

impl ArtifactFormat {
    /// Whether `pg_restore`-style per-table selection is possible.
    pub const fn supports_selective_restore(self) -> bool {
        matches!(self, ArtifactFormat::PgCustom | ArtifactFormat::PgDirectory)
    }

    /// Whether the artifact is a directory rather than a single file.
    pub const fn is_directory(self) -> bool {
        matches!(
            self,
            ArtifactFormat::PgDirectory | ArtifactFormat::MydumperDir
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct BackupManifest {
    pub manifest_version: u32,
    pub id: Uuid,
    pub source_profile_id: Uuid,
    pub source_profile_name: String,
    pub engine: Engine,
    pub server_version: String,
    pub dump_tool: String,
    pub dump_tool_version: String,
    pub database: String,
    pub created_at: DateTime<Utc>,
    pub format: ArtifactFormat,
    /// Every table present in the artifact, including schema-only ones.
    pub tables: Vec<String>,
    /// Subset of `tables` that was dumped **with its rows**.
    ///
    /// This records what the job was asked to include, not what turned out to
    /// be there. A table selected for data that happens to hold zero rows
    /// appears here, and nothing in the artifact distinguishes it from one that
    /// should have had rows and did not — see
    /// [`crate::ops::drill`], which is careful about exactly that.
    pub tables_with_data: Vec<String>,
    /// Options the job ran with, kept opaque so shapes can evolve freely.
    pub options: serde_json::Value,
    pub artifact_filename: String,
    /// Size of the artifact on disk. See the note in `events::ProgressEvent`
    /// for why byte counts export as `number`.
    #[specta(type = f64)]
    pub size_bytes: u64,
    /// SHA-256 of the artifact, used to detect corruption before a restore.
    pub sha256: String,
    pub encrypted: bool,
    /// Public keys this artifact was encrypted to.
    ///
    /// Recorded so a restore that cannot decrypt can say *which* key is needed
    /// rather than only that it failed. Defaulted so manifests written before
    /// encryption existed still parse.
    #[serde(default)]
    pub encryption_recipients: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest not found: {0}")]
    NotFound(PathBuf),
    #[error("manifest is unreadable: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest is malformed: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error(
        "manifest version {found} is newer than supported version {supported}; upgrade DBSync Studio"
    )]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("artifact checksum mismatch: manifest says {expected}, file is {actual}")]
    ChecksumMismatch { expected: String, actual: String },
}

impl BackupManifest {
    /// Manifest path for a given artifact path.
    pub fn path_for(artifact_path: impl AsRef<Path>) -> PathBuf {
        let p = artifact_path.as_ref();
        let mut name = p.file_name().unwrap_or_default().to_os_string();
        name.push(".manifest.json");
        p.with_file_name(name)
    }

    pub fn write(&self, artifact_path: impl AsRef<Path>) -> Result<PathBuf, ManifestError> {
        let path = Self::path_for(artifact_path);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(path)
    }

    /// Read and validate the manifest sitting next to an artifact.
    ///
    /// This does not hash the artifact — call [`verify_artifact`] for that,
    /// which is potentially expensive on large files.
    pub fn read(artifact_path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let path = Self::path_for(&artifact_path);
        if !path.exists() {
            return Err(ManifestError::NotFound(path));
        }
        let raw = std::fs::read_to_string(&path)?;
        let manifest: BackupManifest = serde_json::from_str(&raw)?;
        if manifest.manifest_version > MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion {
                found: manifest.manifest_version,
                supported: MANIFEST_VERSION,
            });
        }
        Ok(manifest)
    }

    /// Hash the artifact and compare against the recorded checksum.
    pub fn verify_artifact(&self, artifact_path: impl AsRef<Path>) -> Result<(), ManifestError> {
        let actual = sha256_file(artifact_path)?;
        if actual != self.sha256 {
            return Err(ManifestError::ChecksumMismatch {
                expected: self.sha256.clone(),
                actual,
            });
        }
        Ok(())
    }
}

/// Streaming SHA-256 so multi-gigabyte artifacts never land in memory.
pub fn sha256_file(path: impl AsRef<Path>) -> Result<String, std::io::Error> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(sha: &str) -> BackupManifest {
        BackupManifest {
            manifest_version: MANIFEST_VERSION,
            id: Uuid::new_v4(),
            source_profile_id: Uuid::new_v4(),
            source_profile_name: "prod-de".into(),
            engine: Engine::Mysql,
            server_version: "8.0.42".into(),
            dump_tool: "mysqldump".into(),
            dump_tool_version: "8.0.42".into(),
            database: "app".into(),
            created_at: Utc::now(),
            format: ArtifactFormat::SqlGz,
            tables: vec!["users".into(), "orders".into()],
            tables_with_data: vec!["orders".into()],
            options: serde_json::json!({"single_transaction": true}),
            artifact_filename: "app_20260101_000000.sql.gz".into(),
            size_bytes: 3,
            sha256: sha.into(),
            encrypted: false,
            encryption_recipients: Vec::new(),
        }
    }

    #[test]
    fn manifest_path_appends_suffix_without_losing_extension() {
        let p = BackupManifest::path_for("/backups/app_2026.sql.gz");
        assert_eq!(
            p,
            PathBuf::from("/backups/app_2026.sql.gz.manifest.json"),
            "restores locate the manifest by exact artifact name"
        );
    }

    #[test]
    fn write_then_read_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("app.sql.gz");
        std::fs::write(&artifact, b"abc").unwrap();

        let m = sample("placeholder");
        m.write(&artifact).unwrap();

        let read = BackupManifest::read(&artifact).unwrap();
        assert_eq!(read.database, "app");
        assert_eq!(read.tables_with_data, vec!["orders".to_string()]);
    }

    #[test]
    fn missing_manifest_is_reported_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("nope.sql.gz");
        assert!(matches!(
            BackupManifest::read(&artifact),
            Err(ManifestError::NotFound(_))
        ));
    }

    #[test]
    fn future_manifest_versions_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("app.sql.gz");
        std::fs::write(&artifact, b"abc").unwrap();

        let mut m = sample("x");
        m.manifest_version = MANIFEST_VERSION + 1;
        m.write(&artifact).unwrap();

        assert!(matches!(
            BackupManifest::read(&artifact),
            Err(ManifestError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn checksum_mismatch_is_detected_before_restore() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("app.sql.gz");
        std::fs::write(&artifact, b"abc").unwrap();

        let good = sha256_file(&artifact).unwrap();
        sample(&good).verify_artifact(&artifact).unwrap();

        // Simulate a truncated/corrupted download.
        std::fs::write(&artifact, b"abd").unwrap();
        assert!(matches!(
            sample(&good).verify_artifact(&artifact),
            Err(ManifestError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn sha256_matches_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("abc.txt");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn only_pg_archive_formats_support_selective_restore() {
        assert!(ArtifactFormat::PgCustom.supports_selective_restore());
        assert!(ArtifactFormat::PgDirectory.supports_selective_restore());
        assert!(!ArtifactFormat::SqlGz.supports_selective_restore());
    }
}
