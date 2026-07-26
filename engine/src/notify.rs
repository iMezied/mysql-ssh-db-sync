//! Telling someone what happened.
//!
//! Two channels, one payload. A [`RunReport`] describes a finished scheduled
//! run; the desktop app turns it into a native notification, and a webhook URL
//! turns it into an HTTP POST.
//!
//! # What must never be in a report
//!
//! A webhook body leaves this machine and goes to a third party the user has
//! merely pasted a URL for. So a report carries profile *names*, never
//! connection details: no host, no port, no username, no password, no SSH key
//! path, and no absolute filesystem paths. [`RunReport::artifact_name`] holds
//! the artifact's file name — which is the part a human actually needs, since
//! it contains the timestamp — and not the directory it lives in, which would
//! leak the user's home directory and account name to an outside endpoint.
//!
//! This is enforced by construction: every field is populated by
//! [`RunReport::new`] and there is nowhere to put a `DbConfig`.

use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

use crate::events::JobKind;
use crate::job::JobOutcome;
use crate::verify::VerificationReport;

/// The compact form of a verification, for people rather than for diffing.
///
/// The counts are exported to TypeScript as `number`. specta refuses to emit
/// `usize` as anything else, and rightly: JS has no 64-bit integer. A table
/// count that needed one would be a bigger problem than its representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct VerificationSummary {
    pub passed: bool,
    #[specta(type = f64)]
    pub tables_checked: usize,
    #[specta(type = f64)]
    pub failures: usize,
    #[specta(type = f64)]
    pub skipped: usize,
}

impl From<&VerificationReport> for VerificationSummary {
    fn from(r: &VerificationReport) -> Self {
        Self {
            passed: r.passed(),
            tables_checked: r.tables_checked,
            failures: r.failures,
            skipped: r.skipped,
        }
    }
}

/// What happened on one scheduled run.
// Not `Eq`: `duration_seconds` is an f64, and a duration is a measurement
// rather than an identity — nothing compares two reports for equality.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct RunReport {
    /// Always `"dbsync.run.finished"`, so a receiving endpoint can route on it.
    pub event: String,
    pub schedule_id: Uuid,
    pub schedule_name: String,
    pub job_id: Uuid,
    pub kind: JobKind,
    pub outcome: JobOutcome,
    /// The occurrence this run was for, which is not the same as when it
    /// started if the tick was late or it was a catch-up run.
    pub scheduled_for: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_seconds: f64,
    /// Profile *name*. Never the host, port, user or password.
    pub source_profile: String,
    pub dest_profile: Option<String>,
    pub database: String,
    pub target_database: Option<String>,
    /// File name only — never the directory. See the module docs.
    pub artifact_name: Option<String>,
    #[specta(type = Option<f64>)]
    pub artifact_bytes: Option<u64>,
    pub verification: Option<VerificationSummary>,
    #[specta(type = f64)]
    pub removed_artifacts: usize,
    pub error: Option<String>,
}

/// The inputs a report is assembled from.
///
/// A struct rather than a dozen positional arguments, so adding a field cannot
/// silently reorder two `Option<String>`s at a call site.
pub struct RunOutcome<'a> {
    pub schedule_id: Uuid,
    pub schedule_name: &'a str,
    pub job_id: Uuid,
    pub kind: JobKind,
    pub outcome: JobOutcome,
    pub scheduled_for: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub source_profile: &'a str,
    pub dest_profile: Option<&'a str>,
    pub database: &'a str,
    pub target_database: Option<&'a str>,
    pub artifact_path: Option<&'a std::path::Path>,
    pub verification: Option<&'a VerificationReport>,
    pub removed_artifacts: usize,
    pub error: Option<String>,
}

