# How to keep an off-site copy

A backup that only exists on the machine that made it is one failure away from
not existing. A destination is the second copy: every backup is uploaded to each
enabled destination as soon as it is written.

## Add a destination

The secret access key is read from **stdin**, never from an argument — a
credential on the command line lands in shell history and is visible in `ps` to
every user on the machine.

```bash
printf '%s' "$SECRET_KEY" | dbsync destination add \
  --name off-site \
  --endpoint https://s3.eu-west-1.amazonaws.com \
  --bucket acme-backups \
  --region eu-west-1 \
  --prefix prod \
  --access-key-id AKIA...
```

For MinIO and most self-hosted gateways add `--path-style`, which addresses the
bucket by path rather than by subdomain.

Or use the app's **Off-site** page.

### Rules enforced at creation

- **A destination with no credential is refused** before it is stored. One that
  looks configured and cannot upload surfaces at 3am as a failed backup.
- **Plaintext `http://` is refused** for anything but loopback, with an error
  naming `https://`.

## Test it

```bash
dbsync destination test off-site
```

Proves the endpoint resolves, the credential is accepted, and the bucket exists
and can be listed. It does **not** prove the credential can write — only a write
proves that, so `push` is the stronger check.

## Use it

Nothing more to do. Every backup uploads to every enabled destination as it is
written.

**A failed upload fails the job.** That is deliberate: the whole point of the
second copy is that you find out immediately when you do not have one.

Upload something that already exists:

```bash
dbsync destination push /backups/app_20260727T040000.sql.gz
```

## Pause without losing configuration

```bash
dbsync destination disable off-site
dbsync destination enable off-site
```

Disabling keeps the credential. Pausing a destination for an afternoon must not
mean setting it up again.

## Off-site retention

Separate from local retention, and set on the destination:

```bash
dbsync destination retention off-site --keep-last 30
dbsync destination retention off-site --max-age-days 90
dbsync destination retention off-site            # passing neither clears it
```

Listing follows continuation tokens, so a bucket with more than a page of
objects is handled correctly.

## Rotate the credential

```bash
printf '%s' "$NEW_KEY" | dbsync destination set-key off-site
```

## Remove it

```bash
dbsync destination remove off-site
```

Removes the row and its keychain entry. It does not delete anything already in
the bucket.

## Combine with encryption

Object storage is the case encryption is for. Turn on `--encrypt` (or the
schedule toggle) and the artifact is unreadable to anyone who gets at the bucket.
See [Encryption](encryption.md).

## Troubleshooting

| Symptom | Cause |
|---|---|
| Refused at creation, "invalid" | No credential given, or a plaintext `http://` endpoint |
| `test` passes but uploads fail | The credential can list but not write |
| Every backup fails after adding a destination | Intended: the destination is enabled and cannot upload. Fix it or disable it. |
| Bucket appears empty | Wrong prefix, or `--path-style` needed |
| Imported destination does nothing | Imported destinations arrive **disabled** and need a credential |

## Related

- [Encryption](encryption.md) · [Library, retention and drills](library-retention-drills.md)
- Concepts: [Destination](../concepts.md#destination)
