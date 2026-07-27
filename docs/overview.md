# Overview

*Written for anyone. No terminal required to read this page.*

## What it is

DBSync Studio copies databases. It takes a MySQL, PostgreSQL or MongoDB database on one
server, writes a backup file you can keep, and — when you want it — puts that
data onto another server. It comes in two forms that share the same brain:

- a **desktop app**, for people who want to see what is happening
- a **command-line tool** called `dbsync`, for servers, cron jobs and CI

Both read and write the same configuration. Anything the app can do, the
command line can do, which is what makes it safe to set something up by hand and
then automate it.

## The problem it solves

Most database copying is done with a hand-rolled shell script that works on the
machine where it was written. The failure modes are well known and expensive:

- **The backup ran and produced garbage.** Nobody notices until a restore is
  needed, which is the worst possible moment to find out.
- **The backup cannot be restored.** MySQL dumps taken by a privileged user
  carry `DEFINER=` clauses that fail on restore for anyone without `SUPER`.
- **The copy is a production database with real customer data**, now sitting on
  a laptop or a staging box that a dozen people can read.
- **Credentials end up in the script**, then in the repository, then in
  everyone's shell history.
- **The one person who understood the script left.**

## What it does about each

| Problem | What DBSync does |
|---|---|
| Backup produced garbage | Every backup writes a **manifest** — engine, version, tables, sizes, checksum. A restore checks the artifact against it before touching a server. |
| Backup cannot be restored | `DEFINER=` clauses are stripped as the dump streams past, quote-aware, so rows whose *data* mentions `DEFINER=` are untouched. |
| Nobody knows if it still works | A **drill** restores the newest backup into a scratch database, checks it, drops it, and exits non-zero if anything was wrong. Put it on a timer. |
| Production data on a staging box | **Masking** rewrites named columns on the destination after a sync — emails, phone numbers, hashes, nulls. |
| Credentials in scripts | Passwords, SSH key passphrases and object-store keys live in the **OS keychain**. The application database holds none of them. |
| Copy on one machine only | **Off-site destinations** upload every artifact to S3-compatible storage as it is written. |
| Only one person understands it | Configuration **exports to a shareable file** that contains no credentials — safe to commit to a repository or attach to an onboarding doc. |

## What it deliberately does not do

Being clear about this is part of the design, not an apology.

- **No SQL Server.** MySQL, PostgreSQL and MongoDB are supported. SQL Server is on the roadmap and
  are not in the codebase. A stub that appears in the dropdown and fails behind
  every path would be worse than the gap. The reasoning is recorded in
  [DECISIONS.md](../DECISIONS.md).
- **No incremental, binlog or WAL-based replication.** Backups are full dumps.
- **No multi-user access control.** It runs as you, with your credentials.
- **A drill proves a restore, not correctness.** See
  [Verification](explanation/verification.md) for exactly what is and is not
  proven.
- **Masking protects the destination, not the artifact.** The backup file still
  holds the real data. That is a deliberate choice — the artifact is your
  recovery path, and a masked recovery path is not one.

## What a normal week looks like

1. A schedule runs at 04:00, backs up production, and writes an artifact.
2. The artifact is uploaded to off-site storage as soon as it is written.
3. Retention deletes the artifacts that have aged out, locally and off-site.
4. A drill runs at 05:00, restores last night's artifact into a scratch
   database, checks it against its manifest, and drops it.
5. Nothing notifies you, because notifications default to failures only.
6. Once a month someone runs a sync to refresh staging from production, with
   masking on, so the staging copy has no real email addresses in it.

## Where to go next

- Just want it working? [Your first backup](tutorials/01-first-backup.md).
- Evaluating it? [For decision makers](roles/decision-makers.md).
- Need to know where the secrets are? [Security model](explanation/security-model.md).
