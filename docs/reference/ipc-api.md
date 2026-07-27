# IPC API reference

The 65 commands the webview can invoke. Generated from the Rust signatures by
`tauri-specta` into `apps/desktop/src/bindings.ts`, which is **generated — never
hand-edited**.

```bash
# regenerate without launching the app
cargo test -p db-sync-desktop --lib export_typescript_bindings
```

`npm run tauri dev` also regenerates on every debug run, so the TypeScript cannot
drift from the Rust.

## Calling convention

Commands return a discriminated result rather than throwing. `src/lib/api.ts`
unwraps it into a rejected promise so TanStack Query's error states work:

```ts
const profiles = await api.listProfiles();          // throws ApiError on failure
```

`ApiError.kind` is one of: `duplicate_name`, `not_found`, `corrupt`, `invalid`,
`keychain`, `storage`, `key`.

## The rule this surface exists to keep

**No command returns a secret.** The webview can store one and ask whether one
exists. Escrow writes to a file and returns the *path*.

---

## Connections

| Command | Returns |
|---|---|
| `list_profiles` | `ConnectionProfile[]` |
| `get_profile(id)` | `ConnectionProfile` |
| `create_profile(input, dbPassword)` | `ConnectionProfile` |
| `update_profile(id, patch)` | `ConnectionProfile` |
| `delete_profile(id)` | `boolean` — purges every secret belonging to it |
| `set_profile_secret(id, kind, value)` | `null` — empty value clears |
| `profile_secret_status(id)` | `SecretStatus` — whether, never what |
| `test_connection(id)` | `ConnectionReport` — four steps, never `Err` for an unreachable server |
| `trust_host_key(hostPort, algorithm, fingerprint, replace)` | `null` |
| `list_databases(id)` / `list_tables(id, database)` | catalog reads |

`ProfileUpdate.ssh_connection_id` is doubly-optional: an omitted key leaves the
tunnel alone, an explicit `null` detaches it.

## SSH servers

| Command | Returns |
|---|---|
| `list_ssh_connections` | `SshConnection[]` |
| `create_ssh_connection(input, passphrase)` | `SshConnection` |
| `update_ssh_connection(id, patch)` | `SshConnection` — every profile follows |
| `delete_ssh_connection(id)` | `boolean` — refused while referenced, error names holders |
| `set_ssh_connection_passphrase(id, value)` | `null` |
| `ssh_connection_status(id)` | `SshConnectionStatus` — passphrase stored?, used by |
| `test_ssh_connection(id)` | `SshReport` — SSH alone, no database |

`SshConnectionUpdate.jump_host_id` carries the same presence semantics.

## Backup, restore, sync

| Command | Returns |
|---|---|
| `start_backup(...)` / `start_restore(...)` / `start_sync(...)` | job id; progress arrives as events |
| `backup_directory` | the default artifact directory |
| `list_artifacts(directory)` / `check_artifact(path)` / `delete_artifact(path)` | library operations |
| `library_stats(directory)` | sizes, growth, shrink warnings |

## Sync plans and masking

| Command | Returns |
|---|---|
| `list_sync_plans(profileId)` / `create_sync_plan` / `update_sync_plan` / `delete_sync_plan` | plan CRUD |
| `import_tables_conf(contents)` | parsed selection from legacy config |
| `set_sync_plan_masking(id, masking)` | plan, with revision bumped |
| `masking_preview(planId)` | the SQL, with the salt as a bound placeholder |

## Schedules

| Command | Returns |
|---|---|
| `list_schedules` / `get_schedule` / `create_schedule` / `update_schedule` / `delete_schedule` | schedule CRUD |
| `run_schedule_now(id)` | runs it immediately |
| `preview_cron(...)` | the next few run times |
| `crontab_line(id)` | a correctly quoted crontab line |
| `scheduler_status` | whether the in-app scheduler is running |

## Encryption

| Command | Returns |
|---|---|
| `backup_key_status` | exists?, public half, extra recipients |
| `generate_backup_key` | creates it |
| `set_backup_key_recipients(keys)` | validates each; a non-key is refused |
| `export_backup_key_to_file` | **the path**, never the key |

## Off-site destinations

| Command | Returns |
|---|---|
| `list_destinations` / `create_destination(input, secretAccessKey)` / `update_destination` / `delete_destination` | destination CRUD |
| `set_destination_credential(id, secret)` | secret goes in, never comes back |
| `test_destination(id)` | reachable + credential accepted + bucket listable |
| `push_artifact_offsite(path)` | uploads to every enabled destination |

## Configuration sharing

| Command | Returns |
|---|---|
| `export_config_to_file` | the path; the bundle carries no secrets |
| `preview_config_import(path)` | the parsed bundle |
| `import_config(path)` | `ImportReport` |

## Jobs, audit, app

| Command | Returns |
|---|---|
| `list_jobs(limit)` / `active_job_ids` / `cancel_job(id)` | job control |
| `list_audit(limit)` | configuration changes |
| `get_app_settings` / `set_app_settings(...)` | settings |
| `app_info` | engine version and store path |
| `cli_status` / `install_cli` | the bundled `dbsync` on `PATH` |

## Events

Long-running jobs emit `tauri_specta::Event` payloads rather than bare `emit`
calls, so the payload type is generated into `bindings.ts` alongside the
commands. `JobPhase` covers `initializing`, `ssh_connect`, `tunneling`,
`introspect`, `dump_schema`, `dump_data`, `compress`, `transfer`, `restore`,
`verify`, `cleanup`, `done`.

## Testing this boundary

Commands are tested **through** the IPC path, not called directly — the layer
Tauri generates (name registration, camelCase deserialisation, `State`
extraction) is where the interesting failures are.

```bash
cargo test -p db-sync-desktop --test ipc
```

## Related

- [For developers](../roles/developers.md)
- [Architecture](../explanation/architecture.md)
