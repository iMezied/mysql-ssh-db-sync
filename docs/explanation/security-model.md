# Security model

## The problem

Database tooling handles the most sensitive credentials an organisation has, and
the usual failure is not a clever attack. It is a password in a shell script, in
a repository, in everyone's history. It is a dump of production sitting on a
staging box. It is a config file shared during onboarding that nobody remembered
to redact.

Each of those is a *design* failure: the system had somewhere to put a secret
that it should not have had.

## The approach

**Make it structurally impossible rather than remembered.**

### Secrets are not in the database, because there is no column for them

| Secret | Where | Keyed by |
|---|---|---|
| Database password | OS keychain | Profile id |
| SSH key passphrase | OS keychain | SSH connection id |
| Object-store secret key | OS keychain | Destination id |
| Backup encryption key | OS keychain | Fixed app-scoped account |
| SSH private key | Never touched | Only a path is stored |

The `profiles`, `ssh_connections` and `destinations` tables hold endpoints,
usernames, paths and key **ids**. Nothing that is a credential.

### Secrets do not cross the IPC boundary

The webview can store a secret and ask whether one exists. There is deliberately
**no command that returns one**. Status commands answer "yes there is a password"
and never "here it is".

The one thing that must leave the engine — the backup key, for escrow — is
written to a file and the *path* is returned. The secret never becomes a
JavaScript string. The file is created `0600` as part of opening it, so it is
never briefly world-readable.

### Secrets never appear in argv

Child processes (`mysqldump`, `pg_dump`, `psql`) receive credentials through
environment variables or `0600` credential files. Never `-p<password>`, which is
visible in `ps` to every user on the machine and lands in shell history.

The same reasoning drives `dbsync destination add` reading the secret access key
from **stdin** rather than a flag.

### Shared configuration has nowhere to put a secret

An export bundle is built from types (`SharedProfile`, `SharedSshConnection`,
`SharedDestination`) that have **no field a credential could occupy**. The
keychain is not consulted in either direction — not as policy, but because
nothing in the bundle could receive what it returned.

Three consequences:

- An SSH key *path* travels; the key does not.
- Masking rules travel; the salt does not. Anyone holding both could reverse the
  pseudonyms.
- Records match by name, not id, because ids differ between machines.

### Host keys are pinned

First contact surfaces the fingerprint for out-of-band verification. A **changed**
key is presented as a distinct and much louder case, and replacing it is an
explicit separate action. There is no `StrictHostKeyChecking=no` equivalent,
because silently accepting a changed key is indistinguishable from accepting a
machine-in-the-middle.

### Transport is TLS, and plaintext is refused

Object-store endpoints must be `https://`. Plaintext `http://` is refused for
anything but loopback, at creation, with an error naming the fix.

### Destructive actions require typed confirmation

| Action | Gate |
|---|---|
| Restore over an existing database | The exact target name, typed back, checked by the engine |
| Restore into a `prod`-tagged connection | Typed confirmation even for non-destructive strategies |
| A schedule doing a destructive restore | Refused outright — nobody is present at 04:00 |
| Deleting an SSH server in use | Refused, error names every holder |
| A drill's target | Engine-generated scratch name; nothing else is droppable |

### Changes are recorded, and the record cannot be turned off

`audit_log` records what was **changed**, as distinct from what **ran**. There is
no setting and no column to disable it. It records that a secret was set, never
what it was. A failed audit write never aborts the change being audited — an
incomplete log is a better outcome than refusing to delete a profile because the
log was unwritable.

### The webview is given as little as possible

Tauri capabilities are `core:default` only. Dump and restore processes are
spawned by the Rust engine, so the webview needs no shell permission.

## Trade-offs

**The keychain is the trust anchor.** An attacker with your unlocked OS account
has your credentials. That is true of every tool that stores credentials for you;
the alternative is prompting for every password on every run, which people work
around by putting passwords back in scripts.

**Artifacts are unencrypted by default.** Encryption is opt-in per backup or per
schedule. The default favours "you can always restore this" over "this is
protected at rest"; for anything leaving the machine, turn it on.

**The application database is not encrypted.** It holds no credentials, but it
does hold hostnames, usernames and table names. Treat it as configuration, not as
public.

**Masking protects the destination, not the artifact.** A masked recovery path is
not a recovery path.

**Single-user.** No internal RBAC. The OS account and existing database grants
are the access control.

## Related

- [For security reviewers](../roles/security-reviewers.md) — the review checklist
- [Encryption](../guides/encryption.md) · [Masking](../guides/masking.md)
- [DECISIONS.md](../../DECISIONS.md) — M9, M10, M13 and M15 cover most of this
