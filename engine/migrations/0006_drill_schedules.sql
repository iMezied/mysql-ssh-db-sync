-- Let a schedule be a restore drill as well as a backup or a sync.
--
-- # Why this rebuilds the table
--
-- A drill has no sync plan. It restores the newest artifact from a directory
-- into a scratch database on one profile, checks it against its manifest, and
-- drops it — there is no table selection to inherit, because the artifact
-- already fixes what it contains.
--
-- `sync_plan_id` was `NOT NULL`, and SQLite cannot drop a NOT NULL constraint
-- with ALTER TABLE. The alternatives were both worse than a rebuild:
--
--   * A sentinel plan id for drills. A row that points at a plan which does not
--     exist, or at a real plan it ignores, is exactly the kind of quiet lie
--     this project keeps finding and removing.
--   * A separate `drill_schedules` table. Cron, timezone, enabled, catch-up,
--     notify, webhook and the last-run high-water mark would all be duplicated,
--     and so would the scheduler's due-check — two implementations of "is this
--     due" is two things to drift.
--
-- Nothing references `schedules`, so the rebuild cannot orphan another table's
-- foreign key. The FK on `sync_plan_id` is kept and still cascades: deleting a
-- plan should still delete the sync schedules that ran it. A drill's NULL is
-- exempt from the constraint, which is standard SQL and what makes this work.

CREATE TABLE schedules_new (
    id               TEXT PRIMARY KEY NOT NULL,
    -- "sync" or "drill". Defaulted so every existing row reads as what it is.
    kind             TEXT NOT NULL DEFAULT 'sync',
    -- Now nullable: required for a sync, and always NULL for a drill.
    sync_plan_id     TEXT,
    name             TEXT NOT NULL DEFAULT '',
    -- For a sync, the optional restore destination. For a drill, the profile
    -- being drilled, and required. Still deliberately not a foreign key — see
    -- 0002 for why a deleted profile must fail loudly rather than set NULL.
    dest_profile_id  TEXT,
    cron_expression  TEXT NOT NULL,
    timezone         TEXT NOT NULL DEFAULT 'local',
    enabled          INTEGER NOT NULL DEFAULT 1,
    action_json      TEXT NOT NULL DEFAULT '{}',
    webhook_url      TEXT,
    notify           TEXT NOT NULL DEFAULT 'on_failure',
    catch_up         INTEGER NOT NULL DEFAULT 0,
    last_run_at      TEXT,
    last_outcome     TEXT,
    last_job_id      TEXT,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    FOREIGN KEY (sync_plan_id) REFERENCES sync_plans (id) ON DELETE CASCADE
);

-- Every existing row is a sync; the column list is explicit so a future column
-- added to one table and not the other fails loudly here instead of shifting
-- values silently into the wrong column.
INSERT INTO schedules_new (
    id, kind, sync_plan_id, name, dest_profile_id, cron_expression, timezone,
    enabled, action_json, webhook_url, notify, catch_up,
    last_run_at, last_outcome, last_job_id, created_at, updated_at
)
SELECT
    id, 'sync', sync_plan_id, name, dest_profile_id, cron_expression, timezone,
    enabled, action_json, webhook_url, notify, catch_up,
    last_run_at, last_outcome, last_job_id, created_at, updated_at
FROM schedules;

DROP TABLE schedules;
ALTER TABLE schedules_new RENAME TO schedules;

-- Recreated: dropping the table took the old indexes with it.
CREATE INDEX IF NOT EXISTS idx_schedules_plan ON schedules (sync_plan_id);
CREATE INDEX IF NOT EXISTS idx_schedules_enabled ON schedules (enabled);
