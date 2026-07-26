-- Application preferences.
--
-- Deliberately a key/value table rather than a column per setting: these are
-- read once at startup and written when a toggle is flipped, never queried or
-- joined, so a migration per preference would be pure ceremony.
--
-- Secrets never appear here. Anything sensitive belongs in the OS keychain,
-- which is why there is no "value_is_secret" column to get wrong.

CREATE TABLE IF NOT EXISTS app_settings (
    key    TEXT PRIMARY KEY NOT NULL,
    value  TEXT NOT NULL
);
