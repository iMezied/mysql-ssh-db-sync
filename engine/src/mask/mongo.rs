//! Masking for MongoDB.
//!
//! # Why this is a separate implementation and not a dialect
//!
//! The relational path composes one `UPDATE … SET` per table and lets the
//! server compute every value. That works because SQL has `SHA2`/`sha256`.
//!
//! MongoDB's aggregation language has no general-purpose hash. `$toHashedIndexKey`
//! exists, but it is a 64-bit index hash in a different output space, and the
//! whole point of [`MaskTransform::Hash`] here is a salted SHA-256 that matches
//! what the SQL engines produce — so that a team masking a MySQL copy and a
//! MongoDB copy with the same salt gets the same pseudonym for the same input,
//! and the two copies still join.
//!
//! So the work splits by transform, and the split is visible in the types:
//!
//! * [`MaskTransform::Null`] and [`MaskTransform::Constant`] are one
//!   `updateMany` each. The server does all of it, nothing is read back to this
//!   process, and a collection of any size costs one round trip.
//! * [`MaskTransform::Hash`], [`MaskTransform::Email`] and
//!   [`MaskTransform::Phone`] need the value to compute the replacement, so
//!   documents are read, hashed here, and written back in batches.
//!
//! Reading the values here is not a new exposure. This application has just
//! streamed the entire source database through this process into an artifact on
//! this disk; a field value passing through memory on the way to being replaced
//! is inside a boundary that was already crossed.
//!
//! # The guarantee is unchanged
//!
//! Either the destination holds masked data or it holds nothing. The read-back
//! in [`check_filters`] counts documents that do not have the masked shape,
//! exactly as the SQL check does, and the caller drops the database if the
//! count is not zero or the check cannot run. That is what makes the split
//! above safe: whichever route a transform took, the proof is the same.

use mongodb::bson::{Bson, Document, doc};
use sha2::{Digest, Sha256};

use super::{DEFAULT_HASH_LENGTH, FAKE_EMAIL_DOMAIN, FAKE_PHONE_PREFIX, MaskRule, MaskTransform};

/// How many documents to rewrite per bulk write.
///
/// Small enough that a failure does not strand a huge in-flight batch, large
/// enough that a million-document collection is not a million round trips.
pub const REWRITE_BATCH: usize = 500;

/// The oldest MongoDB this can run against.
///
/// `$regexMatch`, used by every read-back filter, arrived in 4.2. Masking that
/// cannot be verified is masking this application will not do, so the floor is
/// the *check's* requirement rather than the update's.
pub const MIN_SERVER_MAJOR: u32 = 4;
pub const MIN_SERVER_MINOR: u32 = 2;

/// A masking step the server can perform by itself.
// No `Eq`: a BSON `Document` can hold a double, and floats have no total
// equality. `PartialEq` is what the tests need and all BSON can honestly offer.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerUpdate {
    pub collection: String,
    /// The `$set` document handed to `updateMany`, keyed by field path.
    pub set: Document,
}

/// A masking step that needs the current value to compute the new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputedUpdate {
    pub collection: String,
    /// Field path, as the rule spells it — dotted for a nested field.
    pub field: String,
    pub transform: MaskTransform,
}

/// Everything one masking run has to do, split by who computes the value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MaskPlan {
    pub server: Vec<ServerUpdate>,
    pub computed: Vec<ComputedUpdate>,
}

impl MaskPlan {
    pub fn is_empty(&self) -> bool {
        self.server.is_empty() && self.computed.is_empty()
    }
}

/// Split rules into the work the server does and the work this process does.
///
/// Grouped by collection and ordered by name for the same reason the SQL side
/// is: a run has to be reproducible, and a diff of two runs has to be readable.
pub fn plan(rules: &[MaskRule]) -> MaskPlan {
    use std::collections::BTreeMap;

    let mut by_collection: BTreeMap<&str, Vec<&MaskRule>> = BTreeMap::new();
    for rule in rules {
        by_collection
            .entry(rule.table.as_str())
            .or_default()
            .push(rule);
    }

    let mut out = MaskPlan::default();
    for (collection, rules) in by_collection {
        let mut set = Document::new();
        for rule in rules {
            match &rule.transform {
                // `null` here is a real BSON null, not a missing field. The
                // read-back counts a surviving non-null as a violation, and a
                // field that was already absent stays absent — which is the
                // same outcome, no readable value.
                MaskTransform::Null => {
                    set.insert(rule.column.clone(), Bson::Null);
                }
                MaskTransform::Constant { value } => {
                    set.insert(rule.column.clone(), Bson::String(value.clone()));
                }
                MaskTransform::Hash { .. } | MaskTransform::Email | MaskTransform::Phone => {
                    out.computed.push(ComputedUpdate {
                        collection: collection.to_string(),
                        field: rule.column.clone(),
                        transform: rule.transform.clone(),
                    });
                }
            }
        }
        if !set.is_empty() {
            out.server.push(ServerUpdate {
                collection: collection.to_string(),
                set,
            });
        }
    }
    out
}

