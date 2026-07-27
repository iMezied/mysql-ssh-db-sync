# How to encrypt backups at rest

Artifacts are plain files by default. Encryption protects the file itself, using
[age](https://age-encryption.org), with the installation's key held in the OS
keychain.

## Create the key

```bash
dbsync key status      # does one exist, what is its public half, who else can decrypt
dbsync key generate    # create it, if there is not one already
```

Or in the app: **Settings → Backup key**.

There is one key per machine, stored under a fixed app-scoped keychain account
so it can be found without a connection.

## Encrypt a backup

```bash
dbsync backup prod --encrypt
```

Or turn on **Encrypt** on a schedule, which applies to every run.

The manifest records that the artifact is encrypted and which recipients can
decrypt it, so a restore knows what it is looking at before it starts.

## Escrow the key

An encrypted backup and a lost key is a lost backup.

```bash
dbsync key export > /secure/location/dbsync-key.txt
```

In the app, escrow writes to a **file** and tells you the path. The secret never
becomes a string in the webview, and the file is created `0600` as part of
opening it, so it is never briefly world-readable.

Put it wherever your organisation keeps things like this. Not next to the
backups.

## Restore the key on another machine

```bash
dbsync key import < /secure/location/dbsync-key.txt
```

This replaces the current key. Artifacts encrypted to the old key stop being
decryptable by this installation unless the old key is also kept somewhere.

## Let more than one person decrypt

```bash
dbsync key recipients age1... age1...
```

Additional recipients apply to **future** backups. Existing artifacts keep the
recipient list they were written with.

A recipient that is not a valid age key is refused at the point of entry rather
than accepted and discovered at recovery time — the failure mode being a
manifest naming somebody who can never decrypt.

## Restoring an encrypted artifact

Nothing special:

```bash
dbsync restore staging /backups/app_20260727T040000.sql.gz.age
```

The key is found in the keychain. If it is not there, or is the wrong key, the
restore fails before touching the server.

## What this does and does not protect

| Threat | Covered |
|---|---|
| Backup drive or laptop stolen | Yes, if the artifact is encrypted |
| Object-store bucket exposed | Yes, if the artifact is encrypted |
| Attacker with your unlocked OS account | No — the key is in your keychain |
| Data leaking to a staging environment | No, that is [masking](masking.md) |
| Application database read | Nothing sensitive is in it — see [Security model](../explanation/security-model.md) |

## Troubleshooting

| Symptom | Cause |
|---|---|
| "no key exists" on `--encrypt` | Run `dbsync key generate` |
| Restore fails on an encrypted artifact | Wrong machine, or the key was replaced. Import the right one. |
| A recipient cannot decrypt | They were added after that artifact was written |
| Recipient rejected | Not a valid age public key |

## Related

- [Off-site copies](offsite.md) — the case encryption most matters for
- [Security model](../explanation/security-model.md)
