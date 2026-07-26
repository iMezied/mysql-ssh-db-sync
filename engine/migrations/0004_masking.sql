-- Column-level masking rules, attached to the plan that selects the tables.
--
-- On the plan rather than on the schedule because the rules describe the
-- *data*, not the timing: a plan's tables and the columns that need masking
-- change together, and every schedule running that plan should inherit the
-- same protection rather than each carrying its own copy to drift out of step.
--
-- Defaulted to an empty list so every existing plan reads back as "no masking",
-- which is what it has always been.
ALTER TABLE sync_plans ADD COLUMN masking TEXT NOT NULL DEFAULT '[]';
