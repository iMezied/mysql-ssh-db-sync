-- The steps a composite job is made of.
--
-- `job_history` records that a sync ran and whether it worked. It cannot say
-- that the backup took four minutes, the restore took thirty-one, and masking
-- never ran — that structure lived only in banner comments in `ops::sync` and
-- in a flat log. One row per step makes the shape of a run readable, and makes
-- "which part is it on" answerable while it is still going.

-- Rows are written as PENDING before the first step starts, so `started_at` is
-- nullable. Inserting on begin instead cannot distinguish "step 4 has not
-- happened yet" from "step 4 never will": a run that dies at step 2 would read
-- as a two-step run that finished.
--
-- NO FOREIGN KEY to `job_history`, deliberately. The engine ops are callable
-- without a history row — that is not a loophole, it is how every integration
-- test drives `ops::sync`, and the row is owned by the shell (the Tauri
-- command, the CLI, the scheduler) rather than by the operation. An FK would
-- make a diagnostic write able to abort a forty minute restore. Nothing
-- deletes from `job_history`, so the cascade it would buy has no work to do.
CREATE TABLE IF NOT EXISTS job_steps (
    job_id      TEXT    NOT NULL,
    -- 1-based, because it is read as "step 2 of 5".
    idx         INTEGER NOT NULL,
    kind        TEXT    NOT NULL,
    -- Decided when the step is planned, so it can name the actual database
    -- rather than repeating the kind.
    label       TEXT    NOT NULL,
    started_at  TEXT,
    finished_at TEXT,
    -- NULL with a started_at means running; NULL without means pending.
    outcome     TEXT,
    -- Opaque, same reasoning as `job_history.options_json`: what is worth
    -- showing beside a step will change, and should not need a migration.
    detail_json TEXT    NOT NULL DEFAULT '{}',
    PRIMARY KEY (job_id, idx)
);
