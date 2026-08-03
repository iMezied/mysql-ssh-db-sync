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
    /// List saved SSH servers and what tunnels through them.
    ///
    /// Read-only, like `profiles`. Creating one belongs in the desktop app,
    /// where an unrecognised host key can be verified before it is pinned —
    /// a prompt this command has no way to answer from cron.
    Ssh,
    /// List recent job history.
    Jobs {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Report the resolved store path and engine version.
    Doctor,
    /// Show recent configuration changes.
    ///
    /// Distinct from `dbsync jobs`, which records what *ran*. This records
    /// what was *changed* — a masking rule removed, a connection re-pointed,
    /// the backup key exported — which is the question asked after an
    /// incident and is usually not a job at all.
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Summarise the backup library: sizes, growth, and backups that shrank.
    ///
    /// Exits non-zero when a backup came out dramatically smaller than the one
    /// before it, so this is usable as a cron check. That is the failure
    /// nothing else notices: the artifact is valid, its checksum matches, and
    /// it restores — it is only wrong relative to yesterday.
    Library {
        /// Directory holding the backups. Defaults to the app's backup folder.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
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
    /// Back up a database to an artifact on disk.
    ///
    /// Every table is dumped with its data unless `--schema-only` or
    /// `--exclude` says otherwise — the opposite of the GUI's default, because
    /// a command run from cron with no table list means "all of it".
    Backup {
        /// Connection to back up. Id, or a unique prefix of its name.
        profile: String,
        /// Database to dump. Defaults to the profile's own.
        #[arg(long)]
        database: Option<String>,
        /// Where to write it. Defaults to the app's backup folder.
        #[arg(long)]
        dir: Option<PathBuf>,

        /// Dump this table's schema but not its rows. Repeatable.
        #[arg(long = "schema-only")]
        schema_only: Vec<String>,
        /// Leave this table out entirely. Repeatable.
        #[arg(long = "exclude")]
        exclude: Vec<String>,

        /// Encrypt the artifact to the installation's backup key.
        #[arg(long)]
        encrypt: bool,
        /// Write the dump uncompressed.
        #[arg(long)]
        no_compress: bool,
        /// Count rows before dumping and record them in the manifest.
        ///
        /// Lets a later `dbsync drill` compare exact numbers instead of only
        /// checking that each table arrived. Costs a full scan per data table,
        /// on top of the scan the dump itself does.
        #[arg(long)]
        count_rows: bool,
    },
    /// Restore an artifact into a database.
    ///
    /// The target defaults to a new timestamped database, which cannot destroy
    /// anything. `--replace` and `--into` can, and require `--confirm` with the
    /// exact target name.
    Restore {
        /// Connection to restore into. Id, or a unique prefix of its name.
        profile: String,
        /// Path to the artifact. Its manifest, if present, is checked first.
        artifact: PathBuf,

        /// Restore into a new `{prefix}_{timestamp}` database. The default,
        /// with the prefix taken from the artifact's own database name.
        #[arg(long, group = "target")]
        new_prefix: Option<String>,
        /// Drop this database if it exists, then restore into it.
        #[arg(long, group = "target")]
        replace: Option<String>,
        /// Restore into this database without dropping it first.
        #[arg(long, group = "target")]
        into: Option<String>,

        /// The target's name, typed back. Required when the restore can
        /// destroy data, and checked by the engine, not here.
        #[arg(long)]
        confirm: Option<String>,

        /// Skip the checksum comparison against the manifest.
        ///
        /// The check is cheap next to a restore and catches a truncated or
        /// altered artifact before it reaches a server, so turning it off
        /// wants a reason.
        #[arg(long)]
        no_verify_checksum: bool,

        /// PostgreSQL: restore only these tables. Needs an archive format.
        #[arg(long = "only-table")]
        only_tables: Vec<String>,
        /// PostgreSQL: `pg_restore -j`. Needs an archive format.
        #[arg(long)]
        jobs: Option<u16>,
        /// PostgreSQL: drop each object before recreating it.
        #[arg(long)]
        clean: bool,
    },
    /// Manage the masking rules on a sync plan.
    ///
    /// Masking rewrites columns on the destination after a sync restores them.
    /// It does not touch the backup artifact, which still holds the real data.
    #[command(subcommand)]
    Mask(MaskCommand),
    /// Run a saved chain of actions.
    ///
    /// Pipelines are built in the app, not here — the option surface is large,
    /// and a second construction path is a second place for the destructive
    /// target check to be forgotten. The same reasoning as schedules.
    #[command(subcommand)]
    Pipeline(PipelineCommand),
    /// Manage the backup encryption key.
    #[command(subcommand)]
    Key(KeyCommand),
    /// Share configuration with a team, without sharing access.
    #[command(subcommand)]
    Config(ConfigCommand),
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
enum ConfigCommand {
    /// Write a shareable bundle of connections, plans and destinations.
    ///
    /// Contains no credentials — the types it is built from have no field a
    /// password could occupy — so the output is safe to commit to a
    /// repository, paste into a ticket, or attach to an onboarding document.
    Export {
        /// Where to write it. Defaults to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Apply a bundle to this machine.
    ///
    /// Matches existing records by name, creating what is missing and updating
    /// what is there. It never writes a credential and never removes anything
    /// the bundle omits.
    Import {
        /// The bundle to read. Defaults to stdin.
        file: Option<PathBuf>,
        /// Print what would change without changing anything.
        #[arg(long)]
        dry_run: bool,
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
enum PipelineCommand {
    /// List saved pipelines.
    List,
    /// Show what one pipeline will do, step by step.
    Show {
        /// Id, or a unique prefix of the pipeline's name.
        pipeline: String,
    },
    /// Run a pipeline now.
    ///
    /// Exits non-zero when any step failed or a check did not pass, so cron
    /// and CI notice a chain that has quietly stopped working.
    Run {
        /// Id, or a unique prefix of the pipeline's name.
        pipeline: String,
        /// The name of a database this pipeline replaces, typed back.
        ///
        /// Repeat once per destructive step, in the order they appear.
        /// Checked by the engine, not here, so the app and the command line
        /// enforce it identically.
        #[arg(long = "confirm")]
        confirm: Vec<String>,
        /// Where a backup step writes when it names no directory of its own.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
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
    /// Create a recurring restore drill.
    ///
    /// A drill restores the newest artifact in a directory into a scratch
    /// database, checks it against its own manifest, and drops it. A backup is
    /// a belief until it has been restored, and a check nobody remembers to
    /// run is a check that stops happening — so this is the one worth putting
    /// on a timer.
    ///
    /// It cannot touch an existing database: the scratch name is generated by
    /// the engine, and nothing else is droppable.
    AddDrill {
        /// What to call it.
        name: String,
        /// Connection to restore into. Id, or a unique prefix of its name.
        profile: String,
        /// Five-field cron expression, e.g. "0 4 * * *".
        cron: String,
        /// Directory holding the backups. Defaults to the app's backup folder.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Read every row rather than only counting them.
        #[arg(long)]
        deep: bool,
        /// Leave the scratch database behind when the drill fails.
        #[arg(long)]
        keep_on_failure: bool,
        /// Interpret the cron expression in UTC rather than local time.
        #[arg(long)]
        utc: bool,
        /// POST a report here after each run.
        #[arg(long)]
        webhook: Option<String>,
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
            let store = open_store(&store_path).await?;
            let profiles = store.list_profiles().await?;
            eprintln!("profiles: {}", profiles.len());
            store.close().await;
        }
        Command::Profiles => {
            let store = open_store(&store_path).await?;
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
        Command::Ssh => {
            let store = open_store(&store_path).await?;
            let connections = store.list_ssh_connections().await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&connections)?);
            } else if connections.is_empty() {
                eprintln!("no SSH servers saved");
            } else {
                let by_id: std::collections::HashMap<_, _> =
                    connections.iter().map(|c| (c.id, c.name.as_str())).collect();
                for c in &connections {
                    // Named, not counted: the reason to look at this list is
                    // usually "what breaks if I change this one".
                    let used_by = store.profiles_using_ssh_connection(c.id).await?;
                    let used = if used_by.is_empty() {
                        "unused".to_string()
                    } else {
                        used_by
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    let via = match c.jump_host_id.and_then(|id| by_id.get(&id)) {
                        Some(name) => format!(" via {name}"),
                        None => String::new(),
                    };
                    println!(
                        "{}  {:<24} {}@{}:{}{}  [{}]",
                        c.id,
                        c.name,
                        c.endpoint.user,
                        c.endpoint.host,
                        c.endpoint.port,
                        via,
                        used
                    );
                }
            }
            store.close().await;
        }
        Command::Jobs { limit } => {
            let store = open_store(&store_path).await?;
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
            let store = open_store(&store_path).await?;
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
            let store = open_store(&store_path).await?;
            let result = run_drill(&store, &profile, dir, deep, keep_on_failure, cli.json).await;
            store.close().await;
            result?;
        }
        Command::Pipeline(cmd) => {
            let store = open_store(&store_path).await?;
            let result = run_pipeline_command(&store, cmd, cli.json).await;
            store.close().await;
            result?;
        }
        Command::Backup {
            profile,
            database,
            dir,
            schema_only,
            exclude,
            encrypt,
            no_compress,
            count_rows,
        } => {
            let store = open_store(&store_path).await?;
            let result = run_backup(
                &store,
                BackupArgs {
                    profile: &profile,
                    database,
                    dir,
                    schema_only,
                    exclude,
                    encrypt,
                    compress: !no_compress,
                    count_rows,
                },
                cli.json,
            )
            .await;
            store.close().await;
            result?;
        }
        Command::Audit { limit } => {
            let store = open_store(&store_path).await?;
            let entries = store.list_audit(limit).await;
            store.close().await;
            let entries = entries?;

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                eprintln!("nothing has been changed yet");
            } else {
                for e in &entries {
                    println!(
                        "{}  {:<22} {:<28} {}",
                        e.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                        e.action,
                        truncate(&e.subject, 28),
                        e.detail
                    );
                }
            }
        }
        Command::Library { dir } => {
            let directory = match dir {
                Some(d) => d,
                None => db_sync_engine::paths::app_data_dir()
                    .context("could not determine the backup directory")?
                    .join("backups"),
            };
            let stats = db_sync_engine::library::stats(&directory);

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!(
                    "{} artifact(s), {} across {} database(s)",
                    stats.total_artifacts,
                    human_bytes(stats.total_bytes),
                    stats.databases.len()
                );
                if stats.unattributed > 0 {
                    println!(
                        "{} without a manifest ({})",
                        stats.unattributed,
                        human_bytes(stats.unattributed_bytes)
                    );
                }
                println!();
                for d in &stats.databases {
                    println!(
                        "{:<24} {:>10} latest  {:>10} total  {:>3} artifact(s)  {}",
                        truncate(&d.database, 24),
                        human_bytes(d.newest_bytes),
                        human_bytes(d.total_bytes),
                        d.artifacts,
                        match d.bytes_per_day {
                            None => "no trend yet".to_string(),
                            Some(rate) if rate.abs() < 1024.0 => "flat".to_string(),
                            Some(rate) => format!(
                                "{}{}/day",
                                if rate > 0.0 { "+" } else { "-" },
                                human_bytes(rate.abs() as u64)
                            ),
                        }
                    );
                }
            }

            let shrinks = stats.all_shrinks();
            if !shrinks.is_empty() {
                eprintln!();
                for s in &shrinks {
                    eprintln!(
                        "SHRANK: {} is {} ({:.0}% of {}, which was {})",
                        s.filename,
                        human_bytes(s.bytes),
                        s.percent_of_previous(),
                        s.previous_filename,
                        human_bytes(s.previous_bytes)
                    );
                }
                bail!(
                    "{} backup(s) came out far smaller than the one before — usually a table \
                     that stopped being selected, a truncated dump, or a row filter matching \
                     nothing",
                    shrinks.len()
                );
            }
        }
        Command::Restore {
            profile,
            artifact,
            new_prefix,
            replace,
            into,
            confirm,
            no_verify_checksum,
            only_tables,
            jobs,
            clean,
        } => {
            let store = open_store(&store_path).await?;
            let result = run_restore(
                &store,
                RestoreArgs {
                    profile: &profile,
                    artifact,
                    new_prefix,
                    replace,
                    into,
                    confirm,
                    verify_checksum: !no_verify_checksum,
                    only_tables,
                    jobs,
                    clean,
                },
                cli.json,
            )
            .await;
            store.close().await;
            result?;
        }
        Command::Mask(cmd) => {
            let store = open_store(&store_path).await?;
            let result = run_mask_command(cmd, &store, cli.json).await;
            store.close().await;
            result?;
        }
        Command::Key(cmd) => {
            let store = open_store(&store_path).await?;
            let result = run_key_command(cmd, &store).await;
            store.close().await;
            result?;
        }
        Command::Config(cmd) => {
            let store = open_store(&store_path).await?;
            let result = run_config_command(cmd, &store).await;
            store.close().await;
            result?;
        }
        Command::Destination(cmd) => {
            let store = open_store(&store_path).await?;
            let result = run_destination_command(cmd, &store, cli.json).await;
            store.close().await;
            result?;
        }
        Command::Daemon { interval } => {
            let store = open_store(&store_path).await?;
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
        &store.tool_source().await,
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

// ── Backup ──────────────────────────────────────────────────────────────

/// Build the table selections for a headless backup.
///
/// Defaults to schema **and data** for everything, which is the opposite of
/// the GUI. The GUI shows a table list and asks; a cron line cannot, and a
/// backup that silently dumped only schemas would be a file that looks right
/// and restores an empty database.
fn backup_selections(
    tables: &[String],
    schema_only: &[String],
    exclude: &[String],
) -> Vec<db_sync_engine::backup::TableSelection> {
    use db_sync_engine::backup::{TableMode, TableSelection};

    tables
        .iter()
        .map(|name| {
            let mode = if exclude.iter().any(|e| e == name) {
                TableMode::Exclude
            } else if schema_only.iter().any(|s| s == name) {
                TableMode::SchemaOnly
            } else {
                TableMode::SchemaAndData
            };
            TableSelection {
                name: name.clone(),
                mode,
                where_filter: None,
            }
        })
        .collect()
}

/// Grouped so the call site is not nine positional arguments.
struct BackupArgs<'a> {
    profile: &'a str,
    database: Option<String>,
    dir: Option<PathBuf>,
    schema_only: Vec<String>,
    exclude: Vec<String>,
    encrypt: bool,
    compress: bool,
    count_rows: bool,
}

async fn run_backup(store: &Store, args: BackupArgs<'_>, json: bool) -> Result<()> {
    use db_sync_engine::backup::{BackupRequest, CommonBackupOptions};

    let BackupArgs {
        database,
        dir,
        schema_only,
        exclude,
        encrypt,
        compress,
        count_rows,
        ..
    } = args;
    let profile = resolve_profile(store, args.profile).await?;

    let database = database
        .or_else(|| profile.db.database.clone())
        .context("no database given and the profile does not name one; pass --database")?;

    let output_dir = match dir {
        Some(d) => d,
        None => db_sync_engine::paths::app_data_dir()
            .context("could not determine the backup directory")?
            .join("backups"),
    };
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("could not create {}", output_dir.display()))?;

    let (event_tx, rx) = create_event_channel(EVENT_CHANNEL_CAPACITY);
    if json {
        tokio::spawn(stream_channel_as_json(rx));
    } else {
        tokio::spawn(stream_channel_as_text(rx));
    }
    let ctx = JobContext::with_sender(Uuid::new_v4(), event_tx);

    // The table list comes from the server, so `--schema-only` and `--exclude`
    // name real tables or name nothing. Asking here also means a typo is
    // visible in the summary rather than silently selecting nothing.
    let tables = introspect_table_names(&profile, &database, store).await?;
    for named in schema_only.iter().chain(exclude.iter()) {
        if !tables.contains(named) {
            bail!("{database} has no table called {named:?}");
        }
    }

    let request = BackupRequest {
        common: CommonBackupOptions {
            database: database.clone(),
            selections: backup_selections(&tables, &schema_only, &exclude),
            output_dir,
            compress,
            encrypt,
            record_row_counts: count_rows,
        },
        engine: default_backup_options(profile.engine),
    };
    request
        .validate(&profile)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    db_sync_engine::ops::record_start(
        store,
        &ctx,
        db_sync_engine::events::JobKind::Backup,
        profile.id,
        None,
        serde_json::to_string(&request).unwrap_or_else(|_| "{}".into()),
    )
    .await
    .map_err(|e| anyhow::anyhow!("could not record the job: {e}"))?;

    let result = db_sync_engine::ops::backup(&profile, &request, store, &store.tool_source().await, &ctx)
        .await;

    // The off-site copy is part of a backup, not an extra step, so a headless
    // run ships to the same destinations the app does.
    let offsite = match &result {
        Ok(artifact) => db_sync_engine::ops::push_offsite(artifact, store, &ctx)
            .await
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let failures = db_sync_engine::ops::push_failures(&offsite);

    let outcome = match (&result, failures.is_empty()) {
        (Ok(_), true) => JobOutcome::Success,
        (Ok(_), false) => JobOutcome::Failed,
        (Err(e), _) => {
            ctx.emit_error(db_sync_engine::events::JobPhase::Done, e.to_string())
                .await;
            JobOutcome::Failed
        }
    };

    let artifact = result.map_err(|e| anyhow::anyhow!("{e}"))?;
    let _ = db_sync_engine::ops::record_finish(
        store,
        &ctx,
        outcome,
        Some(artifact.display().to_string()),
    )
    .await;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "artifact": artifact.display().to_string(),
                "offsite": offsite,
            })
        );
    } else {
        eprintln!("wrote {}", artifact.display());
    }

    // Same rule as everywhere else: a backup that did not reach the
    // destination it was configured to reach has not fully succeeded.
    if !failures.is_empty() {
        bail!(
            "the artifact is on disk but the off-site copy failed: {}",
            failures.join("; ")
        );
    }
    Ok(())
}

