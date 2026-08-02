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
    /// [`expand_selections`] gives these `SchemaAndData`, so a new table is
    /// captured rather than missed — but silently changing what a nightly
    /// backup contains is worth saying out loud.
    pub fn unlisted_in(&self, available: &[String]) -> Vec<String> {
        available
            .iter()
            .filter(|name| !self.selections.iter().any(|s| &&s.name == name))
            .cloned()
            .collect()
    }

    /// This plan as a full selection list for the tables that exist today.
    pub fn expand_for(&self, available: &[String]) -> Vec<TableSelection> {
        expand_selections(&self.selections, available)
    }
}

/// Fill in the tables a selection list does not mention.
///
/// A saved set names *exceptions* — the handful of tables to skip or take
/// structure-only — so anything it stays silent about carries data. The
/// direction is deliberate: guessing wrong has to produce a backup that is too
/// large, never one that is quietly missing a table somebody added last week.
///
/// It also removes a difference between the engines that a selection list
/// could not previously express. An unmentioned table reaches `mysqldump` as
/// structure-without-rows (its schema pass names only the database) but reaches
/// `pg_dump` with all of its rows (its flags only ever *exclude*). Expanding
/// first means the same set describes the same backup on either.
///
/// Tables the set lists that the source no longer has are dropped. They cannot
/// be dumped, and naming one to `mysqldump` fails the whole job rather than
/// that one table; [`SyncPlan::missing_from`] is how a caller reports them.
///
/// Idempotent for a list that already covers every table, which is what the
/// desktop app and the CLI both send — they build their selections from a live
/// introspection, so expansion finds nothing to add.
pub fn expand_selections(
    selections: &[TableSelection],
    available: &[String],
) -> Vec<TableSelection> {
    available
        .iter()
        .map(|name| {
            selections
                .iter()
                .find(|s| names_same_table(&s.name, name))
                .cloned()
                .unwrap_or_else(|| TableSelection::with_data(name.as_str()))
        })
        .collect()
}

/// Whether a saved entry names the same table an introspection just reported.
///
/// A set imported from a legacy `tables.conf` holds bare names, while
/// PostgreSQL introspection qualifies every table with its schema. Compared
/// naively, a saved `Exclude` on `orders` would not match an available
/// `public.orders`, so the table would fall through to the unlisted branch and
/// be re-included *with its data* — the exact opposite of what the set says,
/// and silently.
///
/// Only `public` is matched. It is the one schema an unqualified name is
/// understood to mean, and matching any schema would let a set aimed at
/// `public.orders` quietly govern `archive.orders` too.
fn names_same_table(saved: &str, available: &str) -> bool {
    saved == available || available.strip_prefix("public.") == Some(saved)
}

/// Parse a legacy `tables.conf` into the selections it names.
///
/// One table per line, `#` comments, blanks ignored — the format the Bash tool
/// used. Everything listed carries data.
///
/// This is only half of what the file means. The other half — everything it
/// does *not* name is structure-without-rows — cannot be expressed by a list of
/// the names it does mention, because [`expand_selections`] reads an unmentioned
/// table as schema+data. Use [`selections_from_tables_conf`], which needs the
/// source's table list and states the rest explicitly.
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

