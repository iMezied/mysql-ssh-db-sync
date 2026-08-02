//! Discovery and version checking for external client binaries.
//!
//! We shell out to the vendors' own dump/restore tools rather than
//! reimplementing their formats. We do NOT bundle Oracle's `mysqldump`: it is
//! GPLv2 and bundling would impose that licence on the whole app. Binaries are
//! discovered on the host and may be overridden per profile.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::types::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Mysqldump,
    Mysql,
    Mydumper,
    Myloader,
    PgDump,
    PgDumpall,
    PgRestore,
    Psql,
    Mongodump,
    Mongorestore,
}

impl Tool {
    /// Every tool, so discovery and the settings page cannot drift from the
    /// enum by forgetting to list one.
    pub const ALL: [Tool; 10] = [
        Tool::Mysqldump,
        Tool::Mysql,
        Tool::Mydumper,
        Tool::Myloader,
        Tool::PgDump,
        Tool::PgDumpall,
        Tool::PgRestore,
        Tool::Psql,
        Tool::Mongodump,
        Tool::Mongorestore,
    ];

    pub const fn binary_name(self) -> &'static str {
        match self {
            Tool::Mysqldump => "mysqldump",
            Tool::Mysql => "mysql",
            Tool::Mydumper => "mydumper",
            Tool::Myloader => "myloader",
            Tool::PgDump => "pg_dump",
            Tool::PgDumpall => "pg_dumpall",
            Tool::PgRestore => "pg_restore",
            Tool::Psql => "psql",
            Tool::Mongodump => "mongodump",
            Tool::Mongorestore => "mongorestore",
        }
    }

    pub const fn engine(self) -> Engine {
        match self {
            Tool::Mysqldump | Tool::Mysql | Tool::Mydumper | Tool::Myloader => Engine::Mysql,
            Tool::PgDump | Tool::PgDumpall | Tool::PgRestore | Tool::Psql => Engine::Postgres,
            Tool::Mongodump | Tool::Mongorestore => Engine::Mongo,
        }
    }

    /// Tools without which the engine cannot function at all.
    pub const fn is_required(self) -> bool {
        matches!(
            self,
            Tool::Mysqldump
                | Tool::Mysql
                | Tool::PgDump
                | Tool::PgRestore
                | Tool::Psql
                | Tool::Mongodump
                | Tool::Mongorestore
        )
    }

    /// The environment variable this tool reads its password from.
    ///
    /// Needed by name, not by value: a containerised tool only receives the
    /// variables `docker` is told to forward, so the variable has to be listed
    /// on the docker command line while the secret itself stays in the
    /// environment. `docker run -e NAME` (no `=`) forwards the caller's value,
    /// which is what keeps the password out of argv — see [`crate::exec`].
    ///
    /// MongoDB's tools take credentials as arguments or in a URI rather than
    /// from the environment, so they have none.
    pub const fn password_env(self) -> Option<&'static str> {
        match self {
            Tool::Mysqldump | Tool::Mysql | Tool::Mydumper | Tool::Myloader => Some("MYSQL_PWD"),
            Tool::PgDump | Tool::PgDumpall | Tool::PgRestore | Tool::Psql => Some("PGPASSWORD"),
            Tool::Mongodump | Tool::Mongorestore => None,
        }
    }

    /// The Homebrew formula that provides this tool, if there is one.
    ///
    /// `mysql-client` and `libpq` are both keg-only, so Homebrew deliberately
    /// does not symlink them onto `PATH`. That is fine here — the extra
    /// directories searched by [`crate::exec::find_tool`] include both opt
    /// prefixes — but it does mean "installed" and "on PATH" are different
    /// questions for these two.
    pub const fn brew_formula(self) -> Option<&'static str> {
        match self {
            Tool::Mysqldump | Tool::Mysql => Some("mysql-client"),
            Tool::PgDump | Tool::PgDumpall | Tool::PgRestore | Tool::Psql => Some("libpq"),
            Tool::Mongodump | Tool::Mongorestore => {
                Some("mongodb/brew/mongodb-database-tools")
            }
            Tool::Mydumper | Tool::Myloader => Some("mydumper"),
        }
    }
}

/// Where the external client binaries come from.
///
/// Global rather than per-profile: this describes the machine the app is
/// running on, not the database being talked to. A per-profile binary override
/// still wins over whatever is set here — that one *is* about the database,
/// which is why the two coexist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolSource {
    /// Binaries installed on this machine.
    ///
    /// The default, and the only one that works with no container runtime
    /// present. Changing the default would silently re-route every existing
    /// install through Docker.
    Local,
    /// Borrow the binaries from a container that is already running.
    ///
    /// Nothing to download when a database container is already there, and the
    /// most fragile of the three: it stops working the moment that container
    /// is stopped or replaced, and the client version is whatever that image
    /// happens to ship.
    DockerExec {
        container: String,
        /// Directory holding the binaries inside the container. `None` means
        /// they are on the container's `PATH`, which is the usual case.
        #[serde(default)]
        bin_dir: Option<String>,
    },
    /// Run a throwaway container from an image, one per invocation.
    ///
    /// Costs an image pull once and then always has the right client, with no
    /// dependency on anything else still running.
    DockerRun { image: String },
}

impl Default for ToolSource {
    fn default() -> Self {
        Self::Local
    }
}

/// The hostname a container uses to reach the machine hosting it.
///
/// Provided by Docker Desktop on macOS and Windows; on Linux it exists only
/// when the container was started with `--add-host`, which [`ToolSource`] does
/// for the containers it starts itself.
pub const DOCKER_HOST_ALIAS: &str = "host.docker.internal";

impl ToolSource {
    pub const fn is_docker(&self) -> bool {
        matches!(self, ToolSource::DockerExec { .. } | ToolSource::DockerRun { .. })
    }

    /// Whether the machinery this source depends on is actually reachable.
    ///
    /// Checked before a job starts rather than left to the first spawn. The
    /// Docker sources fail at spawn time otherwise, which on a large database
    /// is a minute of connecting, tunnelling and introspecting spent to earn
    /// the right to fail on a `stat` — and the message that comes back is
    /// `No such file or directory (os error 2)`, which does not name Docker,
    /// does not name the setting that chose it, and is wrong about what is
    /// missing.
    pub fn preflight(&self) -> Result<(), String> {
        if self.is_docker() && crate::exec::find_docker().is_none() {
            return Err(crate::exec::docker_missing_message());
        }
        Ok(())
    }

