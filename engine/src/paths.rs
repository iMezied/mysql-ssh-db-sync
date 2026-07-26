//! Application paths.
//!
//! The GUI and the CLI must resolve the *same* store, or profiles created in
//! one are invisible to the other and the headless-parity guarantee is a
//! fiction. This module is the single source of truth; neither binary is
//! allowed to derive the path independently.

use std::path::PathBuf;

/// Bundle identifier. Must match `identifier` in `tauri.conf.json` — Tauri
/// derives its `app_data_dir()` from that value, and this module reproduces the
/// same convention for the CLI.
pub const APP_IDENTIFIER: &str = "com.dbsync-studio.app";

pub const STORE_FILENAME: &str = "dbsync.db";

#[derive(Debug, thiserror::Error)]
#[error("could not determine the user's data directory")]
pub struct PathError;

/// Directory holding application data.
///
/// Matches Tauri's `app_data_dir()`: the platform data directory joined with
/// the bundle identifier.
///
/// | Platform | Location |
/// |---|---|
/// | macOS   | `~/Library/Application Support/<identifier>` |
/// | Linux   | `$XDG_DATA_HOME/<identifier>` (or `~/.local/share/<identifier>`) |
/// | Windows | `%APPDATA%\<identifier>` |
pub fn app_data_dir() -> Result<PathBuf, PathError> {
    let base = directories::BaseDirs::new().ok_or(PathError)?;
    Ok(base.data_dir().join(APP_IDENTIFIER))
}

/// Full path to the shared SQLite store.
pub fn default_store_path() -> Result<PathBuf, PathError> {
    Ok(app_data_dir()?.join(STORE_FILENAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_path_lives_under_the_identifier_directory() {
        let dir = app_data_dir().expect("data dir");
        let store = default_store_path().expect("store path");

        assert!(
            dir.ends_with(APP_IDENTIFIER),
            "app data dir must be namespaced by the bundle identifier, got {}",
            dir.display()
        );
        assert_eq!(store.parent(), Some(dir.as_path()));
        assert_eq!(
            store.file_name().and_then(|s| s.to_str()),
            Some(STORE_FILENAME)
        );
    }

    #[test]
    fn identifier_matches_the_tauri_config() {
        // A mismatch here silently splits the GUI and CLI onto separate
        // databases, which looks like "the CLI lost my profiles".
        let config = include_str!("../../apps/desktop/src-tauri/tauri.conf.json");
        let expected = format!("\"identifier\": \"{APP_IDENTIFIER}\"");
        assert!(
            config.contains(&expected),
            "tauri.conf.json identifier must match paths::APP_IDENTIFIER ({APP_IDENTIFIER})"
        );
    }
}