impl RunReport {
    pub fn new(o: RunOutcome<'_>) -> Self {
        let artifact_bytes = o.artifact_path.and_then(|p| {
            std::fs::metadata(p)
                .ok()
                .filter(|m| m.is_file())
                .map(|m| m.len())
        });

        Self {
            event: "dbsync.run.finished".into(),
            schedule_id: o.schedule_id,
            schedule_name: o.schedule_name.to_string(),
            job_id: o.job_id,
            kind: o.kind,
            outcome: o.outcome,
            scheduled_for: o.scheduled_for,
            started_at: o.started_at,
            finished_at: o.finished_at,
            duration_seconds: (o.finished_at - o.started_at).num_milliseconds() as f64 / 1000.0,
            source_profile: o.source_profile.to_string(),
            dest_profile: o.dest_profile.map(str::to_string),
            database: o.database.to_string(),
            target_database: o.target_database.map(str::to_string),
            // Only the file name. The directory would carry the user's home
            // path, and with it their account name, to an external endpoint.
            artifact_name: o
                .artifact_path
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned()),
            artifact_bytes,
            verification: o.verification.map(VerificationSummary::from),
            removed_artifacts: o.removed_artifacts,
            error: o.error,
        }
    }

    pub const fn succeeded(&self) -> bool {
        matches!(self.outcome, JobOutcome::Success)
    }

    /// A title and body for a native OS notification.
    ///
    /// Short by necessity: on macOS a notification body is truncated after
    /// roughly two lines, so the most important fact goes first.
    pub fn to_notification(&self) -> Notification {
        let verb = match self.kind {
            JobKind::Sync => "Sync",
            JobKind::Backup => "Backup",
            JobKind::Restore => "Restore",
            JobKind::Verify => "Verification",
        };

        let title = match self.outcome {
            JobOutcome::Success => format!("{verb} finished — {}", self.schedule_name),
            JobOutcome::Failed => format!("{verb} FAILED — {}", self.schedule_name),
            JobOutcome::Cancelled => format!("{verb} cancelled — {}", self.schedule_name),
        };

        let body = match (&self.error, &self.verification) {
            // The error is the whole message when there is one.
            (Some(e), _) => truncate(e, 200),
            (None, Some(v)) if !v.passed => format!(
                "{} restored to {}, but verification found {} discrepancy(ies) across {} table(s).",
                self.database,
                self.target_database.as_deref().unwrap_or("the destination"),
                v.failures,
                v.tables_checked
            ),
            (None, Some(v)) => format!(
                "{} → {} · {} table(s) verified · {:.0}s",
                self.database,
                self.target_database.as_deref().unwrap_or("artifact"),
                v.tables_checked,
                self.duration_seconds
            ),
            (None, None) => format!(
                "{} · {} · {:.0}s",
                self.database,
                self.artifact_name.as_deref().unwrap_or("no artifact"),
                self.duration_seconds
            ),
        };

        Notification {
            title,
            body,
            is_failure: !self.succeeded(),
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A native OS notification, for the desktop layer to render.
///
/// The engine defines the content but cannot show it: doing so needs Tauri, and
/// the engine must stay usable from `dbsync`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub is_failure: bool,
}

/// The shape a webhook endpoint expects.
///
/// Inferred from the URL rather than configured. A Slack incoming webhook
/// silently accepts a POST it cannot render and returns 200, so a raw report
/// sent there produces no message and no error — the worst combination. The
/// host name is the one piece of information that is already unambiguous, so
/// it is what decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum WebhookFormat {
    /// The full [`RunReport`] as JSON. For anything that parses it itself.
    Json,
    Slack,
    Teams,
}

impl WebhookFormat {
    /// Work out what an endpoint expects from its host.
    pub fn infer(url: &str) -> Self {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_default();

        if host == "hooks.slack.com" || host.ends_with(".slack.com") {
            WebhookFormat::Slack
        } else if host.ends_with("webhook.office.com")
            || host.ends_with("office.com")
            || host.ends_with("logic.azure.com")
        {
            WebhookFormat::Teams
        } else {
            WebhookFormat::Json
        }
    }
}