    /// Rewrite a hostname so a containerised tool reaches the same server.
    ///
    /// This is the whole reason tunnelled backups need special handling: an SSH
    /// tunnel's local end is a loopback port on the *host*, and inside a
    /// container loopback is the container itself. Left alone, a tunnelled dump
    /// through Docker connects to nothing and fails with "connection refused"
    /// pointing at a port that is demonstrably open.
    ///
    /// Public hostnames are returned untouched.
    pub fn rewrite_host(&self, host: &str) -> String {
        if !self.is_docker() || !is_loopback(host) {
            return host.to_string();
        }
        DOCKER_HOST_ALIAS.to_string()
    }
}

/// Whether a host refers to the local machine.
///
/// The IPv6 forms matter: a tunnel bound to `::1` is written that way in some
/// configurations, and missing it produces exactly the silent failure
/// [`ToolSource::rewrite_host`] exists to prevent.
fn is_loopback(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    matches!(bare, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")
        || bare.starts_with("127.")
}

/// A located tool, and everything needed to invoke it.
///
/// Replaces the bare `PathBuf` the dump and restore code used to carry. The
/// difference matters because a containerised tool has no host path at all:
/// what it has is a command line that happens to start with `docker`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTool {
    tool: Tool,
    location: Location,
    /// Host paths the tool must be able to see. Empty for a local tool, which
    /// already shares this machine's filesystem.
    mounts: Vec<Mount>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Location {
    Local(PathBuf),
    DockerExec { container: String, program: String },
    DockerRun { image: String, program: String },
}

/// Whether the tool may write through a mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountMode {
    /// An input the tool reads: an archive to restore, a credentials file, a
    /// table-of-contents filter.
    ReadOnly,
    /// An output directory the tool writes into — `pg_dump -Fc`/`-Fd`, which
    /// produce their archive themselves rather than streaming it.
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Mount {
    host: PathBuf,
    container: String,
    mode: MountMode,
}

/// Where mounted paths appear inside the container.
///
/// Under a directory of our own rather than at the same path as on the host:
/// a host path like `/Users/...` may collide with something real in the image,
/// and a temp file's name is not meaningful to the tool anyway.
const MOUNT_ROOT: &str = "/mnt/dbsync";

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    /// `docker exec` joins a container that is already running, so there is no
    /// point at which a bind mount could be added.
    ///
    /// Refused rather than worked around. `docker cp` would copy the file in,
    /// but for the file this most often carries — a password — that means
    /// leaving a credential inside a container this application does not own
    /// and did not start.
    #[error(
        "{what} has to be handed to {tool} as a file, and a tool borrowed from an \
         already-running container cannot be given one. Use an image-based tool source, \
         or install the client tools locally."
    )]
    NotSupported { tool: &'static str, what: String },
}

impl ResolvedTool {
    /// Work out how to run `tool`, or `None` when it cannot be found.
    ///
    /// `override_path` is a path on *this* machine, so setting one forces
    /// local execution even when the global source is Docker. That is the
    /// less surprising of the two readings: someone who points a profile at
    /// `/usr/local/mysql-5.7/bin/mysqldump` is naming a specific binary, and
    /// silently running a different one out of a container would defeat the
    /// only reason to set it.
    ///
    /// The Docker variants cannot be verified without invoking Docker, which
    /// is far too slow for a lookup that happens per command. They are checked
    /// by [`ToolSource`]'s test action instead, and a wrong container or image
    /// surfaces as a spawn failure with Docker's own message.
    pub fn resolve(tool: Tool, source: &ToolSource, override_path: Option<&str>) -> Option<Self> {
        let binary = tool.binary_name();

        if let Some(explicit) = override_path {
            let path = Path::new(explicit);
            return path
                .is_file()
                .then(|| Self {
                    tool,
                    location: Location::Local(path.to_path_buf()),
                    mounts: Vec::new(),
                });
        }

        let location = match source {
            ToolSource::Local => Location::Local(crate::exec::find_tool(binary, None)?),
            ToolSource::DockerExec { container, bin_dir } => Location::DockerExec {
                container: container.clone(),
                program: in_container_path(bin_dir.as_deref(), binary),
            },
            ToolSource::DockerRun { image } => Location::DockerRun {
                image: image.clone(),
                program: in_container_path(None, binary),
            },
        };
        Some(Self { tool, location, mounts: Vec::new() })
    }

