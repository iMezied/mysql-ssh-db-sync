# Decisions

Running log of choices that are not obvious from the code, with the reasoning.
Newest section last.

---

## M0 — Foundation

### Engine / GUI / CLI split is structural, not aspirational

`engine/` has no dependency on `tauri` or `tauri-specta`, and the Cargo manifest
says so. Persistence lives in `engine::store`, not in the Tauri command layer.

The first attempt at this project put row mapping in `commands.rs`, which meant
`engine-cli` could not read a profile at all — headless parity was impossible by
construction rather than merely unimplemented. Anything the GUI can do, `dbsync`
must be able to do; the type system now enforces where that logic may live.

### External dump tools are discovered, never bundled

`mysqldump` is GPLv2. Bundling it into a distributed application imposes GPL
obligations on the whole application. Binaries are located on the host
(`PATH` plus common install locations) and may be overridden per profile via
`ToolOverrides`.

PostgreSQL's client tools are under the permissive PostgreSQL Licence and
*could* be bundled later; for now both engines use the same discovery path so
there is one code path to test.

This also removes the Docker requirement the bash predecessor had — it ran every
MySQL client command inside a local `mysql:8` container.

### Secrets never cross the IPC boundary

There is deliberately no `get_password` command. The webview can:

- write a secret (`set_profile_secret`), and
- ask whether one exists (`profile_secret_status`).

It can never read one. The previous implementation exposed a `get_password`
command returning plaintext to JavaScript; any script running in the webview
could have harvested every stored credential.

Secrets are held as `secrecy::SecretString` in memory and will be handed to
child processes via environment variables or 0600 credential files — never as
argv elements, which are world-readable via `ps`.

### keyring must be given an explicit backend per platform

`keyring` 3.x selects its credential store by *feature flag*, and with default
features it compiles to an **in-memory mock**. Nothing errors: `set_password`
succeeds, the UI reports the password saved, and the value is gone — the mock
does not even persist between `Entry` instances in the same process.

Every stored credential would have silently evaporated, and the failure would
have looked like "the app forgot my password" rather than a build
misconfiguration.

The dependency is therefore declared per-target in `engine/Cargo.toml`
(`apple-native` / `windows-native` / `sync-secret-service` + `crypto-rust`) and
deliberately *not* in `[workspace.dependencies]`, where the feature set would be
easy to copy without the target gating.

`engine/tests/keychain.rs` is the regression guard: it exercises the real
backend and fails against the mock. It is `#[ignore]`d because CI runners have
no unlocked keychain, so run it locally after touching anything credential
related:

```bash
cargo test -p db-sync-engine --test keychain -- --ignored
```

### `Option<Option<T>>` needs a custom deserializer

`ProfileUpdate::ssh` distinguishes "leave unchanged" from "clear". Serde's
default handling collapses an explicit JSON `null` into `None`, making the outer
`Option` useless — both cases arrive identically.

The `double_option` deserializer runs only when the key is *present*, so absence
falls through to `Default` (`None` = leave alone) while an explicit `null`
becomes `Some(None)` (= clear). A unit test pins both directions, because this
silently degrades rather than failing loudly.

### `u64` fields export to TypeScript as `number`

`specta-typescript` 0.0.12 refuses to export `u64`/`i64` to avoid precision
loss, and offers no global override. Byte counts and row counts are annotated
`#[specta(type = f64)]`.

`f64` represents integers exactly up to 2^53 — about 9 petabytes, or 9×10¹⁵
rows. Emitting `bigint` instead would force BigInt handling through every
arithmetic path in the UI to guard against a limit no real database reaches.

### Progress events are lossy; the job log is not

Live progress uses a `tokio::sync::broadcast` channel, which drops messages for
slow subscribers. That is the right trade for a UI feed, but it means the
channel can never be the record of what happened.

`JobContext::emit_event` appends to a durable log *before* publishing, and that
log is persisted to `job_history`. The event bridge treats `RecvError::Lagged`
as "keep going" — the previous implementation broke out of the loop on any
error, so a single burst of progress permanently froze the UI for the rest of
the session.

### Cancellation registers tokens, not senders

`JobRegistry` maps job id to `CancellationToken`. The earlier version stored the
event sender, so "cancel" emitted a message saying the job was cancelled and
removed the map entry, while the job itself carried on running.

### Tunnels are owned, not tracked by PID

`TunnelHandle` holds an `Arc` whose `Drop` cancels the tunnel, so a tunnel
cannot outlive the job that opened it — including on an early return or a panic.
The bash predecessor located tunnels with `pgrep -f` pattern matching and leaked
them whenever the pattern missed.

Local ports are allocated from the ephemeral range by binding port 0, rather
than the hardcoded 13306/13307, which collide as soon as two jobs run at once.

### Host keys are pinned, never blindly accepted

`StrictHostKeyChecking=no` is not used. An unknown host prompts with its
fingerprint and the accepted key is pinned in `known_hosts`; `remember_host` is
deliberately `ON CONFLICT DO NOTHING` so a *changed* key is detectable rather
than silently overwritten. Re-pinning requires the explicit `replace_host_key`.

### DEFINER stripping is a streaming, quote-aware filter

`mysqldump` has no flag to omit `DEFINER=`. (`--skip-definer` is a MySQL *Shell*
feature; the earlier code called it on `mysqldump` behind a version check that
was itself broken — it parsed the minor version where it meant the patch, so
the flag never fired and the bug stayed hidden.)

The predecessor's `sed 's/DEFINER=[^ ]* / /g'` over the finished file is wrong
twice: it rewrites the text inside string literals and row data, and it needs a
second full pass over a multi-gigabyte file.

`definer::strip_definers` tracks quote state (including backslash escapes and
doubled quotes) and only removes clauses found outside string literals, inline
on the stream. `tests/verify-definer-roundtrip.sh` proves it end to end against
a real `mysqldump`: the raw dump is rejected without SUPER, the filtered dump
restores cleanly, and rows whose *data* contains `DEFINER=` come back
byte-identical.

### Verification uses exact counts, never `TABLE_ROWS`