/// A legacy `tables.conf`, completed against the tables the source actually has.
///
/// # Why the table list is required
///
/// The file names the tables that carry data and says nothing about the rest,
/// because the old script's default was structure-without-rows. A set says the
/// opposite: [`expand_selections`] gives an unmentioned table its data, which is
/// the right default for a set built in the editor — a table added last week
/// should land in tonight's backup — and the exact inverse of what this file
/// means. Storing only the listed names would therefore turn a 236-table
/// selection into "every table, with data", and it would do it silently: the
/// backup succeeds, the artifact is valid, and it is simply far larger than the
/// file asked for.
///
/// So the rest are named as [`TableMode::SchemaOnly`] rather than left out. That
/// makes the stored set explicit and self-describing, and matches what the
/// desktop editor already saves.
///
/// Built over `available` rather than over the file, so it inherits the two
/// properties expansion has: names are spelled the way an introspection spells
/// them, and a table the file lists that the source no longer has is dropped
/// rather than handed to `mysqldump`, which would fail the whole job over it.
/// [`SyncPlan::missing_from`] is how a caller reports those.
///
/// `available` must be the *whole* table list. Passing a partial one silently
/// narrows the set to it.
pub fn selections_from_tables_conf(contents: &str, available: &[String]) -> Vec<TableSelection> {
    let listed = parse_tables_conf(contents);
    available
        .iter()
        .map(|name| {
            if listed.iter().any(|s| names_same_table(&s.name, name)) {
                TableSelection::with_data(name.as_str())
            } else {
                TableSelection::schema_only(name.as_str())
            }
        })
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

    fn names(selections: &[TableSelection]) -> Vec<&str> {
        selections.iter().map(|s| s.name.as_str()).collect()
    }

    fn mode_of<'a>(selections: &'a [TableSelection], name: &str) -> &'a TableMode {
        &selections
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} is missing from the expansion"))
            .mode
    }

    #[test]
    fn a_table_the_set_never_mentions_carries_data() {
        // The whole point: a table added by last week's migration must land in
        // tonight's backup with its rows, not be silently skipped or reduced
        // to a bare CREATE TABLE.
        let p = plan(vec![TableSelection::schema_only("audit_log")]);
        let available = vec!["audit_log".to_string(), "invoices".to_string()];

        let expanded = p.expand_for(&available);

        assert_eq!(mode_of(&expanded, "invoices"), &TableMode::SchemaAndData);
        assert_eq!(mode_of(&expanded, "audit_log"), &TableMode::SchemaOnly);
    }

    #[test]
    fn listed_modes_and_row_filters_survive_expansion() {
        let p = plan(vec![
            TableSelection {
                name: "orders".into(),
                mode: TableMode::SchemaAndData,
                where_filter: Some("created_at > '2026-01-01'".into()),
            },
            TableSelection {
                name: "temp".into(),
                mode: TableMode::Exclude,
                where_filter: None,
            },
        ]);
        let available = vec!["orders".to_string(), "temp".to_string()];

        let expanded = p.expand_for(&available);

        assert_eq!(mode_of(&expanded, "temp"), &TableMode::Exclude);
        assert_eq!(
            expanded
                .iter()
                .find(|s| s.name == "orders")
                .unwrap()
                .where_filter
                .as_deref(),
            Some("created_at > '2026-01-01'"),
            "a row filter is part of the selection and must not be dropped"
        );
    }

    #[test]
    fn a_listed_table_the_source_no_longer_has_is_dropped() {
        // Naming a vanished table to mysqldump fails the entire job, not just
        // that table. `missing_from` is what reports it instead.
        let p = plan(vec![
            TableSelection::with_data("orders"),
            TableSelection::with_data("gone"),
        ]);
        let available = vec!["orders".to_string()];

        assert_eq!(names(&p.expand_for(&available)), vec!["orders"]);
        assert_eq!(p.missing_from(&available), vec!["gone"]);
    }

    #[test]
    fn a_bare_saved_name_still_matches_a_public_schema_table() {
        // The silent un-exclude: a set imported from a legacy tables.conf holds
        // `orders`, PostgreSQL introspection reports `public.orders`. Matched
        // naively they are different tables, so the exclusion is lost and the
        // table comes back with all of its rows.
        let p = plan(vec![TableSelection {
            name: "orders".into(),
            mode: TableMode::Exclude,
            where_filter: None,
        }]);

        let expanded = p.expand_for(&["public.orders".to_string()]);

        assert_eq!(expanded.len(), 1, "must not become two entries");
        assert_eq!(
            expanded[0].mode,
            TableMode::Exclude,
            "the exclusion must survive schema qualification"
        );
    }

    #[test]
    fn a_bare_name_does_not_capture_another_schema() {
        // `orders` means `public.orders`. Letting it govern `archive.orders`
        // would apply a set to a table nobody aimed it at.
        let p = plan(vec![TableSelection::schema_only("orders")]);

        let expanded = p.expand_for(&["archive.orders".to_string()]);

        assert_eq!(expanded[0].mode, TableMode::SchemaAndData);
    }

    #[test]
    fn expansion_is_idempotent_for_a_list_that_covers_everything() {
        // The desktop app and the CLI both build selections from a live
        // introspection, so expanding what they send must change nothing.
        let full = vec![
            TableSelection::with_data("orders"),
            TableSelection::schema_only("audit_log"),
        ];
        let available = vec!["orders".to_string(), "audit_log".to_string()];

        let once = expand_selections(&full, &available);
        assert_eq!(once, full);
        assert_eq!(expand_selections(&once, &available), once);
    }

    #[test]
    fn an_empty_set_takes_everything_and_an_empty_source_takes_nothing() {
        let available = vec!["orders".to_string(), "users".to_string()];
        let from_nothing = expand_selections(&[], &available);
        assert_eq!(names(&from_nothing), vec!["orders", "users"]);
        assert!(
            from_nothing
                .iter()
                .all(|s| s.mode == TableMode::SchemaAndData)
        );

        let p = plan(vec![TableSelection::with_data("orders")]);
        assert!(p.expand_for(&[]).is_empty());
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

    fn modes(selections: &[TableSelection]) -> Vec<(&str, TableMode)> {
        selections
            .iter()
            .map(|s| (s.name.as_str(), s.mode))
            .collect()
    }

    #[test]
    fn a_completed_conf_says_schema_only_for_what_the_file_omits() {
        // The whole point. Left implicit, `expand_selections` would give
        // `audit_log` its data, and the import would mean the opposite of the
        // file it came from.
        let available = vec![
            "orders".to_string(),
            "audit_log".to_string(),
            "sessions".to_string(),
        ];
        let out = selections_from_tables_conf("orders\n", &available);

        assert_eq!(
            modes(&out),
            vec![
                ("orders", TableMode::SchemaAndData),
                ("audit_log", TableMode::SchemaOnly),
                ("sessions", TableMode::SchemaOnly),
            ]
        );
    }

    #[test]
    fn a_completed_conf_survives_expansion_unchanged() {
        // It covers every table, so the run-time completion has nothing to add
        // and cannot promote a schema-only table to schema+data.
        let available = vec!["orders".to_string(), "audit_log".to_string()];
        let out = selections_from_tables_conf("orders\n", &available);

        assert_eq!(expand_selections(&out, &available), out);
    }

    #[test]
    fn a_completed_conf_drops_a_table_the_source_no_longer_has() {
        // Naming a dropped table to `mysqldump` fails the whole job, not just
        // that table.
        let available = vec!["orders".to_string()];
        let out = selections_from_tables_conf("orders\nsalla_user_mappings\n", &available);

        assert_eq!(modes(&out), vec![("orders", TableMode::SchemaAndData)]);
    }

    #[test]
    fn a_completed_conf_reads_a_bare_name_as_the_public_schema() {
        // The file holds bare names; PostgreSQL introspection qualifies them.
        // Compared naively, `orders` would miss `public.orders` and the table
        // the file asked for would come back empty.
        let available = vec!["public.orders".to_string(), "archive.orders".to_string()];
        let out = selections_from_tables_conf("orders\n", &available);

        assert_eq!(
            modes(&out),
            vec![
                ("public.orders", TableMode::SchemaAndData),
                ("archive.orders", TableMode::SchemaOnly),
            ]
        );
    }

    #[test]
    fn a_completed_conf_is_empty_when_the_source_has_no_tables() {
        assert!(selections_from_tables_conf("orders\n", &[]).is_empty());
    }
}
