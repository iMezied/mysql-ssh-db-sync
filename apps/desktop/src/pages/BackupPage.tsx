import { useMemo, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { AlertTriangle, Database, Play, Search } from "lucide-react";

import PageHeader from "@/components/PageHeader";
import { api } from "@/lib/api";
import { cn, formatBytes } from "@/lib/utils";
import type { BackupRequest, TableInfo } from "@/bindings";

type TableMode = "schema_and_data" | "schema_only" | "exclude";

const MODE_LABELS: Record<TableMode, string> = {
  schema_and_data: "Schema + data",
  schema_only: "Schema only",
  exclude: "Exclude",
};

const MODE_STYLES: Record<TableMode, string> = {
  schema_and_data: "bg-blue-600 text-white",
  schema_only: "bg-slate-700 text-slate-200",
  exclude: "bg-slate-800 text-slate-500",
};

function keyOf(t: TableInfo): string {
  return t.schema ? `${t.schema}.${t.name}` : t.name;
}

/**
 * Source browser and table selection.
 *
 * Replaces the old `tables.conf`: every table defaults to schema-only and the
 * user promotes the ones that need rows. Running the backup lands in M2′.
 */
export default function BackupPage() {
  const [profileId, setProfileId] = useState("");
  const [database, setDatabase] = useState("");
  const [filter, setFilter] = useState("");
  const [modes, setModes] = useState<Record<string, TableMode>>({});
  const [started, setStarted] = useState<string | null>(null);

  const profiles = useQuery({ queryKey: ["profiles"], queryFn: api.listProfiles });
  const backupDir = useQuery({
    queryKey: ["backup-dir"],
    queryFn: api.backupDirectory,
  });

  const databases = useQuery({
    queryKey: ["databases", profileId],
    queryFn: () => api.listDatabases(profileId),
    enabled: profileId !== "",
  });

  const tables = useQuery({
    queryKey: ["tables", profileId, database],
    queryFn: () => api.listTables(profileId, database),
    enabled: profileId !== "" && database !== "",
  });

  const visible = useMemo(() => {
    const rows = tables.data ?? [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return rows;
    return rows.filter((t) => keyOf(t).toLowerCase().includes(needle));
  }, [tables.data, filter]);

  const counts = useMemo(() => {
    const rows = tables.data ?? [];
    const modeOf = (t: TableInfo) => modes[keyOf(t)] ?? "schema_only";
    return {
      withData: rows.filter((t) => modeOf(t) === "schema_and_data").length,
      schemaOnly: rows.filter((t) => modeOf(t) === "schema_only").length,
      excluded: rows.filter((t) => modeOf(t) === "exclude").length,
      total: rows.length,
    };
  }, [tables.data, modes]);

  const nonTransactionalSelected = useMemo(
    () =>
      (tables.data ?? []).filter(
        (t) =>
          !t.transactional &&
          (modes[keyOf(t)] ?? "schema_only") === "schema_and_data",
      ),
    [tables.data, modes],
  );

  const start = useMutation({
    mutationFn: () => {
      const rows = tables.data ?? [];
      const request: BackupRequest = {
        common: {
          database,
          selections: rows.map((t) => ({
            name: t.name,
            mode: modes[keyOf(t)] ?? "schema_only",
            where_filter: null,
          })),
          output_dir: backupDir.data ?? "",
          compress: true,
          encrypt: false,
        },
        engine: {
          engine: "mysql",
          single_transaction: true,
          hex_blob: true,
          set_gtid_purged_off: true,
          add_drop_table: true,
          extended_insert: true,
          routines: true,
          triggers: true,
          events: true,
          default_character_set: "utf8mb4",
          disable_column_statistics: false,
          strip_definer: true,
          parallel_threads: null,
          extra_flags: [],
        },
      };
      return api.startBackup(profileId, request);
    },
    onSuccess: (jobId) => setStarted(jobId),
  });

  const selectedProfile = profiles.data?.find((p) => p.id === profileId);
  const canRun =
    profileId !== "" &&
    database !== "" &&
    (tables.data?.length ?? 0) > 0 &&
    selectedProfile?.engine === "mysql";

  const setVisibleTo = (mode: TableMode) => {
    setModes((prev) => {
      const next = { ...prev };
      for (const t of visible) next[keyOf(t)] = mode;
      return next;
    });
  };

  return (
    <>
      <PageHeader
        title="Backup"
        description="Choose a source, then decide per table whether it carries data. Everything defaults to schema-only."
      />

      <div className="space-y-4 p-6">
        <div className="flex flex-wrap gap-3">
          <label className="min-w-56 flex-1">
            <span className="field-label">Source connection</span>
            <select
              className="field-input"
              value={profileId}
              onChange={(e) => {
                setProfileId(e.target.value);
                setDatabase("");
                setModes({});
              }}
            >
              <option value="">Select a connection…</option>
              {profiles.data?.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name} ({p.engine})
                </option>
              ))}
            </select>
          </label>

          <label className="min-w-56 flex-1">
            <span className="field-label">Database</span>
            <select
              className="field-input"
              value={database}
              disabled={!profileId || databases.isLoading}
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
        </div>

        {databases.isError && (
          <ErrorNote
            title="Could not list databases"
            detail={(databases.error as Error).message}
          />
        )}

        {tables.isError && (
          <ErrorNote
            title="Could not list tables"
            detail={(tables.error as Error).message}
          />
        )}

        {!profileId && (
          <div className="panel flex items-center gap-3 p-8 text-sm text-slate-500">
            <Database className="h-5 w-5" />
            Pick a connection to browse its tables.
          </div>
        )}

        {tables.isLoading && database && (
          <p className="text-sm text-slate-500">Introspecting…</p>
        )}

        {tables.data && (
          <>
            <div className="flex flex-wrap items-center gap-3">
              <div className="relative min-w-64 flex-1">
                <Search className="pointer-events-none absolute left-2.5 top-2.5 h-4 w-4 text-slate-600" />
                <input
                  className="field-input pl-8"
                  placeholder={`Filter ${tables.data.length} tables…`}
                  value={filter}
                  onChange={(e) => setFilter(e.target.value)}
                />
              </div>

              <div className="flex gap-1.5">
                {(Object.keys(MODE_LABELS) as TableMode[]).map((mode) => (
                  <button
                    key={mode}
                    onClick={() => setVisibleTo(mode)}
                    className="rounded-md border border-slate-700 px-2.5 py-1.5 text-xs text-slate-300 transition hover:bg-slate-800"
                    title={
                      filter
                        ? `Apply to the ${visible.length} filtered tables`
                        : "Apply to all tables"
                    }
                  >
                    All &rarr; {MODE_LABELS[mode]}
                  </button>
                ))}
              </div>
            </div>

            <div className="flex flex-wrap gap-4 text-xs text-slate-500">
              <span>
                <strong className="text-blue-300">{counts.withData}</strong> with
                data
              </span>
              <span>
                <strong className="text-slate-300">{counts.schemaOnly}</strong>{" "}
                schema only
              </span>
              <span>
                <strong className="text-slate-400">{counts.excluded}</strong>{" "}
                excluded
              </span>
              <span>{counts.total} total</span>
            </div>

            {nonTransactionalSelected.length > 0 && (
              <div className="flex items-start gap-2 rounded-md border border-amber-500/40 bg-amber-500/5 p-3">
                <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-amber-400" />
                <p className="text-xs leading-relaxed text-amber-200/90">
                  {nonTransactionalSelected.length} selected table
                  {nonTransactionalSelected.length === 1 ? " is" : "s are"} not
                  InnoDB ({nonTransactionalSelected.map((t) => t.name).join(", ")}
                  ). These are not covered by{" "}
                  <code>--single-transaction</code>, so a consistent snapshot
                  cannot be guaranteed for them.
                </p>
              </div>
            )}

            <div className="panel divide-y divide-slate-800">
              {visible.length === 0 ? (
                <p className="p-6 text-center text-sm text-slate-500">
                  No tables match “{filter}”.
                </p>
              ) : (
                visible.map((t) => (
                  <TableRow
                    key={keyOf(t)}
                    table={t}
                    mode={modes[keyOf(t)] ?? "schema_only"}
                    onMode={(mode) =>
                      setModes((prev) => ({ ...prev, [keyOf(t)]: mode }))
                    }
                  />
                ))
              )}
            </div>

            <div className="flex flex-wrap items-center gap-3 border-t border-slate-800 pt-4">
              <button
                onClick={() => start.mutate()}
                disabled={!canRun || start.isPending}
                className="flex items-center gap-1.5 rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
              >
                <Play className="h-4 w-4" />
                {start.isPending ? "Starting…" : "Run backup"}
              </button>

              {selectedProfile?.engine === "postgres" && (
                <span className="text-xs text-amber-400">
                  PostgreSQL backup arrives in the next milestone.
                </span>
              )}

              {backupDir.data && (
                <span className="font-mono text-xs text-slate-600">
                  → {backupDir.data}
                </span>
              )}
            </div>

            {start.isError && (
              <ErrorNote
                title="Could not start the backup"
                detail={(start.error as Error).message}
              />
            )}

            {started && (
              <p className="text-xs text-emerald-400">
                Backup started. Watch it on the Jobs page — job {started.slice(0, 8)}.
              </p>
            )}

            <p className="text-xs text-slate-600">
              Saving these selections as a reusable sync plan arrives in a later
              milestone.
            </p>
          </>
        )}
      </div>
    </>
  );
}