`information_schema.TABLES.TABLE_ROWS` is a planner estimate for InnoDB and
frequently reports 0 for a freshly imported table. The predecessor used it to
"verify" restores, which turned a failed restore into a green checkmark.

`verify::build_report` compares exact `COUNT(*)` values and distinguishes
missing tables, count mismatches, intentionally schema-only tables, and skipped
counts. A skipped count is reported as unknown, never as success.

### Backup options are per-engine, not a flat struct

`BackupRequest { common, engine: Mysql(..) | Postgres(..) }`. `--hex-blob` is
meaningless to `pg_dump` and `--format=custom` is meaningless to `mysqldump`; a
single flat struct would carry fields that are silently ignored half the time.
`validate()` rejects impossible combinations (parallel `pg_dump` without the
directory format, a row filter on a schema-only table) before a tunnel opens.

### PostgreSQL defaults to the custom archive format

`-Fc` is the only format supporting both selective restore and parallel
restore. Plain SQL is available but the UI has to warn that choosing it gives up
both.

### Bindings are generated by a test, not only at app startup

`export_typescript_bindings` is a `#[test]`. Generating bindings solely as a
side effect of `run()` means CI — which never launches the app — typechecks
whatever stale `bindings.ts` happens to be committed. CI regenerates and then
fails on `git diff --exit-code`.

### Migrations are versioned files from day one

`sqlx::migrate!` against `engine/migrations/`, not inline
`CREATE TABLE IF NOT EXISTS` strings. There is no upgrade path from ad-hoc DDL
once a schema exists on a user's machine, and this one is a day old.

### Release-candidate crates are pinned exactly

`specta`, `specta-typescript` and `tauri-specta` are pinned with `=`. They are
release candidates whose API has already moved under this project once —
`tauri_specta::ts` no longer exists in rc.25, which is what stopped the earlier
scaffold from compiling at all.

### Tauri capabilities grant `core:default` only (M0)

The shell plugin was removed. Dump and restore processes are spawned by the Rust
engine via `tokio::process`, so the webview needs no shell permission; granting
one would be attack surface for no benefit.

---

## M1′ — Connectivity

### Host keys are pinned between two attempts, not during one

`check_server_key` is called mid-handshake. Blocking it while a dialog waits for
a human would hold a half-open SSH connection open for as long as the user takes
to answer.

Instead an unknown key fails the attempt with `TunnelError::HostKeyUnknown`,
carrying the algorithm and fingerprint. The UI shows those, and if the user
confirms, `trust_host_key` pins the key and the test is re-run. A *changed* key
produces `HostKeyChanged` with both fingerprints and is styled very differently
in the UI — first contact is routine, a changed key is what interception looks
like — and replacing a pin requires an explicit `replace` flag.

`connect_error` deliberately does not flatten these into a generic
`Connect`. `client::connect` returns the *handler's* error type, so an early
version's blanket `map_err` destroyed exactly the fingerprint the user needed,
and the UI could only say "connection failed". Caught by a test asserting the
prompt is offered.

### MySQL catalog reads need `CONVERT`, not `CAST`

MySQL 8's data-dictionary columns report as `VARBINARY`, which will not decode
into a `String`. The obvious fix, `CAST(TABLE_NAME AS CHAR)`, *transcodes* and
mangles non-ASCII identifiers: `naïve_café` comes back as `naÃ¯ve_cafÃ©`.
`CONVERT(TABLE_NAME USING utf8mb4)` reinterprets the bytes instead.

The connection also sets `charset("utf8mb4")` explicitly. Without it, a
`COUNT(*)` against a table whose name contains non-ASCII characters fails with
"table doesn't exist".

### The fixture had to be loaded with `SET NAMES utf8mb4`

The same investigation turned up a corrupt fixture: the entrypoint sourced
`01-schema.sql` with a non-UTF-8 client charset, so the unicode table names were
*stored* double-encoded.

`tests/verify-fixtures.sh` had passed anyway, because it compared rendered text
using a client with the same mis-encoding — the corruption cancelled out on both
sides. It now compares `HEX(TABLE_NAME)` against the expected UTF-8 bytes, which
cannot cancel out, and the shell helpers pass
`--default-character-set=utf8mb4`.

### Identifiers are quoted, never interpolated

`quote_mysql_ident` / `quote_pg_ident` escape embedded quote characters. Table
names come from the catalog, but a table may legally be named
``a`; DROP DATABASE app; SELECT `1`` — interpolating one unquoted into a
`COUNT(*)` would execute it. Both are unit-tested against that exact string.

### `transactional` is a serialised field, not a method

The UI warns when a selected table is not covered by `--single-transaction`.
That rule (`storage_engine == InnoDB`, or PostgreSQL) lives in Rust and is
serialised as a field, because a method cannot cross the IPC boundary and
re-deriving it in TypeScript would let the two drift.

### SSH sessions must be closed politely, not dropped

**This was a real bug, found by a flaky test.**

OpenSSH 9.8 added `PerSourcePenalties`, on by default. The server tracks source
addresses that produce authentication failures or handshakes that end without a
clean shutdown, and for a penalty period answers further connections from that
address with `Not allowed at this time.` — before sending its version banner.

The first implementation closed a tunnel by dropping the `Handle`, which closes
the TCP socket without sending `SSH_MSG_DISCONNECT`. Every finished job looked
to the server like a session that vanished. Against a modern bastion a user
running several jobs would accrue penalties until connections were refused
outright, and the message they would see is "connection failed" with no clue
why.

`disconnect_politely` now sends `SSH_MSG_DISCONNECT` for the session, and for
the jump-host session behind it, before anything is dropped.

How it was found is worth recording, because five plausible theories were wrong
first. The symptom was that a container-backed suite starting within ~30 s of
the previous one lost about half its SSH connects to `Disconnect`. Ruled out
with measurements: suite flakiness (`tunnel` passes 12/12 across four
consecutive standalone runs), the server refusing everything (`ssh(1)` gets
20/20 parallel in the same window), `MaxStartups` (raised to 200:30:400; the
listener reported `0 of 200-400` in use), leaked sshd sessions (zero before and
after), client socket exhaustion (exactly one `TIME_WAIT`), test parallelism
(`--test-threads=2` failed identically), leaked sqlx pools (closing them
deterministically changed nothing), and PAM (`UsePAM no` changed nothing).

