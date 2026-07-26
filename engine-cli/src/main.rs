//! `dbsync` — headless entry point to the engine.
//!
//! Exists so that scheduled/CI runs have exactly the same capabilities as the
//! GUI. Progress is written to stdout as JSON-lines so it can be piped into a
//! log collector; human-readable output goes to stderr.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use db_sync_engine::ENGINE_VERSION;
use db_sync_engine::events::{EVENT_CHANNEL_CAPACITY, ProgressEvent, create_event_channel};
use db_sync_engine::job::{JobContext, JobOutcome, JobRegistry};
use db_sync_engine::schedule::Schedule;
use db_sync_engine::scheduler::Scheduler;
use db_sync_engine::store::Store;
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "dbsync",
    version = ENGINE_VERSION,
    about = "Database backup, restore and cross-server sync"
)]
struct Cli {
    /// Path to the application database. Defaults to the shared location the
    /// desktop app uses, so both see the same profiles.
    #[arg(long, global = true)]
    store: Option<PathBuf>,

    /// Emit progress as JSON-lines on stdout.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List connection profiles.
    Profiles,
    /// List recent job history.
    Jobs {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Report the resolved store path and engine version.
    Doctor,
    /// Strip `DEFINER=` clauses from a MySQL dump on stdin, writing to stdout.
    ///
    /// Useful on its own for repairing an existing dump that fails to restore
    /// with "you need SUPER privilege". Unlike a `sed` one-liner, this is
    /// quote-aware and leaves the text alone inside string literals.
    StripDefiners,
    /// Inspect and run scheduled jobs.
    #[command(subcommand)]
    Schedule(ScheduleCommand),
    /// Prove the newest backup in a directory actually restores.
    ///
    /// Restores it into a scratch database, checks it against its manifest,
    /// then drops it. Exits non-zero if the restore or the check failed, so
    /// cron and CI report a backup that has quietly stopped being restorable.
    Drill {
        /// Connection to restore into. Id, or a unique prefix of its name.
        profile: String,
        /// Directory holding the backups. Defaults to the app's backup folder.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Also read every row, not just count them.
        #[arg(long)]
        deep: bool,
        /// Leave the scratch database behind when the drill fails.
        #[arg(long)]
        keep_on_failure: bool,
    },
    /// Manage the masking rules on a sync plan.
    ///
    /// Masking rewrites columns on the destination after a sync restores them.
    /// It does not touch the backup artifact, which still holds the real data.
    #[command(subcommand)]
    Mask(MaskCommand),
    /// Manage the backup encryption key.
    #[command(subcommand)]
    Key(KeyCommand),
    /// Manage off-site destinations.
    ///
    /// A backup that only exists on the machine that made it is one failure
    /// away from not existing. A destination is the second copy: every backup
    /// is uploaded to each enabled destination as soon as it is written.
    #[command(subcommand)]
    Destination(DestinationCommand),
    /// Run the scheduler in the foreground until interrupted.
    ///
    /// The desktop app runs this same loop internally; use this to run
    /// schedules on a server, under systemd or in a container, with no GUI.
    Daemon {
        /// Seconds between due checks. Cron's resolution is one minute, so
        /// there is nothing to gain below about 30.
        #[arg(long, default_value_t = 30)]
        interval: u64,
    },
}

#[derive(Subcommand)]
enum DestinationCommand {
    /// List destinations and where they point.
    List,
    /// Add an S3-compatible destination.
    ///
    /// The secret access key is read from stdin, never from an argument: a
    /// credential on the command line lands in shell history and is visible in
    /// `ps` to every user on the machine.
    Add {
        /// Name used in logs and in every other command here.
        #[arg(long)]
        name: String,
        /// Base URL with scheme, e.g. https://s3.eu-west-1.amazonaws.com
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        bucket: String,
        /// Use us-east-1 if the provider does not have regions.
        #[arg(long, default_value = "us-east-1")]
        region: String,
        /// Key prefix, so one bucket can hold several sources.
        #[arg(long, default_value = "")]
        prefix: String,
        /// Address the bucket by path. Required by MinIO and most
        /// self-hosted gateways.
        #[arg(long)]
        path_style: bool,
        #[arg(long)]
        access_key_id: String,
        /// Keep at most this many artifacts at the destination.
        #[arg(long)]
        keep_last: Option<u32>,
        /// Remove artifacts at the destination older than this many days.
        #[arg(long)]
        max_age_days: Option<u32>,
    },
    /// Replace a destination's secret access key, read from stdin.
    SetKey {
        /// Id, or a unique prefix of the name.
        destination: String,
    },
    /// Check that a destination is reachable and the credential is accepted.
    ///
    /// Proves the endpoint resolves, the credential is valid and the bucket
    /// can be listed. It does not prove the credential can write — only a
    /// write proves that, so `push` is the stronger check.
    Test {
        /// Id, or a unique prefix of the name. Omit to test every destination.
        destination: Option<String>,
    },
    /// Start or stop using a destination, keeping its configuration.
    Enable {
        destination: String,
    },
    Disable {
        destination: String,
    },
    /// Set what a destination keeps. Passing neither limit clears the policy.
    Retention {
        destination: String,
        #[arg(long)]
        keep_last: Option<u32>,
        #[arg(long)]
        max_age_days: Option<u32>,
    },
    /// Upload an existing artifact to every enabled destination.
    ///
    /// For backfilling artifacts taken before a destination was configured,
    /// and for retrying one whose upload failed. Exits non-zero if any
    /// destination could not be reached.
    Push {
        /// Path to the artifact. Its manifest is sent alongside it.
        artifact: PathBuf,
    },
    /// Delete a destination and its stored credential.
    ///
    /// Objects already at the destination are left where they are.
    Remove {
        destination: String,
    },
}

#[derive(Subcommand)]
enum KeyCommand {
    /// Show whether a key exists, its public half, and who else can decrypt.
    Status,
    /// Create the installation's key if it does not already have one.
    ///
    /// Never replaces an existing key: doing so would orphan every artifact
    /// already encrypted to it.
    Generate,
    /// Print the secret key so it can be stored somewhere safe.
    ///
    /// Encrypted backups are blocked until this has been done once. An
    /// artifact encrypted to a key nobody has a copy of is worse than no
    /// artifact: it passes every integrity check and is unreadable forever.
    Export,
    /// Adopt a key exported from elsewhere, replacing the current one.
    ///
    /// Reads the secret from stdin so it never appears in shell history or in
    /// the process list.
    Import,
    /// Replace the additional recipients that can decrypt future backups.
    ///
    /// Pass `age1...` public keys. Passing none clears the list; the
    /// installation's own key is always included regardless.
    Recipients { keys: Vec<String> },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum TransformArg {
    /// Salted SHA-256, hex. NULL stays NULL.
    Hash,
    /// A deterministic address at example.invalid. NULL stays NULL.
    Email,
    /// A deterministic number in the reserved 555 range. NULL stays NULL.
    Phone,
    /// Every row set to NULL. Fails on a NOT NULL column.
    Null,
    /// Every row set to `--value`, NULLs included.
    Constant,
}

#[derive(Subcommand)]
enum MaskCommand {
    /// Show the masking rules on a plan.
    List {
        /// Plan id, or a unique prefix of its name.
        plan: String,
    },
    /// Add or replace a rule for one column.
    Add {
        /// Plan id, or a unique prefix of its name.
        plan: String,
        /// Table name as the plan spells it. For PostgreSQL this may be
        /// `schema.table`; a bare name means `public`.
        table: String,
        column: String,
        #[arg(long, value_enum)]
        transform: TransformArg,
        /// Replacement value, required by `--transform constant`.
        #[arg(long)]
        value: Option<String>,
        /// Truncate a hash to this many hex characters.
        #[arg(long)]
        length: Option<u16>,
    },
    /// Drop the rule for one column.
    Remove {
        /// Plan id, or a unique prefix of its name.
        plan: String,
        table: String,
        column: String,
    },
    /// Print the SQL a masking run would send to the destination.
    ///
    /// For review before a first run, and for answering "what exactly did this
    /// do to my data" afterwards. The salt and any constants appear as bound
    /// placeholders, never as literals, so the output is safe to paste into a
    /// ticket.
    Sql {
        /// Plan id, or a unique prefix of its name.
        plan: String,
    },
}

#[derive(Subcommand)]
enum ScheduleCommand {
    /// List configured schedules and when they next run.
    List,
    /// Show one schedule in full.
    Show {
        /// Schedule id, or a unique prefix of its name.
        schedule: String,
    },
    /// Run a schedule once, right now, and wait for it to finish.
    ///
    /// Exits non-zero if the run failed, so `cron` and CI report it.
    Run {
        /// Schedule id, or a unique prefix of its name.
        schedule: String,
    },
    /// Run any schedules that are currently due, then exit.
    ///
    /// The building block for driving schedules from an external timer instead
    /// of leaving the app or the daemon running.
    Tick,
    /// Print a crontab line that runs a schedule from system cron.
    Crontab {
        /// Schedule id, or a unique prefix of its name.
        schedule: String,
    },
}

fn emit_json(event: &ProgressEvent) {
    if let Ok(line) = serde_json::to_string(event) {
        println!("{line}");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    // Runs as a pure filter: no store, no config, safe to use on any dump.
    if matches!(cli.command, Command::StripDefiners) {
        let stdin = std::io::stdin().lock();
        let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
        let modified = db_sync_engine::definer::strip_definers_stream(stdin, &mut stdout)?;
        std::io::Write::flush(&mut stdout)?;
        eprintln!("stripped DEFINER clauses from {modified} line(s)");
        return Ok(());
    }

    // Resolved by the engine so the CLI and GUI can never diverge onto
    // separate databases.
    let store_path = match cli.store {
        Some(p) => p,
        None => db_sync_engine::paths::default_store_path()
            .context("could not determine the application data directory")?,
    };

    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }

    match cli.command {
        // Handled above, before the store is opened.
        Command::StripDefiners => unreachable!("filter commands return early"),
        Command::Doctor => {
            eprintln!("dbsync {ENGINE_VERSION}");
            eprintln!("store: {}", store_path.display());
            let store = Store::open(&store_path).await?;
            let profiles = store.list_profiles().await?;
            eprintln!("profiles: {}", profiles.len());
            store.close().await;
        }
        Command::Profiles => {
            let store = Store::open(&store_path).await?;
            let profiles = store.list_profiles().await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&profiles)?);
            } else if profiles.is_empty() {
                eprintln!("no profiles configured");
            } else {
                for p in &profiles {
                    println!(
                        "{}  {:<24} {:<9} {:<8} {}@{}:{}",
                        p.id,
                        p.name,
                        p.engine.as_str(),
                        p.environment.as_str(),
                        p.db.user,
                        p.db.host,
                        p.db.port
                    );
                }
            }
            store.close().await;
        }
        Command::Jobs { limit } => {
            let store = Store::open(&store_path).await?;
            let jobs = store.list_jobs(limit).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&jobs)?);
            } else if jobs.is_empty() {
                eprintln!("no jobs recorded");
            } else {
                for j in &jobs {
                    println!(
                        "{}  {:?}  {}  {}",
                        j.id,
                        j.kind,
                        j.started_at.to_rfc3339(),
                        j.outcome
                            .map(|o| o.as_str().to_string())
                            .unwrap_or_else(|| "running".into())
                    );
                }
            }
            store.close().await;
        }
        Command::Schedule(cmd) => {
            let store = Store::open(&store_path).await?;
            let result = run_schedule_command(cmd, &store, cli.json).await;
            store.close().await;
            result?;
        }
        Command::Drill {
            profile,
            dir,
            deep,
            keep_on_failure,
        } => {
            let store = Store::open(&store_path).await?;
            let result = run_drill(&store, &profile, dir, deep, keep_on_failure, cli.json).await;
            store.close().await;
            result?;
        }
        Command::Mask(cmd) => {
            let store = Store::open(&store_path).await?;
            let result = run_mask_command(cmd, &store, cli.json).await;
            store.close().await;
            result?;
        }
        Command::Key(cmd) => {
            let store = Store::open(&store_path).await?;
            let result = run_key_command(cmd, &store).await;
            store.close().await;
            result?;
        }
        Command::Destination(cmd) => {
            let store = Store::open(&store_path).await?;
            let result = run_destination_command(cmd, &store, cli.json).await;
            store.close().await;
            result?;
        }
        Command::Daemon { interval } => {
            let store = Store::open(&store_path).await?;
            let (event_tx, _rx) = create_event_channel(EVENT_CHANNEL_CAPACITY);

            let scheduler = Scheduler::new(store.clone(), JobRegistry::new(), event_tx.clone())
                .with_tick(Duration::from_secs(interval.max(1)));
            let shutdown = tokio_util::sync::CancellationToken::new();

            if cli.json {
                tokio::spawn(stream_channel_as_json(event_tx.subscribe()));
            }

            // Without this, Ctrl-C and `systemctl stop` kill the process
            // mid-dump and leave a half-written artifact behind.
            let signal_token = shutdown.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    eprintln!("shutting down; in-flight jobs are being left to finish");
                    signal_token.cancel();
                }
            });

            eprintln!("dbsync daemon running; {interval}s between checks. Ctrl-C to stop.");
            scheduler.run(shutdown).await;
            store.close().await;
        }
    }

    Ok(())
}

