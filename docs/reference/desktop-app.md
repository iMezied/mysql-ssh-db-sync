# Desktop app reference

DBSync Studio. Eleven pages, one per job to be done.

## Connections

Every connection: name, environment tag, engine, `user@host:port/database`, and
`via <ssh server>` when tunnelled. A key icon shows whether a password is stored.

Expanding a row gives:

- **SSH tunnel** — re-point an existing connection at a different saved server,
  or detach it. Re-test after changing this; the host field means a different
  thing on each side of it.
- **Test connection** — four steps reported separately: SSH, tunnel, database,
  catalog. An unrecognised or changed host key is surfaced here with its
  fingerprint and an explicit trust action.

Creating one asks for name, engine, environment, host, port, user, optional
database, an optional saved SSH server, and a password that goes to the keychain.

## SSH servers

Saved SSH endpoints, reused by any number of connections. Each row shows
`user@host:port`, the auth kind, its jump host, and how many connections use it.

Expanding gives **Test SSH** (connect and authenticate with no database
involved), the list of profiles and jump hosts using it, and a passphrase field
for key-file auth.

Deleting one that is still referenced is refused, and the error names every
holder.

## Backup

Pick a connection and a database, choose tables, choose compression, encryption
and whether to record row counts, then run. Progress is per phase; the job can be
cancelled.

## Restore

Pick an artifact and a target connection. Three strategies: new timestamped
database (default, destroys nothing), into an existing database, or replace.
The latter two require typing the target name back; a `prod`-tagged connection
requires it either way.

## Sync

Sync plans and runs. A plan names a connection, a database and a table selection
— schema-and-data, schema-only, or excluded, with an optional `WHERE` filter per
data table. Running a sync backs up and restores in one job, with optional
verification.

An existing `tables.conf` can be imported rather than re-ticked.

## Schedules

Schedules of both kinds — `sync` and `drill`. The cron field previews the next
few run times as you type. Per schedule: timezone, enabled, output directory,
compression, encryption, verify and deep verify, row counts, retention, webhook
and notification policy, catch-up.

Destructive restore strategies are refused here.

## Masking

Rules per plan, with a preview of the exact SQL a run would send and a list of
rules that are **inert** because their table is schema-only. Transforms: hash,
email, phone, null, constant.

## Library

Artifacts on disk with sizes and growth over time, plus a warning when a backup
came out dramatically smaller than the one before it. Per artifact: check,
delete, push off-site.

## Off-site

S3-compatible destinations: endpoint, region, bucket, prefix, access key id, and
a credential that goes to the keychain. Test, enable, disable, set retention, and
push an existing artifact.

Plaintext `http://` endpoints are refused for anything but loopback. A
destination with no credential is refused before it is stored.

## Jobs

Run history with per-step detail and outcomes. Cancel a running job here.

## Settings

- **Scheduler enabled** — whether the app runs schedules itself. On by default.
  Turn it off if system cron drives them, so two copies do not fire.
- **Close to tray** — on by default. With it off, closing the window silently
  stops every schedule.
- **Launch at login** — read from the OS, since it can be changed outside the app.
- **Backup key** — status, generate, escrow to a file, additional recipients.
- **Shared configuration** — export a bundle, preview and import one.
- **CLI** — install the bundled `dbsync` onto your `PATH`.
- **Tool status** — which client binaries were found and their versions.

## Tray

The app can close to the tray and keep running schedules. A one-time notice
explains this the first time, not every time.

## Related

- [CLI reference](cli.md) — the same capabilities headless
- [IPC API](ipc-api.md) — what the UI actually calls
- [Settings, paths and artifacts](settings-paths-artifacts.md)
