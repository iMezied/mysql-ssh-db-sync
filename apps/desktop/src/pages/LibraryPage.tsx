import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Check,
  CloudUpload,
  FileArchive,
  ShieldAlert,
  ShieldCheck,
  Trash2,
  TrendingDown,
} from "lucide-react";

import PageHeader from "@/components/PageHeader";
import { api } from "@/lib/api";
import { formatBytes, formatTimestamp } from "@/lib/utils";
import type {
  Artifact,
  DatabaseStats,
  IntegrityCheck,
  LibraryStats,
} from "@/bindings";

export default function LibraryPage() {
  const queryClient = useQueryClient();
  const [checks, setChecks] = useState<Record<string, IntegrityCheck>>({});

  const directory = useQuery({
    queryKey: ["backup-dir"],
    queryFn: api.backupDirectory,
  });

  const artifacts = useQuery({
    queryKey: ["artifacts"],
    queryFn: () => api.listArtifacts(null),
  });

  const stats = useQuery({
    queryKey: ["library-stats"],
    queryFn: () => api.libraryStats(null),
  });

  const check = useMutation({
    mutationFn: (path: string) => api.checkArtifact(path),
    onSuccess: (result, path) =>
      setChecks((prev) => ({ ...prev, [path]: result })),
  });

  const remove = useMutation({
    mutationFn: (path: string) => api.deleteArtifact(path),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["artifacts"] }),
  });

  // For artifacts taken before a destination was configured, and for retrying
  // one whose upload failed. Progress and per-destination results arrive on
  // the normal job event stream, so this returns as soon as the job starts.
  const destinations = useQuery({
    queryKey: ["destinations"],
    queryFn: api.listDestinations,
  });
  const push = useMutation({
    mutationFn: (path: string) => api.pushArtifactOffsite(path),
  });
  const enabledDestinations =
    destinations.data?.filter((d) => d.enabled).length ?? 0;

  return (
    <>
      <PageHeader
        title="Library"
        description="Every artifact this app has produced. Each carries a manifest recording how it was taken and a checksum to prove it is intact."
      />

      <div className="space-y-4 p-6">
        {directory.data && (
          <p className="font-mono text-xs text-slate-600">{directory.data}</p>
        )}

        {artifacts.isLoading && (
          <p className="text-sm text-slate-500">Reading the backup directory…</p>
        )}

        {artifacts.data?.length === 0 && (
          <div className="panel flex flex-col items-center gap-2 p-10 text-center">
            <FileArchive className="h-6 w-6 text-slate-600" />
            <p className="text-sm text-slate-400">No backups yet.</p>
            <p className="text-sm text-slate-500">
              Run one from the Backup page and it will appear here.
            </p>
          </div>
        )}

        {stats.data && (stats.data.total_artifacts ?? 0) > 0 && (
          <LibrarySummary stats={stats.data} />
        )}

        <div className="grid gap-2">
          {artifacts.data?.map((a) => (
            <ArtifactRow
              key={a.path}
              artifact={a}
              check={checks[a.path]}
              checking={check.isPending && check.variables === a.path}
              deleting={remove.isPending && remove.variables === a.path}
              pushing={push.isPending && push.variables === a.path}
              offsiteCount={enabledDestinations}
              onCheck={() => check.mutate(a.path)}
              onDelete={() => remove.mutate(a.path)}
              onPush={() => push.mutate(a.path)}
            />
          ))}
        </div>
      </div>
    </>
  );
}

/**
 * Size and growth across the library.
 *
 * The chart is the least of it. The part worth having is the shrink warning:
 * a backup that came out at a fraction of the one before it usually means a
 * table stopped being selected or a dump was truncated, and nothing else in
 * the app notices — the artifact is valid, its checksum matches, and a restore
 * of it succeeds. It is only wrong relative to yesterday.
 */
