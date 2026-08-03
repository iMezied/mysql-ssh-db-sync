//! MongoDB restore execution.
//!
//! Streams `artifact → age → mongorestore`, reading the archive from stdin.
//!
//! The interesting part is how the database gets renamed. A restore in this
//! application almost always lands in a database that is *not* the one the dump
//! came from — a timestamped scratch database for a drill, a differently-named
//! copy for a sync. MySQL gets that by never emitting `USE`; PostgreSQL by
//! creating the database and pointing the client at it. MongoDB has neither
//! mechanism: the archive carries the source namespace in every entry, and
//! `mongorestore` writes it back where it came from unless told otherwise.
//!
//! `--nsFrom=<source>.* --nsTo=<target>.*` is what tells it otherwise, and it
//! needs the source database's name — which is why this reads the manifest and
//! refuses to run without one. Restoring a MongoDB archive without knowing
//! where it came from would silently overwrite the source database, and on a
//! profile pointed at production that is the worst outcome this application
//! could produce.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::{EngineRestoreOptions, MongoRestoreOptions, RestoreError, RestoreRun, TargetNaming};
use crate::backup::mysql::Endpoint;
use crate::db::MONGO_AUTH_SOURCE;
use crate::events::JobPhase;
use crate::exec::{ChildHandle, ToolCommand, wait_checked};
use crate::tools::{ResolvedTool, Tool};
use crate::job::JobContext;
use crate::manifest::BackupManifest;

/// How often to report streaming progress, in bytes.
const PROGRESS_EVERY: u64 = 4 * 1024 * 1024;

enum RestoreProgress {
    Phase(JobPhase, String),
    Bytes { done: u64, total: u64 },
}

