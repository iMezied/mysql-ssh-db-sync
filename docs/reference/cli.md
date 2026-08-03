# CLI reference

```
dbsync [OPTIONS] <COMMAND>
```

Generated from `dbsync --help`. Re-run it if this page and the binary disagree —
the binary wins.

## Global options

| Option | Effect |
|---|---|
| `--store <PATH>` | Application database to use. Defaults to the shared location the desktop app uses, so both see the same profiles. |
| `--json` | Emit progress and results as JSON-lines on stdout |
| `-h, --help` | Help. `--help` (long) gives the full rationale; `-h` a summary. |
| `-V, --version` | Version |

## Commands at a glance

| Command | What it does |
|---|---|
| `profiles` | List connection profiles |
| `ssh` | List saved SSH servers and what tunnels through them |
| `jobs` | List recent job history |
| `doctor` | Report the resolved store path and engine version |
| `audit` | Show recent configuration changes |
| `library` | Summarise the backup library; non-zero on a shrink warning |
| `strip-definers` | Strip `DEFINER=` clauses from a MySQL dump on stdin |
| `schedule` | Inspect and run scheduled jobs |
| `drill` | Prove the newest backup in a directory actually restores |
| `pipeline` | List, inspect and run saved chains of actions |
| `backup` | Back up a database to an artifact on disk |
| `restore` | Restore an artifact into a database |
| `mask` | Manage the masking rules on a sync plan |
| `key` | Manage the backup encryption key |
| `config` | Share configuration with a team, without sharing access |
| `destination` | Manage off-site destinations |
| `daemon` | Run the scheduler in the foreground until interrupted |

There is **no `sync` command**. A sync needs a plan, a destination and a restore
strategy; once named, that is a schedule. Create it in the app and run it with
`dbsync schedule run <name>`.

---

## Listing and inspection

### `dbsync profiles`

Lists id, name, engine, environment and `user@host:port`.

### `dbsync ssh`

Lists id, name, `user@host:port`, ` via <jump>` when there is one, and in
brackets the profiles tunnelling through it (or `unused`).

Read-only by design: creating one means verifying an unrecognised host key,
which no cron job can answer.

### `dbsync jobs [--limit N]`

Default limit 20. What **ran**.

### `dbsync audit [--limit N]`

Default limit 50. What was **changed** — a masking rule removed, a connection
re-pointed, the backup key exported. Usually not a job at all.

### `dbsync doctor`

Store path, engine version, profile count.

### `dbsync library [--dir PATH]`

Sizes, growth, and backups that shrank. **Exits non-zero** on a shrink warning,
so it works as a cron check.

---

## `dbsync backup <PROFILE>`

`<PROFILE>` is an id or a unique prefix of a name.

| Option | Effect |
|---|---|
| `--database <NAME>` | Database to dump. Defaults to the profile's own. |
| `--dir <PATH>` | Where to write it. Defaults to the app's backup folder. |
| `--schema-only <TABLE>` | Schema but not rows. Repeatable. |
| `--exclude <TABLE>` | Leave out entirely. Repeatable. |
| `--encrypt` | Encrypt to the installation's backup key |
| `--no-compress` | Write uncompressed |
| `--count-rows` | Count rows first and record them in the manifest. Costs a full scan per data table; lets a later drill compare exact numbers. |

Every table is dumped with its data unless `--schema-only` or `--exclude` says
otherwise — the opposite of the GUI default, because a cron command with no table
list means "all of it".

---

## `dbsync restore <PROFILE> <ARTIFACT>`

| Option | Effect |
|---|---|
| `--new-prefix <PREFIX>` | Restore into `{prefix}_{timestamp}`. The default, prefix taken from the artifact's database name. |
| `--into <DB>` | Restore into this database without dropping it |
| `--replace <DB>` | Drop this database if it exists, then restore |
| `--confirm <NAME>` | The target's name typed back. Required when the restore can destroy data, checked by the engine. |
| `--no-verify-checksum` | Skip the manifest checksum comparison |
| `--only-table <TABLE>` | PostgreSQL only; needs an archive format. Repeatable. |

---

## `dbsync drill <PROFILE>`

| Option | Effect |
|---|---|
| `--dir <PATH>` | Directory holding the backups |
| `--deep` | Read every row, not just count them |
| `--keep-on-failure` | Leave the scratch database behind on failure |

Exits non-zero if the restore or the check failed. A passing drill always cleans
up.

---

## `dbsync pipeline <SUBCOMMAND>`

