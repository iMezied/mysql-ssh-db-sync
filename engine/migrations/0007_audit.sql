-- Configuration changes, as distinct from job runs.
--
-- `job_history` already records what ran: backups, restores, syncs, drills.
-- What it cannot answer is the question asked after an incident — "who changed
-- this, and when" — because the answer is usually not a job at all. A masking
-- rule was removed. A profile was re-pointed at a different host. The backup
-- key was exported. A shared bundle was imported over the top of everything.
--
-- Those are the events here. There is no "enabled" setting: a record of
-- sensitive changes that can be switched off is a record nobody can rely on,
-- and the volume is a handful of rows a week.
CREATE TABLE IF NOT EXISTS audit_log (
    id         TEXT PRIMARY KEY NOT NULL,
    at         TEXT NOT NULL,
    -- Machine-readable verb, e.g. "profile.deleted".
    action     TEXT NOT NULL,
    -- What it happened to, in words a person recognises.
    subject    TEXT NOT NULL,
    -- Free-form context. Opaque like job_history.options_json, and subject to
    -- the same rule as everything else: no secret is ever written here.
    detail     TEXT NOT NULL DEFAULT ''
);

-- The only query: most recent first.
CREATE INDEX IF NOT EXISTS idx_audit_at ON audit_log(at DESC);
