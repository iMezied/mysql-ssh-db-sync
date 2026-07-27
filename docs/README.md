# DBSync Studio documentation

Backup, restore and cross-server sync for MySQL and PostgreSQL, as a desktop
app and a headless CLI that share one engine and one database.

**New here?** Read [Overview](overview.md) first — it is written for anyone,
including people who will never open a terminal. Then pick your role below.

---

## Start with your role

| You are | Start here | Then |
|---|---|---|
| **Deciding whether to use this** | [For decision makers](roles/decision-makers.md) | [Overview](overview.md), [Security model](explanation/security-model.md) |
| **Running it day to day** (DBA, ops, SRE) | [For operators](roles/operators.md) | [First backup](tutorials/01-first-backup.md), [CLI reference](reference/cli.md) |
| **Working on the code** | [For developers](roles/developers.md) | [Architecture](explanation/architecture.md), [Data model](reference/data-model.md) |
| **Reviewing it for security or compliance** | [For security reviewers](roles/security-reviewers.md) | [Security model](explanation/security-model.md), [Verification](explanation/verification.md) |

Everyone benefits from [Concepts](concepts.md) — nine words that mean something
specific here, and getting them wrong is where the confusion starts.

---

## All documentation

### Orientation

- [Overview](overview.md) — what it does and what problem it solves, in plain language
- [Concepts](concepts.md) — connection, SSH server, plan, artifact, manifest, drill, destination
- [Roles](roles/) — routed entry points for four audiences

### Tutorials — learning by doing

- [1. Your first backup and restore](tutorials/01-first-backup.md) — install to restored database
- [2. Make it automatic, and prove it works](tutorials/02-schedule-and-drill.md) — a nightly schedule and a drill that tests it

### How-to guides — task-oriented

- [Connections and SSH servers](guides/connections-and-ssh.md)
- [Backup and restore](guides/backup-and-restore.md)
- [Sync between servers](guides/sync.md)
- [Scheduling](guides/scheduling.md)
- [Masking](guides/masking.md)
- [Encryption](guides/encryption.md)
- [Off-site copies](guides/offsite.md)
- [Library, retention and drills](guides/library-retention-drills.md)
- [Sharing configuration with a team](guides/sharing-config.md)
- [Running headless: cron, systemd, containers](guides/headless.md)
- [Troubleshooting](guides/troubleshooting.md)

### Reference — complete and factual

- [CLI](reference/cli.md) — every command and flag
- [Desktop app](reference/desktop-app.md) — every page and what it does
- [IPC API](reference/ipc-api.md) — all 65 commands the UI can call
- [Data model](reference/data-model.md) — tables, migrations, what is stored where
- [Settings, paths and artifacts](reference/settings-paths-artifacts.md)

### Explanation — why it works this way

- [Architecture](explanation/architecture.md) — the engine/CLI/GUI split and why it is enforced
- [Security model](explanation/security-model.md) — where secrets live and what never crosses a boundary
- [Verification](explanation/verification.md) — what "verified" proves, and what it does not

### Elsewhere in the repo

- [README.md](../README.md) — project README, build and packaging
- [DECISIONS.md](../DECISIONS.md) — the decision log, milestone by milestone, with the reasoning

---

## The one-paragraph version

A **connection** describes how to reach one database server, optionally through
a saved **SSH server**. A **sync plan** names the tables you care about. A
**backup** writes an **artifact** plus a **manifest** describing what went into
it. A **restore** puts it back, defaulting to a new timestamped database that
cannot destroy anything. A **schedule** runs either of those unattended. A
**drill** restores the newest artifact into a scratch database and checks it
against its manifest, because a backup nobody has restored is a belief rather
than a fact. Passwords and keys live in the OS keychain, never in the database
or a config file.
