-- Saved chains of actions.
--
-- `ops::sync` already runs backup-then-restore as one job, but the shape is
-- fixed and nothing is saved: the wizard is rebuilt every time, and it refuses
-- a destructive target on purpose. A pipeline is the same composition written
-- down and named, with the target strategy the wizard would not offer.

CREATE TABLE IF NOT EXISTS pipelines (
    id             TEXT PRIMARY KEY,
    -- Unique, for the same reasons 0009 gave table sets unique names: the CLI
    -- resolves a pipeline by name prefix and refuses an ambiguous one, and a
    -- picker showing two identical rows on a control that can drop a database
    -- is a footgun.
    name           TEXT NOT NULL UNIQUE,
    -- Opaque, like `job_history.options_json` and `schedules.action_json`.
    -- The set of step kinds and their fields will grow; that must not need a
    -- migration, and a step written by an older version must still read.
    steps_json     TEXT NOT NULL,
    -- The destructive target names, as a human typed them back when arming
    -- this pipeline to run unattended. NULL means it has never been armed.
    --
    -- Stored as the signature rather than as a boolean, deliberately. A flag
    -- survives an edit to the step it was granted for; this does not. Renaming
    -- the target of a nightly replace makes the stored value stop matching, so
    -- the pipeline disarms itself instead of being silently re-aimed at a
    -- database nobody agreed to drop. See `Pipeline::is_armed`.
    unattended_ack TEXT,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);

-- No foreign key from a step to the connections it names. The steps live in
-- one JSON blob, so there is nothing for SQLite to reference, and a pipeline
-- whose connection was deleted has to survive being read: the editor needs to
-- show it and say which step is broken. `Pipeline::validate_against` reports
-- that, and the run refuses before touching anything.
