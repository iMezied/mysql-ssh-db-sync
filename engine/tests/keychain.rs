//! Real OS keychain round-trip.
//!
//! Ignored by default: CI runners have no unlocked keychain / Secret Service,
//! and a failure there would say nothing about the code. Run locally with:
//!
//!     cargo test -p db-sync-engine --test keychain -- --ignored
//!
//! The unit tests in `secrets` cover account namespacing without touching the
//! OS; these cover the part that can only be verified against the real backend.

use db_sync_engine::secrets::{
    SecretKind, delete_all_for_profile, get_secret, has_secret, set_secret,
};
use secrecy::ExposeSecret;
use uuid::Uuid;

/// Always clean up, even on assertion failure, so a failed run does not leave
/// entries behind in the developer's keychain.
struct Cleanup(Uuid);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = delete_all_for_profile(self.0);
    }
}

#[test]
#[ignore = "requires an unlocked OS keychain"]
fn secret_round_trips_through_the_os_keychain() {
    let id = Uuid::new_v4();
    let _cleanup = Cleanup(id);

    assert!(!has_secret(id, SecretKind::DbPassword).unwrap());
    assert!(get_secret(id, SecretKind::DbPassword).unwrap().is_none());

    set_secret(id, SecretKind::DbPassword, "s3cr3t-p@ss word").unwrap();

    assert!(has_secret(id, SecretKind::DbPassword).unwrap());
    let fetched = get_secret(id, SecretKind::DbPassword).unwrap().unwrap();
    assert_eq!(fetched.expose_secret(), "s3cr3t-p@ss word");
}

#[test]
#[ignore = "requires an unlocked OS keychain"]
fn secret_kinds_do_not_collide() {
    let id = Uuid::new_v4();
    let _cleanup = Cleanup(id);

    set_secret(id, SecretKind::DbPassword, "db-value").unwrap();
    set_secret(id, SecretKind::SshKeyPassphrase, "ssh-value").unwrap();

    assert_eq!(
        get_secret(id, SecretKind::DbPassword)
            .unwrap()
            .unwrap()
            .expose_secret(),
        "db-value"
    );
    assert_eq!(
        get_secret(id, SecretKind::SshKeyPassphrase)
            .unwrap()
            .unwrap()
            .expose_secret(),
        "ssh-value"
    );
}

#[test]
#[ignore = "requires an unlocked OS keychain"]
fn setting_an_empty_value_clears_the_secret() {
    let id = Uuid::new_v4();
    let _cleanup = Cleanup(id);

    set_secret(id, SecretKind::DbPassword, "temporary").unwrap();
    assert!(has_secret(id, SecretKind::DbPassword).unwrap());

    set_secret(id, SecretKind::DbPassword, "").unwrap();
    assert!(!has_secret(id, SecretKind::DbPassword).unwrap());

    // Clearing an already-absent secret must not error.
    set_secret(id, SecretKind::DbPassword, "").unwrap();
}

#[test]
#[ignore = "requires an unlocked OS keychain"]
fn overwriting_replaces_the_previous_value() {
    let id = Uuid::new_v4();
    let _cleanup = Cleanup(id);

    set_secret(id, SecretKind::DbPassword, "first").unwrap();
    set_secret(id, SecretKind::DbPassword, "second").unwrap();

    assert_eq!(
        get_secret(id, SecretKind::DbPassword)
            .unwrap()
            .unwrap()
            .expose_secret(),
        "second"
    );
}

#[test]
#[ignore = "requires an unlocked OS keychain"]
fn deleting_a_profile_purges_every_secret_it_owns() {
    let id = Uuid::new_v4();
    let _cleanup = Cleanup(id);

    set_secret(id, SecretKind::DbPassword, "db").unwrap();
    set_secret(id, SecretKind::SshKeyPassphrase, "ssh").unwrap();

    delete_all_for_profile(id).unwrap();

    assert!(!has_secret(id, SecretKind::DbPassword).unwrap());
    assert!(!has_secret(id, SecretKind::SshKeyPassphrase).unwrap());

    // Purging twice must be safe — profile deletion can be retried.
    delete_all_for_profile(id).unwrap();
}

#[test]
#[ignore = "requires an unlocked OS keychain"]
fn profiles_do_not_read_each_others_secrets() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let _ca = Cleanup(a);
    let _cb = Cleanup(b);

    set_secret(a, SecretKind::DbPassword, "a-secret").unwrap();

    assert!(!has_secret(b, SecretKind::DbPassword).unwrap());
    assert_eq!(
        get_secret(a, SecretKind::DbPassword)
            .unwrap()
            .unwrap()
            .expose_secret(),
        "a-secret"
    );
}
