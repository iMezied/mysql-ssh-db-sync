//! MongoDB backup execution.
//!
//! One pass, not three. `mongodump --archive` writes a single self-describing
//! stream carrying every collection's documents *and* its indexes and options,
//! so the schema/data/trigger split the MySQL backend needs has no counterpart
//! here — there is no ordering hazard to design around because there is nothing
//! that fires on insert.
//!
//! The stream goes `mongodump → gzip (inside the archive) → age → file`,
//! inline, exactly like the other engines: no uncompressed intermediate, and a
//! single artifact whose checksum this process controls. That last property is
//! the whole reason MongoDB fits this application at all.
//!
//! `--gzip` is `mongodump`'s own compression, applied *within* the archive
//! rather than wrapped around it, so there is no second gzip layer here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{BackupError, BackupRequest, EngineBackupOptions, MongoBackupOptions, TableMode};
use crate::backup::mysql::Endpoint;
use crate::db::MONGO_AUTH_SOURCE;
use crate::events::JobPhase;
use crate::exec::{ChildHandle, ToolCommand, find_tool, probe_version, wait_checked};
use crate::job::JobContext;
use crate::manifest::{ArtifactFormat, BackupManifest, MANIFEST_VERSION, sha256_file};
use crate::profile::ConnectionProfile;
use crate::tools::{CompatibilityVerdict, Version, check_mongodump_compatibility};
use crate::types::Engine;

/// Hand a password to `mongodump`/`mongorestore` without putting it in argv.
///
/// The other engines have an environment variable for this — `MYSQL_PWD`,
/// `PGPASSWORD`. **The MongoDB Database Tools have none.** Omit `--password`
/// and they do not fall back to anything: they prompt on stdin and, with no
/// terminal, fail the connection with an authentication error that says
/// nothing about the real cause. Pass `--password=…` instead and the
/// credential is readable by every user on the machine via `ps`, which is one
/// of the failure modes this project was built to remove.
///
/// `--config` is the mechanism the tools provide for exactly this: a YAML file
/// holding the password, created 0600. The file is created with those
/// permissions rather than chmod'ed afterwards, so it is never briefly
/// world-readable, and the returned handle deletes it on drop — which is why
/// callers must keep it alive until the child has exited.
///
/// `mongorestore` needs this even more than `mongodump` does: its stdin is the
/// archive, so answering a prompt there is not possible at all.
pub(crate) fn password_config(password: &SecretString) -> Result<tempfile::NamedTempFile, String> {
    use std::io::Write as _;

    let mut builder = tempfile::Builder::new();
    builder.prefix("dbsync-mongo-").suffix(".conf");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }

    let mut file = builder
        .tempfile()
        .map_err(|e| format!("could not create the credentials file: {e}"))?;

    writeln!(file, "password: {}", yaml_quote(password.expose_secret()))
        .map_err(|e| format!("could not write the credentials file: {e}"))?;
    file.flush()
        .map_err(|e| format!("could not flush the credentials file: {e}"))?;

    Ok(file)
}

/// Render a string as a YAML double-quoted scalar.
///
/// Double-quoted rather than single-quoted, and the reason is the one property
/// worth having: **the result is always exactly one line.** A single-quoted
/// scalar may legally span lines — YAML folds them — so a password containing a
/// newline would put attacker-chosen text at the start of a line inside a
/// credentials file. It happens to parse as part of the password, but that
/// safety then depends on the reader implementing flow folding correctly, for a
/// file whose other keys include `uri`. Escaping the newline removes the
/// question instead of answering it.
fn yaml_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Everything else below space, escaped by code point. Unlikely in a
            // password, and cheaper to handle than to reason about.
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Everything the blocking dump worker needs.
struct DumpPlan {
    mongodump: PathBuf,
    endpoint: Endpoint,
    database: String,
    options: MongoBackupOptions,
    /// Collections the plan excludes. MongoDB is dumped by exclusion because
    /// `mongodump` takes one database per archive and `--excludeCollection` is
    /// the only per-collection selector that composes with that.
    excluded: Vec<String>,
    compress: bool,
    artifact: PathBuf,
    recipients: Vec<String>,
}

enum DumpProgress {
    Phase(JobPhase, String),
    Warn(String),
}