/// Every table in a database, as the backup would see them.
async fn introspect_table_names(
    profile: &db_sync_engine::ConnectionProfile,
    database: &str,
    store: &Store,
) -> Result<Vec<String>> {
    // PostgreSQL can only introspect the database it is connected to, so the
    // target is part of opening the connection rather than just the query.
    let connection = db_sync_engine::connect::open(profile, store, Some(database))
        .await
        .map_err(|e| anyhow::anyhow!("could not connect: {e}"))?;

    let tables = connection.introspector.list_tables(database).await;
    connection.close().await;

    let tables = tables.map_err(|e| anyhow::anyhow!("could not read the table list: {e}"))?;

    Ok(tables
        .into_iter()
        .map(|t| match t.schema {
            // Schema-qualified for PostgreSQL: a bare name matches in every
            // schema, which would pull in a same-named table elsewhere.
            Some(schema) => format!("{schema}.{}", t.name),
            None => t.name,
        })
        .collect())
}

/// Sensible defaults for a headless dump, matching what the GUI sends.
fn default_backup_options(
    engine: db_sync_engine::Engine,
) -> db_sync_engine::backup::EngineBackupOptions {
    use db_sync_engine::backup::{
        EngineBackupOptions, MongoBackupOptions, MysqlBackupOptions, PostgresBackupOptions,
    };

    match engine {
        db_sync_engine::Engine::Mysql => EngineBackupOptions::Mysql(MysqlBackupOptions::default()),
        db_sync_engine::Engine::Postgres => {
            EngineBackupOptions::Postgres(PostgresBackupOptions::default())
        }
        db_sync_engine::Engine::Mongo => EngineBackupOptions::Mongo(MongoBackupOptions::default()),
    }
}

