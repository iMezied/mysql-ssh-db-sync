//! Column-level data masking.
//!
//! # Where masking happens, and what that costs
//!
//! Masking runs **on the destination, after the restore**. It does not happen
//! during the dump.
//!
//! That is a deliberate trade, and the reason is worth stating plainly:
//! `mysqldump`, `pg_dump` and `mongodump` have no way to apply an expression to
//! a column. Masking inside the dump would mean writing our own dump encoder —
//! hand-rolling the literal encoding for every column type in every engine —
//! and a bug in that encoder does not produce a masking failure, it produces a
//! corrupted database that restores cleanly. Acting on the destination that
//! already holds the data is the option where the engine, not this crate, is
//! responsible for type fidelity.
//!
//! For the relational engines the work *is* SQL the destination executes on
//! itself. MongoDB is the exception and [`mongo`] says why: its aggregation
//! language has no general-purpose hash, so the hashing transforms are computed
//! here and written back. The guarantee below is identical either way, which is
//! what makes the difference an implementation detail rather than a caveat.
//!
//! The consequence, which must never be soft-pedalled anywhere in this app:
//!
//! > **The artifact still contains the real data.** Masking protects the
//! > destination, not the backup file.
//!
//! An artifact taken from a masked sync is exactly as sensitive as the source.
//! Encrypt it ([`crate::crypto`]), keep it where the source's data would be
//! allowed to live, and do not hand it to anyone who is only cleared to see the
//! masked copy.
//!
//! # The guarantee that makes this safe to rely on
//!
//! Either the destination holds masked data, or it holds nothing.
//!
//! Every masking run is followed by a check that reads the destination back and
//! counts rows that do not have the masked shape. If the masking statements
//! fail, or the check finds unmasked rows, or the check itself cannot run, the
//! caller drops the destination database. A half-masked database is the worst
//! possible outcome — it looks finished, and someone believes it — so it is
//! never left in place.
//!
//! # Determinism, and why it is not anonymisation
//!
//! [`MaskTransform::Hash`], [`MaskTransform::Email`] and [`MaskTransform::Phone`]
//! are deterministic: the same input always produces the same output, in every
//! table, on every run. That is what keeps a masked copy usable — `users.email`
//! and `orders.billing_email` still join, and a dev database refreshed weekly
//! keeps stable pseudonyms.
//!
//! Determinism has a price, and it is a real one. This is **pseudonymisation,
//! not anonymisation**. Anyone holding both the masked data and the salt can
//! confirm a guess: hash `alice@example.com`, look for the result. Emails,
//! phone numbers and names are all small, guessable domains.
//!
//! The salt is what stands between those two situations, so where it lives
//! matters: it is stored in the operator's local app database and is never
//! written to the destination. Someone who compromises the dev server gets
//! pseudonyms without the salt, and cannot run that attack. Someone who
//! compromises the operator's machine already has the production credentials.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use specta::Type;

use crate::db::{DbError, Statement, quote_mysql_ident, quote_pg_ident};
use crate::types::Engine;

pub mod mongo;

/// The engines that speak SQL.
///
/// This exists so that a document store cannot reach the statement builders at
/// all. Every function below that composes SQL takes a `SqlDialect` rather than
/// an [`Engine`], so "generate an `UPDATE` for MongoDB" is not a case that has
/// to be remembered and handled — it is a sentence that does not typecheck.
/// The conversion happens once, at the two public entry points, and MongoDB is
/// routed to [`mongo`] before it gets there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqlDialect {
    Mysql,
    Postgres,
}

impl SqlDialect {
    fn of(engine: Engine) -> Result<Self, MaskError> {
        match engine {
            Engine::Mysql => Ok(SqlDialect::Mysql),
            Engine::Postgres => Ok(SqlDialect::Postgres),
            Engine::Mongo => Err(MaskError::Db(DbError::NotSql(Engine::Mongo))),
        }
    }
}

/// Where the masking salt lives in `app_settings`.
pub const SALT_SETTING: &str = "masking.salt";

/// Reserved by RFC 2606 precisely so it can never resolve, so a masked address
/// cannot be delivered to. `example.com` is a real domain that accepts mail;
/// using it would turn a masking mistake into someone else's inbox.
pub const FAKE_EMAIL_DOMAIN: &str = "example.invalid";

/// The 555 range is reserved for fiction in NANP, so a masked number cannot
/// ring a real phone.
pub const FAKE_PHONE_PREFIX: &str = "+1555";