What settled it was dropping SSH entirely: a twelve-connection plain-TCP probe
that just reads the greeting. Idle, it got twelve banners; immediately after a
suite run, it got twelve copies of `Not allowed at this time.` That located the
refusal in the server, before any SSH exchange, and named the mechanism. The
same probe now returns banners after a run, which is how the fix was confirmed.

`engine/tests/tunnel.rs::repeated_open_and_close_stays_healthy` opens and closes
twenty tunnels in sequence, the shape that exposed this.

### The test sshd disables PerSourcePenalties

Separately from the fix above, the fixture sets `PerSourcePenalties no`.

The tunnel suite deliberately generates the exact events penalties exist to
punish — wrong keys, refused host keys, unreachable hosts. With penalties on it
blocks itself, and unrelated suites that merely run afterwards fail for reasons
that have nothing to do with what they are testing.

This is a property of the fixture, not of the product. A real bastion is
configured by whoever runs it, and the client-side half — not vanishing without
saying goodbye — is fixed in the engine rather than configured away.

### Tunnel-owning tasks must outlive the runtime that created them

A tunnel's accept loop and its forwarding tasks are spawned on the runtime that
opened it. `#[tokio::test]` builds a runtime per test and drops it on return, so
a tunnel shared between tests died with whichever test created it — every later
test then saw `0 bytes at EOF`.

The integration suites therefore use one process-lifetime runtime with an
explicit `db_test!` macro. This mirrors the application, where tunnels run on
the Tauri runtime and outlive any single query, and is the same class of bug as
the dropped-runtime pool issue from M0.

---

## M2′ — MySQL backup and restore

### Triggers are dumped after the data, not with the schema

A trigger created before its table's rows fires once per restored row. The
fixture has an `AFTER INSERT ON orders` trigger that writes to `audit_log`;
restoring two orders turned two audit rows into four.

The dump is therefore three passes — schema (with `--skip-triggers`), then
data, then triggers — which is the order `mysqldump` itself uses for a single
table. The Bash predecessor had the same defect, unnoticed because it never
compared row counts afterwards.

Found by verification, not by review. It is the clearest argument for exact
`COUNT(*)` checking: a plausible-looking restore was quietly wrong.

### Credentials reach child processes through the environment

`MYSQL_PWD`, never `-p<password>`. argv is world-readable through `ps`, so the
Bash predecessor leaked the production password to every user on the machine
for the duration of each dump.

`ToolCommand::display()` renders the command for the job log and is safe to log
precisely because secrets are not in argv. Both the backup and restore command
builders have a test asserting no argument looks like an inline password flag —
naively checking for the substring `-p` matches `--port` and `--protocol`, so
the assertion matches whole arguments.

### stderr is captured on a thread, and never discarded

Reading a child's stderr after its stdout deadlocks as soon as the stderr pipe
fills — which a chatty `mysql` client will do. A background thread collects it
and keeps the last 50 lines, which is what a failure message actually needs.

A test pushes 5000 lines through to prove the deadlock is gone.

### Cancellation kills the process group

Children are spawned with `process_group(0)` and cancelled with a negative-PID
signal, so a pipeline dies as a unit rather than leaving orphans. A test spawns
a grandchild and asserts it is gone afterwards.

A cancelled child is reported as `Cancelled`, not as a failure — "exited with
signal 15" is not something a user should have to interpret.

### A failed backup deletes its artifact

A partial `.sql.gz` looks restorable: right name, right extension, plausible
size. It is deleted on failure so the library cannot offer it. The
corresponding test asserts no `.sql.gz` survives a cancelled backup.

### The restore checksum is verified before the destination is touched

`verify_checksum` runs first, so a corrupted artifact fails without a
destination database having been created. A test truncates an artifact and
asserts the schema count on the server is unchanged.

### Restore session settings are restored afterwards

`FOREIGN_KEY_CHECKS=0`, `UNIQUE_CHECKS=0` and `AUTOCOMMIT=0` are worth 5–10x on
a large import, and the first is required for correctness — the fixture's
foreign-key cycle cannot be inserted in any order. All three are re-enabled and
committed in a postamble rather than left off for the rest of the connection.

The stream is fed line by line so that cancellation lands between statements
rather than mid-statement.

### The test user has restore privileges but not SUPER

The fixture grants `dbsync` everything a restore needs, listed explicitly.
`GRANT ALL ON *.*` would include SUPER and silently invalidate the point of the
round-trip test, which is that a DEFINER-stripped dump restores for a user who
*cannot* set a definer.

### MySQL client tools are a dependency, not a bundle

The round-trip cannot be verified without `mysqldump` and `mysql` on the host.
CI installs `mysql-client` via apt; locally they came from Homebrew, which
installs keg-only into `/opt/homebrew/opt/mysql-client/bin` — not on `PATH`.

`find_tool` searches that path explicitly, along with the other common install
locations, because a GUI app launched from Finder does not inherit the shell's
`PATH` at all. Discovery finding a keg-only install is a small proof that the
search list is doing real work.

---

## M3′ — PostgreSQL backup and restore

### One pg_dump pass, not three

MySQL needs separate schema, data and trigger passes. PostgreSQL does not:
`--exclude-table-data` keeps a table's structure while dropping its rows, which
is exactly what the UI's "schema only" selection means, and `pg_dump` already
emits constraints, indexes and triggers *after* the data. The trigger-ordering
bug that bit the MySQL path cannot arise here.

Table names are schema-qualified throughout. A bare PostgreSQL pattern matches
in every schema, so `--exclude-table-data=orders` would also strip the rows
from `reporting.orders`. The UI sends `schema.table` for both engines.

### A newer pg_dump produces a dump an older server cannot restore

`pg_dump` 18 emits `SET transaction_timeout = 0`. That parameter arrived in
PostgreSQL 17, so restoring such a dump into a 16 server aborts with
"unrecognized configuration parameter". The *dump* succeeds — only the restore
fails, potentially much later.

