# Architecture

## The problem

A desktop app and a command-line tool that do "the same thing" usually do not.
The GUI grows logic in its event handlers, the CLI grows a different copy in its
argument parser, and the two drift until a backup taken by one cannot be restored
by the other. The version skew is discovered during a recovery.

## The approach

One engine. Two thin shells.

```
┌──────────────────────────┐     ┌──────────────────────┐
│  apps/desktop (Tauri)    │     │  engine-cli (dbsync) │
│  presentation only       │     │  cron / CI           │
└────────────┬─────────────┘     └──────────┬───────────┘
             │                              │
             │   both depend on the engine; │
             │   neither owns domain logic  │
             └───────────────┬──────────────┘
                             ▼
                 ┌───────────────────────┐
                 │  engine (Rust)        │
                 │  no tauri dependency  │
                 └───────────────────────┘
```

**The split is enforced, not aspirational.** `engine/Cargo.toml` has no `tauri`
dependency, so domain logic physically cannot reach for a window handle. If it
needs one, it is in the wrong crate.

Persistence lives in `engine::store`, so the CLI and GUI read the same profiles
and write the same job history. The rule that falls out: anything the GUI can do,
`dbsync` must be able to do.

The one deliberate exception is creating connections and SSH servers, which is
GUI-only because it can require verifying an unrecognised host key — a prompt no
cron job can answer. Everything else is headless.

## The engine's modules

| Area | Modules |
|---|---|
| Persistence and config | `store`, `profile`, `sshconn`, `plan`, `settings`, `paths`, `types` |
| Connectivity | `connect`, `ssh`, `db`, `tools`, `exec` |
| Backup and restore | `backup`, `restore`, `manifest`, `definer`, `library`, `retention` |
| Correctness | `verify`, `mask`, `crypto`, `backupkey` |
| Automation | `schedule`, `scheduler`, `cron`, `job`, `events`, `notify` |
| Distribution | `destination`, `s3`, `share` |
| Accountability | `audit` |
| Composition | `ops` — what the GUI and CLI both call |

`ops` is the seam that keeps the shells thin. A new capability that both need
belongs there, not in `commands.rs` and again in `main.rs`.

## Trade-offs

**Rust everywhere costs compile time.** Debug builds of the workspace are large
and slow; `[profile.dev]` tuning is a lever if it hurts.

**The generated TypeScript is a hard dependency on `tauri-specta`.**
`bindings.ts` cannot be hand-edited, and adding a command means regenerating. In
exchange the frontend cannot call a command with the wrong argument shape and
find out at runtime.

**One SQLite file is a single point of contention.** Two processes writing at
once is handled by SQLite, but it does mean the daemon and the app both touching
the store is a real scenario, which is why the in-app scheduler can be turned
off.

**One `Introspector`, not one per shape.** MongoDB was expected to force the
trait apart into a relational contract and a document one, and it did not. Its
vocabulary is relational — table, row, column — but what it asks for is generic:
name the containers, name the records, count them, digest them, list their
fields. A collection answers all five. `Engine::is_relational()` marks the three
places that genuinely branch, and all three generate SQL.

What did not carry over is masking. MongoDB's aggregation language has no
general-purpose hash, so `mask::mongo` is a parallel implementation rather than
a dialect: `Null` and `Constant` are one server-side `updateMany`, while the
hashing transforms read each document and compute the replacement here. The
read-back that proves masking took is identical in effect either way, which is
what keeps the guarantee intact.

**SQL Server is still not stubbed.** `BACKUP DATABASE` writes server-side, so
there is no client-side stream and no artifact of the shape every other feature
assumes. See [DECISIONS.md](../../DECISIONS.md), M14.

## The frontend

React and TypeScript, TanStack Query for server state, React Router for the
fourteen pages, Tailwind for styling. Pages are thin: they call `api.*`, which
unwraps the discriminated result from the generated bindings into promises so
Query's error states work normally.

There is no client-side domain logic. A page that needs to know something asks
the engine.

## Why commands are tested through the IPC path

The layer Tauri generates — command-name registration, camelCase argument
deserialisation, `State` extraction, response serialisation — is generated from
the same signatures `bindings.ts` is, so it is structurally hard to get wrong.
It has been wrong anyway. `tests/ipc.rs` drives real `InvokeRequest`s through a
mock runtime, so a command renamed without its call site, or a handler left out
of `generate_handler!`, fails in CI rather than in front of a user.

## Related

- [For developers](../roles/developers.md)
- [Data model](../reference/data-model.md) · [IPC API](../reference/ipc-api.md)
- [DECISIONS.md](../../DECISIONS.md) — the full decision log
