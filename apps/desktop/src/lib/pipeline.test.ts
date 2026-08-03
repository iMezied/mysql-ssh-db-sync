/**
 * These decide what the builder lets somebody save, and — through
 * `destructiveSignature` — whether arming a pipeline is possible at all. The
 * signature has to match `Pipeline::destructive_signature` byte for byte: the
 * engine compares what the user typed against its own copy, so a different
 * join here would make arming fail with a message about a name the user can
 * see is correct.
 */
import { describe, expect, it } from "vitest";

import {
  destructiveSignature,
  destructiveTargets,
  describeStep,
  isArmed,
  moveStep,
  newStep,
  validatePipeline,
} from "./pipeline";
import type {
  ConnectionProfile,
  Pipeline,
  PipelineStep,
  TargetNaming,
} from "@/bindings";

const SRC = "11111111-1111-1111-1111-111111111111";
const DST = "22222222-2222-2222-2222-222222222222";

function profile(id: string, name: string, engine: "mysql" | "postgres" = "mysql") {
  return {
    id,
    name,
    engine,
    environment: "dev",
    ssh_connection_id: null,
    db: { host: "localhost", port: 3306, user: "root", database: null },
    tool_overrides: {},
    created_at: "2026-08-03T00:00:00Z",
    updated_at: "2026-08-03T00:00:00Z",
  } as unknown as ConnectionProfile;
}

const PROFILES = [profile(SRC, "prod"), profile(DST, "staging")];

function backup(database = "shop"): PipelineStep {
  return { ...newStep("backup", SRC, "mysql"), database } as PipelineStep;
}

function restore(naming: TargetNaming): PipelineStep {
  return { ...newStep("restore", DST, "mysql"), naming } as PipelineStep;
}

const REPLACE = { strategy: "drop_and_recreate", name: "staging" } as const;
const NEW = { strategy: "new_timestamped", prefix: "copy" } as const;
const INTO = { strategy: "into_existing", name: "staging" } as const;

describe("validatePipeline", () => {
  it("accepts a backup, restore and verify", () => {
    expect(
      validatePipeline("nightly", [backup(), restore(NEW), newStep("verify", "", "mysql")], PROFILES),
    ).toBe(null);
  });

  it("needs a name and at least one step", () => {
    expect(validatePipeline("  ", [backup()])).toMatch(/name/i);
    expect(validatePipeline("nightly", [])).toMatch(/at least one step/i);
  });

  it("refuses a restore with no backup before it", () => {
    expect(validatePipeline("x", [restore(NEW)])).toMatch(/step 1/i);
  });

  it("allows a restore from a file with no backup", () => {
    const fromFile = {
      ...newStep("restore", DST, "mysql"),
      source: { from: "path", path: "/tmp/shop.sql.gz" },
    } as PipelineStep;
    expect(validatePipeline("x", [fromFile])).toBe(null);
  });

  it("refuses a verify after a restore from a file", () => {
    // There is no source connection in that run to compare against, and a
    // verify that quietly did nothing would read as a passed check.
    const fromFile = {
      ...newStep("restore", DST, "mysql"),
      source: { from: "path", path: "/tmp/shop.sql.gz" },
    } as PipelineStep;
    expect(
      validatePipeline("x", [fromFile, newStep("verify", "", "mysql")]),
    ).toMatch(/step 2/i);
  });

  it("refuses masking into a database the run did not create", () => {
    const mask = { kind: "mask", rules: [{}] } as unknown as PipelineStep;
    expect(validatePipeline("x", [backup(), restore(INTO), mask])).toMatch(
      /step 3/i,
    );
  });

  it("allows masking after a replace", () => {
    const mask = { kind: "mask", rules: [{}] } as unknown as PipelineStep;
    expect(validatePipeline("x", [backup(), restore(REPLACE), mask], PROFILES)).toBe(
      null,
    );
  });

  it("refuses two steps replacing the same database", () => {
    expect(
      validatePipeline("x", [backup(), restore(REPLACE), restore(REPLACE)], PROFILES),
    ).toMatch(/two steps replace staging/i);
  });

  it("refuses off-site and retention with no artifact", () => {
    expect(validatePipeline("x", [newStep("push_offsite", "", "mysql")])).toMatch(
      /step 1/i,
    );
    expect(validatePipeline("x", [newStep("retention", "", "mysql")])).toMatch(
      /step 1/i,
    );
  });

  it("refuses a retention policy that keeps everything", () => {
    const keepAll = {
      kind: "retention",
      policy: { keep_last: null, max_age_days: null },
    } as PipelineStep;
    expect(validatePipeline("x", [backup(), keepAll])).toMatch(/keeps everything/i);
  });

  it("refuses restoring a mysql backup into a postgres connection", () => {
    const pg = profile(DST, "warehouse", "postgres");
    expect(
      validatePipeline("x", [backup(), restore(NEW)], [profile(SRC, "prod"), pg]),
    ).toMatch(/translates/i);
  });

  it("names a step whose connection has been deleted", () => {
    expect(validatePipeline("x", [backup()], [profile(DST, "staging")])).toMatch(
      /step 1.*no longer exists/i,
    );
  });
});