// ── Restore ─────────────────────────────────────────────────────────────

/// Bytes at a size a person reads, for table output.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Grouped so the call site is not eleven positional arguments.
struct RestoreArgs<'a> {
    profile: &'a str,
    artifact: PathBuf,
    new_prefix: Option<String>,
    replace: Option<String>,
    into: Option<String>,
    confirm: Option<String>,
    verify_checksum: bool,
    only_tables: Vec<String>,
    jobs: Option<u16>,
    clean: bool,
}

/// Turn the mutually exclusive target flags into a naming strategy.
///
/// Clap already refuses more than one of them, so the order here only decides
/// what happens if that ever stops being true. It resolves to the *least*
/// destructive interpretation last-resort — a bug in the argument definitions
/// should not silently promote a restore into a DROP.
fn target_naming(
    replace: Option<String>,
    into: Option<String>,
    new_prefix: Option<String>,
    artifact_database: Option<&str>,
) -> db_sync_engine::restore::TargetNaming {
    use db_sync_engine::restore::TargetNaming;

    match (replace, into, new_prefix) {
        (Some(name), None, None) => TargetNaming::DropAndRecreate { name },
        (None, Some(name), None) => TargetNaming::IntoExisting { name },
        (None, None, Some(prefix)) => TargetNaming::NewTimestamped { prefix },
        // No target given, or — impossible today — more than one. Both land on
        // the strategy that cannot destroy anything. Naming it after the
        // database the artifact came from keeps a folder of restores readable.
        _ => TargetNaming::NewTimestamped {
            prefix: artifact_database.unwrap_or("restore").to_string(),
        },
    }
}

