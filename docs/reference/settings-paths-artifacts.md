# Settings, paths and artifacts

## Paths

Everything is namespaced by the bundle identifier `com.dbsync-studio.app`, which
matches `identifier` in `tauri.conf.json`. The CLI reproduces the same convention
rather than deriving its own, so both binaries resolve the same store.

| Platform | Application data directory |
|---|---|
| macOS | `~/Library/Application Support/com.dbsync-studio.app` |
| Linux | `$XDG_DATA_HOME/com.dbsync-studio.app` (or `~/.local/share/...`) |
| Windows | `%APPDATA%\com.dbsync-studio.app` |

| What | Where |
|---|---|
| Application database | `<app data>/dbsync.db` |
| Default backup directory | `<app data>/backups` |
| Secrets | OS keychain, not the filesystem |

```bash
dbsync doctor                        # resolved store path and version
dbsync --store /srv/dbsync.db ...    # override per command
dbsync backup prod --dir /backups    # override the artifact directory
```

## Settings

Four, all in the app's Settings page.

| Setting | Default | What turning it off means |
|---|---|---|
| `scheduler_enabled` | on | The app stops running schedules itself. Correct when system cron drives them; otherwise nothing fires them. |
| `close_to_tray` | on | Closing the window quits the app, which **silently stops every schedule** — the app would appear configured for backups that never happen. |
| `launch_at_login` | off | Read from the OS, not the store, since it can be changed outside the app. |
| `background_notice_shown` | — | Bookkeeping: the "closing does not quit" notice is shown once, not every time. |

## Artifacts

An artifact is the file a backup produces. Its name carries the source database
and a UTC timestamp:

```
app_20260727T040000.sql.gz
app_20260727T040000.sql.gz.age     # encrypted
```

| Variant | Produced by |
|---|---|
| `.sql` | `--no-compress` |
| `.sql.gz` | default |
| `.dump` | PostgreSQL archive format (needed for selective restore) |
| `.age` suffix | `--encrypt` |

## The manifest

Written alongside every artifact. It is what makes a restore checkable.

| Field | Notes |
|---|---|
| `manifest_version` | Format version |
| `id` | This backup's id |
| `source_profile_id` / `source_profile_name` | Where it came from |
| `engine` / `server_version` | `mysql`, `postgres` or `mongo`, and the server's version |
| `dump_tool` / `dump_tool_version` | Which binary produced it |
| `database` | Source database |
| `created_at` | UTC |
| `format` | Artifact format |
| `tables` | Every table in the backup |
| `tables_with_data` | Which of those carry rows |
| `source_row_counts` | **Only when `--count-rows` was used** |
| `options` | The options the run was given |
| `artifact_filename` / `size_bytes` | The file |
| `encrypted` / `encryption_recipients` | Encryption state |

A restore compares the artifact's checksum against the manifest before touching a
server. Without `source_row_counts`, a drill can tell that a table arrived but
cannot tell an empty restore from a table that was empty at the source, and
reports **not compared** rather than guessing.

## External tools

| Engine | Needed |
|---|---|
| MySQL | `mysqldump`, `mysql` |
| PostgreSQL | `pg_dump`, `pg_restore`, `psql` |

They are discovered on `PATH` and version-checked; the Settings page reports what
was found. Per-connection overrides exist for machines with several versions
installed.

Credentials reach these processes through environment variables or `0600`
credential files, never `-p<password>` — a credential in argv is visible in `ps`
to every user on the machine.

## Related

- [Data model](data-model.md) · [Security model](../explanation/security-model.md)
- [Verification](../explanation/verification.md)
