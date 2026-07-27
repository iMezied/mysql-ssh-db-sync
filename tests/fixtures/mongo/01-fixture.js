// MongoDB fixture.
//
// Chosen, like the SQL fixtures, to contain the things that break naive
// tooling rather than to look tidy:
//
//   * documents in one collection with *different* field sets, so a field list
//     read from a sample would disagree with one read from the whole
//     collection — the reason `column_names` scans rather than samples;
//   * a nested subdocument, so a masking rule addressed by dotted path has
//     something to resolve against;
//   * an array of subdocuments, so the case masking deliberately declines is
//     present and its read-back can be observed;
//   * a field holding a number where its siblings hold a string, so the text
//     rendering the masking path relies on is exercised;
//   * non-ASCII text and a unicode collection name;
//   * documents whose field *order* differs while their content matches,
//     which the digest has to treat as equal.

const db = new Mongo().getDB("fixture");

db.users.insertMany([
  {
    _id: 1,
    email: "alice@corp.test",
    display_name: "Alice",
    phone: "+441632960901",
    profile: { contact: { email: "alice.work@corp.test" }, tier: "gold" },
    created_at: new Date("2026-01-02T03:04:05Z"),
  },
  {
    // Same fields, written in a different order. A digest that hashed the raw
    // BSON encoding would call this a different document from an identical
    // restore that happened to normalise ordering.
    display_name: "Bob",
    email: "bob@corp.test",
    _id: 2,
    created_at: new Date("2026-01-03T03:04:05Z"),
    profile: { tier: "silver", contact: { email: "bob.work@corp.test" } },
    phone: "+441632960902",
  },
  {
    _id: 3,
    email: "chloé@corp.test",
    display_name: "Chloé",
    // A number where the others hold a string.
    phone: 441632960903,
    // No `profile` at all: a field present in some documents and not others.
    created_at: new Date("2026-01-04T03:04:05Z"),
    // A field only this document has, so a first-N sample would miss it.
    referred_by: 1,
  },
  {
    _id: 4,
    email: null,
    display_name: "No Email",
    created_at: new Date("2026-01-05T03:04:05Z"),
  },
]);

db.orders.insertMany([
  { _id: 1, buyer_email: "alice@corp.test", total: 120.5 },
  { _id: 2, buyer_email: "bob@corp.test", total: 80 },
  // An array of subdocuments. A masking rule pointed inside this is refused
  // rather than flattened, and the read-back is what reports it.
  {
    _id: 3,
    buyer_email: "alice@corp.test",
    total: 10,
    lines: [{ sku: "A-1", note: "gift" }, { sku: "B-2" }],
  },
]);

// A collection excluded by the selection tests.
db.sessions.insertMany([
  { _id: 1, token: "abc", user_id: 1 },
  { _id: 2, token: "def", user_id: 2 },
]);

// A unicode collection name, matching the SQL fixtures' unicode table.
db.getCollection("naïve_café").insertOne({ _id: 1, note: "café" });

// An index, so a restore that drops indexes can be told from one that keeps
// them.
db.users.createIndex({ email: 1 }, { name: "users_email_idx" });
