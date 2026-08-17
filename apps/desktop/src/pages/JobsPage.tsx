import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { ChevronRight } from "lucide-react";

import PageHeader from "@/components/PageHeader";
import JobProgressStrip from "@/components/JobProgressStrip";
import Pager from "@/components/Pager";
import { api } from "@/lib/api";
import { clampPage, offsetOf } from "@/lib/paging";
import { cn, formatDuration, formatTimestamp } from "@/lib/utils";
import { useTick } from "@/lib/useTick";
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

/**
 * Rows per page of history and of the change log.
 *
 * Sized to fill the pane without turning the page into a scroll: both lists
 * grow without limit — a nightly schedule alone adds a row a day — and before
 * this they were a single flat list capped at 50, which quietly hid everything
 * older and could not be paged back to.
 */
const JOBS_PER_PAGE = 25;
const AUDIT_PER_PAGE = 20;

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
  const [page, setPage] = useState(0);

  const audit = useQuery({
    queryKey: ["audit", page],
    queryFn: () => api.listAudit(AUDIT_PER_PAGE, offsetOf(page, AUDIT_PER_PAGE)),
    // Hold the previous page while the next one loads. Without it the list
    // collapses to nothing between clicks and the pager jumps up the screen.
    placeholderData: keepPreviousData,
  });

  usePageInRange(page, setPage, audit.data?.total, AUDIT_PER_PAGE);

  if (!audit.data || audit.data.total === 0) return null;

  return (
    <section>
      <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-slate-500">
        Configuration changes
      </h2>
      <div className="panel divide-y divide-slate-800">
        {audit.data.entries.map((entry) => (
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

      <Pager
        page={page}
        pageSize={AUDIT_PER_PAGE}
        total={audit.data.total}
        onPage={setPage}
        noun="changes"
      />
    </section>
  );
}

/**
 * Pull the page back into range when the list shrinks under it.
 *
 * Reachable without deleting anything: sit on the last page, then let the
 * count drop — a pruned log, a store swapped by an import — and the request
 * returns an empty page that reads as "no history" rather than "you are past
 * the end". The arithmetic lives in `lib/paging` so its edges are tested.
 */
function usePageInRange(
  page: number,
  setPage: (page: number) => void,
  total: number | undefined,
  pageSize: number,
) {
  useEffect(() => {
    if (total === undefined) return;
    const clamped = clampPage(page, total, pageSize);
    if (clamped !== page) setPage(clamped);
  }, [page, setPage, total, pageSize]);
}

export default function JobsPage() {
  const [live, setLive] = useState<ProgressEvent[]>([]);
  const [page, setPage] = useState(0);

  const jobs = useQuery({
    queryKey: ["jobs", JOBS_PER_PAGE, page],
    queryFn: () => api.listJobs(JOBS_PER_PAGE, offsetOf(page, JOBS_PER_PAGE)),
    placeholderData: keepPreviousData,
  });

  usePageInRange(page, setPage, jobs.data?.total, JOBS_PER_PAGE);

  // A job with no finish time has not necessarily survived: a crash or a quit
  // leaves the row open forever. Only this process can say which ones are
  // genuinely still going, and the difference matters — one is worth waiting
  // for, the other is worth starting again.
  const unfinished = jobs.data?.jobs.some((j) => j.outcome === null) ?? false;
  const active = useQuery({
    queryKey: ["active-jobs"],
    queryFn: () => api.activeJobIds(),
    refetchInterval: unfinished ? ACTIVE_POLL_MS : false,
  });
  const activeIds = new Set(active.data ?? []);

  const anyLive = jobs.data?.jobs.some(
    (j) => j.outcome === null && activeIds.has(j.id),
  );
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

          {jobs.data?.total === 0 ? (
            <div className="panel p-8 text-center text-sm text-slate-500">
              No jobs have run yet.
            </div>
          ) : (
            <>
              <div className="panel divide-y divide-slate-800">
                {jobs.data?.jobs.map((job) => (
                  <JobRow
                    key={job.id}
                    job={job}
                    live={activeIds.has(job.id)}
                    now={now}
                  />
                ))}
              </div>

              {jobs.data && (
                <Pager
                  page={page}
                  pageSize={JOBS_PER_PAGE}
                  total={jobs.data.total}
                  onPage={setPage}
                  noun="jobs"
                />
              )}
            </>
          )}
        </section>
      </div>
    </>
  );
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
    // The whole row opens the job, rather than a "details" link at the end of
    // it: the row *is* the job, and the thing a user wants after a run is the
    // one they just started.
    <Link
      to={`/jobs/${job.id}`}
      className="block px-4 py-3 transition hover:bg-slate-800/40"
    >
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
        <ChevronRight className="h-3.5 w-3.5 shrink-0 text-slate-600" />
      </div>

      {running && <JobProgressStrip jobId={job.id} />}

      {interrupted && (
        <p className="mt-1.5 text-xs text-amber-300/70">
          Nothing is running this job — the app was quit or restarted before it
          finished. Start it again.
        </p>
      )}
    </Link>
  );
}
