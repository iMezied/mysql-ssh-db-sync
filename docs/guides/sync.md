# How to sync between servers

A sync is one job that backs up a source and restores it to a destination, with
verification, and optionally masking on the way in. Refreshing staging from
production is the case it exists for.

## Where a sync runs from

| Route | How |
|---|---|
| Desktop app | **Sync** page, interactively |
| Schedule | A `sync`-kind schedule with a destination connection set |
| CLI | Via a schedule: `dbsync schedule run <name>` |

There is no standalone `dbsync sync` command. A sync needs a plan, a
destination and a restore strategy, and once you have named all three you have
described a schedule. Create it once and run it on demand or on a timer.

## Prerequisites

- Two connections: source and destination, both tested
- A sync plan on the source — which tables, and which of those carry data

## Create the plan

**Sync → New plan**: pick the connection, pick the database, then per table
choose schema-and-data, schema-only, or excluded. A schema-and-data table may
carry a `WHERE` filter, which is how you take last month's orders rather than
all of them.

Plans have a revision number that increments on change, so a schedule notices
that the plan under it was edited.

### Importing an existing table list

If you already have a `tables.conf` from the legacy tooling, the app imports it
rather than making you re-tick 200 boxes.

The file names only the tables that carry data, so the import completes it
against the source: every other table it finds is set to **schema only**, the
way the old script behaved. That is why the button waits for the table list to
load — and why a table the file names but the source no longer has is dropped
rather than carried as a dead entry.

## Run it

From the app's **Sync** page: pick plan, destination, target strategy, and
whether to verify.

As a schedule:

```bash
dbsync schedule run nightly-refresh
```

## Target strategies

Same three as a plain restore, with one rule that only applies here:

- **New timestamped database** — the default, destroys nothing
- **Into an existing database** — needs typed confirmation
- **Replace** — drops first, needs typed confirmation

**A schedule cannot use a destructive strategy.** It is refused when you save it,
because there is nobody present at 04:00 to confirm.

## Verification

Turn **Verify** on. After the restore, the destination is compared against the
source:

- **Standard**: row counts per table
- **Deep** (`deep_verify`): content digests, column by column

Deep costs a full scan of every table on both sides. It is off by default and
stays off on upgrade, so an existing schedule does not silently acquire the cost.

What each level actually proves: [Verification](../explanation/verification.md).

## Masking

If the destination is less trusted than the source — staging, a developer
machine, anywhere a support engineer can read — put masking on the plan. It
rewrites named columns **on the destination, after the restore**, and reads back
to prove the rewrite happened.

See [Masking](masking.md).

## Retention

A sync writes an artifact like any backup, so the same retention applies: keep
the last N, or delete anything older than D days. Set it on the schedule.

## Troubleshooting

| Symptom | Cause |
|---|---|
| "a sync schedule needs a plan" | Kind is `sync` but no plan is set |
| "a schedule with a destination must say how to restore" | Destination set, restore options missing |
| "the plan is for a Mysql source but the restore options are Postgres" | Source and destination engines differ. Cross-engine sync is not supported. |
| Verify reports "not compared" | The manifest has no row counts. Back up with `--count-rows`. |
| Destination has real customer data | Masking was not on the plan, or the rule named a column that does not exist |

## Related

- [Scheduling](scheduling.md) · [Masking](masking.md)
- [Verification](../explanation/verification.md)
- Concepts: [Sync plan](../concepts.md#sync-plan)
