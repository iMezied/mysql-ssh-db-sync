# Tutorial 1: your first backup and restore

By the end of this you will have backed up a real database to a file, looked at
what that file claims about itself, and restored it into a fresh database
without touching the original. About fifteen minutes.

## What you'll need

- MySQL or PostgreSQL you can reach, with a user that can read it
- The matching client tools on your machine: `mysqldump` and `mysql`, or
  `pg_dump`, `pg_restore` and `psql`
- DBSync Studio built or installed. From a checkout:

  ```bash
  cargo build --workspace
  ```

  That gives you `target/debug/dbsync`. Add it to your `PATH`, or use the full
  path below.

## Step 1: confirm the tool can see its own store

```bash
dbsync doctor
```

```
dbsync 0.1.0
store: /Users/you/Library/Application Support/com.dbsync-studio.app/dbsync.db
profiles: 0
```

That path is the shared application database. The desktop app and the CLI both
use it, which is why anything you set up in one shows up in the other. Zero
profiles is expected.

## Step 2: create a connection

Connections are created in the desktop app, because setting one up can involve
verifying an SSH host key, and that is not a question a terminal-driven setup
should answer for you.

```bash
cd apps/desktop && npm run tauri dev
```

In the app: **Connections → New connection**. Fill in name, engine, host, port,
user, and the password. Press **Save connection**.

The password goes to your OS keychain. The application database gets everything
else.

Now confirm the CLI sees it:

```bash
dbsync profiles
```

```
9f1c...  local-mysql              mysql     dev      root@127.0.0.1:3306
```

You have a working connection. **This is the first result — everything below
builds on it.**

## Step 3: test it before you rely on it

Expand the connection's row in the app and press **Test connection**. You get
four separate results — SSH, tunnel, database, catalog — rather than one
pass/fail, because four different things can be wrong and each needs a different
fix.

Fix anything red before continuing.

## Step 4: take a backup

```bash
dbsync backup local-mysql --count-rows
```

The argument is the connection's name, or a unique prefix of it. You will see
progress, then a path:

```
wrote /Users/you/Library/Application Support/com.dbsync-studio.app/backups/app_20260727T142211.sql.gz
```

Every table was dumped with its data, because a command run from cron with no
table list means "all of it". `--count-rows` counted rows first and recorded
them, which is what lets a later drill compare exact numbers rather than only
checking that each table arrived. It costs a full scan per table.

## Step 5: look at what the artifact says about itself

```bash
dbsync library
```

```
app_20260727T142211.sql.gz   4.2 MiB   2026-07-27 14:22   app@local-mysql
```

The library reads each artifact's manifest — engine, server version, dump tool
and version, every table, sizes, checksum. This command also exits non-zero if a
backup came out dramatically smaller than the one before it, which makes it
usable as a cron check on its own.

## Step 6: restore it, safely

```bash
dbsync restore local-mysql /path/to/app_20260727T142211.sql.gz
```

With no target flags, the restore goes into a **new timestamped database**
(`app_20260727T145501` or similar). It cannot destroy anything, which is why it
is the default.

Before anything reaches the server, the artifact's checksum is compared against
its manifest. A truncated or altered file is caught here rather than halfway
through a restore.

Confirm it landed:

```bash
mysql -e "SHOW DATABASES LIKE 'app_%'"     # or: psql -c '\l'
```

## Step 7: prove it properly

```bash
dbsync drill local-mysql
```

A drill does what you just did by hand, plus the check and the cleanup: newest
artifact, restore into a scratch database the engine names, compare against the
manifest, drop it. It exits non-zero if anything was wrong.

```
drill passed: 14 tables, row counts matched
```

## What you built

A connection whose password is in the keychain, a backup artifact with a
manifest describing it, a restore into a database that could not have destroyed
anything, and a one-command rehearsal that proves the artifact is still good.

**Next:** [Tutorial 2 — make it automatic, and prove it works](02-schedule-and-drill.md),
which puts both of those on a timer.

Reference for everything you just typed: [CLI](../reference/cli.md).
