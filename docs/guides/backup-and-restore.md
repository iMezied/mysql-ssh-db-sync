# How to back up and restore

## Back up

```bash
dbsync backup <profile> [--database NAME] [--dir PATH] [--count-rows]
              [--schema-only TABLE]... [--exclude TABLE]...
              [--encrypt] [--no-compress]
```

`<profile>` is a connection id or a unique prefix of its name.

```bash
dbsync backup prod --count-rows
dbsync backup prod --database orders --schema-only audit_log --exclude sessions
```

**Every table is dumped with its data unless you say otherwise.** This is the
opposite of the app's default, on purpose: a command run from cron with no table
list means "all of it".

In the app, **Backup** gives you the same options with a table picker.

### Options that cost something

| Flag | Cost | Buys you |
|---|---|---|
| `--count-rows` | A full scan per data table | A drill can compare exact numbers instead of only checking each table arrived |
| `--encrypt` | Negligible CPU | Artifact encrypted at rest to the installation key |
| `--no-compress` | Much larger files | Faster write, and a plain-text dump you can read |

## What you get

An artifact in the backup directory, and a manifest describing it: engine,
server version, dump tool and version, database, every table, which carry data,
sizes, checksum, encryption recipients if any.

```bash
dbsync library                    # what exists, sizes, growth, shrink warnings
dbsync library --dir /backups     # a different directory
```

## Restore

```bash
dbsync restore <profile> <artifact> [target flags] [--no-verify-checksum]
```

Three target strategies, in increasing order of danger:

**1. New timestamped database (the default, cannot destroy anything)**

```bash
dbsync restore staging /backups/app_20260727T040000.sql.gz
dbsync restore staging /backups/app_....sql.gz --new-prefix recovery
```

**2. Into an existing database, without dropping it**

```bash
dbsync restore staging /backups/app_....sql.gz --into app_test --confirm app_test
```

**3. Replace: drop it first, then restore**

```bash
dbsync restore staging /backups/app_....sql.gz --replace app_test --confirm app_test
```

`--confirm` takes the target's name typed back. It is checked by the engine, not
by the CLI, so the app and the command line enforce it identically. A connection
tagged `prod` requires confirmation even for strategies that would not otherwise
need it.

### Checksum verification

Before an artifact reaches a server its checksum is compared against its
manifest. `--no-verify-checksum` skips that. The check is cheap next to a restore
and catches a truncated or altered artifact, so turning it off wants a reason.

### PostgreSQL selective restore

```bash
dbsync restore staging dump.dump --only-table orders --only-table customers
```

Needs an archive-format dump, not plain SQL.

## The DEFINER problem

MySQL dumps taken by a privileged user carry `DEFINER=` clauses that fail on
restore for any user without `SUPER`. They are stripped as the dump streams,
quote-aware, so rows whose *data* contains the literal text `DEFINER=` come back
byte-identical.

To repair a dump you already have:

```bash
dbsync strip-definers < raw.sql > clean.sql
```

## Verification

Restore alone does not compare anything against the source. For that:

- `dbsync drill <profile>` — restore the newest artifact, check it, drop it
- a sync with verify on — see [Sync](sync.md) and
  [Verification](../explanation/verification.md)

## Troubleshooting

| Symptom | Fix |
|---|---|
| "checksum does not match the manifest" | The artifact is truncated or altered. Do not use `--no-verify-checksum` to get past it; get a good copy. |
| "you need SUPER privilege" on restore | A dump from elsewhere. Run it through `dbsync strip-definers`. |
| Restore refuses without `--confirm` | Intended. The target can destroy data, or the connection is `prod`-tagged. |
| `mysqldump: command not found` | Client tools missing. The app's Settings page reports which tools it found and their versions. |
| Restore succeeded but the app misbehaves | A drill proves an artifact restores, not that the data is correct. See [Verification](../explanation/verification.md). |

## Related

- [CLI reference](../reference/cli.md)
- [Encryption](encryption.md) · [Off-site copies](offsite.md)
- [Library, retention and drills](library-retention-drills.md)
