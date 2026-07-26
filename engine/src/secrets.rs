//! OS keychain access.
//!
//! SECURITY INVARIANT: secrets resolved here must never be returned across the
//! Tauri command boundary into the webview, and must never be passed to a child
//! process as an argv element. The engine reads them internally and hands them
//! to child processes via environment variables or 0600 temp credential files.
//!
//! There is deliberately no `get_*` command exposed by the desktop app.

use secrecy::SecretString;
use uuid::Uuid;

pub const KEYRING_SERVICE: &str = "com.dbsync-studio.credentials";

/// Which secret belonging to a profile is being addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    /// The database user's password.
    DbPassword,
    /// Passphrase protecting the SSH private key file.
    SshKeyPassphrase,
    /// The installation's age identity for encrypting backup artifacts.
    ///
    /// Application-scoped rather than per-profile: one key encrypts every
    /// artifact, so a restore does not depend on which profile made the backup.
    /// Stored under [`APP_SCOPE`] for that reason.
    BackupKey,
}

impl SecretKind {
    const fn suffix(self) -> &'static str {
        match self {
            SecretKind::DbPassword => "db",
            SecretKind::SshKeyPassphrase => "ssh",
            SecretKind::BackupKey => "backup-key",
        }
    }
}

/// The pseudo-profile application-wide secrets are filed under.
///
/// The nil UUID can never collide with a real profile: `Uuid::new_v4` does not
/// generate it, and no profile is ever created with it.
pub const APP_SCOPE: Uuid = Uuid::nil();

fn account(profile_id: Uuid, kind: SecretKind) -> String {
    format!("{}#{}", profile_id, kind.suffix())
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("keychain unavailable: {0}")]
    Backend(#[from] keyring::Error),
}

/// Store (or replace) a secret. Passing an empty string deletes the entry.
pub fn set_secret(profile_id: Uuid, kind: SecretKind, value: &str) -> Result<(), SecretError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &account(profile_id, kind))?;
    if value.is_empty() {
        return match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        };
    }
    entry.set_password(value)?;
    Ok(())
}

/// Fetch a secret. Returns `None` when no entry exists.
pub fn get_secret(profile_id: Uuid, kind: SecretKind) -> Result<Option<SecretString>, SecretError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &account(profile_id, kind))?;
    match entry.get_password() {
        Ok(v) => Ok(Some(SecretString::from(v))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Whether a secret exists, without materialising it.
///
/// This is what the UI is allowed to know — "a password is set" — as opposed to
/// the password itself.
pub fn has_secret(profile_id: Uuid, kind: SecretKind) -> Result<bool, SecretError> {
    Ok(get_secret(profile_id, kind)?.is_some())
}

/// Remove every secret belonging to a profile. Called on profile deletion.
///
/// Deliberately does NOT touch [`SecretKind::BackupKey`]: that is
/// application-scoped, and deleting a profile must never destroy the key that
/// decrypts every artifact ever taken.
pub fn delete_all_for_profile(profile_id: Uuid) -> Result<(), SecretError> {
    for kind in [SecretKind::DbPassword, SecretKind::SshKeyPassphrase] {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account(profile_id, kind))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_are_namespaced_per_kind() {
        let id = Uuid::nil();
        assert_ne!(
            account(id, SecretKind::DbPassword),
            account(id, SecretKind::SshKeyPassphrase)
        );
    }

    #[test]
    fn accounts_are_namespaced_per_profile() {
        let kind = SecretKind::DbPassword;
        assert_ne!(account(Uuid::new_v4(), kind), account(Uuid::new_v4(), kind));
    }
}
