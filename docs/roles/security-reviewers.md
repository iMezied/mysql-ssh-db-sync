# For security reviewers

*A map of where sensitive material lives and what crosses which boundary. The
full statement is [Security model](../explanation/security-model.md); this page
is the review checklist and where to look in the code.*

## Trust boundaries

```
   webview (React)                 ← untrusted-ish: renders data, holds no secret
        │ Tauri IPC (65 commands, generated, typed)
   desktop shell (Rust)            ← thin; extracts state, calls engine
        │
   engine (Rust)                   ← all domain logic, all secret access
        ├── OS keychain            ← every credential
        ├── SQLite store           ← configuration only, no credentials
        ├── child processes        ← mysqldump / pg_dump / psql / mysql / pg_restore
        └── network                ← SSH (russh), S3 over TLS, webhooks
```

## What is stored where

| Material | Location | Notes |
|---|---|---|
| DB passwords | OS keychain, keyed by connection id | No column exists for it |
| SSH key passphrases | OS keychain, keyed by **SSH server** id | One entry per server, not per connection |
| SSH private keys | Never touched | Only a *path* is stored; `ssh-agent` is preferred |
| Object-store secret keys | OS keychain, keyed by destination id | Access key **id** is config, not secret |
| Backup encryption key | OS keychain, fixed app-scoped account | age identity; one per machine |
| Host key fingerprints | `known_hosts` table | Pinned, see below |
| Everything else | SQLite at the app data dir | Endpoints, table selections, cron, retention |

## Claims to verify

Each of these is testable, and where it is enforced is named.

1. **No command returns a secret.** The IPC surface can store a secret and
   report whether one exists (`profile_secret_status`, `ssh_connection_status`,
   `backup_key_status`) and has no command that returns one. Check
   `apps/desktop/src-tauri/src/commands.rs`.
2. **The encryption key never becomes a JS string.** Escrow writes to a file and
   returns the *path* (`export_backup_key_to_file`). The file is created `0600`
   as part of opening it, so it is never briefly world-readable.
3. **Credentials never appear in argv.** Child processes receive them via
   environment variables or `0600` credential files, never `-p<password>`.
   Check `engine/src/exec.rs`.
4. **Host keys are pinned.** First contact surfaces the fingerprint for out-of-band
   verification; a *changed* key is presented as a distinct, louder case and
   requires an explicit replace. There is no bypass flag.
5. **Shared configuration cannot carry a secret.** `engine/src/share.rs` builds
   from types (`SharedProfile`, `SharedSshConnection`, `SharedDestination`) that
   have no field a credential could occupy. The keychain is not consulted in
   either direction.
6. **Plaintext object-store endpoints are refused.** `http://` is rejected for
   anything but loopback, at creation, with an error naming `https://`.
7. **The audit log has no off switch.** `engine/src/audit.rs`. It records that a
   secret was set, never its value. A failed audit write never aborts the change
   being audited.
8. **Tauri capabilities are minimal.** `core:default` only. The webview has no
   shell permission; dump and restore processes are spawned by the engine.

## Data handling

- **Backups contain production data in full.** Masking applies to a sync
  destination, not to the artifact. Treat artifact directories and buckets as
  production-classified.
- **Encryption at rest is opt-in per backup** (`--encrypt`, or the toggle in the
  app), using age. Additional recipients can be added so more than one person
  can decrypt; a recipient that is not a valid age key is refused at the point of
  entry rather than discovered at recovery time.
- **Notifications name profiles only.** No host, port, username, password, key
  path or directory leaves the machine in a webhook. Redirects are not followed,
  delivery is a single 10-second attempt, and a failed webhook never fails the
  run.

## Destructive-action gates

| Action | Gate |
|---|---|
| Restore over an existing database | Typed confirmation of the exact target name, checked in the engine |
| Restore into a `prod`-tagged connection | Typed confirmation even for non-destructive strategies |
| Scheduled destructive restore | Refused outright — nobody is present to confirm |
| Delete an SSH server in use | Refused, error names every holder |
| Trust a *changed* host key | Separate explicit action from trusting a new one |
| Drill target | Engine-generated scratch name; nothing else is droppable |

## Known limitations to note in a review

- Single-user tool; no internal RBAC. OS account and database grants are the
  access control.
- The application database is not encrypted at rest. It holds no credentials,
  but it does hold hostnames, usernames and table names.
- Artifacts are unencrypted unless you ask for encryption.
- `known_hosts` pinning is per-installation, not shared between machines.

## Next

- [Security model](../explanation/security-model.md) — the full statement
- [Verification](../explanation/verification.md) — what a passing drill proves
- [Data model](../reference/data-model.md) — every table and column
