# Concepts

Nine words that mean something specific here. Most confusion traces back to one
of them.

---

## Connection

*Also called a profile in the code and the CLI.*

How to reach **one database server**: engine (MySQL, PostgreSQL or MongoDB), host, port,
user, an optional default database, an environment tag, and optionally which
saved SSH server to tunnel through.

A connection does **not** hold its password. The password lives in the OS
keychain, keyed by the connection's id. The application database has no column
it could occupy.

The **environment tag** (`dev`, `staging`, `prod`) is not decoration: a
destructive restore into a connection tagged `prod` requires typed confirmation
even for strategies that would not otherwise need it.

Reference: [Data model](reference/data-model.md#profiles) · Guide:
[Connections and SSH servers](guides/connections-and-ssh.md)

---

## SSH server

A saved, named SSH endpoint — host, port, user, and how to authenticate
(`ssh-agent` or a key file path). Any number of connections tunnel through the
same one.

Two things follow from it being a record rather than a field:

- Editing it once changes every connection that uses it.
- Deleting one that something still points at is **refused**, and the error
  names what is holding it.

A server may name **one** other saved server as its jump host. Chained jumps are
not supported and are refused when you save, not when the job runs at 04:00.

When a tunnel is in use, the connection's host and port are resolved **from the
SSH server**, which usually means `127.0.0.1`. This is the single most common
configuration mistake.

Guide: [Connections and SSH servers](guides/connections-and-ssh.md)

---

## Sync plan

*Also called a plan.*

A named, reusable **table selection** against one connection and one database.
Each table is either schema-and-data, schema-only, or excluded, and a
schema-and-data table may carry a `WHERE` filter.

A plan is where masking rules live, and it is what a sync-kind schedule points
at. It has a **revision** number that increments on change, so a schedule
notices that the plan it runs was edited.

Reference: [Data model](reference/data-model.md#sync_plans) · Guide:
[Sync between servers](guides/sync.md)

---

## Artifact

The **file a backup produces**. A dump, optionally compressed, optionally
encrypted, sitting in a directory you chose.

The name carries the source database and a timestamp. The file alone is not the
whole story — see manifest.

Reference: [Settings, paths and artifacts](reference/settings-paths-artifacts.md)

---

## Manifest

The record **describing an artifact**: manifest version, source connection id
and name, engine, server version, which dump tool and version produced it, the
database, when, the format, every table, which tables carry data, optional
source row counts, size, checksum, whether it is encrypted and to which
recipients.

The manifest is what makes a restore checkable. Before an artifact reaches a
server, its checksum is compared against the manifest — cheap next to a restore,
and it catches a truncated or altered file.

Row counts are recorded **only** when the backup was told to (`--count-rows`, or
the toggle in the app). Without them a drill can still tell that a table is
missing, but it cannot tell a table that restored empty from one that was empty
at the source, and says so rather than guessing.

Explanation: [Verification](explanation/verification.md)

---

## Pipeline

A **named, ordered list of steps** — back up, restore, verify, mask, copy
off-site, retain, drill — saved once and run as one job.

Distinct from a sync, which is the same idea with a fixed shape and nothing
saved. A pipeline can also *replace* a destination database, which a sync
deliberately refuses; it asks for the target's name typed back on every run,
and for a scheduled one, typed back once when it is **armed**. Editing a
destructive step disarms it, so permission is granted for a named database
rather than for a pipeline.

`dbsync pipeline list` shows them. This answers "what chains do we have, and
which of them can destroy something".

---

## Job

One **run** of something long: a backup, a restore, a sync, a drill. A job has a
lifecycle, emits progress events, can be cancelled, and leaves a durable record
in `job_history`.

`dbsync jobs` lists them. This answers "did it run, and did it work".

---

## Audit entry

A record of a **configuration change**, as distinct from a run: a masking rule
removed, a connection re-pointed at another host, the backup key exported, a
shared bundle imported.

There is no off switch. It records that a secret was set, never what it was.
`dbsync audit` lists it. This answers "who changed what", which is the question
asked after an incident and is usually not about a job at all.

---

## Drill

A **restore rehearsal**. It takes the newest artifact in a directory, restores it
into a scratch database whose name the engine chooses, checks it against its
manifest, and drops it. It exits non-zero if the restore or the check failed.

A drill cannot touch an existing database. The scratch name is generated and
nothing else is droppable.

`--deep` reads every row rather than only counting them.

Guide: [Library, retention and drills](guides/library-retention-drills.md)

---

## Destination

An **off-site copy target**: S3-compatible object storage, described by endpoint,
region, bucket, prefix and access key id. The secret access key is in the
keychain, never in the database.

Every backup is uploaded to each **enabled** destination as soon as it is
written. A failed upload fails the job, deliberately: a backup that silently
exists in only one place is the thing off-site storage was supposed to prevent.

Plaintext `http://` endpoints are refused for anything but loopback.

Guide: [Off-site copies](guides/offsite.md)

---

## Quick disambiguation

| These sound alike | Difference |
|---|---|
| Job history vs audit log | What **ran** vs what was **changed** |
| Artifact vs manifest | The **file** vs the **record describing it** |
| Backup vs sync | Write a file vs write a file **and** restore it somewhere, as one job |
| Drill vs verify | Rehearse a **restore from an artifact** vs check a **sync that just ran** |
| Connection vs SSH server | The **database** endpoint vs the **tunnel** endpoint |
| Retention (local) vs retention (destination) | What is kept **on this machine** vs **in the bucket** — set separately |
