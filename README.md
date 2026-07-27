# DBSync Studio

Cross-server database backup, restore and sync for MySQL and PostgreSQL — a
desktop app for DBAs, plus a headless CLI that does exactly the same things.

> **Status: M9.** MySQL and PostgreSQL backup and restore work end to end over
> SSH tunnels; cross-server sync runs as one job with verification and
> retention. Scheduling, packaging, encryption at rest, content verification,
> restore drills and column masking are in. See the
> [roadmap](#roadmap) for what is outstanding. The original Bash tool in this
> repo still works and is unchanged; see [Legacy tool](#legacy-tool).

---

## Documentation

Full documentation lives in **[docs/](docs/README.md)**, organised by role:

| You are | Start here |
|---|---|
| Evaluating this | [For decision makers](docs/roles/decision-makers.md) |
| Running it | [For operators](docs/roles/operators.md) · [First backup](docs/tutorials/01-first-backup.md) |
| Working on the code | [For developers](docs/roles/developers.md) · [Architecture](docs/explanation/architecture.md) |
| Reviewing it for security | [For security reviewers](docs/roles/security-reviewers.md) · [Security model](docs/explanation/security-model.md) |

Plus [concepts](docs/concepts.md), eleven [how-to guides](docs/README.md#how-to-guides--task-oriented),
and complete [CLI](docs/reference/cli.md), [IPC](docs/reference/ipc-api.md) and
[data model](docs/reference/data-model.md) reference. This README stays focused
on building and packaging the project itself.

---

## Contents

- [Documentation](#documentation)
- [Why](#why)
- [Architecture](#architecture)
- [How a backup job flows through the system](#how-a-backup-job-flows-through-the-system)
- [Development setup](#development-setup)
- [SSH servers](#ssh-servers)
- [Scheduling](#scheduling)
- [Masking](#masking)
- [Off-site destinations](#off-site-destinations)
- [Packaging](#packaging)
- [Testing](#testing)
- [Security model](#security-model)
- [Roadmap](#roadmap)
- [Legacy tool](#legacy-tool)

---

## Why

This project began as `db_migrate.sh`: a Bash script that tunnelled into a
production MySQL server, dumped all table schemas plus data for a selected
subset, and restored the result into a freshly named database on a second
server. It worked, and it had a set of failure modes worth naming, because the
design here is a direct response to them:

| Problem in the Bash tool | How this is addressed |
|---|---|
| `2>/dev/null` on the dump and restore hid every error; partial dumps looked successful | All child-process stderr is captured into the job log and surfaced |
| Passwords passed as `-p<pass>` argv, visible in `ps` | Secrets live in the OS keychain and reach children via env or 0600 files |
| Restore "verified" with `information_schema.TABLE_ROWS`, an estimate that reads 0 for freshly imported InnoDB tables | Exact `COUNT(*)` comparison, with a per-table verdict |
| `sed 's/DEFINER=[^ ]* //g'` corrupted row data containing that text | Quote-aware streaming filter, proven against a real dump |
| Tunnels tracked by `pgrep -f`, leaking orphans | Tunnels owned by a handle whose `Drop` closes them |
| Hardcoded local ports 13306/13307, colliding between runs | Ephemeral port allocation |
| `StrictHostKeyChecking=no` | Host keys pinned, changes surfaced |
| Backups never deleted | Retention policy that never deletes the only remaining backup |
| Required a local Docker MySQL container | Native client binaries discovered on the host |

Reasoning for the non-obvious choices is in [DECISIONS.md](DECISIONS.md).

## Architecture

```
┌──────────────────────────┐     ┌──────────────────────┐
│  apps/desktop            │     │  engine-cli          │
│  Tauri 2 + React + TS    │     │  `dbsync` binary     │
│  (presentation only)     │     │  (cron / CI)         │
└────────────┬─────────────┘     └──────────┬───────────┘
             │                              │
             │   both depend on the engine; │
             │   neither owns domain logic  │
             └───────────────┬──────────────┘
                             ▼
                 ┌───────────────────────┐
                 │  engine (Rust)        │
                 │  no tauri dependency  │
                 ├───────────────────────┤
                 │ store    profile      │
                 │ secrets  job/events   │
                 │ ssh      sshconn      │
                 │ db       tools        │
                 │ backup   restore      │
                 │ verify   retention    │
                 │ manifest definer      │
                 │ plan     settings     │
                 │ cron  schedule        │
                 │ scheduler  notify     │
                 │ crypto  backupkey     │
                 │ mask                  │
                 └───────────────────────┘
```

The split is enforced, not aspirational: `engine/Cargo.toml` has no `tauri`
dependency, and persistence lives in `engine::store` so the CLI and GUI read the
same profiles and write the same job history. Anything the GUI can do, `dbsync`
must be able to do.

| Path | Contents |
|---|---|
| `engine/` | All domain logic. Options, validation, persistence, secrets, filters |
| `engine/migrations/` | Versioned SQLite schema (`sqlx::migrate!`) |
| `engine-cli/` | `dbsync` — headless entry point, JSON-lines progress |
| `apps/desktop/src-tauri/` | Tauri commands, typed event bridge, app state |
| `apps/desktop/src/` | React UI. `bindings.ts` is generated — never edit it |
| `tests/fixtures/` | MySQL and PostgreSQL fixture schemas |
| `legacy tables.conf` | Importable — `plan::parse_tables_conf` reads the old format |
| `tests/*.sh` | Fixture and DEFINER round-trip verification |

## How a backup job flows through the system

1. **The UI submits a `BackupRequest`** — a `CommonBackupOptions` (database,
   per-table selections, output directory) plus a per-engine variant,
   `Mysql(..)` or `Postgres(..)`. Options that mean nothing to the other engine
   are unrepresentable rather than silently ignored.

2. **`validate()` runs before anything opens.** Engine mismatch, an empty
   selection, a row filter on a schema-only table, or parallel `pg_dump`
   without the directory format all fail here — before a tunnel or a
   destination database exists.

3. **A `JobContext` is created and registered.** It carries a
   `CancellationToken` and a durable log buffer. `JobRegistry` maps job id to
   that token, so cancelling actually propagates into child processes.

4. **A tunnel is opened if the profile needs one.** `TunnelHandle` owns the
   tunnel; the last clone dropping closes it, so an early return or panic
   cannot leak one. The local port comes from the ephemeral range.

5. **The dump streams.** For MySQL the output passes through
   `definer::strip_definers` inline — quote-aware, so `DEFINER=` inside row data
   survives — and then through streaming gzip. No uncompressed intermediate
   file is written.

6. **Progress is emitted twice.** `JobContext::emit_event` appends to the
   durable log *first*, then publishes to a lossy broadcast channel. The desktop
   app forwards those as typed `JobProgress` events; the CLI writes JSON-lines.
   A lagging consumer drops live messages and never loses log lines.

7. **A manifest is written** next to the artifact: engine, server and tool
   versions, options, table lists, size, SHA-256 and format. Restores read it to
   pick the right tool and to detect corruption before touching a destination.

8. **Retention runs**, and reports what it deleted. It never deletes the newest
   artifact, whatever the policy says.

## Development setup

Requires Rust (stable, edition 2024 — 1.85+), Node 20+, Docker for the
integration fixtures, and the MySQL client tools.

The engine shells out to `mysqldump`/`mysql` and `pg_dump`/`pg_restore`/`psql`;
they are never bundled (see [DECISIONS.md](DECISIONS.md) on the GPL
implications for `mysqldump`).

```bash
brew install mysql-client libpq        # macOS
apt install mysql-client postgresql-client   # Debian
```

Homebrew installs both keg-only, which is fine — discovery searches those
locations even though they are not on `PATH`.

**Match the PostgreSQL client major version to the server you back up.** A
newer `pg_dump` produces a dump an older server cannot restore: `pg_dump` 18
emits `SET transaction_timeout = 0`, which PostgreSQL 16 rejects. The app warns
when it detects this.

```bash
cargo build --workspace
cd apps/desktop && npm install
```

Run the desktop app with hot reload:

```bash
cd apps/desktop && npm run tauri dev
```

Run the CLI:

```bash
cargo run -p db-sync-cli -- doctor
```

`bindings.ts` is generated from the Rust command signatures. Regenerate after
changing any `#[tauri::command]`:

```bash
cargo test -p db-sync-desktop --lib export_typescript_bindings
```

## SSH servers

An SSH server is a saved record of its own, not a field on the connection that
uses it. One bastion commonly fronts a dozen databases; when its address, user
or key rotates, editing it in one place moves every connection that tunnels
through it.

Servers are managed on the **SSH servers** page in the app, and a connection
picks one from a dropdown. Any server may name another as its jump host —
chained jumps are not supported, and a route that would need one is refused when
it is saved rather than when it next runs.

```bash
dbsync ssh          # what is saved, and what tunnels through each one
```

The CLI lists them and does not create them: adding a server means verifying an
unrecognised host key, which is a prompt no cron job can answer.

Deleting a server that something still points at is refused, and the error names
every connection and jump host holding it. Cascading instead would turn a
tunnelled connection into a direct one without saying so.

**Upgrading from an earlier version needs no action.** Tunnels configured on
individual connections are adopted into saved servers the first time the app or
the CLI opens the store, matched by endpoint so the same server configured three
times becomes one record — and the key passphrase moves with it, from the
connection's keychain entry to the server's.

## Scheduling

A **schedule** binds a saved sync plan to a cron expression. The plan already
carries the source, the database and the table selection, so a schedule only
adds *when*, *where to*, and *who to tell*.

```
Sync plan  ──▶  Schedule  ──▶  backup [→ restore → verify] → retention → notify
 (what)          (when)              the same code path the buttons run
```

Expressions are standard five-field cron, plus `@hourly`, `@daily`, `@weekly`,
`@monthly` and `@yearly`. The form previews the next five real timestamps as
you type — the only reliable way to catch a mistyped expression before it backs
up at the wrong time for a month.

Two cron behaviours are worth knowing, both matching `cron(8)`:

- If **neither** the day-of-month nor the day-of-week field is `*`, they combine
  with **OR**. `0 0 13 * 5` means "the 13th, and also every Friday" — not
  "Friday the 13th".
- Six-field (Quartz-style) expressions are **rejected**, not reinterpreted.
  `0 0 2 * * *` read as five fields would mean midnight on the 2nd.

**Daylight saving.** A local-time schedule follows the wall clock, so a time
inside the spring-forward hour does not exist and does not run that day, and a
time in the autumn repeat runs once. Pick **UTC** for anything that must fire
every 24 hours exactly.

**Missed runs.** Off by default, a schedule does not make up an occurrence it
slept through — opening a laptop at 09:00 should not start a production backup
meant for 03:00. Turn on *catch up* and it makes up **one** run, however many
were missed.

**Unattended means no confirmation is possible.** A schedule cannot use a
destructive restore target; it always creates a fresh timestamped database.
This is enforced in the engine, on create and on update, and the request a
schedule builds never carries a confirmation at all.

### Keeping it running

Closing the window leaves the app in the tray and schedules keep firing; the
tray's **Quit** stops them, and says so. Settings has toggles for the in-app
scheduler, close-to-tray, and launch at login.

### Or drive it from cron

Every schedule offers a `crontab` line, and `dbsync` runs the identical code
path the app does:

```bash
dbsync backup prod                  # dump everything, honouring destinations
dbsync backup prod --schema-only audit_log --exclude sessions
dbsync backup prod --count-rows      # so a later drill can compare exactly
dbsync restore staging backup.sql.gz          # into a new timestamped database
dbsync restore staging backup.sql.gz --replace dev_app --confirm dev_app
dbsync key generate                 # create the backup encryption key
dbsync key export > key.txt         # required before any encrypted backup
dbsync key recipients age1... age1... # let teammates decrypt future backups
dbsync drill staging                # prove the newest backup restores, once
dbsync schedule add-drill "nightly drill" staging "0 4 * * *"   # every night
dbsync schedule list                # what exists, and when it next runs
dbsync schedule show nightly        # id or a unique name prefix
dbsync schedule run nightly         # once, now; non-zero exit if it failed
dbsync schedule tick                # run whatever is due, then exit
dbsync schedule crontab nightly     # a line for system cron, plus the caveats
dbsync daemon                       # the scheduler loop, headless
```

`dbsync backup` dumps every table **with its data** unless told otherwise —
the opposite of the GUI, which shows a table list and asks. A cron line cannot
ask, and a backup that silently dumped only schemas would be a file that looks
right and restores an empty database.

`dbsync restore` defaults to a new timestamped database, which cannot destroy
anything. `--replace` and `--into` can, and the engine requires `--confirm`
with the exact target name — for `--replace` always, and for `--into` when the
connection is tagged production. The check happens before any connection is
opened, so a missing confirmation costs nothing.

Pause the schedule in the app first, or both will run it. Cron reads the
expression in **local time** regardless of the schedule's setting, runs with a
bare `PATH`, and can only reach the keychain while your login session is
unlocked — `schedule crontab` prints all of this alongside the line.

Schedules are created in the app, not the CLI: the option surface is large, and
a second construction path would be a second place for the destructive-target
check to be forgotten.

### Notifications and webhooks

Native notification on failure by default; every run or never are the other
options. A webhook URL receives a JSON POST per run:

```json
{
  "event": "dbsync.run.finished",
  "schedule_name": "nightly staging refresh",
  "outcome": "success",
  "kind": "sync",
  "scheduled_for": "2026-07-27T02:30:00Z",
  "duration_seconds": 42.0,
  "source_profile": "prod-mysql",
  "dest_profile": "staging-mysql",
  "database": "app",
  "target_database": "app_staging_20260727_023004",
  "artifact_name": "app_20260727_023000.sql.gz",
  "artifact_bytes": 1048576,
  "verification": { "passed": true, "tables_checked": 12, "failures": 0, "skipped": 0 },
  "removed_artifacts": 1,
  "error": null
}
```

**Slack and Microsoft Teams are detected from the URL** and receive a rendered
message instead of the raw report — a Slack incoming webhook returns 200 for a
body it cannot render, so a raw report sent there produces no message *and* no
error. Anything else keeps the full JSON above, so a consumer that parses it is
never silently switched to a chat payload.

Profiles appear **by name only**. No host, port, username, password, key path
or directory ever leaves the machine — the artifact is named, not located.
Redirects are not followed, delivery is a single 10-second attempt, and a
failed webhook is logged against the job but never fails the run.

## Masking

Masking rewrites columns so a production copy can be handed to people who are
not cleared to see production. Rules live on the sync plan, so every schedule
running that plan inherits them.

Rules are edited on the **Masking** page in the app, or from the CLI:

```bash
dbsync mask add nightly users email --transform email
dbsync mask add nightly users ssn   --transform null
dbsync mask add nightly users notes --transform constant --value redacted
dbsync mask list nightly            # `!` marks a rule that will not run
dbsync mask sql  nightly            # exactly what the destination will execute
```

| Transform | Result | NULL |
|---|---|---|
| `hash` | salted SHA-256 in hex, `--length` to truncate | preserved |
| `email` | deterministic address at `example.invalid` | preserved |
| `phone` | deterministic number under `+1555` | preserved |
| `null` | every row NULL — fails on a NOT NULL column | — |
| `constant` | every row set to `--value`, NULLs included | overwritten |

**The backup artifact is not masked.** Masking runs on the destination, after
the restore, as SQL the destination server executes on itself; `mysqldump` and
`pg_dump` cannot apply an expression to a column. An artifact from a masked
sync is exactly as sensitive as the source — encrypt it, and do not hand it to
anyone who is only cleared to see the masked copy.

**Either the destination is masked, or it is dropped.** Every run is followed
by a read-back that counts rows without the masked shape. If the masking fails,
or the read-back finds unmasked rows, or the read-back cannot run, the sync
drops the destination database and fails. A half-masked database looks finished
and someone believes it, so it is never left standing. Masking is therefore
refused for a restore into an existing database — dropping that would destroy
data the sync did not create.

**Rules are checked against the source before the backup starts.** A rule
naming a column that does not exist protects nothing, and without that check
the operator learns about it by reading real addresses out of the dev database.
A rule on a table the plan does not copy with data is reported, not fatal:
nothing reaches the destination, so nothing is exposed.

**Deterministic, which means pseudonymisation and not anonymisation.** The same
input always produces the same output, so `users.email` and
`orders.billing_email` still join and a weekly refresh keeps stable pseudonyms.
The price is that anyone holding both the masked data and the salt can confirm
a guess. The salt lives in the operator's local app database and is never
written to the destination, so compromising the dev server does not enable that
attack.

Masked tables are excluded from deep verification. Their contents differ from
the source by design, so they are recorded as *not compared* — the same as any
other undigestable table, so masking cannot become a way for a genuinely broken
table to report success.

## Off-site destinations

A backup that only exists on the machine that made it is one failure away from
not existing — the same disk, the same laptop, the same office fire. A
destination is the second copy: every backup is uploaded to each enabled
destination as soon as it is written, before the restore half of a sync runs.

Anything speaking the S3 API works: AWS, Cloudflare R2, Backblaze B2, Wasabi,
MinIO.

```bash
# The secret access key is read from stdin, never from an argument.
dbsync destination add \
  --name off-site \
  --endpoint https://s3.eu-west-1.amazonaws.com \
  --bucket acme-backups \
  --region eu-west-1 \
  --prefix prod \
  --access-key-id AKIA... \
  --keep-last 30

dbsync destination list
dbsync destination test              # exits non-zero if any is unusable
dbsync destination push backup.sql.gz   # backfill or retry one artifact
```

```bash
dbsync config export --out team.json   # no credentials are in it
dbsync config import team.json --dry-run
dbsync config import team.json
dbsync audit                           # what was changed, and when
```

A bundle carries connections, sync plans and off-site destinations — the shape
of the work, not the ability to do it. **It contains no passwords, no SSH keys
and no access keys**, because the types it is built from have no field one
could occupy. It is safe to commit or attach to an onboarding document.
Importing matches existing records by name, creates what is missing, updates
what is there, and never removes anything the bundle omits. Imported
destinations arrive switched off, since a destination with no credential
cannot upload and an enabled one that cannot upload fails every backup.

`dbsync audit` shows what was *changed*, as distinct from `dbsync jobs`, which
shows what *ran*. A masking rule removed, a connection re-pointed at a different
host, the backup key exported, a bundle imported over the top — those are the
events worth having after an incident, and they are usually not jobs. There is
no setting to disable it: a record of sensitive changes that can be turned off
is a record nobody can rely on.

`dbsync library` summarises what is on disk — sizes, per-day growth, and any
backup that came out far smaller than the one before it. It exits non-zero on
that last one, so it works as a cron check. That failure is invisible to
everything else in the app: the artifact is valid, its checksum matches and it
restores. It is only wrong relative to yesterday.

The desktop app has the same thing under **Off-site**, with presets for S3, R2,
B2 and MinIO. The Library page's *Send off-site* button is the `push` command.
A destination with no stored credential is called out in red there, because it
looks identical to a working one in every other respect and the whole value of
having one is the belief that a second copy exists.

### What is guaranteed

- **A failed upload fails the job.** A backup that was written locally but never
  reached the destination it was configured to reach has not done what it said,
  and job history records it as failed. Silently succeeding is the exact belief
  a destination exists to prevent.
- **The manifest travels with the artifact.** An artifact in a bucket with no
  manifest can still be restored, but nothing can say whether it arrived intact.
- **The upload is read back.** A `200` means the request was accepted, not that
  the object is readable at the size that was sent. Every upload is followed by
  a `HEAD` comparing the stored size.
- **One broken destination does not stop another.** Each gets its own result;
  two off-site copies exist precisely so that losing one is survivable.
- **Retention will not run on a failed push.** If this run did not produce the
  second copy it was meant to, it has not earned the right to delete the older
  local ones.

### Credentials and transport

The secret access key lives in the OS keychain, keyed by the destination's id.
Nothing in the `destinations` table is sensitive — endpoint, bucket, region and
access key id only — so the row is safe to log, list and export, and there is no
code path that has to remember to redact it.

Plaintext `http://` endpoints are **refused** for anything but a loopback
address. SigV4 authenticates a request; it does not encrypt one, so an `http://`
destination would send both the backup and the credentials signing it across the
network in the clear. `http://localhost.evil.example` is a remote host with a
reassuring name and is refused too — the check is on the resolved host, not on
the string.

### Off-site retention

Each destination has its own policy, separate from the local one: off-site
storage is usually cheaper and is the copy that survives losing the machine, so
keeping more there than locally is the normal case. The same guarantee applies
as locally — **the newest artifact is never deleted**, whatever the policy says.
Manifests are not counted as artifacts, so `--keep-last 10` keeps ten backups
rather than five.

### What is verified, and what is not

The signing is pinned against the published AWS SigV4 `get-vanilla` vector, and
the client and the whole push path are exercised against MinIO: upload,
read-back, list with pagination, delete, multipart, cancellation, a missing
credential, a wrong credential, a missing bucket, and retention. **AWS itself,
R2, B2 and Wasabi are not exercised by any test here.** They speak the same
protocol and are expected to work, but that is an expectation rather than a
result, so it is written down instead of implied.

## Packaging

```bash
cd apps/desktop
npm run bundle                      # everything for this platform
npm run bundle -- --bundles app,dmg
```

Use `npm run bundle`, not `npm run tauri build`. It compiles `dbsync`, stages
it, and applies [`tauri.bundle.conf.json`](apps/desktop/src-tauri/tauri.bundle.conf.json)
— the overlay that puts the CLI inside the app. A plain `tauri build` produces
a working app *without* it.

The overlay exists because `externalBin` is validated by `tauri-build`, which
runs on every `cargo build`, `cargo test` and `cargo clippy` of the desktop
crate; in the base config it makes all of them fail until the CLI has been
staged. [The details are next to the config.](apps/desktop/src-tauri/README.md)

On macOS the CLI lands in `DBSync Studio.app/Contents/MacOS/dbsync`, next to
the GUI binary — Settings → *Command-line tool* links it into `~/.local/bin` so
a terminal and `cron` can find it.

That matters because every schedule offers a crontab line, and `cron` runs with
a bare `PATH`. The generated line uses an absolute path to whichever `dbsync`
is actually resolvable, rather than a bare name that would fail at 03:00.

### Signing and notarization

Nothing is stored in the repository. Both are driven entirely by environment
variables, so an unsigned build works out of the box and a signed one needs no
config change:

| Variable | Purpose |
|---|---|
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_CERTIFICATE` / `APPLE_CERTIFICATE_PASSWORD` | base64 `.p12`, for CI |
| `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` | notarization; `APPLE_PASSWORD` is an **app-specific** password |
| `WINDOWS_CERTIFICATE` / `WINDOWS_CERTIFICATE_PASSWORD` | Authenticode, for CI |

```bash
export APPLE_SIGNING_IDENTITY="Developer ID Application: … (TEAMID)"
export APPLE_ID="you@example.com" APPLE_TEAM_ID="TEAMID"
export APPLE_PASSWORD="abcd-efgh-ijkl-mnop"   # app-specific, not your Apple ID password
npm run tauri build
```

Tauri turns on the hardened runtime automatically when signing, which
notarization requires. [`entitlements.plist`](apps/desktop/src-tauri/entitlements.plist)
deliberately requests **nothing** and records why each candidate entitlement was
rejected.

> **Keychain items are bound to the code signature.** Moving from a local
> ad-hoc build to a Developer ID build — or rotating certificates — makes macOS
> treat the app as a different application, and saved database passwords prompt
> for access once more. Nothing is lost; this is macOS working correctly.

Without any credentials the build still succeeds and is ad-hoc signed: it runs
on the machine that built it, and needs right-click → Open elsewhere.

### Releasing

Push a `v*` tag and [`.github/workflows/release.yml`](.github/workflows/release.yml)
builds macOS (Apple Silicon and Intel), Windows and Linux bundles, plus a
standalone `dbsync` archive per platform, into a draft release. Signing
activates only if the corresponding repository secrets exist.

Run it from the Actions tab with *dry run* to rehearse the whole thing without
creating a tag or a draft.

Linux bundles are built on `ubuntu-22.04` on purpose: an AppImage linked
against a newer glibc will not start on an older distribution.

### Auto-update

Not wired up. It needs a signing keypair and a release endpoint to serve the
manifest, and shipping a plugin pointed at infrastructure that does not exist
would fail at runtime for every user. `bundle.createUpdaterArtifacts` is
`false`; turning it on, adding `tauri-plugin-updater` and setting the endpoint
is the whole change once there is somewhere to publish to.

## Testing

```bash
cargo test --workspace
```

Integration fixtures — two databases seeded with the things that break naive
tooling (DEFINER clauses, binary payloads, FK cycles, reserved-word and unicode
identifiers, a MyISAM table, and row data containing the literal text
`DEFINER=`):

```bash
docker compose -f docker-compose.test.yml up -d --wait mysql postgres
tests/verify-fixtures.sh
tests/verify-definer-roundtrip.sh
docker compose -f docker-compose.test.yml down -v
```

`verify-definer-roundtrip.sh` is the one worth understanding: it takes a real
`mysqldump`, confirms the raw dump is *rejected* when restoring as a user
without SUPER, confirms the filtered dump restores cleanly, and confirms rows
whose data merely mentions `DEFINER=` come back byte-identical.

The full backup/restore round-trip needs the OS keychain, so it is `#[ignore]`d
alongside the other credential-touching suites:

```bash
cargo test -p db-sync-engine --test roundtrip -- --ignored
```

It dumps the fixture through a tunnel, restores it as a user without SUPER, and
checks that binary payloads, unicode identifiers, a foreign-key cycle and rows
containing the literal text `DEFINER=` all survive.

The PostgreSQL equivalent covers all three dump formats and selective restore:

```bash
cargo test -p db-sync-engine --test roundtrip_pg -- --ignored
```

Keychain tests touch the real OS credential store, so they are `#[ignore]`d:

```bash
cargo test -p db-sync-engine --test keychain -- --ignored
```

To run **everything**, ignored suites included — which is what CI does:

```bash
docker compose -f docker-compose.test.yml up -d --wait
cargo test --workspace -- --include-ignored
```

On Linux that needs a Secret Service, which a headless runner has to be given:

```bash
dbus-run-session -- bash -c '
  echo -n "some-password" | gnome-keyring-daemon --unlock
  cargo test --workspace -- --include-ignored
'
```

The password must be **non-empty**. An empty one leaves gnome-keyring without a
default collection and every secret write fails with `NoStorageAccess`, which
looks like a permissions problem and is not.

Scheduler behaviour that needs no database server — a schedule whose
destination profile was deleted, a plan that cannot run, the one-run-at-a-time
guard, notification policy — runs in the normal suite:

```bash
cargo test -p db-sync-engine --test scheduler --test schedules
```

The scheduled path is also exercised against real containers, in `roundtrip`:
a schedule that comes due, moves data, verifies it, and enforces retention.

## Security model

- **Secrets never cross the IPC boundary.** The webview can store a secret and
  ask whether one exists. There is deliberately no command that returns one.
- **Never in argv.** Credentials reach child processes through environment
  variables or 0600 credential files, never `-p<password>`.
- **Host keys are pinned**, and a changed key is surfaced rather than silently
  accepted. There is no `StrictHostKeyChecking=no` equivalent.
- **Minimal Tauri capabilities**: `core:default` only. Dump and restore
  processes are spawned by the Rust engine, so the webview needs no shell
  permission.
- **Destructive restores require typed confirmation**, and production-tagged
  targets require it even for non-obviously-destructive strategies.
- **Masking protects the destination, not the artifact.** The backup file still
  holds the real data; a masked sync leaves the destination masked or drops it.
  See [Masking](#masking).
- **The encryption key is exported to a file, never to the webview.** Settings
  can ask for the key to be escrowed and is told where it landed; the secret
  itself never becomes a JS string. The file is created `0600` as part of
  opening it, so it is never briefly world-readable.
- **Off-site destinations store no credential.** The secret access key is in the
  keychain; the `destinations` table holds only an endpoint, bucket, region and
  access key id. Plaintext `http://` endpoints are refused for anything but
  loopback. See [Off-site destinations](#off-site-destinations).
- **A saved SSH server holds no key material.** The `ssh_connections` table
  holds a host, port, user and — for key-file auth — a *path*. The key stays on
  disk and its passphrase is in the keychain, keyed by the server's id. See
  [SSH servers](#ssh-servers).

## Roadmap

| Milestone | Scope | State |
|---|---|---|
| **M0** | Engine/CLI/GUI split, persistence, secrets, options model, DEFINER filter, verification, retention, test harness, CI | **Done** |
| **M1′** | SSH tunnels (russh) with jump hosts and host-key pinning, table introspection, test-connection | **Done** |
| **M2′** | MySQL backup and restore end to end, cancellation, backup library, verification | **Done** |
| **M3′** | PostgreSQL backup and restore, formats, parallel and selective restore | **Done** |
| **M4′** | Sync wizard, sync plans, retention enforcement | **Done** |
| **M4′** | Scheduler, tray mode, launch at login, notifications and webhooks | **Done** |
| **M5′** | Packaging: bundles, icons, bundled CLI, signing and notarization config, release workflow | **Done** |
| **M6** | Encryption at rest (age), key generation, escrow and import | **Done** |
| **M7** | Verification beyond row counts: content digests and column comparison | **Done** |
| **M8** | Restore drills — proving an artifact restores, on a schedule | **Done** |
| **M9** | Column-level masking on the destination, with a verified read-back | **Done** |
| **M10** | Off-site destinations: S3-compatible upload, credentials, off-site retention, CLI and GUI | **Done** |
| **M12** | Slack and Teams webhook rendering, library size and growth analytics | **Done** |
| **M13** | Shareable configuration export/import carrying no credentials, audit trail | **Done** |
| **M15** | Saved SSH servers, shared by reference and adopted from existing configurations | **Done** |

**M14 (MongoDB, SQL Server) is not started**, and deliberately so — a stub
would put an engine in the connection dropdown that fails behind every path.
It is not plumbing: 27 exhaustive `match` arms on `Engine`, three
engine-shaped abstractions, and an introspection contract defined in terms of
tables, rows and columns. MongoDB needs masking reimplemented as aggregation
pipelines and content verification rebuilt; SQL Server has no client-side dump
stream at all, so "what is an artifact" — the concept the manifest, checksum,
drill, off-site upload and retention are all built on — has to be answered
first. [DECISIONS.md](DECISIONS.md) records the three questions that have to be
settled before any code is written.

A drill compares exact row counts only when the backup recorded them
(`--count-rows`, or the toggle on the Backup and Schedules pages). Without
them it still catches a table that is missing, and reports one that restored
empty as *not compared* — because the manifest alone cannot tell that apart
from a table that was empty at the source.

Not in scope for v1: incremental/binlog/WAL sync, multi-user access control.
Trait seams are left where they would attach.

## Legacy tool

The original Bash tool (`db_migrate.sh`, `ui.sh`, `validate.sh`) is unchanged
and still works. It remains the supported path until M2′ reaches feature parity
for MySQL.

```bash
./db_migrate.sh dry-run      # validate configuration, touch nothing
./db_migrate.sh sync         # backup + restore
./db_migrate.sh backup       # backup only
./db_migrate.sh restore      # restore from a saved backup
```

It reads `.env` and `table.conf` from the repository root; both are git-ignored.
Its 215-entry table list imports directly into a sync plan — Sync → *Import
tables.conf* — so nothing has to be retyped.

## Licence

MIT — see [LICENSE](LICENSE).