/// Restore the newest backup into a scratch database and check it.
async fn run_drill(
    store: &Store,
    needle: &str,
    dir: Option<PathBuf>,
    deep: bool,
    keep_on_failure: bool,
    json: bool,
) -> Result<()> {
    let profile = resolve_profile(store, needle).await?;

    let artifact_dir = match dir {
        Some(d) => d,
        None => db_sync_engine::paths::app_data_dir()
            .context("could not determine the backup directory")?
            .join("backups"),
    };

    let (event_tx, rx) = create_event_channel(EVENT_CHANNEL_CAPACITY);
    if json {
        tokio::spawn(stream_channel_as_json(rx));
    } else {
        tokio::spawn(stream_channel_as_text(rx));
    }

    let ctx = JobContext::with_sender(Uuid::new_v4(), event_tx);
    let outcome = db_sync_engine::ops::drill(
        &profile,
        &db_sync_engine::ops::DrillRequest {
            artifact_dir,
            restore: default_restore_options(profile.engine),
            deep_verify: deep,
            keep_on_failure,
        },
        store,
        &ctx,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        eprintln!();
        eprintln!("{}", outcome.report.to_markdown());
    }

    // The exit code is the whole point of running this from cron.
    if outcome.report.passed() {
        eprintln!(
            "drill passed: {} restored and verified ({} tables)",
            outcome.artifact, outcome.report.tables_checked
        );
        Ok(())
    } else {
        bail!(
            "drill FAILED: {} did not restore cleanly ({} problem(s)){}",
            outcome.artifact,
            outcome.report.failures,
            if outcome.dropped {
                String::new()
            } else {
                format!("; {} left in place", outcome.scratch_database)
            }
        )
    }
}

