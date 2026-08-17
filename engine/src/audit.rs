//! A record of configuration changes.
//!
//! [`crate::job`] already records what *ran*. This records what was *changed*,
//! which is the question asked after an incident and is usually not a job at
//! all: a masking rule removed, a connection re-pointed at a different host,
//! the backup key exported, a shared bundle imported over the top.
//!
//! # No off switch
//!
//! There is deliberately no setting to disable this. A record of sensitive
//! changes that can be turned off is a record nobody can rely on, and the
//! volume is a handful of rows a week.
//!
//! # No secrets
//!
//! `detail` is free-form and subject to the same rule as everywhere else. It
//! records *that* a password was set, never what it was.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

/// One recorded change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct AuditEntry {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    /// Machine-readable verb, e.g. `profile.deleted`.
    pub action: String,
    /// What it happened to, in words a person recognises.
    pub subject: String,
    pub detail: String,
}

/// One page of recorded changes, with the size of the whole set.
///
/// The count travels with the rows for the same reason as [`crate::job::JobPage`]:
/// fetched separately it can disagree with the page it is describing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct AuditPage {
    pub entries: Vec<AuditEntry>,
    /// Total rows in `audit_log`, not the length of `entries`.
    pub total: u32,
}

/// The verbs this application records.
///
/// A closed set rather than free strings, so a call site cannot invent a verb
/// that no reader is looking for — and so the list itself documents what is
/// considered worth recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    ProfileCreated,
    ProfileUpdated,
    ProfileDeleted,
    SshConnectionCreated,
    /// One record, many databases: an edit here re-points every profile that
    /// tunnels through it, which is exactly what makes it worth recording.
    SshConnectionUpdated,
    SshConnectionDeleted,
    /// A password or passphrase was stored. Never what it was.
    SecretSet,
    MaskingChanged,
    DestinationCreated,
    DestinationUpdated,
    DestinationDeleted,
    ScheduleCreated,
    ScheduleDeleted,
    /// The backup key was written to a file. Worth knowing: from that moment
    /// the key exists somewhere this application does not control.
    BackupKeyExported,
    /// A key was adopted, so artifacts encrypted to the previous one stop
    /// being readable unless it was kept.
    BackupKeyImported,
    ConfigImported,
    /// An artifact was deleted from the library by hand.
    ArtifactDeleted,
    PipelineCreated,
    PipelineUpdated,
    PipelineDeleted,
    /// A pipeline that can drop a database was authorised to run with nobody
    /// present, or that authorisation was withdrawn. The entry a security
    /// reviewer looks for first.
    PipelineArmed,
}

impl AuditAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            AuditAction::ProfileCreated => "profile.created",
            AuditAction::ProfileUpdated => "profile.updated",
            AuditAction::ProfileDeleted => "profile.deleted",
            AuditAction::SshConnectionCreated => "ssh_connection.created",
            AuditAction::SshConnectionUpdated => "ssh_connection.updated",
            AuditAction::SshConnectionDeleted => "ssh_connection.deleted",
            AuditAction::SecretSet => "secret.set",
            AuditAction::MaskingChanged => "masking.changed",
            AuditAction::DestinationCreated => "destination.created",
            AuditAction::DestinationUpdated => "destination.updated",
            AuditAction::DestinationDeleted => "destination.deleted",
            AuditAction::ScheduleCreated => "schedule.created",
            AuditAction::ScheduleDeleted => "schedule.deleted",
            AuditAction::BackupKeyExported => "backup_key.exported",
            AuditAction::BackupKeyImported => "backup_key.imported",
            AuditAction::ConfigImported => "config.imported",
            AuditAction::ArtifactDeleted => "artifact.deleted",
            AuditAction::PipelineCreated => "pipeline.created",
            AuditAction::PipelineUpdated => "pipeline.updated",
            AuditAction::PipelineDeleted => "pipeline.deleted",
            AuditAction::PipelineArmed => "pipeline.armed",
        }
    }

    /// One line explaining why this is worth recording, for the UI.
    pub const fn why(self) -> &'static str {
        match self {
            AuditAction::ProfileUpdated => "a connection now points somewhere else than it did",
            AuditAction::ProfileDeleted => "backups for it stop happening",
            AuditAction::SshConnectionUpdated => {
                "every connection tunnelling through it now reaches somewhere else"
            }
            AuditAction::SshConnectionDeleted => "nothing can tunnel through it any more",
            AuditAction::MaskingChanged => "a column that was being masked may no longer be",
            AuditAction::BackupKeyExported => {
                "the key now exists somewhere this application does not control"
            }
            AuditAction::BackupKeyImported => {
                "artifacts encrypted to the previous key are unreadable unless it was kept"
            }
            AuditAction::ConfigImported => "connections and plans were overwritten in bulk",
            AuditAction::DestinationDeleted => "off-site copies stop being made",
            AuditAction::ScheduleDeleted => "an unattended job stops running",
            AuditAction::ArtifactDeleted => "a backup was removed by hand",
            AuditAction::PipelineUpdated => "a saved chain now does something else than it did",
            AuditAction::PipelineDeleted => "a saved chain stops being runnable",
            AuditAction::PipelineArmed => {
                "a chain that can drop a database may now run with nobody present"
            }
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_a_distinct_stored_form() {
        // The stored string is what a reader greps for; two actions sharing
        // one would make a search silently incomplete.
        let all = [
            AuditAction::ProfileCreated,
            AuditAction::ProfileUpdated,
            AuditAction::ProfileDeleted,
            AuditAction::SecretSet,
            AuditAction::MaskingChanged,
            AuditAction::DestinationCreated,
            AuditAction::DestinationUpdated,
            AuditAction::DestinationDeleted,
            AuditAction::ScheduleCreated,
            AuditAction::ScheduleDeleted,
            AuditAction::BackupKeyExported,
            AuditAction::BackupKeyImported,
            AuditAction::ConfigImported,
            AuditAction::ArtifactDeleted,
        ];
        let mut seen = std::collections::HashSet::new();
        for action in all {
            assert!(
                seen.insert(action.as_str()),
                "{} is used twice",
                action.as_str()
            );
            assert!(
                action.as_str().contains('.'),
                "{} should read as subject.verb",
                action.as_str()
            );
        }
    }

    #[test]
    fn the_consequential_actions_explain_themselves() {
        // A log line nobody understands is a log line nobody reads.
        for action in [
            AuditAction::BackupKeyExported,
            AuditAction::BackupKeyImported,
            AuditAction::MaskingChanged,
            AuditAction::ConfigImported,
            AuditAction::ProfileDeleted,
            AuditAction::SshConnectionUpdated,
        ] {
            assert!(
                !action.why().is_empty(),
                "{} needs a reason a reader can act on",
                action.as_str()
            );
        }
    }
}