function LibrarySummary({ stats }: { stats: LibraryStats }) {
  const shrinks = stats.databases.flatMap((d) => d.shrinks);

  return (
    <section className="space-y-3">
      {shrinks.length > 0 && (
        <div className="flex gap-3 rounded-lg border border-amber-500/40 bg-amber-500/5 p-4">
          <TrendingDown className="mt-0.5 h-4 w-4 shrink-0 text-amber-400" />
          <div className="space-y-2 text-xs leading-relaxed text-amber-200/90">
            <p>
              <strong className="font-semibold">
                {shrinks.length === 1
                  ? "One backup is"
                  : `${shrinks.length} backups are`}{" "}
                far smaller than the one before.
              </strong>{" "}
              That is usually a table that stopped being selected, a dump that
              was truncated, or a row filter that started matching nothing.
              Nothing else flags it: the artifact is valid and it restores.
            </p>
            <ul className="space-y-1 font-mono text-[11px] text-amber-200/80">
              {shrinks.slice(0, 5).map((s) => (
                <li key={s.filename}>
                  {s.filename} — {formatBytes(s.bytes)}, was{" "}
                  {formatBytes(s.previous_bytes)} ({s.previous_filename})
                </li>
              ))}
            </ul>
          </div>
        </div>
      )}

      <div className="panel divide-y divide-slate-800">
        <div className="flex flex-wrap gap-x-6 gap-y-1 px-4 py-3 text-xs text-slate-400">
          <span>
            <strong className="text-slate-200">{stats.total_artifacts}</strong>{" "}
            artifacts
          </span>
          <span>
            <strong className="text-slate-200">
              {formatBytes(stats.total_bytes)}
            </strong>{" "}
            on disk
          </span>
          <span>
            <strong className="text-slate-200">{stats.databases.length}</strong>{" "}
            {stats.databases.length === 1 ? "database" : "databases"}
          </span>
          {(stats.unattributed ?? 0) > 0 && (
            // Counted, not hidden: they occupy the same disk, and a total that
            // quietly excluded them would understate what is there.
            <span className="text-slate-500">
              {stats.unattributed} without a manifest (
              {formatBytes(stats.unattributed_bytes)})
            </span>
          )}
        </div>

        {stats.databases.map((d) => (
          <DatabaseRow key={d.database} stats={d} />
        ))}
      </div>
    </section>
  );
}

function DatabaseRow({ stats }: { stats: DatabaseStats }) {
  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-1 px-4 py-2.5">
      <span className="min-w-32 font-mono text-sm text-slate-200">
        {stats.database}
      </span>
      <Sparkline points={stats.series.map((p) => p.bytes ?? 0)} />
      <span className="text-xs text-slate-400">
        {formatBytes(stats.newest_bytes)} latest
      </span>
      <span className="text-xs text-slate-500">
        {stats.artifacts} {stats.artifacts === 1 ? "artifact" : "artifacts"},{" "}
        {formatBytes(stats.total_bytes)} total
      </span>
      <Growth bytesPerDay={stats.bytes_per_day} />
    </div>
  );
}

function Growth({ bytesPerDay }: { bytesPerDay: number | null }) {
  // Absent with a single artifact. Inventing a rate from one point produces a
  // number that gets quoted back later as if it meant something.
  if (bytesPerDay === null) {
    return <span className="text-xs text-slate-600">no trend yet</span>;
  }
  if (Math.abs(bytesPerDay) < 1024) {
    return <span className="text-xs text-slate-500">flat</span>;
  }
  const growing = bytesPerDay > 0;
  return (
    <span
      className={`text-xs ${growing ? "text-slate-400" : "text-slate-500"}`}
      title="Average change per day across the whole span"
    >
      {growing ? "+" : "−"}
      {formatBytes(Math.abs(bytesPerDay))}/day
    </span>
  );
}

