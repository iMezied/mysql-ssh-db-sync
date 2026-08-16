//! PostgreSQL backup execution.
//!
//! Unlike MySQL this is a single `pg_dump` invocation. There is no need to dump
//! schema and data separately: `--exclude-table-data` keeps a table's structure
//! while dropping its rows, which is exactly the "schema-only" selection the UI
//! offers. `pg_dump` also already emits constraints, indexes and triggers after
//! the data, so the trigger-ordering hazard that bites MySQL does not arise.
//!
//! Table names here are schema-qualified (`public.orders`). PostgreSQL patterns
//! without a qualifier match in every schema, which would silently exclude data
//! from a table of the same name in a different schema.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::mysql::Endpoint;
use super::{
    BackupError, BackupRun, EngineBackupOptions, PgDumpFormat, PostgresBackupOptions, TableMode,
};
use crate::events::{JobPhase, ProgressEvent};
use crate::exec::{ChildHandle, ToolCommand, wait_checked};
use crate::job::JobContext;
use crate::manifest::{BackupManifest, MANIFEST_VERSION, sha256_file};
use crate::tools::{
    CompatibilityVerdict, MountMode, ResolvedTool, Tool, Version, check_pg_dump_compatibility,
};
use crate::types::Engine;

/// How often to report the growing artifact size.
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

struct DumpPlan {
    pg_dump: ResolvedTool,
    pg_dumpall: Option<ResolvedTool>,
    endpoint: Endpoint,
    database: String,
    options: PostgresBackupOptions,
    /// Tables whose structure travels but whose rows do not.
    schema_only: Vec<String>,
    /// Tables omitted entirely.
    excluded: Vec<String>,
    artifact: PathBuf,
    globals_path: Option<PathBuf>,
    /// Public keys to encrypt to. Empty means the artifact is not encrypted.
    recipients: Vec<String>,
}

enum DumpProgress {
    Phase(JobPhase, String),
    Size(u64),
}

