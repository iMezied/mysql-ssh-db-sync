//! A named chain of actions, saved once and run on demand.
//!
//! [`crate::ops::sync`] already runs backup-then-restore as one job, but its
//! shape is fixed and nothing about it is saved: the wizard is rebuilt from
//! scratch every time, and it deliberately refuses a destructive target. The
//! recurring real task it does not cover is "back up production and put it on
//! staging, replacing what is there" — which is a chain somebody wants to name
//! and press a button on, not reassemble.
//!
//! A pipeline is that chain: an ordered list of steps, validated as a whole,
//! executed as one cancellable job with one history record. It composes the
//! existing operations rather than reimplementing any of them — the same rule
//! `ops::sync` follows, and for the same reason.
//!
//! # Data flows down the list
//!
//! A [`PipelineStep::Restore`] consumes what the most recent
//! [`PipelineStep::Backup`] produced. There is no wiring between steps to draw
//! or to get wrong, and it matches how a person describes the thing out loud.
//! [`ArtifactSource`] exists for the other case — restoring a file no step here
//! created — and is explicit precisely because it is the exception.
//!
//! # Destructive steps and unattended runs
//!
//! A restore that replaces a database needs its name typed back. The engine
//! enforces that in [`crate::restore::RestoreRequest::validate`] and this module
//! does not weaken it: a manual run supplies the confirmation at run time, and
//! an unattended run supplies one captured earlier by a human.
//!
//! What makes that safe is [`Pipeline::destructive_signature`]. Arming stores
//! the signature of the destructive targets as they were when somebody typed
//! them. Editing a target changes the signature, the stored acknowledgment
//! stops matching, and the pipeline is no longer armed. Renaming the target of
//! a nightly unattended replace therefore disarms it rather than silently
//! re-aiming it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

use crate::backup::{EngineBackupOptions, TableSelection};
use crate::mask::MaskRule;
use crate::profile::ConnectionProfile;
use crate::restore::{EngineRestoreOptions, TargetNaming};
use crate::retention::RetentionPolicy;
use crate::step::JobStepKind;
use crate::types::Engine;

const fn yes() -> bool {
    true
}

/// Where a restore step gets the artifact it replays.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "from")]
pub enum ArtifactSource {
    /// Whatever the most recent backup step in this pipeline wrote.
    #[default]
    PreviousStep,
    /// The newest artifact in a directory, the way a drill picks one.
    NewestInDirectory { dir: PathBuf },
    /// One named file.
    Path { path: PathBuf },
}

/// One action in a pipeline.
///
/// Internally tagged, so a step gains a field without invalidating the stored
/// JSON of every pipeline written before it. Every added field is
/// `#[serde(default)]` for the same reason, and because an unattended run must
/// not acquire a new cost on upgrade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PipelineStep {
    Backup {
        profile_id: Uuid,
        database: String,
        /// A saved table set. When set, its selections are used and
        /// `selections` is ignored — the set is the thing being maintained.
        #[serde(default)]
        plan_id: Option<Uuid>,
        #[serde(default)]
        selections: Vec<TableSelection>,
        /// Defaults to the app's backup directory when absent.
        #[serde(default)]
        output_dir: Option<PathBuf>,
        #[serde(default = "yes")]
        compress: bool,
        #[serde(default)]
        encrypt: bool,
        #[serde(default)]
        record_row_counts: bool,
        engine: EngineBackupOptions,
    },
    Restore {
        profile_id: Uuid,
        #[serde(default)]
        source: ArtifactSource,
        naming: TargetNaming,
        engine: EngineRestoreOptions,
        #[serde(default = "yes")]
        verify_checksum: bool,
    },
    /// Compare the restored database against the source it came from.
    Verify {
        #[serde(default)]
        deep: bool,
    },
    /// Mask columns on the database the previous restore wrote.
    Mask {
        #[serde(default)]
        rules: Vec<MaskRule>,
    },
    /// Copy the artifact to every enabled off-site destination.
    PushOffsite,
    /// Prune the backup directory the artifact was written to.
    Retention { policy: RetentionPolicy },
    /// Prove the newest artifact restores, into a scratch database that is
    /// dropped afterwards.
    Drill {
        profile_id: Uuid,
        #[serde(default)]
        artifact_dir: Option<PathBuf>,
        #[serde(default)]
        deep: bool,
        #[serde(default)]
        keep_on_failure: bool,
    },
}

