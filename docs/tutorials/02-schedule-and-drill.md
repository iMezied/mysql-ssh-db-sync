# Tutorial 2: make it automatic, and prove it works

By the end of this you will have a nightly backup that runs without you, a drill
that restores it and fails loudly if it cannot, and a notification that only
fires when something is wrong. About twenty minutes.

## What you'll need

- [Tutorial 1](01-first-backup.md) finished: a working connection and one
  successful backup
- A Slack or Teams incoming webhook URL, if you want notifications. Optional.

## Step 1: create a sync plan

A schedule backs up a **plan**, not a connection, because "which tables" is a
decision you want to make once and reuse.

In the app: **Sync → New plan**. Pick your connection, pick the database, and
choose what each table does — schema and data, schema only, or excluded. Name it
`nightly` and save.

Schema-only is the right answer for large audit or log tables you want the shape
of but not the volume of.

## Step 2: schedule it

In the app: **Schedules → New schedule**.

- Name: `nightly-backup`
- Plan: `nightly`
- Cron: `0 4 * * *` (04:00 every day)
- Leave the destination connection empty — this is a backup, not a sync
- Turn on **Verify**
- Set retention: keep the last 14

The app previews the next few run times as you type the cron expression, so you
can see immediately whether you meant what you typed.

Confirm from the CLI:

```bash
dbsync schedule list
```

```
nightly-backup    sync    0 4 * * *    next: 2026-07-28 04:00    enabled
```

**That is the first real result: something will now happen without you.**

## Step 3: run it once, now, rather than waiting until 04:00

```bash
dbsync schedule run nightly-backup
```

This runs the schedule immediately and waits for it to finish, which is how you
find out that the cron expression was right and the credentials still work
*before* trusting it overnight.

```bash
dbsync jobs --limit 3
```

## Step 4: add the drill

A backup that has never been restored is a belief. Put the rehearsal on its own
timer:

```bash
dbsync schedule add-drill nightly-drill local-mysql "0 5 * * *"
```

Two arguments beyond the name: the connection to restore *into*, and the cron
expression. The drill restores the newest artifact into a scratch database it
names itself, checks it against the manifest, and drops it.

Add `--deep` to read every row rather than only counting them. It costs a full
scan of every table on both sides, so it is off by default.

Run it once to be sure:

```bash
dbsync schedule run nightly-drill
```

## Step 5: get told when it breaks

Edit the schedule in the app and paste your webhook URL into **Webhook**. Leave
the policy on **On failure**, which is the default: a nightly backup that worked
is not news.

The payload shape is inferred from the URL, so a Slack URL gets Slack blocks and
a Teams URL gets a Teams card. Profiles appear **by name only** — no host, port,
username, password, key path or directory ever leaves the machine.

## Step 6: decide what actually runs the schedule

The app runs schedules itself while it is open, including in the tray. On a
server there is no app, so use the daemon:

```bash
dbsync daemon --interval 30
```

Or drive it from system cron instead, and let the tool write the line for you:

```bash
dbsync schedule crontab nightly-backup
```

```
0 4 * * * '/Applications/DBSync Studio.app/Contents/MacOS/dbsync' schedule run nightly-backup
```

The path is quoted correctly, which matters more than it sounds — an unquoted
path with a space silently runs the wrong command.

**If you use system cron, turn the app's own scheduler off** in Settings, or two
copies will fire.

## Step 7: confirm the whole loop

```bash
dbsync schedule list      # both schedules, with last outcome
dbsync jobs --limit 5     # what ran
dbsync library            # what exists, and whether anything shrank
dbsync audit --limit 5    # what you changed while setting this up
```

## What you built

A nightly backup with 14-day retention, a nightly drill that proves the newest
artifact still restores, a notification that stays quiet unless something breaks,
and a cron line that survives a machine reboot. The drill exiting non-zero is
what turns "we have backups" into something you can put on a monitoring
dashboard.

**Next:**
- [Off-site copies](../guides/offsite.md) — a second copy, because one machine is
  one failure away
- [Masking](../guides/masking.md) — if any of this data reaches a non-production
  environment
- [Running headless](../guides/headless.md) — systemd units and containers