| Subcommand | What it does |
|---|---|
| `list` | Saved pipelines, with what each replaces and whether it is armed |
| `show <PIPELINE>` | The chain step by step, and what a run would need |
| `run <PIPELINE>` | Run it now |

`run` options:

| Option | Effect |
|---|---|
| `--confirm <NAME>` | A database this pipeline replaces, typed back. Repeat once per destructive step, in the order they appear |
| `--dir <PATH>` | Where a backup step writes when it names no directory of its own |

`<PIPELINE>` is an id or a unique prefix of the name. An ambiguous prefix is
refused rather than guessed — guessing could start a chain that drops a
database.

Exits non-zero when any step failed or a check did not pass.

Pipelines are **created in the app**, not here. The option surface is large,
and a second construction path is a second place for the destructive target
check to be forgotten — the same reasoning as schedules.

A pipeline that replaces a database needs `--confirm` for each target, or to
have been **armed** in the app for unattended runs. An armed one runs with no
arguments:

```bash
dbsync pipeline run "refresh staging"
```

See [Pipelines](../guides/pipelines.md).

---

## `dbsync schedule <SUBCOMMAND>`

| Subcommand | Effect |
|---|---|
| `list` | Configured schedules and when they next run |
| `show <SCHEDULE>` | One schedule in full |
| `run <SCHEDULE>` | Run once, now, and wait for it |
| `tick` | Run anything currently due, then exit |
| `crontab <SCHEDULE>` | Print a correctly quoted crontab line |
| `add-drill <NAME> <PROFILE> <CRON>` | Create a recurring drill. `--dir`, `--deep`. |

`<SCHEDULE>` is an id or a unique prefix of a name. `<CRON>` is five fields, e.g.
`"0 4 * * *"`.

---

## `dbsync mask <SUBCOMMAND>`

| Subcommand | Effect |
|---|---|
| `list <PLAN>` | Rules on a plan |
| `add <PLAN> <TABLE> <COLUMN> --transform <T> [--value V]` | Add or replace a rule |
| `remove <PLAN> <TABLE> <COLUMN>` | Drop a rule |
| `sql <PLAN>` | Print the SQL a masking run would send |

Transforms: `hash` (salted SHA-256, NULL stays NULL), `email` (deterministic
address at `example.invalid`), `phone` (deterministic, reserved 555 range),
`null` (fails on `NOT NULL`), `constant` (needs `--value`, NULLs included).

For PostgreSQL the table may be `schema.table`; a bare name means `public`.

---

## `dbsync key <SUBCOMMAND>`

| Subcommand | Effect |
|---|---|
| `status` | Whether a key exists, its public half, and who else can decrypt |
| `generate` | Create the installation's key if there is not one |
| `export` | Print the secret key so it can be stored somewhere safe |
| `import` | Adopt a key exported elsewhere, replacing the current one |
| `recipients <KEY>...` | Replace the additional recipients for future backups |

---

## `dbsync config <SUBCOMMAND>`

| Subcommand | Effect |
|---|---|
| `export [--out FILE]` | Write a shareable bundle. Defaults to stdout. |
| `import [FILE] [--dry-run]` | Apply a bundle. Defaults to stdin. |

Contains no credentials. Matches by name, creates what is missing, updates what
is there, removes nothing.

---

## `dbsync destination <SUBCOMMAND>`

| Subcommand | Effect |
|---|---|
| `list` | Destinations and where they point |
| `add` | Add an S3-compatible destination. Secret key from **stdin**. |
| `set-key <DEST>` | Replace the secret access key, from stdin |
| `test <DEST>` | Endpoint resolves, credential accepted, bucket listable |
| `enable <DEST>` / `disable <DEST>` | Start or stop using it, keeping configuration |
| `retention <DEST> [--keep-last N] [--max-age-days D]` | Neither limit clears the policy |
| `push <ARTIFACT>` | Upload an existing artifact to every enabled destination |
| `remove <DEST>` | Delete it and its stored credential |

`add` options: `--name`, `--endpoint`, `--bucket`, `--access-key-id` (all
required), `--region` (default `us-east-1`), `--prefix` (default empty),
`--path-style`.

---

## `dbsync daemon [--interval SECONDS]`

Runs the scheduler loop in the foreground until interrupted. Default interval 30
seconds; cron's resolution is one minute, so there is nothing to gain below that.

---

## `dbsync strip-definers`

Reads a MySQL dump on stdin, writes it to stdout with `DEFINER=` clauses removed.
Quote-aware, so text inside string literals is untouched.

```bash
dbsync strip-definers < raw.sql > clean.sql
```

## Related

- [Running headless](../guides/headless.md)
- [Desktop app reference](desktop-app.md)