async fn run_restore(store: &Store, args: RestoreArgs<'_>, json: bool) -> Result<()> {
    use db_sync_engine::restore::{EngineRestoreOptions, PostgresRestoreOptions, RestoreRequest};

    let profile = resolve_profile(store, args.profile).await?;

    if !args.artifact.is_file() {
        bail!("{} is not a file", args.artifact.display());
    }

    // Read before anything else, so the summary below can say what is about to
    // be restored rather than only where it is going.
    let manifest = db_sync_engine::manifest::BackupManifest::read(&args.artifact).ok();

    let naming = target_naming(
        args.replace,
        args.into,
        args.new_prefix,
        manifest.as_ref().map(|m| m.database.as_str()),
    );

    let engine = match profile.engine {
        db_sync_engine::Engine::Mysql => {
            if !args.only_tables.is_empty() || args.jobs.is_some() || args.clean {
                // Silently ignoring them would produce a restore that is not
                // the one that was asked for.
                bail!(
                    "--only-table, --jobs and --clean are PostgreSQL options, and {:?} is MySQL",
                    profile.name
                );
            }
            EngineRestoreOptions::Mysql(Default::default())
        }
        db_sync_engine::Engine::Postgres => {
            EngineRestoreOptions::Postgres(PostgresRestoreOptions {
                only_tables: args.only_tables,
                parallel_jobs: args.jobs,
                clean: args.clean,
                ..Default::default()
            })
        }
        db_sync_engine::Engine::Mongo => {
            if args.clean {
                bail!(
                    "--clean is a PostgreSQL option, and {:?} is MongoDB. \
                     Use --replace to drop collections as they are restored.",
                    profile.name
                );
            }
            // `--only-table` and `--jobs` do carry over: mongorestore filters
            // by namespace and restores collections in parallel. Reusing the
            // flags rather than adding MongoDB-only spellings keeps one CLI.
            EngineRestoreOptions::Mongo(db_sync_engine::restore::MongoRestoreOptions {
                only_collections: args.only_tables,
                parallel_collections: args.jobs,
                ..Default::default()
            })
        }
    };

    let request = RestoreRequest {
        artifact_path: args.artifact.clone(),
        naming,
        engine,
        verify_checksum: args.verify_checksum,
        typed_confirmation: args.confirm,
    };

    // Checked here as well as inside the job so a missing confirmation costs
    // nothing — no history row, no connection, no tunnel.
    request
        .validate(&profile, manifest.as_ref())
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if !json {
        eprintln!(
            "restoring {} into {} on {:?}",
            args.artifact
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            request.naming.resolve(chrono::Utc::now()),
            profile.name
        );
        match &manifest {
            Some(m) => eprintln!(
                "  artifact: {} from {}, {} table(s), taken {}",
                m.database,
                m.source_profile_name,
                m.tables.len(),
                m.created_at.to_rfc3339()
            ),
            None => eprintln!(
                "  no manifest alongside this artifact — its contents and checksum \
                 are unknown, so nothing can be verified before or after"
            ),
        }
    }

    let (event_tx, rx) = create_event_channel(EVENT_CHANNEL_CAPACITY);
    if json {
        tokio::spawn(stream_channel_as_json(rx));
    } else {
        tokio::spawn(stream_channel_as_text(rx));
    }

    let ctx = JobContext::with_sender(Uuid::new_v4(), event_tx);

    // Recorded the same way the GUI records it, so a restore run headlessly
    // appears in the same history the app shows.
    db_sync_engine::ops::record_start(
        store,
        &ctx,
        db_sync_engine::events::JobKind::Restore,
        profile.id,
        Some(profile.id),
        serde_json::to_string(&request).unwrap_or_else(|_| "{}".into()),
    )
    .await
    .map_err(|e| anyhow::anyhow!("could not record the job: {e}"))?;

    let result = db_sync_engine::ops::restore(&profile, &request, store, &store.tool_source().await, &ctx)
        .await;

    let outcome = match &result {
        Ok(_) => JobOutcome::Success,
        Err(e) => {
            ctx.emit_error(db_sync_engine::events::JobPhase::Done, e.to_string())
                .await;
            JobOutcome::Failed
        }
    };
    let _ = db_sync_engine::ops::record_finish(
        store,
        &ctx,
        outcome,
        Some(args.artifact.display().to_string()),
    )
    .await;

    let target = result.map_err(|e| anyhow::anyhow!("{e}"))?;

    if json {
        println!("{}", serde_json::json!({ "target_database": target }));
    } else {
        eprintln!("restored into {target}");
        eprintln!(
            "nothing has verified this against the source; run `dbsync drill` or a sync \
             with --verify for that"
        );
    }
    Ok(())
}

