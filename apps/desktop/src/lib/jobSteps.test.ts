/**
 * These decide whether a run reads as finished or as failed. A step that
 * never ran must not render like one that succeeded, and a step still going
 * must not render like one that took no time at all.
 */
import { describe, expect, it } from "vitest";

import {
  stepDurationMs,
  stepStatus,
  stepSummary,
  worthShowing,
} from "./jobSteps";
import type { JobStep, JobStepKind } from "@/bindings";

function step(over: Partial<JobStep> = {}): JobStep {
  return {
    job_id: "00000000-0000-0000-0000-000000000000",
    index: 1,
    kind: "backup" as JobStepKind,
    label: "Back up shop",
    started_at: null,
    finished_at: null,
    outcome: null,
    detail: {
      artifact: null,
      database: null,
      tables_checked: null,
      error: null,
      notes: [],
    },
    ...over,
  };
}

describe("stepStatus", () => {
  it("calls a planned step pending, not running", () => {
    expect(stepStatus(step())).toBe("pending");
  });

  it("calls a started step with no outcome running", () => {
    expect(stepStatus(step({ started_at: "2026-08-02T10:00:00Z" }))).toBe(
      "running",
    );
  });

  it("lets a recorded outcome win over the timestamps", () => {
    expect(
      stepStatus(
        step({ started_at: "2026-08-02T10:00:00Z", outcome: "failed" }),
      ),
    ).toBe("failed");
  });

  it("reports a step the run never reached as skipped", () => {
    // No started_at, but an outcome: the job ended and settled it.
    expect(stepStatus(step({ outcome: "skipped" }))).toBe("skipped");
  });
});

describe("stepDurationMs", () => {
  it("is null for a step that never started", () => {
    expect(stepDurationMs(step(), Date.parse("2026-08-02T10:05:00Z"))).toBe(
      null,
    );
  });

  it("measures a finished step between its own timestamps", () => {
    const s = step({
      started_at: "2026-08-02T10:00:00Z",
      finished_at: "2026-08-02T10:04:00Z",
      outcome: "success",
    });
    expect(stepDurationMs(s)).toBe(4 * 60 * 1000);
  });

  it("measures a running step against the clock it is given", () => {
    const s = step({ started_at: "2026-08-02T10:00:00Z" });
    expect(stepDurationMs(s, Date.parse("2026-08-02T10:01:30Z"))).toBe(90_000);
  });

  it("says nothing rather than zero for a running step with no clock", () => {
    expect(stepDurationMs(step({ started_at: "2026-08-02T10:00:00Z" }))).toBe(
      null,
    );
  });
});

describe("stepSummary", () => {
  it("is null when the step had nothing to report", () => {
    expect(stepSummary(step())).toBe(null);
  });

  it("leads with the error when there is one", () => {
    const s = step({
      outcome: "failed",
      detail: {
        artifact: null,
        database: "shop_copy",
        tables_checked: null,
        error: "target database is not empty",
        notes: [],
      },
    });
    expect(stepSummary(s)).toBe("target database is not empty · shop_copy");
  });

  it("shows an artifact by filename, not by full path", () => {
    const s = step({
      detail: {
        artifact: "/Users/me/Backups/shop_20260802.sql.gz",
        database: null,
        tables_checked: null,
        error: null,
        notes: [],
      },
    });
    expect(stepSummary(s)).toBe("shop_20260802.sql.gz");
  });

  it("pluralises the table count", () => {
    const one = step({
      detail: {
        artifact: null,
        database: null,
        tables_checked: 1,
        error: null,
        notes: [],
      },
    });
    expect(stepSummary(one)).toBe("1 table checked");
  });
});

describe("worthShowing", () => {
  it("hides the breakdown for a single-step job", () => {
    // A plain backup is one step; a panel saying so is noise.
    expect(worthShowing([step()])).toBe(false);
    expect(worthShowing([])).toBe(false);
  });

  it("shows it as soon as there is a shape to see", () => {
    expect(worthShowing([step(), step({ index: 2, kind: "restore" })])).toBe(
      true,
    );
  });
});
