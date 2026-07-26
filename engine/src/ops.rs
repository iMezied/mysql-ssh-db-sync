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
use crate::restore::{RestoreError, RestoreRequest, run_mysql_restore, run_postgres_restore};
use crate::secrets::{self, SecretKind};
use crate::ssh::TunnelHandle;
use crate::store::Store;
use crate::types::Engine;
use crate::verify::{self, VerificationReport};

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

    let endpoint = reachable.endpoint.clone();
    let artifact = match profile.engine {
        Engine::Mysql => run_mysql_backup(profile, request, endpoint, version, ctx).await?,
        Engine::Postgres => run_postgres_backup(profile, request, endpoint, version, ctx).await?,
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

    let report = verify::build_report(&expected, &actual, request.schema_only, &BTreeMap::new());

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