The original guard treated one major version of drift as fine and only warned
beyond that. It now warns for **any** client-newer-than-server gap, names the
restore hazard, and names the client version to install instead. Dumping with a
newer client is still allowed, because it is correct when the destination is
equally new.

Found by running the round-trip against a PostgreSQL 16 fixture with the
host's `pg_dump` 18. The test fixture now runs a server matching the client,
since matching versions is what the app recommends; the mismatch cases are
covered by unit tests on `check_pg_dump_compatibility`.

### Selective restore filters data entries only

`pg_restore -L` takes a filtered table of contents. Filtering table
*definitions* as well looks tempting and is a trap: a TOC line for an index or
constraint names the index, not the table it belongs to, so there is no way to
tell from `--list` output which ones belong to a table being dropped. With
`--exit-on-error` set, the first orphaned index aborts the whole restore.

So the full schema is always restored and selection controls which tables'
rows come with it. That is also the more useful reading of "restore only these
tables", and it leaves the destination structurally complete either way.

### Archive formats write themselves; plain SQL is piped

`custom` and `directory` are given `--file` and left to write their own output,
which is also what makes `--jobs` possible for the directory format. Plain SQL
goes to stdout so it can be gzipped in flight, the same as the MySQL path.

A directory archive has no single file to hash, so its manifest records no
checksum rather than a misleading one, and the restore path skips the
integrity check for that format instead of failing it.

### psql needs ON_ERROR_STOP

By default `psql` reports success having skipped statements that failed, which
would report a broken restore as a good one. Every invocation sets
`ON_ERROR_STOP=1`, and `pg_restore` gets `--exit-on-error` for the same reason.

The target database is created from a connection to `postgres`, because a
database cannot be created from inside itself.

### Target-specific dependency sections are easy to get wrong

An earlier edit inserted `libc` under `[target.'cfg(unix)'.dependencies]` and
silently swept `sha2`, `flate2`, `specta` and `russh` into that section with
it. Everything still built on macOS and Linux; the Windows CI job would have
failed on four missing crates. `tempfile` later landed inside the *Linux*
target block the same way.

Worth checking the whole manifest after any edit near a `[target....]` header —
appending to `[dependencies]` is not what a naive text insertion does.

---

## M4′ — Cross-server sync

### Sync is composed, not reimplemented

`ops::sync` calls the same `backup`, `restore` and `verify_restore` the
standalone commands use. A second dump/restore implementation tuned for the
pipeline would drift from the one the individual buttons run, and the
difference would only show up on the day someone relies on it.

The engine check comes first and fails before anything is dumped: copying
MySQL to PostgreSQL is a migration, not a sync, and nothing here translates
dialects. A test asserts no artifact is produced when the check trips.

### Retention runs after verification, and only if it passed

Deleting old backups is the last thing a sync does, and it is skipped entirely
when verification found discrepancies — a failed verification is exactly the
moment the older backups matter most.

The plan is logged before it is acted on, so the job log records precisely
which files went. Combined with the rule that the newest artifact is never
deleted, a retention policy cannot leave a user with nothing.

### A sync that restored but failed verification is not a success

Every individual step can report success while the data is wrong. The job is
recorded as `failed` when verification finds discrepancies, because the
question a user asks the history is "did it work", not "did each command exit
zero".

### The sync wizard never offers a destructive target

Standalone restore supports drop-and-recreate with typed confirmation. The
wizard deliberately does not: it always creates a fresh timestamped database.
This is the screen most likely to be aimed at production by mistake, and the
non-destructive path costs nothing.

The engine still enforces confirmation independently — a test drives
`ops::sync` with a `DropAndRecreate` target and no confirmation and asserts it
is refused, so the guarantee does not depend on the UI.

### Sync plans replace tables.conf, and can import it

The Bash tool required a hand-maintained `tables.conf`, git-ignored and
therefore easy to drift from the schema. Plans live in the store, are attached
to a profile, and carry a revision that bumps on save so a plan that changed
under a schedule is visible.

`plan::parse_tables_conf` reads the old format, matching the original loader's
behaviour down to taking the first whitespace-delimited token per line (the old
script piped it through `awk '{print $1}'`) and skipping duplicates. The
repository's own `table.conf` has 215 entries; nobody should retype those.

`missing_from` and `unlisted_in` report drift in both directions. A plan
outlives the schema it was written against, and silently backing up less than
the user believes is the failure worth preventing.

The `sync_plans` table has existed since the M0 migration and was unused until
now — schema written ahead of the code that needs it, so no migration was
required here.

## M4′ — Scheduling

### The cron parser is written here, not taken from a crate

This code decides when unattended production backups run, and its semantics
have to be the ones already in the user's head. Standard five-field cron is a
small, fully specifiable language, and its genuinely surprising parts are worth
owning and testing directly rather than trusting to a dependency's
interpretation of them.

Two behaviours are implemented deliberately, matching Vixie cron:

* **Day-of-month / day-of-week combine with OR** when neither field is a star,
  and with AND when either is. So `0 0 13 * 5` is "the 13th, and also every
  Friday", not "Friday the 13th". This astonishes people, but it is what every
  crontab on every Unix does.
* **A leading `*` is what makes a field a star**, including `*/2`. That is
  where Vixie sets its flag, so that is where we set ours.

Six- and seven-field expressions are *rejected*, not reinterpreted. A Quartz
user pasting `0 0 2 * * *` means 02:00 daily; read as five fields it becomes
midnight on the 2nd. Silently shifting every field by one is the worst
available outcome, so the error names the problem.

`@reboot` is rejected too. Accepting it and never firing would silently lose
every backup that schedule was meant to make.

### Matching is the primitive; "next run" is a scan over it

`CronExpression::matches` answers "does this wall-clock minute satisfy the
expression", exactly as `cron(8)` works. `next_after` and `prev_at_or_before`
are scans over that same predicate rather than separate arithmetic.

