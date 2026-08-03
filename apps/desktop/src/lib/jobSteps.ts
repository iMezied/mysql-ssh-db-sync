/**
 * Reading a job's steps back into something a person can scan.
 *
 * The engine plans every step before the first one runs, so a row exists for
 * work that never happened. Turning four nullable columns into one word —
 * pending, running, done, failed, skipped — is the whole job here, and it is
 * worth testing because getting it wrong makes a failed run look finished.
 */
import type { JobStep, JobStepKind, JobStepOutcome } from "@/bindings";

export type StepStatus = "pending" | "running" | JobStepOutcome;

export const STEP_KIND_LABELS: Record<JobStepKind, string> = {
  backup: "Backup",
  restore: "Restore",
  verify: "Verify",
  mask: "Mask",
  offsite: "Off-site copy",
  retention: "Retention",
  drill: "Drill",
  cleanup: "Cleanup",
};

/**
 * What state a step is in.
 *
 * `outcome` wins whenever it is set. A step with no outcome is running if it
 * started and pending if it did not — the distinction the planned-up-front rows
 * exist to make.
 */
export function stepStatus(step: JobStep): StepStatus {
  if (step.outcome) return step.outcome;
  return step.started_at ? "running" : "pending";
}

/**
 * How long a step took, in milliseconds.
 *
 * `null` when it has not started, or when it is still going and the caller did
 * not supply a clock — a running step measured against nothing would report
 * zero, which reads as instant rather than as unknown.
 */
export function stepDurationMs(step: JobStep, nowMs?: number): number | null {
  if (!step.started_at) return null;
  const start = new Date(step.started_at).getTime();
  const end = step.finished_at ? new Date(step.finished_at).getTime() : nowMs;
  if (end == null || Number.isNaN(start)) return null;
  return Math.max(0, end - start);
}

/**
 * The one line shown under a step's label.
 *
 * Ordered by what a reader wants first: why it failed, then what it produced,
 * then anything it wanted to add.
 */
export function stepSummary(step: JobStep): string | null {
  const d = step.detail;
  const parts: string[] = [];

  if (d.error) parts.push(d.error);
  if (d.database) parts.push(d.database);
  if (d.artifact) parts.push(basename(d.artifact));
  if (d.tables_checked != null) {
    parts.push(
      `${d.tables_checked} table${d.tables_checked === 1 ? "" : "s"} checked`,
    );
  }
  // `notes` is optional on the wire: the Rust field is `#[serde(default)]`, so
  // a row written by a version that did not have it comes back absent.
  parts.push(...(d.notes ?? []));

  return parts.length > 0 ? parts.join(" · ") : null;
}

/**
 * Whether the step breakdown is worth drawing at all.
 *
 * A backup is one step, and a panel saying so tells the reader nothing they did
 * not get from the page title.
 */
export function worthShowing(steps: JobStep[]): boolean {
  return steps.length > 1;
}

function basename(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}