/// The replacement value for one field, computed the same way the SQL engines
/// compute theirs.
///
/// Returns `None` when the value should be left alone: a missing or null field
/// stays missing or null, matching [`MaskTransform::preserves_null`] and the
/// `CONCAT`/`||` NULL propagation the SQL expressions rely on.
///
/// Non-string scalars are rendered to text first, which is what
/// `CAST(col AS CHAR)` and `col::text` do on the relational side. A subdocument
/// or array is *not* rendered — see [`is_maskable`].
pub fn replacement(value: Option<&Bson>, transform: &MaskTransform, salt: &str) -> Option<Bson> {
    let value = value?;
    if matches!(value, Bson::Null) {
        return None;
    }
    let text = scalar_to_text(value)?;

    let digest = {
        let mut hasher = Sha256::new();
        hasher.update(salt.as_bytes());
        hasher.update(text.as_bytes());
        format!("{:x}", hasher.finalize())
    };

    let masked = match transform {
        MaskTransform::Hash { length } => {
            let len = length.unwrap_or(DEFAULT_HASH_LENGTH) as usize;
            digest.chars().take(len).collect::<String>()
        }
        MaskTransform::Email => {
            format!("{}@{FAKE_EMAIL_DOMAIN}", &digest[..16])
        }
        MaskTransform::Phone => {
            // The same 7 hex characters the SQL expressions take, so a phone
            // number masked on MySQL and on MongoDB comes out identical.
            let n = u64::from_str_radix(&digest[..7], 16).unwrap_or(0) % 10_000_000;
            format!("{FAKE_PHONE_PREFIX}{n:07}")
        }
        // Handled entirely by the server; never routed here.
        MaskTransform::Null | MaskTransform::Constant { .. } => return None,
    };

    Some(Bson::String(masked))
}

/// Whether a value is one this can mask at all.
///
/// A subdocument or an array is not: replacing it with a hash of its rendering
/// would destroy structure the rest of the copy may depend on, and silently
/// changing a document's shape is worse than declining. Declining is safe
/// because the read-back still runs — an unmasked subdocument is counted as a
/// violation and the destination is dropped, so the operator is told rather
/// than left with a field they believe is masked.
pub fn is_maskable(value: &Bson) -> bool {
    !matches!(value, Bson::Document(_) | Bson::Array(_))
}

/// Render a scalar the way the SQL engines' text cast would.
fn scalar_to_text(value: &Bson) -> Option<String> {
    Some(match value {
        Bson::String(s) => s.clone(),
        Bson::Int32(v) => v.to_string(),
        Bson::Int64(v) => v.to_string(),
        Bson::Double(v) => v.to_string(),
        Bson::Boolean(v) => v.to_string(),
        Bson::ObjectId(v) => v.to_hex(),
        Bson::DateTime(v) => v.try_to_rfc3339_string().ok()?,
        Bson::Decimal128(v) => v.to_string(),
        // Documents and arrays are refused by `is_maskable`; anything else
        // exotic (binary, regex, code) has no meaningful text form to mask.
        _ => return None,
    })
}

/// Read a value at a dotted path.
pub fn get_path<'a>(doc: &'a Document, path: &str) -> Option<&'a Bson> {
    let mut current = doc;
    let mut parts = path.split('.').peekable();

    while let Some(part) = parts.next() {
        let value = current.get(part)?;
        if parts.peek().is_none() {
            return Some(value);
        }
        current = match value {
            Bson::Document(d) => d,
            // A path that runs through an array addresses every element in
            // MongoDB's own query language. Following that here would mean
            // rewriting elements, which `is_maskable` already declines, so the
            // path simply does not resolve and the read-back reports it.
            _ => return None,
        };
    }
    None
}