One implementation means the "next run at …" shown in the UI cannot disagree
with what the scheduler actually does. A separately-derived next-occurrence
calculation would eventually diverge, and the UI would confidently display a
time at which nothing happens.

The scan walks day by day and only descends into the minutes a matching day
actually names, which keeps the worst case (`0 0 29 2 *`, up to four years
out) to about 1,600 cheap date checks rather than two million minute checks.

### Daylight saving is handled by choosing, not by hoping

A local-time expression is evaluated against the wall clock:

* A time inside the **spring-forward gap does not exist**, so it does not fire
  that day. `30 2 * * *` skips the transition day in New York.
* A time in the **autumn repeat fires once**, on the first pass.

`ScheduleTimezone::Utc` exists as the escape hatch for schedules that must fire
every 24 hours exactly.

The ambiguous case compares the two candidate instants directly rather than
trusting `LocalResult::Ambiguous`'s field order: chrono orders that pair by UTC
*offset*, not by instant, so its `.earliest()` is the later instant at every
autumn transition. Verified by probe, not assumed.

The scan is generic over `chrono::TimeZone` and `chrono-tz` is a
**dev-dependency** so these can be tested against real IANA zones in both
hemispheres. This machine's own zone has had no DST since 2016, so asserting
against `Local` would have asserted nothing.

### A schedule cannot carry a destructive target

Every destructive path in the app is gated on the user typing the name of the
thing being destroyed. Nobody is at the keyboard at 03:00, so a schedule cannot
supply that confirmation — and a schedule that *could* drop a database would be
a standing instruction to destroy production on a timer.

Three layers, none of which depends on the UI:

1. `Schedule::validate` rejects `DropAndRecreate` outright.
2. The store re-validates on both create **and** update, so a safe schedule
   cannot be edited into a dangerous one.
3. `Schedule::sync_request` never populates `typed_confirmation`, so even a
   schedule that somehow reached the table would be refused by the restore
   layer.

### Due detection, and what happens when the laptop was shut

`due_at` fires for an occurrence strictly newer than `max(last_run_at,
created_at)`. Creating a nightly 03:00 backup at 09:00 must not immediately run
last night's, and the high-water mark then stops one occurrence firing twice.

The mark is stamped with the **occurrence**, not with "now". A run that starts
40 seconds late must not leave a mark 40 seconds past its own occurrence, or a
schedule finer than that lateness would skip its next tick.

Beyond a 90-second grace window an occurrence is only run if `catch_up` is on.
This is a desktop app: the machine sleeping through 03:00 is the common case,
not the exception. But catch-up makes up **one** run however many were missed —
a week away with an hourly schedule produces one backup, not 168 — and it is
off by default, because starting a production backup at 09:00 is not what "at
3am" meant.

Manual "Run now" deliberately does not move the mark at all. Testing a schedule
at 14:00 must not cancel the occurrence it was created for.

### A schedule that outlives what it points at fails loudly

`sync_plan_id` cascades: a schedule whose plan is gone can never run, and
leaving it to fail nightly would be noise.

`dest_profile_id` is deliberately **not** a foreign key. `ON DELETE SET NULL`
would silently downgrade a cross-server sync to a local-backup-only job, and
nobody would find out until they needed the replica; `ON DELETE CASCADE` would
delete the schedule outright. Instead the run fails with a message naming the
schedule and stating that nothing was backed up or restored, and notifies.

The same principle covers a corrupt `cron_expression` in the store: it is
reported as corruption rather than treated as "never run".

### One run at a time, per schedule

A backup slower than its own interval must not start a second copy of itself:
two `mysqldump`s against the same source writing into the same directory is how
a backup set gets corrupted. The scheduler claims a schedule id before spawning
and releases it when the run finishes; a second request is refused rather than
queued, because running it later would be worse than not running it.

### Webhooks carry no connection details

A webhook body leaves the machine for a URL the user has merely pasted. The
payload carries profile *names*, an outcome, a duration and a verification
summary — never a host, port, username, password or key path.

The artifact appears as its **file name only**. The full path would carry the
user's home directory, and with it their account name, to a third-party
endpoint. A test asserts the serialised payload contains neither.

Redirects are not followed: a 302 is enough for a compromised or merely
misconfigured endpoint to forward a description of the user's infrastructure to
a host they never agreed to send anything to.

Delivery is one attempt with a 10-second timeout, and a failure never fails the
run. The backup either happened or it did not, and that is already recorded;
losing the courtesy copy must not turn a good backup into a bad one. Whether it
landed is written into the job log, before the log is snapshotted.

### Notification defaults to failures only

A nightly backup that worked is not news. A failed one is the only thing the
user needs to see, and burying it under thirty successes is how it gets missed.

The engine builds the title and body; the desktop layer shows them. Showing a
native notification needs Tauri, and the engine has to stay usable from
`dbsync`.

### Closing the window must not stop the backups

With a tray icon, closing the window hides it and schedules keep running; the
tray's Quit item is labelled "Quit (stops schedules)" so the difference is
visible. Without it, "close the window" and "cancel every backup" would be the
same gesture.

A window that vanishes with no explanation reads as a crash, so the first time
it happens the app says once that it is still running. Once, not every time.

The two flags the window-close handler reads are mirrored into atomics on
`AppState`. That handler runs on the main thread, and reading them from SQLite
there would block the UI behind a connection pool a running backup may be
holding. Closing a window must never be able to hang behind a dump.

### The scheduler's shutdown token is a parameter, not a field

A cancelled `CancellationToken` stays cancelled. The desktop app stops and
starts the loop as the user toggles the scheduler, so it needs a fresh token
each time — a stale one would produce a loop that exits on its first poll and a
setting that appears to do nothing. A test asserts a restarted loop keeps
running.

### `:memory:` opens one connection, not five

Every connection to `:memory:` gets its own separate database, so a
multi-connection pool runs migrations on one connection and then serves queries
from empty ones. It surfaces as "no such table" a long way from the cause — as
it did, in a scheduler test. `Store::open` now pins an in-memory store to a
single connection so it behaves the way anyone writing one would expect.

### The CLI can run schedules but not create them

