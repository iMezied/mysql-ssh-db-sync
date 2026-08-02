import { useEffect, useRef } from "react";
import { Link, useParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { ArrowLeft } from "lucide-react";

import PageHeader from "@/components/PageHeader";
import JobProgressStrip from "@/components/JobProgressStrip";
import { api } from "@/lib/api";
import { KIND_LABELS, logLines, summariseOptions } from "@/lib/jobDetails";
import { useProgressStore } from "@/lib/jobProgress";
import { useTick } from "@/lib/useTick";
import { cn, formatDuration, formatTimestamp } from "@/lib/utils";
import type { JobOutcome, JobRecord, ProgressEvent } from "@/bindings";

const OUTCOME_STYLES: Record<JobOutcome, string> = {
  success: "bg-emerald-500/15 text-emerald-300",
  failed: "bg-red-500/15 text-red-300",
  cancelled: "bg-slate-500/15 text-slate-400",
};

/**
 * Deep enough to cover a job reached from the history list, which shows fifty.
 * A job started from a button is the newest row there is, so it is found on the
 * first fetch either way.
 */
const HISTORY_LIMIT = 200;

/** Backstop for the `JobFinished` event, same as the Jobs list uses. */
const ACTIVE_POLL_MS = 5_000;

/**
 * One job, from the moment it is started to the record it leaves behind.
 *
 * This is where every "run" button lands. Before it existed, pressing Run
 * returned a job id in a line of green text and left the user to find the job
 * themselves — which meant the thing they had just started, and the only thing
 * they wanted to watch, was two clicks away and indistinguishable from the
 * fifty runs above it.
 */
export default function JobDetailPage() {
  const { jobId = "" } = useParams();

  const jobs = useQuery({
    queryKey: ["jobs", HISTORY_LIMIT],
    queryFn: () => api.listJobs(HISTORY_LIMIT),
  });
  const job = jobs.data?.find((j) => j.id === jobId) ?? null;

  const launched = useProgressStore((s) => s.launched[jobId]);
  const lines = useProgressStore((s) => s.lines[jobId]);
  const streamedOutcome = useProgressStore((s) => s.outcomes[jobId]);

  const outcome = job?.outcome ?? streamedOutcome ?? null;

  // An unfinished row does not mean a running job: a crash or a quit leaves one
  // open forever. Only this process can say which are genuinely still going.
  const active = useQuery({
    queryKey: ["active-jobs"],
    queryFn: () => api.activeJobIds(),
    refetchInterval: outcome == null ? ACTIVE_POLL_MS : false,
  });
  const running = outcome == null && (active.data?.includes(jobId) ?? false);
  const now = useTick(running);

  const startedAt = job?.started_at ?? launched?.startedAt ?? null;
  const title = job ? KIND_LABELS[job.kind] : (launched?.title ?? "Job");
  const detail = launched?.detail ?? (job ? describeJob(job) : undefined);

  // Nothing is known and nothing is coming: not in history, not running, and
  // not started from this window. An id typed by hand, or a run from a build
  // whose database has since been reset.
  const missing =
    !job && !launched && !lines && !jobs.isLoading && !active.isLoading;

  return (
    <>
      <PageHeader
        title={title}
        description={detail}
        actions={
          <Link
            to="/jobs"
            className="flex items-center gap-1.5 rounded-md border border-slate-700 px-3 py-1.5 text-xs text-slate-300 transition hover:bg-slate-800"
          >
            <ArrowLeft className="h-3.5 w-3.5" />
            All jobs
          </Link>
        }
      />

      <div className="space-y-5 p-6">
        {missing ? (
          <div className="panel p-8 text-center text-sm text-slate-500">
            No job with the id{" "}
            <span className="font-mono text-slate-400">{jobId}</span> in this
            app's history.
          </div>
        ) : (
          <>
            <section className="panel space-y-3 p-4">
              <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
                <span
                  className={cn(
                    "rounded px-2 py-0.5 text-[10px] font-semibold uppercase",
                    outcome
                      ? OUTCOME_STYLES[outcome]
                      : running
                        ? "bg-blue-500/15 text-blue-300"
                        : "bg-amber-500/15 text-amber-300",
                  )}
                >
                  {outcome ?? (running ? "running" : "interrupted")}
                </span>
                <span className="text-xs text-slate-500">
                  started {formatTimestamp(startedAt)}
                </span>
                {startedAt && (
                  <span className="text-xs tabular-nums text-slate-500">
                    {running || job?.finished_at
                      ? formatDuration(startedAt, job?.finished_at ?? null, now)
                      : "—"}
                  </span>
                )}
                <span className="ml-auto font-mono text-[11px] text-slate-600">
                  {jobId}
                </span>
              </div>

              {running && <JobProgressStrip jobId={jobId} />}

              {outcome == null && !running && job && (
                <p className="text-xs text-amber-300/70">
                  Nothing is running this job — the app was quit or restarted
                  before it finished. Start it again.
                </p>
              )}

              {job?.artifact_path && (
                <p className="break-all font-mono text-xs text-slate-500">
                  {job.artifact_path}
                </p>
              )}
            </section>

            {!job && (
              // Deliberate on the Rust side: history records what a *profile*
              // did, and a manual push has no profile behind it. Saying so is
              // better than an empty details panel the user reads as a bug.
              <p className="max-w-3xl text-xs leading-relaxed text-slate-500">
                This run is not written to job history — an off-site push is not
                attributed to a connection. It streams here while it runs, and
                is gone once the app is restarted.
              </p>
            )}

            {job && <JobFacts job={job} />}

            <Timeline
              lines={lines ?? []}
              stored={logLines(job?.log)}
              follow={running}
            />
          </>
        )}
      </div>
    </>
  );
}

/** Source, destination, and what the run was asked to do. */
function JobFacts({ job }: { job: JobRecord }) {
  const profiles = useQuery({
    queryKey: ["profiles"],
    queryFn: api.listProfiles,
  });

  const nameOf = (id: string | null) =>
    id ? (profiles.data?.find((p) => p.id === id)?.name ?? null) : null;

  const source = nameOf(job.source_profile_id);
  // A restore records the same profile at both ends; naming it twice would
  // suggest two servers were involved.
  const dest =
    job.dest_profile_id && job.dest_profile_id !== job.source_profile_id
      ? nameOf(job.dest_profile_id)
      : null;

  const facts = [
    ...(source ? [{ label: dest ? "Source" : "Connection", value: source }] : []),
    ...(dest ? [{ label: "Destination", value: dest }] : []),
    ...summariseOptions(job.kind, job.options_json),
  ];

  if (facts.length === 0) return null;

  return (
    <section className="panel divide-y divide-slate-800">
      {facts.map((f) => (
        <div key={f.label} className="flex gap-4 px-4 py-2 text-xs">
          <span className="w-28 shrink-0 text-slate-500">{f.label}</span>
          <span className="min-w-0 break-words text-slate-200">{f.value}</span>
        </div>
      ))}
    </section>
  );
}

/**
 * What the job has said so far, oldest first.
 *
 * Live events when there are any, and the stored log otherwise: a job from a
 * previous session has no stream left, but it does have the same lines written
 * down. A finished job keeps whichever it had — the transcript outlives the run.
 */
function Timeline({
  lines,
  stored,
  follow,
}: {
  lines: ProgressEvent[];
  stored: string[];
  follow: boolean;
}) {
  const box = useRef<HTMLDivElement>(null);
  const live = lines;
  const count = live.length > 0 ? live.length : stored.length;

  // Pinned to the newest line while the job runs. Scrolls the panel itself
  // rather than the page: `scrollIntoView` here would drag the whole window
  // down every second and leave the heading off screen.
  useEffect(() => {
    if (!follow) return;
    const el = box.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [count, follow]);

  return (
    <section>
      <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-slate-500">
        Timeline
      </h2>
      <div
        ref={box}
        className="panel max-h-96 overflow-y-auto p-3 font-mono text-xs"
      >
        {count === 0 ? (
          <p className="text-slate-600">
            {follow
              ? "Nothing yet — the first line arrives as soon as the job starts work."
              : "This job left no log."}
          </p>
        ) : live.length > 0 ? (
          live.map((e, i) => (
            <div key={`${e.ts}-${i}`} className="flex gap-2">
              <span className="shrink-0 text-slate-600">
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
              <span className="min-w-0 break-words text-slate-300">
                {e.table ? `${e.table} — ` : ""}
                {e.message}
              </span>
            </div>
          ))
        ) : (
          stored.map((line, i) => (
            <div
              key={i}
              className={cn(
                "break-words",
                line.includes("[ERROR]")
                  ? "text-red-400"
                  : line.includes("[WARN]")
                    ? "text-amber-400"
                    : "text-slate-400",
              )}
            >
              {line}
            </div>
          ))
        )}
      </div>
    </section>
  );
}

/** A one-line description of a job read back from history. */
function describeJob(job: JobRecord): string | undefined {
  const facts = summariseOptions(job.kind, job.options_json);
  return facts.length > 0 ? facts.map((f) => f.value).join(" · ") : undefined;
}
