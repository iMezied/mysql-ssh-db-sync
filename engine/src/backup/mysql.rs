//! MySQL backup execution.
//!
//! Three passes: schema for every table, then data for the selected subset,
//! then triggers. The first two are the shape the Bash predecessor
//! established — every table's structure reaches the destination while only
//! the selected rows travel.
//!
//! Triggers come *last*, and that ordering is load-bearing. Created before the
//! data, an `AFTER INSERT` trigger fires once per restored row and writes rows
//! nobody asked for: restoring a fixture with two orders and two audit entries
//! produced four audit entries. `mysqldump` emits triggers after a table's data
//! for exactly this reason. Verification caught this.
//!
//! The stream goes `mysqldump` → DEFINER filter → gzip → file, inline. No
//! uncompressed intermediate is ever written: on a large database that would
//! double the disk requirement and add a second full pass.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use secrecy::SecretString;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{BackupError, BackupRequest, EngineBackupOptions, MysqlBackupOptions, TableSelection};
use crate::definer;
use crate::events::{JobPhase, ProgressEvent};
use crate::exec::{ChildHandle, ToolCommand, find_tool, probe_version, wait_checked};
use crate::job::JobContext;
use crate::manifest::{ArtifactFormat, BackupManifest, MANIFEST_VERSION, sha256_file};
use crate::profile::ConnectionProfile;
use crate::tools::{Version, mysql_needs_column_statistics_flag};
use crate::types::Engine;

/// Where the dump is being sent, once a tunnel (if any) is up.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<SecretString>,
}

/// Everything the blocking dump worker needs.
struct DumpPlan {
    mysqldump: PathBuf,
    endpoint: Endpoint,
    database: String,
    options: MysqlBackupOptions,
    schema_tables: Vec<String>,
    data_tables: Vec<TableSelection>,
    /// Emit triggers in a final pass, after the data.
    include_triggers: bool,
    artifact: PathBuf,
    column_statistics_off: bool,
    /// Public keys to encrypt to. Empty means the artifact is not encrypted.
    recipients: Vec<String>,
}

/// Progress reported from the blocking worker.
enum DumpProgress {
    Phase(JobPhase, String),
    Table {
        index: usize,
        total: usize,
        name: String,
        bytes: u64,
    },
    Warn(String),
}