/// Find a profile by id or by a unique prefix of its name.
async fn resolve_profile(store: &Store, needle: &str) -> Result<db_sync_engine::ConnectionProfile> {
    if let Ok(id) = Uuid::parse_str(needle) {
        return Ok(store.require_profile(id).await?);
    }

    let all = store.list_profiles().await?;
    let lowered = needle.to_lowercase();
    let matches: Vec<_> = all
        .iter()
        .filter(|p| p.name.to_lowercase().starts_with(&lowered))
        .collect();

    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no connection matches {needle:?}; `dbsync profiles` lists them"),
        many => {
            let names: Vec<&str> = many.iter().map(|p| p.name.as_str()).collect();
            bail!(
                "{needle:?} matches several connections: {}",
                names.join(", ")
            )
        }
    }
}

/// Conservative restore options for a drill.
///
/// A drill restores into a database it just created, so the destructive
/// options that exist for real restores have nothing to act on and are left
/// off rather than defaulted on.
fn default_restore_options(
    engine: db_sync_engine::Engine,
) -> db_sync_engine::restore::EngineRestoreOptions {
    match engine {
        db_sync_engine::Engine::Mysql => db_sync_engine::restore::EngineRestoreOptions::Mysql(
            db_sync_engine::restore::MysqlRestoreOptions::default(),
        ),
        db_sync_engine::Engine::Postgres => {
            db_sync_engine::restore::EngineRestoreOptions::Postgres(
                db_sync_engine::restore::PostgresRestoreOptions::default(),
            )
        }
    }
}

