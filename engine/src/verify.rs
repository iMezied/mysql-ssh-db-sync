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
}

impl TableVerdict {
    pub const fn is_failure(&self) -> bool {
        matches!(
            self,
            TableVerdict::RowCountMismatch { .. } | TableVerdict::MissingAtDestination
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
}
