//! MySQL restore execution.
//!
//! Streams `artifact → gunzip → mysql`. The session optimisations wrapped
//! around the stream are what make a large restore finish in minutes rather
//! than hours, and they are re-enabled at the end rather than left off.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::{EngineRestoreOptions, MysqlRestoreOptions, RestoreError, RestoreRequest};
use crate::backup::mysql::Endpoint;
use crate::events::JobPhase;
use crate::exec::{ChildHandle, ToolCommand, find_tool, wait_checked};
use crate::job::JobContext;
use crate::manifest::BackupManifest;
use crate::profile::ConnectionProfile;

/// Progress from the blocking restore worker.
enum RestoreProgress {
    Phase(JobPhase, String),
    Bytes { done: u64, total: u64 },
}

/// How often to report streaming progress, in bytes.
const PROGRESS_EVERY: u64 = 4 * 1024 * 1024;

/// Run a MySQL restore, returning the database that was written to.
pub async fn run_mysql_restore(
    profile: &ConnectionProfile,
    request: &RestoreRequest,
    endpoint: Endpoint,
    ctx: &JobContext,
) -> Result<String, RestoreError> {
    let manifest = BackupManifest::read(&request.artifact_path).ok();
    request.validate(profile, manifest.as_ref())?;

    // Trust the bytes on disk over the manifest. A manifest can be absent,
    // stale or hand-edited; the age header cannot lie about what the file is,
    // and getting this wrong means either feeding ciphertext to `mysql` or
    // silently skipping decryption.
    let identity = if crate::crypto::looks_encrypted(&request.artifact_path) {
        let key = crate::backupkey::identity().map_err(|_| {
            let recipients = manifest
                .as_ref()
                .map(|m| m.encryption_recipients.join(", "))
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "(not recorded)".into());
            RestoreError::Invalid(format!(
                "this artifact is encrypted and no backup key is available on this machine. It                  was encrypted to: {recipients}. Import that key in Settings before restoring."
            ))
        })?;
        Some(key)
    } else {
        if manifest.as_ref().is_some_and(|m| m.encrypted) {
            // The manifest says encrypted and the file is not. Something
            // replaced the artifact; restoring it anyway would be restoring
            // something nobody vouched for.
            return Err(RestoreError::Invalid(
                "the manifest says this artifact is encrypted, but the file is not. It has been                  replaced or corrupted; refusing to restore it."
                    .into(),
            ));
        }
        None
    };

    let EngineRestoreOptions::Mysql(options) = &request.engine else {
        return Err(RestoreError::EngineMismatch {
            profile: profile.engine,
            options: request.engine.engine(),
        });
    };

    let mysql = find_tool("mysql", profile.tool_overrides.mysql.as_deref()).ok_or_else(|| {
        RestoreError::Invalid(
            "could not find the mysql client; install it or set an override".into(),
        )
    })?;

    // Corruption must be caught before a destination database is touched.
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

    let target = request.naming.resolve(chrono::Utc::now());

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
        mysql,
        endpoint,
        target: target.clone(),
        artifact: request.artifact_path.clone(),
        identity,
        artifact_size,
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

struct RestoreWorker {
    mysql: PathBuf,
    endpoint: Endpoint,
    target: String,
    artifact: PathBuf,
    /// Present when the artifact is encrypted; the key that reads it.
    identity: Option<secrecy::SecretString>,
    artifact_size: u64,
    options: MysqlRestoreOptions,
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