// ── Off-site destinations ───────────────────────────────────────────────

async fn resolve_destination(
    store: &Store,
    needle: &str,
) -> Result<db_sync_engine::destination::Destination> {
    if let Ok(id) = Uuid::parse_str(needle) {
        return Ok(store.require_destination(id).await?);
    }

    let all = store.list_destinations().await?;
    let lowered = needle.to_lowercase();
    let matches: Vec<_> = all
        .iter()
        .filter(|d| d.name.to_lowercase().starts_with(&lowered))
        .collect();

    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no destination matches {needle:?}; `dbsync destination list` shows them"),
        many => {
            let names: Vec<&str> = many.iter().map(|d| d.name.as_str()).collect();
            bail!(
                "{needle:?} matches several destinations: {}",
                names.join(", ")
            )
        }
    }
}

/// Read a credential from stdin.
///
/// Never an argument. A secret on the command line is written to shell history
/// and is readable in `ps` by every other user on the machine.
fn read_credential(prompt: &str) -> Result<String> {
    eprintln!("{prompt}");
    let mut secret = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut secret)
        .context("could not read the key from stdin")?;

    let secret = secret.trim().to_string();
    if secret.is_empty() {
        bail!("no key was supplied, so nothing was changed");
    }
    Ok(secret)
}

fn describe_retention(policy: db_sync_engine::retention::RetentionPolicy) -> String {
    match (policy.keep_last, policy.max_age_days) {
        (None, None) => "keeps everything".to_string(),
        (Some(n), None) => format!("keeps the newest {n}"),
        (None, Some(d)) => format!("keeps {d} days"),
        (Some(n), Some(d)) => format!("keeps the newest {n}, and at most {d} days"),
    }
}