    /// Make a host path visible to the tool, returning the path the tool must
    /// use to reach it.
    ///
    /// For a local tool this is the identity function: the tool already shares
    /// this filesystem, and the returned path is the one passed in. For a
    /// containerised tool it records a bind mount and returns the path *inside*
    /// the container.
    ///
    /// Every place that hands a filename to a client tool has to go through
    /// here, and the reason is that forgetting fails quietly in the worst
    /// direction. `pg_dump -Fc -f /Users/me/backups/app.dump` inside a
    /// container writes a perfectly good archive — into the container, which is
    /// then discarded. The host is left with nothing, and the failure surfaces
    /// as a missing file long after the dump reported success.
    pub fn mount(
        &mut self,
        host: impl AsRef<Path>,
        mode: MountMode,
    ) -> Result<PathBuf, MountError> {
        let host = host.as_ref();

        match &self.location {
            Location::Local(_) => Ok(host.to_path_buf()),
            Location::DockerExec { .. } => Err(MountError::NotSupported {
                tool: self.tool.binary_name(),
                what: host
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| host.display().to_string()),
            }),
            Location::DockerRun { .. } => {
                // Indexed so two mounts with the same basename — an artifact
                // and its manifest, say — cannot land on top of each other.
                let index = self.mounts.len();
                let name = host
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "mounted".to_string());
                let container = format!("{MOUNT_ROOT}/{index}/{name}");

                self.mounts.push(Mount {
                    host: host.to_path_buf(),
                    container: container.clone(),
                    mode,
                });
                Ok(PathBuf::from(container))
            }
        }
    }

    /// `-v host:container[:ro]` for each recorded mount.
    fn mount_args(&self) -> Vec<String> {
        self.mounts
            .iter()
            .flat_map(|m| {
                let suffix = match m.mode {
                    MountMode::ReadOnly => ":ro",
                    MountMode::ReadWrite => "",
                };
                [
                    "-v".to_string(),
                    format!("{}:{}{suffix}", m.host.display(), m.container),
                ]
            })
            .collect()
    }

    /// A command with the tool selected but no arguments yet.
    ///
    /// Callers append the tool's own flags with `.args(..)`, which lands them
    /// after the image or container name — exactly where Docker expects the
    /// command's arguments. That is why none of the dump argument-building
    /// code had to change to gain container support.
    pub fn command(&self) -> crate::exec::ToolCommand {
        use crate::exec::ToolCommand;

        match &self.location {
            Location::Local(path) => ToolCommand::new(path.display().to_string()),
            Location::DockerExec { container, program } => {
                let mut args = vec!["exec".to_string(), "-i".to_string()];
                args.extend(self.forwarded_env());
                args.push(container.clone());
                args.push(program.clone());
                docker_command().args(args)
            }
            Location::DockerRun { image, program } => {
                let mut args = vec![
                    "run".to_string(),
                    "--rm".to_string(),
                    "-i".to_string(),
                    // Makes `host.docker.internal` resolve on Linux, where it
                    // is not built in. Harmless on Docker Desktop, which
                    // provides it anyway — and without it every tunnelled
                    // backup on Linux fails.
                    format!("--add-host={DOCKER_HOST_ALIAS}:host-gateway"),
                ];
                args.extend(self.forwarded_env());
                // Before the image name: everything after it belongs to the
                // command, not to `docker run`.
                args.extend(self.mount_args());
                args.push(image.clone());
                args.push(program.clone());
                docker_command().args(args)
            }
        }
    }

    /// `-e NAME` for the password variable, with no `=value`.
    ///
    /// Docker reads the value from its own environment, where
    /// [`crate::exec::ToolCommand::secret_env`] put it. The name appears in
    /// argv; the password does not.
    fn forwarded_env(&self) -> Vec<String> {
        match self.tool.password_env() {
            Some(var) => vec!["-e".to_string(), var.to_string()],
            None => Vec::new(),
        }
    }

    pub const fn tool(&self) -> Tool {
        self.tool
    }

    /// Ask the tool its version.
    ///
    /// Goes through [`Self::command`] rather than executing a path directly,
    /// because for a containerised tool the version that matters is the one
    /// inside the container — which is frequently not the one on the host, and
    /// is what the compatibility checks in this module need to reason about.
    pub fn probe_version(&self) -> Option<Version> {
        let text = self.command().arg("--version").output_text().ok()?;
        Version::parse_first(&text)
    }

    /// Whether this runs inside a container, and so needs a rewritten host.
    pub const fn is_containerised(&self) -> bool {
        !matches!(self.location, Location::Local(_))
    }

    /// The host path, when there is one. `None` for containerised tools.
    pub fn local_path(&self) -> Option<&Path> {
        match &self.location {
            Location::Local(p) => Some(p),
            _ => None,
        }
    }

    /// How to describe this in a job log.
    pub fn display(&self) -> String {
        match &self.location {
            Location::Local(p) => p.display().to_string(),
            Location::DockerExec { container, program } => {
                format!("docker exec {container} {program}")
            }
            Location::DockerRun { image, program } => format!("docker run {image} {program}"),
        }
    }
}

/// A command that runs the Docker client, found rather than left to `PATH`.
///
/// Resolved per command instead of once at [`ResolvedTool::resolve`] time: it
/// is a handful of `stat` calls against a process that runs for minutes, and
/// resolving late means installing Docker while the app is open takes effect
/// without a restart.
///
/// Falls back to the bare name when nothing is found, so the failure stays a
/// spawn error rather than a panic. Reaching that fallback in a job means
/// [`ToolSource::preflight`] was not called, and the message will be the poor
/// one that preflight exists to replace.
fn docker_command() -> crate::exec::ToolCommand {
    let program = crate::exec::find_docker()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "docker".to_string());
    crate::exec::ToolCommand::new(program)
}

fn in_container_path(bin_dir: Option<&str>, binary: &str) -> String {
    match bin_dir.map(str::trim).filter(|d| !d.is_empty()) {
        Some(dir) => format!("{}/{binary}", dir.trim_end_matches('/')),
        None => binary.to_string(),
    }
}

/// What discovery found for one tool, in the shape the settings page needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ToolStatus {
    pub tool: Tool,
    pub binary: String,
    /// How it would be invoked — a host path, or a `docker …` line.
    pub location: Option<String>,
    pub version: Option<String>,
    /// Whether its absence stops an engine working at all.
    pub required: bool,
    /// The Homebrew formula that would provide it, if Homebrew is available.
    pub brew_formula: Option<String>,
}

impl ToolStatus {
    pub const fn found(&self) -> bool {
        self.location.is_some()
    }
}

/// Look for every tool through a given source.
///
/// Version probes actually run each binary, which for a container source means
/// starting a container per tool. That is slow enough to matter — call it off
/// the UI thread and show a spinner, rather than on every render.
pub fn discover(source: &ToolSource) -> Vec<ToolStatus> {
    Tool::ALL
        .into_iter()
        .map(|tool| {
            let resolved = ResolvedTool::resolve(tool, source, None);
            // Probed once and reused. For a container source each probe starts
            // a container, so asking twice would double the cost of opening
            // the settings page.
            let version = resolved.as_ref().and_then(|r| r.probe_version());

            // For a container source `resolve` always succeeds — there is no
            // filesystem to check — so whether the tool is really *there* is
            // only answered by having run it. A local tool that fails to
            // report a version is still installed, and still usable.
            let present = resolved
                .as_ref()
                .is_some_and(|r| !r.is_containerised() || version.is_some());

            ToolStatus {
                tool,
                binary: tool.binary_name().to_string(),
                version: version.map(|v| v.to_string()),
                location: resolved.filter(|_| present).map(|r| r.display()),
                required: tool.is_required(),
                brew_formula: tool.brew_formula().map(str::to_string),
            }
        })
        .collect()
}

