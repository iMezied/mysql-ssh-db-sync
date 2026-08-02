-- A table set's name has to be unique within its connection.
--
-- Two places already assume it and are quietly wrong without it:
-- `share.rs` matches an incoming bundle's plan by name, so a config import
-- updates an arbitrary one of a colliding pair and leaves the other stale; and
-- the CLI's `resolve_plan` refuses an ambiguous prefix, which makes a
-- duplicated name unusable headlessly. The desktop picker also renders sets by
-- name, and two identical rows on a control that starts production backups is
-- a footgun.

-- Collisions are resolved by RENAMING, never by deleting. A set is a table
-- selection somebody built by hand — on a 109-table database that is real work,
-- and losing it to a schema migration would be indefensible. The oldest row
-- keeps the bare name; each later one gains a counter.
UPDATE sync_plans
   SET name = name || ' (' || (
       SELECT COUNT(*) + 1
         FROM sync_plans older
        WHERE older.profile_id = sync_plans.profile_id
          AND older.name       = sync_plans.name
          AND older.rowid      < sync_plans.rowid
   ) || ')'
 WHERE EXISTS (
       SELECT 1
         FROM sync_plans other
        WHERE other.profile_id = sync_plans.profile_id
          AND other.name       = sync_plans.name
          AND other.rowid      < sync_plans.rowid
   );

-- An index rather than a table rebuild. 0006 had to rebuild `schedules` to
-- change a constraint, and rebuilding here would mean dropping and recreating
-- the table that `schedules.sync_plan_id` points at.
CREATE UNIQUE INDEX IF NOT EXISTS idx_sync_plans_profile_name
    ON sync_plans (profile_id, name);