/// Build the body for a given endpoint shape.
///
/// The generic form is the report itself, so nothing is lost for a consumer
/// that knows how to read it. The chat forms are deliberately terse: a
/// notification that has to be expanded to learn whether it failed is a
/// notification people stop reading.
pub fn payload_for(format: WebhookFormat, report: &RunReport) -> serde_json::Value {
    let notification = report.to_notification();

    match format {
        WebhookFormat::Json => serde_json::to_value(report).unwrap_or(serde_json::Value::Null),

        WebhookFormat::Slack => serde_json::json!({
            // `text` is the fallback shown in notifications and on clients
            // that do not render attachments.
            "text": notification.title,
            "attachments": [{
                "color": if report.succeeded() { "good" } else { "danger" },
                "fallback": notification.title,
                "text": notification.body,
                "fields": slack_fields(report),
            }],
        }),

        WebhookFormat::Teams => serde_json::json!({
            "@type": "MessageCard",
            "@context": "https://schema.org/extensions",
            "themeColor": if report.succeeded() { "2EB67D" } else { "E01E5A" },
            "summary": notification.title,
            "title": notification.title,
            "text": notification.body,
            "sections": [{
                "facts": teams_facts(report),
            }],
        }),
    }
}

/// The handful of fields worth showing without expanding the message.
fn facts(report: &RunReport) -> Vec<(&'static str, String)> {
    let mut out = vec![
        ("Database", report.database.clone()),
        ("Source", report.source_profile.clone()),
    ];
    if let Some(dest) = &report.dest_profile {
        out.push(("Destination", dest.clone()));
    }
    out.push(("Duration", format!("{:.0}s", report.duration_seconds)));
    if let Some(v) = &report.verification {
        out.push((
            "Verified",
            format!(
                "{} table(s){}",
                v.tables_checked,
                if v.passed {
                    String::new()
                } else {
                    format!(", {} FAILED", v.failures)
                }
            ),
        ));
    }
    out
}

fn slack_fields(report: &RunReport) -> Vec<serde_json::Value> {
    facts(report)
        .into_iter()
        .map(|(title, value)| serde_json::json!({ "title": title, "value": value, "short": true }))
        .collect()
}

fn teams_facts(report: &RunReport) -> Vec<serde_json::Value> {
    facts(report)
        .into_iter()
        .map(|(name, value)| serde_json::json!({ "name": name, "value": value }))
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error("could not build the HTTP client: {0}")]
    Client(String),
    #[error("the request failed: {0}")]
    Transport(String),
    #[error("the endpoint answered {status}")]
    Status { status: u16 },
    #[error("the endpoint redirected to {location:?}; webhooks are not followed across hosts")]
    Redirected { location: String },
}

/// How long a webhook may take before it is abandoned.
///
/// A slow endpoint must never be able to hold up the scheduler, and the report
/// has already been written to job history by the time this runs — the POST is
/// a courtesy copy, not the record.
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);

fn client() -> Result<&'static reqwest::Client, WebhookError> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();

    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(WEBHOOK_TIMEOUT)
                .connect_timeout(WEBHOOK_TIMEOUT)
                // Redirects are not followed. A webhook body describes the
                // user's infrastructure, and a 302 is enough for a compromised
                // or merely misconfigured endpoint to forward it to a host the
                // user never agreed to send anything to.
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("dbsync/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| WebhookError::Client(e.clone()))
}

