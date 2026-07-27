import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";

import PageHeader from "@/components/PageHeader";
import { api } from "@/lib/api";
import { cn, formatDuration, formatTimestamp } from "@/lib/utils";
import { events, type JobOutcome, type ProgressEvent } from "@/bindings";

const OUTCOME_STYLES: Record<JobOutcome, string> = {
  success: "bg-emerald-500/15 text-emerald-300",
  failed: "bg-red-500/15 text-red-300",
  cancelled: "bg-slate-500/15 text-slate-400",
};

/** Most recent live progress lines, newest last. */
const LIVE_LIMIT = 200;

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
  const queryClient = useQueryClient();
  const [live, setLive] = useState<ProgressEvent[]>([]);

  const jobs = useQuery({ queryKey: ["jobs"], queryFn: () => api.listJobs(50) });

  useEffect(() => {
    const progress = events.jobProgress.listen((e) => {
      setLive((prev) => [...prev, e.payload].slice(-LIVE_LIMIT));
    });
    const finished = events.jobFinished.listen(() => {
      void queryClient.invalidateQueries({ queryKey: ["jobs"] });
    });

    return () => {
      void progress.then((unlisten) => unlisten());
      void finished.then((unlisten) => unlisten());
    };
  }, [queryClient]);

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
                <div key={job.id} className="flex items-center gap-4 px-4 py-3">
                  <span className="w-16 text-xs uppercase text-slate-400">
                    {job.kind}
                  </span>
                  <span className="flex-1 text-xs text-slate-500">
                    {formatTimestamp(job.started_at)}
                  </span>
                  <span className="w-20 text-right text-xs text-slate-500">
                    {formatDuration(job.started_at, job.finished_at)}
                  </span>
                  <span
                    className={cn(
                      "w-20 rounded px-2 py-0.5 text-center text-[10px] font-semibold uppercase",
                      job.outcome
                        ? OUTCOME_STYLES[job.outcome]
                        : "bg-blue-500/15 text-blue-300",
                    )}
                  >
                    {job.outcome ?? "running"}
                  </span>
                </div>
              ))}
            </div>
          )}
        </section>
      </div>
    </>
  );
}