async fn run_destination_command(cmd: DestinationCommand, store: &Store, json: bool) -> Result<()> {
    use db_sync_engine::destination::{
        DestinationCreate, DestinationKind, DestinationUpdate, S3Destination,
    };
    use db_sync_engine::retention::RetentionPolicy;
    use db_sync_engine::secrets::{self, SecretKind};

    match cmd {
        DestinationCommand::List => {
            let all = store.list_destinations().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&all)?);
                return Ok(());
            }
            if all.is_empty() {
                eprintln!("no off-site destinations configured");
                eprintln!("backups are being kept only on this machine");
                return Ok(());
            }
            for d in &all {
                // Whether a credential exists is shown; the credential is not.
                // A destination with no key is configured and unusable, which
                // is the state most worth surfacing in a list.
                let keyed =
                    secrets::has_secret(d.id, SecretKind::ObjectStoreSecret).unwrap_or(false);
                println!(
                    "{:<20} {:<40} {:<9} {:<12} {}",
                    d.name,
                    d.kind.describe(),
                    if d.enabled { "enabled" } else { "disabled" },
                    if keyed { "has key" } else { "NO KEY" },
                    describe_retention(d.retention),
                );
            }
        }

        DestinationCommand::Add {
            name,
            endpoint,
            bucket,
            region,
            prefix,
            path_style,
            access_key_id,
            keep_last,
            max_age_days,
        } => {
            let secret = read_credential("paste the secret access key, then press Ctrl-D:")?;

            let created = store
                .create_destination(DestinationCreate {
                    name,
                    kind: DestinationKind::S3(S3Destination {
                        endpoint,
                        region,
                        bucket,
                        prefix,
                        path_style,
                        access_key_id,
                    }),
                    enabled: true,
                    retention: RetentionPolicy {
                        keep_last,
                        max_age_days,
                    },
                })
                .await?;

            secrets::set_secret(created.id, SecretKind::ObjectStoreSecret, &secret)?;

            // Checked immediately rather than at the next backup: a typo in a
            // bucket name is cheap to fix now and expensive to discover at 3am.
            match db_sync_engine::ops::test_destination(&created).await {
                Ok(()) => eprintln!("added {:?} — {}", created.name, created.kind.describe()),
                Err(e) => {
                    eprintln!("added {:?}, but it is not usable yet: {e}", created.name);
                    eprintln!(
                        "fix it and re-check with `dbsync destination test {}`",
                        created.name
                    );
                }
            }
            println!("{}", created.id);
        }

        DestinationCommand::SetKey { destination } => {
            let d = resolve_destination(store, &destination).await?;
            let secret = read_credential("paste the new secret access key, then press Ctrl-D:")?;
            secrets::set_secret(d.id, SecretKind::ObjectStoreSecret, &secret)?;

            match db_sync_engine::ops::test_destination(&d).await {
                Ok(()) => eprintln!("the new key for {:?} works", d.name),
                Err(e) => bail!("the new key was stored but is not accepted: {e}"),
            }
        }

        DestinationCommand::Test { destination } => {
            let targets = match destination {
                Some(needle) => vec![resolve_destination(store, &needle).await?],
                None => store.list_destinations().await?,
            };
            if targets.is_empty() {
                eprintln!("no off-site destinations configured");
                return Ok(());
            }

            let mut failed = 0;
            for d in &targets {
                match db_sync_engine::ops::test_destination(d).await {
                    Ok(()) => println!("{:<20} ok    {}", d.name, d.kind.describe()),
                    Err(e) => {
                        failed += 1;
                        println!("{:<20} FAIL  {e}", d.name);
                    }
                }
            }
            // Non-zero so this is usable as a health check in cron or CI.
            if failed > 0 {
                bail!(
                    "{failed} of {} destination(s) are not usable",
                    targets.len()
                );
            }
        }

        DestinationCommand::Enable { destination } => {
            let d = resolve_destination(store, &destination).await?;
            store
                .update_destination(
                    d.id,
                    DestinationUpdate {
                        enabled: Some(true),
                        ..Default::default()
                    },
                )
                .await?;
            eprintln!("{:?} will receive future backups", d.name);
        }

        DestinationCommand::Disable { destination } => {
            let d = resolve_destination(store, &destination).await?;
            store
                .update_destination(
                    d.id,
                    DestinationUpdate {
                        enabled: Some(false),
                        ..Default::default()
                    },
                )
                .await?;
            eprintln!(
                "{:?} will be skipped; its configuration and key are kept",
                d.name
            );
        }

        DestinationCommand::Retention {
            destination,
            keep_last,
            max_age_days,
        } => {
            let d = resolve_destination(store, &destination).await?;
            let policy = RetentionPolicy {
                keep_last,
                max_age_days,
            };
            store
                .update_destination(
                    d.id,
                    DestinationUpdate {
                        retention: Some(policy),
                        ..Default::default()
                    },
                )
                .await?;
            eprintln!("{:?} now {}", d.name, describe_retention(policy));
        }

        DestinationCommand::Push { artifact } => {
            if !artifact.is_file() {
                bail!("{} is not a file", artifact.display());
            }

            let ctx = JobContext::new(Uuid::new_v4());
            if json {
                tokio::spawn(stream_channel_as_json(ctx.subscribe()));
            } else {
                tokio::spawn(stream_channel_as_text(ctx.subscribe()));
            }

            let results = db_sync_engine::ops::push_offsite(&artifact, store, &ctx).await?;
            if results.is_empty() {
                eprintln!("no enabled destinations; nothing was uploaded");
                return Ok(());
            }

            for r in &results {
                match &r.error {
                    None => println!("{:<20} ok    {}", r.destination_name, r.url),
                    Some(e) => println!("{:<20} FAIL  {e}", r.destination_name),
                }
            }

            let failures = db_sync_engine::ops::push_failures(&results);
            if !failures.is_empty() {
                bail!(
                    "{} destination(s) failed: {}",
                    failures.len(),
                    failures.join("; ")
                );
            }
        }

        DestinationCommand::Remove { destination } => {
            let d = resolve_destination(store, &destination).await?;
            db_sync_engine::ops::forget_destination(store, d.id).await?;
            eprintln!("removed {:?} and its stored key", d.name);
            eprintln!("objects already at {} were left alone", d.kind.describe());
        }
    }

    Ok(())
}

// ── Backup key ──────────────────────────────────────────────────────────

async fn run_key_command(cmd: KeyCommand, store: &Store) -> Result<()> {
    use db_sync_engine::backupkey;

    match cmd {
        KeyCommand::Status => {
            let status = backupkey::status(store).await?;
            if !status.exists {
                eprintln!("no backup key on this machine");
                eprintln!("run `dbsync key generate` before taking an encrypted backup");
                return Ok(());
            }
            println!("public key   {}", status.public.unwrap_or_default());
            println!(
                "escrowed     {}",
                if status.exported {
                    "yes"
                } else {
                    "NO — encrypted backups are blocked until `dbsync key export`"
                }
            );
            if status.extra_recipients.is_empty() {
                println!("recipients   (only this machine)");
            } else {
                for r in &status.extra_recipients {
                    println!("recipient    {r}");
                }
            }
        }

        KeyCommand::Generate => {
            let before = backupkey::status(store).await?;
            let status = backupkey::ensure_exists(store).await?;
            if before.exists {
                eprintln!("a key already exists; leaving it alone");
                eprintln!("replacing it would make every existing encrypted backup unreadable");
            } else {
                eprintln!("generated a backup key");
            }
            println!("{}", status.public.unwrap_or_default());
            if !status.exported {
                eprintln!();
                eprintln!("Now run `dbsync key export` and store the result somewhere safe.");
                eprintln!("Encrypted backups are blocked until you do.");
            }
        }

        KeyCommand::Export => {
            let secret = backupkey::export(store).await?;
            // The secret goes to stdout so it can be redirected into a file or
            // a password manager; the warning goes to stderr so it is not
            // captured along with it.
            eprintln!("This is the only thing that can decrypt your backups.");
            eprintln!("Store it in a password manager. Anyone holding it can read every artifact.");
            eprintln!();
            println!("{}", secrecy::ExposeSecret::expose_secret(&secret));
        }

        KeyCommand::Import => {
            // Read from stdin rather than an argument: a key on the command
            // line lands in shell history and is visible in `ps`.
            eprintln!("paste the secret key, then press Ctrl-D:");
            let mut secret = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut secret)
                .context("could not read the key from stdin")?;

            let status = backupkey::import(store, &secret).await?;
            eprintln!("imported");
            println!("{}", status.public.unwrap_or_default());
        }

        KeyCommand::Recipients { keys } => {
            backupkey::set_extra_recipients(store, &keys).await?;
            if keys.is_empty() {
                eprintln!("cleared the additional recipients");
            } else {
                eprintln!(
                    "future backups will also be readable by {} key(s)",
                    keys.len()
                );
            }
            eprintln!("existing artifacts are unaffected — they were encrypted already");
        }
    }

    Ok(())
}

