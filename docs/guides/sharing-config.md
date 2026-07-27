# How to share configuration with a team

A bundle carries connections, SSH servers, sync plans and off-site destinations —
the shape of the work, not the ability to do it. It contains no passwords, no SSH
keys and no access keys, because the types it is built from have no field one
could occupy.

It is safe to commit to a repository, paste into a ticket, or attach to an
onboarding document.

## Export

```bash
dbsync config export > dbsync-config.json
dbsync config export --out dbsync-config.json
```

Or the app: **Settings → Shared configuration → Export a bundle**, which writes a
file and tells you where.

## Import

```bash
dbsync config import dbsync-config.json --dry-run   # print what would change
dbsync config import dbsync-config.json
```

In the app, **Preview** is required before **Import**, because an import changes
this machine.

## What import does

- **Matches by name, not by id.** Two machines generate different ids for the
  same server, so matching on id would duplicate everything on every import.
- **Creates what is missing, updates what is there.**
- **Never removes anything the bundle omits.** "I shared my config with you" must
  not be able to delete a connection you rely on.
- **Never writes a credential.**

## What the report tells you

| Line | Meaning | Action |
|---|---|---|
| created / updated | Counts, across SSH servers, connections, plans, destinations | None |
| These connections cannot connect until you set a password | Named individually | Set each password |
| These SSH servers use a key with a passphrase this machine does not have | Named individually | Store each passphrase |
| These destinations arrived switched off and need an access key | Named individually | Add the key, then enable |
| **These connections named an SSH server the bundle did not carry** | Imported as **direct** connections | Fix before use — see below |
| These plans could not be imported | Their connection was not in the bundle and is not on this machine | Import or create the connection first |

The orphaned-SSH case is shown in red and is the one to read twice. Nothing
failed, which is exactly the problem: a tunnelled connection quietly becoming a
direct one is noticed when it fails, or worse, when it succeeds against something
else.

## What travels and what does not

| Travels | Stays behind |
|---|---|
| Host, port, user, environment tag | Database passwords |
| SSH endpoint and **key file path** | The key file and its passphrase |
| Jump host, **by name** | — |
| Table selections and `WHERE` filters | — |
| Masking rules | The masking salt |
| Destination endpoint, bucket, region, prefix, access key **id** | The secret access key |

The masking salt is deliberately excluded: it is derived from a local secret, and
anyone holding both the rules and the salt could reverse the pseudonyms.

## Onboarding a new machine

1. `dbsync config import team-config.json`
2. Read the report and set each named password and passphrase
3. Add the access key for each destination, then enable it
4. Fix any orphaned SSH reference
5. Test every connection before scheduling anything

## Version compatibility

A bundle written before saved SSH servers existed still parses — the
`ssh_connections` field is defaulted, not required.

## Troubleshooting

| Symptom | Cause |
|---|---|
| Import button disabled | Preview it first |
| Plans skipped | Their connection is missing on this machine |
| Connections became direct | The bundle did not carry the SSH server they named |
| Duplicate connections | A name changed on one side; matching is by name |

## Related

- [Security model](../explanation/security-model.md)
- Why a bundle has nowhere to put a secret: [DECISIONS.md](../../DECISIONS.md), M13
