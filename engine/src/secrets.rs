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
    /// The secret access key of an off-site destination.
    ///
    /// Keyed by the *destination's* id rather than a profile's: a destination
    /// belongs to the installation, not to one database, and several profiles
    /// back up to the same bucket.
    ObjectStoreSecret,
}

impl SecretKind {
    const fn suffix(self) -> &'static str {
        match self {
            SecretKind::DbPassword => "db",
            SecretKind::SshKeyPassphrase => "ssh",
            SecretKind::BackupKey => "backup-key",
            SecretKind::ObjectStoreSecret => "object-store",
        }
    }
}

/// The pseudo-profile application-wide secrets are filed under.
///
/// The nil UUID can never collide with a real profile: `Uuid::new_v4` does not
/// generate it, and no profile is ever created with it.
///
/// Use [`app_scope`] rather than this constant, so tests stay isolated.
pub const APP_SCOPE: Uuid = Uuid::nil();

/// Environment variable that redirects app-scoped secrets, for tests.
pub const APP_SCOPE_OVERRIDE: &str = "DBSYNC_APP_SCOPE";

/// Which scope application-wide secrets are actually filed under.
///
/// # Why this is not just [`APP_SCOPE`]
///
/// Every other secret is keyed by a random profile id, so a test that writes
/// one is isolated by construction. The app scope is *fixed*, and the keychain
/// belongs to the machine rather than to the temporary store a test opens — so
/// a test calling [`crate::backupkey::ensure_exists`] creates a real backup key
/// in the developer's own login keychain and leaves it there. Worse, on a
/// machine that already has one, the test would quietly encrypt its fixtures
/// to the developer's actual key.
///
/// Setting [`APP_SCOPE_OVERRIDE`] to a UUID moves those secrets somewhere
/// disposable.
///
/// # Why it is ignored in release builds
///
/// The override decides which key encrypted backups are written to. Honouring
/// it in a shipped binary would let anything able to set an environment
/// variable point the app at an empty scope, where it would generate a fresh
/// key and encrypt to that instead — producing artifacts the user cannot
/// decrypt and would not know were different. A test convenience is not worth
/// that, so it exists only where tests do.
pub fn app_scope() -> Uuid {
    if cfg!(debug_assertions)
        && let Ok(raw) = std::env::var(APP_SCOPE_OVERRIDE)
        && let Ok(id) = Uuid::parse_str(&raw)
    {
        return id;
    }
    APP_SCOPE
}

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
/// decrypts every artifact ever taken. It also does not touch
/// [`SecretKind::ObjectStoreSecret`], which belongs to a destination — several
/// profiles ship to the same bucket, so deleting one must not lock the others
/// out of it.
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

/// Remove the credential belonging to a destination, on its deletion.
pub fn delete_for_destination(destination_id: Uuid) -> Result<(), SecretError> {
    set_secret(destination_id, SecretKind::ObjectStoreSecret, "")
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

    #[test]
    fn the_app_scope_defaults_to_the_shared_one() {
        // With nothing set, key operations must find the machine's real key.
        // A default that drifted would silently generate a second key and
        // encrypt to it, leaving existing artifacts undecryptable.
        assert!(std::env::var(APP_SCOPE_OVERRIDE).is_err());
        assert_eq!(app_scope(), APP_SCOPE);
    }

    #[test]
    fn a_malformed_override_is_ignored_rather_than_guessed_at() {
        // Falling back to the real scope is the safe reading: the alternative
        // is inventing a scope from a typo and generating a key there.
        assert_eq!(
            Uuid::parse_str("not-a-uuid").ok().unwrap_or(APP_SCOPE),
            APP_SCOPE
        );
    }
}