/// POST a report to a webhook endpoint.
///
/// One attempt. Retrying an endpoint that answered with an error risks
/// delivering the same alert several times, and the run itself is already
/// durably recorded — a failed webhook is a lost notification, not lost data.
/// Failure is returned so the caller can log it against the job.
pub async fn post_webhook(url: &str, report: &RunReport) -> Result<u16, WebhookError> {
    let format = WebhookFormat::infer(url);
    let body = payload_for(format, report);

    let response = client()?
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| WebhookError::Transport(e.to_string()))?;

    let status = response.status();

    if status.is_redirection() {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("(no Location header)")
            .to_string();
        return Err(WebhookError::Redirected { location });
    }

    if !status.is_success() {
        return Err(WebhookError::Status {
            status: status.as_u16(),
        });
    }

    Ok(status.as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{TableVerdict, TableVerification};
    use std::path::Path;

    fn report(outcome: JobOutcome) -> RunReport {
        let started = Utc::now();
        RunReport::new(RunOutcome {
            schedule_id: Uuid::nil(),
            schedule_name: "nightly staging refresh",
            job_id: Uuid::nil(),
            kind: JobKind::Sync,
            outcome,
            scheduled_for: started,
            started_at: started,
            finished_at: started + chrono::Duration::seconds(42),
            source_profile: "prod-mysql",
            dest_profile: Some("staging-mysql"),
            database: "app",
            target_database: Some("app_20260726_030000"),
            artifact_path: Some(Path::new("/Users/someone/backups/app_20260726.sql.gz")),
            verification: None,
            removed_artifacts: 2,
            error: None,
        })
    }

    // ── What must not leak ──────────────────────────────────────────────

    #[test]
    fn a_report_carries_no_connection_details() {
        // The one property that matters most: this body is POSTed to a URL the
        // user pasted, and it must not describe how to reach their database.
        let json = serde_json::to_string(&report(JobOutcome::Success)).unwrap();

        for forbidden in [
            "password", "passwd", "secret", "host", "port", "3306", "5432", "ssh", "key",
        ] {
            assert!(
                !json.to_lowercase().contains(forbidden),
                "webhook payload must not mention {forbidden:?}: {json}"
            );
        }
    }

    #[test]
    fn the_artifact_directory_is_never_sent() {
        // The full path would carry the user's home directory, and with it
        // their account name, to a third-party endpoint.
        let r = report(JobOutcome::Success);
        assert_eq!(r.artifact_name.as_deref(), Some("app_20260726.sql.gz"));

        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("/Users/"), "leaked a home directory: {json}");
        assert!(!json.contains("someone"), "leaked an account name: {json}");
    }

    #[test]
    fn profiles_appear_by_name_only() {
        let r = report(JobOutcome::Success);
        assert_eq!(r.source_profile, "prod-mysql");
        assert_eq!(r.dest_profile.as_deref(), Some("staging-mysql"));
    }

    // ── Shape ───────────────────────────────────────────────────────────

    #[test]
    fn the_payload_is_routable() {
        let r = report(JobOutcome::Success);
        assert_eq!(r.event, "dbsync.run.finished");

        let json: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(json["event"], "dbsync.run.finished");
        assert_eq!(json["outcome"], "success");
        assert_eq!(json["kind"], "sync");
    }

    #[test]
    fn duration_is_measured_between_start_and_finish() {
        assert_eq!(report(JobOutcome::Success).duration_seconds, 42.0);
    }

    #[test]
    fn a_missing_artifact_reports_no_size_rather_than_zero() {
        // Zero bytes and "the file is gone" are different problems.
        let outcome = |path: Option<&'static Path>| RunOutcome {
            schedule_id: Uuid::nil(),
            schedule_name: "n",
            job_id: Uuid::nil(),
            kind: JobKind::Backup,
            outcome: JobOutcome::Success,
            scheduled_for: Utc::now(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            source_profile: "p",
            dest_profile: None,
            database: "app",
            target_database: None,
            artifact_path: path,
            verification: None,
            removed_artifacts: 0,
            error: None,
        };

        let missing = RunReport::new(outcome(Some(Path::new("/nonexistent/nowhere.sql.gz"))));
        assert!(missing.artifact_bytes.is_none());
        assert_eq!(
            missing.artifact_name.as_deref(),
            Some("nowhere.sql.gz"),
            "the name is still reportable when the file is not"
        );

        assert!(RunReport::new(outcome(None)).artifact_name.is_none());
    }

    #[test]
    fn artifact_size_is_read_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sql.gz");
        std::fs::write(&path, b"0123456789").unwrap();

        let r = RunReport::new(RunOutcome {
            schedule_id: Uuid::nil(),
            schedule_name: "n",
            job_id: Uuid::nil(),
            kind: JobKind::Backup,
            outcome: JobOutcome::Success,
            scheduled_for: Utc::now(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            source_profile: "p",
            dest_profile: None,
            database: "app",
            target_database: None,
            artifact_path: Some(&path),
            verification: None,
            removed_artifacts: 0,
            error: None,
        });
        assert_eq!(r.artifact_bytes, Some(10));
    }

    // ── Notifications ───────────────────────────────────────────────────

    #[test]
    fn a_failure_notification_leads_with_the_error() {
        let mut r = report(JobOutcome::Failed);
        r.error = Some("could not connect to the destination: connection refused".into());

        let n = r.to_notification();
        assert!(n.is_failure);
        assert!(n.title.contains("FAILED"));
        assert!(n.title.contains("nightly staging refresh"));
        assert!(n.body.contains("connection refused"));
    }

    #[test]
    fn a_long_error_is_truncated_to_fit_a_notification() {
        let mut r = report(JobOutcome::Failed);
        r.error = Some("x".repeat(5_000));

        let body = r.to_notification().body;
        assert!(
            body.chars().count() <= 200,
            "got {} chars",
            body.chars().count()
        );
        assert!(body.ends_with('…'));
    }

    #[test]
    fn a_failed_verification_is_called_out_even_though_nothing_errored() {
        // The dangerous case: every step exited zero and the data is wrong.
        let mut r = report(JobOutcome::Failed);
        r.verification = Some(VerificationSummary {
            passed: false,
            tables_checked: 12,
            failures: 3,
            skipped: 0,
        });

        let n = r.to_notification();
        assert!(n.is_failure);
        assert!(n.body.contains("verification"), "got: {}", n.body);
        assert!(n.body.contains('3'));
    }

    #[test]
    fn a_successful_run_notification_stays_brief() {
        let mut r = report(JobOutcome::Success);
        r.verification = Some(VerificationSummary {
            passed: true,
            tables_checked: 12,
            failures: 0,
            skipped: 0,
        });

        let n = r.to_notification();
        assert!(!n.is_failure);
        assert!(n.title.contains("finished"));
        assert!(n.body.contains("12 table"));
    }

    #[test]
    fn a_cancelled_run_is_not_reported_as_a_failure_in_the_title() {
        let n = report(JobOutcome::Cancelled).to_notification();
        assert!(n.title.contains("cancelled"));
        assert!(n.is_failure, "still not a success, so it is still surfaced");
    }

    // ── Verification summary ────────────────────────────────────────────

    #[test]
    fn the_summary_drops_the_per_table_detail() {
        // A full report can be thousands of rows; a webhook body should not be.
        let full = VerificationReport {
            tables: vec![
                TableVerification {
                    table: "orders".into(),
                    verdict: TableVerdict::Match,
                };
                500
            ],
            tables_checked: 500,
            failures: 0,
            skipped: 2,
        };

        let summary = VerificationSummary::from(&full);
        assert!(summary.passed);
        assert_eq!(summary.tables_checked, 500);
        assert_eq!(summary.skipped, 2);

        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("orders"), "per-table detail leaked into it");
        assert!(json.len() < 120);
    }
}