/// A container that is currently running, for the exec source picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DockerContainer {
    pub name: String,
    pub image: String,
}

/// List running containers.
///
/// Three outcomes, deliberately distinct: the client is not where we can see
/// it, the client answered with nothing running, and the client answered with
/// containers. "Docker is not installed", "Docker is installed but this app
/// cannot find it" and "Docker is up but empty" want completely different
/// advice, and collapsing them produces the unhelpful "no containers found"
/// for a machine with a running database container in it.
pub fn docker_containers() -> Result<Vec<DockerContainer>, String> {
    let docker = crate::exec::find_docker().ok_or_else(crate::exec::docker_missing_message)?;
    let program = docker.display().to_string();

    let listed = crate::exec::ToolCommand::new(program.clone())
        .args(["ps", "--format", "{{.ID}}\t{{.Names}}\t{{.Image}}"])
        .output_text()
        .map_err(|e| format!("could not ask Docker what is running: {e}"))?;

    let mut rows = parse_ps(&listed);

    // Only when `docker ps` gave up on naming one. In a healthy install this
    // is never true, and the second round trip never happens.
    if rows.iter().any(|r| !is_image_reference(&r.image)) {
        let created_from = inspect_created_from(&program, &rows);
        for row in &mut rows {
            row.image = pick_image(row, &created_from);
        }
    }

    Ok(rows
        .into_iter()
        .map(|r| DockerContainer {
            name: r.name,
            image: r.image,
        })
        .collect())
}

/// The best label available for one container's image.
///
/// `docker ps`'s answer wins whenever it is usable — it describes the image
/// that is *running*. `Config.Image` is the rescue, and only the rescue: it
/// records what was asked for at creation, which is right for a moved tag and
/// wrong for a container started from a bare ID.
///
/// `created_from` carries full IDs against the abbreviated ones `docker ps`
/// prints, so they are matched by prefix. No match leaves the ID showing,
/// which is merely unhelpful — borrowing another container's image would be
/// actively misleading.
fn pick_image(row: &Listed, created_from: &[(String, String)]) -> String {
    if is_image_reference(&row.image) {
        return row.image.clone();
    }

    created_from
        .iter()
        .find(|(id, _)| !row.id.is_empty() && id.starts_with(&row.id))
        .map(|(_, image)| image.trim())
        .filter(|image| is_image_reference(image))
        .unwrap_or(&row.image)
        .to_string()
}

/// One row of `docker ps`, before the image has been made presentable.
struct Listed {
    id: String,
    name: String,
    image: String,
}

fn parse_ps(text: &str) -> Vec<Listed> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?.trim();
            let name = fields.next()?.trim();
            let image = fields.next()?.trim();
            (!name.is_empty()).then(|| Listed {
                id: id.to_string(),
                name: name.to_string(),
                image: image.to_string(),
            })
        })
        .collect()
}

/// Ask Docker what reference each container was created from.
///
/// Returns pairs rather than a map because the IDs come back in full and go in
/// abbreviated, so they are matched by prefix.
///
/// Failure is not propagated: this is a cosmetic improvement to a label, and a
/// container that stopped between the two calls makes `docker inspect` exit
/// non-zero. Losing the whole container list over that would trade a slightly
/// ugly name for an unusable picker.
fn inspect_created_from(program: &str, rows: &[Listed]) -> Vec<(String, String)> {
    if rows.is_empty() {
        return Vec::new();
    }

    let mut args = vec![
        "inspect".to_string(),
        "--format".to_string(),
        "{{.Id}}\t{{.Config.Image}}".to_string(),
    ];
    args.extend(rows.iter().map(|r| r.id.clone()));

    let Ok(out) = crate::exec::ToolCommand::new(program.to_string())
        .args(args)
        .output_text()
    else {
        return Vec::new();
    };

    out.lines()
        .filter_map(|line| {
            let (id, image) = line.split_once('\t')?;
            Some((id.trim().to_string(), image.trim().to_string()))
        })
        .collect()
}

/// Whether a string names an image in a way a human can act on.
///
/// `docker ps` prints a bare image ID whenever the running image no longer
/// carries a tag — which happens to anyone who has re-pulled one. `docker pull
/// mysql:8` moves the tag onto the newly fetched image and leaves the running
/// container's image dangling, so the picker offers `d2c60b1b225c` where it
/// means `mysql:8`. The container is fine and its clients still work; only the
/// label is useless, and it is useless exactly when there are several
/// containers to tell apart.
///
/// A reference always carries a tag, a digest or a registry path. An ID is hex
/// and nothing else, so that is the whole test.
fn is_image_reference(image: &str) -> bool {
    let image = image.trim();
    !image.is_empty()
        && !image.starts_with("sha256:")
        && !image.chars().all(|c| c.is_ascii_hexdigit())
}

