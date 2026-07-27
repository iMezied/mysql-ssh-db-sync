# Troubleshooting

Start here:

```bash
dbsync doctor              # store path, engine version, profile count
dbsync jobs --limit 10     # what ran, and how it ended
dbsync audit --limit 20    # what changed, and when
```

The app's **Jobs** page has the same history with per-step detail.

## Connecting

| Symptom | Likely cause | Fix |
|---|---|---|
| SSH step fails | Bastion unreachable, or auth | Test the SSH server on its own from the SSH servers page |
| "no authentication methods succeeded" | `ssh-agent` has no key, wrong key path, or passphrase not stored | `ssh-add -l`, or store the passphrase on the SSH server |
| SSH passes, tunnel fails | The bastion cannot reach the database host/port | Those fields are resolved **from the bastion** — usually `127.0.0.1` |
| Tunnel passes, database fails | Credentials, or the DB user cannot connect from the bastion's address | Check the grant's host part |
| Database passes, catalog fails | The user cannot read the catalog | Grant catalog read |
| Host key warning | First contact, or the key changed | Verify out of band. A changed key needs an explicit replace. |

## Backing up

| Symptom | Cause | Fix |
|---|---|---|
| `mysqldump: command not found` | Client tools missing | Install them; Settings reports what was found |
| Dump tool version mismatch warning | Client older than the server | Upgrade the client |
| Backup much smaller than yesterday | A table stopped being selected, or a `WHERE` filter changed | `dbsync library` flags it and exits non-zero |
| Job fails at the upload step | A destination is enabled and cannot upload | Fix the credential or disable it |
| Backup very slow with `--count-rows` | It is a full scan per data table | Drop the flag if exact drill comparison is not needed |

## Restoring

| Symptom | Cause | Fix |
|---|---|---|
| "checksum does not match the manifest" | Truncated or altered artifact | Get a good copy. Do not skip the check to get past it. |
| "you need SUPER privilege" | `DEFINER=` clauses from an external dump | `dbsync strip-definers < raw.sql > clean.sql` |
| Refuses without `--confirm` | The target can destroy data, or the connection is `prod`-tagged | Type the target name back |
| Encrypted artifact will not restore | Key missing or replaced | `dbsync key import` the right one |
| PostgreSQL `--only-table` rejected | Plain SQL dump | Needs an archive format |

## Scheduling

| Symptom | Cause | Fix |
|---|---|---|
| Never fires | Nothing is running it | App closed with tray off, no daemon, no cron |
| Fires twice | App scheduler **and** cron | Turn one off |
| Wrong hour | Timezone or DST | Set the schedule's timezone explicitly |
| "cannot use the ... strategy" | A destructive restore in a schedule | Refused by design; nobody is present to confirm |
| Missed run never happened | `catch_up` off | Turn it on if you want wake-up catch-up |

## Drills

| Symptom | Cause | Fix |
|---|---|---|
| "not compared" for every table | The backup recorded no row counts | Back up with `--count-rows` |
| Fails on restore | The artifact is not restorable — the thing drills exist to find | Check the source, then take a fresh backup |
| Scratch database left behind | Failed with `--keep-on-failure` | Inspect, then drop it |

## Off-site

| Symptom | Cause | Fix |
|---|---|---|
| Refused at creation | No credential, or plaintext `http://` | Supply the key; use `https://` |
| `test` passes, uploads fail | Credential can list but not write | Widen the policy |
| Bucket looks empty | Wrong prefix, or path-style needed | `--path-style` for MinIO and most self-hosted gateways |

## Keychain

| Symptom | Cause | Fix |
|---|---|---|
| `NoStorageAccess` (Linux) | Keyring unlocked with an **empty** password, or no Secret Service | Unlock with a non-empty password under `dbus-run-session` |
| Prompted repeatedly (macOS) | The binary changed identity | Allow it once more; a rebuilt dev binary is a different binary |
| Secrets vanished after a rebuild | Dev binary, unsigned, new identity | Expected in development; not a packaged-app behaviour |

## Import and sharing

| Symptom | Cause | Fix |
|---|---|---|
| Connections became direct | The bundle did not carry their SSH server | Import or create it, then re-point them |
| Plans skipped | Their connection is missing | Create it, re-import |
| Destinations do nothing | They arrive disabled with no credential | Add the key, then enable |

## When none of this helps

1. `dbsync --json <command>` for structured output
2. The app's Jobs page for the per-step breakdown
3. `dbsync audit` — something changed, and it says what and when
4. [DECISIONS.md](../../DECISIONS.md) — if the behaviour looks deliberate, it
   probably is, and the reasoning is written down
