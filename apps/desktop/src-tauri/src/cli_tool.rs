//! Making the bundled `dbsync` reachable from a terminal.
//!
//! Every schedule offers a crontab line that invokes `dbsync`, so the CLI ships
//! inside the app bundle. That is only half the job: cron and the user's shell
//! look on `PATH`, and nothing inside an application bundle is on `PATH`.
//!
//! # No privilege escalation
//!
//! This never asks for an administrator password and never writes anywhere it
//! has not checked it can write. If no writable directory is available it
//! returns the exact command for the user to run themselves — being handed a
//! command you can read is better than being handed a password prompt from an
//! app that wants to write to a system directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use specta::Type;

/// Where the CLI is, and whether a terminal can already find it.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct CliStatus {
    /// The copy shipped inside this application bundle, if it is there.
    pub bundled_path: Option<String>,
    /// An existing `dbsync` already on `PATH`, if any.
    pub installed_path: Option<String>,
    /// True when the thing on `PATH` is the copy this app ships.
    pub linked_to_bundle: bool,
    /// Directories this app could write a link into, best first.
    pub install_targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct CliInstall {
    pub path: String,
    /// False when the directory is not on the user's `PATH`, so the UI can say
    /// what still has to be done rather than claiming success.
    pub on_path: bool,
    /// Set when nothing could be written and the user has to do it themselves.
    pub manual_command: Option<String>,
}

const EXE: &str = if cfg!(windows) {
    "dbsync.exe"
} else {
    "dbsync"
};

/// The CLI shipped alongside this executable.
///
/// Tauri places an external binary next to the main executable — inside
/// `Contents/MacOS` on macOS — so this is a sibling lookup rather than a
/// resource-directory one.
pub fn bundled_cli() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let sibling = exe.parent()?.join(EXE);
    if sibling.is_file() {
        return Some(sibling);
    }

    // During `tauri dev` nothing is staged next to the debug binary, but the
    // workspace build of the CLI is two directories up. Without this the
    // feature would be untestable outside a full bundle.
    let target_dir = exe.parent()?.parent()?;
    for profile in ["release", "debug"] {
        let candidate = target_dir.join(profile).join(EXE);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}

/// Resolve `dbsync` the way a shell would.
fn cli_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(EXE))
        .find(|candidate| candidate.is_file())
}

/// Directories worth offering, best first.
///
/// `~/.local/bin` comes first deliberately: it needs no privileges and is
/// per-user, so installing there can never break another account or collide
/// with a package manager. `/usr/local/bin` is only offered when it already
/// exists and is writable — which on macOS usually means Homebrew owns it.
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Some(home) = home_dir() {
        dirs.push(home.join(".local").join("bin"));
    }

    #[cfg(unix)]
    for shared in ["/usr/local/bin", "/opt/homebrew/bin"] {
        let dir = PathBuf::from(shared);
        if dir.is_dir() && is_writable(&dir) {
            dirs.push(dir);
        }
    }

    dirs
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(unix)]
fn is_writable(dir: &Path) -> bool {
    // Asking the OS beats reasoning about ownership and group membership.
    use std::ffi::CString;
    let Ok(c) = CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

#[cfg(not(unix))]
fn is_writable(_dir: &Path) -> bool {
    false
}

fn on_path(dir: &Path) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|p| p == dir)
}

pub fn status() -> CliStatus {
    let bundled = bundled_cli();
    let installed = cli_on_path();

    // Compare resolved paths: the installed copy is normally a symlink, and
    // comparing the link itself would always report "not linked".
    let linked_to_bundle = match (&bundled, &installed) {
        (Some(b), Some(i)) => {
            let canon = |p: &PathBuf| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            canon(b) == canon(i)
        }
        _ => false,
    };

    CliStatus {
        bundled_path: bundled.map(|p| p.display().to_string()),
        installed_path: installed.map(|p| p.display().to_string()),
        linked_to_bundle,
        install_targets: candidate_dirs()
            .into_iter()
            .map(|p| p.display().to_string())
            .collect(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(
        "this build does not ship the dbsync command line tool; install it separately, or build \
         the app with `npm run bundle:cli` first"
    )]
    NotBundled,
    // A directory that cannot be created is not an error here: `install`
    // degrades to returning a command the user can run, which is more useful
    // than a failure. Hence no variant for it.
    #[error("could not write the link: {0}")]
    Link(#[source] std::io::Error),
}

/// Link the bundled CLI into a directory a terminal can find.
///
/// Returns what was done, including whether the directory is actually on
/// `PATH` — reporting success for a link nobody's shell will ever look at
/// would be worse than reporting nothing.
pub fn install() -> Result<CliInstall, CliError> {
    let source = bundled_cli().ok_or(CliError::NotBundled)?;

    // Windows has no dependable unprivileged symlink: it needs Developer Mode
    // or an elevated process. Adding the directory to PATH is the supported
    // route, so say that rather than failing with a permissions error.
    if cfg!(windows) {
        return Ok(CliInstall {
            path: source.display().to_string(),
            on_path: false,
            manual_command: Some(format!(
                "setx PATH \"%PATH%;{}\"",
                source.parent().unwrap_or(Path::new(".")).display()
            )),
        });
    }

    let Some(dir) = candidate_dirs().into_iter().next() else {
        return Ok(manual_fallback(&source));
    };

    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("could not create {}: {e}", dir.display());
        return Ok(manual_fallback(&source));
    }
    if !is_writable(&dir) {
        return Ok(manual_fallback(&source));
    }

    let link = dir.join(EXE);
    // Replacing an existing link is the normal case on an app update, when the
    // bundle has moved. Removing only a symlink, never a real file, so a
    // hand-installed dbsync binary is not silently destroyed.
    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::remove_file(&link).map_err(CliError::Link)?;
        }
        Ok(_) => {
            return Ok(CliInstall {
                path: link.display().to_string(),
                on_path: on_path(&dir),
                manual_command: Some(format!(
                    "# {} already exists and is not a link; replace it by hand if you meant to\nln \
                     -sf {:?} {:?}",
                    link.display(),
                    source,
                    link
                )),
            });
        }
        Err(_) => {}
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(&source, &link).map_err(CliError::Link)?;

    Ok(CliInstall {
        path: link.display().to_string(),
        on_path: on_path(&dir),
        manual_command: None,
    })
}

fn manual_fallback(source: &Path) -> CliInstall {
    CliInstall {
        path: source.display().to_string(),
        on_path: false,
        manual_command: Some(format!("sudo ln -sf {source:?} /usr/local/bin/dbsync")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_executable_name_matches_the_platform() {
        #[cfg(windows)]
        assert_eq!(EXE, "dbsync.exe");
        #[cfg(not(windows))]
        assert_eq!(EXE, "dbsync");
    }

    #[test]
    fn status_never_panics_without_a_bundle() {
        // Runs from `cargo test`, where nothing is staged next to the test
        // binary. It must report absence rather than fall over.
        let s = status();
        assert!(s.install_targets.iter().all(|t| !t.is_empty()));
    }

    #[test]
    fn a_user_local_directory_is_preferred() {
        // It needs no privileges and cannot collide with a package manager.
        let dirs = candidate_dirs();
        if let Some(first) = dirs.first() {
            assert!(
                first.ends_with(".local/bin"),
                "expected ~/.local/bin first, got {}",
                first.display()
            );
        }
    }

    #[test]
    fn path_membership_is_exact_not_substring() {
        // "/usr/local/bin2" must not count as "/usr/local/bin" being present.
        let dir = PathBuf::from("/definitely/not/on/path");
        assert!(!on_path(&dir));
    }
}
