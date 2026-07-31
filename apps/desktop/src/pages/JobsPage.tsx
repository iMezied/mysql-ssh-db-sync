import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";

import PageHeader from "@/components/PageHeader";
import { api } from "@/lib/api";
import {
  cn,
  formatBytes,
  formatDuration,
  formatElapsed,
  formatTimestamp,
} from "@/lib/utils";
import {
  PHASE_LABELS,
  describeProgress,
  remainingMs,
  useProgressStore,
} from "@/lib/jobProgress";
import {
  events,
  type JobOutcome,
  type JobRecord,
  type ProgressEvent,
} from "@/bindings";

const OUTCOME_STYLES: Record<JobOutcome, string> = {
  success: "bg-emerald-500/15 text-emerald-300",
  failed: "bg-red-500/15 text-red-300",
  cancelled: "bg-slate-500/15 text-slate-400",
};

/** Most recent live progress lines, newest last. */
const LIVE_LIMIT = 200;

/** How often the elapsed clock and the estimate redraw while a job runs. */
const TICK_MS = 1_000;

/**
 * How often to re-ask which jobs this process is actually running.
 *
 * Only polled while an unfinished job is on screen, and only as a backstop:
 * the `JobFinished` event is what normally clears one.
 */
const ACTIVE_POLL_MS = 5_000;

/**
 * What was *changed*, as opposed to what ran.
 *
 * Job history answers "did the backup work". This answers the question asked
 * after an incident, which is usually not about a job at all: a masking rule
 * was removed, a connection was re-pointed, the key was exported.
 */
function ChangeLog() {
  const audit = useQuery({
    queryKey: ["audit"],
    queryFn: () => api.listAudit(20),
  });

  if (!audit.data || audit.data.length === 0) return null;

  return (
    <section>
      <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-slate-500">
        Configuration changes
      </h2>
      <div className="panel divide-y divide-slate-800">
        {audit.data.map((entry) => (
          <div
            key={entry.id}
            className="flex flex-wrap items-baseline gap-x-3 gap-y-1 px-4 py-2 text-xs"
          >
            <span className="w-36 shrink-0 text-slate-500">
              {formatTimestamp(entry.at)}
            </span>
            <span className="font-mono text-slate-300">{entry.action}</span>
            <span className="text-slate-200">{entry.subject}</span>
            {entry.detail && (
              <span className="text-slate-500">{entry.detail}</span>
            )}
          </div>
        ))}
      </div>
    </section>
  );
}

export default function JobsPage() {
  const [live, setLive] = useState<ProgressEvent[]>([]);

  const jobs = useQuery({ queryKey: ["jobs"], queryFn: () => api.listJobs(50) });

  // A job with no finish time has not necessarily survived: a crash or a quit
  // leaves the row open forever. Only this process can say which ones are
  // genuinely still going, and the difference matters — one is worth waiting
  // for, the other is worth starting again.
  const unfinished = jobs.data?.some((j) => j.outcome === null) ?? false;
  const active = useQuery({
    queryKey: ["active-jobs"],
    queryFn: () => api.activeJobIds(),
    refetchInterval: unfinished ? ACTIVE_POLL_MS : false,
  });
  const activeIds = new Set(active.data ?? []);

  const anyLive = jobs.data?.some((j) => j.outcome === null && activeIds.has(j.id));
  const now = useTick(anyLive ?? false);

  // Page-local scrollback. The store in `App` keeps the *latest* state per job
  // for the progress bars; this keeps the running transcript, which only has
  // somewhere to go while this page is open.
  useEffect(() => {
    const progress = events.jobProgress.listen((e) => {
      setLive((prev) => [...prev, e.payload].slice(-LIVE_LIMIT));
    });
    return () => {
      void progress.then((unlisten) => unlisten());
    };
  }, []);

  return (
    <>
      <PageHeader
        title="Jobs"
        description="Live progress, the durable history of every run, and a record of what was changed."
      />

      <div className="space-y-6 p-6">
        <ChangeLog />

        <section>
          <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-slate-500">
            Live activity
          </h2>
          <div className="panel h-40 overflow-y-auto p-3 font-mono text-xs">
            {live.length === 0 ? (
              <p className="text-slate-600">
                No job running. Progress from a running job streams here.
              </p>
            ) : (
              live.map((e, i) => (
                <div key={`${e.job_id}-${i}`} className="flex gap-2">
                  <span className="text-slate-600">
                    {new Date(e.ts).toLocaleTimeString()}
                  </span>
                  <span
                    className={cn(
                      "w-12 shrink-0",
                      e.level === "error"
                        ? "text-red-400"
                        : e.level === "warn"
                          ? "text-amber-400"
                          : "text-slate-500",
                    )}
                  >
                    {e.level}
                  </span>
                  <span className="text-slate-300">{e.message}</span>
                </div>
              ))
            )}
          </div>
        </section>

        <section>
          <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-slate-500">
            History
          </h2>

          {jobs.data?.length === 0 ? (
            <div className="panel p-8 text-center text-sm text-slate-500">
              No jobs have run yet.
            </div>
          ) : (
            <div className="panel divide-y divide-slate-800">
              {jobs.data?.map((job) => (
                <JobRow
                  key={job.id}
                  job={job}
                  live={activeIds.has(job.id)}
                  now={now}
                />
              ))}
            </div>
          )}
        </section>
      </div>
    </>
  );
}