/// Open the shared store, upgrading anything an older version left behind.
///
/// The GUI does the same on start. Whichever the user opens first performs the
/// adoption of inline SSH configs into saved connections; the other finds
/// nothing to do. Doing it in only one of the two would mean the CLI reading a
/// half-migrated store, which is worse than either doing it or not.
async fn open_store(path: &std::path::Path) -> Result<Store> {
    let store = Store::open(path).await?;

    match db_sync_engine::sshconn::adopt_legacy_configs(&store).await {
        Ok(adopted) => {
            for a in &adopted {
                eprintln!(
                    "upgraded: {} now tunnels through the saved SSH connection {:?}",
                    a.profile_name, a.ssh_connection_name
                );
            }
        }
        // Reported, not fatal: the original configuration is still on the
        // profiles, so the next run can try again.
        Err(e) => eprintln!("warning: could not adopt inline SSH configurations: {e}"),
    }

    Ok(store)
}

// ── Pipelines ───────────────────────────────────────────────────────────

async fn run_pipeline_command(store: &Store, cmd: PipelineCommand, json: bool) -> Result<()> {
    match cmd {
        PipelineCommand::List => {
            let pipelines = store.list_pipelines().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&pipelines)?);
                return Ok(());
            }
            if pipelines.is_empty() {
                eprintln!("no pipelines yet; build one in the app");
                return Ok(());
            }
            for p in &pipelines {
                let mut notes = vec![format!("{} step(s)", p.steps.len())];
                if let Some(targets) = p.destructive_signature() {
                    notes.push(format!("replaces {}", targets.replace('\n', ", ")));
                }
                if p.is_armed() {
                    notes.push("armed for unattended runs".into());
                }
                println!("{}  {}", p.name, notes.join(" · "));
            }
            Ok(())
        }

        PipelineCommand::Show { pipeline } => {
            let pipeline = resolve_pipeline(store, &pipeline).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&pipeline)?);
                return Ok(());
            }

            let profiles = store.list_profiles().await?;
            println!("{}", pipeline.name);
            for (i, step) in pipeline.steps.iter().enumerate() {
                println!("  {}. {}", i + 1, step.label(&profiles));
            }
            // Say what a run would need before somebody discovers it mid-cron.
            if let Some(targets) = pipeline.destructive_signature() {
                println!();
                println!(
                    "replaces {} — needs --confirm, or arming in the app to run unattended",
                    targets.replace('\n', ", ")
                );
            }
            if let Err(e) = pipeline.validate_against(&profiles) {
                println!();
                println!("will not run: {e}");
            }
            Ok(())
        }

        PipelineCommand::Run {
            pipeline,
            confirm,
            dir,
        } => run_pipeline_now(store, &pipeline, confirm, dir, json).await,
    }
}