/// Run a MySQL backup.
///
/// `endpoint` is already resolved — with a tunnel that is its local end, which
/// is why this takes an endpoint rather than reaching for the profile's host.
pub async fn run_mysql_backup(
    profile: &ConnectionProfile,
    request: &BackupRequest,
    endpoint: Endpoint,
    server_version: String,
    // Public keys to encrypt the artifact to. Empty means no encryption.
    recipients: &[String],
    // Exact source row counts, when the request asked for them. Empty
    // otherwise — see `CommonBackupOptions::record_row_counts`.
    source_row_counts: &std::collections::BTreeMap<String, u64>,
    ctx: &JobContext,
) -> Result<PathBuf, BackupError> {
    request.validate(profile)?;

    let EngineBackupOptions::Mysql(options) = &request.engine else {
        return Err(BackupError::EngineMismatch {
            profile: profile.engine,
            options: request.engine.engine(),
        });
    };

    let mysqldump = find_tool("mysqldump", profile.tool_overrides.mysqldump.as_deref())
        .ok_or_else(|| BackupError::ToolMissing {
            tool: "mysqldump".into(),
        })?;

    let tool_version = probe_version(mysqldump.as_os_str());
    let server = Version::parse_first(&server_version);

    // An 8.x client against a pre-8.0 server queries a table that does not
    // exist unless this is disabled. Detecting it beats a confusing mid-dump
    // failure.
    let column_statistics_off = match (tool_version, server) {
        (Some(c), Some(s)) => mysql_needs_column_statistics_flag(c, s),
        _ => options.disable_column_statistics,
    };

    ctx.emit(
        JobPhase::Initializing,
        format!(
            "mysqldump {} → {}",
            tool_version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".into()),
            request.common.output_dir.display()
        ),
    )
    .await;

    let data_tables: Vec<TableSelection> = request
        .common
        .tables_with_data()
        .into_iter()
        .cloned()
        .collect();
    let schema_tables: Vec<String> = request
        .common
        .included_tables()
        .into_iter()
        .map(|s| s.name.clone())
        .collect();

    std::fs::create_dir_all(&request.common.output_dir)
        .map_err(|e| BackupError::Io(format!("could not create the output directory: {e}")))?;

    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    // The extension states what the file is. A `.sql.gz` that is actually
    // ciphertext produces a baffling gzip error the first time someone reaches
    // for it outside this app.
    let suffix = if recipients.is_empty() {
        "sql.gz"
    } else {
        "sql.gz.age"
    };
    let filename = format!("{}_{}.{suffix}", request.common.database, timestamp);
    let artifact = request.common.output_dir.join(&filename);

    let plan = DumpPlan {
        mysqldump,
        endpoint,
        database: request.common.database.clone(),
        options: options.clone(),
        schema_tables: schema_tables.clone(),
        data_tables: data_tables.clone(),
        include_triggers: options.triggers,
        artifact: artifact.clone(),
        column_statistics_off,
        recipients: recipients.to_vec(),
    };

    // The currently-running child, so cancellation can reach into the blocking
    // worker and stop it.
    let current_child: Arc<Mutex<Option<ChildHandle>>> = Arc::new(Mutex::new(None));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DumpProgress>();

    // Forward worker progress onto the job's event stream.
    let forward_ctx = ctx.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(update) = rx.recv().await {
            match update {
                DumpProgress::Phase(phase, message) => forward_ctx.emit(phase, message).await,
                DumpProgress::Table {
                    index,
                    total,
                    name,
                    bytes,
                } => {
                    let event =
                        ProgressEvent::new(forward_ctx.job_id, JobPhase::DumpData, "dumping table")
                            .with_table(name)
                            .with_progress(index as u64, total as u64)
                            .with_rows(bytes);
                    forward_ctx.emit_event(event).await;
                }
                DumpProgress::Warn(message) => {
                    forward_ctx.emit_warn(JobPhase::DumpData, message).await
                }
            }
        }
    });

    // Watch for cancellation and kill whatever child is running.
    let cancel = ctx.cancel_token();
    let killer_child = current_child.clone();
    let killer = tokio::spawn(async move {
        cancel.cancelled().await;
        if let Some(child) = killer_child.lock().await.as_mut() {
            tracing::info!("cancelling {}", child.program());
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
            // A partial artifact is worse than none: it looks restorable.
            let _ = std::fs::remove_file(&artifact);
            return Err(e);
        }
    };

    ctx.bail_if_cancelled()
        .map_err(|_| BackupError::Cancelled)?;

    ctx.emit(JobPhase::Compress, "computing checksum").await;
    let sha256 = sha256_file(&artifact)
        .map_err(|e| BackupError::Io(format!("could not hash the artifact: {e}")))?;

    let manifest = BackupManifest {
        manifest_version: MANIFEST_VERSION,
        id: Uuid::new_v4(),
        source_profile_id: profile.id,
        source_profile_name: profile.name.clone(),
        engine: Engine::Mysql,
        server_version,
        dump_tool: "mysqldump".into(),
        dump_tool_version: tool_version
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".into()),
        database: request.common.database.clone(),
        created_at: Utc::now(),
        format: ArtifactFormat::SqlGz,
        tables: schema_tables,
        tables_with_data: data_tables.iter().map(|t| t.name.clone()).collect(),
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
            "backup complete: {} ({} bytes)",
            artifact.display(),
            total_bytes
        ),
    )
    .await;

    Ok(artifact)
}