// ── Schedules ───────────────────────────────────────────────────────────

// ── Masking ─────────────────────────────────────────────────────────────

/// Find a sync plan by id, or by a unique prefix of its name.
///
/// Plans are stored per profile and there is no global list, so this walks
/// them. Ambiguity is reported rather than guessed at: picking the wrong plan
/// here means masking the wrong columns.
async fn resolve_plan(store: &Store, needle: &str) -> Result<db_sync_engine::plan::SyncPlan> {
    let mut all = Vec::new();
    for profile in store.list_profiles().await? {
        all.extend(store.list_sync_plans(profile.id).await?);
    }

    if let Ok(id) = Uuid::parse_str(needle) {
        return all
            .into_iter()
            .find(|p| p.id == id)
            .with_context(|| format!("no sync plan with id {id}"));
    }

    let lowered = needle.to_lowercase();
    let matches: Vec<_> = all
        .iter()
        .filter(|p| p.name.to_lowercase().starts_with(&lowered))
        .collect();

    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no sync plan matches {needle:?}"),
        many => {
            let names: Vec<&str> = many.iter().map(|p| p.name.as_str()).collect();
            bail!("{needle:?} matches several plans: {}", names.join(", "))
        }
    }
}

async fn run_mask_command(cmd: MaskCommand, store: &Store, json: bool) -> Result<()> {
    use db_sync_engine::mask::{MaskRule, MaskTransform};

    match cmd {
        MaskCommand::List { plan } => {
            let plan = resolve_plan(store, &plan).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan.masking)?);
                return Ok(());
            }

            if plan.masking.is_empty() {
                eprintln!("{}: no masking rules", plan.name);
                return Ok(());
            }

            let active = plan.active_masking();
            for rule in &plan.masking {
                // A rule on a table the plan does not copy with data protects
                // nothing, because nothing reaches the destination. Safe, but
                // it almost always means the plan and the rules have drifted.
                let mark = if active.contains(&rule) { " " } else { "!" };
                println!(
                    "{mark} {}.{}  {}",
                    rule.table,
                    rule.column,
                    rule.transform.describe()
                );
            }
            if active.len() != plan.masking.len() {
                eprintln!(
                    "\n! = the plan does not copy that table with data, so the rule will not run"
                );
            }
            eprintln!("\nThe backup artifact is NOT masked; only the destination is.");
        }

        MaskCommand::Add {
            plan,
            table,
            column,
            transform,
            value,
            length,
        } => {
            let transform = match transform {
                TransformArg::Hash => MaskTransform::Hash { length },
                TransformArg::Email => MaskTransform::Email,
                TransformArg::Phone => MaskTransform::Phone,
                TransformArg::Null => MaskTransform::Null,
                TransformArg::Constant => MaskTransform::Constant {
                    value: value.context("--transform constant needs --value")?,
                },
            };

            let plan = resolve_plan(store, &plan).await?;
            let mut rules = plan.masking.clone();
            // Replace rather than duplicate: two rules on one column would be
            // refused at run time, and "add" reading as "edit" is what anyone
            // typing this twice expects.
            rules.retain(|r| !(r.table == table && r.column == column));
            rules.push(MaskRule {
                table: table.clone(),
                column: column.clone(),
                transform: transform.clone(),
            });

            let updated = store.set_sync_plan_masking(plan.id, rules).await?;
            eprintln!(
                "{}: {table}.{column} will be {} (revision {})",
                updated.name,
                transform.describe(),
                updated.revision
            );
        }

        MaskCommand::Remove {
            plan,
            table,
            column,
        } => {
            let plan = resolve_plan(store, &plan).await?;
            let mut rules = plan.masking.clone();
            let before = rules.len();
            rules.retain(|r| !(r.table == table && r.column == column));
            if rules.len() == before {
                bail!("{} has no masking rule for {table}.{column}", plan.name);
            }

            let updated = store.set_sync_plan_masking(plan.id, rules).await?;
            eprintln!(
                "{}: {table}.{column} is no longer masked (revision {})",
                updated.name, updated.revision
            );
        }

        MaskCommand::Sql { plan } => {
            let plan = resolve_plan(store, &plan).await?;
            let profile = store.require_profile(plan.profile_id).await?;

            let active: Vec<MaskRule> = plan.active_masking().into_iter().cloned().collect();
            if active.is_empty() {
                eprintln!("{}: no masking rules would run", plan.name);
                return Ok(());
            }

            // A placeholder, not the real salt: this output is meant to be
            // shareable, and the salt is the one value that must not be.
            let updates = db_sync_engine::mask::update_statements(
                profile.engine,
                &active,
                "<salt bound at run time>",
            )?;
            let checks = db_sync_engine::mask::check_statements(profile.engine, &active)?;

            println!("-- masking, applied to the destination after the restore");
            for u in &updates {
                println!("{};", u.statement.sql);
            }
            println!("\n-- read-back; every count must be zero or the sync aborts");
            for c in &checks {
                println!("{};", c.statement.sql);
            }
        }
    }

    Ok(())
}