/// Run a PostgreSQL backup.
pub async fn run_postgres_backup(
    run: BackupRun<'_>,
    ctx: &JobContext,
) -> Result<PathBuf, BackupError> {
    let BackupRun {
        profile,
        request,
        endpoint,
        server_version,
        recipients,
        source_row_counts,
        tools,
    } = run;

    request.validate(profile)?;

    let EngineBackupOptions::Postgres(options) = &request.engine else {
        return Err(BackupError::EngineMismatch {
            profile: profile.engine,
            options: request.engine.engine(),
        });
    };

    let pg_dump = ResolvedTool::resolve(
        Tool::PgDump,
        tools,
        profile.tool_overrides.pg_dump.as_deref(),
    )
    .ok_or_else(|| BackupError::ToolMissing {
        tool: "pg_dump".into(),
    })?;

    // Inside a container, loopback is the container rather than the host that
    // holds the tunnel's local end. See `ToolSource::rewrite_host`.
    let endpoint = Endpoint {
        host: tools.rewrite_host(&endpoint.host),
        ..endpoint
    };

    let tool_version = pg_dump.probe_version();

    // pg_dump cannot read a server newer than itself, and the failure lands
    // mid-dump with a confusing message. Check before opening anything.
    if let (Some(client), Some(server)) = (tool_version, Version::parse_first(&server_version)) {
        match check_pg_dump_compatibility(client, server) {
            CompatibilityVerdict::Blocked(reason) => {
                return Err(BackupError::Invalid(reason));
            }
            CompatibilityVerdict::Warn(reason) => {
                ctx.emit_warn(JobPhase::Initializing, reason).await
            }
            CompatibilityVerdict::Ok => {}
        }
    }

    let schema_only: Vec<String> = request
        .common
        .selections
        .iter()
        .filter(|s| s.mode == TableMode::SchemaOnly)
        .map(|s| s.name.clone())
        .collect();
    let excluded: Vec<String> = request
        .common
        .selections
        .iter()
        .filter(|s| s.mode == TableMode::Exclude)
        .map(|s| s.name.clone())
        .collect();

    std::fs::create_dir_all(&request.common.output_dir)
        .map_err(|e| BackupError::Io(format!("could not create the output directory: {e}")))?;

    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    let filename = format!(
        "{}_{}{}",
        request.common.database,
        timestamp,
        artifact_suffix(options.format)
    );
    let artifact = request.common.output_dir.join(&filename);

    let globals_path = options.include_globals.then(|| {
        request
            .common
            .output_dir
            .join(format!("{filename}.globals.sql"))
    });

    let pg_dumpall = if options.include_globals {
        let found = ResolvedTool::resolve(
            Tool::PgDumpall,
            tools,
            profile.tool_overrides.pg_dumpall.as_deref(),
        );
        if found.is_none() {
            ctx.emit_warn(
                JobPhase::Initializing,
                "pg_dumpall not found; roles and globals will not be captured",
            )
            .await;
        }
        found
    } else {
        None
    };

    ctx.emit(
        JobPhase::Initializing,
        format!(
            "pg_dump {} ({:?} format) → {}",
            tool_version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".into()),
            options.format,
            artifact.display()
        ),
    )
    .await;

    let plan = DumpPlan {
        pg_dump,
        pg_dumpall,
        endpoint,
        database: request.common.database.clone(),
        options: options.clone(),
        schema_only,
        excluded,
        artifact: artifact.clone(),
        globals_path: globals_path.clone(),
        recipients: recipients.to_vec(),
    };

    let current_child: Arc<Mutex<Option<ChildHandle>>> = Arc::new(Mutex::new(None));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DumpProgress>();

    let forward_ctx = ctx.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(update) = rx.recv().await {
            match update {
                DumpProgress::Phase(phase, message) => forward_ctx.emit(phase, message).await,
                DumpProgress::Size(bytes) => {
                    let event =
                        ProgressEvent::new(forward_ctx.job_id, JobPhase::DumpData, "dumping")
                            .with_bytes(bytes);
                    forward_ctx.emit_event(event).await;
                }
            }
        }
    });

    let cancel = ctx.cancel_token();
    let killer_child = current_child.clone();
    let killer = tokio::spawn(async move {
        cancel.cancelled().await;
        if let Some(child) = killer_child.lock().await.as_mut() {
            child.kill_group();
        }
    });

    let worker_cancel = ctx.cancel_token();
    let worker_child = current_child.clone();
    let result =
        tokio::task::spawn_blocking(move || run_dump(plan, tx, worker_child, worker_cancel))
            .await
            .map_err(|e| BackupError::Io(format!("dump worker panicked: {e}")))?;

    killer.abort();
    let _ = forwarder.await;

    let total_bytes = match result {
        Ok(bytes) => bytes,
        Err(e) => {
            // A partial archive is worse than none: pg_restore would accept it
            // and load a subset without complaint.
            remove_artifact(&artifact);
            if let Some(g) = &globals_path {
                let _ = std::fs::remove_file(g);
            }
            return Err(e);
        }
    };

    ctx.bail_if_cancelled()
        .map_err(|_| BackupError::Cancelled)?;

    // A directory archive has no single file to hash; hashing its manifest of
    // parts is not equivalent, so record no checksum rather than a misleading
    // one.
    let sha256 = if options.format == PgDumpFormat::Directory {
        String::new()
    } else {
        ctx.emit(JobPhase::Compress, "computing checksum").await;
        sha256_file(&artifact)
            .map_err(|e| BackupError::Io(format!("could not hash the artifact: {e}")))?
    };

    let manifest = BackupManifest {
        manifest_version: MANIFEST_VERSION,
        id: Uuid::new_v4(),
        source_profile_id: profile.id,
        source_profile_name: profile.name.clone(),
        engine: Engine::Postgres,
        server_version,
        dump_tool: "pg_dump".into(),
        dump_tool_version: tool_version
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".into()),
        database: request.common.database.clone(),
        created_at: Utc::now(),
        format: options.format.artifact_format(),
        tables: request
            .common
            .included_tables()
            .into_iter()
            .map(|s| s.name.clone())
            .collect(),
        tables_with_data: request
            .common
            .tables_with_data()
            .into_iter()
            .map(|s| s.name.clone())
            .collect(),
        source_row_counts: source_row_counts.clone(),
        options: serde_json::to_value(&request.engine).unwrap_or(serde_json::Value::Null),
        artifact_filename: filename,
        size_bytes: total_bytes,
        sha256,
        encrypted: !recipients.is_empty(),
        encryption_recipients: recipients.to_vec(),
    };

    manifest
        .write(&artifact)
        .map_err(|e| BackupError::Io(format!("could not write the manifest: {e}")))?;

    ctx.emit(
        JobPhase::Done,
        format!(
            "backup complete: {} ({total_bytes} bytes)",
            artifact.display()
        ),
    )
    .await;

    Ok(artifact)
}