/// The blocking half: run the dumps and stream them into the artifact.
fn run_dump(
    plan: DumpPlan,
    tx: UnboundedSender<DumpProgress>,
    current_child: Arc<Mutex<Option<ChildHandle>>>,
    cancel: CancellationToken,
) -> Result<u64, BackupError> {
    let file = std::fs::File::create(&plan.artifact)
        .map_err(|e| BackupError::Io(format!("could not create the artifact: {e}")))?;
    restrict_permissions(&plan.artifact);

    let mut writer =
        crate::crypto::ArtifactSink::new(std::io::BufWriter::new(file), &plan.recipients)
            .map_err(|e| BackupError::Io(e.to_string()))?;

    let _ = tx.send(DumpProgress::Phase(
        JobPhase::DumpSchema,
        format!("dumping schema for {} tables", plan.schema_tables.len()),
    ));

    // ── Schema pass ─────────────────────────────────────────────────────
    let schema_cmd = schema_command(&plan);
    let _ = tx.send(DumpProgress::Phase(
        JobPhase::DumpSchema,
        schema_cmd.display(),
    ));
    let stripped = stream_command(
        &schema_cmd,
        &mut writer,
        &current_child,
        &cancel,
        plan.options.strip_definer,
    )?;
    if stripped > 0 {
        let _ = tx.send(DumpProgress::Phase(
            JobPhase::DumpSchema,
            format!("stripped DEFINER clauses from {stripped} lines"),
        ));
    }

    // ── Data pass ───────────────────────────────────────────────────────
    let total = plan.data_tables.len();
    let _ = tx.send(DumpProgress::Phase(
        JobPhase::DumpData,
        format!("dumping data for {total} tables"),
    ));

    for (index, table) in plan.data_tables.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(BackupError::Cancelled);
        }

        let cmd = data_command(&plan, table);
        match stream_command(&cmd, &mut writer, &current_child, &cancel, false) {
            Ok(_) => {}
            Err(BackupError::Cancelled) => return Err(BackupError::Cancelled),
            // One unreadable table should not throw away the whole dump, but it
            // must be loud — the predecessor swallowed these entirely.
            Err(e) => {
                let _ = tx.send(DumpProgress::Warn(format!(
                    "table {} failed and was skipped: {e}",
                    table.name
                )));
                continue;
            }
        }

        // Stat the path rather than reaching through the writer: the number of
        // layers now depends on whether encryption is on, and progress
        // reporting has no business knowing that.
        let bytes = std::fs::metadata(&plan.artifact)
            .map(|m| m.len())
            .unwrap_or(0);
        let _ = tx.send(DumpProgress::Table {
            index: index + 1,
            total,
            name: table.name.clone(),
            bytes,
        });
    }

    // ── Trigger pass ────────────────────────────────────────────────────
    //
    // After the data, so restoring rows cannot fire them.
    if plan.include_triggers {
        if cancel.is_cancelled() {
            return Err(BackupError::Cancelled);
        }
        let _ = tx.send(DumpProgress::Phase(
            JobPhase::DumpSchema,
            "dumping triggers".into(),
        ));
        let cmd = trigger_command(&plan);
        let stripped = stream_command(
            &cmd,
            &mut writer,
            &current_child,
            &cancel,
            plan.options.strip_definer,
        )?;
        if stripped > 0 {
            let _ = tx.send(DumpProgress::Phase(
                JobPhase::DumpSchema,
                format!("stripped DEFINER clauses from {stripped} trigger lines"),
            ));
        }
    }

    // Finishing matters more than it looks: gzip writes its CRC and length
    // here, and age its final authenticated chunk. Dropping the writer instead
    // leaves a file that looks complete and is not.
    let buf = writer
        .finish()
        .map_err(|e| BackupError::Io(format!("could not finish the artifact stream: {e}")))?;
    let file = buf
        .into_inner()
        .map_err(|e| BackupError::Io(format!("could not flush the artifact: {e}")))?;
    file.sync_all()
        .map_err(|e| BackupError::Io(format!("could not sync the artifact: {e}")))?;

    let size = std::fs::metadata(&plan.artifact)
        .map(|m| m.len())
        .map_err(|e| BackupError::Io(format!("could not stat the artifact: {e}")))?;

    Ok(size)
}

/// Run one dump command, streaming its stdout into `writer`.
///
/// Returns the number of lines the DEFINER filter modified.
fn stream_command<W: Write>(
    cmd: &ToolCommand,
    writer: &mut W,
    current_child: &Arc<Mutex<Option<ChildHandle>>>,
    cancel: &CancellationToken,
    strip_definer: bool,
) -> Result<u64, BackupError> {
    let (child, stdout, stderr) = cmd.spawn_streaming()?;

    // Publish the child so cancellation can kill it mid-stream.
    {
        let mut slot = current_child.blocking_lock();
        *slot = Some(child);
    }

    let reader = std::io::BufReader::with_capacity(1 << 16, stdout);
    let stripped = if strip_definer {
        definer::strip_definers_stream(reader, writer)
            .map_err(|e| BackupError::Io(format!("streaming the dump failed: {e}")))?
    } else {
        let mut reader = reader;
        std::io::copy(&mut reader, writer)
            .map_err(|e| BackupError::Io(format!("streaming the dump failed: {e}")))?;
        0
    };

    let child = current_child
        .blocking_lock()
        .take()
        .expect("child was stored before streaming");

    match wait_checked(child, stderr, cancel) {
        Ok(warnings) => {
            for line in warnings {
                tracing::debug!("mysqldump: {line}");
            }
            Ok(stripped)
        }
        Err(crate::exec::ExecError::Cancelled { .. }) => Err(BackupError::Cancelled),
        Err(e) => Err(e.into()),
    }
}