describe("destructiveSignature", () => {
  it("is null when nothing is replaced", () => {
    expect(destructiveSignature([backup(), restore(NEW)])).toBe(null);
  });

  it("joins targets with a newline, exactly as the engine does", () => {
    const two = [
      backup(),
      restore(REPLACE),
      restore({ strategy: "drop_and_recreate", name: "dev" }),
    ];
    expect(destructiveTargets(two)).toEqual(["staging", "dev"]);
    expect(destructiveSignature(two)).toBe("staging\ndev");
  });
});

describe("isArmed", () => {
  const armed = (steps: PipelineStep[], ack: string | null): Pipeline =>
    ({
      id: "p",
      name: "nightly",
      steps,
      unattended_ack: ack,
      created_at: "",
      updated_at: "",
    }) as Pipeline;

  it("is true only while the acknowledgment describes the current targets", () => {
    const steps = [backup(), restore(REPLACE)];
    expect(isArmed(armed(steps, "staging"))).toBe(true);
    expect(isArmed(armed(steps, null))).toBe(false);
  });

  it("goes false when a destructive target is renamed", () => {
    // The property the whole arming design rests on: permission is granted for
    // a named database, not for a pipeline.
    const renamed = [
      backup(),
      restore({ strategy: "drop_and_recreate", name: "production" }),
    ];
    expect(isArmed(armed(renamed, "staging"))).toBe(false);
  });

  it("is false for a pipeline that destroys nothing", () => {
    expect(isArmed(armed([backup(), restore(NEW)], "anything"))).toBe(false);
  });
});

describe("newStep", () => {
  it("defaults a restore to the strategy that cannot destroy anything", () => {
    const step = newStep("restore", DST, "mysql");
    expect(step.kind === "restore" && step.naming.strategy).toBe("new_timestamped");
  });

  it("gives each engine its own options rather than MySQL's", () => {
    const pg = newStep("backup", SRC, "postgres");
    expect(pg.kind === "backup" && pg.engine.engine).toBe("postgres");
  });
});

describe("moveStep", () => {
  const steps = [backup("a"), backup("b"), backup("c")];
  const names = (list: PipelineStep[]) =>
    list.map((s) => (s.kind === "backup" ? s.database : s.kind));

  it("moves a step down and up", () => {
    expect(names(moveStep(steps, 0, 2))).toEqual(["b", "c", "a"]);
    expect(names(moveStep(steps, 2, 0))).toEqual(["c", "a", "b"]);
  });

  it("returns the same list for a move that goes nowhere", () => {
    expect(moveStep(steps, 1, 1)).toBe(steps);
    expect(moveStep(steps, 0, 9)).toBe(steps);
    expect(moveStep(steps, -1, 0)).toBe(steps);
  });
});

describe("describeStep", () => {
  it("says what a replace will do, in the words that matter", () => {
    // The connection is deliberately named differently from the database, so
    // the sentence is readable rather than accidentally symmetrical.
    expect(describeStep(restore(REPLACE), [profile(DST, "eu-west")])).toBe(
      "Restore into staging, replacing it on eu-west",
    );
  });

  it("names a deleted connection rather than showing a bare id", () => {
    expect(describeStep(backup(), [])).toBe(
      "Back up shop from a deleted connection",
    );
  });
});