#[cfg(test)]
mod webhook_format_tests {
    use super::*;

    fn report(outcome: JobOutcome) -> RunReport {
        let started = Utc::now();
        let mut r = RunReport::new(RunOutcome {
            schedule_id: Uuid::nil(),
            schedule_name: "nightly staging refresh",
            job_id: Uuid::nil(),
            kind: JobKind::Sync,
            outcome,
            scheduled_for: started,
            started_at: started,
            finished_at: started + chrono::Duration::seconds(42),
            source_profile: "prod-mysql",
            dest_profile: Some("staging-mysql"),
            database: "app",
            target_database: Some("app_20260726"),
            artifact_path: None,
            verification: None,
            removed_artifacts: 0,
            error: None,
        });
        r.verification = Some(VerificationSummary {
            passed: matches!(outcome, JobOutcome::Success),
            tables_checked: 12,
            failures: if matches!(outcome, JobOutcome::Success) {
                0
            } else {
                3
            },
            skipped: 0,
        });
        r
    }

    #[test]
    fn slack_urls_are_recognised() {
        assert_eq!(
            WebhookFormat::infer("https://hooks.slack.com/services/T00/B00/xxx"),
            WebhookFormat::Slack
        );
    }

    #[test]
    fn teams_urls_are_recognised() {
        for url in [
            "https://outlook.webhook.office.com/webhookb2/abc",
            "https://prod-12.westus.logic.azure.com/workflows/abc",
        ] {
            assert_eq!(WebhookFormat::infer(url), WebhookFormat::Teams, "{url}");
        }
    }

