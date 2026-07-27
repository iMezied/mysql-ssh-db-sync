# How to schedule unattended runs

## Two kinds of schedule

| Kind | What it does | Needs |
|---|---|---|
| `sync` | Back up a plan's tables, and optionally restore them to a destination | A plan |
| `drill` | Restore the newest artifact into a scratch database, check it, drop it | A connection to restore into |

A drill takes no plan — it restores whatever artifact is newest, and the artifact
already fixes what it contains.

## Create one

In the app: **Schedules → New schedule**. The cron field previews the next few
run times as you type, so a wrong expression is visible immediately.

Drills from the CLI:

```bash
dbsync schedule add-drill nightly-drill staging "0 5 * * *" [--dir PATH] [--deep]
```

## What a schedule carries

| Field | Notes |
|---|---|
| Cron | Five fields, e.g. `0 4 * * *` |
| Timezone | Explicit, so a DST shift does not move the run silently |
| Enabled | Pause without deleting |
| Output directory | Where artifacts land |
| Compress / Encrypt | Per schedule |
| Verify / Deep verify | Deep costs a full scan on both sides; off by default |
| Record row counts | Lets a later drill compare exact numbers; costs a scan |
| Retention | Keep last N, and/or delete older than D days |
| Webhook + notify policy | `never`, `on_failure` (default), `always` |
| Catch-up | Whether a run missed while the machine was asleep runs on wake |
| Keep on failure | Drills only: leave the scratch database for inspection |

**Destructive restore strategies are refused.** Nobody is present at the
scheduled time to type a confirmation.

## Inspect and run

```bash
dbsync schedule list                 # all, with next run and last outcome
dbsync schedule show nightly-backup  # one, in full
dbsync schedule run nightly-backup   # run now, wait for it, report
dbsync schedule tick                 # run anything currently due, then exit
```

`run` is how you find out the cron expression and credentials are right before
trusting them overnight.

## What actually fires them

Pick exactly one, or you get duplicate runs.

**1. The app.** It runs schedules itself while open, including in the tray.
Closing the window keeps it running by default — with that off, a closed window
silently stops every schedule, and the app would appear configured for backups
that never happen.

**2. The daemon.** Same loop, no GUI:

```bash
dbsync daemon --interval 30
```

**3. System cron.** Let the tool write the line:

```bash
dbsync schedule crontab nightly-backup
```

The path is quoted correctly, which matters: an unquoted path containing a space
runs the wrong command.

**If you use cron or the daemon, turn off the app's scheduler** in Settings.

Details for servers and containers: [Running headless](headless.md).

## Notifications

Set a webhook URL on the schedule. The payload shape is inferred from the URL —
Slack blocks for a Slack URL, a card for Teams, full JSON for anything else — so
a consumer that parses JSON is never silently switched to a chat payload.

Default policy is **on failure**: a nightly backup that worked is not news.

Profiles appear **by name only**. No host, port, username, password, key path or
directory leaves the machine. Redirects are not followed, delivery is a single
10-second attempt, and a failed webhook never fails the run.

## Troubleshooting

| Symptom | Cause |
|---|---|
| Schedule never fires | Nothing is running it: app closed with tray off, no daemon, no cron |
| Runs twice | Both the app and cron are firing it |
| Fires at the wrong hour | Timezone on the schedule, or a DST boundary |
| "a drill has no sync plan" | Drill kind with a plan set |
| "a drill needs a connection to restore into" | Drill kind with no profile |
| Missed run does not happen on wake | `catch_up` is off |

## Related

- [Tutorial 2](../tutorials/02-schedule-and-drill.md)
- [Library, retention and drills](library-retention-drills.md)
- [Running headless](headless.md)
