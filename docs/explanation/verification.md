# Verification: what is actually proven

## The problem

"The backup succeeded" is a statement about a process, not about a file. Every
interesting backup failure is one where the process reported success:

- the dump ran against the wrong database and produced a valid, small artifact
- a table stopped being selected and nobody noticed for six weeks
- the file was truncated by a full disk, and the checksum was never checked
- the artifact restores fine and the data in it is a month old

A tool that says "backup complete" and nothing else has told you almost nothing.

## The four levels

Each answers a different question. None of them subsumes the one below.

### 1. Checksum against the manifest

**Question: is this file the one that was written?**

Every artifact has a manifest recording its checksum. Before an artifact reaches
a server, the two are compared. A truncated or altered file is caught here rather
than halfway through a restore.

Cheap next to a restore. `--no-verify-checksum` exists and wants a reason.

**Does not prove**: that the contents are correct, only that they are unchanged
since the backup.

### 2. Drill — restore into a scratch database

**Question: can this artifact still be restored at all?**

`dbsync drill <profile>` restores the newest artifact into a scratch database
whose name the engine chooses, checks it against the manifest, and drops it.
Non-zero exit on failure.

**Proves**: the artifact is well-formed, the target server accepts it, the schema
applies, and every table named in the manifest arrived.

**Does not prove**: that the row *contents* are right, or that your application
works against it.

### 3. Row counts

**Question: did the same amount of data arrive?**

Only possible when the backup recorded them (`--count-rows`, or the schedule
toggle). It costs a full scan per data table, which is why it is opt-in.

Without counts, a drill can tell that a table arrived but **cannot tell a table
that restored empty from one that was empty at the source**. It reports that as
**not compared** rather than as passing. That distinction is the whole point: a
verification that guesses is worse than one that admits ignorance.

### 4. Deep comparison

**Question: is the content the same, not just the amount?**

`--deep` on a drill, or `deep_verify` on a sync schedule: content digests, column
by column, rather than counts.

Costs a full scan of both sides. Off by default and stays off on upgrade, so an
existing schedule does not silently acquire the cost.

**Proves**: the bytes match. This is the strongest statement available.

## What none of them prove

**That your application works against the restored data.** A schema that applies
and rows that match is not the same as an application that starts. If that matters
— and for a disaster recovery plan it does — the drill is the first step of a
rehearsal, not the whole rehearsal.

**That the backup is recent enough.** Verification says nothing about recovery
point. A perfectly verified artifact from six weeks ago is a perfectly verified
six-week-old artifact.

**That you can find the artifact under pressure.** Off-site copies, retention
policies and someone knowing the restore command are separate problems.

## The one failure verification cannot see

An artifact can be valid, checksummed, restorable and row-matched, and still be
wrong — because it is wrong *relative to yesterday*. A `WHERE` filter changed, or
a table stopped being selected, and the backup is internally consistent and
missing half your data.

That is what the library's **shrink warning** is for. `dbsync library` exits
non-zero when a backup came out dramatically smaller than the one before it. It
is the only check here that compares a backup against history rather than against
itself.

## A verification setup that means something

```bash
dbsync backup prod --count-rows          # counts recorded
dbsync drill staging --deep              # restored, and contents compared
dbsync library                           # and not suspiciously smaller than yesterday
```

Three commands, three distinct failures, all of them actionable, none of them
overlapping.

## Related

- [Library, retention and drills](../guides/library-retention-drills.md)
- [Sync between servers](../guides/sync.md)
- [Concepts: manifest](../concepts.md#manifest)