/// Full SHA-256 in hex. Chosen over something shorter because a column too
/// narrow to hold it fails the post-mask check loudly, whereas a short hash
/// quietly raises the collision rate.
pub const DEFAULT_HASH_LENGTH: u16 = 64;

/// What to replace a column's values with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum MaskTransform {
    /// Salted SHA-256, hex, optionally truncated. NULL stays NULL.
    Hash { length: Option<u16> },
    /// A deterministic address at [`FAKE_EMAIL_DOMAIN`]. NULL stays NULL.
    Email,
    /// A deterministic number in the reserved 555 range. NULL stays NULL.
    Phone,
    /// Every row set to NULL. Fails loudly on a NOT NULL column.
    Null,
    /// Every row set to one literal, NULLs included.
    ///
    /// Bound as text, so this is for text-ish columns. On a numeric column
    /// PostgreSQL rejects it outright and MySQL coerces; either way the
    /// post-mask check has the final say.
    Constant { value: String },
}

impl MaskTransform {
    /// Whether the transform maps equal inputs to equal outputs across tables.
    pub const fn is_deterministic(&self) -> bool {
        matches!(
            self,
            MaskTransform::Hash { .. } | MaskTransform::Email | MaskTransform::Phone
        )
    }

    /// Whether NULL survives the transform unchanged.
    pub const fn preserves_null(&self) -> bool {
        matches!(
            self,
            MaskTransform::Hash { .. } | MaskTransform::Email | MaskTransform::Phone
        )
    }

    fn hash_length(&self) -> u16 {
        match self {
            MaskTransform::Hash { length } => length.unwrap_or(DEFAULT_HASH_LENGTH),
            _ => DEFAULT_HASH_LENGTH,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            MaskTransform::Hash { length } => match length {
                Some(n) => format!("salted SHA-256, first {n} hex characters"),
                None => "salted SHA-256".to_string(),
            },
            MaskTransform::Email => format!("deterministic address at {FAKE_EMAIL_DOMAIN}"),
            MaskTransform::Phone => format!("deterministic number under {FAKE_PHONE_PREFIX}"),
            MaskTransform::Null => "set to NULL".to_string(),
            MaskTransform::Constant { value } => format!("set to {value:?}"),
        }
    }
}

/// One column, one transform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MaskRule {
    /// Table name as the plan spells it. For PostgreSQL this may be
    /// `schema.table`; a bare name means `public`.
    pub table: String,
    pub column: String,
    pub transform: MaskTransform,
}

impl MaskRule {
    pub fn hash(table: impl Into<String>, column: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            column: column.into(),
            transform: MaskTransform::Hash { length: None },
        }
    }

    pub fn email(table: impl Into<String>, column: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            column: column.into(),
            transform: MaskTransform::Email,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MaskError {
    #[error(
        "masking rule for {table}.{column} names a column that does not exist; \
         {table} has: {available}"
    )]
    UnknownColumn {
        table: String,
        column: String,
        available: String,
    },
    #[error(
        "masking rule for {table}.{column} names a table that is not being copied with data, \
         so nothing would be masked"
    )]
    UnknownTable { table: String, column: String },
    #[error("two masking rules both target {table}.{column}")]
    DuplicateRule { table: String, column: String },
    #[error(
        "masking needs to own the destination database so it can drop it if masking fails, \
         and {naming} restores into a database that already existed"
    )]
    UnsafeNaming { naming: String },
    #[error("{count} row(s) in {table}.{column} are not masked")]
    NotMasked {
        table: String,
        column: String,
        count: i64,
    },
    #[error(transparent)]
    Db(#[from] DbError),
}

// ── Salt ────────────────────────────────────────────────────────────────