/// Base connection arguments shared by every pass.
fn connection_args(plan: &DumpPlan) -> Vec<String> {
    vec![
        format!("--host={}", plan.endpoint.host),
        format!("--port={}", plan.endpoint.port),
        format!("--user={}", plan.endpoint.user),
        // Never `-p<password>`: argv is world-readable via `ps`.
        "--protocol=TCP".to_string(),
    ]
}

fn with_password(mut cmd: ToolCommand, endpoint: &Endpoint) -> ToolCommand {
    if let Some(pw) = &endpoint.password {
        cmd = cmd.secret_env("MYSQL_PWD", pw.clone());
    }
    cmd
}

fn schema_command(plan: &DumpPlan) -> ToolCommand {
    let o = &plan.options;
    let mut args = connection_args(plan);

    args.push("--no-data".into());
    if o.single_transaction {
        args.push("--single-transaction".into());
    }
    if o.add_drop_table {
        args.push("--add-drop-table".into());
    }
    if o.set_gtid_purged_off {
        args.push("--set-gtid-purged=OFF".into());
    }
    if o.routines {
        args.push("--routines".into());
    }
    // Triggers are deliberately excluded here and emitted after the data; see
    // the module docs.
    args.push("--skip-triggers".into());
    if o.events {
        args.push("--events".into());
    }
    if plan.column_statistics_off {
        args.push("--column-statistics=0".into());
    }
    args.push(format!(
        "--default-character-set={}",
        o.default_character_set
    ));
    args.extend(o.extra_flags.clone());

    // `--databases` is deliberately not used: it injects CREATE DATABASE and
    // USE, which would redirect the restore into the source's database name
    // instead of the new one.
    args.push(plan.database.clone());

    with_password(
        ToolCommand::new(plan.mysqldump.display().to_string()).args(args),
        &plan.endpoint,
    )
}

fn data_command(plan: &DumpPlan, table: &TableSelection) -> ToolCommand {
    let o = &plan.options;
    let mut args = connection_args(plan);

    args.push("--no-create-info".into());
    if o.single_transaction {
        args.push("--single-transaction".into());
    }
    // Triggers came with the schema pass; repeating them here would duplicate.
    args.push("--skip-triggers".into());
    if o.extended_insert {
        args.push("--extended-insert".into());
    }
    if o.hex_blob {
        args.push("--hex-blob".into());
    }
    if o.set_gtid_purged_off {
        args.push("--set-gtid-purged=OFF".into());
    }
    if plan.column_statistics_off {
        args.push("--column-statistics=0".into());
    }
    args.push(format!(
        "--default-character-set={}",
        o.default_character_set
    ));
    if let Some(filter) = &table.where_filter {
        args.push(format!("--where={filter}"));
    }
    args.extend(o.extra_flags.clone());

    args.push(plan.database.clone());
    args.push(table.name.clone());

    with_password(
        ToolCommand::new(plan.mysqldump.display().to_string()).args(args),
        &plan.endpoint,
    )
}

/// Dump only the triggers, for the final pass.
fn trigger_command(plan: &DumpPlan) -> ToolCommand {
    let o = &plan.options;
    let mut args = connection_args(plan);

    // No schema, no rows — just the trigger definitions.
    args.push("--no-create-info".into());
    args.push("--no-data".into());
    args.push("--triggers".into());
    args.push("--skip-routines".into());
    args.push("--skip-events".into());
    if o.single_transaction {
        args.push("--single-transaction".into());
    }
    if o.set_gtid_purged_off {
        args.push("--set-gtid-purged=OFF".into());
    }
    if plan.column_statistics_off {
        args.push("--column-statistics=0".into());
    }
    args.push(format!(
        "--default-character-set={}",
        o.default_character_set
    ));
    args.push(plan.database.clone());

    with_password(
        ToolCommand::new(plan.mysqldump.display().to_string()).args(args),
        &plan.endpoint,
    )
}

