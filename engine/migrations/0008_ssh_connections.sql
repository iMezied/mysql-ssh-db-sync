-- SSH connections become a record in their own right.
--
-- Until now a tunnel was a JSON blob on the profile that used it, so one
-- bastion fronting six databases was six copies of the same address, user and
-- key path — and changing it meant remembering all six. A profile now points
-- at a named row here instead.
--
-- `endpoint` is JSON (host, port, user, auth) for the same reason the other
-- config columns are: adding an auth method is a new variant, not a migration.
-- It holds no secret. A key-file path is configuration; the passphrase is in
-- the OS keychain, keyed by the id of the row below.
--
-- A jump host is a reference to another row here rather than embedded fields,
-- so a shared bastion is shared rather than copied one level down. ON DELETE
-- RESTRICT: removing a connection that something still routes through would
-- silently turn a tunnelled profile into a direct one.
CREATE TABLE IF NOT EXISTS ssh_connections (
    id            TEXT PRIMARY KEY NOT NULL,
    name          TEXT NOT NULL UNIQUE,
    endpoint      TEXT NOT NULL,
    jump_host_id  TEXT REFERENCES ssh_connections (id) ON DELETE RESTRICT,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_ssh_connections_jump_host
    ON ssh_connections (jump_host_id);

-- NULL means "connect directly, without a tunnel", exactly as a NULL
-- `ssh_config` did.
ALTER TABLE profiles
    ADD COLUMN ssh_connection_id TEXT
    REFERENCES ssh_connections (id) ON DELETE RESTRICT;

CREATE INDEX IF NOT EXISTS idx_profiles_ssh_connection
    ON profiles (ssh_connection_id);

-- `profiles.ssh_config` is deliberately left in place and is not read as
-- configuration any more. Existing rows are adopted into the table above at
-- startup by `sshconn::adopt_legacy_configs`, which deduplicates identical
-- endpoints and moves the stored key passphrase onto the new record — neither
-- of which is expressible here. The column is cleared as each profile is
-- adopted, and dropping it is left for a later migration so that an upgrade
-- interrupted halfway still has the original configuration to read.