impl PipelineStep {
    pub const fn kind(&self) -> JobStepKind {
        match self {
            PipelineStep::Backup { .. } => JobStepKind::Backup,
            PipelineStep::Restore { .. } => JobStepKind::Restore,
            PipelineStep::Verify { .. } => JobStepKind::Verify,
            PipelineStep::Mask { .. } => JobStepKind::Mask,
            PipelineStep::PushOffsite => JobStepKind::Offsite,
            PipelineStep::Retention { .. } => JobStepKind::Retention,
            PipelineStep::Drill { .. } => JobStepKind::Drill,
        }
    }

    /// Whether running this step can destroy data that was already there.
    pub fn is_destructive(&self) -> bool {
        match self {
            PipelineStep::Restore { naming, .. } => naming.is_destructive(),
            _ => false,
        }
    }

    /// The connection this step acts on, when it names one of its own.
    pub const fn profile_id(&self) -> Option<Uuid> {
        match self {
            PipelineStep::Backup { profile_id, .. }
            | PipelineStep::Restore { profile_id, .. }
            | PipelineStep::Drill { profile_id, .. } => Some(*profile_id),
            _ => None,
        }
    }

    /// A sentence saying what this step will do, for the plan and the run.
    ///
    /// Names the actual database or connection rather than repeating the kind,
    /// because the kind is already shown beside it.
    pub fn label(&self, profiles: &[ConnectionProfile]) -> String {
        let name_of = |id: &Uuid| {
            profiles
                .iter()
                .find(|p| p.id == *id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "a deleted connection".to_string())
        };

        match self {
            PipelineStep::Backup {
                profile_id,
                database,
                ..
            } => format!("Back up {database} from {}", name_of(profile_id)),
            PipelineStep::Restore {
                profile_id, naming, ..
            } => {
                let target = match naming {
                    TargetNaming::NewTimestamped { prefix } => {
                        format!("a new {prefix}_… database")
                    }
                    TargetNaming::DropAndRecreate { name } => format!("{name}, replacing it"),
                    TargetNaming::IntoExisting { name } => format!("the existing {name}"),
                };
                format!("Restore into {target} on {}", name_of(profile_id))
            }
            PipelineStep::Verify { deep } => match deep {
                true => "Compare rows and contents against the source".to_string(),
                false => "Compare row counts against the source".to_string(),
            },
            PipelineStep::Mask { rules } => format!(
                "Mask {} column{} on the destination",
                rules.len(),
                if rules.len() == 1 { "" } else { "s" }
            ),
            PipelineStep::PushOffsite => "Copy off-site".to_string(),
            PipelineStep::Retention { policy } => match (policy.keep_last, policy.max_age_days) {
                (Some(n), _) => format!("Keep only the newest {n} backups"),
                (None, Some(d)) => format!("Delete backups older than {d} days"),
                (None, None) => "Apply retention".to_string(),
            },
            PipelineStep::Drill { profile_id, .. } => {
                format!("Drill the newest backup on {}", name_of(profile_id))
            }
        }
    }
}