/// Backups routinely contain production data; keep them owner-readable only.
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
    use crate::backup::{CommonBackupOptions, TableMode};

    fn plan() -> DumpPlan {
        DumpPlan {
            recipients: Vec::new(),
            mysqldump: PathBuf::from("/usr/bin/mysqldump"),
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: 13306,
                user: "root".into(),
                password: Some(SecretString::from("hunter2")),
            },
            database: "app".into(),
            options: MysqlBackupOptions::default(),
            schema_tables: vec!["users".into()],
            data_tables: vec![TableSelection::with_data("orders")],
            artifact: PathBuf::from("/tmp/app.sql.gz"),
            column_statistics_off: false,
            include_triggers: true,
        }
    }

    #[test]
    fn the_password_never_appears_in_the_command_line() {
        let rendered = schema_command(&plan()).display();
        assert!(
            !rendered.contains("hunter2"),
            "password leaked into argv: {rendered}"
        );

        // The dangerous form is short `-p<password>`. Checking for the
        // substring "-p" would also match --port and --protocol, so match
        // whole arguments instead.
        for token in rendered.split(' ') {
            assert!(
                token != "-p" && !(token.starts_with("-p") && !token.starts_with("--")),
                "argument {token:?} looks like an inline password flag"
            );
        }
    }

    #[test]
    fn schema_pass_dumps_no_data_but_keeps_routines() {
        let rendered = schema_command(&plan()).display();
        assert!(rendered.contains("--no-data"));
        assert!(rendered.contains("--routines"));
        assert!(rendered.contains("--events"));
        assert!(rendered.contains("--single-transaction"));
    }

    #[test]
    fn schema_pass_excludes_triggers() {
        // A trigger created before the data fires once per restored row. The
        // fixture's AFTER INSERT trigger turned 2 audit rows into 4 this way.
        let rendered = schema_command(&plan()).display();
        assert!(
            rendered.contains("--skip-triggers"),
            "triggers must not be created before the data is loaded"
        );
    }

    #[test]
    fn triggers_are_emitted_in_their_own_final_pass() {
        let rendered = trigger_command(&plan()).display();
        assert!(rendered.contains("--triggers"));
        assert!(
            rendered.contains("--no-data"),
            "no rows in the trigger pass"
        );
        assert!(
            rendered.contains("--no-create-info"),
            "tables already exist by this point"
        );
        assert!(
            rendered.contains("--skip-routines") && rendered.contains("--skip-events"),
            "routines and events already came with the schema pass"
        );
    }

    #[test]
    fn schema_pass_never_uses_databases_flag() {
        // `--databases` injects CREATE DATABASE and USE, which would send the
        // restore into the source database's name.
        let rendered = schema_command(&plan()).display();
        assert!(!rendered.contains("--databases"));
        assert!(!rendered.contains("--add-drop-database"));
        assert!(rendered.ends_with(" app"), "database is a bare argument");
    }

    #[test]
    fn data_pass_omits_schema_and_triggers() {
        let p = plan();
        let rendered = data_command(&p, &p.data_tables[0]).display();
        assert!(rendered.contains("--no-create-info"));
        assert!(
            rendered.contains("--skip-triggers"),
            "triggers already came with the schema pass"
        );
        assert!(
            rendered.contains("--hex-blob"),
            "binary data corrupts without this"
        );
        assert!(rendered.ends_with(" app orders"));
    }

    #[test]
    fn row_filters_are_passed_through() {
        let mut p = plan();
        p.data_tables[0].where_filter = Some("created_at > '2026-01-01'".into());
        let rendered = data_command(&p, &p.data_tables[0]).display();
        assert!(rendered.contains("--where=created_at > '2026-01-01'"));
    }

    #[test]
    fn column_statistics_flag_is_added_only_when_needed() {
        let mut p = plan();
        assert!(!schema_command(&p).display().contains("--column-statistics"));

        p.column_statistics_off = true;
        assert!(
            schema_command(&p)
                .display()
                .contains("--column-statistics=0"),
            "an 8.x client against a 5.7 server needs this"
        );
    }

    #[test]
    fn extra_flags_reach_the_command() {
        let mut p = plan();
        p.options.extra_flags = vec!["--quick".into()];
        assert!(schema_command(&p).display().contains("--quick"));
    }

    #[test]
    fn disabling_options_removes_their_flags() {
        let mut p = plan();
        p.options.single_transaction = false;
        p.options.routines = false;
        p.options.events = false;

        let rendered = schema_command(&p).display();
        assert!(!rendered.contains("--single-transaction"));
        assert!(!rendered.contains("--routines"));
        assert!(!rendered.contains("--events"));
    }

    #[test]
    fn selections_split_into_schema_and_data_sets() {
        let common = CommonBackupOptions {
            database: "app".into(),
            selections: vec![
                TableSelection::with_data("orders"),
                TableSelection::schema_only("audit_log"),
                TableSelection {
                    name: "temp".into(),
                    mode: TableMode::Exclude,
                    where_filter: None,
                },
            ],
            output_dir: PathBuf::from("/tmp"),
            compress: true,
            encrypt: false,
            record_row_counts: false,
        };

        // Excluded tables appear in neither set.
        assert_eq!(common.tables_with_data().len(), 1);
        assert_eq!(common.included_tables().len(), 2);
    }
}