/// Run a MongoDB backup.
pub async fn run_mongo_backup(
    profile: &ConnectionProfile,
    request: &BackupRequest,
    endpoint: Endpoint,
    server_version: String,
    recipients: &[String],
    source_row_counts: &std::collections::BTreeMap<String, u64>,
    ctx: &JobContext,
) -> Result<PathBuf, BackupError> {
    request.validate(profile)?;

    let EngineBackupOptions::Mongo(options) = &request.engine else {
        return Err(BackupError::EngineMismatch {
            profile: profile.engine,
            options: request.engine.engine(),
        });
    };

    let mongodump = find_tool("mongodump", profile.tool_overrides.mongodump.as_deref())
        .ok_or_else(|| BackupError::ToolMissing {
            tool: "mongodump".into(),
        })?;

    let tool_version = probe_version(mongodump.as_os_str());
    if let (Some(client), Some(server)) = (tool_version, Version::parse_first(&server_version))
        && let CompatibilityVerdict::Warn(message) =
            check_mongodump_compatibility(client, server)
    {
        ctx.emit_warn(JobPhase::Initializing, message).await;
    }

    ctx.emit(
        JobPhase::Initializing,
        format!(
            "mongodump {} → {}",
            tool_version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".into()),
            request.common.output_dir.display()
        ),
    )
    .await;

    // Everything not excluded carries its documents: `validate` has already
    // refused the structure-only mode that MongoDB cannot express, so
    // `included` and `tables_with_data` are the same set. Saying that here
    // rather than deriving it twice keeps the manifest honest.
    let included: Vec<String> = request
        .common
        .included_tables()
        .into_iter()
        .map(|s| s.name.clone())
        .collect();
    let excluded: Vec<String> = request
        .common
        .selections
        .iter()
        .filter(|s| s.mode == TableMode::Exclude)
        .map(|s| s.name.clone())
        .collect();

    if !options.oplog {
        ctx.emit_warn(
            JobPhase::Initializing,
            "this dump is consistent within each collection but not across them. \
             Turn on oplog capture (needs a replica set) for a point-in-time copy.",
        )
        .await;
    }

    std::fs::create_dir_all(&request.common.output_dir)
        .map_err(|e| BackupError::Io(format!("could not create the output directory: {e}")))?;

    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    // The extension states what the file is, layer by layer, outermost last —
    // the same rule the other engines follow so a file reached for outside this
    // app can be opened by reading its name.
    let suffix = match (request.common.compress, recipients.is_empty()) {
        (true, true) => "archive.gz",
        (true, false) => "archive.gz.age",
        (false, true) => "archive",
        (false, false) => "archive.age",
    };
    let filename = format!("{}_{}.{suffix}", request.common.database, timestamp);
    let artifact = request.common.output_dir.join(&filename);

    let plan = DumpPlan {
        mongodump,
        endpoint,
        database: request.common.database.clone(),
        options: options.clone(),
        excluded,
        compress: request.common.compress,
        artifact: artifact.clone(),
        recipients: recipients.to_vec(),
    };

    let current_child: Arc<Mutex<Option<ChildHandle>>> = Arc::new(Mutex::new(None));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DumpProgress>();

    let forward_ctx = ctx.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(update) = rx.recv().await {
            match update {
                DumpProgress::Phase(phase, message) => forward_ctx.emit(phase, message).await,
                DumpProgress::Warn(message) => {
                    forward_ctx.emit_warn(JobPhase::DumpData, message).await
                }
            }
        }
    });

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
            // A partial archive is worse than none: mongorestore would read the
            // prefix and restore part of a database while reporting success.
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
        engine: Engine::Mongo,
        server_version,
        dump_tool: "mongodump".into(),
        dump_tool_version: tool_version
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".into()),
        database: request.common.database.clone(),
        created_at: Utc::now(),
        format: ArtifactFormat::MongoArchive,
        tables: included.clone(),
        // Identical to `tables`, and that is a property of the engine rather
        // than an oversight — see the note where `included` is built.
        tables_with_data: included,
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