        let _ = tx.send(RestoreProgress::Phase(
            JobPhase::Restore,
            "streaming the dump".into(),
        ));
        self.stream_dump(&tx, &current_child, &cancel)
    }

    fn base_command(&self) -> ToolCommand {
        let mut cmd = ToolCommand::new(self.mysql.display().to_string()).args([
            format!("--host={}", self.endpoint.host),
            format!("--port={}", self.endpoint.port),
            format!("--user={}", self.endpoint.user),
            "--protocol=TCP".to_string(),
            format!("--default-character-set={}", self.options.charset),
        ]);
        if let Some(pw) = &self.endpoint.password {
            cmd = cmd.secret_env("MYSQL_PWD", pw.clone());
        }
        cmd
    }

    /// Create (and optionally drop) the destination database.
    ///
    /// The name is quoted, not interpolated: a prefix is user-supplied.
    fn create_target(
        &self,
        current_child: &Arc<Mutex<Option<ChildHandle>>>,
        cancel: &CancellationToken,
    ) -> Result<(), RestoreError> {
        let quoted = crate::db::quote_mysql_ident(&self.target)
            .map_err(|e| RestoreError::Invalid(e.to_string()))?;

        let mut sql = String::new();
        if self.drop_first {
            sql.push_str(&format!("DROP DATABASE IF EXISTS {quoted};"));
        }
        sql.push_str(&format!(
            "CREATE DATABASE {quoted} CHARACTER SET {} COLLATE {};",
            self.options.charset, self.options.collation
        ));

        let cmd = self.base_command().arg("--execute").arg(sql);
        self.run_to_completion(cmd, current_child, cancel)
    }

    fn stream_dump(
        &self,
        tx: &UnboundedSender<RestoreProgress>,
        current_child: &Arc<Mutex<Option<ChildHandle>>>,
        cancel: &CancellationToken,
    ) -> Result<(), RestoreError> {
        let cmd = self.base_command().arg(self.target.clone());
        let (child, mut stdin, stderr) = cmd.spawn_writing()?;
        {
            *current_child.blocking_lock() = Some(child);
        }

        let outcome = self.pump(&mut stdin, tx, cancel);
        // Closing stdin is what tells `mysql` the input is finished; without
        // this it waits forever and the job never completes.
        drop(stdin);

        let child = current_child
            .blocking_lock()
            .take()
            .expect("child was stored before streaming");

        let wait = wait_checked(child, stderr, cancel);

        // Report the streaming error first when there is one: it is closer to
        // the cause than "mysql exited 1".
        outcome?;

        match wait {
            Ok(warnings) => {
                for line in warnings {
                    tracing::debug!("mysql: {line}");
                }
                Ok(())
            }
            Err(crate::exec::ExecError::Cancelled { .. }) => Err(RestoreError::Cancelled),
            Err(e) => Err(RestoreError::Exec(e)),
        }
    }

    /// Feed the decompressed dump into the client, wrapped in session settings.
    fn pump(
        &self,
        stdin: &mut std::process::ChildStdin,
        tx: &UnboundedSender<RestoreProgress>,
        cancel: &CancellationToken,
    ) -> Result<(), RestoreError> {
        let o = &self.options;

        // Preamble: these three together are worth 5-10x on a large import.
        let mut preamble = String::new();
        if o.foreign_key_checks_off {
            // Also required for correctness, not just speed: the fixture has a
            // foreign-key cycle that cannot be inserted in any order.
            preamble.push_str("SET FOREIGN_KEY_CHECKS=0;\n");
        }
        if o.unique_checks_off {
            preamble.push_str("SET UNIQUE_CHECKS=0;\n");
        }
        if o.autocommit_off {
            preamble.push_str("SET AUTOCOMMIT=0;\n");
        }
        if o.disable_binlog {
            preamble.push_str("SET SQL_LOG_BIN=0;\n");
        }
        stdin
            .write_all(preamble.as_bytes())
            .map_err(|e| RestoreError::Invalid(format!("could not write to mysql: {e}")))?;

        let file = std::fs::File::open(&self.artifact)
            .map_err(|e| RestoreError::Invalid(format!("could not open the artifact: {e}")))?;

        // Progress is measured against the compressed size, which is what the
        // user sees on disk.
        // Counting happens on the *outermost* layer so progress tracks the
        // bytes actually on disk, which is the number the user can see.
        // Decryption, then decompression, sit inside it.
        let counted = CountingReader::new(file);
        let counter = counted.counter.clone();
        let decoded = crate::crypto::artifact_reader(counted, self.identity.as_ref())
            .map_err(|e| RestoreError::Invalid(e.to_string()))?;
        let mut reader = std::io::BufReader::with_capacity(1 << 16, decoded);

        let mut buf = Vec::with_capacity(1 << 16);
        let mut last_report = 0u64;

        loop {
            if cancel.is_cancelled() {
                return Err(RestoreError::Cancelled);
            }

            buf.clear();
            // Split on newlines so a cancellation lands between statements
            // rather than mid-statement.
            let n = reader
                .read_until(b'\n', &mut buf)
                .map_err(|e| RestoreError::Invalid(format!("reading the artifact failed: {e}")))?;
            if n == 0 {
                break;
            }

            stdin
                .write_all(&buf)
                .map_err(|e| RestoreError::Invalid(format!("mysql closed its input early: {e}")))?;

            let done = counter.load(std::sync::atomic::Ordering::Relaxed);
            if done - last_report >= PROGRESS_EVERY {
                last_report = done;
                let _ = tx.send(RestoreProgress::Bytes {
                    done,
                    total: self.artifact_size,
                });
            }
        }

        // Postamble: put the session back and commit. Leaving checks off would
        // silently weaken the destination for the rest of the connection.
        let mut postamble = String::new();
        if o.autocommit_off {
            postamble.push_str("COMMIT;\n");
        }
        if o.unique_checks_off {
            postamble.push_str("SET UNIQUE_CHECKS=1;\n");
        }
        if o.foreign_key_checks_off {
            postamble.push_str("SET FOREIGN_KEY_CHECKS=1;\n");
        }
        if o.autocommit_off {
            postamble.push_str("SET AUTOCOMMIT=1;\n");
        }
        stdin
            .write_all(postamble.as_bytes())
            .map_err(|e| RestoreError::Invalid(format!("could not finish the restore: {e}")))?;

        let _ = tx.send(RestoreProgress::Bytes {
            done: self.artifact_size,
            total: self.artifact_size,
        });

        Ok(())
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

/// Path helper for locating an artifact's manifest.
pub fn manifest_path_for(artifact: &Path) -> PathBuf {
    BackupManifest::path_for(artifact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restore::TargetNaming;
    use secrecy::SecretString;

    fn worker(naming: &TargetNaming) -> RestoreWorker {
        RestoreWorker {
            identity: None,
            mysql: PathBuf::from("/usr/bin/mysql"),
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: 13307,
                user: "root".into(),
                password: Some(SecretString::from("hunter2")),
            },
            target: naming.resolve(chrono::Utc::now()),
            artifact: PathBuf::from("/tmp/a.sql.gz"),
            artifact_size: 100,
            options: MysqlRestoreOptions::default(),
            drop_first: naming.is_destructive(),
            create_database: !matches!(naming, TargetNaming::IntoExisting { .. }),
        }
    }

    #[test]
    fn the_password_never_appears_in_the_command_line() {
        let w = worker(&TargetNaming::NewTimestamped {
            prefix: "restore".into(),
        });
        let rendered = w.base_command().display();
        assert!(!rendered.contains("hunter2"));
        for token in rendered.split(' ') {
            assert!(
                token != "-p" && !(token.starts_with("-p") && !token.starts_with("--")),
                "argument {token:?} looks like an inline password flag"
            );
        }
    }

    #[test]
    fn a_timestamped_restore_creates_but_never_drops() {
        let w = worker(&TargetNaming::NewTimestamped {
            prefix: "restore".into(),
        });
        assert!(w.create_database);
        assert!(!w.drop_first, "the default restore must be non-destructive");
    }

    #[test]
    fn drop_and_recreate_drops_first() {
        let w = worker(&TargetNaming::DropAndRecreate {
            name: "staging".into(),
        });
        assert!(w.drop_first);
        assert!(w.create_database);
        assert_eq!(w.target, "staging");
    }

    #[test]
    fn restoring_into_an_existing_database_does_not_create_it() {
        let w = worker(&TargetNaming::IntoExisting {
            name: "existing".into(),
        });
        assert!(!w.create_database, "the database is already there");
        assert!(!w.drop_first);
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