`dbsync schedule list/show/run/tick/crontab` and `dbsync daemon` cover running
schedules headlessly, under systemd, or from system cron. Creating one is done
in the app: the option surface is large, and a second construction path would be
a second place for the destructive-target check to be forgotten.

`schedule crontab` prints the line *and* the three things that actually go
wrong when people move a schedule into system cron — cron's bare `PATH`, the
keychain needing an unlocked login session, and the app's own scheduler running
the same schedule concurrently. A UTC schedule additionally warns that cron
reads the expression in local time.

Paths in that line are shell-quoted. `/Applications/DBSync Studio.app/…` is the
normal macOS case, and an unquoted crontab line there silently runs the wrong
command.

## M5′ — Packaging

### The CLI ships inside the app bundle

Every schedule offers a crontab line that invokes `dbsync`. Without the CLI in
the bundle, the first thing that line asks of the user is to go and obtain a
second binary that was never published anywhere.

`beforeBuildCommand` compiles it and stages it as a Tauri external binary, so it
lands next to the GUI executable — `Contents/MacOS/dbsync` on macOS. Settings
offers to link it somewhere a shell will look.

The generated crontab line uses an **absolute path** to whichever `dbsync` is
actually resolvable, preferring one already on `PATH` and falling back to the
bundled copy. `cron` runs with a bare `PATH` that contains neither
`~/.local/bin` nor the inside of an application bundle, so a line saying just
`dbsync` is a line that fails at 03:00 with "command not found".

Verified against a real bundle: the CLI inside `DBSync Studio.app` and the GUI
resolve the same store path, which is the whole point of the engine/CLI split.

### Installing the CLI never escalates privileges

`~/.local/bin` is preferred: it needs no privileges, is per-user, and cannot
collide with a package manager. `/usr/local/bin` and `/opt/homebrew/bin` are
offered only when they already exist and `access(2)` says they are writable —
asking the OS beats reasoning about ownership, groups and ACLs.

When nothing is writable the result carries the exact `ln -s` command instead of
a failure. Being handed a command you can read is better than being handed a
password prompt from an application that wants to write to a system directory.

An existing `dbsync` is replaced only if it is a symlink. A real file there was
put there by someone, and silently destroying it would be worse than declining.

The result also reports whether the directory is actually **on** `PATH`.
Claiming success for a link no shell will ever look at is worse than saying
nothing.

### macOS gets a template menu-bar icon, not the app tile

macOS menu-bar icons are template images: flat silhouettes the system tints for
the current appearance and inverts when the menu is open. A full-colour tile
there reads as wrong and stays dark against a dark menu bar. The tray uses a
dedicated monochrome glyph on macOS and the coloured application icon
everywhere else.

The master icon is kept as SVG and rasterised at 1024 for the `.icns`, because
the previous 512px raster went visibly soft on the gradient when macOS upscaled
it for Retina. Quick Look is the only rasteriser available on this machine and
it renders an SVG at its *declared* size without scaling small ones up, so both
source files declare a large intrinsic size and are downsampled afterwards.

### The entitlements file requests nothing

Every entitlement weakens the hardened runtime and each has to be justified by
something the app cannot do without. Notarization needs the hardened runtime,
which Tauri enables when signing; it does not need an entitlement.

`allow-jit` was rejected because the web view's JIT runs in Apple's own
`WebContent` process. `disable-library-validation` was rejected because nothing
is `dlopen`'d — `mysqldump` and `pg_dump` are separate processes with their own
signatures, not libraries loaded into ours. App Sandbox entitlements were
rejected because sandboxing would break both things the app exists to do: run
the vendor tools from wherever the user installed them, and write backups where
the user chooses.

The file exists anyway, empty and commented, so the reasoning is recorded where
someone would go looking to add one.

### Keychain items are bound to the code signature

Moving from an ad-hoc local build to a Developer ID build, or rotating
certificates, makes macOS treat the app as a different application, and saved
database passwords prompt for access again. This is documented in the README
and in the entitlements file because it looks exactly like a bug the first time
it happens, and the natural reaction — re-entering every password — is the
right one but only if you know that is what is going on.

### Signing is entirely environment-driven

No identity, certificate path or team ID appears in `tauri.conf.json`. An
unsigned build works out of the box and a signed one needs no config change,
which also means a fork or a contributor without an Apple account can build the
app without editing anything.

### No auto-updater

It needs a signing keypair and an endpoint serving an update manifest. Shipping
`tauri-plugin-updater` pointed at infrastructure that does not exist would fail
at runtime for every user, and a placeholder public key would fail signature
verification on the first check. `createUpdaterArtifacts` is `false` until
there is somewhere to publish to.

### Linux bundles are built on the oldest supported base

`ubuntu-22.04`, not `ubuntu-latest`. An AppImage linked against a newer glibc
will not start on an older distribution, and there is no runtime fix — only
building on the oldest base you intend to support.

## CI — running the suites that touch real credentials

### The credential-touching suites now run in CI

Twenty-two tests — every backup, restore, verification and scheduled run
against real servers — were `#[ignore]`d because they need an OS credential
store, and a headless runner has none. That meant the tests proving the app's
central claim never ran anywhere except a developer's laptop.

`gnome-keyring` inside `dbus-run-session` provides a Secret Service, and
`--include-ignored` runs them.

**The keyring password must be non-empty.** With an empty one gnome-keyring
starts, answers on the bus, and then fails every write with
`NoStorageAccess(NoResult)` because no default collection was created. It reads
exactly like a permissions problem and is not. Three initialisation recipes
were tried in an `ubuntu:24.04` container; only a non-empty password passed.

### PostgreSQL clients come from PGDG, not from Ubuntu

`pg_dump` refuses outright to read a server newer than itself —
`check_pg_dump_compatibility` returns `Blocked`, by design. The fixture is
`postgres:18` and Ubuntu ships a client several majors behind, so enabling the
ignored suites with the distribution package would have failed every
PostgreSQL round-trip immediately. The repository is added and
`postgresql-client-18` installed explicitly, with the codename taken from
`lsb_release -cs` so a runner-image bump does not silently pin the wrong one.

