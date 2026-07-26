//! Sync plans: a named, reusable table selection.
//!
//! This is what replaces the Bash tool's `tables.conf` — a file the user had to
//! maintain by hand, git-ignore, and keep in step with the schema. A plan is
//! attached to a source profile, versioned so a change can be reasoned about,
//! and reused by scheduled runs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

use crate::backup::TableSelection;
use crate::mask::MaskRule;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SyncPlan {
    pub id: Uuid,
    pub profile_id: Uuid,
    pub name: String,
    pub database: String,
    pub selections: Vec<TableSelection>,
    /// Columns masked on the destination after a sync restores this plan.
    ///
    /// Lives here rather than on the schedule because it describes the data,
    /// not the timing — every schedule running this plan inherits the same
    /// protection instead of keeping a copy that can drift.
    #[serde(default)]
    pub masking: Vec<MaskRule>,
    /// Bumped on every save, so a plan that changed under a schedule is
    /// visible rather than silent.
    pub revision: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SyncPlanCreate {
    pub profile_id: Uuid,
    pub name: String,
    pub database: String,
    pub selections: Vec<TableSelection>,
    #[serde(default)]
    pub masking: Vec<MaskRule>,
}

impl SyncPlan {
    pub fn tables_with_data(&self) -> Vec<String> {
        self.selections
            .iter()
            .filter(|s| s.mode == crate::backup::TableMode::SchemaAndData)
            .map(|s| s.name.clone())
            .collect()
    }

    /// Masking rules whose table is actually copied with data.
    ///
    /// A rule on a schema-only or excluded table protects nothing because
    /// nothing reaches the destination — safe, but not the same as active.
    pub fn active_masking(&self) -> Vec<&MaskRule> {
        let with_data = self.tables_with_data();
        self.masking
            .iter()
            .filter(|r| with_data.contains(&r.table))
            .collect()
    }

    pub fn schema_only_tables(&self) -> Vec<String> {
        self.selections
            .iter()
            .filter(|s| s.mode == crate::backup::TableMode::SchemaOnly)
            .map(|s| s.name.clone())
            .collect()
    }

    /// Tables in the plan that no longer exist on the source.
    ///
    /// A plan outlives the schema it was written against; a scheduled run
    /// should say which tables have gone rather than fail obscurely or, worse,
    /// silently back up less than the user believes.
    pub fn missing_from(&self, available: &[String]) -> Vec<String> {
        self.selections
            .iter()
            .filter(|s| s.mode != crate::backup::TableMode::Exclude)
            .map(|s| s.name.clone())
            .filter(|name| !available.contains(name))
            .collect()
    }

    /// Tables on the source that the plan says nothing about.
    ///
    /// These default to schema-only, which is safe, but a new table carrying
    /// data the user wanted is worth flagging.
    pub fn unlisted_in(&self, available: &[String]) -> Vec<String> {
        available
            .iter()
            .filter(|name| !self.selections.iter().any(|s| &&s.name == name))
            .cloned()
            .collect()
    }
}

/// Parse a legacy `tables.conf` into selections.
///
/// One table per line, `#` comments, blanks ignored — the format the Bash tool
/// used. Everything listed carries data; everything else defaults to
/// schema-only, which is exactly what the old script did.
pub fn parse_tables_conf(contents: &str) -> Vec<TableSelection> {
    let mut seen = std::collections::HashSet::new();
    contents
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        // The old file allowed trailing comments and stray columns; take the
        // first whitespace-delimited token, as `awk '{print $1}'` did.
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| seen.insert(name.to_string()))
        .map(TableSelection::with_data)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::TableMode;

    fn plan(selections: Vec<TableSelection>) -> SyncPlan {
        SyncPlan {
            id: Uuid::new_v4(),
            profile_id: Uuid::new_v4(),
            name: "nightly".into(),
            database: "app".into(),
            selections,
            masking: Vec::new(),
            revision: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn partitions_selections_by_mode() {
        let p = plan(vec![
            TableSelection::with_data("orders"),
            TableSelection::with_data("users"),
            TableSelection::schema_only("audit_log"),
            TableSelection {
                name: "temp".into(),
                mode: TableMode::Exclude,
                where_filter: None,
            },
        ]);

        assert_eq!(p.tables_with_data(), vec!["orders", "users"]);
        assert_eq!(p.schema_only_tables(), vec!["audit_log"]);
    }

    #[test]
    fn reports_tables_that_have_disappeared() {
        let p = plan(vec![
            TableSelection::with_data("orders"),
            TableSelection::schema_only("audit_log"),
        ]);

        let available = vec!["orders".to_string()];
        assert_eq!(p.missing_from(&available), vec!["audit_log"]);
    }

    #[test]
    fn excluded_tables_are_not_reported_as_missing() {
        // The user already said they do not want it; its absence is not news.
        let p = plan(vec![TableSelection {
            name: "temp".into(),
            mode: TableMode::Exclude,
            where_filter: None,
        }]);
        assert!(p.missing_from(&[]).is_empty());
    }

    #[test]
    fn reports_new_tables_the_plan_does_not_mention() {
        let p = plan(vec![TableSelection::with_data("orders")]);
        let available = vec!["orders".to_string(), "invoices".to_string()];
        assert_eq!(p.unlisted_in(&available), vec!["invoices"]);
    }

    #[test]
    fn a_plan_matching_the_schema_reports_no_drift() {
        let p = plan(vec![
            TableSelection::with_data("orders"),
            TableSelection::schema_only("audit_log"),
        ]);
        let available = vec!["orders".to_string(), "audit_log".to_string()];
        assert!(p.missing_from(&available).is_empty());
        assert!(p.unlisted_in(&available).is_empty());
    }

    // ── Legacy tables.conf import ───────────────────────────────────────

    #[test]
    fn imports_a_legacy_tables_conf() {
        let conf = "\
# Orders
orders
order_items

# Users
users
";
        let selections = parse_tables_conf(conf);
        let names: Vec<&str> = selections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["orders", "order_items", "users"]);
        assert!(
            selections
                .iter()
                .all(|s| s.mode == TableMode::SchemaAndData),
            "everything listed in tables.conf carried data"
        );
    }

    #[test]
    fn import_skips_comments_and_blank_lines() {
        assert!(parse_tables_conf("# only a comment\n\n   \n").is_empty());
    }

    #[test]
    fn import_deduplicates() {
        // The Bash tool warned and skipped duplicates; so do we.
        let selections = parse_tables_conf("orders\nusers\norders\n");
        assert_eq!(selections.len(), 2);
    }

    #[test]
    fn import_takes_the_first_token_on_a_line() {
        // The old loader ran the file through `awk '{print $1}'`.
        let selections = parse_tables_conf("orders   # the orders table\n");
        assert_eq!(selections[0].name, "orders");
    }

    #[test]
    fn import_tolerates_indentation() {
        let selections = parse_tables_conf("   orders\n\t users\n");
        assert_eq!(selections.len(), 2);
        assert_eq!(selections[0].name, "orders");
        assert_eq!(selections[1].name, "users");
    }
}