/// Install a formula with Homebrew, returning its output.
///
/// Runs to completion rather than streaming: a client install is tens of
/// seconds, not the hours a dump can take, and the output is only worth
/// reading when it fails.
pub fn brew_install(formula: &str) -> Result<String, String> {
    // Refusing anything that is not one of our own formula strings. This value
    // reaches a process spawn, and while `ToolCommand` passes arguments as a
    // vector rather than through a shell — so there is no injection to worry
    // about — an unexpected formula name would still install something nobody
    // asked for.
    if !Tool::ALL.iter().any(|t| t.brew_formula() == Some(formula)) {
        return Err(format!("{formula} is not a formula this app installs"));
    }

    let brew = crate::exec::find_tool("brew", None)
        .ok_or_else(|| "Homebrew is not installed on this machine".to_string())?;

    crate::exec::ToolCommand::new(brew.display().to_string())
        .args(["install", formula])
        .output_text()
        .map_err(|e| format!("{formula} failed to install: {e}"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DiscoveredTool {
    pub tool: Tool,
    pub path: PathBuf,
    pub version: Option<Version>,
}

/// A `major.minor.patch` version, with unknown components treated as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Type)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Extract the first `N[.N[.N]]` found in a tool's `--version` output.
    ///
    /// Formats vary: `pg_dump (PostgreSQL) 16.2`, `mysqldump  Ver 8.0.42 for
    /// osx10.19`, `mysqldump Ver 10.19 Distrib 10.11.6-MariaDB`.
    pub fn parse_first(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_digit() {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                let token = &text[start..i];
                let mut parts = token.split('.').filter(|p| !p.is_empty());
                let major = parts.next()?.parse().ok()?;
                let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
                let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
                return Some(Self::new(major, minor, patch));
            }
            i += 1;
        }
        None
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum CompatibilityVerdict {
    Ok,
    /// Allowed, but the user should know.
    Warn(String),
    /// Refused unless the user explicitly overrides.
    Blocked(String),
}

impl CompatibilityVerdict {
    pub const fn is_ok(&self) -> bool {
        matches!(self, CompatibilityVerdict::Ok)
    }
}

/// Check `pg_dump` against the server it will read.
///
/// Two distinct hazards, and they point in opposite directions:
///
/// * **Client older than the server** — `pg_dump` refuses outright, part-way
///   through, with a confusing message. Blocked.
/// * **Client newer than the server** — the dump *succeeds*, but embeds
///   directives the older server does not understand, so it fails on restore.
///   pg_dump 18 emits `SET transaction_timeout = 0`, a parameter introduced in
///   PostgreSQL 17; restoring that into a 16 server aborts with "unrecognized
///   configuration parameter". Warned, with the fix named, because dumping with
///   a newer client is fine when the destination is equally new.
pub fn check_pg_dump_compatibility(client: Version, server: Version) -> CompatibilityVerdict {
    if client.major < server.major {
        return CompatibilityVerdict::Blocked(format!(
            "pg_dump {client} is older than the server ({server}); \
             pg_dump cannot dump from a newer server. Install PostgreSQL {} client tools.",
            server.major
        ));
    }
    if client.major > server.major {
        return CompatibilityVerdict::Warn(format!(
            "pg_dump {client} is newer than the server ({server}). The dump will \
             succeed, but it may contain directives this server version does not \
             understand, so restoring it back into a PostgreSQL {} server can fail. \
             Use PostgreSQL {} client tools to match the server.",
            server.major, server.major
        ));
    }
    CompatibilityVerdict::Ok
}

/// An 8.0+ `mysqldump` queries `information_schema.COLUMN_STATISTICS`, which
/// does not exist before 8.0 — the dump fails unless `--column-statistics=0`
/// is passed.
pub fn mysql_needs_column_statistics_flag(client: Version, server: Version) -> bool {
    client.major >= 8 && server.major < 8
}

/// Check `mongodump` against the server it will read.
///
/// The interesting thing here is what this deliberately does **not** do.
///
/// `mongodump` ships in the MongoDB Database Tools, which are versioned in
/// their own `100.x` series, unrelated to the server's. A client reporting
/// `100.9.4` against a `7.0.5` server is the *normal* case, not a client 93
/// majors ahead — so the comparison [`check_pg_dump_compatibility`] makes is
/// meaningless here, and making it would block every correctly-installed
/// MongoDB setup on this planet.
///
/// What is worth saying is the reverse: a `mongodump` whose major version is
/// *not* in the 100 series is one of the old server-bundled builds that were
/// retired at MongoDB 4.4, and those really do fail against a modern server.
/// That gets a warning rather than a block, because a user pointing an
/// override at a deliberately old binary for a deliberately old server is
/// making a choice this app has no business overruling.
pub fn check_mongodump_compatibility(client: Version, server: Version) -> CompatibilityVerdict {
    if client.major < 100 {
        return CompatibilityVerdict::Warn(format!(
            "mongodump {client} predates the MongoDB Database Tools, which were split out \
             at server 4.4 and are versioned separately from the server ({server}). \
             Install the Database Tools (a 100.x mongodump) unless this old binary is \
             deliberate."
        ));
    }
    CompatibilityVerdict::Ok
}

#[cfg(test)]
mod source_tests {
    use super::*;

    fn exec_source() -> ToolSource {
        ToolSource::DockerExec {
            container: "mysql8".into(),
            bin_dir: None,
        }
    }

    fn run_source() -> ToolSource {
        ToolSource::DockerRun {
            image: "mysql:8".into(),
        }
    }

    fn resolved(tool: Tool, source: &ToolSource) -> ResolvedTool {
        ResolvedTool::resolve(tool, source, None).expect("docker sources always resolve")
    }

    /// The command line with the Docker client reduced to its bare name.
    ///
    /// The real command carries an absolute path — that is the point of
    /// [`crate::exec::find_docker`] — and it differs per machine and is absent
    /// on a runner with no Docker at all. Asserting on the raw string would
    /// make these tests about where Docker happens to be installed rather than
    /// about the arguments, which is what they are actually checking.
    fn cmdline(tool: &ResolvedTool) -> String {
        let rendered = tool.command().display();
        let (program, rest) = rendered.split_once(' ').unwrap_or((&rendered, ""));
        // `file_stem`, not `file_name`: Windows spells it `docker.exe`.
        let name = Path::new(program)
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.to_string());
        format!("{name} {rest}").trim_end().to_string()
    }

    #[test]
    fn a_container_source_runs_docker_by_absolute_path() {
        // The regression this exists for: `Command::new("docker")` resolves
        // through the inherited PATH, which for an app started from Finder is
        // /usr/bin:/bin:/usr/sbin:/sbin — four directories no Docker
        // distribution installs into. Every containerised dump died on
        // "No such file or directory" on a machine with containers running.
        let Some(expected) = crate::exec::find_docker() else {
            return; // No Docker here; `preflight` is what covers that case.
        };

        for source in [exec_source(), run_source()] {
            let rendered = resolved(Tool::Mysqldump, &source).command().display();
            assert!(
                rendered.starts_with(&expected.display().to_string()),
                "PATH would have to be right for this to run: {rendered}"
            );
        }
    }

    #[test]
    fn a_local_source_never_needs_docker() {
        assert!(ToolSource::Local.preflight().is_ok());
    }

    #[test]
    fn a_container_source_preflight_matches_what_can_be_found() {
        // Both directions, so this stays honest on a machine with Docker and
        // on one without: preflight passes exactly when the client is there.
        let have_docker = crate::exec::find_docker().is_some();
        for source in [exec_source(), run_source()] {
            let verdict = source.preflight();
            assert_eq!(verdict.is_ok(), have_docker, "{source:?} → {verdict:?}");
            if let Err(msg) = verdict {
                assert!(msg.contains("docker"), "must name what is missing: {msg}");
            }
        }
    }

    #[test]
    fn local_is_the_default_so_existing_installs_do_not_change() {
        assert_eq!(ToolSource::default(), ToolSource::Local);
        assert!(!ToolSource::Local.is_docker());
    }

    #[test]
    fn a_tunnelled_host_is_rewritten_for_containers_only() {
        // The bug this exists to prevent: inside a container, loopback is the
        // container, so a tunnel's local end is unreachable and the dump fails
        // against a port that is demonstrably open on the host.
        for host in ["127.0.0.1", "localhost", "::1", "127.1.2.3", "[::1]"] {
            assert_eq!(
                exec_source().rewrite_host(host),
                DOCKER_HOST_ALIAS,
                "{host} is the local machine and must be redirected"
            );
            assert_eq!(run_source().rewrite_host(host), DOCKER_HOST_ALIAS);
            assert_eq!(
                ToolSource::Local.rewrite_host(host),
                host,
                "a local tool talks to loopback directly"
            );
        }
    }

    #[test]
    fn a_real_hostname_is_never_rewritten() {
        let host = "db-mysql-sgp1.ondigitalocean.com";
        for source in [ToolSource::Local, exec_source(), run_source()] {
            assert_eq!(source.rewrite_host(host), host);
        }
    }

    #[test]
    fn docker_run_adds_the_host_gateway_and_forwards_the_password_by_name() {
        let line = cmdline(&resolved(Tool::Mysqldump, &run_source()));

        assert!(line.starts_with("docker run --rm -i"), "got: {line}");
        assert!(
            line.contains(&format!("--add-host={DOCKER_HOST_ALIAS}:host-gateway")),
            "Linux has no host.docker.internal without this: {line}"
        );
        // The name, never the value — argv is world-readable via `ps`.
        assert!(line.contains("-e MYSQL_PWD"), "got: {line}");
        assert!(!line.contains("MYSQL_PWD="), "a value leaked into argv: {line}");
        // Image then command, so the caller's `.args(..)` land as the tool's
        // own arguments rather than as Docker options.
        assert!(line.ends_with("mysql:8 mysqldump"), "got: {line}");
    }

    #[test]
    fn docker_exec_targets_the_named_container() {
        let line = cmdline(&resolved(Tool::PgDump, &exec_source()));
        assert!(line.starts_with("docker exec -i"), "got: {line}");
        assert!(line.contains("-e PGPASSWORD"), "got: {line}");
        assert!(line.ends_with("mysql8 pg_dump"), "got: {line}");
    }

    #[test]
    fn mongo_tools_forward_no_password_variable() {
        // They take credentials in the URI or in arguments, so forwarding a
        // variable would be cargo-culted noise on every command line.
        let line = cmdline(&resolved(Tool::Mongodump, &run_source()));
        assert!(!line.contains(" -e "), "got: {line}");
        assert!(line.ends_with("mysql:8 mongodump"), "got: {line}");
    }

    #[test]
    fn a_bin_dir_is_joined_without_doubling_the_separator() {
        let source = ToolSource::DockerExec {
            container: "c".into(),
            bin_dir: Some("/usr/local/mysql/bin/".into()),
        };
        let line = cmdline(&resolved(Tool::Mysqldump, &source));
        assert!(line.ends_with("c /usr/local/mysql/bin/mysqldump"), "got: {line}");
    }

    #[test]
    fn an_empty_bin_dir_falls_back_to_the_container_path() {
        let source = ToolSource::DockerExec {
            container: "c".into(),
            bin_dir: Some("  ".into()),
        };
        let line = cmdline(&resolved(Tool::Mysqldump, &source));
        assert!(line.ends_with("c mysqldump"), "got: {line}");
    }

    #[test]
    fn a_profile_override_forces_the_local_binary() {
        // Naming a specific binary is the only reason to set an override, so
        // quietly running a different one out of a container would defeat it.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("mysqldump");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();

        let resolved =
            ResolvedTool::resolve(Tool::Mysqldump, &run_source(), fake.to_str()).unwrap();

        assert!(!resolved.is_containerised());
        assert_eq!(resolved.local_path(), Some(fake.as_path()));
    }

    #[test]
    fn an_override_pointing_at_nothing_does_not_resolve() {
        assert!(
            ResolvedTool::resolve(Tool::Mysqldump, &ToolSource::Local, Some("/nope/mysqldump"))
                .is_none()
        );
    }

    #[test]
    fn every_tool_names_a_formula_and_a_sane_password_variable() {
        for tool in Tool::ALL {
            assert!(
                tool.brew_formula().is_some(),
                "{tool:?} has no install route to offer"
            );
            match tool.engine() {
                crate::types::Engine::Mongo => assert_eq!(tool.password_env(), None),
                _ => assert!(tool.password_env().is_some(), "{tool:?}"),
            }
        }
    }

    // ── The container picker's labels ───────────────────────────────────

    #[test]
    fn a_tag_is_a_usable_label_and_a_bare_id_is_not() {
        for reference in [
            "mysql:8",
            "postgres:18",
            "mongodb/mongodb-community-server:7.0-ubuntu2204",
            "public.ecr.aws/supabase/postgres:17.6.1.140",
            // Pinned by digest: long and ugly, but it does name an image.
            "mysql@sha256:d2c60b1b225c6d7845f0abdb596fc35c2d4122bcad6ec2195",
        ] {
            assert!(is_image_reference(reference), "{reference} is a reference");
        }

        for id in [
            "d2c60b1b225c",
            "8485e4e11b29",
            "sha256:d2c60b1b225c6d7845f0abdb596fc35c2d4122bcad6ec219588035a118f75d93",
            "",
            "   ",
        ] {
            assert!(!is_image_reference(id), "{id} tells the user nothing");
        }
    }

    #[test]
    fn ps_rows_are_parsed_and_headerless_junk_is_dropped() {
        let out = "8485e4e11b29\tmysql8\td2c60b1b225c\n\
                   f26a885ce420\tpostgres-db\tpostgres:16\n\
                   \n\
                   malformed-line-with-no-tabs\n";
        let rows = parse_ps(out);

        assert_eq!(rows.len(), 2, "a malformed line must not become a container");
        assert_eq!(rows[0].id, "8485e4e11b29");
        assert_eq!(rows[0].name, "mysql8");
        assert_eq!(rows[1].image, "postgres:16");
    }

    #[test]
    fn a_container_name_is_never_invented_from_a_blank_field() {
        assert!(parse_ps("abc123\t\tmysql:8\n").is_empty());
    }

    #[test]
    fn a_dangling_image_falls_back_to_what_the_container_was_created_from() {
        // The real case: `docker pull mysql:8` moved the tag to a new image,
        // so the running container's image lost its only tag and `docker ps`
        // reports the ID. `Config.Image` still remembers `mysql:8`.
        let ps = parse_ps("8485e4e11b29\tmysql8\td2c60b1b225c\n");
        let inspected = vec![(
            "8485e4e11b29a0f3c6d1e2b7c8a94f05d3e6b1a2c7f80d9e4b3a6c5f2e1d0b9a8".to_string(),
            "mysql:8".to_string(),
        )];

        let chosen = pick_image(&ps[0], &inspected);
        assert_eq!(chosen, "mysql:8", "the ID was the only thing wrong with it");
    }

    #[test]
    fn a_tag_that_docker_ps_already_resolved_is_left_alone() {
        // The opposite mistake: a container started by ID has `Config.Image`
        // set to that ID, while `docker ps` resolved a perfectly good tag.
        // Preferring `Config.Image` unconditionally would make this worse.
        let ps = parse_ps("f26a885ce420\tpostgres-db\tpostgres:16\n");
        let inspected = vec![(
            "f26a885ce4201234567890abcdef1234567890abcdef1234567890abcdef1234".to_string(),
            "3a82e1f56c8f".to_string(),
        )];

        assert_eq!(pick_image(&ps[0], &inspected), "postgres:16");
    }

    #[test]
    fn an_unmatched_or_useless_inspection_leaves_the_id_showing() {
        // Degrading to today's behaviour is correct here: an ID is a poor
        // label, but inventing one for the wrong container would be worse.
        let ps = parse_ps("8485e4e11b29\tmysql8\td2c60b1b225c\n");

        assert_eq!(pick_image(&ps[0], &[]), "d2c60b1b225c");
        assert_eq!(
            pick_image(
                &ps[0],
                &[("0000000000000000".to_string(), "mysql:8".to_string())]
            ),
            "d2c60b1b225c",
            "a different container's image must not be borrowed"
        );
    }

    #[test]
    fn brew_install_refuses_a_formula_this_app_does_not_own() {
        // The value reaches a process spawn. Arguments go through a vector
        // rather than a shell, so there is nothing to inject — but installing
        // software nobody asked for is bad enough on its own.
        for bogus in ["curl", "mysql-client; rm -rf /", "", "../evil"] {
            let err = brew_install(bogus).expect_err("must refuse");
            assert!(err.contains("not a formula"), "{bogus} → {err}");
        }
    }

    #[test]
    fn discovery_covers_every_tool_and_reports_what_can_be_done_about_it() {
        // Local discovery only — a container source would start containers.
        let found = discover(&ToolSource::Local);
        assert_eq!(found.len(), Tool::ALL.len());

        for status in &found {
            assert_eq!(status.found(), status.location.is_some());
            assert!(
                status.brew_formula.is_some(),
                "{} is missing with nothing to suggest",
                status.binary
            );
        }
    }

    #[test]
    fn the_source_round_trips_through_json() {
        for source in [
            ToolSource::Local,
            exec_source(),
            run_source(),
            ToolSource::DockerExec {
                container: "c".into(),
                bin_dir: Some("/bin".into()),
            },
        ] {
            let json = serde_json::to_string(&source).unwrap();
            let back: ToolSource = serde_json::from_str(&json).unwrap();
            assert_eq!(source, back, "{json}");
        }
    }
    // ── Mounts ──────────────────────────────────────────────────────────
    //
    // The failure these guard is the quietest one in the whole Docker path: a
    // tool that runs, exits 0, and writes its output somewhere the host cannot
    // see. Nothing errors. The artifact is simply not there.

    #[test]
    fn a_local_tool_uses_host_paths_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("creds.conf");
        std::fs::write(&file, b"x").unwrap();

        let mut tool = ResolvedTool::resolve(Tool::Mongodump, &ToolSource::Local, file.to_str())
            .expect("an override always resolves to a local tool");

        assert_eq!(
            tool.mount(&file, MountMode::ReadOnly).unwrap(),
            file,
            "a local tool already shares this filesystem"
        );
        assert!(
            !tool.command().display().contains("-v"),
            "nothing to bind when there is no container"
        );
    }

    #[test]
    fn a_mounted_path_is_rewritten_and_bound_in() {
        let mut tool = resolved(Tool::Mongodump, &run_source());
        let inside = tool
            .mount(Path::new("/tmp/dbsync-mongo-abc.conf"), MountMode::ReadOnly)
            .unwrap();

        assert!(
            inside.starts_with(MOUNT_ROOT),
            "the tool must be given the container's path, got {inside:?}"
        );

        let rendered = tool.command().display();
        assert!(
            rendered.contains("-v /tmp/dbsync-mongo-abc.conf:"),
            "the host file must be bound in: {rendered}"
        );
        assert!(rendered.contains(":ro"), "an input must be read-only: {rendered}");
    }

    #[test]
    fn a_writable_mount_is_not_marked_read_only() {
        // pg_dump -Fc writes its archive itself; a read-only bind would fail.
        let mut tool = resolved(Tool::PgDump, &run_source());
        tool.mount(Path::new("/backups"), MountMode::ReadWrite).unwrap();

        let rendered = tool.command().display();
        assert!(rendered.contains("-v /backups:"), "{rendered}");
        assert!(
            !rendered.contains(":ro"),
            "pg_dump could not write through this: {rendered}"
        );
    }

    #[test]
    fn mounts_are_emitted_before_the_image() {
        // Anything after the image name is the container's command, not
        // docker's, so a -v placed there would be passed to mysqldump.
        let mut tool = resolved(Tool::Mysqldump, &run_source());
        tool.mount(Path::new("/tmp/x"), MountMode::ReadOnly).unwrap();

        let rendered = tool.command().display();
        let mount_at = rendered.find("-v ").expect("mount present");
        let image_at = rendered.find("mysql:8").expect("image present");
        assert!(
            mount_at < image_at,
            "the bind mount landed after the image: {rendered}"
        );
    }

    #[test]
    fn two_mounts_with_the_same_name_do_not_collide() {
        let mut tool = resolved(Tool::PgRestore, &run_source());
        let a = tool.mount(Path::new("/one/app.dump"), MountMode::ReadOnly).unwrap();
        let b = tool.mount(Path::new("/two/app.dump"), MountMode::ReadOnly).unwrap();
        assert_ne!(a, b, "the second mount shadowed the first");
    }

    #[test]
    fn borrowing_a_running_container_cannot_accept_a_host_file() {
        // `docker exec` joins a container that already started, so there is no
        // moment at which a bind mount could be added. Refused rather than
        // silently passing a path that does not exist inside it.
        let mut tool = resolved(Tool::Mongodump, &exec_source());
        let err = tool
            .mount(Path::new("/tmp/creds.conf"), MountMode::ReadOnly)
            .expect_err("this cannot work and must not pretend to");
        assert!(err.to_string().contains("creds.conf"), "got: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_postgres_version_banner() {
        let v = Version::parse_first("pg_dump (PostgreSQL) 16.2").unwrap();
        assert_eq!(v, Version::new(16, 2, 0));
    }

    #[test]
    fn parses_mysql_version_banner() {
        let v = Version::parse_first("mysqldump  Ver 8.0.42 for macos14 on arm64").unwrap();
        assert_eq!(v, Version::new(8, 0, 42));
    }

    #[test]
    fn parses_bare_major_version() {
        assert_eq!(
            Version::parse_first("psql (PostgreSQL) 17").unwrap(),
            Version::new(17, 0, 0)
        );
    }

    #[test]
    fn returns_none_when_no_digits_present() {
        assert!(Version::parse_first("command not found").is_none());
    }

    #[test]
    fn versions_order_numerically_not_lexically() {
        assert!(Version::new(9, 0, 0) < Version::new(10, 0, 0));
        assert!(Version::new(8, 0, 42) > Version::new(8, 0, 9));
    }

    #[test]
    fn older_pg_dump_against_newer_server_is_blocked() {
        let verdict = check_pg_dump_compatibility(Version::new(15, 6, 0), Version::new(16, 2, 0));
        assert!(matches!(verdict, CompatibilityVerdict::Blocked(_)));
    }

    #[test]
    fn matching_major_versions_are_ok() {
        assert!(
            check_pg_dump_compatibility(Version::new(16, 0, 0), Version::new(16, 2, 0)).is_ok()
        );
    }

    #[test]
    fn even_one_major_newer_is_flagged() {
        // Not "close enough": PostgreSQL 17 added transaction_timeout, which a
        // 16 server rejects when restoring a dump taken with a 17 client.
        let verdict = check_pg_dump_compatibility(Version::new(17, 0, 0), Version::new(16, 2, 0));
        assert!(matches!(verdict, CompatibilityVerdict::Warn(_)));
    }

    #[test]
    fn column_statistics_flag_needed_only_for_new_client_old_server() {
        assert!(mysql_needs_column_statistics_flag(
            Version::new(8, 0, 42),
            Version::new(5, 7, 40)
        ));
        assert!(!mysql_needs_column_statistics_flag(
            Version::new(8, 0, 42),
            Version::new(8, 0, 30)
        ));
        assert!(!mysql_needs_column_statistics_flag(
            Version::new(5, 7, 40),
            Version::new(5, 7, 40)
        ));
    }

    #[test]
    fn every_engine_has_the_required_tools_to_dump_and_restore() {
        let required: Vec<Tool> = Tool::ALL.into_iter().filter(|t| t.is_required()).collect();

        for engine in Engine::ALL {
            assert!(
                required.iter().any(|t| t.engine() == engine),
                "{engine} has no required tool, so nothing would be discovered for it"
            );
        }
        assert!(!Tool::Mydumper.is_required(), "parallel mode is optional");
    }

    #[test]
    fn tool_binary_names_are_unique() {
        let mut names: Vec<&str> = Tool::ALL.iter().map(|t| t.binary_name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "two tools resolve to one binary");
    }

    #[test]
    fn parses_mongodump_version_banner() {
        let v = Version::parse_first("mongodump version: 100.9.4").unwrap();
        assert_eq!(v, Version::new(100, 9, 4));
    }

    #[test]
    fn mongodump_is_not_version_matched_against_the_server() {
        // The Database Tools are versioned 100.x independently of the server.
        // Applying pg_dump's rule here would block every correct install: a
        // 100.9.4 client against a 7.0.5 server is normal, not 93 majors ahead.
        let verdict =
            check_mongodump_compatibility(Version::new(100, 9, 4), Version::new(7, 0, 5));
        assert!(verdict.is_ok(), "got: {verdict:?}");

        // And the same pairing under pg_dump's rule would be refused — which is
        // exactly why the two checks cannot share an implementation.
        assert!(matches!(
            check_pg_dump_compatibility(Version::new(100, 9, 4), Version::new(7, 0, 5)),
            CompatibilityVerdict::Warn(_)
        ));
    }

    #[test]
    fn a_pre_database_tools_mongodump_is_flagged() {
        let verdict = check_mongodump_compatibility(Version::new(4, 2, 3), Version::new(7, 0, 5));
        assert!(matches!(verdict, CompatibilityVerdict::Warn(_)));
    }
}
