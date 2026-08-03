import {
  PHASE_LABELS,
  describeProgress,
  remainingMs,
  useProgressStore,
} from "@/lib/jobProgress";
import { formatBytes, formatElapsed } from "@/lib/utils";

/**
 * Phase, position and estimate for a job this process is running now.
 *
 * Reads the store rather than taking a progress prop so that the Jobs list and
 * a single job's page cannot drift apart: both show the same sample of the same
 * run, whichever one the user happens to be looking at.
 */
export default function JobProgressStrip({ jobId }: { jobId: string }) {
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
        {/* Only for a job that has more than one step. On a plain backup
            "Step 1 of 1" is noise dressed up as information. */}
        {latest.step != null &&
          latest.step_total != null &&
          latest.step_total > 1 && (
            <span className="shrink-0 tabular-nums text-slate-500">
              Step {latest.step} of {latest.step_total}
            </span>
          )}
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
        {latest.rows != null && <span>{latest.rows.toLocaleString()} rows</span>}
        {/* Prefixed with a tilde, always: it is extrapolated from throughput so
            far, and a bare "3m 20s" reads as a promise the job cannot keep. */}
        {left != null && (
          <span className="text-slate-400">~{formatElapsed(left)} left</span>
        )}
      </div>
    </div>
  );
}