### The fixtures job builds only the crates it exercises

It ran `cargo test --workspace` without installing the GTK and webview
development packages, so it could not build the desktop crate at all — proven
by reproducing the failure in a container. Scoping it to `db-sync-engine` and
`db-sync-cli` fixes it without adding minutes of dependency installation for a
crate the `rust` job already builds and tests on all three platforms.

### CI changes are verified in a container first

The whole of the above was developed against a local `ubuntu:24.04` image
mirroring the runner, with the fixture stack reachable over the host network
and the Docker socket mounted so the tests' `docker exec` assertions work.
Committing an unverifiable CI change and iterating through red builds is slower
and leaves a worse history.

### The bundled CLI is declared in an overlay config, not the base

`externalBin` is validated by `tauri-build`, which runs on every `cargo build`,
`cargo test` and `cargo clippy` of the desktop crate — not only when bundling.
With it in `tauri.conf.json`, all of those fail with `resource path ... doesn't
exist` until someone has run `npm run bundle:cli`, which breaks the ordinary
developer loop and two CI jobs that have no reason to compile the CLI.
`bundle.resources` behaves identically; it is not specific to `externalBin`.

This was caught by a plain `cargo test --workspace` failing *after* the staged
binary was cleaned up — the earlier verification had passed only because the
directory happened to still be there from an earlier bundle. Worth recording:
a build that depends on a leftover artifact looks exactly like a build that
works.

`tauri.bundle.conf.json` carries the one line, and `npm run bundle` stages the
CLI and applies the overlay in a single command that cannot forget either step.

## M11 — Parallel MySQL dumps are refused, not ignored

`MysqlBackupOptions::parallel_threads`, `Tool::Mydumper`,
`ToolOverrides::mydumper` and `ArtifactFormat::MydumperDir` have all existed
since M0. The MySQL backend never reads the option — but
`EngineBackupOptions::artifact_format()` returns `MydumperDir` whenever it is
set, and `MydumperDir::is_directory()` is `true`.

So a silently-ignored setting produced a single gzipped file carrying a
manifest that declared it a directory, and the restore path believes the
manifest. A missing feature is a disappointment; a manifest that lies about
the shape of the artifact is a corrupted restore.

`validate` now returns `NotImplemented` for it. One test asserts the refusal,
and a second documents the reason by asserting that the format it would have
declared *is* a directory — so if that ever changes, the refusal gets
revisited rather than silently outliving its cause.

PostgreSQL's `parallel_jobs` is genuinely implemented (`pg_dump -j`, directory
format only) and is unaffected.

## M12 — Webhook shape is inferred, not configured

A Slack incoming webhook accepts a POST it cannot render and returns 200. So a
raw `RunReport` sent to one produces no message *and* no error — the worst
combination available, and one the user only discovers by noticing the channel
has been silent for a month.

The endpoint's host is already unambiguous, so it decides: `hooks.slack.com`
gets a Slack attachment, `webhook.office.com` and `logic.azure.com` get a Teams
MessageCard, everything else keeps the full report. That means no setting to
get wrong and no migration, and existing schedules start rendering properly the
moment they are upgraded.

Host matching is exact or suffix-on-a-dot-boundary. `hooks.slack.com.evil.test`
is not Slack, and a test says so.

The chat payloads carry the same guarantee as the raw one — profile names, no
hosts, ports, credentials or paths — restated as its own test, because these
are the shapes that actually get pasted into a shared channel.

## M9 — Masking runs on the destination, and the artifact is not masked

`mysqldump` and `pg_dump` cannot apply an expression to a column. There is no
flag, and no combination of flags, that makes either of them emit
`sha256(email)` instead of `email`.

That leaves two ways to build masking, and they are not close in risk.

**Write our own dump encoder.** Read rows over sqlx, apply the transform in
Rust, emit `INSERT` statements. This is the only design where the artifact
itself is masked. It is also the design where we hand-roll literal encoding for
every column type in two engines — binary, `SET`, `ENUM`, arrays, `JSONB`,
timestamps with and without zone, every character set. A bug there does not
produce a masking failure. It produces a dump that restores cleanly into a
corrupted database, which is the failure mode this project has spent its whole
history trying to eliminate.

**Ask the destination server to transform its own data.** One `UPDATE` per
table after the restore lands. The server owns type fidelity, as it already
does for every other value it stores. Nothing in this crate has to know what a
`GEOMETRY` column is.

We took the second. The cost is stated everywhere it could matter — module
docs, README, CLI output: **masking protects the destination, not the backup
file.** An artifact from a masked sync is exactly as sensitive as the source.
That is a real limitation and it is not hidden behind a reassuring word like
"secure".

### Either the destination is masked, or it does not exist

A masking `UPDATE` reporting success is not evidence that a column is
unreadable. A silent truncation on a column too narrow for the hash, a trigger
that rewrites the row, a coercion that discards the expression — none of those
raise an error. Masking that trusts its own `UPDATE` is a feature that reports
success while leaking.

So every run is followed by a read-back: one query per table counting rows that
do not have the masked shape. If any count is non-zero, if the masking
statements fail, or if the read-back itself cannot run, the caller **drops the
destination database** and fails the sync.

The alternative — leaving a half-masked database in place with a warning — is
the worst available outcome. It looks finished. Someone believes it. Dropping a
dev database is an inconvenience that is fixed by re-running; a dev database
that silently contains real customer data is a breach nobody knows about.

That guarantee is why masking refuses `IntoExisting` naming. Honouring "or it
does not exist" would mean dropping a database this sync never created.

### Rules are checked against the source before the backup starts

A rule naming `users.email` when the column is really `email_address` protects
nothing, and nothing else in the system would notice. The check runs against
the source schema before a single byte is dumped, because that is the only
point where the remedy is editing a rule rather than dropping a database
someone may already be using.

A rule on a table the plan does not copy with data is *reported, not fatal*.
Nothing reaches the destination, so nothing is exposed — but it almost always
means the plan and the rules have drifted apart, so it is surfaced (`!` in
`dbsync mask list`) rather than silently accepted.

