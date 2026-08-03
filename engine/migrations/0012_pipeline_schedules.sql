-- A schedule can run a pipeline.
--
-- 0006 made `schedules` carry a `kind` so a drill could reuse the cron, the
-- timezone, the enabled flag, the catch-up rule, the notify policy and the
-- high-water mark rather than duplicating all of it in a second table. The
-- same argument applies again: a pipeline schedule differs from a sync only
-- in what it points at.

-- Additive, so no table rebuild. `sync_plan_id` has been nullable since 0006,
-- which is what lets a pipeline schedule leave it empty — the steps carry
-- their own connections and table selections, so there is nothing for a plan
-- to say.
--
-- No foreign key to `pipelines`, matching `dest_profile_id`, and for the same
-- reason stated there: a deleted pipeline must make the schedule fail loudly
-- at its next run, not have this column quietly set to NULL. A schedule whose
-- pipeline vanished and now reads as a sync with no plan is a job that stops
-- happening while still looking configured.
ALTER TABLE schedules ADD COLUMN pipeline_id TEXT;
