# How to set up connections and SSH servers

A connection describes one database server. An SSH server describes one bastion,
saved once and reused by every connection that tunnels through it.

## Prerequisites

- The desktop app running. Both are created there, not in the CLI: adding an SSH
  server means verifying an unrecognised host key, which is a prompt no cron job
  can answer.
- For key-file auth, the private key already on this machine.

## Create an SSH server

**SSH servers → New SSH server.**

| Field | Notes |
|---|---|
| Name | How every connection and the audit log refer to it. Must be unique. |
| Jump host | Another *saved* server, or "Connect directly". One hop only. |
| Host / Port / User | The bastion itself |
| Authentication | `ssh-agent` (preferred) or `Key file` |
| Private key path | Key-file only. A path, never the key itself. |
| Key passphrase | Key-file only. Goes to the OS keychain, keyed by this server. |

Press **Save SSH server**, expand the row, and press **Test SSH**. This connects
and authenticates without involving a database, which separates "the bastion is
unreachable" from "the database refused us".

### First contact with a host key

The first test surfaces the server's fingerprint and asks you to verify it out
of band before trusting it. Compare against the server itself:

```bash
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
```

A **changed** key is presented differently and much louder. That happens after a
legitimate rebuild, and it is also exactly what interception looks like. Do not
continue until whoever runs the server confirms the new fingerprint.

## Create a connection

**Connections → New connection.**

| Field | Notes |
|---|---|
| Name | Unique |
| Engine | MySQL, PostgreSQL or MongoDB |
| Environment | `dev`, `staging`, `prod`. Not decoration — `prod` forces typed confirmation on destructive restores. |
| Host / Port | **See the warning below** |
| User | Database user |
| Database | Optional default |
| SSH tunnel | A saved SSH server, or none |
| Password | Goes to the OS keychain |

> **The one mistake everyone makes.** With a tunnel selected, the host and port
> are resolved **from the SSH server**, not from your machine. A database on the
> bastion itself is `127.0.0.1`. A database the bastion can reach is whatever the
> bastion calls it. The app says this inline next to the field.

Expand the row and press **Test connection**. Four steps report separately: SSH,
tunnel, database, catalog.

## Change the tunnel on an existing connection

Expand the connection's row. The **SSH tunnel** selector there re-points it at a
different server, or detaches it entirely. Re-test afterwards: the host field
means a different thing on each side of that change.

## Verify from the CLI

```bash
dbsync profiles
dbsync ssh
```

`dbsync ssh` shows each server, its endpoint, its jump host, and what tunnels
through it:

```
1111...  bastion-eu   ubuntu@bastion.example.com:22            [prod-eu, prod-us]
2222...  db-host      deploy@10.0.0.7:2222 via bastion-eu      [unused]
```

## Delete an SSH server

Deleting one that something still points at is **refused**, and the error names
every connection and jump host holding it. Detach them first. Cascading would
silently turn a tunnelled connection into a direct one, which either fails or,
worse, succeeds against something else.

Deleting a server that nothing uses also removes its stored passphrase.

## Troubleshooting

| Symptom | Cause |
|---|---|
| SSH step fails, "connection refused" | Bastion host/port wrong, or firewall |
| SSH step fails, "no authentication methods succeeded" | `ssh-agent` has no key loaded, or the key path is wrong, or the passphrase is not stored |
| SSH passes, tunnel fails | The bastion cannot reach the database host/port you gave |
| Tunnel passes, database fails | Wrong database credentials, or the user cannot connect from the bastion's address |
| Database passes, catalog fails | The user lacks read access to the catalog |
| "chained jumps are not supported" | The server you picked as a jump host has a jump host of its own, or is itself used as one |

## Related

- Concepts: [Connection](../concepts.md#connection),
  [SSH server](../concepts.md#ssh-server)
- Reference: [Data model](../reference/data-model.md)
- Why it is a record rather than a field: [DECISIONS.md](../../DECISIONS.md), M15
