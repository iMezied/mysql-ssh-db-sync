//! Restore verification.
//!
//! The bash predecessor "verified" restores by reading
//! `information_schema.TABLES.TABLE_ROWS`, which is a planner estimate for
//! InnoDB and frequently reports 0 for a freshly imported table. That turned a
//! failed restore into a green checkmark. Verification here compares exact
//! counts and reports every discrepancy.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum TableVerdict {
    /// Exact counts agree.
    Match,
    /// Present on both sides with differing counts.
    RowCountMismatch {
        #[specta(type = f64)]
        source: u64,
        #[specta(type = f64)]
        destination: u64,
    },
    /// In the plan but absent from the destination.
    MissingAtDestination,
    /// At the destination but not expected. Usually harmless.
    UnexpectedAtDestination,
    /// Counting was skipped, e.g. the table exceeded the time budget.
    Skipped { reason: String },
    /// Counts agree but the contents do not.
    ///
    /// The dangerous case, and the reason digests exist: the right number of
    /// rows holding the wrong bytes. Truncated text, a mangled character set,
    /// a column that came back NULL — every one of those passes a row count.
    ContentMismatch {
        #[specta(type = f64)]
        rows: u64,
    },
    /// Counts and contents agree but the columns differ.
    SchemaMismatch {
        /// Columns on the source that the destination does not have.
        missing: Vec<String>,
        /// Columns at the destination that the source does not have.
        extra: Vec<String>,
    },
}

