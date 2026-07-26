//! The installation's backup key: generation, escrow and recipients.
//!
//! Sits between [`crypto`](crate::crypto) (which knows nothing about where keys
//! live) and the store (which must never hold a secret). The secret half is in
//! the OS keychain; the public half and the escrow flag are ordinary settings.
//!
//! # Why escrow is enforced, not suggested
//!
//! The keychain is not a backup of itself. Reinstall the OS, lose the laptop,
//! or reset the login keychain, and every encrypted artifact ever taken becomes
//! permanently unreadable — while continuing to pass every integrity check it
//! has. That failure is discovered on the one day it matters.
//!
//! So [`ensure_ready_for_encryption`] refuses to let an encrypted backup start
//! until the key has been exported at least once. It is the one place in this
//! application where a nag is the correct design.

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::crypto::{self, CryptoError};
use crate::secrets::{self, SecretKind, app_scope};
use crate::settings;
use crate::store::Store;

/// Settings keys.
pub const PUBLIC_KEY: &str = "backup_key_public";
pub const KEY_EXPORTED: &str = "backup_key_exported";
/// Extra `age1...` recipients, newline separated.
pub const EXTRA_RECIPIENTS: &str = "backup_key_extra_recipients";

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Secrets(#[from] secrets::SecretError),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error(
        "the backup key has not been exported yet. Export it and store it somewhere safe before \
         taking an encrypted backup — if this machine's keychain is lost, an artifact encrypted \
         to a key you do not have a copy of can never be read again"
    )]
    NotExported,
    #[error(
        "no backup key exists on this machine, but this artifact is encrypted. Import the key it \
         was encrypted to, or restore from an unencrypted backup"
    )]
    Missing,
    #[error(
        "the stored public key does not match the key in the keychain. One of them was replaced \
         out from under the other; artifacts encrypted to {stored} need that key back"
    )]
    Mismatch { stored: String },
}

/// What the UI is allowed to know about the backup key.
///
/// The secret half never appears here — [`export`] is the only way out, and it
/// is a deliberate, separate action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct KeyStatus {
    pub exists: bool,
    /// `age1...`, safe to display.
    pub public: Option<String>,
    pub exported: bool,
    pub extra_recipients: Vec<String>,
}

/// Read the current state without creating anything.
pub async fn status(store: &Store) -> Result<KeyStatus, KeyError> {
    let public = store.get_setting(PUBLIC_KEY).await?;
    let exported = settings::parse_flag(store.get_setting(KEY_EXPORTED).await?.as_deref(), false);

    Ok(KeyStatus {
        exists: public.is_some() && secrets::has_secret(app_scope(), SecretKind::BackupKey)?,
        public,
        exported,
        extra_recipients: extra_recipients(store).await?,
    })
}