/// The blocking half: run `mongodump` and stream its archive into the artifact.
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

    // Held until the child has exited: dropping it deletes the file, and
    // mongodump reads it after it starts.
    let credentials = match &plan.endpoint.password {
        Some(pw) => Some(password_config(pw).map_err(BackupError::Io)?),
        None => None,
    };

    let cmd = dump_command(&plan, credentials.as_ref().map(|f| f.path()));
    let _ = tx.send(DumpProgress::Phase(JobPhase::DumpData, cmd.display()));

    let (child, stdout, stderr) = cmd.spawn_streaming()?;
    {
        *current_child.blocking_lock() = Some(child);
    }

    let mut reader = std::io::BufReader::with_capacity(1 << 16, stdout);
    let copied = std::io::copy(&mut reader, &mut writer)
        .map_err(|e| BackupError::Io(format!("streaming the archive failed: {e}")));

    let child = current_child
        .blocking_lock()
        .take()
        .expect("child was stored before streaming");

    let waited = wait_checked(child, stderr, &cancel);

    // Report the streaming error ahead of the exit status: it is closer to the
    // cause than "mongodump exited 1".
    copied?;

    match waited {
        Ok(notices) => {
            // `mongodump` writes its ordinary progress to stderr, so these are
            // not failures — but a "Failed:" line among them is, and
            // `wait_checked` only sees the exit status.
            for line in notices {
                if line.contains("Failed:") || line.contains("error:") {
                    let _ = tx.send(DumpProgress::Warn(format!("mongodump: {line}")));
                } else {
                    tracing::debug!("mongodump: {line}");
                }
            }
        }
        Err(crate::exec::ExecError::Cancelled { .. }) => return Err(BackupError::Cancelled),
        Err(e) => return Err(e.into()),
    }

    // Finishing matters: gzip writes its trailer and age its final
    // authenticated chunk. Dropping the writer leaves a file that looks
    // complete and is not.
    let buf = writer
        .finish()
        .map_err(|e| BackupError::Io(format!("could not finish the artifact stream: {e}")))?;
    let file = buf
        .into_inner()
        .map_err(|e| BackupError::Io(format!("could not flush the artifact: {e}")))?;
    file.sync_all()
        .map_err(|e| BackupError::Io(format!("could not sync the artifact: {e}")))?;

    std::fs::metadata(&plan.artifact)
        .map(|m| m.len())
        .map_err(|e| BackupError::Io(format!("could not stat the artifact: {e}")))
}

