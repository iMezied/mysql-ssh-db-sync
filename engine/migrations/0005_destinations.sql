-- Off-site destinations: where a second copy of each artifact is sent.
--
-- No credential column, and there will not be one. The secret access key lives
-- in the OS keychain under the destination's id; everything stored here is
-- safe to read, log and export. A column called `secret` would be one bad
-- SELECT away from a backup file containing live credentials.
--
-- `kind` holds the tagged JSON of DestinationKind rather than a set of typed
-- columns. Adding SFTP or another dialect is then a new variant rather than a
-- migration, and the alternative — a wide table where two thirds of the
-- columns are NULL for any given row — describes nothing accurately.

CREATE TABLE IF NOT EXISTS destinations (
    id          TEXT PRIMARY KEY NOT NULL,
    -- Unique so a job log naming a destination is unambiguous.
    name        TEXT NOT NULL UNIQUE,
    kind        TEXT NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    retention   TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- Every backup asks for the enabled ones and nothing else.
CREATE INDEX IF NOT EXISTS idx_destinations_enabled ON destinations(enabled);
