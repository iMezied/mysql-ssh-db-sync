# For operators

*DBAs, ops, SRE. You will spend most of your time in the CLI.*

## The ten-minute version

```bash
dbsync doctor                      # where is the store, what version
dbsync profiles                    # what connections exist
dbsync ssh                         # what tunnels exist, and what uses them
dbsync backup prod --count-rows    # back up, recording row counts
dbsync library                     # sizes, growth, and anything that shrank
dbsync drill staging               # prove the newest artifact restores
dbsync jobs --limit 10             # what ran
dbsync audit --limit 20            # what changed
```

Every command takes `--store <path>` to point at a different application
database, and `--json` for machine-readable output. `--json` is what you want in
CI.

## Setting up

Connections and SSH servers are created **in the desktop app**, not the CLI. That
is deliberate: adding an SSH server means verifying an unrecognised host key
before it is pinned, and that is a prompt no cron job can answer. Once they
exist, everything else is available headless.

Read [Connections and SSH servers](../guides/connections-and-ssh.md) before the
first one. The trap: with a tunnel in use, the database host and port are
resolved *from the SSH server*, usually `127.0.0.1`.

## The daily runbook

**A backup failed.**
`dbsync jobs --limit 5` for the outcome, then the app's Jobs page for the
per-step detail. Steps are reported separately — SSH, tunnel, database, catalog —
because four different things can be wrong and they need different fixes.

**A drill failed.**
The artifact did not restore, or did not match its manifest. Run it by hand with
`--keep-on-failure` to leave the scratch database behind for inspection.

**Something shrank.**
`dbsync library` exits non-zero when a backup came out dramatically smaller than
the one before it. That is the failure nothing else notices: the artifact is
valid, the checksum matches, it restores, and it is only wrong relative to
yesterday. Usually a table stopped being selected, or a `WHERE` filter changed.

**A restore is needed, for real.**
Default target is a new timestamped database, which cannot destroy anything.
`--replace` and `--into` can destroy, and require `--confirm` with the exact
target name typed back. Production-tagged connections require confirmation even
where it would not otherwise be needed.

```bash
dbsync restore prod-restore /backups/app_20260727T040000.sql.gz \
  --into app_recovered --confirm app_recovered
```

**The tunnel host key changed.**
The app surfaces it and refuses to continue until someone confirms the new
fingerprint out of band. There is no `StrictHostKeyChecking=no` equivalent, on
purpose.

## Running it unattended

Three options, in increasing order of independence:

1. **The app runs its own scheduler** while it is running (including in the
   tray). Fine for a workstation.
2. **`dbsync daemon`** runs the same loop in the foreground with no GUI. This is
   what systemd or a container should run.
3. **System cron** calls `dbsync schedule run <name>`.
   `dbsync schedule crontab <name>` prints the exact line, correctly quoted.

If you use system cron, turn the app's own scheduler off in Settings so two
copies do not fire.

Full detail: [Running headless](../guides/headless.md)

## What to monitor

| Signal | Command | Non-zero means |
|---|---|---|
| Backups still restorable | `dbsync drill <profile>` | The artifact did not restore or did not match |
| Backup sizes sane | `dbsync library` | Something shrank dramatically |
| Off-site reachable | `dbsync destination test <name>` | Endpoint, credential or bucket problem |
| Schedules healthy | `dbsync schedule list` | (inspect `last_outcome`) |

A failed off-site upload **fails the job** rather than logging a warning. That is
intentional and it is the behaviour you want alerting on.

## Retention

Two policies, set separately and both optional:

- **Local**: on the schedule, by count (`keep_last`) and age (`max_age_days`).
- **Off-site**: on the destination, same two limits, `dbsync destination
  retention <name> --keep-last N --max-age-days D`. Passing neither clears it.

Nothing is deleted that a policy does not name.

## Things that will bite you once

- **A tunnelled connection's host is resolved from the bastion.** `localhost`
  means the bastion's localhost.
- **`--count-rows` costs a full scan per data table.** Without it, drills compare
  presence, not exact numbers, and say so.
- **Masking runs on the destination, after restore.** The artifact keeps the real
  data.
- **Schedules refuse destructive restore strategies.** There is nobody present at
  04:00 to confirm.
- **Imported off-site destinations arrive disabled**, because they have no
  credential yet, and an enabled destination that cannot upload fails every
  backup.

## Next

- [CLI reference](../reference/cli.md) — every command and flag
- [Troubleshooting](../guides/troubleshooting.md)
- [Tutorial: schedule and drill](../tutorials/02-schedule-and-drill.md)
