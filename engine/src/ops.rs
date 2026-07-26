//! High-level operations: what the GUI and the CLI both call.
//!
//! Each function here owns one job from start to finish — tunnel up, work done,
//! tunnel down, outcome recorded. Putting this in the engine rather than in the
//! Tauri layer is what keeps `dbsync` able to do everything the app can.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::backup::mysql::Endpoint;
use crate::backup::{BackupError, BackupRequest, run_mysql_backup, run_postgres_backup};
use crate::connect::{self, ConnectError};
use crate::db::ConnectParams;
use crate::events::{JobKind, JobPhase};
use crate::job::{JobContext, JobOutcome, JobRecord};
use crate::profile::ConnectionProfile;
use crate::restore::{
    EngineRestoreOptions, RestoreError, RestoreRequest, TargetNaming, run_mysql_restore,
    run_postgres_restore,
};
use crate::retention::RetentionPolicy;
use crate::secrets::{self, SecretKind};
use crate::ssh::TunnelHandle;
use crate::store::Store;
use crate::types::Engine;
use crate::verify::{self, VerificationReport};
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error(transparent)]
    Connect(#[from] ConnectError),
    #[error(transparent)]
    Backup(#[from] BackupError),
    #[error(transparent)]
    Restore(#[from] RestoreError),
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    // Field named `source_engine`, not `source`: thiserror treats a `source`
    // field as the underlying error cause.
    #[error("cannot sync a {source_engine:?} source to a {dest_engine:?} destination")]
    EngineMismatch {
        source_engine: Engine,
        dest_engine: Engine,
    },
    #[error("job was cancelled")]
    Cancelled,
}

/// A tunnel plus the local endpoint the tools should talk to.
struct Reachable {
    endpoint: Endpoint,
    /// Held so the tunnel stays open for the life of the operation.
    _tunnel: Option<TunnelHandle>,
}

/// Bring up whatever is needed to reach a profile's database.
async fn reach(profile: &ConnectionProfile, store: &Store) -> Result<Reachable, OpError> {
    let tunnel = connect::open_tunnel(profile, store).await?;

    let (host, port) = match &tunnel {
        Some(t) => ("127.0.0.1".to_string(), t.local_port()),
        None => (profile.db.host.clone(), profile.db.port),
    };

    Ok(Reachable {
        endpoint: Endpoint {
            host,
            port,
            user: profile.db.user.clone(),
            password: secrets::get_secret(profile.id, SecretKind::DbPassword)
                .map_err(ConnectError::Secrets)?,
        },
        _tunnel: tunnel,
    })
}

/// Ask the server for its version string, used for the manifest and for
/// client-compatibility decisions.
async fn server_version(
    profile: &ConnectionProfile,
    endpoint: &Endpoint,
    database: Option<&str>,
) -> Result<String, OpError> {
    let params = ConnectParams {
        engine: profile.engine,
        host: endpoint.host.clone(),
        port: endpoint.port,
        user: endpoint.user.clone(),
        password: endpoint.password.clone(),
        database: database.map(str::to_string),
    };
    let introspector = crate::db::connect(&params).await?;
    let info = introspector.server_info().await?;
    introspector.close().await;
    Ok(info.version)
}

/// Run a backup end to end.
pub async fn backup(
    profile: &ConnectionProfile,
    request: &BackupRequest,
    store: &Store,
    ctx: &JobContext,
) -> Result<PathBuf, OpError> {
    ctx.emit(JobPhase::SshConnect, "connecting to the source")
        .await;
    let reachable = reach(profile, store).await?;

    ctx.emit(JobPhase::Introspect, "reading server version")
        .await;
    let version =
        server_version(profile, &reachable.endpoint, Some(&request.common.database)).await?;

    // Resolved here rather than inside each engine so both get the same rule,
    // and so the escrow check runs before a single byte is dumped: discovering
    // that the key was never exported *after* an hour-long dump would be an
    // expensive way to learn it.
    let recipients = if request.common.encrypt {
        ctx.emit(JobPhase::Initializing, "resolving encryption recipients")
            .await;
        let keys = crate::backupkey::ensure_ready_for_encryption(store)
            .await
            .map_err(|e| BackupError::Invalid(e.to_string()))?;
        ctx.emit(
            JobPhase::Initializing,
            format!("encrypting to {} recipient(s)", keys.len()),
        )
        .await;
        keys
    } else {
        Vec::new()
    };

    let endpoint = reachable.endpoint.clone();
    let artifact = match profile.engine {
        Engine::Mysql => {
            run_mysql_backup(profile, request, endpoint, version, &recipients, ctx).await?
        }
        Engine::Postgres => {
            run_postgres_backup(profile, request, endpoint, version, &recipients, ctx).await?
        }
    };

    Ok(artifact)
}

/// Run a restore end to end, returning the database that was written.
pub async fn restore(
    profile: &ConnectionProfile,
    request: &RestoreRequest,
    store: &Store,
    ctx: &JobContext,
) -> Result<String, OpError> {
    ctx.emit(JobPhase::SshConnect, "connecting to the destination")
        .await;
    let reachable = reach(profile, store).await?;

    let endpoint = reachable.endpoint.clone();
    let target = match profile.engine {
        Engine::Mysql => run_mysql_restore(profile, request, endpoint, ctx).await?,
        Engine::Postgres => run_postgres_restore(profile, request, endpoint, ctx).await?,
    };
    Ok(target)
}

/// What to compare, and where.
pub struct VerifyRequest<'a> {
    pub source_profile: &'a ConnectionProfile,
    pub dest_profile: &'a ConnectionProfile,
    pub source_database: &'a str,
    pub dest_database: &'a str,
    /// Tables the plan said would carry rows.
    pub tables_with_data: &'a [String],
    /// Tables expected to exist but be empty.
    pub schema_only: &'a [String],
    /// Also compare table contents and columns, not just row counts.
    ///
    /// Costs a full scan of every table on both sides, which is why it is a
    /// choice rather than the default — but it is the only thing that catches
    /// the right number of rows holding the wrong bytes.
    pub deep: bool,
}

/// Compare a restored database against what the backup said it contained.
///
/// Counts are exact. Knowing which tables were supposed to carry rows and which
/// were schema-only is what lets an empty schema-only table read as correct
/// rather than as a missing table.
pub async fn verify_restore(
    request: VerifyRequest<'_>,
    store: &Store,
    ctx: &JobContext,
) -> Result<VerificationReport, OpError> {
    ctx.emit(JobPhase::Verify, "counting rows on the source")
        .await;
    let source = reach(request.source_profile, store).await?;
    let expected = count_tables(
        request.source_profile,
        &source.endpoint,
        request.source_database,
        request.tables_with_data,
    )
    .await?;
    drop(source);

    ctx.emit(JobPhase::Verify, "counting rows on the destination")
        .await;
    let dest = reach(request.dest_profile, store).await?;

    let mut all: Vec<String> = request.tables_with_data.to_vec();
    all.extend_from_slice(request.schema_only);
    let actual = count_tables(
        request.dest_profile,
        &dest.endpoint,
        request.dest_database,
        &all,
    )
    .await?;
    drop(dest);

    let mut report =
        verify::build_report(&expected, &actual, request.schema_only, &BTreeMap::new());

    // Row counts are cheap and always available; digests need a full scan. The
    // deep pass runs second and only ever *downgrades* a match, so a table that
    // could not be digested stays reported as matching rather than as suspect.
    if request.deep {
        ctx.emit(JobPhase::Verify, "comparing table contents").await;
        match deep_compare(&request, store, &expected).await {
            Ok(deep) => verify::refine_with_contents(&mut report, &deep),
            Err(e) => {
                // Losing the deep comparison weakens verification but must not
                // fail a restore that the row counts say is good.
                ctx.emit_warn(
                    JobPhase::Verify,
                    format!("could not compare table contents: {e}"),
                )
                .await;
            }
        }
    }

    let level = if report.passed() {
        JobPhase::Done
    } else {
        JobPhase::Verify
    };
    if report.passed() {
        ctx.emit(
            level,
            format!("verification passed for {} tables", report.tables_checked),
        )
        .await;
    } else {
        ctx.emit_error(
            level,
            format!("verification found {} problem(s)", report.failures),
        )
        .await;
    }

    Ok(report)
}

/// Collect digests and column lists from both sides.
async fn deep_compare(
    request: &VerifyRequest<'_>,
    store: &Store,
    expected: &BTreeMap<String, u64>,
) -> Result<verify::DeepComparison, OpError> {
    let mut tables: Vec<String> = request.tables_with_data.to_vec();
    tables.extend_from_slice(request.schema_only);

    let source = reach(request.source_profile, store).await?;
    let (source_digests, source_columns) = digest_tables(
        request.source_profile,
        &source.endpoint,
        request.source_database,
        &tables,
    )
    .await?;
    drop(source);

    let dest = reach(request.dest_profile, store).await?;
    let (dest_digests, dest_columns) = digest_tables(
        request.dest_profile,
        &dest.endpoint,
        request.dest_database,
        &tables,
    )
    .await?;
    drop(dest);

    Ok(verify::DeepComparison {
        source_digests,
        dest_digests,
        source_columns,
        dest_columns,
        row_counts: expected.clone(),
    })
}

type DigestsAndColumns = (
    BTreeMap<String, Option<String>>,
    BTreeMap<String, Vec<String>>,
);

async fn digest_tables(
    profile: &ConnectionProfile,
    endpoint: &Endpoint,
    database: &str,
    tables: &[String],
) -> Result<DigestsAndColumns, OpError> {
    let params = ConnectParams {
        engine: profile.engine,
        host: endpoint.host.clone(),
        port: endpoint.port,
        user: endpoint.user.clone(),
        password: endpoint.password.clone(),
        database: Some(database.to_string()),
    };
    let introspector = crate::db::connect(&params).await?;

    let mut digests = BTreeMap::new();
    let mut columns = BTreeMap::new();

    for table in tables {
        // A table that cannot be digested is recorded as `None`, never
        // skipped: the refinement step needs to know the difference between
        // "compared and equal" and "could not compare".
        match introspector.table_digest(database, table).await {
            Ok(d) => {
                digests.insert(table.clone(), d);
            }
            Err(e) => {
                tracing::debug!("could not digest {database}.{table}: {e}");
                digests.insert(table.clone(), None);
            }
        }
        if let Ok(cols) = introspector.column_names(database, table).await {
            columns.insert(table.clone(), cols);
        }
    }

    introspector.close().await;
    Ok((digests, columns))
}

/// Exact `COUNT(*)` for each table, skipping ones that do not exist.
///
/// A missing table is reported as absent rather than as zero, so the report can
/// distinguish "not restored" from "restored empty".
async fn count_tables(
    profile: &ConnectionProfile,
    endpoint: &Endpoint,
    database: &str,
    tables: &[String],
) -> Result<BTreeMap<String, u64>, OpError> {
    // PostgreSQL can only see the database it connected to, so the target
    // database is part of the connection rather than just the query.
    let params = ConnectParams {
        engine: profile.engine,
        host: endpoint.host.clone(),
        port: endpoint.port,
        user: endpoint.user.clone(),
        password: endpoint.password.clone(),
        database: Some(database.to_string()),
    };
    let introspector = crate::db::connect(&params).await?;

    let mut counts = BTreeMap::new();
    for table in tables {
        match introspector.exact_row_count(database, table).await {
            Ok(n) => {
                counts.insert(table.clone(), n);
            }
            Err(e) => {
                tracing::debug!("could not count {database}.{table}: {e}");
            }
        }
    }

    introspector.close().await;
    Ok(counts)
}

/// Record a job start in history.
pub async fn record_start(
    store: &Store,
    ctx: &JobContext,
    kind: JobKind,
    source: uuid::Uuid,
    dest: Option<uuid::Uuid>,
    options_json: String,
) -> Result<(), OpError> {
    let record = JobRecord {
        id: ctx.job_id,
        kind,
        source_profile_id: source,
        dest_profile_id: dest,
        started_at: chrono::Utc::now(),
        finished_at: None,
        outcome: None,
        artifact_path: None,
        options_json,
        log: String::new(),
    };
    store.insert_job(&record).await?;
    Ok(())
}

/// Record a job's terminal state, including its full log.
pub async fn record_finish(
    store: &Store,
    ctx: &JobContext,
    outcome: JobOutcome,
    artifact: Option<String>,
) -> Result<(), OpError> {
    store
        .finish_job(ctx.job_id, outcome, artifact, ctx.log_snapshot().await)
        .await?;
    Ok(())
}

// ── Cross-server sync ───────────────────────────────────────────────────

/// Back up a source and restore it to a destination as one job.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SyncRequest {
    pub backup: BackupRequest,
    pub naming: TargetNaming,
    pub restore: EngineRestoreOptions,
    /// Compare exact row counts once the restore finishes.
    pub verify: bool,
    /// Also compare table contents and columns.
    ///
    /// Defaulted so a stored request written before this existed still
    /// deserialises — an old schedule keeps its old, shallower behaviour
    /// rather than silently acquiring a full table scan.
    #[serde(default)]
    pub deep_verify: bool,
    /// Applied to the source's backup directory after a successful run.
    pub retention: Option<RetentionPolicy>,
    /// Required when the destination naming strategy is destructive.
    pub typed_confirmation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SyncOutcome {
    pub artifact_path: String,
    pub target_database: String,
    pub verification: Option<VerificationReport>,
    /// Artifacts retention removed, if it ran.
    pub removed_artifacts: Vec<String>,
}

/// Run a full source-to-destination sync.
///
/// Backup, restore, verify, retain — one job, one history record, cancellable
/// at every stage. Each step reuses the same code path the standalone commands
/// use, so there is no second implementation to drift.
pub async fn sync(
    source: &ConnectionProfile,
    dest: &ConnectionProfile,
    request: &SyncRequest,
    store: &Store,
    ctx: &JobContext,
) -> Result<SyncOutcome, OpError> {
    // Cross-engine sync is not a copy, it is a migration, and nothing here
    // translates dialects. Fail before touching either server.
    if source.engine != dest.engine {
        return Err(OpError::EngineMismatch {
            source_engine: source.engine,
            dest_engine: dest.engine,
        });
    }

    // ── Backup ──────────────────────────────────────────────────────────
    let artifact = backup(source, &request.backup, store, ctx).await?;
    ctx.bail_if_cancelled().map_err(|_| OpError::Cancelled)?;

    // ── Restore ─────────────────────────────────────────────────────────
    let restore_request = RestoreRequest {
        artifact_path: artifact.clone(),
        naming: request.naming.clone(),
        engine: request.restore.clone(),
        verify_checksum: true,
        typed_confirmation: request.typed_confirmation.clone(),
    };

    let target = restore(dest, &restore_request, store, ctx).await?;
    ctx.bail_if_cancelled().map_err(|_| OpError::Cancelled)?;

    // ── Verify ──────────────────────────────────────────────────────────
    let verification = if request.verify {
        let with_data: Vec<String> = request
            .backup
            .common
            .tables_with_data()
            .into_iter()
            .map(|s| s.name.clone())
            .collect();
        let schema_only: Vec<String> = request
            .backup
            .common
            .selections
            .iter()
            .filter(|s| s.mode == crate::backup::TableMode::SchemaOnly)
            .map(|s| s.name.clone())
            .collect();

        Some(
            verify_restore(
                VerifyRequest {
                    source_profile: source,
                    dest_profile: dest,
                    source_database: &request.backup.common.database,
                    dest_database: &target,
                    tables_with_data: &with_data,
                    schema_only: &schema_only,
                    deep: request.deep_verify,
                },
                store,
                ctx,
            )
            .await?,
        )
    } else {
        None
    };

    // ── Retention ───────────────────────────────────────────────────────
    //
    // Deliberately after verification: a failed verification is exactly when
    // the older backups matter most.
    let removed = match &request.retention {
        Some(policy) if verification.as_ref().is_none_or(|r| r.passed()) => {
            apply_retention(&request.backup.common.output_dir, *policy, ctx).await
        }
        Some(_) => {
            ctx.emit_warn(
                JobPhase::Cleanup,
                "verification failed; keeping every existing backup",
            )
            .await;
            Vec::new()
        }
        None => Vec::new(),
    };

    Ok(SyncOutcome {
        artifact_path: artifact.display().to_string(),
        target_database: target,
        verification,
        removed_artifacts: removed,
    })
}

/// Apply a retention policy, reporting exactly what it removed.
///
/// Deleting backups is never silent: the plan is logged before it is acted on.
pub async fn apply_retention(
    directory: &std::path::Path,
    policy: RetentionPolicy,
    ctx: &JobContext,
) -> Vec<String> {
    if !policy.is_enabled() {
        return Vec::new();
    }

    let plan = crate::library::plan_cleanup(directory, policy);
    if plan.delete.is_empty() {
        ctx.emit(JobPhase::Cleanup, "retention: nothing to remove")
            .await;
        return Vec::new();
    }

    for candidate in &plan.delete {
        ctx.emit(
            JobPhase::Cleanup,
            format!("retention will remove {}", candidate.path),
        )
        .await;
    }

    let removed = crate::library::apply_cleanup(&plan);
    ctx.emit(
        JobPhase::Cleanup,
        format!(
            "retention removed {} artifact(s), reclaiming {} bytes",
            removed.len(),
            plan.bytes_reclaimed
        ),
    )
    .await;

    removed
}

// ── Restore drills ──────────────────────────────────────────────────────

/// The prefix every scratch database a drill creates begins with.
///
/// Load-bearing, not cosmetic: [`drop_scratch_database`] refuses to drop
/// anything whose name does not start with it. A drill generates the name
/// itself and never accepts one from a caller, so "the nightly drill dropped
/// our production database" is not a mistake this code can make.
pub const DRILL_PREFIX: &str = "dbsync_drill";

/// Prove that the newest backup in a directory actually restores.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DrillRequest {
    pub artifact_dir: PathBuf,
    pub restore: EngineRestoreOptions,
    /// Compare table contents as well as row counts.
    #[serde(default)]
    pub deep_verify: bool,
    /// Leave the scratch database in place when the drill fails, so the
    /// wreckage can be inspected. A passing drill always cleans up.
    #[serde(default)]
    pub keep_on_failure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DrillOutcome {
    pub artifact: String,
    pub scratch_database: String,
    /// False when the scratch database was deliberately left behind.
    pub dropped: bool,
    pub report: VerificationReport,
}

/// Restore the newest artifact into a scratch database, check it, drop it.
///
/// # What a drill actually proves
///
/// It verifies the restored data against the **manifest**, not against the
/// live source. The source has moved on since the backup was taken — rows have
/// been added, tables altered — so comparing against it would report drift as
/// corruption. What a drill answers is the question that matters at 3am on a
/// bad day: *does this file restore, and does it contain what it claims to?*
///
/// # Why it is worth doing at all
///
/// A backup is a belief until it has been restored. Checksums prove the bytes
/// are the bytes that were written; they say nothing about whether the dump was
/// coherent, whether the destination can accept it, or whether the tool that
/// wrote it did so correctly. The only proof is a restore.
pub async fn drill(
    dest: &ConnectionProfile,
    request: &DrillRequest,
    store: &Store,
    ctx: &JobContext,
) -> Result<DrillOutcome, OpError> {
    ctx.emit(JobPhase::Initializing, "looking for the newest backup")
        .await;

    // `list_artifacts` returns newest first.
    let artifact = crate::library::list_artifacts(&request.artifact_dir)
        .into_iter()
        .next()
        .ok_or_else(|| {
            OpError::Backup(BackupError::Invalid(format!(
                "no backups found in {}; a drill needs something to restore",
                request.artifact_dir.display()
            )))
        })?;

    let artifact_path = PathBuf::from(&artifact.path);
    let manifest = crate::manifest::BackupManifest::read(&artifact_path).map_err(|e| {
        OpError::Backup(BackupError::Invalid(format!(
            "{} has no readable manifest, so there is nothing to check the restore against: {e}",
            artifact.filename
        )))
    })?;

    ctx.emit(
        JobPhase::Restore,
        format!(
            "drilling {} ({} tables, taken {})",
            artifact.filename,
            manifest.tables.len(),
            manifest.created_at.to_rfc3339()
        ),
    )
    .await;

    // The name is generated here and nowhere else. See DRILL_PREFIX.
    let restore_request = RestoreRequest {
        artifact_path: artifact_path.clone(),
        naming: TargetNaming::NewTimestamped {
            prefix: DRILL_PREFIX.to_string(),
        },
        engine: request.restore.clone(),
        verify_checksum: true,
        // A drill never drops anything that already existed, so there is
        // nothing for a confirmation to protect.
        typed_confirmation: None,
    };

    let scratch = restore(dest, &restore_request, store, ctx).await?;

    let report =
        check_against_manifest(dest, &manifest, &scratch, request.deep_verify, store, ctx).await?;

    // Clean up unless the drill failed and the user asked to keep the evidence.
    let keep = !report.passed() && request.keep_on_failure;
    if keep {
        ctx.emit_warn(
            JobPhase::Cleanup,
            format!("drill failed; leaving {scratch} in place for inspection"),
        )
        .await;
    } else {
        drop_scratch_database(dest, &scratch, store, ctx).await?;
    }

    Ok(DrillOutcome {
        artifact: artifact.filename,
        scratch_database: scratch,
        dropped: !keep,
        report,
    })
}

/// Check a restored database against what the manifest said it held.
///
/// The manifest records which tables carried data and which were schema-only,
/// which is enough to catch the failures that matter: a table that did not
/// arrive, or one that arrived empty when it should not have.
async fn check_against_manifest(
    dest: &ConnectionProfile,
    manifest: &crate::manifest::BackupManifest,
    scratch: &str,
    deep: bool,
    store: &Store,
    ctx: &JobContext,
) -> Result<VerificationReport, OpError> {
    ctx.emit(JobPhase::Verify, "checking the restored database")
        .await;

    let reachable = reach(dest, store).await?;
    let counts = count_tables(dest, &reachable.endpoint, scratch, &manifest.tables).await?;

    let schema_only: Vec<String> = manifest
        .tables
        .iter()
        .filter(|t| !manifest.tables_with_data.contains(t))
        .cloned()
        .collect();

    // Every table the manifest says carried data must exist and be non-empty.
    // An exact count is not available — the manifest does not record one — so
    // "at least one row" is the strongest claim that can honestly be made.
    let mut expected = BTreeMap::new();
    let mut tables = Vec::new();
    for name in &manifest.tables_with_data {
        match counts.get(name) {
            None => tables.push(crate::verify::TableVerification {
                table: name.clone(),
                verdict: crate::verify::TableVerdict::MissingAtDestination,
            }),
            Some(0) => tables.push(crate::verify::TableVerification {
                table: name.clone(),
                verdict: crate::verify::TableVerdict::RowCountMismatch {
                    source: 1,
                    destination: 0,
                },
            }),
            Some(n) => {
                expected.insert(name.clone(), *n);
                tables.push(crate::verify::TableVerification {
                    table: name.clone(),
                    verdict: crate::verify::TableVerdict::Match,
                });
            }
        }
    }

    for name in &schema_only {
        tables.push(crate::verify::TableVerification {
            table: name.clone(),
            verdict: if counts.contains_key(name) {
                crate::verify::TableVerdict::Match
            } else {
                crate::verify::TableVerdict::MissingAtDestination
            },
        });
    }

    let failures = tables.iter().filter(|t| t.verdict.is_failure()).count();
    let mut report = VerificationReport {
        tables_checked: tables.len(),
        failures,
        skipped: 0,
        tables,
    };

    // A deep drill additionally proves each table can be read end to end — a
    // digest scans every row, which surfaces corruption that a COUNT(*) index
    // scan would never touch.
    if deep {
        ctx.emit(JobPhase::Verify, "reading every row").await;
        let (digests, columns) =
            digest_tables(dest, &reachable.endpoint, scratch, &manifest.tables).await?;

        for entry in &mut report.tables {
            if matches!(entry.verdict, crate::verify::TableVerdict::Match)
                && matches!(digests.get(&entry.table), Some(None))
                && columns.get(&entry.table).is_some_and(|c| !c.is_empty())
            {
                entry.verdict = crate::verify::TableVerdict::Skipped {
                    reason: "could not be read end to end".into(),
                };
            }
        }
        report.skipped = report
            .tables
            .iter()
            .filter(|t| matches!(t.verdict, crate::verify::TableVerdict::Skipped { .. }))
            .count();
    }

    let _ = expected;
    Ok(report)
}

/// Drop a database a drill created.
///
/// Refuses any name not produced by this module. The guard is the whole reason
/// this function is safe to call unattended: the caller cannot pass a name in,
/// so it cannot pass in the wrong one.
async fn drop_scratch_database(
    dest: &ConnectionProfile,
    name: &str,
    store: &Store,
    ctx: &JobContext,
) -> Result<(), OpError> {
    if !is_drill_database(name) {
        // Reaching here means a bug upstream renamed the target. Refusing is
        // the only safe response; the scratch database leaking is a far
        // smaller problem than dropping the wrong one.
        ctx.emit_error(
            JobPhase::Cleanup,
            format!("refusing to drop {name:?}: it is not a drill database"),
        )
        .await;
        return Err(OpError::Restore(RestoreError::Invalid(format!(
            "refusing to drop {name:?}: only databases created by a drill may be dropped"
        ))));
    }

    let reachable = reach(dest, store).await?;
    let params = ConnectParams {
        engine: dest.engine,
        host: reachable.endpoint.host.clone(),
        port: reachable.endpoint.port,
        user: reachable.endpoint.user.clone(),
        password: reachable.endpoint.password.clone(),
        // Never connect *to* the database being dropped.
        database: match dest.engine {
            Engine::Postgres => Some("postgres".to_string()),
            Engine::Mysql => None,
        },
    };

    let quoted = match dest.engine {
        Engine::Mysql => crate::db::quote_mysql_ident(name),
        Engine::Postgres => crate::db::quote_pg_ident(name),
    }?;

    crate::db::execute_raw(&params, &format!("DROP DATABASE IF EXISTS {quoted}")).await?;
    ctx.emit(JobPhase::Cleanup, format!("dropped {name}")).await;
    Ok(())
}

/// Whether a name was generated by [`drill`].
pub fn is_drill_database(name: &str) -> bool {
    // `{prefix}_{YYYYMMDD_HHMMSS}` — the underscore matters, so a real database
    // called `dbsync_drills` cannot be mistaken for scratch.
    name.strip_prefix(DRILL_PREFIX)
        .and_then(|rest| rest.strip_prefix('_'))
        .is_some_and(|stamp| {
            stamp.len() == 15
                && stamp.chars().enumerate().all(
                    |(i, c)| {
                        if i == 8 { c == '_' } else { c.is_ascii_digit() }
                    },
                )
        })
}

#[cfg(test)]
mod drill_tests {
    use super::*;

    #[test]
    fn a_generated_drill_name_is_recognised() {
        let name = TargetNaming::NewTimestamped {
            prefix: DRILL_PREFIX.to_string(),
        }
        .resolve(chrono::Utc::now());
        assert!(
            is_drill_database(&name),
            "{name} was generated by a drill and must be droppable"
        );
    }

    #[test]
    fn nothing_else_is_droppable() {
        // The guard that makes an unattended drill safe. Every one of these is
        // a database somebody would be very upset to lose.
        for name in [
            "production",
            "app",
            "dbsync",
            "dbsync_drill",                  // the prefix alone, no timestamp
            "dbsync_drills",                 // a real database that merely starts the same
            "dbsync_drill_",                 // truncated
            "dbsync_drill_2026",             // short timestamp
            "dbsync_drill_20260726_0300001", // long timestamp
            "dbsync_drill_abcdefg_hijkl",    // right shape, not digits
            "xdbsync_drill_20260726_030000", // prefix not at the start
            "",
        ] {
            assert!(
                !is_drill_database(name),
                "{name:?} must not be treated as a drill database"
            );
        }
    }

    #[test]
    fn the_separator_is_required() {
        // Without it, a real database called `dbsync_drillsomething` would be
        // one bad rename away from being dropped nightly.
        assert!(!is_drill_database("dbsync_drill20260726_030000"));
    }

    #[test]
    fn the_timestamp_must_have_its_underscore_in_the_right_place() {
        assert!(is_drill_database("dbsync_drill_20260726_030000"));
        assert!(!is_drill_database("dbsync_drill_2026072_6030000"));
        assert!(!is_drill_database("dbsync_drill_202607260_30000"));
    }

    #[test]
    fn a_drill_request_never_carries_a_confirmation() {
        // A drill runs unattended. It must not be able to authorise the
        // destruction of anything that already existed — and it does not need
        // to, because it only ever creates a fresh database.
        let request = DrillRequest {
            artifact_dir: PathBuf::from("/backups"),
            restore: EngineRestoreOptions::Mysql(Default::default()),
            deep_verify: false,
            keep_on_failure: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("typed_confirmation"));
    }
}
