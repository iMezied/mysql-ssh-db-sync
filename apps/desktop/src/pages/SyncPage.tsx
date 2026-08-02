import { useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useMutation, useQuery } from "@tanstack/react-query";
import { AlertTriangle, ArrowRight, Play } from "lucide-react";

import PageHeader from "@/components/PageHeader";
import { api } from "@/lib/api";
import { useProgressStore } from "@/lib/jobProgress";
import { cn } from "@/lib/utils";
import type {
  ConnectionProfile,
  EngineBackupOptions,
  EngineRestoreOptions,
  SyncRequest,
  TableMode,
} from "@/bindings";
import {
  defaultBackupOptions,
  defaultRestoreOptions,
  supportsSchemaOnly,
} from "@/lib/engineDefaults";
import TableSelector from "@/components/TableSelector";
import { keyOf, materialise } from "@/lib/tableSelection";

/**
 * Source → plan → destination, in one job.
 *
 * The review step spells out exactly what will happen before anything runs;
 * this is the screen most likely to be pointed at production.
 */
export default function SyncPage() {
  const [sourceId, setSourceId] = useState("");
  const [destId, setDestId] = useState("");
  const [database, setDatabase] = useState("");
  const [modes, setModes] = useState<Record<string, TableMode>>({});
  const [prefix, setPrefix] = useState("sync");
  const [verify, setVerify] = useState(true);
  const [keepLast, setKeepLast] = useState<number | null>(null);

  const navigate = useNavigate();
  const noteLaunch = useProgressStore((s) => s.noteLaunch);

  const profiles = useQuery({ queryKey: ["profiles"], queryFn: api.listProfiles });
  const backupDir = useQuery({
    queryKey: ["backup-dir"],
    queryFn: api.backupDirectory,
  });

  const databases = useQuery({
    queryKey: ["databases", sourceId],
    queryFn: () => api.listDatabases(sourceId),
    enabled: sourceId !== "",
  });

  const tables = useQuery({
    queryKey: ["tables", sourceId, database],
    queryFn: () => api.listTables(sourceId, database),
    enabled: sourceId !== "" && database !== "",
  });

  const source = profiles.data?.find((p) => p.id === sourceId);
  const dest = profiles.data?.find((p) => p.id === destId);

  const engineMismatch =
    source && dest && source.engine !== dest.engine ? { source, dest } : null;

  // `mongodump` writes a collection whole or not at all, and the engine
  // refuses a schema-only selection for it — so the old hardcoded
  // "schema_only" default made every MongoDB sync fail before it started.
  const defaultMode: TableMode = supportsSchemaOnly(source?.engine ?? "mysql")
    ? "schema_only"
    : "schema_and_data";

  const withData = useMemo(
    () =>
      (tables.data ?? []).filter(
        (t) => (modes[keyOf(t)] ?? defaultMode) === "schema_and_data",
      ),
    [tables.data, modes, defaultMode],
  );

  const engineBackupOptions = (): EngineBackupOptions =>
    defaultBackupOptions(source?.engine ?? "mysql");

  const engineRestoreOptions = (): EngineRestoreOptions =>
    defaultRestoreOptions(source?.engine ?? "mysql");

  const start = useMutation({
    mutationFn: () => {
      const request: SyncRequest = {
        backup: {
          common: {
            database,
            selections: materialise(tables.data ?? [], modes, defaultMode),
            output_dir: backupDir.data ?? "",
            compress: true,
            encrypt: false,
          },
          engine: engineBackupOptions(),
        },
        // Always a fresh timestamped database: the wizard never offers a
        // destructive target, because this is the screen most likely to be
        // aimed at production by mistake.
        naming: { strategy: "new_timestamped", prefix },
        restore: engineRestoreOptions(),
        verify,
        retention: keepLast ? { keep_last: keepLast, max_age_days: null } : null,
        typed_confirmation: null,
      };
      return api.startSync(sourceId, destId, request);
    },
    onSuccess: (jobId) => {
      noteLaunch(jobId, {
        title: "Sync",
        detail: `${source?.name ?? "source"} → ${dest?.name ?? "destination"}`,
      });
      navigate(`/jobs/${jobId}`);
    },
  });

  const ready =
    sourceId !== "" &&
    destId !== "" &&
    sourceId !== destId &&
    database !== "" &&
    !engineMismatch &&
    (tables.data?.length ?? 0) > 0;

  return (
    <>
      <PageHeader
        title="Sync"
        description="Back up a source and restore it to another server in one job, then verify the result."
      />

      <div className="space-y-5 p-6">
        <section className="grid grid-cols-2 gap-4">
          <ProfilePicker
            label="Source"
            value={sourceId}
            profiles={profiles.data ?? []}
            exclude={destId}
            onChange={(id) => {
              setSourceId(id);
              setDatabase("");
              setModes({});
            }}
          />
          <ProfilePicker
            label="Destination"
            value={destId}
            profiles={profiles.data ?? []}
            exclude={sourceId}
            onChange={setDestId}
          />
        </section>

        {engineMismatch && (
          <Warning>
            {engineMismatch.source.name} is {engineMismatch.source.engine} and{" "}
            {engineMismatch.dest.name} is {engineMismatch.dest.engine}. Copying
            between engines is a migration, not a sync — nothing here translates
            SQL dialects.
          </Warning>
        )}

        {dest?.environment === "prod" && (
          <Warning>
            The destination is tagged <strong>production</strong>. This creates a
            new timestamped database and never drops an existing one, but check
            you meant this server.
          </Warning>
        )}

        {sourceId && (
          <label className="block max-w-sm">
            <span className="field-label">Database</span>
            <select
              className="field-input"
              value={database}
              disabled={databases.isLoading}
              onChange={(e) => {
                setDatabase(e.target.value);
                setModes({});
              }}
            >
              <option value="">
                {databases.isLoading ? "Loading…" : "Select a database…"}
              </option>
              {databases.data?.map((d) => (
                <option key={d.name} value={d.name}>
                  {d.name}
                </option>
              ))}
            </select>
          </label>
        )}

        {tables.data && (
          <section className="space-y-3">
            <div className="flex flex-wrap items-center gap-3">
              <h2 className="text-sm font-medium text-slate-200">Tables</h2>
              <span className="text-xs text-slate-500">
                {withData.length} of {tables.data.length} carry data
              </span>
            </div>

            <TableSelector
              tables={tables.data}
              value={modes}
              onChange={setModes}
              engine={source?.engine ?? "mysql"}
              defaultMode={defaultMode}
            />
          </section>
        )}

        {ready && (
          <section className="panel space-y-3 p-4">
            <h2 className="text-sm font-semibold text-slate-200">Review</h2>

            <div className="flex flex-wrap items-center gap-2 text-sm">
              <span className="rounded bg-slate-800 px-2 py-1 font-mono text-xs text-slate-200">
                {source?.name}/{database}
              </span>
              <ArrowRight className="h-4 w-4 text-slate-600" />
              <span className="rounded bg-slate-800 px-2 py-1 font-mono text-xs text-slate-200">
                {dest?.name}/{prefix}_&lt;timestamp&gt;
              </span>
            </div>

            <ol className="space-y-1 text-xs text-slate-400">
              <li>
                1. Dump {tables.data?.length ?? 0} tables from{" "}
                <span className="font-mono">{database}</span> ({withData.length}{" "}
                with data)
              </li>
              <li>2. Create a new database on {dest?.name} and restore into it</li>
              <li>
                3.{" "}
                {verify
                  ? "Compare exact row counts on both sides"
                  : "Skip verification"}
              </li>
              <li>
                4.{" "}
                {keepLast
                  ? `Keep the newest ${keepLast} backup(s), delete older ones`
                  : "Keep every backup"}
              </li>
            </ol>

            <div className="flex flex-wrap items-end gap-4 border-t border-slate-800 pt-3">
              <label className="w-40">
                <span className="field-label">Target prefix</span>
                <input
                  className="field-input"
                  value={prefix}
                  onChange={(e) => setPrefix(e.target.value)}
                />
              </label>

              <label className="flex items-center gap-2 pb-2 text-xs text-slate-300">
                <input
                  type="checkbox"
                  checked={verify}
                  onChange={(e) => setVerify(e.target.checked)}
                  className="h-4 w-4 rounded border-slate-600 bg-slate-900"
                />
                Verify row counts afterwards
              </label>

              <label className="flex items-center gap-2 pb-2 text-xs text-slate-300">
                <input
                  type="checkbox"
                  checked={keepLast !== null}
                  onChange={(e) => setKeepLast(e.target.checked ? 5 : null)}
                  className="h-4 w-4 rounded border-slate-600 bg-slate-900"
                />
                Keep only the newest
                <input
                  type="number"
                  min={1}
                  disabled={keepLast === null}
                  value={keepLast ?? 5}
                  onChange={(e) => setKeepLast(Number(e.target.value))}
                  className="w-14 rounded border border-slate-700 bg-slate-950 px-1.5 py-1 text-xs disabled:opacity-40"
                />
                backups
              </label>
            </div>

            <div className="flex items-center gap-3 pt-1">
              <button
                onClick={() => start.mutate()}
                disabled={start.isPending}
                className="flex items-center gap-1.5 rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
              >
                <Play className="h-4 w-4" />
                {start.isPending ? "Starting…" : "Run sync"}
              </button>
            </div>

            {start.isError && (
              <p className="text-xs text-red-400">
                {(start.error as Error).message}
              </p>
            )}
          </section>
        )}
      </div>
    </>
  );
}

function ProfilePicker({
  label,
  value,
  profiles,
  exclude,
  onChange,
}: {
  label: string;
  value: string;
  profiles: ConnectionProfile[];
  exclude: string;
  onChange: (id: string) => void;
}) {
  return (
    <label>
      <span className="field-label">{label}</span>
      <select
        className="field-input"
        value={value}
        onChange={(e) => onChange(e.target.value)}
      >
        <option value="">Select a connection…</option>
        {profiles
          // Syncing a server to itself would restore alongside the source.
          .filter((p) => p.id !== exclude)
          .map((p) => (
            <option key={p.id} value={p.id}>
              {p.name} ({p.engine}, {p.environment})
            </option>
          ))}
      </select>
    </label>
  );
}

function Warning({ children }: { children: React.ReactNode }) {
  return (
    <div
      className={cn(
        "flex items-start gap-2 rounded-md border p-3",
        "border-amber-500/40 bg-amber-500/5",
      )}
    >
      <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-amber-400" />
      <p className="text-xs leading-relaxed text-amber-200/90">{children}</p>
    </div>
  );
}
