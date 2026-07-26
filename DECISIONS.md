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

### Integration suites are sequenced, not run as one `cargo test`

**Known issue.** When a container-backed, SSH-heavy suite starts within roughly
30 seconds of the previous one, about half its SSH connects fail with
`Disconnected` before authentication. `tests/run-integration.sh` inserts a
settle gap between suites, and CI uses it instead of
`cargo test --workspace`.

What was ruled out, with measurements:

| Hypothesis | Result |
|---|---|
| A suite is simply flaky | No — `tunnel` passes 12/12 across four consecutive standalone runs |
| Server refusing connections | No — `ssh(1)` opens 20 parallel sessions in the same window, 20/20 |
| sshd `MaxStartups` throttling | No — raised to 200:30:400; listener reports `0 of 200-400` used |
| Leaked sshd sessions | No — session count is 0 before and after a run |
| Client socket exhaustion | No — exactly one `TIME_WAIT` against the published port |
| Test parallelism | No — `--test-threads=2` fails the same way |
| Leaked sqlx pools | No — closing them deterministically changed nothing |

The mechanism is unidentified and recorded rather than hidden. It has not been
observed to affect the application, which opens one tunnel per job rather than a
dozen simultaneously, and the full profile → keychain → tunnel → introspection
path passes end to end. Worth revisiting before M2′ leans harder on tunnels.

One self-inflicted aggravator *was* found and removed: adding
`LogLevel VERBOSE` to the test sshd (while trying to instrument this) made the
failure markedly worse.

### Tunnel-owning tasks must outlive the runtime that created them

A tunnel's accept loop and its forwarding tasks are spawned on the runtime that
opened it. `#[tokio::test]` builds a runtime per test and drops it on return, so
a tunnel shared between tests died with whichever test created it — every later
test then saw `0 bytes at EOF`.

The integration suites therefore use one process-lifetime runtime with an
explicit `db_test!` macro. This mirrors the application, where tunnels run on
the Tauri runtime and outlive any single query, and is the same class of bug as
the dropped-runtime pool issue from M0.