/// The read-back that proves the masking took.
///
/// One filter per rule, each matching documents that do **not** have the masked
/// shape. `countDocuments` with this filter answers the same question the SQL
/// `SUM(CASE WHEN … )` projection answers, and a non-zero answer has the same
/// consequence: the destination is dropped.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectionCheck {
    pub collection: String,
    pub field: String,
    pub transform: MaskTransform,
    pub filter: Document,
}

pub fn check_filters(rules: &[MaskRule]) -> Vec<CollectionCheck> {
    rules
        .iter()
        .map(|rule| CollectionCheck {
            collection: rule.table.clone(),
            field: rule.column.clone(),
            transform: rule.transform.clone(),
            filter: violation_filter(&rule.column, &rule.transform),
        })
        .collect()
}

/// Stands in for a value that has no string form.
///
/// A null byte cannot appear in a masked value — a hash is hex, an address ends
/// in a reserved domain, a phone number is digits — so this can never be
/// mistaken for something that *was* masked.
const UNCONVERTIBLE: &str = "\u{0}unmaskable";

/// A filter matching documents whose field is not masked.
///
/// `{field: {$ne: null}}` excludes both a null and a missing field, which is
/// the behaviour wanted throughout: there is no readable value in either case,
/// so neither is a violation.
///
/// The rendering to text is `$convert` with `onError` rather than the shorter
/// `$toString`, and that is not a stylistic choice. `$toString` on a
/// subdocument or an array aborts the whole aggregation with a conversion
/// error, so a single document whose field holds structure would take down the
/// check for the entire collection. The destination still gets dropped — every
/// error on this path does that — but the operator is handed
/// "Unsupported conversion from object to string" instead of being told which
/// field in which collection was left readable. `onError` turns that into what
/// it actually is: a value that is not masked, counted as one.
fn violation_filter(field: &str, transform: &MaskTransform) -> Document {
    let present = doc! { field: { "$ne": Bson::Null } };
    let as_text = doc! { "$convert": {
        "input": format!("${field}"),
        "to": "string",
        "onError": UNCONVERTIBLE,
        "onNull": UNCONVERTIBLE,
    } };

    match transform {
        MaskTransform::Hash { length } => {
            let len = length.unwrap_or(DEFAULT_HASH_LENGTH);
            doc! { "$and": [
                present,
                { "$expr": { "$not": { "$regexMatch": {
                    "input": as_text,
                    "regex": format!("^[0-9a-f]{{{len}}}$"),
                } } } },
            ] }
        }
        MaskTransform::Email => doc! { "$and": [
            present,
            { "$expr": { "$not": { "$regexMatch": {
                "input": as_text,
                "regex": format!("@{}$", regex_escape(FAKE_EMAIL_DOMAIN)),
            } } } },
        ] },
        MaskTransform::Phone => doc! { "$and": [
            present,
            { "$expr": { "$not": { "$regexMatch": {
                "input": as_text,
                "regex": format!("^{}", regex_escape(FAKE_PHONE_PREFIX)),
            } } } },
        ] },
        MaskTransform::Null => present,
        // A constant overwrites nulls too, so a surviving null is a miss —
        // the same reasoning as the SQL branch.
        MaskTransform::Constant { value } => doc! { "$or": [
            { field: Bson::Null },
            { "$expr": { "$ne": [as_text, value.clone()] } },
        ] },
    }
}

