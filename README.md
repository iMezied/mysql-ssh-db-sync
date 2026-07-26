# DBSync Studio

Cross-server database backup, restore and sync for MySQL and PostgreSQL — a
desktop app for DBAs, plus a headless CLI that does exactly the same things.

> **Status: M2′.** MySQL backup and restore work end to end over SSH tunnels,
> with exact-count verification. PostgreSQL support lands next — see
> [Roadmap](#roadmap). The original Bash tool in this repo still works and is
> unchanged; see [Legacy tool](#legacy-tool).

---

## Contents

- [Why](#why)
- [Architecture](#architecture)
- [How a backup job flows through the system](#how-a-backup-job-flows-through-the-system)
- [Development setup](#development-setup)
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
                 │ ssh      db  tools    │
                 │ backup   restore      │
                 │ verify   retention    │
                 │ manifest definer      │
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

The engine shells out to `mysqldump` and `mysql`; they are never bundled (see
[DECISIONS.md](DECISIONS.md) on the GPL implications). Install them with
`brew install mysql-client` on macOS or `apt install mysql-client` on Debian.
Homebrew installs keg-only, which is fine — discovery searches that location
even though it is not on `PATH`.

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

Keychain tests touch the real OS credential store, so they are `#[ignore]`d —
CI runners have no unlocked keychain. Run them locally after changing anything
credential related:

```bash
cargo test -p db-sync-engine --test keychain -- --ignored
```

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

## Roadmap

| Milestone | Scope | State |
|---|---|---|
| **M0** | Engine/CLI/GUI split, persistence, secrets, options model, DEFINER filter, verification, retention, test harness, CI | **Done** |
| **M1′** | SSH tunnels (russh) with jump hosts and host-key pinning, table introspection, test-connection | **Done** |
| **M2′** | MySQL backup and restore end to end, cancellation, backup library, verification | **Done** |
| **M3′** | PostgreSQL backup and restore, formats, parallel and selective restore | Next |
| **M4′** | Sync wizard, scheduler, notifications, retention enforcement, packaging | |

Not in scope for v1: data masking, incremental/binlog/WAL sync, cloud upload,
multi-user access control. Trait seams are left where they would attach.

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
Its table list is a good starting point for a sync plan once the GUI can import
one.

## Licence

MIT — see [LICENSE](LICENSE).
