import { create } from "zustand";

import type { JobOutcome, JobPhase, ProgressEvent } from "@/bindings";

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

/**
 * A job this window started, as described at the moment the button was pressed.
 *
 * Kept because the job's own record cannot always answer it. A just-started job
 * has not been read back from the database yet, and an off-site push is never
 * written to job history at all — see `push_artifact_offsite`. Without this the
 * page the user lands on would have nothing to put in its heading.
 */
export type LaunchedJob = {
  /** The kind of run, as a heading: "Restore", "Off-site upload". */
  title: string;
  /** What it is doing: "into oqoodi_production". */
  detail: string;
  startedAt: string;
};

/**
 * Log lines kept per job, oldest first.
 *
 * Enough to cover the interesting part of a run — a per-table dump of a large
 * database is the long case — without holding every line of an all-day sync in
 * memory for a window nobody is looking at.
 */
const LINE_LIMIT = 500;

/** How many jobs keep a transcript before the oldest one is dropped. */
const TRANSCRIPT_LIMIT = 20;

const OUTCOMES = ["success", "failed", "cancelled"] as const;

type ProgressStore = {
  byJob: Record<string, JobProgress>;
  /**
   * The streamed transcript per job.
   *
   * Separate from `byJob` because the two have opposite lifetimes: progress is
   * dropped the moment a job finishes so a stale 98% bar cannot sit next to a
   * green "success", while the transcript is most wanted right after that —
   * it is the record of what the run actually did.
   */
  lines: Record<string, ProgressEvent[]>;
  /** Which jobs hold a transcript, oldest first. */
  transcripts: string[];
  /** Terminal state as it arrived on the event stream, for jobs with no row. */
  outcomes: Record<string, JobOutcome>;
  launched: Record<string, LaunchedJob>;
  record: (event: ProgressEvent) => void;
  forget: (jobId: string) => void;
  noteLaunch: (jobId: string, job: { title: string; detail: string }) => void;
  noteFinished: (jobId: string, outcome: string) => void;
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
  lines: {},
  transcripts: [],
  outcomes: {},
  launched: {},

  record: (event) =>
    set((state) => ({
      byJob: {
        ...state.byJob,
        [event.job_id]: nextProgress(state.byJob[event.job_id], event),
      },
      ...appendLine(state, event),
    })),

  forget: (jobId) =>
    set((state) => {
      if (!(jobId in state.byJob)) return state;
      const byJob = { ...state.byJob };
      delete byJob[jobId];
      return { byJob };
    }),

  noteLaunch: (jobId, job) =>
    set((state) => ({
      launched: {
        ...state.launched,
        [jobId]: { ...job, startedAt: new Date().toISOString() },
      },
    })),

  // The event carries the outcome as a plain string, so a value this build
  // does not know about is dropped rather than displayed as a status badge.
  noteFinished: (jobId, outcome) =>
    set((state) =>
      (OUTCOMES as readonly string[]).includes(outcome)
        ? { outcomes: { ...state.outcomes, [jobId]: outcome as JobOutcome } }
        : state,
    ),
}));

/** Where a job's progress stands after one more event. */
function nextProgress(
  prev: JobProgress | undefined,
  event: ProgressEvent,
): JobProgress {
  const at = Date.parse(event.ts);

  // Phase transitions and warnings carry no numbers. They still update what is
  // shown — "connecting to the source" is the useful line at that moment — but
  // they must not disturb the throughput samples.
  if (event.done == null || !Number.isFinite(at)) {
    return {
      latest: event,
      anchor: prev?.anchor ?? null,
      last: prev?.last ?? null,
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
    latest: event,
    anchor: restarted ? sample : prev.anchor,
    last: sample,
  };
}

/** The transcript maps with this event added, evicting the oldest job's. */
function appendLine(
  state: Pick<ProgressStore, "lines" | "transcripts">,
  event: ProgressEvent,
): Pick<ProgressStore, "lines" | "transcripts"> {
  const existing = state.lines[event.job_id];
  const lines = {
    ...state.lines,
    [event.job_id]: [...(existing ?? []), event].slice(-LINE_LIMIT),
  };

  if (existing) return { lines, transcripts: state.transcripts };

  const transcripts = [...state.transcripts, event.job_id];
  while (transcripts.length > TRANSCRIPT_LIMIT) {
    const oldest = transcripts.shift();
    if (oldest != null) delete lines[oldest];
  }
  return { lines, transcripts };
}

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