    #[test]
    fn anything_else_gets_the_raw_report() {
        // The default has to stay the full report: a consumer that parses it
        // must not silently start receiving a chat message instead.
        for url in [
            "https://hooks.example.com/abc",
            "http://localhost:9000/hook",
            "not a url",
        ] {
            assert_eq!(WebhookFormat::infer(url), WebhookFormat::Json, "{url}");
        }
    }

    #[test]
    fn a_lookalike_host_is_not_treated_as_slack() {
        // `hooks.slack.com.evil.test` must not be mistaken for Slack, or a
        // report gets shaped for an endpoint that is not the one it claims.
        assert_eq!(
            WebhookFormat::infer("https://hooks.slack.com.evil.test/x"),
            WebhookFormat::Json
        );
    }

    #[test]
    fn the_generic_payload_is_still_the_whole_report() {
        let r = report(JobOutcome::Success);
        let body = payload_for(WebhookFormat::Json, &r);
        assert_eq!(body["event"], "dbsync.run.finished");
        assert_eq!(body["schedule_name"], "nightly staging refresh");
    }

    #[test]
    fn slack_gets_a_renderable_message_with_a_colour() {
        let body = payload_for(WebhookFormat::Slack, &report(JobOutcome::Success));
        assert!(
            body["text"]
                .as_str()
                .unwrap()
                .contains("nightly staging refresh")
        );
        assert_eq!(body["attachments"][0]["color"], "good");
        // A fallback is what shows in the notification list and on clients
        // that do not render attachments.
        assert!(body["attachments"][0]["fallback"].is_string());

        let failed = payload_for(WebhookFormat::Slack, &report(JobOutcome::Failed));
        assert_eq!(failed["attachments"][0]["color"], "danger");
        assert!(failed["text"].as_str().unwrap().contains("FAILED"));
    }

    #[test]
    fn teams_gets_a_message_card() {
        let body = payload_for(WebhookFormat::Teams, &report(JobOutcome::Success));
        assert_eq!(body["@type"], "MessageCard");
        // Teams refuses a card with no summary.
        assert!(body["summary"].is_string());
        assert_eq!(body["themeColor"], "2EB67D");

        let failed = payload_for(WebhookFormat::Teams, &report(JobOutcome::Failed));
        assert_eq!(failed["themeColor"], "E01E5A");
    }

    #[test]
    fn no_chat_payload_leaks_connection_details() {
        // The same guarantee the raw report makes, restated for the shapes
        // that actually get pasted into a shared channel.
        for format in [WebhookFormat::Slack, WebhookFormat::Teams] {
            let body = payload_for(format, &report(JobOutcome::Failed)).to_string();
            for forbidden in ["password", "3306", "5432", "ssh", "/Users/"] {
                assert!(
                    !body.to_lowercase().contains(forbidden),
                    "{format:?} payload mentions {forbidden:?}: {body}"
                );
            }
        }
    }

    #[test]
    fn the_facts_shown_are_the_ones_worth_reading_at_a_glance() {
        let body = payload_for(WebhookFormat::Slack, &report(JobOutcome::Failed)).to_string();
        assert!(body.contains("prod-mysql"));
        assert!(body.contains("staging-mysql"));
        assert!(
            body.contains("3 FAILED"),
            "a failed verification must be visible: {body}"
        );
    }
}
