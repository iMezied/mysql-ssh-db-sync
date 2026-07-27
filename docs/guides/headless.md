# How to run headless: cron, systemd, containers

Anything the app can do, `dbsync` can do. What it deliberately cannot do is
create connections and SSH servers, because that involves verifying a host key
and no cron job can answer that prompt.

## The shape of a headless install

1. Configure connections and SSH servers **once**, in the app, on any machine.
2. `dbsync config export` a bundle.
3. On the server: `dbsync config import`, then set each password and passphrase
   from the report.
4. Run schedules with the daemon or system cron.

Credentials still go to the OS keychain, which means a headless Linux box needs a
Secret Service available — see below.

## Option 1: system cron

```bash
dbsync schedule crontab nightly-backup
```

```
0 4 * * * '/Applications/DBSync Studio.app/Contents/MacOS/dbsync' schedule run nightly-backup
```

Paste it into `crontab -e`. The quoting is handled; an unquoted path with a space
runs the wrong command.

Turn the app's own scheduler **off** in Settings so two copies do not fire.

## Option 2: the daemon

```bash
dbsync daemon --interval 30
```

Runs the same loop the app runs, in the foreground, until interrupted. Cron's
resolution is one minute, so there is nothing to gain below about 30 seconds.

### systemd unit

```ini
[Unit]
Description=DBSync scheduler
After=network-online.target

[Service]
Type=simple
User=dbsync
ExecStart=/usr/local/bin/dbsync daemon --interval 30
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl enable --now dbsync
journalctl -u dbsync -f
```

## Option 3: containers

Mount the store and the backup directory, and give the container a keychain
equivalent. On Linux that is a Secret Service (gnome-keyring), which must be
unlocked with a **non-empty** password:

```bash
dbus-run-session -- bash -c '
  echo -n "$KEYRING_PASSWORD" | gnome-keyring-daemon --unlock
  dbsync daemon
'
```

An empty password leaves gnome-keyring without a default collection and every
secret write fails with `NoStorageAccess`, which looks like a permissions problem
and is not.

## Pointing at a different store

```bash
dbsync --store /srv/dbsync/dbsync.db schedule list
```

Every command takes `--store`. Without it, the shared location the desktop app
uses is resolved.

## Machine-readable output

`--json` on any command. For long-running jobs it emits progress as JSON-lines on
stdout, which is what you want in CI:

```bash
dbsync --json backup prod | jq -r 'select(.phase) | .phase'
```

## Monitoring

Exit codes are the interface:

```bash
dbsync drill staging   || alert "backups are not restorable"
dbsync library         || alert "a backup shrank"
dbsync destination test off-site || alert "off-site unreachable"
```

## Upgrades

Both the app and the CLI apply pending migrations when they open the store, and
whichever runs first adopts any configuration an older version left behind. A
CLI reading a half-migrated store would be worse than either doing it or not, so
they do the same work.

## Troubleshooting

| Symptom | Cause |
|---|---|
| `NoStorageAccess` on every secret | Keyring unlocked with an empty password, or no Secret Service running |
| Schedules never fire | Nothing is running them; the daemon is not up |
| Runs happen twice | The app's scheduler and cron are both firing |
| "no profiles configured" | Wrong `--store`, or the bundle was never imported |
| Backup fails, "command not found" | `mysqldump` / `pg_dump` not on the service's `PATH` |

## Related

- [Scheduling](scheduling.md) · [Sharing configuration](sharing-config.md)
- [CLI reference](../reference/cli.md)
