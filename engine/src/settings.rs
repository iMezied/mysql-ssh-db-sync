//! Application preferences.
//!
//! In the engine rather than the desktop crate so `dbsync doctor` can report
//! the same state the app is running with — "why didn't my backup run?" is
//! usually answered by one of these three flags, and having to open the GUI to
//! read them would be exactly the wrong place to have to look.

use serde::{Deserialize, Serialize};
use specta::Type;

pub const SCHEDULER_ENABLED: &str = "scheduler_enabled";
pub const CLOSE_TO_TRAY: &str = "close_to_tray";
pub const BACKGROUND_NOTICE_SHOWN: &str = "background_notice_shown";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct AppSettings {
    /// Whether the app runs schedules itself.
    ///
    /// On by default: someone who creates a schedule in the app expects the app
    /// to run it. Turning it off is for people driving schedules from system
    /// cron who do not want two copies firing.
    pub scheduler_enabled: bool,
    /// Whether closing the window leaves the app running in the tray.
    ///
    /// On by default, because with it off a closed window silently stops every
    /// schedule — the app would appear to be configured for nightly backups
    /// that never happen.
    pub close_to_tray: bool,
    /// Launch at login. Read from the OS rather than from the store, since the
    /// user can change it outside the app.
    pub launch_at_login: bool,
    /// Whether the user has already been told that closing the window does not
    /// quit. Shown once, not every time.
    pub background_notice_shown: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            scheduler_enabled: true,
            close_to_tray: true,
            launch_at_login: false,
            background_notice_shown: false,
        }
    }
}

/// Parse a stored flag, falling back to `default` for anything unrecognised.
///
/// A corrupt preference must not stop the app from starting; the worst outcome
/// of guessing here is a toggle in the wrong position, which the user can see
/// and fix.
pub fn parse_flag(raw: Option<&str>, default: bool) -> bool {
    match raw {
        Some("true" | "1") => true,
        Some("false" | "0") => false,
        _ => default,
    }
}

pub const fn flag_str(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_schedules_running() {
        // Both of these being on is what makes "I set up a nightly backup"
        // actually produce nightly backups.
        let s = AppSettings::default();
        assert!(s.scheduler_enabled);
        assert!(s.close_to_tray);
    }

    #[test]
    fn launch_at_login_is_off_until_asked_for() {
        assert!(!AppSettings::default().launch_at_login);
    }

    #[test]
    fn flags_round_trip() {
        assert!(parse_flag(Some(flag_str(true)), false));
        assert!(!parse_flag(Some(flag_str(false)), true));
    }

    #[test]
    fn legacy_and_corrupt_values_fall_back_to_the_default() {
        assert!(parse_flag(Some("1"), false));
        assert!(!parse_flag(Some("0"), true));
        assert!(parse_flag(None, true));
        assert!(!parse_flag(None, false));
        assert!(parse_flag(Some("perhaps"), true), "must not panic or flip");
    }
}
