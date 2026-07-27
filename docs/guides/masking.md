# How to mask sensitive columns

Masking rewrites named columns so a production copy can be handed to people who
are not cleared to see production.

## What it does and does not touch

**It rewrites the destination of a sync, after the restore.** The backup artifact
still holds the real data.

That is deliberate. The artifact is your recovery path, and a masked recovery
path is not one. If you need the file itself protected, that is
[encryption](encryption.md), which is a different tool for a different problem.

## Where rules live

On the **sync plan**. Every schedule running that plan inherits them.

## Add rules

In the app: the **Masking** page, per plan.

From the CLI:

```bash
dbsync mask list nightly
dbsync mask add nightly users email --transform email
dbsync mask add nightly users ssn   --transform null
dbsync mask add nightly users phone --transform phone
dbsync mask add nightly users notes --transform constant --value redacted
dbsync mask remove nightly users notes
```

`<plan>` is a plan id or a unique prefix of its name. For PostgreSQL the table
may be `schema.table`; a bare name means `public`.

## The transforms

| Transform | Result | NULL handling |
|---|---|---|
| `hash` | Salted SHA-256, hex | NULL stays NULL |
| `email` | Deterministic address at `example.invalid` | NULL stays NULL |
| `phone` | Deterministic number in the reserved 555 range | NULL stays NULL |
| `null` | Every row NULL | Fails on a `NOT NULL` column |
| `constant` | Every row set to `--value` | NULLs included |

**Deterministic** means the same input gives the same output within an
installation, so joins and uniqueness survive masking. The salt is derived from a
local secret and never leaves the machine — in particular it is not in a shared
configuration bundle, because anyone holding both the rules and the salt could
reverse the pseudonyms.

## See exactly what will run

```bash
dbsync mask sql nightly
```

Prints the `UPDATE` statements a masking run would send, plus the read-back
checks. The salt appears as a bound placeholder, never as a literal.

The app's Masking page shows the same preview, and it also lists **inert** rules:
a rule on a schema-only table has no rows to mask, so it is reported rather than
silently dropped.

## Verification

After the rewrite, each rule is read back to prove it happened. A masking run
that could not confirm its own effect fails rather than reporting success.

## Troubleshooting

| Symptom | Cause |
|---|---|
| Rule reported inert | The table is schema-only in the plan, so there are no rows |
| "fails on a NOT NULL column" | `null` transform on a `NOT NULL` column. Use `constant` or `hash`. |
| Destination still has real values | The rule names a column that does not exist, or the sync ran before the rule was added |
| Joins broke after masking | A non-deterministic expectation. `hash`, `email` and `phone` are deterministic; `constant` collapses every row to one value. |

## Related

- [Sync between servers](sync.md)
- [Security model](../explanation/security-model.md)
- Why it runs on the destination: [DECISIONS.md](../../DECISIONS.md), M9
