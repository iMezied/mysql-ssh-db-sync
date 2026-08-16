/**
 * Per-engine option defaults, in one place.
 *
 * These used to be a `engine === "postgres" ? … : …` ternary repeated in five
 * pages, which had two problems and only one of them was duplication. The
 * other is the failure mode: a two-way ternary has no "else" — a third engine
 * silently receives MySQL's options, and a MySQL backup request against a
 * MongoDB profile is refused by `BackupRequest::validate` with an engine
 * mismatch the user cannot act on. An exhaustive record cannot do that: adding
 * an engine to `Engine` makes TypeScript name every table below that is
 * missing an entry.
 *
 * The values mirror the `Default` impls in `engine/src/backup/mod.rs` and
 * `engine/src/restore/mod.rs`. They are restated rather than fetched because
 * the page has to render them before any command is sent.
 */
import type {
  Engine,
  EngineBackupOptions,
  EngineRestoreOptions,
  PgDumpFormat,
} from "../bindings";

/** Default TCP port, matching `Engine::default_port`. */
export const DEFAULT_PORT: Record<Engine, number> = {
  mysql: 3306,
  postgres: 5432,
  mongo: 27017,
};

/** What the engine is called in the interface. */
export const ENGINE_LABEL: Record<Engine, string> = {
  mysql: "MySQL",
  postgres: "PostgreSQL",
  mongo: "MongoDB",
};

/**
 * What colour the engine's mark is drawn in.
 *
 * Each is its brand hue, lightened for a dark background: the published values
 * (`#4479A1`, `#4169E1`, `#47A248`) sit at roughly 3:1 against `slate-900`,
 * which is thin for a 16px glyph. Lightening also pulls MySQL and PostgreSQL
 * apart — both ship a blue, twenty degrees of hue between them, and at icon
 * size that reads as one colour. Teal against indigo does not.
 *
 * The green is far enough from the `dev` environment badge's emerald to not be
 * mistaken for it, and the two are different shapes anyway: this is a glyph,
 * that is a pill.
 */
export const ENGINE_COLOR: Record<Engine, string> = {
  mysql: "#5FBDDA",
  postgres: "#7B93E8",
  mongo: "#5FBF62",
};

/**
 * What this engine calls a table and a collection of rows.
 *
 * The pages are written in relational vocabulary because four of the five were
 * built before there was anything else. Rather than rewrite them, the words
 * themselves come from here — so a MongoDB profile says "collections" and
 * "documents" without every page growing a branch.
 */
export const ENGINE_NOUNS: Record<Engine, { table: string; tables: string; row: string; rows: string }> = {
  mysql: { table: "table", tables: "tables", row: "row", rows: "rows" },
  postgres: { table: "table", tables: "tables", row: "row", rows: "rows" },
  mongo: {
    table: "collection",
    tables: "collections",
    row: "document",
    rows: "documents",
  },
};

/**
 * Whether this engine can dump a table's structure without its rows.
 *
 * MongoDB cannot: `mongodump` writes one archive in one pass, so a collection
 * is included whole or excluded. The pages hide the schema-only choice rather
 * than offering one the backup would refuse.
 */
export function supportsSchemaOnly(engine: Engine): boolean {
  return engine !== "mongo";
}

/** Whether a per-table row filter reaches the dump tool. */
export function supportsRowFilters(engine: Engine): boolean {
  return engine !== "mongo";
}

export function defaultBackupOptions(
  engine: Engine,
  pgFormat: PgDumpFormat = "custom",
): EngineBackupOptions {
  switch (engine) {
    case "postgres":
      return {
        engine: "postgres",
        format: pgFormat,
        no_owner: true,
        no_privileges: true,
        blobs: true,
        schemas: [],
        serializable_deferrable: false,
        parallel_jobs: null,
        include_globals: false,
        extra_flags: [],
      };
    case "mongo":
      return {
        engine: "mongo",
        // Needs a replica set; most development servers are standalone, so
        // turning this on by default would make the common case an error.
        oplog: false,
        parallel_collections: null,
        dump_users_and_roles: false,
        extra_flags: [],
      };
    case "mysql":
      return {
        engine: "mysql",
        single_transaction: true,
        hex_blob: true,
        set_gtid_purged_off: true,
        add_drop_table: true,
        extended_insert: true,
        routines: true,
        triggers: true,
        events: true,
        default_character_set: "utf8mb4",
        disable_column_statistics: false,
        strip_definer: true,
        parallel_threads: null,
        extra_flags: [],
      };
  }
}

export function defaultRestoreOptions(engine: Engine): EngineRestoreOptions {
  switch (engine) {
    case "postgres":
      return {
        engine: "postgres",
        no_owner: true,
        no_privileges: true,
        parallel_jobs: null,
        only_tables: [],
        clean: false,
      };
    case "mongo":
      return {
        engine: "mongo",
        drop_collections: false,
        only_collections: [],
        parallel_collections: null,
        insertion_workers: null,
        // Without this mongorestore skips documents it could not insert and
        // still exits 0, which would make a partial restore look complete.
        stop_on_error: true,
        restore_indexes: true,
        bypass_document_validation: false,
      };
    case "mysql":
      return {
        engine: "mysql",
        foreign_key_checks_off: true,
        unique_checks_off: true,
        autocommit_off: true,
        disable_binlog: false,
        charset: "utf8mb4",
        collation: "utf8mb4_unicode_ci",
      };
  }
}
