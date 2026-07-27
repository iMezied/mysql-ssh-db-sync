# For developers

*Working on the codebase. Read [Architecture](../explanation/architecture.md)
alongside this.*

## Layout

```
engine/            Rust library. No tauri dependency. All domain logic.
  src/             ~33 modules — see Architecture for the map
  migrations/      SQLite schema, 0001..0008, applied on open
  tests/           17 integration suites
engine-cli/        The `dbsync` binary. Thin: parse args, call engine.
apps/desktop/
  src-tauri/       Tauri shell: commands, tray, events. Also thin.
  src/             React + TypeScript frontend
  src/bindings.ts  GENERATED — do not hand-edit
tests/fixtures/    Seeded MySQL, PostgreSQL and MongoDB fixtures for the round-trips
```

The split is enforced, not aspirational: `engine/Cargo.toml` has no `tauri`
dependency. If domain logic is drifting into `commands.rs` or `main.rs`, that is
the bug.

## Build and run

```bash
cargo build --workspace              # engine + CLI + desktop lib
cd apps/desktop && npm install       # once
cd apps/desktop && npm run tauri dev # app with hot-reloaded frontend
```

`npm run tauri dev` also regenerates `bindings.ts` on every debug run, so the
TypeScript can never drift from the Rust command signatures.

## The generated bindings

`apps/desktop/src/bindings.ts` is produced by `tauri-specta` from the `#[tauri::command] #[specta::specta]` functions. Regenerate without launching the app:

```bash
cargo test -p db-sync-desktop --lib export_typescript_bindings
```

Adding a command means: write it in `commands.rs`, register it in the
`generate_handler!` list in `lib.rs`, regenerate bindings, then add a wrapper in
`src/lib/api.ts`. Forget the middle step and the command exists, typechecks, and
fails at runtime — which is exactly what the IPC test suite catches.

## Testing

```bash
cargo test --workspace                       # everything that needs no credentials
cargo test -p db-sync-desktop --test ipc      # the Tauri boundary
cd apps/desktop && npm run build              # tsc --noEmit + vite build
```

Suites that touch the OS keychain or a real database server are `#[ignore]`d:

```bash
docker compose -f docker-compose.test.yml up -d --wait
cargo test --workspace -- --include-ignored
```

The fixtures are seeded with the things that break naive tooling: `DEFINER=`
clauses, binary payloads, FK cycles, reserved-word and unicode identifiers, a
MyISAM table, and rows whose *data* contains the literal text `DEFINER=`.

**Commands are tested through the IPC path, not called directly.** The layer
Tauri generates — name registration, camelCase argument deserialisation, `State`
extraction — is structurally hard to get wrong and has been wrong. `tests/ipc.rs`
drives real `InvokeRequest`s through a mock runtime.

## House conventions

- **Comments say why, not what.** The codebase is dense with rationale; match
  it. If a line needs a comment explaining what it does, rewrite the line.
- **Errors carry the fix.** "Cannot delete" is a bad error; "still used by
  `prod-eu`, `prod-us`" is the answer to the next question.
- **Nothing that could hold a secret gets a field for one.** This is enforced by
  type design, not by remembering to redact.
- **Double-optional fields carry meaning by presence.** `Option<Option<T>>` with
  a custom deserializer: omitted means "leave alone", explicit `null` means
  "remove". Both `ProfileUpdate::ssh_connection_id` and
  `SshConnectionUpdate::jump_host_id` work this way, and both have boundary
  tests.
- **Migrations are additive.** A column that stops being read is left in place
  and dropped in a later migration, so an upgrade interrupted halfway still has
  the original data.

## Adding a feature, end to end

1. Domain types and logic in `engine/src/<module>.rs`, with module docs saying
   why the shape is what it is.
2. Persistence in `engine/src/store.rs` plus a numbered migration.
3. An `ops::` entry point if both the GUI and CLI need it.
4. A `#[tauri::command]` in `apps/desktop/src-tauri/src/commands.rs`, registered
   in `lib.rs`.
5. Regenerate bindings, wrap in `api.ts`, build the page.
6. A CLI subcommand in `engine-cli/src/main.rs` if it makes sense headless.
7. Tests: engine unit and integration, plus an IPC test for the boundary.
8. Docs: this directory, plus a `DECISIONS.md` entry if you made a real choice.

## Releasing

See [README.md](../../README.md) for bundling, signing and notarization. The
short version: `npm run bundle` builds the CLI, bundles it into the app, and
produces the installers.

## Next

- [Architecture](../explanation/architecture.md)
- [Data model](../reference/data-model.md)
- [IPC API](../reference/ipc-api.md)
- [DECISIONS.md](../../DECISIONS.md) — read before proposing a redesign
