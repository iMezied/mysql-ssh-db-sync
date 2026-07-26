//! `dbsync` — headless entry point to the engine.
//!
//! Exists so that scheduled/CI runs have exactly the same capabilities as the
//! GUI. Progress is written to stdout as JSON-lines so it can be piped into a
//! log collector; human-readable output goes to stderr.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use db_sync_engine::ENGINE_VERSION;
use db_sync_engine::events::ProgressEvent;
use db_sync_engine::job::JobContext;
use db_sync_engine::store::Store;

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
    }

    Ok(())
}

/// Bridge a job's live events onto stdout as JSON-lines.
///
/// Broadcast receivers can lag; lagging must skip the missed messages and keep
/// going, never terminate the bridge. The durable record is the job log.
#[allow(dead_code)]
async fn stream_events_as_json(ctx: &JobContext) {
    let mut rx = ctx.subscribe();
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