function TableRow({
  table,
  mode,
  onMode,
}: {
  table: TableInfo;
  mode: TableMode;
  onMode: (mode: TableMode) => void;
}) {
  return (
    <div className="flex items-center gap-4 px-4 py-2">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate font-mono text-sm text-slate-200">
            {keyOf(table)}
          </span>
          {!table.transactional && table.storage_engine && (
            <span
              className="rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] uppercase text-amber-300"
              title="Not covered by --single-transaction"
            >
              {table.storage_engine}
            </span>
          )}
        </div>
        <div className="mt-0.5 text-xs text-slate-600">
          {table.estimated_rows != null
            ? `~${table.estimated_rows.toLocaleString()} rows`
            : "row count unknown"}
          {" · "}
          {formatBytes((table.data_bytes ?? 0) + (table.index_bytes ?? 0))}
        </div>
      </div>

      <div className="flex shrink-0 gap-1">
        {(Object.keys(MODE_LABELS) as TableMode[]).map((m) => (
          <button
            key={m}
            onClick={() => onMode(m)}
            className={cn(
              "rounded px-2 py-1 text-[11px] transition",
              mode === m
                ? MODE_STYLES[m]
                : "text-slate-500 hover:bg-slate-800 hover:text-slate-300",
            )}
          >
            {MODE_LABELS[m]}
          </button>
        ))}
      </div>
    </div>
  );
}

function ErrorNote({ title, detail }: { title: string; detail: string }) {
  return (
    <div className="rounded-md border border-red-500/40 bg-red-500/5 p-3">
      <div className="text-sm font-medium text-red-300">{title}</div>
      <p className="mt-1 break-words text-xs text-red-200/80">{detail}</p>
    </div>
  );
}