/**
 * A clock that only runs when something on screen depends on it.
 *
 * Left running unconditionally it would re-render the whole history list once
 * a second forever, including in the tray with no window open.
 */
function useTick(enabled: boolean): number {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (!enabled) return;
    setNow(Date.now());
    const id = setInterval(() => setNow(Date.now()), TICK_MS);
    return () => clearInterval(id);
  }, [enabled]);

  return now;
}

/** One row of history: finished, genuinely running, or abandoned. */
function JobRow({
  job,
  live,
  now,
}: {
  job: JobRecord;
  live: boolean;
  now: number;
}) {
  const running = job.outcome === null && live;
  const interrupted = job.outcome === null && !live;

  return (
    <div className="px-4 py-3">
      <div className="flex items-center gap-4">
        <span className="w-16 text-xs uppercase text-slate-400">{job.kind}</span>
        <span className="flex-1 text-xs text-slate-500">
          {formatTimestamp(job.started_at)}
        </span>
        <span className="w-20 text-right text-xs tabular-nums text-slate-500">
          {interrupted ? "—" : formatDuration(job.started_at, job.finished_at, now)}
        </span>
        <span
          className={cn(
            "w-24 rounded px-2 py-0.5 text-center text-[10px] font-semibold uppercase",
            job.outcome
              ? OUTCOME_STYLES[job.outcome]
              : running
                ? "bg-blue-500/15 text-blue-300"
                : "bg-amber-500/15 text-amber-300",
          )}
        >
          {job.outcome ?? (running ? "running" : "interrupted")}
        </span>
      </div>

      {running && <RunningDetail jobId={job.id} />}

      {interrupted && (
        <p className="mt-1.5 text-xs text-amber-300/70">
          Nothing is running this job — the app was quit or restarted before it
          finished. Start it again.
        </p>
      )}
    </div>
  );
}

/** Phase, position and estimate for a job this process is running now. */
function RunningDetail({ jobId }: { jobId: string }) {
  const progress = useProgressStore((s) => s.byJob[jobId]);

  if (!progress) {
    return (
      <p className="mt-1.5 text-xs text-slate-500">Waiting for the first update…</p>
    );
  }

  const { latest } = progress;
  const percent = latest.percent;
  const position = describeProgress(latest, formatBytes);
  const left = remainingMs(progress);

  return (
    <div className="mt-2 space-y-1.5">
      <div className="flex items-baseline gap-2 text-xs">
        <span className="font-medium text-slate-300">
          {PHASE_LABELS[latest.phase]}
        </span>
        {latest.table && (
          <span className="font-mono text-slate-400">{latest.table}</span>
        )}
        <span className="truncate text-slate-500">{latest.message}</span>
      </div>

      {percent != null && (
        <div className="h-1.5 overflow-hidden rounded-full bg-slate-800">
          <div
            className="h-full rounded-full bg-blue-500 transition-[width] duration-500"
            style={{ width: `${Math.min(100, Math.max(0, percent))}%` }}
          />
        </div>
      )}

      <div className="flex flex-wrap items-center gap-x-3 text-[11px] tabular-nums text-slate-500">
        {percent != null && (
          <span className="text-slate-400">{percent.toFixed(0)}%</span>
        )}
        {position && <span>{position}</span>}
        {latest.bytes != null && <span>{formatBytes(latest.bytes)} written</span>}
        {latest.rows != null && (
          <span>{latest.rows.toLocaleString()} rows</span>
        )}
        {/* Prefixed with a tilde, always: it is extrapolated from throughput so
            far, and a bare "3m 20s" reads as a promise the job cannot keep. */}
        {left != null && (
          <span className="text-slate-400">~{formatElapsed(left)} left</span>
        )}
      </div>
    </div>
  );
}
