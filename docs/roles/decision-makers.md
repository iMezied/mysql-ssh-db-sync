# For decision makers

*No terminal required. Read [Overview](../overview.md) first if you have not.*

## What you are buying into

A desktop app plus a command-line tool that back up, restore and copy MySQL and
PostgreSQL databases, with the operational safety rails that hand-rolled scripts
usually lack: checked restores, rehearsals on a timer, credentials in the OS
keychain, data masking for non-production copies, and an audit trail of
configuration changes.

It runs **on your machines**, against **your databases**, with **your
credentials**. There is no service, no account, and no telemetry. Nothing leaves
the machine except backups you explicitly configure to go to object storage you
control.

## The three risks it is built to remove

**1. The backup that was never a backup.**
Every artifact carries a manifest and a checksum, and a scheduled *drill*
restores the newest one into a scratch database on a timer and fails loudly if
it cannot. The question "are our backups good" gets a yes or no from a cron job
rather than a hopeful shrug.

**2. Production data spreading.**
Masking rewrites named columns on the destination of a sync — email addresses,
phone numbers, hashes, nulls — so a refreshed staging environment is not a full
copy of customer data. This is enforced at the point of restore, with a
read-back that proves the rewrite happened.

**3. Credentials in scripts.**
Database passwords, SSH key passphrases and object-store secret keys are held in
the OS keychain. The application's own database has no column that could hold
one, and configuration exported for a colleague is built from types that have no
field a secret could occupy. It is safe to commit.

## What it costs to run

- **No licence or subscription.** It is your codebase.
- **Storage**: full dumps, optionally compressed. Retention policies cap what is
  kept, locally and off-site, by count and by age.
- **Time**: a scheduled backup and a nightly drill are unattended. Set-up is a
  connection, a table selection and a cron expression.
- **A person**: someone has to read a failure notification. Notifications go to
  Slack or Teams via a webhook and default to failures only.

## Where the limits are

Stated plainly, because discovering them later is expensive:

- **No SQL Server.** MySQL, PostgreSQL and MongoDB are supported. SQL Server is roadmap, not
  product, and deliberately not stubbed.
- **Full dumps, not continuous replication.** Recovery point is "the last
  scheduled run", not "the last transaction".
- **Single-user.** No roles, no per-user permissions inside the tool. Access
  control is whatever the OS account and the database grants already are.
- **A passing drill proves an artifact restores.** It does not prove the
  application works against the restored data. See
  [Verification](../explanation/verification.md).
- **The backup file itself is never masked.** Masking applies to a sync
  destination. That is intentional: a masked artifact would be a compromised
  recovery path.

## Questions you are likely to be asked

**"Where does our data go?"**
To the artifact directory you choose, and to any off-site destination you
configure. Nowhere else.

**"What happens if a laptop is lost?"**
The keychain entries are protected by the OS account. Artifacts can be encrypted
at rest to an installation key, with additional recipients if you want more than
one person able to decrypt. The key can be escrowed to a file for a safe.

**"Who changed the backup configuration?"**
`dbsync audit`, or the audit view in the app. It cannot be turned off.

**"Can we prove the backups work for an auditor?"**
A drill schedule produces a dated job record with an outcome per run, and exits
non-zero on failure so external monitoring can watch it.

## What to read next

- [Security model](../explanation/security-model.md) — one page, and the thing
  to hand to a security reviewer
- [Overview](../overview.md) — what a normal week looks like
- [For operators](operators.md) — what your team will actually do with it
