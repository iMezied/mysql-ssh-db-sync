# Data model reference

One SQLite database, nine tables, eight migrations applied on open. The desktop
app and the CLI share it; neither derives the path independently.

## Where it lives

| Platform | Path |
|---|---|
| macOS | `~/Library/Application Support/com.dbsync-studio.app/dbsync.db` |
| Linux | `$XDG_DATA_HOME/com.dbsync-studio.app/dbsync.db` (or `~/.local/share/...`) |
| Windows | `%APPDATA%\com.dbsync-studio.app\dbsync.db` |

`dbsync doctor` prints the resolved path. `--store` overrides it.

## What is never in it

Database passwords, SSH key passphrases, object-store secret keys, the backup
encryption key. All of those are in the OS keychain, keyed by the id of the row
they belong to. There is no column any of them could occupy.

---

## Tables

### `profiles`

Connections. Name (unique), engine, environment tag, `db_config` JSON (host,
port, user, optional database), `tool_overrides`, timestamps.

`ssh_connection_id` references `ssh_connections` with **ON DELETE RESTRICT** —
removing a tunnel something still uses would silently turn a tunnelled profile
into a direct one.

`ssh_config` is a legacy column: it held an inline SSH blob before saved SSH
servers existed, is no longer read as configuration, and is cleared as each
profile is adopted. Dropping it is left to a later migration so an upgrade
interrupted halfway still has the original data.

### `ssh_connections`

Saved SSH servers. Id, name (unique), `endpoint` JSON (host, port, user, auth),
`jump_host_id` self-referencing with ON DELETE RESTRICT, timestamps.

`endpoint` is JSON for the same reason the other config columns are: adding an
auth method is a new variant, not a migration. It holds no secret — a key-file
path is configuration, and the passphrase is in the keychain keyed by this row's
id.

### `sync_plans`

Named table selections. Profile id, name, database, `selections` JSON (per table:
mode and optional `WHERE` filter), `masking` JSON, revision, timestamps.

Revision increments on change so a schedule notices the plan under it was edited.

### `schedules`

Kind (`sync` or `drill`), optional plan id, optional destination profile id, cron
expression, timezone, enabled, `action` JSON (output directory, compress,
encrypt, backup options, optional restore options, verify, deep verify,
retention, record row counts, keep on failure), webhook URL, notify policy,
catch-up, last run at, last outcome, last job id.

### `job_history`

What **ran**. Durable record per job: kind, profile, outcome, timings, artifact.

### `audit_log`

What was **changed**. Action (a closed enum, not free strings), subject, detail,
timestamp. No off switch, no column to disable it. Records that a secret was set,
never its value.

### `destinations`

Off-site targets. Name, kind JSON (S3: endpoint, region, bucket, prefix, path
style, access key id), enabled, retention JSON, timestamps. Holds nothing
sensitive — the secret access key is in the keychain.

### `known_hosts`

Pinned SSH host keys: host and port, algorithm, fingerprint. A changed key is a
distinct case requiring an explicit replace.

### `app_settings`

Scheduler enabled, close to tray, background notice shown. `launch_at_login` is
read from the OS rather than from here, since the user can change it outside the
app.

---

## Migrations

| File | Adds |
|---|---|
| `0001_init.sql` | profiles, sync_plans, job_history, known_hosts |
| `0002_schedules.sql` | schedules |
| `0003_app_settings.sql` | app_settings |
| `0004_masking.sql` | masking on plans |
| `0005_destinations.sql` | destinations |
| `0006_drill_schedules.sql` | drill-kind schedules |
| `0007_audit.sql` | audit_log |
| `0008_ssh_connections.sql` | ssh_connections, `profiles.ssh_connection_id` |

Applied automatically when either binary opens the store. Both also run
`sshconn::adopt_legacy_configs` at startup — whichever is opened first performs
the upgrade and the other finds nothing to do, because a CLI reading a
half-migrated store is worse than either doing it or not.

### Migration conventions

- **Additive.** A column that stops being read stays until a later migration.
- **JSON for shapes that grow variants.** Adding an auth method or a transform
  should not be a schema change.
- **`ON DELETE RESTRICT`, never CASCADE**, for references whose silent removal
  would change behaviour rather than just delete a row.

## Keychain entries

| Secret | Keyed by |
|---|---|
| Database password | Profile id |
| SSH key passphrase | **SSH connection** id |
| Object-store secret key | Destination id |
| Backup encryption key | A fixed app-scoped account (the nil UUID) |

The backup key's fixed account is why it is findable without a profile, and why
tests that would create one are `#[ignore]`d rather than run against a
developer's login keychain.

## Related

- [Security model](../explanation/security-model.md)
- [Settings, paths and artifacts](settings-paths-artifacts.md)