/// Find a schedule by id or by a unique prefix of its name.
///
/// Requiring a full UUID on the command line would make every one of these
/// commands a copy-and-paste exercise; ambiguity is reported rather than
/// guessed at, because the wrong guess here runs the wrong backup.
async fn resolve_schedule(store: &Store, needle: &str) -> Result<Schedule> {
    if let Ok(id) = Uuid::parse_str(needle) {
        return Ok(store.require_schedule(id).await?);
    }

    let all = store.list_schedules().await?;
    let lowered = needle.to_lowercase();
    let matches: Vec<&Schedule> = all
        .iter()
        .filter(|s| s.name.to_lowercase().starts_with(&lowered))
        .collect();

    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no schedule matches {needle:?}; `dbsync schedule list` shows them all"),
        many => {
            let names: Vec<&str> = many.iter().map(|s| s.name.as_str()).collect();
            bail!("{needle:?} matches several schedules: {}", names.join(", "))
        }
    }
}

async fn run_schedule_command(cmd: ScheduleCommand, store: &Store, json: bool) -> Result<()> {
    match cmd {
        ScheduleCommand::List => {
            let schedules = store.list_schedules().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&schedules)?);
                return Ok(());
            }
            if schedules.is_empty() {
                eprintln!("no schedules configured");
                return Ok(());
            }

            let now = chrono::Utc::now();
            for s in &schedules {
                let next = match s.next_run_at(now) {
                    Some(t) => t
                        .with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string(),
                    None => "—".into(),
                };
                let last =
                    s.last_outcome
                        .map(|o| o.as_str())
                        .unwrap_or(if s.last_run_at.is_some() {
                            "running"
                        } else {
                            "never run"
                        });

                println!(
                    "{}  {:<24} {:<16} {:<7} next {:<17} last {}",
                    s.id,
                    truncate(&s.name, 24),
                    s.cron.as_str(),
                    if s.enabled { "on" } else { "off" },
                    next,
                    last
                );
            }
        }

        ScheduleCommand::Show { schedule } => {
            let s = resolve_schedule(store, &schedule).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
                return Ok(());
            }

            let now = chrono::Utc::now();
            println!("{}", s.name);
            println!("  id           {}", s.id);
            println!(
                "  when         {} ({}, {})",
                s.cron.as_str(),
                s.cron.describe(),
                s.timezone.as_str()
            );
            println!("  enabled      {}", s.enabled);
            println!("  catch up     {}", s.catch_up);
            println!(
                "  next run     {}",
                s.next_run_at(now)
                    .map(|t| t.with_timezone(&chrono::Local).to_rfc3339())
                    .unwrap_or_else(|| "—".into())
            );
            println!(
                "  last run     {}",
                s.last_run_at
                    .map(|t| t.with_timezone(&chrono::Local).to_rfc3339())
                    .unwrap_or_else(|| "never".into())
            );
            println!(
                "  last outcome {}",
                s.last_outcome.map(|o| o.as_str()).unwrap_or("—")
            );
            println!(
                "  kind         {}",
                if s.is_sync() { "sync" } else { "backup" }
            );
            println!("  output       {}", s.action.output_dir.display());
            println!("  verify       {}", s.action.verify);
            println!("  notify       {}", s.notify.as_str());
            println!("  webhook      {}", s.webhook_url.as_deref().unwrap_or("—"));
            if let Some(r) = &s.action.restore {
                println!("  restore as   {:?}", r.naming);
            }
            if let Some(r) = &s.action.retention {
                println!(
                    "  retention    keep_last={:?} max_age_days={:?}",
                    r.keep_last, r.max_age_days
                );
            }
        }

        ScheduleCommand::Run { schedule } => {
            let s = resolve_schedule(store, &schedule).await?;
            eprintln!("running {:?}", s.name);

            let outcome = run_and_wait(store, s.id, json, None).await?;
            report_outcome(outcome)?;
        }

        ScheduleCommand::Tick => {
            let (event_tx, rx) = create_event_channel(EVENT_CHANNEL_CAPACITY);
            let scheduler = Scheduler::new(store.clone(), JobRegistry::new(), event_tx);

            if json {
                tokio::spawn(stream_channel_as_json(rx));
            }

            scheduler.tick_once(chrono::Utc::now()).await;

            let started = scheduler.in_flight_ids().await.len();
            if started == 0 {
                eprintln!("nothing is due");
                return Ok(());
            }
            eprintln!("{started} schedule(s) started");

            wait_for_idle(&scheduler).await;
        }

        ScheduleCommand::Crontab { schedule } => {
            let s = resolve_schedule(store, &schedule).await?;
            print_crontab_line(&s)?;
        }
    }

    Ok(())
}