/// Run a MongoDB restore, returning the database that was written to.
pub async fn run_mongo_restore(
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

    let EngineRestoreOptions::Mongo(options) = &request.engine else {
        return Err(RestoreError::EngineMismatch {
            profile: profile.engine,
            options: request.engine.engine(),
        });
    };

    // The source namespace is not optional here, and this is the one engine
    // where a missing manifest cannot be shrugged off: without it there is no
    // `--nsFrom`, and mongorestore puts every collection back in the database
    // it was dumped from. On a production profile that is a silent overwrite of
    // the source.
    let source_database = manifest
        .as_ref()
        .map(|m| m.database.clone())
        .ok_or_else(|| {
            RestoreError::Invalid(
                "this MongoDB archive has no manifest beside it, so the database it was dumped \
                 from is unknown. mongorestore would put every collection back into that \
                 database — possibly the source. Restore it with its .manifest.json file."
                    .into(),
            )
        })?;

    // Trust the bytes on disk over the manifest, for the same reason the other
    // engines do: a manifest can be stale or hand-edited, an age header cannot.
    let identity = if crate::crypto::looks_encrypted(&request.artifact_path) {
        let key = crate::backupkey::identity().map_err(|_| {
            let recipients = manifest
                .as_ref()
                .map(|m| m.encryption_recipients.join(", "))
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "(not recorded)".into());
            RestoreError::Invalid(format!(
                "this artifact is encrypted and no backup key is available on this machine. \
                 It was encrypted to: {recipients}. Import that key in Settings before restoring."
            ))
        })?;
        Some(key)
    } else {
        if manifest.as_ref().is_some_and(|m| m.encrypted) {
            return Err(RestoreError::Invalid(
                "the manifest says this artifact is encrypted, but the file is not. It has been \
                 replaced or corrupted; refusing to restore it."
                    .into(),
            ));
        }
        None
    };

    let mongorestore = ResolvedTool::resolve(
        Tool::Mongorestore,
        tools,
        profile.tool_overrides.mongorestore.as_deref(),
    )
    .ok_or_else(|| {
        RestoreError::Invalid(
            "could not find mongorestore; install the MongoDB Database Tools or set an override"
                .into(),
        )
    })?;

    // Inside a container, loopback is the container rather than the host that
    // holds the tunnel's local end. See `ToolSource::rewrite_host`.
    let endpoint = Endpoint {
        host: tools.rewrite_host(&endpoint.host),
        ..endpoint
    };

    if request.verify_checksum {
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

    let artifact_size = std::fs::metadata(&request.artifact_path)
        .map(|m| m.len())
        .unwrap_or(0);

    let current_child: Arc<Mutex<Option<ChildHandle>>> = Arc::new(Mutex::new(None));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RestoreProgress>();

    let forward_ctx = ctx.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(update) = rx.recv().await {
            match update {
                RestoreProgress::Phase(phase, message) => forward_ctx.emit(phase, message).await,
                RestoreProgress::Bytes { done, total } => {
                    let event = crate::events::ProgressEvent::new(
                        forward_ctx.job_id,
                        JobPhase::Restore,
                        "restoring",
                    )
                    .with_progress(done, total);
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

    let worker = RestoreWorker {
        mongorestore,
        endpoint,
        source_database,
        target: target.clone(),
        artifact: request.artifact_path.clone(),
        identity,
        artifact_size,
        options: options.clone(),
        // A whole-database drop is the naming strategy's job; `mongorestore`
        // has no such flag, so `DropAndRecreate` becomes a per-collection drop.
        // The difference shows in a collection that exists at the target and
        // not in the archive: it survives. That is stated in the restore page
        // rather than pretended away.
        drop_collections: options.drop_collections
            || matches!(request.naming, TargetNaming::DropAndRecreate { .. }),
        gzipped: manifest.as_ref().is_some_and(|m| {
            m.artifact_filename.contains(".archive.gz") || m.artifact_filename.ends_with(".gz")
        }),
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

struct RestoreWorker {
    mongorestore: ResolvedTool,
    endpoint: Endpoint,
    /// The database the archive was dumped from, read from the manifest.
    source_database: String,
    target: String,
    artifact: PathBuf,
    identity: Option<secrecy::SecretString>,
    artifact_size: u64,
    options: MongoRestoreOptions,
    drop_collections: bool,
    /// Whether `mongodump --gzip` compressed inside the archive.
    gzipped: bool,
}

impl RestoreWorker {
    fn run(
        self,
        tx: UnboundedSender<RestoreProgress>,
        current_child: Arc<Mutex<Option<ChildHandle>>>,
        cancel: CancellationToken,
    ) -> Result<(), RestoreError> {
        let _ = tx.send(RestoreProgress::Phase(
            JobPhase::Restore,
            format!(
                "streaming the archive, {} → {}",
                self.source_database, self.target
            ),
        ));
        self.stream(&tx, &current_child, &cancel)
    }

    fn command(&self, mongorestore: &ResolvedTool, config: Option<&std::path::Path>) -> ToolCommand {
        let o = &self.options;
        let mut args = vec![
            format!("--host={}", self.endpoint.host),
            format!("--port={}", self.endpoint.port),
            // Read the archive from stdin.
            "--archive".to_string(),
            // Rename the namespace on the way in. Without this pair every
            // collection goes back into `source_database`.
            format!("--nsFrom={}.*", self.source_database),
            format!("--nsTo={}.*", self.target),
        ];

        if !self.endpoint.user.is_empty() {
            args.push(format!("--username={}", self.endpoint.user));
            args.push(format!("--authenticationDatabase={MONGO_AUTH_SOURCE}"));
        }
        if let Some(path) = config {
            // Never `--password=…`, and there is no environment variable to
            // use instead — see `crate::backup::mongo::password_config`. It
            // matters doubly here: this process's stdin is the archive, so the
            // tool's password prompt cannot be answered at all.
            args.push(format!("--config={}", path.display()));
        }
        if self.gzipped {
            args.push("--gzip".to_string());
        }
        if self.drop_collections {
            args.push("--drop".to_string());
        }
        if o.stop_on_error {
            args.push("--stopOnError".to_string());
        }
        if !o.restore_indexes {
            args.push("--noIndexRestore".to_string());
        }
        if o.bypass_document_validation {
            args.push("--bypassDocumentValidation".to_string());
        }
        if let Some(n) = o.parallel_collections {
            args.push(format!("--numParallelCollections={n}"));
        }
        if let Some(n) = o.insertion_workers {
            args.push(format!("--numInsertionWorkersPerCollection={n}"));
        }
        for name in &o.only_collections {
            // Namespaces in the archive are the *source's*, so a selective
            // restore filters on the source database's name, not the target's.
            // Filtering on the target would match nothing and quietly restore
            // an empty database.
            args.push(format!("--nsInclude={}.{}", self.source_database, name));
        }

        mongorestore.command().args(args)
    }

    fn stream(
        &self,
        tx: &UnboundedSender<RestoreProgress>,
        current_child: &Arc<Mutex<Option<ChildHandle>>>,
        cancel: &CancellationToken,
    ) -> Result<(), RestoreError> {
        // Held until the child has exited; dropping it deletes the file.
        let credentials = match &self.endpoint.password {
            Some(pw) => Some(
                crate::backup::mongo::password_config(pw).map_err(RestoreError::Invalid)?,
            ),
            None => None,
        };

        // A containerised tool cannot see this machine's temp directory, so
        // the file is bound in and the tool is given the path it will actually
        // find it at.
        let mut mongorestore = self.mongorestore.clone();
        let config_path = match &credentials {
            Some(file) => Some(
                mongorestore
                    .mount(file.path(), crate::tools::MountMode::ReadOnly)
                    .map_err(|e| RestoreError::Invalid(e.to_string()))?,
            ),
            None => None,
        };

        let (child, mut stdin, stderr) = self
            .command(&mongorestore, config_path.as_deref())
            .spawn_writing()?;
        {
            *current_child.blocking_lock() = Some(child);
        }

        let outcome = self.pump(&mut stdin, tx, cancel);
        // Closing stdin is what tells mongorestore the archive has ended.
        drop(stdin);

        let child = current_child
            .blocking_lock()
            .take()
            .expect("child was stored before streaming");

        let wait = wait_checked(child, stderr, cancel);

        outcome?;

        match wait {
            Ok(notices) => {
                // mongorestore reports per-collection failures on stderr. With
                // `--stopOnError` those also fail the process, which is why it
                // defaults on; without it they would arrive here attached to a
                // zero exit status.
                let failures: Vec<&String> =
                    notices.iter().filter(|l| l.contains("Failed:")).collect();
                if !failures.is_empty() && !self.options.stop_on_error {
                    return Err(RestoreError::Invalid(format!(
                        "mongorestore reported failures but exited successfully: {}",
                        failures
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    )));
                }
                for line in notices {
                    tracing::debug!("mongorestore: {line}");
                }
                Ok(())
            }
            Err(crate::exec::ExecError::Cancelled { .. }) => Err(RestoreError::Cancelled),
            Err(e) => Err(RestoreError::Exec(e)),
        }
    }

    /// Feed the archive into `mongorestore`, decrypting on the way if needed.
    ///
    /// Unlike the SQL restores this copies opaque bytes: an archive has no line
    /// structure to split on, so cancellation lands on a buffer boundary rather
    /// than a statement boundary. That is fine here — a half-applied archive is
    /// discarded by dropping the target database, which is what every caller
    /// that can cancel does.
    fn pump(
        &self,
        stdin: &mut std::process::ChildStdin,
        tx: &UnboundedSender<RestoreProgress>,
        cancel: &CancellationToken,
    ) -> Result<(), RestoreError> {
        use std::io::{Read, Write};

        let file = std::fs::File::open(&self.artifact)
            .map_err(|e| RestoreError::Invalid(format!("could not open the artifact: {e}")))?;

        // Counted on the outermost layer so progress tracks the bytes on disk,
        // which is the number the user can see. Decryption sits inside it; the
        // archive's own gzip is undone by mongorestore, not here.
        let counted = CountingReader::new(file);
        let counter = counted.counter.clone();
        let decoded = crate::crypto::artifact_reader(counted, self.identity.as_ref())
            .map_err(|e| RestoreError::Invalid(e.to_string()))?;
        let mut reader = std::io::BufReader::with_capacity(1 << 16, decoded);

        let mut buf = vec![0u8; 1 << 16];
        let mut last_report = 0u64;

        loop {
            if cancel.is_cancelled() {
                return Err(RestoreError::Cancelled);
            }

            let n = reader
                .read(&mut buf)
                .map_err(|e| RestoreError::Invalid(format!("reading the artifact failed: {e}")))?;
            if n == 0 {
                break;
            }

            stdin.write_all(&buf[..n]).map_err(|e| {
                RestoreError::Invalid(format!("mongorestore closed its input early: {e}"))
            })?;

            let done = counter.load(std::sync::atomic::Ordering::Relaxed);
            if done - last_report >= PROGRESS_EVERY {
                last_report = done;
                let _ = tx.send(RestoreProgress::Bytes {
                    done,
                    total: self.artifact_size,
                });
            }
        }

        let _ = tx.send(RestoreProgress::Bytes {
            done: self.artifact_size,
            total: self.artifact_size,
        });

        Ok(())
    }
}

/// Counts bytes read from the underlying reader, for progress.
struct CountingReader<R> {
    inner: R,
    counter: Arc<std::sync::atomic::AtomicU64>,
}

impl<R> CountingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

impl<R: std::io::Read> std::io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.counter
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
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
    use secrecy::SecretString;

    fn worker(naming: &TargetNaming) -> RestoreWorker {
        RestoreWorker {
            mongorestore: local_tool(Tool::Mongorestore),
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: 27018,
                user: "admin".into(),
                password: Some(SecretString::from("hunter2")),
            },
            source_database: "prod_app".into(),
            target: naming.resolve(chrono::Utc::now()),
            artifact: PathBuf::from("/tmp/app.archive.gz"),
            identity: None,
            artifact_size: 100,
            options: MongoRestoreOptions::default(),
            drop_collections: matches!(naming, TargetNaming::DropAndRecreate { .. }),
            gzipped: true,
        }
    }

    fn timestamped() -> TargetNaming {
        TargetNaming::NewTimestamped {
            prefix: "restore".into(),
        }
    }

    #[test]
    fn the_password_never_appears_in_the_command_line() {
        let rendered = worker(&timestamped()).command(&local_tool(Tool::Mongorestore), None).display();
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains("--password"), "{rendered}");
    }

    #[test]
    fn the_namespace_is_rewritten_from_the_source_to_the_target() {
        // Without this pair mongorestore puts every collection back into the
        // database it was dumped from — which on a production profile means
        // overwriting the source.
        let w = worker(&timestamped());
        let rendered = w.command(&local_tool(Tool::Mongorestore), None).display();
        assert!(rendered.contains("--nsFrom=prod_app.*"), "{rendered}");
        assert!(
            rendered.contains(&format!("--nsTo={}.*", w.target)),
            "{rendered}"
        );
    }

    #[test]
    fn a_selective_restore_filters_on_the_source_namespace() {
        // The archive holds source namespaces. Filtering on the target's name
        // would match nothing and restore an empty database while reporting
        // success.
        let mut w = worker(&timestamped());
        w.options.only_collections = vec!["orders".into()];
        let rendered = w.command(&local_tool(Tool::Mongorestore), None).display();
        assert!(rendered.contains("--nsInclude=prod_app.orders"), "{rendered}");
        assert!(
            !rendered.contains(&format!("--nsInclude={}.orders", w.target)),
            "{rendered}"
        );
    }

    #[test]
    fn a_timestamped_restore_never_drops() {
        let rendered = worker(&timestamped()).command(&local_tool(Tool::Mongorestore), None).display();
        assert!(
            !rendered.contains("--drop"),
            "the default restore must be non-destructive: {rendered}"
        );
    }

    #[test]
    fn drop_and_recreate_drops_collections() {
        let w = worker(&TargetNaming::DropAndRecreate {
            name: "staging".into(),
        });
        assert!(w.command(&local_tool(Tool::Mongorestore), None).display().contains("--drop"));
        assert_eq!(w.target, "staging");
    }

    #[test]
    fn a_partial_restore_is_a_failed_restore_by_default() {
        assert!(
            MongoRestoreOptions::default().stop_on_error,
            "without this mongorestore skips failed documents and exits 0"
        );
        assert!(worker(&timestamped()).command(&local_tool(Tool::Mongorestore), None).display().contains("--stopOnError"));
    }

    #[test]
    fn gzip_is_passed_only_for_an_archive_that_has_it() {
        assert!(worker(&timestamped()).command(&local_tool(Tool::Mongorestore), None).display().contains("--gzip"));
        let mut w = worker(&timestamped());
        w.gzipped = false;
        assert!(!w.command(&local_tool(Tool::Mongorestore), None).display().contains("--gzip"));
    }

    #[test]
    fn indexes_are_restored_unless_turned_off() {
        assert!(
            !worker(&timestamped())
                .command(&local_tool(Tool::Mongorestore), None)
                .display()
                .contains("--noIndexRestore")
        );
        let mut w = worker(&timestamped());
        w.options.restore_indexes = false;
        assert!(w.command(&local_tool(Tool::Mongorestore), None).display().contains("--noIndexRestore"));
    }

    #[test]
    fn the_tool_authenticates_against_the_same_database_as_the_driver() {
        let rendered = worker(&timestamped()).command(&local_tool(Tool::Mongorestore), None).display();
        assert!(rendered.contains(&format!("--authenticationDatabase={MONGO_AUTH_SOURCE}")));
    }

    #[test]
    fn counting_reader_tracks_consumed_bytes() {
        use std::io::Read;
        let data = b"hello world";
        let mut r = CountingReader::new(&data[..]);
        let counter = r.counter.clone();

        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();

        assert_eq!(out, data);
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::Relaxed),
            data.len() as u64
        );
    }
}