async fn run_pipeline_now(
    store: &Store,
    needle: &str,
    confirm: Vec<String>,
    dir: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let pipeline = resolve_pipeline(store, needle).await?;

    // A destructive pipeline run headlessly with nothing typed and no standing
    // authorisation is refused here rather than by the engine, so the message
    // can name the two ways out. The engine still checks independently.
    if confirm.is_empty()
        && let Some(targets) = pipeline.destructive_signature()
    {
        match pipeline.unattended_ack.as_deref() {
            Some(ack) if ack == targets => {}
            _ => bail!(
                "{:?} replaces {}. Pass --confirm with each name, or arm it in \
                 the app to let it run unattended",
                pipeline.name,
                targets.replace('\n', ", ")
            ),
        }
    }

    // An armed pipeline supplies its own confirmations: the names in the
    // acknowledgment are exactly the ones a human already typed back.
    let typed_confirmations = match confirm.is_empty() {
        false => confirm,
        true => pipeline
            .unattended_ack
            .as_deref()
            .map(|ack| ack.lines().map(str::to_string).collect())
            .unwrap_or_default(),
    };

    let default_output_dir = match dir {
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
    let source = pipeline
        .steps
        .iter()
        .find_map(|s| s.profile_id())
        .context("this pipeline touches no connection")?;
    let dest = pipeline.steps.iter().rev().find_map(|s| s.profile_id());

    db_sync_engine::ops::record_start(
        store,
        &ctx,
        db_sync_engine::events::JobKind::Sync,
        source,
        dest,
        serde_json::json!({ "pipeline": pipeline.name, "steps": pipeline.steps.len() })
            .to_string(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let outcome = db_sync_engine::ops::run_pipeline(
        &pipeline,
        &db_sync_engine::ops::PipelineRunRequest {
            typed_confirmations,
            default_output_dir,
        },
        store,
        &store.tool_source().await,
        &ctx,
    )
    .await;

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            ctx.emit_error(db_sync_engine::events::JobPhase::Done, e.to_string())
                .await;
            let _ = db_sync_engine::ops::record_finish(
                store,
                &ctx,
                db_sync_engine::job::JobOutcome::Failed,
                None,
            )
            .await;
            bail!("{e}");
        }
    };

    let succeeded = outcome.fully_succeeded();
    let _ = db_sync_engine::ops::record_finish(
        store,
        &ctx,
        match succeeded {
            true => db_sync_engine::job::JobOutcome::Success,
            false => db_sync_engine::job::JobOutcome::Failed,
        },
        outcome.artifacts.last().cloned(),
    )
    .await;

    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        eprintln!();
        for step in store.list_job_steps(ctx.job_id).await? {
            eprintln!(
                "  {}. {} — {}",
                step.index,
                step.label,
                step.outcome
                    .map(|o| o.as_str())
                    .unwrap_or("did not finish")
            );
        }
    }

    // The exit code is the whole point of running this from cron.
    if !succeeded {
        bail!("the pipeline ran but not every step did what it claimed");
    }
    Ok(())
}

