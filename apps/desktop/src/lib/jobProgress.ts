import { create } from "zustand";

import type { JobPhase, ProgressEvent } from "@/bindings";

/**
 * Phase names as a person would say them.
 *
 * The raw `dump_data` is fine in a log line but wrong in a status strip, and
 * the distinction between `initializing` and `ssh_connect` matters to whoever
 * is waiting: one is instant, the other is where a bad key hangs.
 */
export const PHASE_LABELS: Record<JobPhase, string> = {
  initializing: "Starting",
  ssh_connect: "Connecting over SSH",
  tunneling: "Opening the tunnel",
  introspect: "Reading the database",
  dump_schema: "Dumping schema",
  dump_data: "Dumping data",
  compress: "Compressing",
  transfer: "Transferring",
  restore: "Restoring",
  verify: "Verifying",
  cleanup: "Cleaning up",
  done: "Finishing",
};

/** One point on a job's progress curve. */
type Sample = { at: number; done: number };

export type JobProgress = {
  /** Newest event for this job, whether or not it carried a measurement. */
  latest: ProgressEvent;
  /**
   * Oldest sample still describing the current measurement.
   *
   * Throughput is measured from here rather than from when the job started.
   * The SSH connect and introspect phases report no progress at all — on a
   * remote database they can run for a minute — and folding those seconds
   * into the average makes the first estimate wildly pessimistic.
   *
   * Null until the first measurable event, which is most of the interesting
   * waiting: "connecting over SSH" has no percentage and is still what the
   * user needs to see.
   */
  anchor: Sample | null;
  /** Newest sample that carried a measurement. */
  last: Sample | null;
};

type ProgressStore = {
  byJob: Record<string, JobProgress>;
  record: (event: ProgressEvent) => void;
  forget: (jobId: string) => void;
};

/**
 * Live progress, kept outside the page that displays it.
 *
 * The events arrive whether or not the Jobs page is mounted, and a user who
 * starts a backup and then goes to look at their connections should not come
 * back to an empty progress bar.
 */
export const useProgressStore = create<ProgressStore>((set) => ({
  byJob: {},

  record: (event) =>
    set((state) => {
      const prev = state.byJob[event.job_id];
      const at = Date.parse(event.ts);

      // Phase transitions and warnings carry no numbers. They still update
      // what is shown — "connecting to the source" is the useful line at that
      // moment — but they must not disturb the throughput samples.
      if (event.done == null || !Number.isFinite(at)) {
        return {
          byJob: {
            ...state.byJob,
            [event.job_id]: {
              latest: event,
              anchor: prev?.anchor ?? null,
              last: prev?.last ?? null,
            },
          },
        };
      }

      const sample: Sample = { at, done: event.done };
      const restarted =
        prev?.last == null ||
        prev.latest.unit !== event.unit ||
        prev.latest.total !== event.total ||
        // A dump that finishes and an upload that starts both count from zero.
        event.done < prev.last.done;

      return {
        byJob: {
          ...state.byJob,
          [event.job_id]: {
            latest: event,
            anchor: restarted ? sample : prev.anchor,
            last: sample,
          },
        },
      };
    }),

  forget: (jobId) =>
    set((state) => {
      if (!(jobId in state.byJob)) return state;
      const byJob = { ...state.byJob };
      delete byJob[jobId];
      return { byJob };
    }),
}));

/**
 * Too short a window and the estimate jumps around on every event; this is
 * long enough for a dump to settle and short enough that the number appears
 * while it is still worth having.
 */
const MIN_WINDOW_MS = 4_000;

/**
 * Table counts are a far rougher clock than bytes — one 40 GB table among two
 * hundred small ones ruins the average — so they need several tables behind
 * them before the extrapolation means anything.
 */
const MIN_TABLES = 4;

/**
 * Milliseconds of work left, or `null` when there is no honest answer yet.
 *
 * Deliberately returns nothing rather than a wide guess: a countdown that
 * swings from 20 seconds to 9 minutes and back teaches the user to ignore it,
 * which costs more than the blank space.
 */
export function remainingMs(progress: JobProgress): number | null {
  const { latest, anchor, last } = progress;
  if (anchor == null || last == null) return null;
  if (latest.total == null || latest.unit == null) return null;

  const window = last.at - anchor.at;
  const advanced = last.done - anchor.done;
  if (advanced <= 0 || window < MIN_WINDOW_MS) return null;
  if (latest.unit === "tables" && advanced < MIN_TABLES) return null;

  const left = latest.total - last.done;
  if (left <= 0) return null;

  return (left / advanced) * window;
}

/** "table 13 of 47" or "8.4 MB of 31.0 MB", depending on what is being counted. */
export function describeProgress(
  event: ProgressEvent,
  formatBytes: (n: number) => string,
): string | null {
  if (event.done == null || event.total == null || event.unit == null) {
    return null;
  }
  return event.unit === "bytes"
    ? `${formatBytes(event.done)} of ${formatBytes(event.total)}`
    : `table ${event.done} of ${event.total}`;
}
