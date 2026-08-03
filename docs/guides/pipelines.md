# How to build and run a pipeline

A pipeline is an ordered list of steps, saved under a name and run as one job:
back up production, restore it onto staging replacing what is there, check it
landed. It is the thing to reach for when the same chain happens more than once.

## Pipeline or sync?

| | Sync | Pipeline |
|---|---|---|
| Shape | Backup → restore, fixed | Any steps, any order |
| Saved | No — rebuilt each time | Yes, under a name |
| Can replace a database | No | Yes, with the name typed back |
| Runs unattended | Yes | Yes, once armed |

If a sync already does what you need, use it. A pipeline earns its keep when
you want the chain written down, or when the destination has to be *replaced*
rather than created alongside.

## Where a pipeline runs from

| Route | How |
|---|---|
| Desktop app | **Pipelines** page → Run |
| CLI | `dbsync pipeline run <name>` |
| Schedule | A `pipeline`-kind schedule |

Pipelines are built in the app, not on the command line. The option surface is
large, and a second construction path is a second place for the destructive
target check to be forgotten — the same reasoning that keeps schedule creation
in the app.

## Building one

**Pipelines → New pipeline**, then add steps. They run top to bottom, and data
flows the same way: a restore consumes whatever the backup above it wrote, and
a verify compares the most recent restore against the source that backup came
from. There is nothing to wire up.

| Step | What it does |
|---|---|
| Back up | Dumps a database from a connection to an artifact |
| Restore | Replays an artifact onto a connection |
| Verify | Compares the restored database against the source |
| Mask | Rewrites columns on the destination |
| Copy off-site | Sends the artifact to every enabled destination |
| Retention | Prunes the backup directory |
| Drill | Proves the newest artifact restores, into a scratch database |

Save is disabled while the chain cannot run, with the reason on screen — a
restore with no backup before it, a verify after a restore from a file, two
steps replacing the same database. The engine refuses the same shapes
independently, so a pipeline written by any route is checked the same way.

## Replacing a database

A restore step offers three targets, in increasing order of danger:

| Target | What happens |
|---|---|
| **New database each run** | Creates `{prefix}_{timestamp}`. Cannot destroy anything, and the default. |
| **Replace a database** | Drops the named database and recreates it. |
| **Into an existing database** | Restores over what is there without dropping first. |

A restore step is born non-destructive. Choosing **Replace** puts a
`destroys data` badge on the step and on the pipeline, and every run asks for
the target's name to be typed back before the Run button works. The
confirmation is cleared afterwards, so coming back to the page is never one
click from replacing a database.

The check is enforced by the engine, not the page. `dbsync pipeline run` needs
`--confirm <name>` for the same reason, and both go through the same
validation.

## Running it unattended

Cron cannot answer a prompt. A pipeline that replaces a database is refused a
schedule until it is **armed**:

1. Open the pipeline, find **Unattended runs** under Run.
2. Type the database names it replaces.
3. **Authorise.**

What is stored is those names. That is what makes arming safe to rely on:

> Editing a destructive step **disarms the pipeline**. An authorisation for
> `staging` never becomes an authorisation for `production`.

Changing the steps at all clears it, so an edit that happens to keep the same
names still costs a deliberate re-arm. Withdrawing is one click, and it leaves
the schedule in place — the next run refuses rather than the schedule
disappearing.

Once armed:

```bash
dbsync pipeline run "refresh staging"
```

runs with no arguments, and a `pipeline`-kind schedule can be created on the
Schedules page.

## Watching a run

Every run lands on its job page, which shows the steps: what ran, how long each
took, and what it produced — the artifact written, the database restored into,
how many tables were compared. A run that fails part-way marks the step that
failed with its reason and every later step as **skipped**, so "how far did it
get" is answerable without reading the log.

Selecting a step narrows the timeline to it.

## Reading it from the command line

```bash
dbsync pipeline list
dbsync pipeline show "refresh staging"
```

`show` prints the chain step by step and says up front what a run would need,
so a missing `--confirm` is not discovered halfway through a cron job.

```bash
dbsync --json pipeline run "refresh staging" --confirm staging
```

Progress arrives as JSON-lines on stdout with the outcome as the last value, so
one stream feeds a log collector. The exit code is non-zero when any step
failed or a check did not pass.

## Gotchas

- **Masking never touches the artifact.** A masked pipeline protects the
  destination; the backup file still holds the real data. Encrypt it and store
  it where the source's data would be allowed to live.
- **Masking cannot follow a restore into an existing database.** Its guarantee
  is "masked or dropped", and it will not drop a database the run did not
  create. The builder refuses the combination.
- **Nothing translates between engines.** A MySQL backup cannot be restored
  into a PostgreSQL connection, and the chain is refused before anything runs.
- **A drill inside a pipeline counts as one step.** It plans its own restore,
  check and cleanup internally; the run reports the chain's shape, not the
  drill's.

## Related

- [How to sync between servers](sync.md) — the fixed-shape, unsaved version
- [Backup and restore](backup-and-restore.md) — the target strategies in detail
- [Scheduling](scheduling.md) — cron expressions, catch-up, notifications
- [Masking](masking.md) — what it protects and what it does not
- [Concepts](../concepts.md) — pipeline, plan, artifact, drill