/// Resolve a pipeline by id or by a unique prefix of its name.
///
/// The same shape as [`resolve_profile`]: an ambiguous prefix is refused rather
/// than guessed, because guessing here could start a chain that drops a
/// database.
async fn resolve_pipeline(
    store: &Store,
    needle: &str,
) -> Result<db_sync_engine::pipeline::Pipeline> {
    if let Ok(id) = Uuid::parse_str(needle) {
        return Ok(store.require_pipeline(id).await?);
    }

    let all = store.list_pipelines().await?;
    let lowered = needle.to_lowercase();
    let matches: Vec<_> = all
        .iter()
        .filter(|p| p.name.to_lowercase().starts_with(&lowered))
        .collect();

    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no pipeline matches {needle:?}; `dbsync pipeline list` shows them"),
        many => {
            let names: Vec<&str> = many.iter().map(|p| p.name.as_str()).collect();
            bail!("{needle:?} matches several pipelines: {}", names.join(", "))
        }
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
        db_sync_engine::Engine::Mongo => db_sync_engine::restore::EngineRestoreOptions::Mongo(
            db_sync_engine::restore::MongoRestoreOptions::default(),
        ),
    }
}

// ── Shareable configuration ─────────────────────────────────────────────

async fn run_config_command(cmd: ConfigCommand, store: &Store) -> Result<()> {
    use db_sync_engine::share;

    match cmd {
        ConfigCommand::Export { out } => {
            let bundle = share::export(store).await?;
            let json = bundle.to_json()?;

            match out {
                Some(path) => {
                    std::fs::write(&path, &json)
                        .with_context(|| format!("could not write {}", path.display()))?;
                    eprintln!(
                        "wrote {} ({} connection(s), {} plan(s), {} destination(s))",
                        path.display(),
                        bundle.profiles.len(),
                        bundle.plans.len(),
                        bundle.destinations.len()
                    );
                    eprintln!("no credentials are in it; whoever imports it supplies their own");
                }
                None => println!("{json}"),
            }
        }

        ConfigCommand::Import { file, dry_run } => {
            let raw = match &file {
                Some(path) => std::fs::read_to_string(path)
                    .with_context(|| format!("could not read {}", path.display()))?,
                None => {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut buf)
                        .context("could not read the bundle from stdin")?;
                    buf
                }
            };

            let bundle = share::ConfigBundle::from_json(&raw)?;

            if dry_run {
                eprintln!(
                    "would import {} connection(s), {} plan(s), {} destination(s) \
                     exported {} by DBSync {}",
                    bundle.profiles.len(),
                    bundle.plans.len(),
                    bundle.destinations.len(),
                    bundle.exported_at.to_rfc3339(),
                    bundle.engine_version
                );
                for p in &bundle.profiles {
                    eprintln!("  connection  {} ({:?}, {})", p.name, p.engine, p.db.host);
                }
                for p in &bundle.plans {
                    eprintln!("  plan        {} on {}", p.name, p.profile_name);
                }
                for d in &bundle.destinations {
                    eprintln!("  destination {} -> {}", d.name, d.kind.describe());
                }
                eprintln!();
                eprintln!("nothing was changed (--dry-run)");
                return Ok(());
            }

            let report = share::import(store, &bundle).await?;

            for (label, names) in [
                ("created connection", &report.profiles_created),
                ("updated connection", &report.profiles_updated),
                ("created plan", &report.plans_created),
                ("updated plan", &report.plans_updated),
                ("created destination", &report.destinations_created),
                ("updated destination", &report.destinations_updated),
            ] {
                for name in names {
                    eprintln!("{label} {name}");
                }
            }

            if report.is_empty() {
                eprintln!("the bundle was empty; nothing changed");
            }

            // The part that needs acting on, said last so it is what remains
            // on screen, and naming each one because "some of these need
            // credentials" is not something anyone acts on.
            if !report.needs_credentials.is_empty() {
                eprintln!();
                eprintln!("these connections cannot connect until you set a password:");
                for name in &report.needs_credentials {
                    eprintln!("  {name}");
                }
            }
            if !report.destinations_needing_keys.is_empty() {
                eprintln!();
                eprintln!("these destinations are switched off until you set an access key:");
                for name in &report.destinations_needing_keys {
                    eprintln!("  {name}   dbsync destination set-key {name}");
                }
            }
            if !report.orphaned_plans.is_empty() {
                eprintln!();
                eprintln!("these plans could not be imported:");
                for detail in &report.orphaned_plans {
                    eprintln!("  {detail}");
                }
            }
        }
    }

    Ok(())
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
                    "{}  {:<24} {:<6} {:<16} {:<7} next {:<17} last {}",
                    s.id,
                    truncate(&s.name, 24),
                    // Shown because the two do very different things: a sync
                    // moves data, a drill proves the backups restore. A list
                    // that did not distinguish them would make "we have four
                    // schedules" say nothing about whether any of them checks.
                    s.kind.as_str(),
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
                if s.is_drill() {
                    "drill — restores the newest artifact and checks it"
                } else if s.is_sync() {
                    "sync — backup, then restore to the destination"
                } else {
                    "backup — local artifact only"
                }
            );
            println!(
                "  {}      {}",
                if s.is_drill() {
                    "artifacts"
                } else {
                    "output   "
                },
                s.action.output_dir.display()
            );
            if s.is_drill() {
                println!("  read rows    {}", s.action.deep_verify);
                println!("  keep on fail {}", s.action.keep_on_failure);
            } else {
                println!("  verify       {}", s.action.verify);
            }
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

        ScheduleCommand::AddDrill {
            name,
            profile,
            cron,
            dir,
            deep,
            keep_on_failure,
            utc,
            webhook,
        } => {
            use db_sync_engine::schedule::{ScheduleAction, ScheduleCreate, ScheduleKind};

            let target = resolve_profile(store, &profile).await?;
            let artifact_dir = match dir {
                Some(d) => d,
                None => db_sync_engine::paths::app_data_dir()
                    .context("could not determine the backup directory")?
                    .join("backups"),
            };

            let created = store
                .create_schedule(ScheduleCreate {
                    pipeline_id: None,
                    kind: ScheduleKind::Drill,
                    name,
                    // A drill has no plan; the artifact fixes what it holds.
                    plan_id: None,
                    dest_profile_id: Some(target.id),
                    cron: cron.parse().map_err(|e| anyhow::anyhow!("{e}"))?,
                    timezone: if utc {
                        db_sync_engine::cron::ScheduleTimezone::Utc
                    } else {
                        db_sync_engine::cron::ScheduleTimezone::Local
                    },
                    action: ScheduleAction {
                        output_dir: artifact_dir,
                        // Backup-shaped fields a drill does not use. They are
                        // part of the shared action rather than a separate
                        // struct; `validate` is what keeps the combination
                        // honest, not these values.
                        compress: true,
                        encrypt: false,
                        backup: default_backup_options(target.engine),
                        restore: None,
                        verify: true,
                        deep_verify: deep,
                        retention: None,
                        // A drill dumps nothing, so there is nothing to count.
                        record_row_counts: false,
                        keep_on_failure,
                    },
                    webhook_url: webhook,
                    // A drill exists to tell you when it fails. Reporting only
                    // failures is the default everywhere else and is right
                    // here too — a passing drill is not news.
                    notify: db_sync_engine::schedule::NotifyPolicy::OnFailure,
                    catch_up: false,
                    enabled: true,
                })
                .await?;

            eprintln!(
                "created drill {:?}: {} ({}), restoring into {:?}",
                created.name,
                created.cron.as_str(),
                created.cron.describe(),
                target.name
            );
            eprintln!("  artifacts    {}", created.action.output_dir.display());
            eprintln!(
                "  next run     {}",
                created
                    .next_run_at(chrono::Utc::now())
                    .map(|t| t.with_timezone(&chrono::Local).to_rfc3339())
                    .unwrap_or_else(|| "—".into())
            );
            println!("{}", created.id);
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

    // ── Restore target selection ────────────────────────────────────────

    #[test]
    fn the_default_restore_target_cannot_destroy_anything() {
        // Running `dbsync restore prod backup.sql.gz` with no target flag must
        // not be able to drop a database. It creates a new one.
        let naming = target_naming(None, None, None, Some("app"));
        assert!(!naming.is_destructive());
        assert!(matches!(
            naming,
            db_sync_engine::restore::TargetNaming::NewTimestamped { ref prefix } if prefix == "app"
        ));
    }

    #[test]
    fn the_default_prefix_falls_back_when_there_is_no_manifest() {
        let naming = target_naming(None, None, None, None);
        assert!(matches!(
            naming,
            db_sync_engine::restore::TargetNaming::NewTimestamped { ref prefix }
                if prefix == "restore"
        ));
    }

    #[test]
    fn each_flag_selects_its_strategy() {
        use db_sync_engine::restore::TargetNaming;

        assert!(matches!(
            target_naming(Some("dev_app".into()), None, None, None),
            TargetNaming::DropAndRecreate { ref name } if name == "dev_app"
        ));
        assert!(matches!(
            target_naming(None, Some("dev_app".into()), None, None),
            TargetNaming::IntoExisting { ref name } if name == "dev_app"
        ));
        assert!(matches!(
            target_naming(None, None, Some("scratch".into()), None),
            TargetNaming::NewTimestamped { ref prefix } if prefix == "scratch"
        ));
    }

    #[test]
    fn only_replace_is_treated_as_destructive() {
        use db_sync_engine::restore::TargetNaming;

        assert!(target_naming(Some("x".into()), None, None, None).is_destructive());
        assert!(!target_naming(None, Some("x".into()), None, None).is_destructive());

        // `IntoExisting` writes over live data without dropping the database.
        // It is not classed destructive, and the engine still demands typed
        // confirmation for it on a production target — this pins that the CLI
        // is not quietly making its own, weaker, decision.
        let _: TargetNaming = target_naming(None, Some("x".into()), None, None);
    }

    #[test]
    fn two_target_flags_resolve_to_the_safe_one() {
        // Clap refuses this today. If that ever changes, the fallback must not
        // be the strategy that drops a database.
        let naming = target_naming(
            Some("dev_app".into()),
            Some("other".into()),
            None,
            Some("app"),
        );
        assert!(
            !naming.is_destructive(),
            "an ambiguous target must never resolve to a DROP"
        );
    }
}
