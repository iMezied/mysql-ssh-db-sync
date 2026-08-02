//! Running external dump and restore tools.
//!
//! Two rules drive everything here:
//!
//! 1. **stderr is never discarded.** The bash predecessor sent it to
//!    `/dev/null`, so a dump that failed halfway produced a truncated file and
//!    a success message. Every line a child writes is captured and, on failure,
//!    surfaced in the error.
//! 2. **Credentials never appear in argv.** `ps` is world-readable; passwords
//!    go through the environment instead.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use secrecy::{ExposeSecret, SecretString};
use tokio_util::sync::CancellationToken;

/// How many stderr lines to keep for the error message.
///
/// Tools like `mysql` can emit a warning per statement; the tail is what
/// actually explains a failure.
const STDERR_TAIL_LINES: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("could not start {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} failed with {status}\n{stderr}")]
    Failed {
        program: String,
        status: String,
        stderr: String,
    },
    #[error("{program} was cancelled")]
    Cancelled { program: String },
    #[error("io error while running {program}: {source}")]
    Io {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// A child process that can be killed as a group.
#[derive(Debug)]
pub struct ChildHandle {
    inner: std::process::Child,
    program: String,
}

impl ChildHandle {
    pub fn id(&self) -> u32 {
        self.inner.id()
    }

    /// Terminate the child and anything it spawned.
    ///
    /// Killing only the direct child would orphan a pipeline's other members;
    /// the process group is what makes cancellation actually stop the work.
    pub fn kill_group(&mut self) {
        #[cfg(unix)]
        {
            let pid = self.inner.id() as i32;
            // Negative pid targets the whole group. SIGTERM first so the tool
            // can close its connection cleanly.
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
        }
        // Always follow up with a direct kill: on Windows there is no group
        // signal, and on Unix a child that ignores SIGTERM still has to go.
        let _ = self.inner.kill();
    }

    pub fn program(&self) -> &str {
        &self.program
    }
}

/// A command to run, with its secret kept out of argv.
pub struct ToolCommand {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    secret_env: Vec<(String, SecretString)>,
}

impl ToolCommand {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            secret_env: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Pass a secret through the environment.
    ///
    /// This is how `MYSQL_PWD` and `PGPASSWORD` are supplied. Environment
    /// variables are visible to the process itself and to root, but not to
    /// other users via `ps`, which is where `-p<password>` leaks.
    pub fn secret_env(mut self, key: impl Into<String>, value: SecretString) -> Self {
        self.secret_env.push((key.into(), value));
        self
    }

    /// The command line as it would be typed, for the job log.
    ///
    /// Safe to log: secrets are in the environment, never in argv.
    pub fn display(&self) -> String {
        let mut out = self.program.clone();
        for a in &self.args {
            out.push(' ');
            if a.contains(' ') {
                out.push_str(&format!("{a:?}"));
            } else {
                out.push_str(a);
            }
        }
        out
    }

    fn build(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        for (k, v) in &self.secret_env {
            cmd.env(k, v.expose_secret());
        }

        // Own process group, so cancellation can take down the whole pipeline.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        cmd
    }

    /// Spawn with stdout piped, stderr captured on a background thread.
    ///
    /// The caller owns the stdout stream — this is how a dump is piped through
    /// the DEFINER filter and into gzip without an intermediate file.
    pub fn spawn_streaming(
        &self,
    ) -> Result<(ChildHandle, std::process::ChildStdout, StderrCollector), ExecError> {
        let mut cmd = self.build();
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let mut child = cmd.spawn().map_err(|e| ExecError::Spawn {
            program: self.program.clone(),
            source: e,
        })?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let collector = StderrCollector::spawn(stderr);

        Ok((
            ChildHandle {
                inner: child,
                program: self.program.clone(),
            },
            stdout,
            collector,
        ))
    }

    /// Spawn with stdin piped, for feeding a restore.
    pub fn spawn_writing(
        &self,
    ) -> Result<(ChildHandle, std::process::ChildStdin, StderrCollector), ExecError> {
        let mut cmd = self.build();
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| ExecError::Spawn {
            program: self.program.clone(),
            source: e,
        })?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let collector = StderrCollector::spawn(stderr);

        Ok((
            ChildHandle {
                inner: child,
                program: self.program.clone(),
            },
            stdin,
            collector,
        ))
    }

    /// Run to completion and return stdout as text. For `--version` probes.
    pub fn output_text(&self) -> Result<String, ExecError> {
        let mut cmd = self.build();
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let out = cmd.output().map_err(|e| ExecError::Spawn {
            program: self.program.clone(),
            source: e,
        })?;

        if !out.status.success() {
            return Err(ExecError::Failed {
                program: self.program.clone(),
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }

        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Collects a child's stderr on a background thread.
///
/// Reading it inline would deadlock: a child that fills the stderr pipe blocks
/// while we are busy reading its stdout.
#[derive(Debug)]
pub struct StderrCollector {
    handle: std::thread::JoinHandle<VecDeque<String>>,
}

impl StderrCollector {
    fn spawn(stderr: std::process::ChildStderr) -> Self {
        let handle = std::thread::spawn(move || {
            use std::io::BufRead;
            let mut tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL_LINES);
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                if tail.len() == STDERR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            tail
        });
        Self { handle }
    }

    /// Wait for the child's stderr to close and return the last lines.
    pub fn finish(self) -> Vec<String> {
        self.handle
            .join()
            .map(|t| t.into_iter().collect())
            .unwrap_or_default()
    }
}

/// Wait for a child, mapping a non-zero exit to an error carrying its stderr.
///
/// `cancel` turns a killed child into `Cancelled` rather than a confusing
/// "exited with signal 15".
pub fn wait_checked(
    mut child: ChildHandle,
    stderr: StderrCollector,
    cancel: &CancellationToken,
) -> Result<Vec<String>, ExecError> {
    let status = child.inner.wait().map_err(|e| ExecError::Io {
        program: child.program.clone(),
        source: e,
    })?;

    let lines = stderr.finish();

    if cancel.is_cancelled() {
        return Err(ExecError::Cancelled {
            program: child.program.clone(),
        });
    }

    if !status.success() {
        return Err(ExecError::Failed {
            program: child.program.clone(),
            status: status.to_string(),
            stderr: lines.join("\n"),
        });
    }

    Ok(lines)
}

/// Locate a tool, honouring an explicit override.
///
/// Searches `PATH` and the usual install locations. We never bundle these
/// binaries — see DECISIONS.md on the GPL implications for `mysqldump`.
pub fn find_tool(binary: &str, override_path: Option<&str>) -> Option<std::path::PathBuf> {
    if let Some(explicit) = override_path {
        let p = Path::new(explicit);
        return p.is_file().then(|| p.to_path_buf());
    }

    if let Some(found) = search_path(binary) {
        return Some(found);
    }

    // Homebrew (both architectures), MacPorts, Postgres.app, and the usual
    // Linux locations. A GUI app launched from Finder does not inherit the
    // shell's PATH, so PATH alone is not enough on macOS.
    const EXTRA_DIRS: &[&str] = &[
        "/opt/homebrew/bin",
        "/opt/homebrew/opt/mysql-client/bin",
        "/opt/homebrew/opt/libpq/bin",
        "/usr/local/bin",
        "/usr/local/opt/mysql-client/bin",
        "/usr/local/opt/libpq/bin",
        "/usr/local/mysql/bin",
        "/opt/local/bin",
        "/usr/bin",
        "/usr/pgsql/bin",
    ];

    for dir in EXTRA_DIRS {
        let candidate = Path::new(dir).join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    // PostgreSQL on Linux installs versioned directories.
    for base in ["/usr/lib/postgresql", "/usr/local/pgsql"] {
        if let Ok(entries) = std::fs::read_dir(base) {
            let mut versions: Vec<_> = entries.flatten().map(|e| e.path()).collect();
            // Prefer the newest major version.
            versions.sort();
            for dir in versions.into_iter().rev() {
                let candidate = dir.join("bin").join(binary);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

fn search_path(binary: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| is_executable(candidate))
}

/// The Docker client, whose name differs on Windows.
const DOCKER_BIN: &str = if cfg!(windows) { "docker.exe" } else { "docker" };

/// Absolute directories every Docker distribution is known to install into.
///
/// Docker gets its own list rather than reusing [`find_tool`]'s because none of
/// these hold a database client, and none of `find_tool`'s hold Docker.
const DOCKER_DIRS: &[&str] = &[
    // Docker Desktop's system-wide symlink, and where a Linux package lands.
    "/usr/local/bin",
    "/usr/bin",
    // Homebrew's `docker` CLI on Apple silicon and Intel — what Colima uses.
    "/opt/homebrew/bin",
    "/opt/local/bin",
    // Docker Desktop itself, for an install whose symlink was never created
    // (it needs an admin password, and declining it is easy to do).
    "/Applications/Docker.app/Contents/Resources/bin",
];

/// The same, relative to the user's home directory.
///
/// Every modern runtime installs per-user by default, which is precisely the
/// place a stripped-down `PATH` cannot reach.
const DOCKER_HOME_DIRS: &[&str] = &[
    ".docker/bin",   // Docker Desktop, user-space install
    ".orbstack/bin", // OrbStack
    ".rd/bin",       // Rancher Desktop
];

/// Locate the `docker` client.
///
/// Kept separate from [`find_tool`] because the failure it prevents is a
/// different one, and nastier. A macOS app launched from Finder inherits
/// `/usr/bin:/bin:/usr/sbin:/sbin` and nothing else — not the shell's `PATH` —
/// and no Docker distribution installs its client into any of those four. The
/// result is a machine where `docker ps` works in every terminal, containers
/// are visibly running, and the app reports "could not start docker: No such
/// file or directory" ten seconds into a backup, with nothing on screen
/// suggesting the app simply could not see the binary.
///
/// `PATH` is still searched first: it is right whenever it has been set (the
/// CLI, a dev build, Linux), and it honours a deliberately non-standard
/// install that no hard-coded list could know about.
pub fn find_docker() -> Option<std::path::PathBuf> {
    if let Some(found) = search_path(DOCKER_BIN) {
        return Some(found);
    }

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let home_dirs = home.into_iter().flat_map(|home| {
        DOCKER_HOME_DIRS
            .iter()
            .map(move |dir| home.join(dir))
            .collect::<Vec<_>>()
    });

    DOCKER_DIRS
        .iter()
        .map(std::path::PathBuf::from)
        .chain(home_dirs)
        .map(|dir| dir.join(DOCKER_BIN))
        .find(|candidate| is_executable(candidate))
}

/// What to tell someone whose Docker we cannot find.
///
/// Names the searched locations rather than saying "install Docker", because
/// the overwhelmingly likely reader of this message has Docker installed and
/// running — see [`find_docker`] — and being told to install it again is worse
/// than useless.
pub fn docker_missing_message() -> String {
    let searched = DOCKER_DIRS
        .iter()
        .map(|d| (*d).to_string())
        .chain(DOCKER_HOME_DIRS.iter().map(|d| format!("~/{d}")))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "could not find the docker command. An app launched from Finder does not inherit \
         your shell's PATH, so Docker working in a terminal is not enough. Searched PATH \
         and: {searched}. If Docker lives somewhere else, switch Database tools to \
         \"Installed on this Mac\" and point the profile at the binaries directly."
    )
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Probe a tool's version by running it with `--version`.
pub fn probe_version(binary: &OsStr) -> Option<crate::tools::Version> {
    let text = ToolCommand::new(binary.to_string_lossy().into_owned())
        .arg("--version")
        .output_text()
        .ok()?;
    crate::tools::Version::parse_first(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_shows_arguments_but_no_secret() {
        let cmd = ToolCommand::new("mysqldump")
            .arg("--single-transaction")
            .arg("mydb")
            .secret_env("MYSQL_PWD", SecretString::from("hunter2"));

        let rendered = cmd.display();
        assert!(rendered.contains("mysqldump"));
        assert!(rendered.contains("--single-transaction"));
        assert!(
            !rendered.contains("hunter2"),
            "a password must never reach the job log, got {rendered}"
        );
    }

    #[test]
    fn arguments_containing_spaces_are_quoted() {
        let cmd = ToolCommand::new("mysqldump").arg("--where=id > 5");
        assert!(cmd.display().contains("\"--where=id > 5\""));
    }

    #[test]
    fn finds_a_tool_on_path() {
        // `sh` exists on every platform this runs on.
        assert!(find_tool("sh", None).is_some());
    }

    #[test]
    fn missing_tools_are_reported_as_absent() {
        assert!(find_tool("definitely-not-a-real-binary-xyz", None).is_none());
    }

    #[test]
    fn an_override_that_does_not_exist_is_not_silently_ignored() {
        // Falling back to PATH here would run a different binary than the user
        // asked for, which is worse than failing.
        assert!(find_tool("sh", Some("/nonexistent/sh")).is_none());
    }

    #[test]
    fn an_override_that_exists_wins() {
        let found = find_tool("sh", Some("/bin/sh")).expect("explicit path");
        assert_eq!(found, Path::new("/bin/sh"));
    }

    #[test]
    fn stderr_is_captured_not_discarded() {
        let cmd = ToolCommand::new("sh")
            .arg("-c")
            .arg("echo boom >&2; exit 3");

        let (child, _stdout, stderr) = cmd.spawn_streaming().expect("spawn");
        let err = wait_checked(child, stderr, &CancellationToken::new())
            .expect_err("non-zero exit must fail");

        match err {
            ExecError::Failed { stderr, status, .. } => {
                assert!(stderr.contains("boom"), "stderr must reach the error");
                assert!(status.contains('3'), "exit status should be reported");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn successful_commands_return_their_stderr_lines() {
        // Tools warn on stderr while succeeding; those lines belong in the log.
        let cmd = ToolCommand::new("sh").arg("-c").arg("echo warning >&2");
        let (child, _stdout, stderr) = cmd.spawn_streaming().expect("spawn");
        let lines = wait_checked(child, stderr, &CancellationToken::new()).expect("should succeed");
        assert_eq!(lines, vec!["warning".to_string()]);
    }

    #[test]
    fn a_large_stderr_does_not_deadlock() {
        // Reading stderr inline after stdout would hang once the pipe filled.
        let cmd = ToolCommand::new("sh")
            .arg("-c")
            .arg("i=0; while [ $i -lt 5000 ]; do echo padding line $i >&2; i=$((i+1)); done");

        let (child, _stdout, stderr) = cmd.spawn_streaming().expect("spawn");
        let lines = wait_checked(child, stderr, &CancellationToken::new()).expect("should succeed");

        assert_eq!(lines.len(), STDERR_TAIL_LINES, "only the tail is kept");
        assert!(lines.last().unwrap().contains("4999"));
    }

    #[test]
    fn cancellation_is_reported_as_cancelled_not_as_a_crash() {
        let cancel = CancellationToken::new();
        let cmd = ToolCommand::new("sh").arg("-c").arg("sleep 30");

        let (mut child, _stdout, stderr) = cmd.spawn_streaming().expect("spawn");
        cancel.cancel();
        child.kill_group();

        let err = wait_checked(child, stderr, &cancel).expect_err("killed child");
        assert!(
            matches!(err, ExecError::Cancelled { .. }),
            "a cancelled job must not look like a failure, got {err:?}"
        );
    }

    #[test]
    fn killing_the_group_takes_down_grandchildren() {
        // `sh -c` with a background child: killing only the direct child would
        // leave the sleep running.
        let cmd = ToolCommand::new("sh")
            .arg("-c")
            .arg("sleep 30 & echo $!; wait");

        let (mut child, stdout, stderr) = cmd.spawn_streaming().expect("spawn");

        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("grandchild pid");
        let grandchild: i32 = line.trim().parse().expect("pid");

        child.kill_group();
        let _ = wait_checked(child, stderr, &CancellationToken::new());

        // Give the signal a moment to land.
        std::thread::sleep(std::time::Duration::from_millis(300));

        #[cfg(unix)]
        {
            // kill(pid, 0) probes existence without signalling.
            let alive = unsafe { libc::kill(grandchild, 0) } == 0;
            assert!(!alive, "grandchild {grandchild} survived the group kill");
        }
    }

    #[test]
    fn docker_is_looked_for_by_path_not_by_name() {
        // Machine-independent: a runner may have no Docker at all. What must
        // hold either way is that a hit is an absolute, runnable path — a bare
        // "docker" would mean we handed the lookup back to `PATH`, which is
        // the bug (a Finder-launched app inherits /usr/bin:/bin:/usr/sbin:/sbin
        // and no Docker distribution installs into any of them).
        let Some(found) = find_docker() else { return };

        assert!(found.is_absolute(), "not runnable without a PATH: {found:?}");
        assert!(is_executable(&found), "found something unrunnable: {found:?}");
        assert_eq!(
            found.file_stem().and_then(|n| n.to_str()),
            Some("docker"),
            "found the wrong binary: {found:?}"
        );
    }

    #[test]
    fn the_docker_search_covers_the_runtimes_people_actually_have() {
        // Docker Desktop's symlink, Homebrew's client, and the per-user
        // installs OrbStack and Rancher Desktop default to. Each was a real
        // report of "Docker is running but the app says it is not".
        for dir in ["/usr/local/bin", "/opt/homebrew/bin"] {
            assert!(DOCKER_DIRS.contains(&dir), "{dir} is not searched");
        }
        for dir in [".docker/bin", ".orbstack/bin", ".rd/bin"] {
            assert!(DOCKER_HOME_DIRS.contains(&dir), "~/{dir} is not searched");
        }
    }

    #[test]
    fn the_missing_docker_message_says_where_it_looked() {
        let msg = docker_missing_message();
        // Naming the locations is the whole value: the reader almost certainly
        // has Docker installed, so "install Docker" would be wrong advice.
        assert!(msg.contains("PATH"), "{msg}");
        assert!(msg.contains("/usr/local/bin"), "{msg}");
        assert!(msg.contains("~/.orbstack/bin"), "{msg}");
        assert!(
            !msg.to_lowercase().contains("install docker"),
            "misleading advice for a machine that already has it: {msg}"
        );
    }

    #[test]
    fn spawning_a_missing_program_is_a_clear_error() {
        let err = ToolCommand::new("definitely-not-a-real-binary-xyz")
            .spawn_streaming()
            .expect_err("should not spawn");
        assert!(matches!(err, ExecError::Spawn { .. }));
    }

    #[test]
    fn version_probing_reads_tool_output() {
        // `sh --version` is not universal, so probe something that is: use the
        // command runner directly against a known output.
        let text = ToolCommand::new("sh")
            .arg("-c")
            .arg("echo 'mysqldump  Ver 8.0.42 for macos14'")
            .output_text()
            .expect("run");
        let v = crate::tools::Version::parse_first(&text).expect("parse");
        assert_eq!(v, crate::tools::Version::new(8, 0, 42));
    }
}