/// A saved chain of actions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct Pipeline {
    pub id: Uuid,
    pub name: String,
    pub steps: Vec<PipelineStep>,
    /// The destructive signature a human typed back when arming this pipeline
    /// for unattended use, or `None` if it has never been armed.
    ///
    /// Compared against the current signature rather than trusted: see
    /// [`Pipeline::is_armed`].
    #[serde(default)]
    pub unattended_ack: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct PipelineCreate {
    pub name: String,
    pub steps: Vec<PipelineStep>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
pub struct PipelineUpdate {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub steps: Option<Vec<PipelineStep>>,
}

impl Pipeline {
    pub fn is_destructive(&self) -> bool {
        self.steps.iter().any(PipelineStep::is_destructive)
    }

    /// The names this pipeline will drop, in step order.
    pub fn destructive_targets(&self) -> Vec<String> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                PipelineStep::Restore {
                    naming: TargetNaming::DropAndRecreate { name },
                    ..
                } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    /// What arming this pipeline commits to, or `None` when nothing is dropped.
    ///
    /// Deliberately derived from the targets rather than being a flag. A flag
    /// survives an edit; this does not, which is the whole point — renaming the
    /// target of a nightly replace must disarm it, not re-aim it.
    pub fn destructive_signature(&self) -> Option<String> {
        let targets = self.destructive_targets();
        (!targets.is_empty()).then(|| targets.join("\n"))
    }

    /// Whether this pipeline may run with nobody present to confirm.
    ///
    /// A pipeline that destroys nothing needs no arming and is not "armed" —
    /// callers should ask [`Self::is_destructive`] first. A destructive one is
    /// armed only while the stored acknowledgment still describes the targets
    /// it currently has.
    pub fn is_armed(&self) -> bool {
        match (&self.unattended_ack, self.destructive_signature()) {
            (Some(ack), Some(current)) => *ack == current,
            _ => false,
        }
    }

    /// Structural validation: the order of the steps, on their own.
    ///
    /// Everything here is decidable from the definition, so the editor can say
    /// why Save is disabled without a round trip and the store can refuse a
    /// pipeline the CLI would otherwise write.
    pub fn validate(&self) -> Result<(), PipelineError> {
        validate_steps(&self.name, &self.steps)
    }

    /// Everything [`Self::validate`] checks, plus what needs the connections.
    pub fn validate_against(&self, profiles: &[ConnectionProfile]) -> Result<(), PipelineError> {
        self.validate()?;

        for (i, step) in self.steps.iter().enumerate() {
            if let Some(id) = step.profile_id()
                && !profiles.iter().any(|p| p.id == id)
            {
                return Err(PipelineError::UnknownProfile { step: i + 1 });
            }
        }

        // Nothing here translates dialects, so a chain that dumps MySQL and
        // replays it into PostgreSQL is a migration wearing a pipeline's
        // clothes. Refuse it before anything is touched, as `ops::sync` does.
        let engine_of = |id: &Uuid| profiles.iter().find(|p| p.id == *id).map(|p| p.engine);
        let mut carried: Option<(Engine, usize)> = None;

        for (i, step) in self.steps.iter().enumerate() {
            match step {
                PipelineStep::Backup { profile_id, .. } => {
                    carried = engine_of(profile_id).map(|e| (e, i + 1));
                }
                PipelineStep::Restore {
                    profile_id,
                    source: ArtifactSource::PreviousStep,
                    ..
                } => {
                    if let (Some((source_engine, backup_step)), Some(dest_engine)) =
                        (carried, engine_of(profile_id))
                        && source_engine != dest_engine
                    {
                        return Err(PipelineError::EngineMismatch {
                            backup_step,
                            restore_step: i + 1,
                            source_engine,
                            dest_engine,
                        });
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }
}

impl PipelineCreate {
    pub fn validate(&self) -> Result<(), PipelineError> {
        validate_steps(&self.name, &self.steps)
    }
}

/// The rules, in one place so create, update and the editor cannot disagree.
fn validate_steps(name: &str, steps: &[PipelineStep]) -> Result<(), PipelineError> {
    if name.trim().is_empty() {
        return Err(PipelineError::NoName);
    }
    if steps.is_empty() {
        return Err(PipelineError::Empty);
    }

    let mut seen_backup = false;
    let mut seen_restore = false;

    for (i, step) in steps.iter().enumerate() {
        let step_number = i + 1;
        match step {
            PipelineStep::Backup { database, .. } => {
                if database.trim().is_empty() {
                    return Err(PipelineError::NoDatabase { step: step_number });
                }
                seen_backup = true;
            }
            PipelineStep::Restore { source, .. } => {
                if matches!(source, ArtifactSource::PreviousStep) && !seen_backup {
                    return Err(PipelineError::NothingToRestore { step: step_number });
                }
                seen_restore = true;
            }
            PipelineStep::Verify { .. } => {
                if !seen_restore {
                    return Err(PipelineError::NothingRestoredYet {
                        step: step_number,
                        what: "verify",
                    });
                }
                if !seen_backup {
                    // Verification compares the destination against the source
                    // it was taken from. An artifact restored from disk has no
                    // source connection in this run to compare against.
                    return Err(PipelineError::NothingToCompareAgainst { step: step_number });
                }
            }
            PipelineStep::Mask { rules } => {
                if !seen_restore {
                    return Err(PipelineError::NothingRestoredYet {
                        step: step_number,
                        what: "mask",
                    });
                }
                if rules.is_empty() {
                    return Err(PipelineError::NoRules { step: step_number });
                }
            }
            PipelineStep::PushOffsite => {
                if !seen_backup {
                    return Err(PipelineError::NoArtifactYet {
                        step: step_number,
                        what: "copy off-site",
                    });
                }
            }
            PipelineStep::Retention { policy } => {
                if !seen_backup {
                    return Err(PipelineError::NoArtifactYet {
                        step: step_number,
                        what: "apply retention",
                    });
                }
                if !policy.is_enabled() {
                    return Err(PipelineError::EmptyRetention { step: step_number });
                }
            }
            PipelineStep::Drill { .. } => {}
        }
    }

    // Masking's guarantee is that the destination ends up masked or ends up
    // gone, and that rests on being able to drop it. `IntoExisting` restores
    // into a database that was already there, so dropping it would destroy data
    // this run never created. Same rule as `SyncRequest::validate_masking`,
    // refused here rather than at the point of no return.
    let mut last_naming: Option<&TargetNaming> = None;
    for (i, step) in steps.iter().enumerate() {
        match step {
            PipelineStep::Restore { naming, .. } => last_naming = Some(naming),
            PipelineStep::Mask { .. } => {
                if let Some(TargetNaming::IntoExisting { name }) = last_naming {
                    return Err(PipelineError::MaskingIntoExisting {
                        step: i + 1,
                        database: name.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    // Two steps dropping the same database means the second one destroys what
    // the first one just restored, which is never what was meant.
    let targets = steps.iter().filter_map(|s| match s {
        PipelineStep::Restore {
            naming: TargetNaming::DropAndRecreate { name },
            ..
        } => Some(name),
        _ => None,
    });
    let mut seen: BTreeSet<&String> = BTreeSet::new();
    for target in targets {
        if !seen.insert(target) {
            return Err(PipelineError::RepeatedDestructiveTarget {
                database: target.clone(),
            });
        }
    }

    Ok(())
}

/// Why a pipeline cannot run.
///
/// Every message names the step and says what to do about it: the editor puts
/// these straight in front of the user, and "invalid pipeline" would send them
/// looking.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PipelineError {
    #[error("a pipeline needs a name")]
    NoName,

    #[error("a pipeline needs at least one step")]
    Empty,

    #[error("step {step} backs up, but names no database")]
    NoDatabase { step: usize },

    #[error(
        "step {step} restores what a backup produced, but no earlier step makes one; \
         add a backup step before it, or point this one at a file"
    )]
    NothingToRestore { step: usize },

    #[error("step {step} would {what}, but nothing has been restored yet")]
    NothingRestoredYet { step: usize, what: &'static str },

    #[error(
        "step {step} verifies a restore against the source it came from, and this \
         pipeline restores a file it did not back up; remove the verify step"
    )]
    NothingToCompareAgainst { step: usize },

    #[error("step {step} masks nothing; add a rule or remove the step")]
    NoRules { step: usize },

    #[error("step {step} would {what}, but no earlier step produces an artifact")]
    NoArtifactYet { step: usize, what: &'static str },

    #[error("step {step} applies a retention policy that keeps everything")]
    EmptyRetention { step: usize },

    #[error(
        "step {step} masks {database}, which this pipeline restored into without \
         dropping; masking can only promise a database ends up masked or dropped, \
         and dropping {database} would destroy data this run did not create"
    )]
    MaskingIntoExisting { step: usize, database: String },

    #[error("two steps replace {database}; the second would destroy what the first restored")]
    RepeatedDestructiveTarget { database: String },

    #[error("step {step} names a connection that no longer exists")]
    UnknownProfile { step: usize },

    #[error(
        "this pipeline replaces nothing, so there is nothing to authorise; \
         arming is only for a pipeline that can drop a database unattended"
    )]
    NothingToAuthorise,

    #[error(
        "step {step} backs up {expected} using the table set {set:?}, which now describes \
         {found}; point the step at {found} or give it its own table list"
    )]
    TableSetMovedOn {
        step: usize,
        set: String,
        expected: String,
        found: String,
    },

    #[error("type {expected:?} to authorise this, not {got:?}")]
    ConfirmationDoesNotMatch { expected: String, got: String },

    // `source_engine`, not `source`: thiserror reads a field called `source` as
    // the underlying error and demands it implement Error.
    #[error(
        "step {backup_step} backs up {source_engine:?} and step {restore_step} restores \
         into {dest_engine:?}; nothing here translates between them"
    )]
    EngineMismatch {
        backup_step: usize,
        restore_step: usize,
        source_engine: Engine,
        dest_engine: Engine,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::MysqlBackupOptions;
    use crate::restore::MysqlRestoreOptions;

    fn backup_step() -> PipelineStep {
        PipelineStep::Backup {
            profile_id: Uuid::nil(),
            database: "shop".into(),
            plan_id: None,
            selections: Vec::new(),
            output_dir: None,
            compress: true,
            encrypt: false,
            record_row_counts: false,
            engine: EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
        }
    }

    fn restore_step(naming: TargetNaming) -> PipelineStep {
        PipelineStep::Restore {
            profile_id: Uuid::nil(),
            source: ArtifactSource::PreviousStep,
            naming,
            engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify_checksum: true,
        }
    }

    fn pipeline(steps: Vec<PipelineStep>) -> Pipeline {
        Pipeline {
            id: Uuid::nil(),
            name: "nightly".into(),
            steps,
            unattended_ack: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn a_backup_then_restore_is_valid() {
        let p = pipeline(vec![
            backup_step(),
            restore_step(TargetNaming::NewTimestamped {
                prefix: "copy".into(),
            }),
            PipelineStep::Verify { deep: false },
        ]);
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn a_restore_with_no_backup_before_it_is_refused() {
        let p = pipeline(vec![restore_step(TargetNaming::NewTimestamped {
            prefix: "copy".into(),
        })]);
        assert_eq!(
            p.validate(),
            Err(PipelineError::NothingToRestore { step: 1 })
        );
    }

    #[test]
    fn a_restore_from_a_file_needs_no_backup() {
        let p = pipeline(vec![PipelineStep::Restore {
            profile_id: Uuid::nil(),
            source: ArtifactSource::Path {
                path: "/tmp/shop.sql.gz".into(),
            },
            naming: TargetNaming::NewTimestamped {
                prefix: "copy".into(),
            },
            engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
            verify_checksum: true,
        }]);
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn verifying_a_restore_from_a_file_is_refused_rather_than_silently_skipped() {
        // There is no source connection in this run to compare against, and a
        // verify step that quietly did nothing would read as a passed check.
        let p = pipeline(vec![
            PipelineStep::Restore {
                profile_id: Uuid::nil(),
                source: ArtifactSource::Path {
                    path: "/tmp/shop.sql.gz".into(),
                },
                naming: TargetNaming::NewTimestamped {
                    prefix: "copy".into(),
                },
                engine: EngineRestoreOptions::Mysql(MysqlRestoreOptions::default()),
                verify_checksum: true,
            },
            PipelineStep::Verify { deep: false },
        ]);
        assert_eq!(
            p.validate(),
            Err(PipelineError::NothingToCompareAgainst { step: 2 })
        );
    }

    #[test]
    fn masking_into_an_existing_database_is_refused_up_front() {
        // Masking can only promise "masked or gone", and it cannot drop a
        // database this run did not create. Caught here rather than after the
        // real data has already landed.
        let p = pipeline(vec![
            backup_step(),
            restore_step(TargetNaming::IntoExisting {
                name: "staging".into(),
            }),
            PipelineStep::Mask {
                rules: vec![MaskRule::email("users", "email")],
            },
        ]);
        assert_eq!(
            p.validate(),
            Err(PipelineError::MaskingIntoExisting {
                step: 3,
                database: "staging".into()
            })
        );
    }

    #[test]
    fn masking_after_a_replace_is_allowed() {
        let p = pipeline(vec![
            backup_step(),
            restore_step(TargetNaming::DropAndRecreate {
                name: "staging".into(),
            }),
            PipelineStep::Mask {
                rules: vec![MaskRule::email("users", "email")],
            },
        ]);
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn an_empty_pipeline_is_refused() {
        assert_eq!(pipeline(Vec::new()).validate(), Err(PipelineError::Empty));
    }

    #[test]
    fn off_site_and_retention_need_an_artifact() {
        assert_eq!(
            pipeline(vec![PipelineStep::PushOffsite]).validate(),
            Err(PipelineError::NoArtifactYet {
                step: 1,
                what: "copy off-site"
            })
        );
    }

    #[test]
    fn two_steps_replacing_one_database_are_refused() {
        let p = pipeline(vec![
            backup_step(),
            restore_step(TargetNaming::DropAndRecreate {
                name: "staging".into(),
            }),
            restore_step(TargetNaming::DropAndRecreate {
                name: "staging".into(),
            }),
        ]);
        assert_eq!(
            p.validate(),
            Err(PipelineError::RepeatedDestructiveTarget {
                database: "staging".into()
            })
        );
    }

    #[test]
    fn only_a_drop_and_recreate_counts_as_destructive() {
        let safe = pipeline(vec![
            backup_step(),
            restore_step(TargetNaming::NewTimestamped {
                prefix: "copy".into(),
            }),
        ]);
        assert!(!safe.is_destructive());
        assert_eq!(safe.destructive_signature(), None);

        let replaces = pipeline(vec![
            backup_step(),
            restore_step(TargetNaming::DropAndRecreate {
                name: "staging".into(),
            }),
        ]);
        assert!(replaces.is_destructive());
        assert_eq!(replaces.destructive_signature().as_deref(), Some("staging"));
    }

    #[test]
    fn renaming_a_destructive_target_disarms_the_pipeline() {
        // The property the whole arming design rests on: permission is granted
        // for a named database, not for a pipeline.
        let mut p = pipeline(vec![
            backup_step(),
            restore_step(TargetNaming::DropAndRecreate {
                name: "staging".into(),
            }),
        ]);
        p.unattended_ack = p.destructive_signature();
        assert!(p.is_armed());

        p.steps[1] = restore_step(TargetNaming::DropAndRecreate {
            name: "production".into(),
        });
        assert!(
            !p.is_armed(),
            "an acknowledgment for `staging` must not authorise dropping `production`"
        );
    }

    #[test]
    fn a_pipeline_that_destroys_nothing_is_never_armed() {
        let mut p = pipeline(vec![
            backup_step(),
            restore_step(TargetNaming::NewTimestamped {
                prefix: "copy".into(),
            }),
        ]);
        p.unattended_ack = Some("anything".into());
        assert!(!p.is_armed());
        assert!(!p.is_destructive());
    }

    #[test]
    fn a_step_written_before_a_field_existed_still_reads() {
        // Built by taking a current step and deleting the two fields added
        // since, rather than by hand-writing older JSON that would drift.
        let mut value = serde_json::to_value(restore_step(TargetNaming::NewTimestamped {
            prefix: "copy".into(),
        }))
        .unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("source");
        object.remove("verify_checksum");

        let step: PipelineStep =
            serde_json::from_value(value).expect("an older step must still read");
        match step {
            PipelineStep::Restore {
                source,
                verify_checksum,
                ..
            } => {
                assert_eq!(source, ArtifactSource::PreviousStep, "the safe default");
                assert!(verify_checksum, "checksums stay on unless turned off");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
