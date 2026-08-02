import { describe, expect, it } from "vitest";

import { basename, describeNaming, logLines, summariseOptions } from "./jobDetails";

/**
 * These read a job's stored request, which the engine keeps as opaque JSON so
 * its shape can change without a migration. That freedom is the whole risk: a
 * field that moves, disappears or changes type must cost the user a missing
 * line on the job page, never a page that fails to render at all.
 */

describe("summariseOptions", () => {
  it("reads the database and the data/schema split out of a backup", () => {
    const options = JSON.stringify({
      common: {
        database: "oqoodi_production",
        selections: [
          { name: "users", mode: "schema_and_data" },
          { name: "sessions", mode: "schema_only" },
          { name: "logs", mode: "exclude" },
        ],
      },
      engine: { engine: "mysql" },
    });

    expect(summariseOptions("backup", options)).toEqual([
      { label: "Database", value: "oqoodi_production" },
      { label: "Tables", value: "1 of 3 with data" },
    ]);
  });

  it("names the database a restore drops, since that is what is at stake", () => {
    const options = JSON.stringify({
      artifact_path: "/Users/x/Backups/oqoodi_20260731.sql.gz",
      naming: { strategy: "drop_and_recreate", name: "oqoodi_production" },
      verify_checksum: true,
    });

    expect(summariseOptions("restore", options)).toEqual([
      { label: "Artifact", value: "oqoodi_20260731.sql.gz" },
      { label: "Target", value: "oqoodi_production (dropped and recreated)" },
      { label: "Checksum", value: "checked before the restore" },
    ]);
  });

  it("reaches through a sync's nested backup for the source database", () => {
    const options = JSON.stringify({
      backup: { common: { database: "shop", selections: [] } },
      naming: { strategy: "new_timestamped", prefix: "sync" },
      verify: true,
      deep_verify: true,
    });

    expect(summariseOptions("sync", options)).toEqual([
      { label: "Database", value: "shop" },
      { label: "Tables", value: "0 of 0 with data" },
      { label: "Target", value: "sync_… (a new database)" },
      { label: "Verification", value: "row counts and table contents" },
    ]);
  });

  it("says nothing rather than guessing when the request is unreadable", () => {
    // An options blob written by a future build, or a truncated one. Either
    // way the rest of the page — status, progress, log — is still correct.
    expect(summariseOptions("backup", "not json")).toEqual([]);
    expect(summariseOptions("backup", "{}")).toEqual([]);
    expect(summariseOptions("restore", '{"naming":{"strategy":"unknown"}}')).toEqual(
      [],
    );
    expect(summariseOptions("backup", '{"common":{"database":42}}')).toEqual([]);
  });
});

describe("describeNaming", () => {
  it("marks a timestamped target as generated, not as a literal name", () => {
    expect(describeNaming({ strategy: "new_timestamped", prefix: "sync" })).toBe(
      "sync_… (a new database)",
    );
  });

  it("distinguishes restoring over from dropping first", () => {
    expect(describeNaming({ strategy: "into_existing", name: "shop" })).toBe(
      "shop (restored over)",
    );
    expect(describeNaming({ strategy: "drop_and_recreate", name: "shop" })).toBe(
      "shop (dropped and recreated)",
    );
  });
});

describe("logLines", () => {
  it("drops the trailing blank a log always ends with", () => {
    expect(logLines("first\nsecond\n")).toEqual(["first", "second"]);
    expect(logLines(null)).toEqual([]);
  });
});

describe("basename", () => {
  it("takes the file name off either separator", () => {
    expect(basename("/var/backups/db.sql.gz")).toBe("db.sql.gz");
    expect(basename("C:\\backups\\db.sql.gz")).toBe("db.sql.gz");
    expect(basename(undefined)).toBeUndefined();
  });
});