impl TableVerdict {
    pub const fn is_failure(&self) -> bool {
        matches!(
            self,
            TableVerdict::RowCountMismatch { .. }
                | TableVerdict::MissingAtDestination
                | TableVerdict::ContentMismatch { .. }
                | TableVerdict::SchemaMismatch { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TableVerification {
    pub table: String,
    pub verdict: TableVerdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct VerificationReport {
    pub tables: Vec<TableVerification>,
    pub tables_checked: usize,
    pub failures: usize,
    pub skipped: usize,
}

impl VerificationReport {
    pub fn passed(&self) -> bool {
        self.failures == 0
    }

    /// Render as Markdown for export and for the job log.
    pub fn to_markdown(&self) -> String {
        let mut out = String::from("| Table | Result |\n|---|---|\n");
        for t in &self.tables {
            let cell = match &t.verdict {
                TableVerdict::Match => "OK".to_string(),
                TableVerdict::RowCountMismatch {
                    source,
                    destination,
                } => format!("MISMATCH: source {source}, destination {destination}"),
                TableVerdict::MissingAtDestination => "MISSING at destination".to_string(),
                TableVerdict::UnexpectedAtDestination => "unexpected at destination".to_string(),
                TableVerdict::Skipped { reason } => format!("skipped ({reason})"),
                TableVerdict::ContentMismatch { rows } => {
                    format!("CONTENT MISMATCH: {rows} rows on both sides, but the data differs")
                }
                TableVerdict::SchemaMismatch { missing, extra } => {
                    let mut parts = Vec::new();
                    if !missing.is_empty() {
                        parts.push(format!("missing columns: {}", missing.join(", ")));
                    }
                    if !extra.is_empty() {
                        parts.push(format!("extra columns: {}", extra.join(", ")));
                    }
                    format!("SCHEMA MISMATCH: {}", parts.join("; "))
                }
            };
            out.push_str(&format!("| {} | {} |\n", t.table, cell));
        }
        out
    }
}

/// Compare exact row counts from both sides.
///
/// `expected` holds the tables the plan said would carry data, with their
/// source counts. `actual` holds what the destination reports. Tables that were
/// intentionally schema-only belong in `schema_only` so their emptiness is not
/// reported as a failure.
pub fn build_report(
    expected: &BTreeMap<String, u64>,
    actual: &BTreeMap<String, u64>,
    schema_only: &[String],
    skipped: &BTreeMap<String, String>,
) -> VerificationReport {
    let mut tables = Vec::new();

    for (name, src_count) in expected {
        if let Some(reason) = skipped.get(name) {
            tables.push(TableVerification {
                table: name.clone(),
                verdict: TableVerdict::Skipped {
                    reason: reason.clone(),
                },
            });
            continue;
        }

        match actual.get(name) {
            None => tables.push(TableVerification {
                table: name.clone(),
                verdict: TableVerdict::MissingAtDestination,
            }),
            Some(dst_count) if dst_count == src_count => tables.push(TableVerification {
                table: name.clone(),
                verdict: TableVerdict::Match,
            }),
            Some(dst_count) => tables.push(TableVerification {
                table: name.clone(),
                verdict: TableVerdict::RowCountMismatch {
                    source: *src_count,
                    destination: *dst_count,
                },
            }),
        }
    }

    // Schema-only tables must exist but are expected to be empty.
    for name in schema_only {
        match actual.get(name) {
            None => tables.push(TableVerification {
                table: name.clone(),
                verdict: TableVerdict::MissingAtDestination,
            }),
            Some(0) => tables.push(TableVerification {
                table: name.clone(),
                verdict: TableVerdict::Match,
            }),
            Some(n) => tables.push(TableVerification {
                table: name.clone(),
                verdict: TableVerdict::RowCountMismatch {
                    source: 0,
                    destination: *n,
                },
            }),
        }
    }

    for name in actual.keys() {
        let known = expected.contains_key(name) || schema_only.iter().any(|s| s == name);
        if !known {
            tables.push(TableVerification {
                table: name.clone(),
                verdict: TableVerdict::UnexpectedAtDestination,
            });
        }
    }

    tables.sort_by(|a, b| a.table.cmp(&b.table));

    let failures = tables.iter().filter(|t| t.verdict.is_failure()).count();
    let skipped_count = tables
        .iter()
        .filter(|t| matches!(t.verdict, TableVerdict::Skipped { .. }))
        .count();

    VerificationReport {
        tables_checked: tables.len(),
        failures,
        skipped: skipped_count,
        tables,
    }
}

// ── Content and schema comparison ───────────────────────────────────────

/// The deeper evidence, gathered from both sides.
///
/// Separate from [`build_report`] on purpose: row counts are cheap and always
/// available, digests need a full table scan and can legitimately be
/// unavailable. Layering the expensive check on top of the cheap one keeps
/// "we could not digest this" distinguishable from "this matched".
#[derive(Debug, Clone, Default)]
pub struct DeepComparison {
    pub source_digests: BTreeMap<String, Option<String>>,
    pub dest_digests: BTreeMap<String, Option<String>>,
    pub source_columns: BTreeMap<String, Vec<String>>,
    pub dest_columns: BTreeMap<String, Vec<String>>,
    pub row_counts: BTreeMap<String, u64>,
}

/// Upgrade `Match` verdicts that the deeper evidence contradicts.
///
/// Only tables currently reported as matching are examined. A row-count
/// mismatch is already a failure, and telling the user their contents also
/// differ adds noise to a problem they can already see.
///
/// A missing digest never turns a match into a failure. Being unable to
/// compare is not evidence of a difference, and treating it as one would make
/// verification cry wolf on exactly the exotic tables people care about most.
pub fn refine_with_contents(report: &mut VerificationReport, deep: &DeepComparison) {
    for entry in &mut report.tables {
        if !matches!(entry.verdict, TableVerdict::Match) {
            continue;
        }

        // Schema first: differing columns explain a differing digest, and
        // "you are missing a column" is far more actionable than "the bytes
        // do not match".
        let src_cols = deep.source_columns.get(&entry.table);
        let dst_cols = deep.dest_columns.get(&entry.table);
        if let (Some(src), Some(dst)) = (src_cols, dst_cols) {
            let missing: Vec<String> = src.iter().filter(|c| !dst.contains(c)).cloned().collect();
            let extra: Vec<String> = dst.iter().filter(|c| !src.contains(c)).cloned().collect();

            if !missing.is_empty() || !extra.is_empty() {
                entry.verdict = TableVerdict::SchemaMismatch { missing, extra };
                continue;
            }
        }

        if let (Some(Some(src)), Some(Some(dst))) = (
            deep.source_digests.get(&entry.table),
            deep.dest_digests.get(&entry.table),
        ) && src != dst
        {
            entry.verdict = TableVerdict::ContentMismatch {
                rows: deep.row_counts.get(&entry.table).copied().unwrap_or(0),
            };
        }
    }

    // The tallies are derived, so they have to be recomputed rather than
    // adjusted — an off-by-one here would make `passed()` disagree with the
    // table list it is summarising.
    report.failures = report
        .tables
        .iter()
        .filter(|t| t.verdict.is_failure())
        .count();
    report.skipped = report
        .tables
        .iter()
        .filter(|t| matches!(t.verdict, TableVerdict::Skipped { .. }))
        .count();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn matching_counts_pass() {
        let r = build_report(
            &map(&[("orders", 100), ("users", 5)]),
            &map(&[("orders", 100), ("users", 5)]),
            &[],
            &BTreeMap::new(),
        );
        assert!(r.passed());
        assert_eq!(r.failures, 0);
        assert_eq!(r.tables_checked, 2);
    }

    #[test]
    fn row_count_mismatch_fails() {
        let r = build_report(
            &map(&[("orders", 100)]),
            &map(&[("orders", 99)]),
            &[],
            &BTreeMap::new(),
        );
        assert!(!r.passed());
        assert_eq!(r.failures, 1);
    }

    #[test]
    fn a_partially_restored_table_is_never_reported_as_ok() {
        // The exact failure the old TABLE_ROWS check missed.
        let r = build_report(
            &map(&[("orders", 1_000_000)]),
            &map(&[("orders", 0)]),
            &[],
            &BTreeMap::new(),
        );
        assert!(!r.passed());
        assert!(matches!(
            r.tables[0].verdict,
            TableVerdict::RowCountMismatch {
                source: 1_000_000,
                destination: 0
            }
        ));
    }

    #[test]
    fn missing_table_fails() {
        let r = build_report(
            &map(&[("orders", 1)]),
            &BTreeMap::new(),
            &[],
            &BTreeMap::new(),
        );
        assert_eq!(r.failures, 1);
        assert!(matches!(
            r.tables[0].verdict,
            TableVerdict::MissingAtDestination
        ));
    }

    #[test]
    fn schema_only_tables_are_expected_to_be_empty() {
        let r = build_report(
            &map(&[("orders", 10)]),
            &map(&[("orders", 10), ("audit_log", 0)]),
            &["audit_log".to_string()],
            &BTreeMap::new(),
        );
        assert!(r.passed(), "an empty schema-only table is correct");
    }

    #[test]
    fn schema_only_table_with_rows_is_a_mismatch() {
        let r = build_report(
            &map(&[]),
            &map(&[("audit_log", 42)]),
            &["audit_log".to_string()],
            &BTreeMap::new(),
        );
        assert_eq!(r.failures, 1);
    }

    #[test]
    fn missing_schema_only_table_fails() {
        let r = build_report(
            &map(&[]),
            &map(&[]),
            &["audit_log".to_string()],
            &BTreeMap::new(),
        );
        assert_eq!(r.failures, 1);
    }

    #[test]
    fn unexpected_destination_tables_are_reported_but_not_failures() {
        let r = build_report(
            &map(&[("orders", 1)]),
            &map(&[("orders", 1), ("leftover", 7)]),
            &[],
            &BTreeMap::new(),
        );
        assert!(r.passed());
        assert!(
            r.tables.iter().any(
                |t| t.table == "leftover" && t.verdict == TableVerdict::UnexpectedAtDestination
            )
        );
    }

    #[test]
    fn skipped_tables_are_not_failures_but_are_counted() {
        let mut skipped = BTreeMap::new();
        skipped.insert("huge".to_string(), "exceeded time budget".to_string());

        let r = build_report(&map(&[("huge", 999)]), &map(&[]), &[], &skipped);
        assert!(r.passed(), "a skipped count is unknown, not wrong");
        assert_eq!(r.skipped, 1);
    }

    #[test]
    fn report_is_sorted_for_stable_diffing() {
        let r = build_report(
            &map(&[("zebra", 1), ("alpha", 1)]),
            &map(&[("zebra", 1), ("alpha", 1)]),
            &[],
            &BTreeMap::new(),
        );
        let names: Vec<&str> = r.tables.iter().map(|t| t.table.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zebra"]);
    }

    #[test]
    fn markdown_export_lists_every_table() {
        let r = build_report(
            &map(&[("orders", 100)]),
            &map(&[("orders", 99)]),
            &[],
            &BTreeMap::new(),
        );
        let md = r.to_markdown();
        assert!(md.contains("| orders |"));
        assert!(md.contains("MISMATCH"));
    }

    // ── Deep comparison ─────────────────────────────────────────────────

    fn matching_report(table: &str, rows: u64) -> VerificationReport {
        let mut expected = BTreeMap::new();
        expected.insert(table.to_string(), rows);
        build_report(&expected, &expected.clone(), &[], &BTreeMap::new())
    }

    fn deep(table: &str, src: &str, dst: &str) -> DeepComparison {
        let mut d = DeepComparison::default();
        d.source_digests.insert(table.into(), Some(src.into()));
        d.dest_digests.insert(table.into(), Some(dst.into()));
        d.row_counts.insert(table.into(), 42);
        d
    }

    #[test]
    fn equal_counts_with_different_contents_is_a_failure() {
        // The whole point: this is what a row count cannot see.
        let mut report = matching_report("orders", 42);
        assert!(report.passed());

        refine_with_contents(&mut report, &deep("orders", "aaa", "bbb"));

        assert!(!report.passed());
        assert_eq!(report.failures, 1);
        assert_eq!(
            report.tables[0].verdict,
            TableVerdict::ContentMismatch { rows: 42 }
        );
    }

    #[test]
    fn equal_counts_with_equal_contents_still_passes() {
        let mut report = matching_report("orders", 42);
        refine_with_contents(&mut report, &deep("orders", "same", "same"));
        assert!(report.passed());
        assert_eq!(report.tables[0].verdict, TableVerdict::Match);
    }

    #[test]
    fn a_missing_digest_never_manufactures_a_failure() {
        // Being unable to compare is not evidence of a difference.
        let mut report = matching_report("blobs", 5);
        let mut d = DeepComparison::default();
        d.source_digests.insert("blobs".into(), None);
        d.dest_digests.insert("blobs".into(), Some("x".into()));

        refine_with_contents(&mut report, &d);
        assert!(report.passed());
        assert_eq!(report.tables[0].verdict, TableVerdict::Match);
    }

    #[test]
    fn a_column_difference_is_reported_instead_of_a_content_difference() {
        // "you are missing a column" is far more actionable than "the bytes
        // do not match", and it explains the digest difference anyway.
        let mut report = matching_report("users", 10);
        let mut d = deep("users", "aaa", "bbb");
        d.source_columns
            .insert("users".into(), vec!["id".into(), "email".into()]);
        d.dest_columns.insert("users".into(), vec!["id".into()]);

        refine_with_contents(&mut report, &d);

        assert_eq!(
            report.tables[0].verdict,
            TableVerdict::SchemaMismatch {
                missing: vec!["email".into()],
                extra: vec![],
            }
        );
        assert!(!report.passed());
    }

    #[test]
    fn an_extra_destination_column_is_reported_too() {
        let mut report = matching_report("users", 10);
        let mut d = deep("users", "same", "same");
        d.source_columns.insert("users".into(), vec!["id".into()]);
        d.dest_columns
            .insert("users".into(), vec!["id".into(), "migrated_at".into()]);

        refine_with_contents(&mut report, &d);
        assert_eq!(
            report.tables[0].verdict,
            TableVerdict::SchemaMismatch {
                missing: vec![],
                extra: vec!["migrated_at".into()],
            }
        );
    }

    #[test]
    fn column_order_alone_is_not_a_mismatch() {
        // A restore may reorder columns; that is not data loss.
        let mut report = matching_report("users", 10);
        let mut d = deep("users", "same", "same");
        d.source_columns
            .insert("users".into(), vec!["id".into(), "email".into()]);
        d.dest_columns
            .insert("users".into(), vec!["email".into(), "id".into()]);

        refine_with_contents(&mut report, &d);
        assert_eq!(report.tables[0].verdict, TableVerdict::Match);
    }

    #[test]
    fn an_existing_row_count_failure_is_not_relabelled() {
        // The user can already see the counts differ; saying the contents also
        // differ is noise on a problem they are looking at.
        let mut expected = BTreeMap::new();
        expected.insert("orders".to_string(), 10u64);
        let mut actual = BTreeMap::new();
        actual.insert("orders".to_string(), 9u64);
        let mut report = build_report(&expected, &actual, &[], &BTreeMap::new());

        refine_with_contents(&mut report, &deep("orders", "aaa", "bbb"));

        assert!(matches!(
            report.tables[0].verdict,
            TableVerdict::RowCountMismatch { .. }
        ));
        assert_eq!(report.failures, 1);
    }

    #[test]
    fn the_markdown_export_explains_a_content_mismatch() {
        let mut report = matching_report("orders", 42);
        refine_with_contents(&mut report, &deep("orders", "aaa", "bbb"));
        let md = report.to_markdown();
        assert!(md.contains("CONTENT MISMATCH"), "got: {md}");
        assert!(md.contains("42 rows on both sides"), "got: {md}");
    }
}
