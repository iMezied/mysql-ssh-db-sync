import { useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { ArrowLeft } from "lucide-react";

import PageHeader from "@/components/PageHeader";
import JobProgressStrip from "@/components/JobProgressStrip";
import { api } from "@/lib/api";
import { KIND_LABELS, logLines, summariseOptions } from "@/lib/jobDetails";
import {
  STEP_KIND_LABELS,
  type StepStatus,
  stepDurationMs,
  stepStatus,
  stepSummary,
  worthShowing,
} from "@/lib/jobSteps";
import EngineMark from "@/components/EngineMark";
import EnvironmentBadge from "@/components/EnvironmentBadge";
import { useProgressStore } from "@/lib/jobProgress";
import { useTick } from "@/lib/useTick";
import { cn, formatElapsed, formatDuration, formatTimestamp } from "@/lib/utils";
import type {
  ConnectionProfile,
  JobOutcome,
  JobRecord,
  JobStep,
  ProgressEvent,
} from "@/bindings";

const OUTCOME_STYLES: Record<JobOutcome, string> = {
  success: "bg-emerald-500/15 text-emerald-300",
  failed: "bg-red-500/15 text-red-300",
  cancelled: "bg-slate-500/15 text-slate-400",
};

const STEP_STYLES: Record<StepStatus, string> = {
  success: "bg-emerald-500/15 text-emerald-300",
  failed: "bg-red-500/15 text-red-300",
  cancelled: "bg-slate-500/15 text-slate-400",
  skipped: "bg-slate-500/15 text-slate-500",
  running: "bg-blue-500/15 text-blue-300",
  pending: "bg-slate-800 text-slate-500",
};

/**
 * Slower than the event stream on purpose. Steps change a handful of times in
 * a run; the live detail inside one arrives on `JobProgress` already.
 */
const STEPS_POLL_MS = 2_000;

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
  const job = jobs.data?.jobs.find((j) => j.id === jobId) ?? null;

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

  const steps = useQuery({
    queryKey: ["job-steps", jobId],
    queryFn: () => api.listJobSteps(jobId),
    refetchInterval: running ? STEPS_POLL_MS : false,
  });

  // Clicking a step narrows the timeline to it. Null is "show everything",
  // which is where a six-step run has to start — the reader does not yet know
  // which part they care about.
  const [focused, setFocused] = useState<number | null>(null);

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

            {worthShowing(steps.data ?? []) && (
              <StepList
                steps={steps.data ?? []}
                now={now}
                focused={focused}
                onFocus={(index) =>
                  setFocused((current) => (current === index ? null : index))
                }
              />
            )}

            <Timeline
              lines={lines ?? []}
              stored={logLines(job?.log)}
              follow={running}
              focused={focused}
              onClearFocus={() => setFocused(null)}
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

  const profileOf = (id: string | null) =>
    id ? (profiles.data?.find((p) => p.id === id) ?? null) : null;

  const source = profileOf(job.source_profile_id);
  // A restore records the same profile at both ends; naming it twice would
  // suggest two servers were involved.
  const dest =
    job.dest_profile_id && job.dest_profile_id !== job.source_profile_id
      ? profileOf(job.dest_profile_id)
      : null;

  // Read long after the run, this is often the only record of which server was
  // touched — worth saying in the same shape the picker used, tag and all.
  const describe = (p: ConnectionProfile) => (
    <span className="flex flex-wrap items-center gap-1.5">
      <EngineMark engine={p.engine} size="sm" />
      {p.name}
      <EnvironmentBadge environment={p.environment} />
    </span>
  );

  const facts = [
    ...(source
      ? [{ label: dest ? "Source" : "Connection", value: describe(source) }]
      : []),
    ...(dest ? [{ label: "Destination", value: describe(dest) }] : []),
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
 * The shape of a composite run: what it set out to do, and how far it got.
 *
 * Every step is planned before the first one starts, so a run that died at the
 * restore shows the verification it never reached as skipped rather than
 * leaving the reader to infer it from a log that stops.
 */
function StepList({
  steps,
  now,
  focused,
  onFocus,
}: {
  steps: JobStep[];
  now: number;
  focused: number | null;
  onFocus: (index: number) => void;
}) {
  return (
    <section>
      <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-slate-500">
        Steps
      </h2>
      <div className="panel divide-y divide-slate-800">
        {steps.map((step) => {
          const status = stepStatus(step);
          const ms = stepDurationMs(step, now);
          const summary = stepSummary(step);
          const isFocused = focused === step.index;

          return (
            <button
              key={step.index}
              type="button"
              onClick={() => onFocus(step.index)}
              className={cn(
                "flex w-full items-baseline gap-3 px-4 py-2.5 text-left transition hover:bg-slate-800/40",
                isFocused && "bg-blue-600/10",
              )}
            >
              <span className="w-4 shrink-0 tabular-nums text-xs text-slate-600">
                {step.index}
              </span>

              <span className="min-w-0 flex-1">
                <span className="flex flex-wrap items-baseline gap-x-2">
                  <span
                    className={cn(
                      "text-sm",
                      status === "pending" || status === "skipped"
                        ? "text-slate-500"
                        : "text-slate-200",
                    )}
                  >
                    {step.label}
                  </span>
                  <span className="text-[11px] uppercase tracking-wide text-slate-600">
                    {STEP_KIND_LABELS[step.kind]}
                  </span>
                </span>
                {summary && (
                  <span
                    className={cn(
                      "mt-0.5 block break-words text-xs",
                      status === "failed" ? "text-red-300/80" : "text-slate-500",
                    )}
                  >
                    {summary}
                  </span>
                )}
              </span>

              {ms != null && (
                <span className="shrink-0 text-xs tabular-nums text-slate-500">
                  {formatElapsed(ms)}
                </span>
              )}
              <span
                className={cn(
                  "shrink-0 rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase",
                  STEP_STYLES[status],
                )}
              >
                {status}
              </span>
            </button>
          );
        })}
      </div>
      <p className="mt-1.5 text-[11px] text-slate-600">
        {focused == null
          ? "Select a step to narrow the timeline to it."
          : `Timeline is showing step ${focused} only.`}
      </p>
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
  focused,
  onClearFocus,
}: {
  lines: ProgressEvent[];
  stored: string[];
  follow: boolean;
  focused: number | null;
  onClearFocus: () => void;
}) {
  const box = useRef<HTMLDivElement>(null);

  // Which source to read is decided before filtering, not after. Narrowing to
  // a step the live stream has not reached yet would otherwise fall through to
  // the stored log and swap the whole panel to a different source mid-run.
  const useLive = lines.length > 0;
  const live = useLive
    ? focused == null
      ? lines
      : lines.filter((e) => e.step === focused)
    : [];
  // The stored log is matched on the `[2/5]` marker `to_log_line` writes: a job
  // from a previous session has no structured events left, only that text.
  const shown = useLive
    ? []
    : focused == null
      ? stored
      : stored.filter((line) => line.includes(`[${focused}/`));
  const count = live.length + shown.length;
  const emptyBecauseFiltered = focused != null && (useLive || stored.length > 0);

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
      <h2 className="mb-2 flex items-baseline gap-2 text-xs font-semibold uppercase tracking-wide text-slate-500">
        Timeline
        {focused != null && (
          <button
            type="button"
            onClick={onClearFocus}
            className="rounded border border-slate-700 px-1.5 py-0.5 text-[10px] font-medium normal-case tracking-normal text-slate-400 transition hover:bg-slate-800"
          >
            step {focused} only — show all
          </button>
        )}
      </h2>
      <div
        ref={box}
        className="panel max-h-96 overflow-y-auto p-3 font-mono text-xs"
      >
        {count === 0 ? (
          <p className="text-slate-600">
            {emptyBecauseFiltered
              ? `Step ${focused} has said nothing yet.`
              : follow
                ? "Nothing yet — the first line arrives as soon as the job starts work."
                : "This job left no log."}
          </p>
        ) : live.length > 0 ? (
          live.map((e, i) => (
            <div key={`${e.ts}-${i}`} className="flex gap-2">
              <span className="shrink-0 text-slate-600">
                {new Date(e.ts).toLocaleTimeString()}
              </span>
              {/* Only while showing every step — inside a filtered view the
                  number is the same on every line. */}
              {focused == null && e.step != null && (
                <span className="w-6 shrink-0 text-right text-slate-700">
                  {e.step}
                </span>
              )}
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
          shown.map((line, i) => (
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