/// Escape the characters that would otherwise be regex syntax.
///
/// `example.invalid` and `+1555` both contain them, and an unescaped `+` is not
/// a stricter pattern — it is an invalid one, which fails the check and drops a
/// correctly masked database.
fn regex_escape(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    for c in literal.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ── Execution ───────────────────────────────────────────────────────────

/// Apply every rule, returning how many documents were rewritten.
///
/// # Cost
///
/// The server-side half is one `updateMany` per collection. The computed half
/// is one `updateOne` per document, because the replacement depends on the
/// value and MongoDB cannot compute it — there is no `bulkWrite` on a
/// collection in this driver to fold them into, and grouping by value only
/// helps for fields that repeat, which the interesting ones (emails, phone
/// numbers, identifiers) do not.
///
/// So masking a million-document collection with a `Hash` rule is a million
/// round trips. That is the price of a pseudonym that matches what the SQL
/// engines produce, stated here rather than discovered in production.
pub async fn apply(
    params: &crate::db::ConnectParams,
    database: &str,
    rules: &[MaskRule],
    salt: &str,
) -> Result<u64, super::MaskError> {
    let plan = plan(rules);
    let introspector = crate::db::MongoIntrospector::connect(params).await?;
    let db = introspector.client().database(database);

    let mut rewritten: u64 = 0;

    for update in &plan.server {
        let result = db
            .collection::<Document>(&update.collection)
            .update_many(doc! {}, doc! { "$set": update.set.clone() })
            .await
            .map_err(|e| {
                crate::db::DbError::Query(format!("masking {}: {e}", update.collection))
            })?;
        rewritten += result.modified_count;
    }

    for update in &plan.computed {
        let collection = db.collection::<Document>(&update.collection);

        // Only documents that have something to mask. A missing or null field
        // is left alone, matching `MaskTransform::preserves_null` and the SQL
        // side's NULL propagation.
        let mut cursor = collection
            .find(doc! { &update.field: { "$ne": Bson::Null } })
            .batch_size(REWRITE_BATCH as u32)
            .await
            .map_err(|e| {
                crate::db::DbError::Query(format!("reading {}: {e}", update.collection))
            })?;

        while cursor
            .advance()
            .await
            .map_err(|e| crate::db::DbError::Query(format!("reading {}: {e}", update.collection)))?
        {
            let document = cursor
                .deserialize_current()
                .map_err(|e| crate::db::DbError::Query(format!("decoding a document: {e}")))?;

            let Some(current) = get_path(&document, &update.field) else {
                continue;
            };
            if !is_maskable(current) {
                // Left as it is, and deliberately not skipped silently: the
                // read-back counts it as unmasked and the destination is
                // dropped, so the operator is told.
                continue;
            }
            let Some(new_value) = replacement(Some(current), &update.transform, salt) else {
                continue;
            };
            let Some(id) = document.get("_id") else {
                continue;
            };

            let result = collection
                .update_one(
                    doc! { "_id": id.clone() },
                    doc! { "$set": { &update.field: new_value } },
                )
                .await
                .map_err(|e| {
                    crate::db::DbError::Query(format!("masking {}: {e}", update.collection))
                })?;
            rewritten += result.modified_count;
        }
    }

    Ok(rewritten)
}

/// Read the destination back and prove every rule took.
///
/// Returns the `collection.field` pairs it confirmed. A non-zero count is
/// returned as [`MaskError::NotMasked`], which the caller turns into a dropped
/// database — the guarantee is that either the destination is masked or it is
/// gone.
pub async fn verify(
    params: &crate::db::ConnectParams,
    database: &str,
    rules: &[MaskRule],
) -> Result<Vec<String>, super::MaskError> {
    let introspector = crate::db::MongoIntrospector::connect(params).await?;
    let db = introspector.client().database(database);

    let mut confirmed = Vec::new();
    for check in check_filters(rules) {
        let unmasked = db
            .collection::<Document>(&check.collection)
            .count_documents(check.filter.clone())
            .await
            .map_err(|e| {
                crate::db::DbError::Query(format!("checking {}: {e}", check.collection))
            })?;

        if unmasked > 0 {
            return Err(super::MaskError::NotMasked {
                table: check.collection,
                column: check.field,
                // The counts elsewhere are signed because SQL returns them
                // that way; the value itself is a count either way.
                count: unmasked as i64,
            });
        }
        confirmed.push(format!("{}.{}", check.collection, check.field));
    }
    Ok(confirmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn salt() -> &'static str {
        "0123456789abcdef"
    }

    // ── Planning ────────────────────────────────────────────────────────

    #[test]
    fn constant_and_null_stay_on_the_server() {
        let rules = vec![
            MaskRule {
                table: "users".into(),
                column: "ssn".into(),
                transform: MaskTransform::Null,
            },
            MaskRule {
                table: "users".into(),
                column: "notes".into(),
                transform: MaskTransform::Constant {
                    value: "redacted".into(),
                },
            },
        ];
        let plan = plan(&rules);
        assert!(
            plan.computed.is_empty(),
            "neither transform needs the old value"
        );
        assert_eq!(plan.server.len(), 1, "one updateMany per collection");
        assert_eq!(plan.server[0].set.len(), 2);
    }

    #[test]
    fn hashing_transforms_need_the_value_and_are_routed_here() {
        let rules = vec![
            MaskRule::hash("users", "password"),
            MaskRule::email("users", "email"),
            MaskRule {
                table: "users".into(),
                column: "phone".into(),
                transform: MaskTransform::Phone,
            },
        ];
        let plan = plan(&rules);
        assert_eq!(plan.computed.len(), 3);
        assert!(
            plan.server.is_empty(),
            "nothing here can be computed by the server"
        );
    }

    #[test]
    fn one_collection_can_need_both_routes() {
        let rules = vec![
            MaskRule::email("users", "email"),
            MaskRule {
                table: "users".into(),
                column: "ssn".into(),
                transform: MaskTransform::Null,
            },
        ];
        let plan = plan(&rules);
        assert_eq!(plan.server.len(), 1);
        assert_eq!(plan.computed.len(), 1);
        assert!(!plan.is_empty());
    }

    // ── Transforms ──────────────────────────────────────────────────────

    #[test]
    fn the_same_input_and_salt_give_the_same_pseudonym() {
        let a = replacement(
            Some(&Bson::String("alice@corp.test".into())),
            &MaskTransform::Email,
            salt(),
        );
        let b = replacement(
            Some(&Bson::String("alice@corp.test".into())),
            &MaskTransform::Email,
            salt(),
        );
        assert_eq!(a, b, "masked copies must still join across collections");
    }

    #[test]
    fn a_different_salt_gives_a_different_pseudonym() {
        let a = replacement(
            Some(&Bson::String("alice@corp.test".into())),
            &MaskTransform::Email,
            salt(),
        );
        let b = replacement(
            Some(&Bson::String("alice@corp.test".into())),
            &MaskTransform::Email,
            "a different salt",
        );
        assert_ne!(a, b);
    }

    #[test]
    fn the_salt_is_hashed_before_the_value_not_after() {
        // Order matters and is part of the cross-engine contract: the SQL side
        // computes SHA256(salt || value). If this ever flips, a MySQL copy and
        // a MongoDB copy stop agreeing, silently.
        let value = "alice@corp.test";
        let expected = {
            let mut h = Sha256::new();
            h.update(salt().as_bytes());
            h.update(value.as_bytes());
            format!("{:x}", h.finalize())
        };
        let got = replacement(
            Some(&Bson::String(value.into())),
            &MaskTransform::Hash { length: None },
            salt(),
        );
        assert_eq!(got, Some(Bson::String(expected)));
    }

    #[test]
    fn a_hash_is_truncated_to_the_requested_length() {
        let got = replacement(
            Some(&Bson::String("secret".into())),
            &MaskTransform::Hash { length: Some(8) },
            salt(),
        );
        let Some(Bson::String(s)) = got else {
            panic!("expected a string")
        };
        assert_eq!(s.len(), 8);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_masked_email_cannot_be_delivered_to() {
        let got = replacement(
            Some(&Bson::String("alice@corp.test".into())),
            &MaskTransform::Email,
            salt(),
        );
        let Some(Bson::String(s)) = got else {
            panic!("expected a string")
        };
        assert!(
            s.ends_with(&format!("@{FAKE_EMAIL_DOMAIN}")),
            "masked address {s} must be undeliverable"
        );
    }

    #[test]
    fn a_masked_phone_cannot_ring_anyone() {
        let got = replacement(
            Some(&Bson::String("+441632960900".into())),
            &MaskTransform::Phone,
            salt(),
        );
        let Some(Bson::String(s)) = got else {
            panic!("expected a string")
        };
        assert!(s.starts_with(FAKE_PHONE_PREFIX), "got {s}");
        assert_eq!(s.len(), FAKE_PHONE_PREFIX.len() + 7);
    }

    #[test]
    fn null_and_missing_are_left_alone() {
        assert_eq!(replacement(None, &MaskTransform::Email, salt()), None);
        assert_eq!(
            replacement(Some(&Bson::Null), &MaskTransform::Email, salt()),
            None
        );
    }

    #[test]
    fn non_string_scalars_are_rendered_before_hashing() {
        // A phone number stored as an int64 is ordinary in a document store,
        // and refusing to mask it would leave it readable.
        assert!(
            replacement(
                Some(&Bson::Int64(441632960900)),
                &MaskTransform::Phone,
                salt()
            )
            .is_some(),
            "an integer field must still mask"
        );
    }

    #[test]
    fn structure_is_never_replaced_by_a_hash() {
        assert!(!is_maskable(&Bson::Document(doc! { "a": 1 })));
        assert!(!is_maskable(&Bson::Array(vec![Bson::Int32(1)])));
        assert!(is_maskable(&Bson::String("x".into())));
    }

    // ── Paths ───────────────────────────────────────────────────────────

    #[test]
    fn a_dotted_path_reads_a_nested_field() {
        let d = doc! { "profile": { "contact": { "email": "a@b.test" } } };
        assert_eq!(
            get_path(&d, "profile.contact.email"),
            Some(&Bson::String("a@b.test".into()))
        );
    }

    #[test]
    fn a_path_that_does_not_resolve_reads_as_absent() {
        let d = doc! { "profile": { "name": "alice" } };
        assert_eq!(get_path(&d, "profile.email"), None);
        assert_eq!(get_path(&d, "nothing.here"), None);
        // Through an array, deliberately — see the note on `get_path`.
        let arr = doc! { "tags": [ { "email": "a@b.test" } ] };
        assert_eq!(get_path(&arr, "tags.email"), None);
    }

    // ── Read-back ───────────────────────────────────────────────────────

    #[test]
    fn every_rule_gets_a_check() {
        let rules = vec![
            MaskRule::hash("users", "password"),
            MaskRule::email("users", "email"),
            MaskRule {
                table: "orders".into(),
                column: "note".into(),
                transform: MaskTransform::Null,
            },
        ];
        let checks = check_filters(&rules);
        assert_eq!(
            checks.len(),
            rules.len(),
            "a rule with no check is a rule with no guarantee"
        );
    }

    #[test]
    fn the_email_check_pattern_is_escaped() {
        // `example.invalid` unescaped would let `exampleXinvalid` pass, and
        // more importantly the phone prefix's `+` is not valid regex at all.
        let checks = check_filters(&[MaskRule::email("users", "email")]);
        let rendered = format!("{:?}", checks[0].filter);
        assert!(
            rendered.contains("example\\\\.invalid") || rendered.contains(r"example\.invalid"),
            "dot must be escaped: {rendered}"
        );
    }

    #[test]
    fn the_phone_check_escapes_the_leading_plus() {
        let checks = check_filters(&[MaskRule {
            table: "users".into(),
            column: "phone".into(),
            transform: MaskTransform::Phone,
        }]);
        let rendered = format!("{:?}", checks[0].filter);
        assert!(
            !rendered.contains(r"^+1555"),
            "an unescaped + is invalid regex and would fail a correct mask: {rendered}"
        );
    }

    #[test]
    fn regex_escaping_covers_the_metacharacters() {
        assert_eq!(regex_escape("a.b"), r"a\.b");
        assert_eq!(regex_escape("+1555"), r"\+1555");
        assert_eq!(regex_escape("plain"), "plain");
    }

    #[test]
    fn a_null_rule_counts_any_surviving_value() {
        let checks = check_filters(&[MaskRule {
            table: "users".into(),
            column: "ssn".into(),
            transform: MaskTransform::Null,
        }]);
        assert_eq!(checks[0].filter, doc! { "ssn": { "$ne": Bson::Null } });
    }

    #[test]
    fn a_constant_rule_counts_surviving_nulls_too() {
        // The constant overwrites nulls, so one that survived means the update
        // did not reach that document.
        let checks = check_filters(&[MaskRule {
            table: "users".into(),
            column: "note".into(),
            transform: MaskTransform::Constant {
                value: "redacted".into(),
            },
        }]);
        let rendered = format!("{:?}", checks[0].filter);
        assert!(rendered.contains("$or"), "{rendered}");
        assert!(rendered.contains("Null"), "{rendered}");
    }

    #[test]
    fn the_hash_check_pins_the_exact_length() {
        let checks = check_filters(&[MaskRule {
            table: "users".into(),
            column: "password".into(),
            transform: MaskTransform::Hash { length: Some(12) },
        }]);
        let rendered = format!("{:?}", checks[0].filter);
        assert!(
            rendered.contains("^[0-9a-f]{12}$"),
            "a truncated column must not pass the check: {rendered}"
        );
    }

    #[test]
    fn a_truncated_hash_fails_its_own_check_pattern() {
        // The failure this guards: a field too narrow for 64 hex characters
        // silently holds a prefix. The pattern is anchored at both ends so the
        // prefix does not match.
        let checks = check_filters(&[MaskRule::hash("users", "password")]);
        let rendered = format!("{:?}", checks[0].filter);
        assert!(rendered.contains(&format!("^[0-9a-f]{{{DEFAULT_HASH_LENGTH}}}$")));
    }
}
