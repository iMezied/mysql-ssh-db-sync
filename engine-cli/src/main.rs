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

// ── Schedules ───────────────────────────────────────────────────────────

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