/// Derive the salt actually mixed into hashes.
///
/// The stored secret is never used directly. Hashing it with a fixed label
/// means the value interpolated into SQL — and therefore visible in the
/// destination server's query log — is a one-way function of the secret rather
/// than the secret itself. Query logs are read by more people than the app
/// database is.
pub fn derive_salt(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dbsync/masking/v1\0");
    hasher.update(secret.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Fetch the installation's masking secret, generating one on first use.
///
/// Stable by design: regenerating it would change every pseudonym, so a
/// destination refreshed on Tuesday would no longer line up with the copy
/// someone took on Monday.
pub async fn ensure_secret(
    store: &crate::store::Store,
) -> Result<String, crate::store::StoreError> {
    if let Some(existing) = store.get_setting(SALT_SETTING).await?
        && !existing.trim().is_empty()
    {
        return Ok(existing);
    }

    // Two v4 UUIDs is 244 bits from the OS CSPRNG, which is plenty, and avoids
    // taking a dependency on a second RNG for one value.
    let secret = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    store.set_setting(SALT_SETTING, &secret).await?;
    Ok(secret)
}

// ── Planning ────────────────────────────────────────────────────────────

/// A rule that will run, and one that will not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MaskingCoverage {
    pub effective: Vec<MaskRule>,
    /// Rules whose table carries no data into the destination, with the reason.
    /// Harmless — there is nothing there to leak — but worth surfacing, because
    /// it usually means the plan and the rules have drifted apart.
    pub inert: Vec<InertRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct InertRule {
    pub rule: MaskRule,
    pub reason: String,
}

impl MaskingCoverage {
    pub fn tables(&self) -> Vec<String> {
        let unique: BTreeSet<String> = self.effective.iter().map(|r| r.table.clone()).collect();
        unique.into_iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.effective.is_empty()
    }
}

/// Work out which rules will do something, and reject the ones that silently
/// would not.
///
/// This runs against the **source** schema, before the backup starts, because
/// the failure it catches is the one that matters most: a rule naming
/// `users.email` when the column is really `email_address` protects nothing,
/// and without this check the operator finds out by reading real addresses out
/// of the dev database.
///
/// A rule on a table that is not copied with data is *not* an error — nothing
/// reaches the destination, so nothing is exposed — but it is reported.
///
/// # Nested fields
///
/// MongoDB rules may address a field inside a subdocument by dotted path, and
/// [`crate::db::Introspector::column_names`] only reports top-level fields. So
/// for a document store the check is against the **root** of the path: a rule
/// on `profile.email` is covered when the collection has a `profile` field. It
/// is a weaker check than the relational one, and deliberately not stronger —
/// guessing at the shape below the root would start rejecting rules that work.
/// The read-back in [`check_statements`] is what actually proves the masking
/// took, and it does look at the exact path.
pub fn plan_coverage(
    engine: Engine,
    rules: &[MaskRule],
    tables_with_data: &[String],
    source_columns: &BTreeMap<String, Vec<String>>,
) -> Result<MaskingCoverage, MaskError> {
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut effective = Vec::new();
    let mut inert = Vec::new();

    for rule in rules {
        let key = (rule.table.clone(), rule.column.clone());
        if !seen.insert(key) {
            return Err(MaskError::DuplicateRule {
                table: rule.table.clone(),
                column: rule.column.clone(),
            });
        }

        if !tables_with_data.iter().any(|t| t == &rule.table) {
            inert.push(InertRule {
                rule: rule.clone(),
                reason: format!(
                    "{} is not in this plan as a table carrying data, so no rows reach the \
                     destination to mask",
                    rule.table
                ),
            });
            continue;
        }

        // A table we cannot introspect is not evidence the column is fine. It
        // is reported as unknown so the run stops rather than proceeding on an
        // assumption.
        let columns = source_columns
            .get(&rule.table)
            .ok_or_else(|| MaskError::UnknownTable {
                table: rule.table.clone(),
                column: rule.column.clone(),
            })?;

        // A dotted path names a field inside a subdocument, so only its root
        // can be checked against the field list. Relational engines get the
        // exact match they have always had: a MySQL column called `a.b` is a
        // column literally called `a.b`, not a path.
        let wanted = if engine.is_relational() {
            rule.column.as_str()
        } else {
            rule.column.split('.').next().unwrap_or(&rule.column)
        };

        if !columns.iter().any(|c| c == wanted) {
            return Err(MaskError::UnknownColumn {
                table: rule.table.clone(),
                column: rule.column.clone(),
                available: columns.join(", "),
            });
        }

        effective.push(rule.clone());
    }

    Ok(MaskingCoverage { effective, inert })
}

// ── SQL generation ──────────────────────────────────────────────────────

/// Quote a table name, splitting `schema.table` for PostgreSQL.
fn quote_table(dialect: SqlDialect, name: &str) -> Result<String, DbError> {
    match dialect {
        SqlDialect::Mysql => quote_mysql_ident(name),
        SqlDialect::Postgres => match name.split_once('.') {
            Some((schema, table)) => Ok(format!(
                "{}.{}",
                quote_pg_ident(schema)?,
                quote_pg_ident(table)?
            )),
            None => Ok(format!(
                "{}.{}",
                quote_pg_ident("public")?,
                quote_pg_ident(name)?
            )),
        },
    }
}

fn quote_column(dialect: SqlDialect, name: &str) -> Result<String, DbError> {
    match dialect {
        SqlDialect::Mysql => quote_mysql_ident(name),
        SqlDialect::Postgres => quote_pg_ident(name),
    }
}

/// Render a column as text, so a check works whatever the column's type is.
fn as_text(dialect: SqlDialect, quoted: &str) -> String {
    match dialect {
        SqlDialect::Mysql => format!("CAST({quoted} AS CHAR)"),
        SqlDialect::Postgres => format!("{quoted}::text"),
    }
}

/// Placeholders differ, so statements are built with a counter rather than a
/// fixed string.
struct Placeholders {
    dialect: SqlDialect,
    next: usize,
}

impl Placeholders {
    fn new(dialect: SqlDialect) -> Self {
        Self { dialect, next: 1 }
    }

    fn take(&mut self) -> String {
        let n = self.next;
        self.next += 1;
        match self.dialect {
            SqlDialect::Mysql => "?".to_string(),
            SqlDialect::Postgres => format!("${n}"),
        }
    }
}

/// The salted digest of a column, as hex, in the destination's own SQL.
fn digest_expr(dialect: SqlDialect, quoted_col: &str, salt: &mut impl FnMut() -> String) -> String {
    let p = salt();
    match dialect {
        // CONCAT returns NULL if any argument is NULL, which is exactly the
        // NULL-preserving behaviour we want and gets it for free.
        SqlDialect::Mysql => format!("SHA2(CONCAT({p}, {quoted_col}), 256)"),
        // `||` is NULL-propagating for the same reason.
        SqlDialect::Postgres => {
            format!("encode(sha256(convert_to({p} || {quoted_col}::text, 'UTF8')), 'hex')")
        }
    }
}

/// The `SET column = ...` expression for one rule.
fn set_expr(
    dialect: SqlDialect,
    quoted_col: &str,
    transform: &MaskTransform,
    binds: &mut Vec<String>,
    ph: &mut Placeholders,
    salt: &str,
) -> String {
    let mut salted = || {
        binds.push(salt.to_string());
        ph.take()
    };

    match transform {
        MaskTransform::Hash { .. } => {
            let digest = digest_expr(dialect, quoted_col, &mut salted);
            let len = transform.hash_length();
            match dialect {
                SqlDialect::Mysql => format!("LEFT({digest}, {len})"),
                SqlDialect::Postgres => format!("left({digest}, {len})"),
            }
        }
        MaskTransform::Email => {
            let digest = digest_expr(dialect, quoted_col, &mut salted);
            match dialect {
                SqlDialect::Mysql => {
                    format!("CONCAT(LEFT({digest}, 16), '@{FAKE_EMAIL_DOMAIN}')")
                }
                SqlDialect::Postgres => {
                    format!("left({digest}, 16) || '@{FAKE_EMAIL_DOMAIN}'")
                }
            }
        }
        MaskTransform::Phone => {
            let digest = digest_expr(dialect, quoted_col, &mut salted);
            match dialect {
                // 7 hex characters is 28 bits; the modulo bias across 10^7 is
                // irrelevant for a number that only has to look like one.
                SqlDialect::Mysql => format!(
                    "CONCAT('{FAKE_PHONE_PREFIX}', \
                     LPAD(CONV(LEFT({digest}, 7), 16, 10) % 10000000, 7, '0'))"
                ),
                SqlDialect::Postgres => format!(
                    "'{FAKE_PHONE_PREFIX}' || \
                     lpad(((('x' || left({digest}, 7))::bit(28)::bigint) % 10000000)::text, 7, '0')"
                ),
            }
        }
        MaskTransform::Null => "NULL".to_string(),
        MaskTransform::Constant { value } => {
            binds.push(value.clone());
            ph.take()
        }
    }
}

/// One `UPDATE` per table, touching every masked column in a single pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableStatement {
    pub table: String,
    pub statement: Statement,
}

/// Build the statements that do the masking.
///
/// Grouped by table so a table is scanned once however many of its columns are
/// masked, and ordered by table name so a run is reproducible.
pub fn update_statements(
    engine: Engine,
    rules: &[MaskRule],
    salt: &str,
) -> Result<Vec<TableStatement>, MaskError> {
    let dialect = SqlDialect::of(engine)?;
    let mut by_table: BTreeMap<&str, Vec<&MaskRule>> = BTreeMap::new();
    for rule in rules {
        by_table.entry(rule.table.as_str()).or_default().push(rule);
    }

    let mut out = Vec::new();
    for (table, rules) in by_table {
        let quoted_table = quote_table(dialect, table)?;
        let mut ph = Placeholders::new(dialect);
        let mut binds = Vec::new();
        let mut assignments = Vec::new();

        for rule in rules {
            let quoted_col = quote_column(dialect, &rule.column)?;
            let expr = set_expr(
                dialect,
                &quoted_col,
                &rule.transform,
                &mut binds,
                &mut ph,
                salt,
            );
            assignments.push(format!("{quoted_col} = {expr}"));
        }

        out.push(TableStatement {
            table: table.to_string(),
            statement: Statement {
                sql: format!("UPDATE {quoted_table} SET {}", assignments.join(", ")),
                binds,
            },
        });
    }

    Ok(out)
}

/// A predicate matching rows that do **not** have the masked shape.
fn violation_expr(
    dialect: SqlDialect,
    quoted_col: &str,
    transform: &MaskTransform,
    binds: &mut Vec<String>,
    ph: &mut Placeholders,
) -> String {
    let text = as_text(dialect, quoted_col);

    match transform {
        MaskTransform::Hash { .. } => {
            let len = transform.hash_length();
            match dialect {
                SqlDialect::Mysql => format!(
                    "{quoted_col} IS NOT NULL AND \
                     (CHAR_LENGTH({text}) <> {len} OR {text} NOT REGEXP '^[0-9a-f]+$')"
                ),
                SqlDialect::Postgres => format!(
                    "{quoted_col} IS NOT NULL AND \
                     (length({text}) <> {len} OR {text} !~ '^[0-9a-f]+$')"
                ),
            }
        }
        MaskTransform::Email => {
            format!("{quoted_col} IS NOT NULL AND {text} NOT LIKE '%@{FAKE_EMAIL_DOMAIN}'")
        }
        MaskTransform::Phone => {
            format!("{quoted_col} IS NOT NULL AND {text} NOT LIKE '{FAKE_PHONE_PREFIX}%'")
        }
        MaskTransform::Null => format!("{quoted_col} IS NOT NULL"),
        MaskTransform::Constant { value } => {
            // A constant overwrites NULLs too, so a surviving NULL is a miss.
            binds.push(value.clone());
            let p = ph.take();
            format!("{quoted_col} IS NULL OR {text} <> {p}")
        }
    }
}

/// A read-back that counts unmasked rows, one query per table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCheck {
    pub table: String,
    pub statement: Statement,
    /// Columns in the order their counts come back.
    pub columns: Vec<String>,
    pub transforms: Vec<MaskTransform>,
}