/** A bare trend line. No axes: the numbers beside it are the precise part. */
function Sparkline({ points }: { points: number[] }) {
  if (points.length < 2) {
    return <span className="w-24 text-xs text-slate-600">—</span>;
  }
  const max = Math.max(...points);
  const min = Math.min(...points);
  const span = max - min || 1;
  const width = 96;
  const height = 20;

  const path = points
    .map((value, i) => {
      const x = (i / (points.length - 1)) * width;
      const y = height - ((value - min) / span) * height;
      return `${i === 0 ? "M" : "L"}${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");

  return (
    <svg
      width={width}
      height={height}
      className="shrink-0"
      role="img"
      aria-label={`${points.length} backups, ${formatBytes(points[0])} to ${formatBytes(points[points.length - 1])}`}
    >
      <path d={path} fill="none" stroke="currentColor" strokeWidth="1.5" className="text-blue-400" />
    </svg>
  );
}

function ArtifactRow({
  artifact,
  check,
  checking,
  deleting,
  pushing,
  offsiteCount,
  onCheck,
  onDelete,
  onPush,
}: {
  artifact: Artifact;
  check: IntegrityCheck | undefined;
  checking: boolean;
  deleting: boolean;
  pushing: boolean;
  offsiteCount: number;
  onCheck: () => void;
  onDelete: () => void;
  onPush: () => void;
}) {
  return (
    <div className="panel px-4 py-3">
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <div className="truncate font-mono text-sm text-slate-100">
            {artifact.filename}
          </div>
          <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1 text-xs text-slate-500">
            <span>{formatBytes(artifact.size_bytes)}</span>
            <span>{formatTimestamp(artifact.modified_at)}</span>
            {artifact.database && (
              <span>
                {artifact.database}
                {artifact.engine ? ` · ${artifact.engine}` : ""}
              </span>
            )}
            {artifact.source_profile_name && (
              <span>from {artifact.source_profile_name}</span>
            )}
            {artifact.table_count != null && (
              <span>
                {artifact.tables_with_data ?? 0} of {artifact.table_count} tables
                with data
              </span>
            )}
          </div>

          {!artifact.has_manifest && (
            <p className="mt-1.5 text-xs text-amber-400">
              No manifest alongside this file — its contents and checksum are
              unknown, so a restore cannot verify it first.
            </p>
          )}

          {check && <IntegrityNote check={check} />}
        </div>

        <div className="flex shrink-0 items-center gap-2">
          <button
            onClick={onCheck}
            disabled={checking}
            title="Hash the file and compare it against the manifest"
            className="rounded-md border border-slate-700 px-2.5 py-1.5 text-xs text-slate-300 transition hover:bg-slate-800 disabled:opacity-50"
          >
            {checking ? "Checking…" : "Verify"}
          </button>
          <button
            onClick={onPush}
            disabled={pushing || offsiteCount === 0}
            title={
              offsiteCount === 0
                ? "No enabled off-site destinations"
                : `Upload this artifact and its manifest to ${offsiteCount} destination(s)`
            }
            className="inline-flex items-center gap-1.5 rounded-md border border-slate-700 px-2.5 py-1.5 text-xs text-slate-300 transition hover:bg-slate-800 disabled:opacity-40"
          >
            <CloudUpload className="h-3.5 w-3.5" />
            {pushing ? "Sending…" : "Send off-site"}
          </button>
          <button
            onClick={onDelete}
            disabled={deleting}
            title="Delete this artifact and its manifest"
            className="rounded p-1.5 text-slate-500 transition hover:bg-red-500/10 hover:text-red-400 disabled:opacity-40"
          >
            <Trash2 className="h-4 w-4" />
          </button>
        </div>
      </div>
    </div>
  );
}

function IntegrityNote({ check }: { check: IntegrityCheck }) {
  if (check.status === "ok") {
    return (
      <p className="mt-1.5 flex items-center gap-1.5 text-xs text-emerald-400">
        <ShieldCheck className="h-3.5 w-3.5" />
        Checksum matches the manifest.
      </p>
    );
  }

  if (check.status === "no_manifest") {
    return (
      <p className="mt-1.5 flex items-center gap-1.5 text-xs text-slate-500">
        <Check className="h-3.5 w-3.5" />
        Nothing to check against.
      </p>
    );
  }

  return (
    <div className="mt-1.5 rounded border border-red-500/40 bg-red-500/5 p-2">
      <p className="flex items-center gap-1.5 text-xs font-medium text-red-300">
        <ShieldAlert className="h-3.5 w-3.5" />
        {check.status === "mismatch"
          ? "This file does not match its manifest — do not restore it"
          : "Could not read this file"}
      </p>
      {check.status === "mismatch" && (
        <dl className="mt-1 space-y-0.5 font-mono text-[11px] text-slate-500">
          <div className="flex gap-2">
            <dt className="w-14 shrink-0">expected</dt>
            <dd className="break-all">{check.expected}</dd>
          </div>
          <div className="flex gap-2">
            <dt className="w-14 shrink-0">actual</dt>
            <dd className="break-all text-red-300">{check.actual}</dd>
          </div>
        </dl>
      )}
      {check.status === "unreadable" && (
        <p className="mt-1 text-[11px] text-slate-500">{check.detail}</p>
      )}
    </div>
  );
}
