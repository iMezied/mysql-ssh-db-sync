//! PostgreSQL restore execution.
//!
//! Archive formats go through `pg_restore`, which is what makes parallel and
//! selective restore possible. Plain SQL goes through `psql`, which can do
//! neither — the reason the default format is `custom`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flate2::read::GzDecoder;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::{EngineRestoreOptions, PostgresRestoreOptions, RestoreError, RestoreRun};
use crate::backup::mysql::Endpoint;
use crate::events::JobPhase;
use crate::exec::{ChildHandle, ToolCommand, wait_checked};
use crate::job::JobContext;
use crate::manifest::{ArtifactFormat, BackupManifest};
use crate::tools::{MountMode, ResolvedTool, Tool};

enum RestoreProgress {
    Phase(JobPhase, String),
}

/// Run a PostgreSQL restore, returning the database written to.
pub async fn run_postgres_restore(
    run: RestoreRun<'_>,
    ctx: &JobContext,
) -> Result<String, RestoreError> {
    let RestoreRun {
        profile,
        request,
        target,
        endpoint,
        tools,
    } = run;

    let manifest = BackupManifest::read(&request.artifact_path).ok();
    request.validate(profile, manifest.as_ref())?;

    let EngineRestoreOptions::Postgres(options) = &request.engine else {
        return Err(RestoreError::EngineMismatch {
            profile: profile.engine,
            options: request.engine.engine(),
        });
    };

    // Without a manifest, infer from the artifact: a gzip stream is plain SQL,
    // anything else is an archive pg_restore can read.
    let format = manifest
        .as_ref()
        .map(|m| m.format)
        .unwrap_or_else(|| infer_format(&request.artifact_path));

    let psql = ResolvedTool::resolve(Tool::Psql, tools, profile.tool_overrides.psql.as_deref())
        .ok_or_else(|| {
            RestoreError::Invalid("could not find psql; install the PostgreSQL client tools".into())
        })?;

    let pg_restore = if format.supports_selective_restore() {
        Some(
            ResolvedTool::resolve(
                Tool::PgRestore,
                tools,
                profile.tool_overrides.pg_restore.as_deref(),
            )
            .ok_or_else(|| {
                RestoreError::Invalid("could not find pg_restore for this archive".into())
            })?,
        )
    } else {
        None
    };

    // Inside a container, loopback is the container rather than the host that
    // holds the tunnel's local end. See `ToolSource::rewrite_host`.
    let endpoint = Endpoint {
        host: tools.rewrite_host(&endpoint.host),
        ..endpoint
    };

    // Only a single-file artifact has a checksum worth verifying.
    if request.verify_checksum && format != ArtifactFormat::PgDirectory {
        if let Some(m) = &manifest {
            ctx.emit(JobPhase::Verify, "verifying artifact checksum")
                .await;
            m.verify_artifact(&request.artifact_path)
                .map_err(|e| RestoreError::Invalid(e.to_string()))?;
        } else {
            ctx.emit_warn(
                JobPhase::Verify,
                "no manifest alongside this artifact; skipping the checksum check",
            )
            .await;
        }
    }

    ctx.emit(
        JobPhase::Restore,
        format!(
            "restoring {} into {target}",
            request.artifact_path.display()
        ),
    )
    .await;

    let current_child: Arc<Mutex<Option<ChildHandle>>> = Arc::new(Mutex::new(None));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RestoreProgress>();

    let forward_ctx = ctx.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(RestoreProgress::Phase(phase, message)) = rx.recv().await {
            forward_ctx.emit(phase, message).await;
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

    let worker = RestoreWorker {
        psql,
        pg_restore,
        endpoint,
        target: target.clone(),
        artifact: request.artifact_path.clone(),
        format,
        options: options.clone(),
        drop_first: request.naming.is_destructive(),
        create_database: !matches!(request.naming, super::TargetNaming::IntoExisting { .. }),
    };

    let worker_cancel = ctx.cancel_token();
    let worker_child = current_child.clone();
    let result = tokio::task::spawn_blocking(move || worker.run(tx, worker_child, worker_cancel))
        .await
        .map_err(|e| RestoreError::Invalid(format!("restore worker panicked: {e}")))?;

    killer.abort();
    let _ = forwarder.await;
    result?;

    ctx.emit(JobPhase::Done, format!("restored into {target}"))
        .await;
    Ok(target)
}

fn infer_format(path: &Path) -> ArtifactFormat {
    if path.is_dir() {
        ArtifactFormat::PgDirectory
    } else if path.extension().is_some_and(|e| e == "gz") {
        ArtifactFormat::SqlGz
    } else {
        ArtifactFormat::PgCustom
    }
}

struct RestoreWorker {
    psql: ResolvedTool,
    pg_restore: Option<ResolvedTool>,
    endpoint: Endpoint,
    target: String,
    artifact: PathBuf,
    format: ArtifactFormat,
    options: PostgresRestoreOptions,
    drop_first: bool,
    create_database: bool,
}

impl RestoreWorker {
    fn run(
        self,
        tx: UnboundedSender<RestoreProgress>,
        current_child: Arc<Mutex<Option<ChildHandle>>>,
        cancel: CancellationToken,
    ) -> Result<(), RestoreError> {
        if self.create_database {
            let _ = tx.send(RestoreProgress::Phase(
                JobPhase::Restore,
                format!("creating database {}", self.target),
            ));
            self.create_target(&current_child, &cancel)?;
        }

        match self.format {
            ArtifactFormat::PgCustom | ArtifactFormat::PgDirectory => {
                let _ = tx.send(RestoreProgress::Phase(
                    JobPhase::Restore,
                    "restoring archive with pg_restore".into(),
                ));
                self.run_pg_restore(&tx, &current_child, &cancel)
            }
            _ => {
                let _ = tx.send(RestoreProgress::Phase(
                    JobPhase::Restore,
                    "streaming plain SQL through psql".into(),
                ));
                self.stream_plain(&current_child, &cancel)
            }
        }
    }

    /// Connect to `postgres` rather than the target: you cannot create a
    /// database from inside itself.
    fn admin_command(&self) -> ToolCommand {
        self.psql_command("postgres")
    }

    fn psql_command(&self, database: &str) -> ToolCommand {
        let mut cmd = self.psql.command().args([
            format!("--host={}", self.endpoint.host),
            format!("--port={}", self.endpoint.port),
            format!("--username={}", self.endpoint.user),
            "--no-password".to_string(),
            // Without this psql reports success even when a statement failed.
            "--set=ON_ERROR_STOP=1".to_string(),
            format!("--dbname={database}"),
        ]);
        if let Some(pw) = &self.endpoint.password {
            cmd = cmd.secret_env("PGPASSWORD", pw.clone());
        }
        cmd
    }

    fn create_target(
        &self,
        current_child: &Arc<Mutex<Option<ChildHandle>>>,
        cancel: &CancellationToken,
    ) -> Result<(), RestoreError> {
        let quoted = crate::db::quote_pg_ident(&self.target)
            .map_err(|e| RestoreError::Invalid(e.to_string()))?;

        let mut sql = String::new();
        if self.drop_first {
            sql.push_str(&format!("DROP DATABASE IF EXISTS {quoted};"));
        }
        sql.push_str(&format!("CREATE DATABASE {quoted};"));

        let cmd = self.admin_command().arg("--command").arg(sql);
        self.run_to_completion(cmd, current_child, cancel)
    }

    fn run_pg_restore(
        &self,
        tx: &UnboundedSender<RestoreProgress>,
        current_child: &Arc<Mutex<Option<ChildHandle>>>,
        cancel: &CancellationToken,
    ) -> Result<(), RestoreError> {
        let pg_restore = self.pg_restore.as_ref().ok_or_else(|| {
            RestoreError::Invalid("pg_restore is required for this archive".into())
        })?;

        // Selective restore works by filtering the archive's table of contents
        // and handing the result back with -L.
        let list_file = if self.options.only_tables.is_empty() {
            None
        } else {
            let _ = tx.send(RestoreProgress::Phase(
                JobPhase::Restore,
                format!(
                    "selecting {} table(s) from the archive",
                    self.options.only_tables.len()
                ),
            ));
            Some(self.build_toc_filter(pg_restore, current_child, cancel)?)
        };

        let mut args = vec![
            format!("--host={}", self.endpoint.host),
            format!("--port={}", self.endpoint.port),
            format!("--username={}", self.endpoint.user),
            "--no-password".to_string(),
            format!("--dbname={}", self.target),
            // Any error must fail the job, not leave a partial database that
            // looks restored.
            "--exit-on-error".to_string(),
        ];

        if self.options.no_owner {
            args.push("--no-owner".into());
        }
        if self.options.no_privileges {
            args.push("--no-privileges".into());
        }
        if self.options.clean {
            args.push("--clean".into());
        }
        if let Some(jobs) = self.options.parallel_jobs {
            args.push(format!("--jobs={jobs}"));
        }
        // Both paths below are on this machine. A containerised pg_restore
        // cannot see either, and would report "no such file" for an archive
        // that is plainly there — so each is bound in read-only and the tool
        // is given the path it will actually find.
        let mut pg_restore = pg_restore.clone();

        if let Some(list) = &list_file {
            let mounted = pg_restore
                .mount(list.path(), MountMode::ReadOnly)
                .map_err(|e| RestoreError::Invalid(e.to_string()))?;
            args.push(format!("--use-list={}", mounted.display()));
        }

        let artifact = pg_restore
            .mount(&self.artifact, MountMode::ReadOnly)
            .map_err(|e| RestoreError::Invalid(e.to_string()))?;
        args.push(artifact.display().to_string());

        let mut cmd = pg_restore.command().args(args);
        if let Some(pw) = &self.endpoint.password {
            cmd = cmd.secret_env("PGPASSWORD", pw.clone());
        }

        self.run_to_completion(cmd, current_child, cancel)
    }

    /// Produce a `-L` list containing only the requested tables' entries.
    ///
    /// Everything that is not a table-ish entry (schemas, types, extensions,
    /// sequences) is kept: dropping those would leave the selected tables
    /// unable to be created.
    fn build_toc_filter(
        &self,
        pg_restore: &ResolvedTool,
        current_child: &Arc<Mutex<Option<ChildHandle>>>,
        cancel: &CancellationToken,
    ) -> Result<tempfile::NamedTempFile, RestoreError> {
        let mut pg_restore = pg_restore.clone();
        let artifact = pg_restore
            .mount(&self.artifact, MountMode::ReadOnly)
            .map_err(|e| RestoreError::Invalid(e.to_string()))?;
        let mut cmd = pg_restore
            .command()
            .arg("--list")
            .arg(artifact.display().to_string());
        if let Some(pw) = &self.endpoint.password {
            cmd = cmd.secret_env("PGPASSWORD", pw.clone());
        }

        let (child, stdout, stderr) = cmd.spawn_streaming()?;
        {
            *current_child.blocking_lock() = Some(child);
        }

        let toc: Vec<String> = std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .collect();

        let child = current_child.blocking_lock().take().expect("stored");
        match wait_checked(child, stderr, cancel) {
            Ok(_) => {}
            Err(crate::exec::ExecError::Cancelled { .. }) => return Err(RestoreError::Cancelled),
            Err(e) => return Err(RestoreError::Exec(e)),
        }

        let mut file = tempfile::NamedTempFile::new()
            .map_err(|e| RestoreError::Invalid(format!("could not create a TOC filter: {e}")))?;

        for line in toc.iter().filter(|l| self.toc_line_wanted(l)) {
            writeln!(file, "{line}")
                .map_err(|e| RestoreError::Invalid(format!("writing the TOC filter: {e}")))?;
        }
        file.flush()
            .map_err(|e| RestoreError::Invalid(format!("flushing the TOC filter: {e}")))?;

        Ok(file)
    }

    fn toc_line_wanted(&self, line: &str) -> bool {
        toc_line_wanted(line, &self.options.only_tables)
    }

    fn stream_plain(
        &self,
        current_child: &Arc<Mutex<Option<ChildHandle>>>,
        cancel: &CancellationToken,
    ) -> Result<(), RestoreError> {
        let cmd = self.psql_command(&self.target);
        let (child, mut stdin, stderr) = cmd.spawn_writing()?;
        {
            *current_child.blocking_lock() = Some(child);
        }

        let outcome = (|| -> Result<(), RestoreError> {
            let file = std::fs::File::open(&self.artifact)
                .map_err(|e| RestoreError::Invalid(format!("could not open the artifact: {e}")))?;
            let decoder = GzDecoder::new(file);
            let mut reader = std::io::BufReader::with_capacity(1 << 16, decoder);

            let mut buf = Vec::with_capacity(1 << 16);
            loop {
                if cancel.is_cancelled() {
                    return Err(RestoreError::Cancelled);
                }
                buf.clear();
                let n = reader.read_until(b'\n', &mut buf).map_err(|e| {
                    RestoreError::Invalid(format!("reading the artifact failed: {e}"))
                })?;
                if n == 0 {
                    break;
                }
                stdin.write_all(&buf).map_err(|e| {
                    RestoreError::Invalid(format!("psql closed its input early: {e}"))
                })?;
            }
            Ok(())
        })();

        // Closing stdin is what tells psql the input is complete.
        drop(stdin);

        let child = current_child.blocking_lock().take().expect("stored");
        let wait = wait_checked(child, stderr, cancel);

        outcome?;

        match wait {
            Ok(_) => Ok(()),
            Err(crate::exec::ExecError::Cancelled { .. }) => Err(RestoreError::Cancelled),
            Err(e) => Err(RestoreError::Exec(e)),
        }
    }

    fn run_to_completion(
        &self,
        cmd: ToolCommand,
        current_child: &Arc<Mutex<Option<ChildHandle>>>,
        cancel: &CancellationToken,
    ) -> Result<(), RestoreError> {
        let (child, _stdout, stderr) = cmd.spawn_streaming()?;
        {
            *current_child.blocking_lock() = Some(child);
        }
        let child = current_child.blocking_lock().take().expect("stored");

        match wait_checked(child, stderr, cancel) {
            Ok(_) => Ok(()),
            Err(crate::exec::ExecError::Cancelled { .. }) => Err(RestoreError::Cancelled),
            Err(e) => Err(RestoreError::Exec(e)),
        }
    }
}

/// Decide whether a `pg_restore --list` line survives table selection.
///
/// A TOC line looks like:
///
/// ```text
/// <dumpId>; <catalogOid> <oid> <DESC...> <schema> <name> <owner>
/// 3401; 0 16428 TABLE DATA public orders dbsync
/// ```
///
/// **Only `TABLE DATA` entries are filtered.** The full schema is always
/// restored, and selection controls which tables' rows come with it.
///
/// Filtering table *definitions* as well would be a trap: a TOC line for an
/// index or constraint names the index, not the table it belongs to, so there
/// is no way to tell from `--list` output which ones belong to a dropped table.
/// With `--exit-on-error` set, the first orphaned index would abort the whole
/// restore. Restoring the full schema and choosing the data is both safe and
/// what "restore only these tables" is normally taken to mean.
pub fn toc_line_wanted(line: &str, only_tables: &[String]) -> bool {
    let trimmed = line.trim_start();
    // Comments and blank lines carry no entry.
    if trimmed.is_empty() || trimmed.starts_with(';') {
        return false;
    }

    // Everything before the first ';' is the dump id.
    let Some((_dump_id, rest)) = trimmed.split_once(';') else {
        return true;
    };

    let fields: Vec<&str> = rest.split_whitespace().collect();
    // Two catalog oids precede the description.
    let Some(after_oids) = fields.get(2..) else {
        return true;
    };

    // Only data entries are subject to selection.
    if after_oids.len() < 4 || after_oids[0] != "TABLE" || after_oids[1] != "DATA" {
        return true;
    }

    let schema = after_oids[2];
    let name = after_oids[3];
    let qualified = format!("{schema}.{name}");

    only_tables.iter().any(|t| {
        t == &qualified
            // A bare name from the UI is treated as being in the default schema.
            || (schema == "public" && t == name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolSource;

    /// A tool pinned to a path that exists, so these fixtures never depend on
    /// whether the client happens to be installed on the machine running the
    /// tests. Only the rendered command line is under test.
    fn local_tool(tool: Tool) -> ResolvedTool {
        ResolvedTool::resolve(tool, &ToolSource::Local, Some("/bin/sh")).expect("/bin/sh exists")
    }
    use crate::restore::TargetNaming;
    use secrecy::SecretString;

    fn worker(format: ArtifactFormat, naming: &TargetNaming) -> RestoreWorker {
        RestoreWorker {
            psql: local_tool(Tool::Psql),
            pg_restore: Some(local_tool(Tool::PgRestore)),
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: 15432,
                user: "dbsync".into(),
                password: Some(SecretString::from("hunter2")),
            },
            target: naming.resolve(chrono::Utc::now()),
            artifact: PathBuf::from("/tmp/a.dump"),
            format,
            options: PostgresRestoreOptions::default(),
            drop_first: naming.is_destructive(),
            create_database: !matches!(naming, TargetNaming::IntoExisting { .. }),
        }
    }

    fn timestamped() -> TargetNaming {
        TargetNaming::NewTimestamped {
            prefix: "restore".into(),
        }
    }

    #[test]
    fn the_password_never_appears_in_the_command_line() {
        let w = worker(ArtifactFormat::PgCustom, &timestamped());
        let rendered = w.psql_command("app").display();
        assert!(!rendered.contains("hunter2"));
        assert!(rendered.contains("--no-password"));
    }

    #[test]
    fn psql_stops_on_the_first_error() {
        // Without ON_ERROR_STOP psql exits 0 having skipped failed statements,
        // which would report a broken restore as a success.
        let w = worker(ArtifactFormat::SqlGz, &timestamped());
        assert!(w.psql_command("app").display().contains("ON_ERROR_STOP=1"),);
    }

    #[test]
    fn the_database_is_created_from_the_postgres_database() {
        // You cannot CREATE DATABASE from inside the database being created.
        let w = worker(ArtifactFormat::PgCustom, &timestamped());
        assert!(w.admin_command().display().contains("--dbname=postgres"));
    }

    #[test]
    fn formats_are_inferred_when_no_manifest_exists() {
        assert_eq!(
            infer_format(Path::new("/tmp/app.sql.gz")),
            ArtifactFormat::SqlGz
        );
        assert_eq!(
            infer_format(Path::new("/tmp/app.dump")),
            ArtifactFormat::PgCustom
        );
    }

    #[test]
    fn a_directory_artifact_is_inferred_as_a_directory_archive() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(infer_format(dir.path()), ArtifactFormat::PgDirectory);
    }

    // ── TOC filtering ───────────────────────────────────────────────────

    #[test]
    fn toc_keeps_data_for_selected_tables() {
        let only = vec!["public.orders".to_string()];
        assert!(toc_line_wanted(
            "3401; 0 16428 TABLE DATA public orders dbsync",
            &only
        ));
    }

    #[test]
    fn toc_drops_data_for_unselected_tables() {
        let only = vec!["public.orders".to_string()];
        assert!(!toc_line_wanted(
            "3402; 0 16440 TABLE DATA public users dbsync",
            &only
        ));
    }

    #[test]
    fn toc_always_keeps_the_full_schema() {
        // Table definitions are never filtered: a TOC line for an index names
        // the index, not its table, so there is no way to drop only the
        // indexes belonging to an excluded table. Dropping the definitions
        // would abort the restore on the first orphaned index.
        let only = vec!["public.orders".to_string()];
        for line in [
            "215; 1259 16428 TABLE public orders dbsync",
            "216; 1259 16440 TABLE public users dbsync",
            "5; 2615 2200 SCHEMA - public dbsync",
            "880; 1247 16400 TYPE public order_status dbsync",
            "2; 3079 16385 EXTENSION - plpgsql",
            "217; 1259 16450 SEQUENCE public orders_id_seq dbsync",
            "2087; 1259 16456 INDEX public idx_orders_user dbsync",
            "3299; 2606 16470 FK CONSTRAINT public orders fk_order_user dbsync",
        ] {
            assert!(toc_line_wanted(line, &only), "should keep: {line}");
        }
    }

    #[test]
    fn toc_ignores_comments_and_blanks() {
        let only = vec!["public.orders".to_string()];
        assert!(!toc_line_wanted(";", &only));
        assert!(!toc_line_wanted("; Archive created at 2026-01-01", &only));
        assert!(!toc_line_wanted("   ", &only));
    }

    #[test]
    fn toc_accepts_an_unqualified_selection_as_public() {
        let only = vec!["orders".to_string()];
        assert!(toc_line_wanted(
            "3401; 0 16428 TABLE DATA public orders dbsync",
            &only
        ));
        // A bare name must not reach into another schema.
        assert!(!toc_line_wanted(
            "3402; 0 16500 TABLE DATA reporting orders dbsync",
            &only
        ));
    }

    #[test]
    fn toc_distinguishes_same_named_tables_in_different_schemas() {
        let only = vec!["reporting.daily_totals".to_string()];
        assert!(toc_line_wanted(
            "3403; 0 16500 TABLE DATA reporting daily_totals dbsync",
            &only
        ));
        assert!(!toc_line_wanted(
            "3404; 0 16510 TABLE DATA public daily_totals dbsync",
            &only
        ));
    }
}