/// Build the statements that prove the masking took.
///
/// This is the difference between believing a column is masked and knowing it.
/// The masking `UPDATE` can report success and still leave the column readable
/// — a silent truncation on a too-narrow column, a trigger that rewrites the
/// row, a type coercion that throws the expression away — and none of those
/// surface as an error.
pub fn check_statements(engine: Engine, rules: &[MaskRule]) -> Result<Vec<TableCheck>, MaskError> {
    let dialect = SqlDialect::of(engine)?;
    let mut by_table: BTreeMap<&str, Vec<&MaskRule>> = BTreeMap::new();
    for rule in rules {
        by_table.entry(rule.table.as_str()).or_default().push(rule);
    }

    let mut out = Vec::new();
    for (table, rules) in by_table {
        let quoted_table = quote_table(dialect, table)?;
        let mut ph = Placeholders::new(dialect);
        let mut binds = Vec::new();
        let mut projections = Vec::new();
        let mut columns = Vec::new();
        let mut transforms = Vec::new();

        for rule in rules {
            let quoted_col = quote_column(dialect, &rule.column)?;
            let violation =
                violation_expr(dialect, &quoted_col, &rule.transform, &mut binds, &mut ph);
            // SUM over an empty table is NULL, and MySQL widens it to DECIMAL;
            // both are coerced here so the caller can read a plain integer.
            projections.push(match dialect {
                SqlDialect::Mysql => format!(
                    "CAST(COALESCE(SUM(CASE WHEN {violation} THEN 1 ELSE 0 END), 0) AS SIGNED)"
                ),
                SqlDialect::Postgres => {
                    format!("COALESCE(SUM(CASE WHEN {violation} THEN 1 ELSE 0 END), 0)::bigint")
                }
            });
            columns.push(rule.column.clone());
            transforms.push(rule.transform.clone());
        }

        out.push(TableCheck {
            table: table.to_string(),
            statement: Statement {
                sql: format!("SELECT {} FROM {quoted_table}", projections.join(", ")),
                binds,
            },
            columns,
            transforms,
        });
    }

    Ok(out)
}