fn dump_command(plan: &DumpPlan, config: Option<&Path>) -> ToolCommand {
    let o = &plan.options;
    let mut args = vec![
        format!("--host={}", plan.endpoint.host),
        format!("--port={}", plan.endpoint.port),
        format!("--db={}", plan.database),
        // Write the archive to stdout so it can be compressed, encrypted and
        // hashed in one pass without an intermediate file.
        "--archive".to_string(),
        // Never let the driver inside mongodump go discovering replica set
        // members: through a tunnel their advertised hostnames do not resolve
        // here. Same reason the introspector sets directConnection.
        "--readPreference=primaryPreferred".to_string(),
    ];

    if !plan.endpoint.user.is_empty() {
        args.push(format!("--username={}", plan.endpoint.user));
        // Stated explicitly so the tool and the driver authenticate against the
        // same database. See `crate::db::MONGO_AUTH_SOURCE`.
        args.push(format!("--authenticationDatabase={MONGO_AUTH_SOURCE}"));
    }
    if let Some(path) = config {
        // Never `--password=…`: argv is world-readable via `ps`. See
        // `password_config` for why this is a file rather than an env var.
        args.push(format!("--config={}", path.display()));
    }

    if plan.compress {
        args.push("--gzip".to_string());
    }
    if o.oplog {
        args.push("--oplog".to_string());
    }
    if o.dump_users_and_roles {
        args.push("--dumpDbUsersAndRoles".to_string());
    }
    if let Some(n) = o.parallel_collections {
        args.push(format!("--numParallelCollections={n}"));
    }
    for name in &plan.excluded {
        args.push(format!("--excludeCollection={name}"));
    }
    args.extend(o.extra_flags.clone());

    ToolCommand::new(plan.mongodump.display().to_string()).args(args)
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
    use crate::backup::{CommonBackupOptions, TableSelection};
    use crate::profile::{DbConfig, ToolOverrides};
    use crate::types::EnvironmentTag;
    use secrecy::SecretString;

    fn plan() -> DumpPlan {
        DumpPlan {
            mongodump: PathBuf::from("/usr/local/bin/mongodump"),
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: 27018,
                user: "admin".into(),
                password: Some(SecretString::from("hunter2")),
            },
            database: "app".into(),
            options: MongoBackupOptions::default(),
            excluded: Vec::new(),
            compress: true,
            artifact: PathBuf::from("/tmp/app.archive.gz"),
            recipients: Vec::new(),
        }
    }

    fn profile() -> ConnectionProfile {
        ConnectionProfile {
            id: Uuid::new_v4(),
            name: "p".into(),
            engine: Engine::Mongo,
            environment: EnvironmentTag::Dev,
            ssh_connection_id: None,
            db: DbConfig {
                host: "127.0.0.1".into(),
                port: 27017,
                user: "admin".into(),
                database: None,
            },
            tool_overrides: ToolOverrides::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn request(selections: Vec<TableSelection>) -> BackupRequest {
        BackupRequest {
            common: CommonBackupOptions {
                database: "app".into(),
                selections,
                output_dir: PathBuf::from("/tmp"),
                compress: true,
                encrypt: false,
                record_row_counts: false,
            },
            engine: EngineBackupOptions::Mongo(MongoBackupOptions::default()),
        }
    }

    #[test]
    fn the_password_never_appears_in_the_command_line() {
        let file = password_config(&SecretString::from("hunter2")).expect("config file");
        let rendered = dump_command(&plan(), Some(file.path())).display();
        assert!(
            !rendered.contains("hunter2"),
            "password leaked into argv: {rendered}"
        );
        assert!(
            !rendered.contains("--password"),
            "argv is readable by every user on the machine: {rendered}"
        );
        assert!(rendered.contains("--config="), "{rendered}");
    }

    // ── Credentials file ────────────────────────────────────────────────

    #[test]
    fn the_credentials_file_holds_the_password_as_yaml() {
        let file = password_config(&SecretString::from("hunter2")).unwrap();
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(contents.trim(), r#"password: "hunter2""#);
    }

    #[test]
    fn a_password_can_never_start_a_new_line() {
        // The hazard: a password carrying a newline and a second setting, so
        // that `uri:` — which would redirect the dump to a server of somebody
        // else's choosing — appears at the start of a line in a file the tool
        // parses. A single-quoted YAML scalar folds multiple lines and would
        // still read it as part of the password, but that is a property of the
        // reader. Escaping makes it a property of the file.
        let file =
            password_config(&SecretString::from("a\nuri: mongodb://evil.test")).unwrap();
        let contents = std::fs::read_to_string(file.path()).unwrap();

        assert_eq!(
            contents.lines().count(),
            1,
            "the file must be exactly one line: {contents:?}"
        );
        assert!(contents.contains("\\n"), "newline must be escaped: {contents:?}");
    }

    #[test]
    fn awkward_passwords_survive_intact() {
        // Round-tripped through the same escapes YAML would undo, so this
        // checks the encoding is reversible rather than merely safe.
        fn unescape(s: &str) -> String {
            let mut out = String::new();
            let mut chars = s.chars();
            while let Some(c) = chars.next() {
                if c != '\\' {
                    out.push(c);
                    continue;
                }
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some(other) => out.push(other),
                    None => break,
                }
            }
            out
        }

        for password in [
            "p:w#d",
            "back\\slash",
            "with spaces",
            "üñïçø∂é",
            "\"quoted\"",
            "it's",
            "line\nbreak",
            "tab\there",
        ] {
            let file = password_config(&SecretString::from(password)).unwrap();
            let contents = std::fs::read_to_string(file.path()).unwrap();
            let body = contents
                .trim_end()
                .strip_prefix("password: \"")
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or_else(|| panic!("unexpected shape for {password:?}: {contents:?}"));
            assert_eq!(unescape(body), password, "{password:?} did not round-trip");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_credentials_file_is_never_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let file = password_config(&SecretString::from("hunter2")).unwrap();
        let mode = std::fs::metadata(file.path()).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "created with the permissions, not chmod'ed after — it must never be \
             briefly readable"
        );
    }

    #[test]
    fn no_config_flag_when_there_is_no_password() {
        let mut p = plan();
        p.endpoint.password = None;
        assert!(!dump_command(&p, None).display().contains("--config"));
    }

    #[test]
    fn the_archive_goes_to_stdout_so_it_can_be_hashed_in_one_pass() {
        let rendered = dump_command(&plan(), None).display();
        // `--archive` with no `=path` means stdout. With a path, the stream
        // would never reach the encryption and checksum layers.
        assert!(rendered.contains(" --archive "), "{rendered}");
        assert!(!rendered.contains("--archive="), "{rendered}");
    }

    #[test]
    fn the_tool_authenticates_against_the_same_database_as_the_driver() {
        // If these two disagree, a backup can succeed while the introspection
        // that verifies it cannot connect at all.
        let rendered = dump_command(&plan(), None).display();
        assert!(rendered.contains(&format!("--authenticationDatabase={MONGO_AUTH_SOURCE}")));
    }

    #[test]
    fn no_credentials_flags_when_the_profile_has_no_user() {
        let mut p = plan();
        p.endpoint.user = String::new();
        p.endpoint.password = None;
        let rendered = dump_command(&p, None).display();
        assert!(!rendered.contains("--username"));
        assert!(!rendered.contains("--authenticationDatabase"));
    }

    #[test]
    fn compression_is_the_tools_own_not_a_second_layer() {
        assert!(dump_command(&plan(), None).display().contains("--gzip"));
        let mut p = plan();
        p.compress = false;
        assert!(!dump_command(&p, None).display().contains("--gzip"));
    }

    #[test]
    fn excluded_collections_become_exclude_flags() {
        let mut p = plan();
        p.excluded = vec!["sessions".into(), "cache".into()];
        let rendered = dump_command(&p, None).display();
        assert!(rendered.contains("--excludeCollection=sessions"));
        assert!(rendered.contains("--excludeCollection=cache"));
    }

    #[test]
    fn oplog_capture_is_off_unless_asked_for() {
        // Defaulting it on would break every standalone server, which is what
        // most development databases are.
        assert!(!dump_command(&plan(), None).display().contains("--oplog"));
        let mut p = plan();
        p.options.oplog = true;
        assert!(dump_command(&p, None).display().contains("--oplog"));
    }

    #[test]
    fn users_and_roles_are_not_dumped_by_default() {
        assert!(
            !dump_command(&plan(), None)
                .display()
                .contains("--dumpDbUsersAndRoles"),
            "restoring these elsewhere would change who can log in there"
        );
    }

    #[test]
    fn extra_flags_reach_the_command() {
        let mut p = plan();
        p.options.extra_flags = vec!["--quiet".into()];
        assert!(dump_command(&p, None).display().contains("--quiet"));
    }

    // ── What MongoDB refuses, and why ───────────────────────────────────

    #[test]
    fn a_structure_only_collection_is_refused_rather_than_approximated() {
        // mongodump writes one archive in one pass with no per-collection
        // document filter, so "structure only" would have to become either the
        // whole collection or none of it — and the manifest would then describe
        // an artifact that was not produced, which the drill believes.
        let req = request(vec![
            TableSelection::with_data("orders"),
            TableSelection::schema_only("audit"),
        ]);
        let err = req.validate(&profile()).unwrap_err();
        assert!(err.to_string().contains("structure-only"), "got: {err}");
    }

    #[test]
    fn a_row_filter_is_refused() {
        let req = request(vec![TableSelection {
            name: "orders".into(),
            mode: TableMode::SchemaAndData,
            where_filter: Some(r#"{"total":{"$gt":100}}"#.into()),
        }]);
        let err = req.validate(&profile()).unwrap_err();
        assert!(err.to_string().contains("row filter"), "got: {err}");
    }

    #[test]
    fn an_ordinary_selection_validates() {
        let req = request(vec![
            TableSelection::with_data("orders"),
            TableSelection {
                name: "sessions".into(),
                mode: TableMode::Exclude,
                where_filter: None,
            },
        ]);
        assert!(req.validate(&profile()).is_ok());
    }

    #[test]
    fn a_mongo_backup_may_be_encrypted() {
        // The archive is a single stream, which is exactly what the PostgreSQL
        // directory formats are refused for.
        let mut req = request(vec![TableSelection::with_data("orders")]);
        req.common.encrypt = true;
        assert!(req.validate(&profile()).is_ok());
    }

    #[test]
    fn the_artifact_format_is_a_single_file() {
        let opts = EngineBackupOptions::Mongo(MongoBackupOptions::default());
        assert_eq!(opts.artifact_format(), ArtifactFormat::MongoArchive);
        assert!(!opts.artifact_format().is_directory());
    }
}