/// Start one schedule and block until it finishes, returning its outcome.
async fn run_and_wait(
    store: &Store,
    schedule_id: Uuid,
    json: bool,
    tick: Option<Duration>,
) -> Result<Option<JobOutcome>> {
    let (event_tx, rx) = create_event_channel(EVENT_CHANNEL_CAPACITY);
    let mut scheduler = Scheduler::new(store.clone(), JobRegistry::new(), event_tx);
    if let Some(t) = tick {
        scheduler = scheduler.with_tick(t);
    }

    if json {
        tokio::spawn(stream_channel_as_json(rx));
    } else {
        tokio::spawn(stream_channel_as_text(rx));
    }

    let Some(_job_id) = scheduler.run_now(schedule_id).await? else {
        bail!("that schedule is already running");
    };

    wait_for_idle(&scheduler).await;

    Ok(store
        .get_schedule(schedule_id)
        .await?
        .and_then(|s| s.last_outcome))
}

async fn wait_for_idle(scheduler: &Scheduler) {
    // Polling rather than a completion channel: a run can take hours, and a
    // half-second poll costs nothing against that.
    while !scheduler.in_flight_ids().await.is_empty() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Turn a run outcome into a process exit status.
///
/// Cron mails the output of a failing job and CI turns a red build red; both
/// depend on the exit code, so a failed backup must never exit zero.
fn report_outcome(outcome: Option<JobOutcome>) -> Result<()> {
    match outcome {
        Some(JobOutcome::Success) => {
            eprintln!("done");
            Ok(())
        }
        Some(JobOutcome::Cancelled) => bail!("the run was cancelled"),
        Some(JobOutcome::Failed) => {
            bail!("the run failed; `dbsync jobs` and the job log have the detail")
        }
        None => bail!("the run finished without recording an outcome"),
    }
}

/// Print a crontab line, plus the things that actually go wrong when people
/// move a schedule into system cron.
fn print_crontab_line(schedule: &Schedule) -> Result<()> {
    let exe = std::env::current_exe()
        .context("could not determine this executable's path")?
        .display()
        .to_string();

    // `@daily` and friends are valid in a crontab, so the expression can be
    // emitted exactly as the user wrote it.
    println!("# {} — {}", schedule.name, schedule.cron.describe());
    println!(
        "{} {} schedule run {} >> {} 2>&1",
        schedule.cron.as_str(),
        shell_quote(&exe),
        schedule.id,
        shell_quote(&log_path_hint(schedule))
    );
    println!();

    eprintln!("Before using this line:");
    eprintln!();
    eprintln!(
        "  * cron runs with a bare PATH. mysqldump/pg_dump are found via PATH, so either set"
    );
    eprintln!("    PATH= at the top of your crontab or set a tool override on the profile.");
    eprintln!("  * passwords live in the OS keychain, which a cron job can only read while the");
    eprintln!("    keychain is unlocked. On macOS that means an active login session; a headless");
    eprintln!("    server should use the daemon under your own user instead.");
    if schedule.timezone == db_sync_engine::cron::ScheduleTimezone::Utc {
        eprintln!(
            "  * this schedule is set to UTC, but cron reads its expression in local time. The"
        );
        eprintln!("    line above will fire at a different moment than the app would.");
    }
    eprintln!(
        "  * the app's own scheduler will also run this schedule if the app is open. Disable"
    );
    eprintln!("    it in the app to avoid two copies running at once.");

    Ok(())
}

fn log_path_hint(schedule: &Schedule) -> String {
    schedule
        .action
        .output_dir
        .join("dbsync-cron.log")
        .display()
        .to_string()
}

/// Quote a path for a crontab line, which is interpreted by `/bin/sh`.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).chain(['…']).collect()
}

async fn stream_channel_as_json(mut rx: db_sync_engine::events::EventReceiver) {
    loop {
        match rx.recv().await {
            Ok(event) => emit_json(&event),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                eprintln!("warning: dropped {skipped} progress events (consumer too slow)");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn stream_channel_as_text(mut rx: db_sync_engine::events::EventReceiver) {
    loop {
        match rx.recv().await {
            Ok(event) => eprintln!("{}", event.to_log_line()),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                eprintln!("warning: dropped {skipped} progress events (consumer too slow)");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_path_is_left_unquoted() {
        assert_eq!(
            shell_quote("/usr/local/bin/dbsync"),
            "/usr/local/bin/dbsync"
        );
    }

    #[test]
    fn a_path_with_spaces_is_quoted() {
        // "/Applications/DBSync Studio.app/..." is the normal macOS case, and
        // an unquoted crontab line there silently runs the wrong command.
        assert_eq!(
            shell_quote("/Applications/DBSync Studio.app/dbsync"),
            "'/Applications/DBSync Studio.app/dbsync'"
        );
    }

    #[test]
    fn an_embedded_quote_cannot_break_out() {
        assert_eq!(shell_quote("/tmp/it's here"), r"'/tmp/it'\''s here'");
    }

    #[test]
    fn an_empty_string_is_still_quoted() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn long_names_are_shortened_for_the_table() {
        assert_eq!(truncate("short", 24), "short");
        let long = truncate("a name far longer than the column allows", 10);
        assert_eq!(long.chars().count(), 10);
        assert!(long.ends_with('…'));
    }
}