const fn artifact_suffix(format: PgDumpFormat) -> &'static str {
    match format {
        PgDumpFormat::Custom => ".dump",
        PgDumpFormat::Directory => ".dumpdir",
        PgDumpFormat::Plain => ".sql.gz",
    }
}

fn remove_artifact(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Total size of an artifact, walking a directory archive if needed.
fn artifact_size(path: &Path) -> u64 {
    if path.is_dir() {
        std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
    } else {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }
}

fn run_dump(
    plan: DumpPlan,
    tx: UnboundedSender<DumpProgress>,
    current_child: Arc<Mutex<Option<ChildHandle>>>,
    cancel: CancellationToken,
) -> Result<u64, BackupError> {
    let _ = tx.send(DumpProgress::Phase(
        JobPhase::DumpSchema,
        format!("dumping {}", plan.database),
    ));

    // Custom and directory archives are written by pg_dump itself, so the
    // destination has to exist from the tool's point of view. Containerised,
    // that is not this machine's path: without the bind mount pg_dump writes a
    // perfectly good archive into a container that is then thrown away, and the
    // failure surfaces as a missing file well after the dump reported success.
    let mut pg_dump = plan.pg_dump.clone();
    let artifact_arg = if plan.options.format == PgDumpFormat::Plain {
        None
    } else {
        let dir = plan.artifact.parent().unwrap_or_else(|| Path::new("."));
        let name = plan
            .artifact
            .file_name()
            .ok_or_else(|| BackupError::Io("artifact has no file name".into()))?;
        let mounted = pg_dump
            .mount(dir, MountMode::ReadWrite)
            .map_err(|e| BackupError::Io(e.to_string()))?;
        Some(mounted.join(name))
    };

    let cmd = dump_command(&pg_dump, &plan, artifact_arg.as_deref());
    let _ = tx.send(DumpProgress::Phase(JobPhase::DumpSchema, cmd.display()));

    match plan.options.format {
        // pg_dump writes the archive itself, so nothing is piped.
        PgDumpFormat::Custom | PgDumpFormat::Directory => {
            run_and_watch(&cmd, &plan.artifact, &tx, &current_child, &cancel)?;
        }
        // Plain SQL is compressed on the way out, the same as MySQL.
        PgDumpFormat::Plain => {
            let file = std::fs::File::create(&plan.artifact)
                .map_err(|e| BackupError::Io(format!("could not create the artifact: {e}")))?;
            restrict_permissions(&plan.artifact);

            let mut writer =
                crate::crypto::ArtifactSink::new(std::io::BufWriter::new(file), &plan.recipients)
                    .map_err(|e| BackupError::Io(e.to_string()))?;

            let (child, stdout, stderr) = cmd.spawn_streaming()?;
            {
                *current_child.blocking_lock() = Some(child);
            }

            let mut reader = std::io::BufReader::with_capacity(1 << 16, stdout);
            std::io::copy(&mut reader, &mut writer)
                .map_err(|e| BackupError::Io(format!("streaming the dump failed: {e}")))?;

            let child = current_child.blocking_lock().take().expect("stored");
            match wait_checked(child, stderr, &cancel) {
                Ok(_) => {}
                Err(crate::exec::ExecError::Cancelled { .. }) => {
                    return Err(BackupError::Cancelled);
                }
                Err(e) => return Err(e.into()),
            }

            let buf = writer
                .finish()
                .map_err(|e| BackupError::Io(format!("could not finish compression: {e}")))?;
            buf.into_inner()
                .map_err(|e| BackupError::Io(format!("could not flush the artifact: {e}")))?
                .sync_all()
                .map_err(|e| BackupError::Io(format!("could not sync the artifact: {e}")))?;
        }
    }

    // Roles and tablespaces live outside any one database, so they need a
    // separate tool and land in a sidecar file.
    if let (Some(pg_dumpall), Some(globals_path)) = (&plan.pg_dumpall, &plan.globals_path) {
        let _ = tx.send(DumpProgress::Phase(
            JobPhase::DumpSchema,
            "dumping roles and globals".into(),
        ));
        let cmd = globals_command(&plan, pg_dumpall);
        let (child, stdout, stderr) = cmd.spawn_streaming()?;
        {
            *current_child.blocking_lock() = Some(child);
        }

        let mut file = std::fs::File::create(globals_path)
            .map_err(|e| BackupError::Io(format!("could not create the globals file: {e}")))?;
        restrict_permissions(globals_path);
        let mut reader = std::io::BufReader::new(stdout);
        std::io::copy(&mut reader, &mut file)
            .map_err(|e| BackupError::Io(format!("writing globals failed: {e}")))?;

        let child = current_child.blocking_lock().take().expect("stored");
        if let Err(e) = wait_checked(child, stderr, &cancel) {
            // Globals are supplementary; losing them should not discard a good
            // dump, but the user has to know.
            let _ = tx.send(DumpProgress::Phase(
                JobPhase::DumpSchema,
                format!("globals dump failed and was skipped: {e}"),
            ));
            let _ = std::fs::remove_file(globals_path);
        }
    }

    Ok(artifact_size(&plan.artifact))
}

/// Run a command that writes its own output file, reporting size as it grows.
fn run_and_watch(
    cmd: &ToolCommand,
    artifact: &Path,
    tx: &UnboundedSender<DumpProgress>,
    current_child: &Arc<Mutex<Option<ChildHandle>>>,
    cancel: &CancellationToken,
) -> Result<(), BackupError> {
    let (child, _stdout, stderr) = cmd.spawn_streaming()?;
    let pid = child.id();
    {
        *current_child.blocking_lock() = Some(child);
    }

    // pg_dump gives no progress on stdout when writing its own file, so the
    // artifact's growth is the only signal available.
    let watching = artifact.to_path_buf();
    let watch_tx = tx.clone();
    let watcher = std::thread::spawn(move || {
        loop {
            std::thread::sleep(PROGRESS_INTERVAL);
            // Sending fails once the receiver is dropped, which is the signal
            // to stop watching.
            if watch_tx
                .send(DumpProgress::Size(artifact_size(&watching)))
                .is_err()
            {
                break;
            }
            #[cfg(unix)]
            if unsafe { libc::kill(pid as i32, 0) } != 0 {
                break;
            }
            #[cfg(not(unix))]
            let _ = pid;
        }
    });

    let child = current_child.blocking_lock().take().expect("stored");
    let outcome = wait_checked(child, stderr, cancel);
    drop(watcher);

    match outcome {
        Ok(_) => Ok(()),
        Err(crate::exec::ExecError::Cancelled { .. }) => Err(BackupError::Cancelled),
        Err(e) => Err(e.into()),
    }
}

fn connection_args(endpoint: &Endpoint) -> Vec<String> {
    vec![
        format!("--host={}", endpoint.host),
        format!("--port={}", endpoint.port),
        format!("--username={}", endpoint.user),
        // Prompting would hang a background job forever.
        "--no-password".to_string(),
    ]
}

fn with_password(mut cmd: ToolCommand, endpoint: &Endpoint) -> ToolCommand {
    if let Some(pw) = &endpoint.password {
        cmd = cmd.secret_env("PGPASSWORD", pw.clone());
    }
    cmd
}

fn dump_command(pg_dump: &ResolvedTool, plan: &DumpPlan, artifact: Option<&Path>) -> ToolCommand {
    let o = &plan.options;
    let mut args = connection_args(&plan.endpoint);

    args.push(o.format.flag().to_string());

    if o.no_owner {
        args.push("--no-owner".into());
    }
    if o.no_privileges {
        args.push("--no-privileges".into());
    }
    if o.blobs {
        args.push("--large-objects".into());
    }
    if o.serializable_deferrable {
        args.push("--serializable-deferrable".into());
    }
    for schema in &o.schemas {
        args.push(format!("--schema={schema}"));
    }
    // Parallel dump is only valid for the directory format; `validate` has
    // already rejected the other combinations.
    if let Some(jobs) = o.parallel_jobs
        && o.format.supports_parallel_dump()
    {
        args.push(format!("--jobs={jobs}"));
    }

    // Structure without rows — this is what "schema only" means for a table.
    for table in &plan.schema_only {
        args.push(format!("--exclude-table-data={table}"));
    }
    for table in &plan.excluded {
        args.push(format!("--exclude-table={table}"));
    }

    args.extend(o.extra_flags.clone());

    // Custom and directory archives are written by pg_dump; plain goes to
    // stdout so it can be compressed on the way past.
    if let Some(path) = artifact {
        args.push(format!("--file={}", path.display()));
    }

    args.push(format!("--dbname={}", plan.database));

    with_password(pg_dump.command().args(args), &plan.endpoint)
}

fn globals_command(plan: &DumpPlan, pg_dumpall: &ResolvedTool) -> ToolCommand {
    let mut args = connection_args(&plan.endpoint);
    args.push("--globals-only".into());
    if plan.options.no_privileges {
        args.push("--no-privileges".into());
    }

    with_password(pg_dumpall.command().args(args), &plan.endpoint)
}

fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolSource;
    use secrecy::SecretString;

    /// A tool pinned to a path that exists, so the fixture never depends on
    /// whether PostgreSQL happens to be installed on the machine running the
    /// tests. Only the rendered command line is under test here.
    fn local_tool(tool: Tool) -> ResolvedTool {
        ResolvedTool::resolve(tool, &ToolSource::Local, Some("/bin/sh")).expect("/bin/sh exists")
    }

    fn plan(format: PgDumpFormat) -> DumpPlan {
        DumpPlan {
            recipients: Vec::new(),
            pg_dump: local_tool(Tool::PgDump),
            pg_dumpall: Some(local_tool(Tool::PgDumpall)),
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: 15432,
                user: "dbsync".into(),
                password: Some(SecretString::from("hunter2")),
            },
            database: "app".into(),
            options: PostgresBackupOptions {
                format,
                ..Default::default()
            },
            schema_only: vec!["public.audit_log".into()],
            excluded: vec!["public.temp".into()],
            artifact: PathBuf::from("/tmp/app.dump"),
            globals_path: None,
        }
    }

    #[test]
    fn the_password_never_appears_in_the_command_line() {
        let rendered = dump_command(
            &local_tool(Tool::PgDump),
            &plan(PgDumpFormat::Custom),
            Some(Path::new("/tmp/app.dump")),
        )
        .display();
        assert!(!rendered.contains("hunter2"));
        assert!(
            rendered.contains("--no-password"),
            "a background job must never wait on a password prompt"
        );
    }

    #[test]
    fn schema_only_tables_keep_structure_but_lose_rows() {
        let rendered = dump_command(
            &local_tool(Tool::PgDump),
            &plan(PgDumpFormat::Custom),
            Some(Path::new("/tmp/app.dump")),
        )
        .display();
        assert!(
            rendered.contains("--exclude-table-data=public.audit_log"),
            "schema-only must exclude data, not the table"
        );
        assert!(!rendered.contains("--exclude-table=public.audit_log"));
    }

    #[test]
    fn excluded_tables_are_dropped_entirely() {
        let rendered = dump_command(
            &local_tool(Tool::PgDump),
            &plan(PgDumpFormat::Custom),
            Some(Path::new("/tmp/app.dump")),
        )
        .display();
        assert!(rendered.contains("--exclude-table=public.temp"));
    }

    #[test]
    fn portable_defaults_are_applied() {
        let rendered = dump_command(
            &local_tool(Tool::PgDump),
            &plan(PgDumpFormat::Custom),
            Some(Path::new("/tmp/app.dump")),
        )
        .display();
        assert!(rendered.contains("--no-owner"));
        assert!(rendered.contains("--no-privileges"));
    }

    #[test]
    fn archive_formats_write_their_own_file() {
        for format in [PgDumpFormat::Custom, PgDumpFormat::Directory] {
            let rendered = dump_command(
                &local_tool(Tool::PgDump),
                &plan(format),
                Some(Path::new("/tmp/app.dump")),
            )
            .display();
            assert!(rendered.contains("--file=/tmp/app.dump"), "{format:?}");
        }
    }

    #[test]
    fn plain_format_streams_to_stdout_for_compression() {
        let rendered =
            dump_command(&local_tool(Tool::PgDump), &plan(PgDumpFormat::Plain), None).display();
        assert!(
            !rendered.contains("--file="),
            "plain output is piped through gzip, not written directly"
        );
        assert!(rendered.contains("-Fp"));
    }

    #[test]
    fn parallel_jobs_only_apply_to_the_directory_format() {
        let mut p = plan(PgDumpFormat::Custom);
        p.options.parallel_jobs = Some(4);
        assert!(
            !dump_command(&local_tool(Tool::PgDump), &p, None)
                .display()
                .contains("--jobs"),
            "pg_dump rejects -j for the custom format"
        );

        let mut p = plan(PgDumpFormat::Directory);
        p.options.parallel_jobs = Some(4);
        assert!(
            dump_command(&local_tool(Tool::PgDump), &p, None)
                .display()
                .contains("--jobs=4")
        );
    }

    #[test]
    fn schema_restriction_is_passed_through() {
        let mut p = plan(PgDumpFormat::Custom);
        p.options.schemas = vec!["public".into(), "reporting".into()];
        let rendered = dump_command(&local_tool(Tool::PgDump), &p, None).display();
        assert!(rendered.contains("--schema=public"));
        assert!(rendered.contains("--schema=reporting"));
    }

    #[test]
    fn artifact_suffixes_match_their_format() {
        assert_eq!(artifact_suffix(PgDumpFormat::Custom), ".dump");
        assert_eq!(artifact_suffix(PgDumpFormat::Directory), ".dumpdir");
        assert_eq!(artifact_suffix(PgDumpFormat::Plain), ".sql.gz");
    }

    #[test]
    fn globals_command_asks_only_for_globals() {
        let p = plan(PgDumpFormat::Custom);
        let rendered = globals_command(&p, &local_tool(Tool::PgDumpall)).display();
        assert!(rendered.contains("--globals-only"));
        assert!(rendered.contains("--no-password"));
    }

    #[test]
    fn artifact_size_walks_a_directory_archive() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("app.dumpdir");
        std::fs::create_dir(&archive).unwrap();
        std::fs::write(archive.join("toc.dat"), b"12345").unwrap();
        std::fs::write(archive.join("1.dat"), b"678").unwrap();

        assert_eq!(artifact_size(&archive), 8);
    }

    #[test]
    fn artifact_size_of_a_missing_path_is_zero() {
        assert_eq!(artifact_size(Path::new("/nonexistent/thing")), 0);
    }
}
