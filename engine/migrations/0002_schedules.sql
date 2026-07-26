-- Fill out the schedules table for unattended runs.
--
-- 0001 created a placeholder with just a cron expression and a webhook. A
-- schedule also has to say what to do (where to write, whether to restore
-- anywhere, what to keep), who to tell, and what happened last time.
--
-- Done as ALTER rather than a recreate: no code has ever written to this table,
-- so every install has it empty, and adding columns preserves whatever a
-- hand-edited development database might contain.

ALTER TABLE schedules ADD COLUMN name TEXT NOT NULL DEFAULT '';

-- Deliberately NOT a foreign key. ON DELETE SET NULL would silently turn a
-- cross-server sync into a local-backup-only job when its destination profile
-- was deleted, and nobody would notice until they needed the replica; ON DELETE
-- CASCADE would delete the schedule outright. Failing loudly at the next run,
-- with a notification, is the only option that tells the user what happened.
ALTER TABLE schedules ADD COLUMN dest_profile_id TEXT;

-- "local" or "utc". Local is what people mean by "back up at 2am"; UTC is the
-- escape hatch for schedules that must not be perturbed by daylight saving.
ALTER TABLE schedules ADD COLUMN timezone TEXT NOT NULL DEFAULT 'local';

-- Opaque, like job_history.options_json: option shapes change with every
-- engine feature, and none of them are ever queried by SQL.
ALTER TABLE schedules ADD COLUMN action_json TEXT NOT NULL DEFAULT '{}';

ALTER TABLE schedules ADD COLUMN notify TEXT NOT NULL DEFAULT 'on_failure';

-- Whether an occurrence missed while the machine slept is made up afterwards.
-- Off by default: a laptop opened at 09:00 should not start a production
-- backup that was meant for 03:00.
ALTER TABLE schedules ADD COLUMN catch_up INTEGER NOT NULL DEFAULT 0;

-- The high-water mark that stops one occurrence running twice, and the outcome
-- the UI shows without having to join job history.
ALTER TABLE schedules ADD COLUMN last_run_at TEXT;
ALTER TABLE schedules ADD COLUMN last_outcome TEXT;
ALTER TABLE schedules ADD COLUMN last_job_id TEXT;

CREATE INDEX IF NOT EXISTS idx_schedules_plan
    ON schedules (sync_plan_id);

-- The scheduler's hot path on every tick.
CREATE INDEX IF NOT EXISTS idx_schedules_enabled
    ON schedules (enabled);