/// What a completed masking run did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MaskingReport {
    /// Tables touched, in the order they were masked.
    pub tables: Vec<String>,
    /// Columns masked, as `table.column`.
    pub columns: Vec<String>,
    /// Rows rewritten, as the destination reported them.
    #[specta(type = f64)]
    pub rows_rewritten: u64,
    /// Rules that could not apply to anything.
    pub inert: Vec<InertRule>,
    /// Always false in a report that is returned; a failed check aborts.
    pub verified: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(t, cols)| {
                (
                    t.to_string(),
                    cols.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    // ── Coverage ────────────────────────────────────────────────────────

    #[test]
    fn a_rule_naming_a_missing_column_is_refused() {
        // The whole point of the check: `email` when the column is
        // `email_address` protects nothing, and nothing else would notice.
        let rules = vec![MaskRule::email("users", "email")];
        let err = plan_coverage(
            Engine::Mysql,
            &rules,
            &["users".to_string()],
            &columns(&[("users", &["id", "email_address"])]),
        )
        .unwrap_err();

        assert!(matches!(err, MaskError::UnknownColumn { .. }), "{err}");
        assert!(
            err.to_string().contains("email_address"),
            "the error should list what is actually there: {err}"
        );
    }

    #[test]
    fn a_rule_on_a_table_without_data_is_inert_not_an_error() {
        // Nothing reaches the destination, so nothing is exposed. Reported,
        // because it usually means the plan and the rules have drifted.
        let rules = vec![MaskRule::email("archived_users", "email")];
        let coverage = plan_coverage(
            Engine::Mysql,
            &rules,
            &["users".to_string()],
            &columns(&[("users", &["id", "email"])]),
        )
        .unwrap();

        assert!(coverage.effective.is_empty());
        assert_eq!(coverage.inert.len(), 1);
        assert!(coverage.inert[0].reason.contains("archived_users"));
    }

    #[test]
    fn a_table_that_cannot_be_introspected_stops_the_run() {
        // "We could not look" is not "the column is fine".
        let rules = vec![MaskRule::email("users", "email")];
        let err = plan_coverage(
            Engine::Mysql,
            &rules,
            &["users".to_string()],
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(matches!(err, MaskError::UnknownTable { .. }), "{err}");
    }

    #[test]
    fn duplicate_rules_are_refused() {
        // Two rules on one column means one silently wins; which one is an
        // implementation detail nobody should have to know.
        let rules = vec![
            MaskRule::email("users", "email"),
            MaskRule::hash("users", "email"),
        ];
        let err = plan_coverage(
            Engine::Mysql,
            &rules,
            &["users".to_string()],
            &columns(&[("users", &["id", "email"])]),
        )
        .unwrap_err();
        assert!(matches!(err, MaskError::DuplicateRule { .. }), "{err}");
    }

    #[test]
    fn a_matching_rule_is_effective() {
        let rules = vec![MaskRule::email("users", "email")];
        let coverage = plan_coverage(
            Engine::Mysql,
            &rules,
            &["users".to_string()],
            &columns(&[("users", &["id", "email"])]),
        )
        .unwrap();
        assert_eq!(coverage.effective, rules);
        assert!(coverage.inert.is_empty());
        assert_eq!(coverage.tables(), vec!["users"]);
    }

    // ── Salt ────────────────────────────────────────────────────────────

    #[test]
    fn the_salt_in_sql_is_not_the_stored_secret() {
        // The salt is interpolated into statements the destination logs.
        let secret = "the-stored-secret";
        let salt = derive_salt(secret);
        assert_ne!(salt, secret);
        assert!(!salt.contains(secret));
        assert_eq!(salt.len(), 64);
    }

    #[test]
    fn salt_derivation_is_stable() {
        // If this changes, every pseudonym in every destination changes with it.
        assert_eq!(derive_salt("abc"), derive_salt("abc"));
        assert_ne!(derive_salt("abc"), derive_salt("abd"));
    }

    // ── Statement generation ────────────────────────────────────────────

    #[test]
    fn one_update_per_table_however_many_columns() {
        let rules = vec![
            MaskRule::email("users", "email"),
            MaskRule::hash("users", "surname"),
            MaskRule::hash("orders", "note"),
        ];
        let stmts = update_statements(Engine::Mysql, &rules, "salt").unwrap();

        assert_eq!(stmts.len(), 2, "one statement per table, not per column");
        let users = stmts.iter().find(|s| s.table == "users").unwrap();
        assert!(users.statement.sql.contains("`email` ="));
        assert!(users.statement.sql.contains("`surname` ="));
    }

    #[test]
    fn identifiers_are_quoted_not_interpolated() {
        // A column named `a`; DROP DATABASE x;--` is legal in MySQL.
        let rules = vec![MaskRule::hash("t", "a`; DROP DATABASE x;--")];
        let stmts = update_statements(Engine::Mysql, &rules, "salt").unwrap();
        assert!(
            stmts[0].statement.sql.contains("`a``; DROP DATABASE x;--`"),
            "backtick must be doubled: {}",
            stmts[0].statement.sql
        );
    }

    #[test]
    fn a_constant_is_bound_never_interpolated() {
        let rules = vec![MaskRule {
            table: "users".into(),
            column: "note".into(),
            transform: MaskTransform::Constant {
                value: "'); DROP TABLE users;--".into(),
            },
        }];
        let stmts = update_statements(Engine::Mysql, &rules, "salt").unwrap();
        assert!(
            !stmts[0].statement.sql.contains("DROP TABLE"),
            "the value must not reach the SQL: {}",
            stmts[0].statement.sql
        );
        assert!(
            stmts[0]
                .statement
                .binds
                .contains(&"'); DROP TABLE users;--".to_string())
        );
    }

    #[test]
    fn postgres_placeholders_are_numbered_in_order() {
        let rules = vec![MaskRule::hash("users", "a"), MaskRule::hash("users", "b")];
        let stmts = update_statements(Engine::Postgres, &rules, "salt").unwrap();
        let sql = &stmts[0].statement.sql;
        assert!(sql.contains("$1"), "{sql}");
        assert!(sql.contains("$2"), "{sql}");
        assert_eq!(stmts[0].statement.binds.len(), 2);
    }

    #[test]
    fn mysql_placeholders_are_positional() {
        let rules = vec![MaskRule::hash("users", "a"), MaskRule::hash("users", "b")];
        let stmts = update_statements(Engine::Mysql, &rules, "salt").unwrap();
        assert_eq!(stmts[0].statement.sql.matches('?').count(), 2);
        assert_eq!(stmts[0].statement.binds.len(), 2);
    }

    #[test]
    fn a_bare_table_name_is_schema_qualified_for_postgres() {
        let rules = vec![MaskRule::hash("users", "a")];
        let stmts = update_statements(Engine::Postgres, &rules, "salt").unwrap();
        assert!(stmts[0].statement.sql.contains(r#""public"."users""#));
    }

    #[test]
    fn a_qualified_table_name_keeps_its_schema() {
        let rules = vec![MaskRule::hash("billing.invoices", "note")];
        let stmts = update_statements(Engine::Postgres, &rules, "salt").unwrap();
        assert!(stmts[0].statement.sql.contains(r#""billing"."invoices""#));
    }

    #[test]
    fn a_masked_email_lands_on_a_domain_that_cannot_receive_mail() {
        for engine in [Engine::Mysql, Engine::Postgres] {
            let rules = vec![MaskRule::email("users", "email")];
            let stmts = update_statements(engine, &rules, "salt").unwrap();
            assert!(
                stmts[0].statement.sql.contains("@example.invalid"),
                "{engine:?}: {}",
                stmts[0].statement.sql
            );
        }
    }

    #[test]
    fn hash_truncation_is_honoured() {
        let rules = vec![MaskRule {
            table: "users".into(),
            column: "ref".into(),
            transform: MaskTransform::Hash { length: Some(12) },
        }];
        let stmts = update_statements(Engine::Mysql, &rules, "salt").unwrap();
        assert!(stmts[0].statement.sql.contains("LEFT(SHA2"));
        assert!(stmts[0].statement.sql.contains(", 12)"));
    }

    // ── Checks ──────────────────────────────────────────────────────────

    #[test]
    fn checks_count_rows_that_are_not_masked() {
        let rules = vec![MaskRule::email("users", "email")];
        let checks = check_statements(Engine::Mysql, &rules).unwrap();

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].columns, vec!["email"]);
        let sql = &checks[0].statement.sql;
        assert!(sql.contains("NOT LIKE '%@example.invalid'"), "{sql}");
        assert!(sql.contains("SUM(CASE WHEN"), "{sql}");
    }

    #[test]
    fn a_null_preserving_transform_does_not_count_nulls_as_unmasked() {
        // A NULL email was never sensitive; counting it would fail every run.
        for engine in [Engine::Mysql, Engine::Postgres] {
            let rules = vec![MaskRule::email("users", "email")];
            let checks = check_statements(engine, &rules).unwrap();
            assert!(
                checks[0].statement.sql.contains("IS NOT NULL AND"),
                "{engine:?}: {}",
                checks[0].statement.sql
            );
        }
    }

    #[test]
    fn a_surviving_null_is_unmasked_for_a_constant() {
        // Constant overwrites NULLs too, so a NULL left behind means the
        // UPDATE did not reach that row.
        let rules = vec![MaskRule {
            table: "users".into(),
            column: "note".into(),
            transform: MaskTransform::Constant {
                value: "redacted".into(),
            },
        }];
        let checks = check_statements(Engine::Postgres, &rules).unwrap();
        assert!(checks[0].statement.sql.contains("IS NULL OR"));
    }

    #[test]
    fn the_null_transform_is_checked_by_absence() {
        let rules = vec![MaskRule {
            table: "users".into(),
            column: "ssn".into(),
            transform: MaskTransform::Null,
        }];
        let checks = check_statements(Engine::Mysql, &rules).unwrap();
        assert!(checks[0].statement.sql.contains("`ssn` IS NOT NULL"));
    }

    #[test]
    fn checks_cover_every_effective_rule() {
        // A rule without a check is a rule nobody is verifying.
        let rules = vec![
            MaskRule::email("users", "email"),
            MaskRule::hash("users", "surname"),
            MaskRule {
                table: "users".into(),
                column: "ssn".into(),
                transform: MaskTransform::Null,
            },
            MaskRule::hash("orders", "note"),
        ];
        let checks = check_statements(Engine::Mysql, &rules).unwrap();
        let covered: usize = checks.iter().map(|c| c.columns.len()).sum();
        assert_eq!(covered, rules.len());
    }

    #[test]
    fn check_binds_line_up_with_placeholders() {
        let rules = vec![
            MaskRule {
                table: "users".into(),
                column: "a".into(),
                transform: MaskTransform::Constant { value: "x".into() },
            },
            MaskRule {
                table: "users".into(),
                column: "b".into(),
                transform: MaskTransform::Constant { value: "y".into() },
            },
        ];
        let checks = check_statements(Engine::Postgres, &rules).unwrap();
        assert!(checks[0].statement.sql.contains("$1"));
        assert!(checks[0].statement.sql.contains("$2"));
        assert_eq!(checks[0].statement.binds, vec!["x", "y"]);
    }

    // ── Properties ──────────────────────────────────────────────────────

    #[test]
    fn deterministic_transforms_are_marked_as_such() {
        // Joins across tables only survive because of this property.
        assert!(MaskTransform::Email.is_deterministic());
        assert!(MaskTransform::Phone.is_deterministic());
        assert!(MaskTransform::Hash { length: None }.is_deterministic());
        assert!(!MaskTransform::Null.is_deterministic());
        assert!(!MaskTransform::Constant { value: "x".into() }.is_deterministic());
    }

    #[test]
    fn the_same_salt_produces_the_same_sql() {
        // Determinism starts here: identical rules must generate identical
        // statements, or the pseudonyms move between runs.
        let rules = vec![MaskRule::email("users", "email")];
        let a = update_statements(Engine::Mysql, &rules, "salt").unwrap();
        let b = update_statements(Engine::Mysql, &rules, "salt").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn statements_are_ordered_by_table_name() {
        // Reproducible runs, and readable logs.
        let rules = vec![
            MaskRule::hash("zebra", "a"),
            MaskRule::hash("alpha", "a"),
            MaskRule::hash("middle", "a"),
        ];
        let stmts = update_statements(Engine::Mysql, &rules, "salt").unwrap();
        let tables: Vec<&str> = stmts.iter().map(|s| s.table.as_str()).collect();
        assert_eq!(tables, vec!["alpha", "middle", "zebra"]);
    }
}
