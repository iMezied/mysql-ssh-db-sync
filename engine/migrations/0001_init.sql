-- Initial schema for DBSync Studio.
--
-- Timestamps are RFC3339 TEXT and UUIDs are TEXT: SQLite has no native type for
-- either, and TEXT keeps the file trivially inspectable with the sqlite3 CLI
-- when debugging a user's machine.
--
-- No secret is ever stored here. Passwords and key passphrases live in the OS
-- keychain, addressed by profile id.

CREATE TABLE IF NOT EXISTS profiles (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL UNIQUE,
    engine          TEXT NOT NULL,
    environment     TEXT NOT NULL,
    ssh_config      TEXT,
    db_config       TEXT NOT NULL,
    tool_overrides  TEXT NOT NULL DEFAULT '{}',
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS job_history (
    id                 TEXT PRIMARY KEY NOT NULL,
    kind               TEXT NOT NULL,
    source_profile_id  TEXT NOT NULL,
    dest_profile_id    TEXT,
    started_at         TEXT NOT NULL,
    finished_at        TEXT,
    outcome            TEXT,
    artifact_path      TEXT,
    options_json       TEXT NOT NULL DEFAULT '{}',
    log                TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_job_history_started_at
    ON job_history (started_at DESC);

-- A named table-selection for a source profile. Supersedes the old
-- tables.conf file. Revisions are kept so a plan change can be rolled back.
CREATE TABLE IF NOT EXISTS sync_plans (
    id                TEXT PRIMARY KEY NOT NULL,
    profile_id        TEXT NOT NULL,
    name              TEXT NOT NULL,
    database_name     TEXT NOT NULL,
    table_selections  TEXT NOT NULL,
    revision          INTEGER NOT NULL DEFAULT 1,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    FOREIGN KEY (profile_id) REFERENCES profiles (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_sync_plans_profile
    ON sync_plans (profile_id);

CREATE TABLE IF NOT EXISTS schedules (
    id               TEXT PRIMARY KEY NOT NULL,
    sync_plan_id     TEXT NOT NULL,
    cron_expression  TEXT NOT NULL,
    enabled          INTEGER NOT NULL DEFAULT 1,
    webhook_url      TEXT,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    FOREIGN KEY (sync_plan_id) REFERENCES sync_plans (id) ON DELETE CASCADE
);

-- App-managed known_hosts. We never use StrictHostKeyChecking=no; an unknown
-- key prompts the user with its fingerprint, and the accepted key is pinned
-- here so a later change is surfaced as a warning rather than silently trusted.
CREATE TABLE IF NOT EXISTS known_hosts (
    host_port    TEXT PRIMARY KEY NOT NULL,
    key_type     TEXT NOT NULL,
    fingerprint  TEXT NOT NULL,
    first_seen   TEXT NOT NULL
);