async fn extra_recipients(store: &Store) -> Result<Vec<String>, KeyError> {
    Ok(store
        .get_setting(EXTRA_RECIPIENTS)
        .await?
        .map(|raw| {
            raw.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// Create a key if none exists, returning the current status.
///
/// Idempotent: an existing key is never replaced, because replacing it would
/// orphan every artifact encrypted to the old one.
pub async fn ensure_exists(store: &Store) -> Result<KeyStatus, KeyError> {
    let current = status(store).await?;
    if current.exists {
        return Ok(current);
    }

    let (secret, public) = crypto::generate_identity();
    secrets::set_secret(app_scope(), SecretKind::BackupKey, secret.expose_secret())?;
    store.set_setting(PUBLIC_KEY, &public).await?;
    store.set_flag(KEY_EXPORTED, false).await?;

    tracing::info!("generated a backup encryption key: {public}");
    status(store).await
}

/// The secret half, for the user to copy somewhere safe.
///
/// Marks the key as exported, which is what unblocks encrypted backups.
pub async fn export(store: &Store) -> Result<SecretString, KeyError> {
    let secret =
        secrets::get_secret(app_scope(), SecretKind::BackupKey)?.ok_or(KeyError::Missing)?;

    // Checked on the way out rather than trusted: if the two halves ever drift
    // apart, the manifest would record a recipient nothing can decrypt.
    let derived = crypto::public_from_identity(&secret)?;
    if let Some(stored) = store.get_setting(PUBLIC_KEY).await?
        && stored != derived
    {
        return Err(KeyError::Mismatch { stored });
    }

    store.set_flag(KEY_EXPORTED, true).await?;
    Ok(secret)
}

/// Adopt an existing identity, replacing whatever is here.
///
/// This is how a second machine reads the first machine's artifacts, and how a
/// key is recovered from escrow.
pub async fn import(store: &Store, secret: &str) -> Result<KeyStatus, KeyError> {
    let secret = SecretString::from(secret.trim().to_owned());
    // Validated before anything is written, so a typo cannot leave the keychain
    // and the store disagreeing.
    let public = crypto::public_from_identity(&secret)?;

    secrets::set_secret(app_scope(), SecretKind::BackupKey, secret.expose_secret())?;
    store.set_setting(PUBLIC_KEY, &public).await?;
    // An imported key is by definition already held somewhere else.
    store.set_flag(KEY_EXPORTED, true).await?;

    status(store).await
}

/// Replace the list of additional recipients.
///
/// Every entry is parsed first: a malformed key here would fail at 03:00, and
/// only for the schedules that happen to encrypt.
pub async fn set_extra_recipients(store: &Store, keys: &[String]) -> Result<(), KeyError> {
    for key in keys {
        crypto::parse_recipient(key)?;
    }
    store
        .set_setting(EXTRA_RECIPIENTS, &keys.join("\n"))
        .await?;
    Ok(())
}

/// Every recipient an artifact should be encrypted to.
///
/// The installation's own key always comes first, so a backup is always
/// readable by the machine that made it even if the extra recipients are wrong.
pub async fn recipients(store: &Store) -> Result<Vec<String>, KeyError> {
    let status = status(store).await?;
    let own = status.public.ok_or(KeyError::Missing)?;

    let mut all = vec![own];
    all.extend(status.extra_recipients);
    Ok(all)
}

/// Gate an encrypted backup on the key being generated and escrowed.
pub async fn ensure_ready_for_encryption(store: &Store) -> Result<Vec<String>, KeyError> {
    let status = ensure_exists(store).await?;
    if !status.exported {
        return Err(KeyError::NotExported);
    }
    recipients(store).await
}

/// The identity needed to read an encrypted artifact.
pub fn identity() -> Result<SecretString, KeyError> {
    secrets::get_secret(app_scope(), SecretKind::BackupKey)?.ok_or(KeyError::Missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn a_fresh_install_has_no_key() {
        let (store, _d) = store().await;
        let s = status(&store).await.unwrap();
        assert!(!s.exists);
        assert!(s.public.is_none());
        assert!(!s.exported);
    }

    #[tokio::test]
    async fn recipients_are_refused_before_a_key_exists() {
        // Encrypting to nothing must never silently produce plaintext.
        let (store, _d) = store().await;
        assert!(matches!(recipients(&store).await, Err(KeyError::Missing)));
    }

    #[tokio::test]
    async fn extra_recipients_are_validated_before_being_stored() {
        let (store, _d) = store().await;
        assert!(
            set_extra_recipients(&store, &["not-a-key".into()])
                .await
                .is_err()
        );
        assert!(
            store.get_setting(EXTRA_RECIPIENTS).await.unwrap().is_none(),
            "nothing may be written when validation fails"
        );
    }

    #[tokio::test]
    async fn an_empty_recipient_list_is_allowed() {
        // Clearing the team keys is legitimate; the installation's own key is
        // added separately and is what keeps the artifact readable.
        let (store, _d) = store().await;
        assert!(set_extra_recipients(&store, &[]).await.is_ok());
    }
}