A table that cannot be introspected is treated as unknown and stops the run.
"We could not look" is not "the column is fine".

### Deterministic, and honest about what that means

The same input produces the same output in every table and on every run. That
is not a nicety: without it `users.email` and `orders.billing_email` no longer
join, and a masked copy stops being usable for the thing dev databases are for.
It also keeps pseudonyms stable across a weekly refresh.

The price is that this is **pseudonymisation, not anonymisation**. Emails,
phone numbers and names are small, guessable domains; anyone holding both the
masked data and the salt can confirm a guess by hashing it. Calling this
"anonymised" would be a lie that someone might rely on in a compliance
conversation.

The salt is what separates those two situations, so it never reaches the
destination. It is stored in the operator's local app database and bound as a
query parameter, never interpolated. What is bound is also not the stored
secret but `sha256("dbsync/masking/v1\0" || secret)` — the destination's query
log is read by more people than the app database is, and a one-way function of
the secret is what should appear there.

Consequences of stability worth knowing: the salt is generated once and never
rotated automatically, because rotating it changes every pseudonym in every
destination at once.

### Masked tables are not deep-verified

Their contents differ from the source by design, so digesting them would report
the feature working as corruption. They are recorded as *not compared* — the
same as any table that cannot be digested for any other reason. Deliberately
not "passed": masking must not become a way for a genuinely broken table to
report success. Columns are still compared, because masking changes values and
never shape.

### Rules live on the plan, not the schedule

They describe the data, not the timing. An operator who adds a rule expects the
schedule that has been running for months to start applying it; storing rules
on the schedule would protect only the runs configured after the rule was
written. Changing them bumps the plan revision, so a schedule whose masking
moved underneath it is as visible as one whose table selection did.

Corrupt masking JSON is a `StoreError::Corrupt`, never an empty list. Reading
unparseable rules as "no masking" would hand somebody an unmasked destination
while the plan still claims the column is protected.

## M9 — The encryption key is exported to a file, not returned to the webview

`commands.rs` opens with a rule this project has held since M0: **no command
returns a secret.** A password can be written to the keychain and its presence
queried; there is deliberately no "get password". A value returned across the
IPC boundary is readable by anything running in the page.

That rule is why encryption had no GUI for three milestones. Escrowing the
backup key means getting the secret in front of a human, and the obvious
implementation — return it and render it — is precisely the thing the rule
forbids. A key sitting in a React state atom is a key in a heap snapshot, a
devtools console, and any script that gets into the page.

So `export_backup_key_to_file` writes the secret to a file and returns **the
path**. The UI can say where it went and tell the user to move it into a
password manager; the value never becomes a JS string. That keeps the M0 rule
intact rather than carving an exception into it for the one secret that most
needs protecting.

The file is opened with `mode(0o600)` as part of *creating* it, not chmodded
afterwards. The difference matters on a shared machine: a chmod after the fact
leaves a window in which the key exists at the default umask. Both the mode and
the truncation are covered by tests — truncation because an age secret is a
fixed length, so a shorter second export would otherwise leave a readable tail
of the first.

## M9 — The masking UI leads with what masking does not do

The page's first element is a warning, before the connection picker, and it
says the backup file is not masked.

That is not decoration. The whole feature exists so a production copy can be
handed to people who should not see production, and the most likely way for it
to hurt someone is a user who configures masking, sees "3 columns masked", and
concludes the artifacts in their backup folder are now safe to share. Every
other surface — module docs, README, `dbsync mask list` — says the same thing,
because the one place a person forms that belief is the place they set it up.

## M9 — App-scoped secrets need a test escape hatch

Every secret in this app is keyed by a profile id, so a test that writes one is
isolated by construction: the id is random, the entry is unique, and cleanup is
a delete on the way out.

The backup key is the exception. It is stored under a **fixed** account — the
nil UUID — because there is one per machine by design: a restore has to find it
without knowing which profile made the backup. That fixedness is correct for
the product and wrong for tests, because the keychain belongs to the machine
and not to the temporary store a test opens.

The consequence was live in this repo and went unnoticed for three milestones:
`cargo test -- --include-ignored` created a real backup key in the developer's
own login keychain and left it there. Worse, on a machine that already had one,
`ensure_exists` is idempotent — so the encryption round-trip tests would have
quietly encrypted their fixtures to the developer's actual key.

`secrets::app_scope()` reads `DBSYNC_APP_SCOPE`, so tests can point app-scoped
secrets at a disposable UUID and delete it afterwards.

### Why the override is compiled out of release builds

It decides which key encrypted backups are written to. Honouring it in a
shipped binary would let anything able to set an environment variable point the
app at an empty scope, where it would generate a fresh key and encrypt to that
instead — producing artifacts the user cannot decrypt and has no reason to
suspect are different. `cfg!(debug_assertions)` keeps it where tests are and
nowhere else.

### Two things the first attempt got wrong

**The variable is per-process, not per-thread.** Cargo runs a binary's tests on
threads, so two guards alive at once shared one variable, and whichever
finished first pulled the scope out from under the other. It surfaced as an
artifact that would not decrypt — a failure that looks exactly like the
encryption being broken. The guard now holds a mutex for its lifetime.

**`delete_all_for_profile` deliberately skips `BackupKey`**, which is right for
its real caller — deleting a profile must never destroy the key that decrypts
every artifact ever taken — and exactly wrong for cleanup, where it silently
left behind the one entry the guard existed to remove. Writing an empty value
is how the secrets layer deletes.

## M9 — Commands are tested through the IPC path, not called directly

`tauri::test::mock_builder` runs commands through real registration, argument
deserialisation, `State` extraction and response serialisation, without a
window or a built frontend. Calling the functions directly would skip every one
of those.

The layer is generated from the same signatures `bindings.ts` is, so it is
structurally hard to get wrong — but that was said about several things in this
project that turned out to be wrong. A command renamed without its call site,
or one left out of `generate_handler!`, now fails in CI rather than in front of
a user.

`generate_backup_key` is deliberately *not* among them: it writes to the
machine's keychain, and a test that creates a real key is the problem described
above rather than a test of it.
