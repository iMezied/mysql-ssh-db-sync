//! Structured progress events emitted by every long-running job.
//!
//! Two consumers exist and both must see the same stream:
//!   * `engine-cli` serialises these to JSON-lines on stdout.
//!   * the desktop app forwards them to the webview as typed Tauri events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Backup,
    Restore,
    Verify,
    Sync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum JobPhase {
    Initializing,
    SshConnect,
    Tunneling,
    Introspect,
    DumpSchema,
    DumpData,
    Compress,
    Transfer,
    Restore,
    Verify,
    Cleanup,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// What the `done`/`total` pair on a [`ProgressEvent`] counts.
///
/// Carried explicitly because the two are not interchangeable to a reader:
/// "table 12 of 47" and "3 MB of 10 MB" are the same numbers with different
/// meanings. It also decides whether an estimate is worth showing at all —
/// bytes advance at a roughly steady rate, table counts do not, because table
/// sizes differ by orders of magnitude within one database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ProgressUnit {
    Bytes,
    Tables,
}

/// A single point-in-time progress record.
///
/// Every field beyond `job_id`/`ts`/`phase`/`level`/`message` is optional
/// because different phases have different meaningful measurements: a schema
/// dump reports bytes, a data dump reports per-table counts.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ProgressEvent {
    pub job_id: Uuid,
    pub ts: DateTime<Utc>,
    pub phase: JobPhase,
    pub level: LogLevel,
    pub message: String,
    pub table: Option<String>,
    // Exported to TypeScript as `number`. JS has no u64, and f64 represents
    // integers exactly up to 2^53 (~9 PB / 9e15 rows) — far beyond any real
    // database. Emitting `bigint` instead would force BigInt handling through
    // the whole UI for no benefit.
    #[specta(type = Option<f64>)]
    pub done: Option<u64>,
    #[specta(type = Option<f64>)]
    pub total: Option<u64>,
    /// What `done`/`total` count. `None` whenever they are.
    pub unit: Option<ProgressUnit>,
    /// Size of the artifact written so far.
    ///
    /// Separate from `done` because the two answer different questions during
    /// the same phase: a MySQL dump knows which table it is on *and* how large
    /// the file has grown, and neither one implies the other.
    #[specta(type = Option<f64>)]
    pub bytes: Option<u64>,
    #[specta(type = Option<f64>)]
    pub rows: Option<u64>,
    pub percent: Option<f64>,
}

impl ProgressEvent {
    pub fn new(job_id: Uuid, phase: JobPhase, message: impl Into<String>) -> Self {
        Self {
            job_id,
            ts: Utc::now(),
            phase,
            level: LogLevel::Info,
            message: message.into(),
            table: None,
            done: None,
            total: None,
            unit: None,
            bytes: None,
            rows: None,
            percent: None,
        }
    }

    pub fn with_level(mut self, level: LogLevel) -> Self {
        self.level = level;
        self
    }

    pub fn with_table(mut self, table: impl Into<String>) -> Self {
        self.table = Some(table.into());
        self
    }

    /// Progress measured in bytes — transfers, and restores replaying a dump.
    pub fn with_progress(mut self, done: u64, total: u64) -> Self {
        self.set_progress(done, total, ProgressUnit::Bytes);
        self
    }

    /// Progress measured in tables, for dumps that walk a table list.
    pub fn with_tables(mut self, done: u64, total: u64) -> Self {
        self.set_progress(done, total, ProgressUnit::Tables);
        self
    }

    fn set_progress(&mut self, done: u64, total: u64, unit: ProgressUnit) {
        self.done = Some(done);
        self.total = Some(total);
        self.unit = Some(unit);
        self.percent = Some(if total > 0 {
            (done as f64 / total as f64) * 100.0
        } else {
            0.0
        });
    }

    /// How large the artifact has grown so far.
    pub fn with_bytes(mut self, bytes: u64) -> Self {
        self.bytes = Some(bytes);
        self
    }

    pub fn with_rows(mut self, rows: u64) -> Self {
        self.rows = Some(rows);
        self
    }

    /// Render as a single log line for the durable job log.
    pub fn to_log_line(&self) -> String {
        let level = match self.level {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        };
        match &self.table {
            Some(t) => format!(
                "{} [{}] {:?} ({}) {}",
                self.ts.to_rfc3339(),
                level,
                self.phase,
                t,
                self.message
            ),
            None => format!(
                "{} [{}] {:?} {}",
                self.ts.to_rfc3339(),
                level,
                self.phase,
                self.message
            ),
        }
    }
}

pub type EventSender = tokio::sync::broadcast::Sender<ProgressEvent>;
pub type EventReceiver = tokio::sync::broadcast::Receiver<ProgressEvent>;

/// Capacity of the live event fan-out channel.
///
/// This is a *lossy* broadcast used for live UI updates only. Subscribers that
/// fall behind receive `RecvError::Lagged` and must continue rather than break;
/// the durable record of a job is the job log in the store, never this channel.
pub const EVENT_CHANNEL_CAPACITY: usize = 4096;

pub fn create_event_channel(capacity: usize) -> (EventSender, EventReceiver) {
    tokio::sync::broadcast::channel(capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_percentage_is_computed() {
        let e = ProgressEvent::new(Uuid::nil(), JobPhase::DumpData, "x").with_progress(25, 100);
        assert_eq!(e.percent, Some(25.0));
    }

    #[test]
    fn zero_total_does_not_divide_by_zero() {
        let e = ProgressEvent::new(Uuid::nil(), JobPhase::DumpData, "x").with_progress(0, 0);
        assert_eq!(e.percent, Some(0.0));
    }

    #[test]
    fn the_unit_says_what_the_numbers_count() {
        let bytes = ProgressEvent::new(Uuid::nil(), JobPhase::Transfer, "x").with_progress(1, 2);
        assert_eq!(bytes.unit, Some(ProgressUnit::Bytes));

        let tables = ProgressEvent::new(Uuid::nil(), JobPhase::DumpData, "x").with_tables(1, 2);
        assert_eq!(tables.unit, Some(ProgressUnit::Tables));
        assert_eq!(tables.percent, Some(50.0));
    }

    #[test]
    fn artifact_size_is_independent_of_step_progress() {
        let e = ProgressEvent::new(Uuid::nil(), JobPhase::DumpData, "x")
            .with_tables(3, 40)
            .with_bytes(9_000);
        assert_eq!(e.done, Some(3));
        assert_eq!(e.bytes, Some(9_000));
    }

    #[test]
    fn phases_serialize_as_snake_case() {
        let json = serde_json::to_string(&JobPhase::DumpSchema).unwrap();
        assert_eq!(json, "\"dump_schema\"");
    }
}
