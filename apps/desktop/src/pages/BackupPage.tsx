import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useMutation, useQuery } from "@tanstack/react-query";
import { Database, Play } from "lucide-react";

import PageHeader from "@/components/PageHeader";
import TableSelector from "@/components/TableSelector";
import { api } from "@/lib/api";
import { useProgressStore } from "@/lib/jobProgress";
import { materialise, modesFromSelections } from "@/lib/tableSelection";
import type {
  BackupRequest,
  EngineBackupOptions,
  PgDumpFormat,
  TableMode,
} from "@/bindings";
import {
  defaultBackupOptions,
  ENGINE_NOUNS,
  supportsSchemaOnly,
} from "@/lib/engineDefaults";

/**
 * Source browser and table selection.
 *
 * Replaces the old `tables.conf`: every table defaults to schema-only and the
 * user promotes the ones that need rows. Running the backup lands in M2′.
 */
export default function BackupPage() {
  const [profileId, setProfileId] = useState("");
  const [database, setDatabase] = useState("");
  const [setId, setSetId] = useState("");
  const [modes, setModes] = useState<Record<string, TableMode>>({});
  const [pgFormat, setPgFormat] = useState<PgDumpFormat>("custom");
  const [countRows, setCountRows] = useState(false);

  const navigate = useNavigate();
  const noteLaunch = useProgressStore((s) => s.noteLaunch);

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

  const selectedProfile = profiles.data?.find((p) => p.id === profileId);
  const engine = selectedProfile?.engine ?? "mysql";
  const nouns = ENGINE_NOUNS[engine];

  /**
   * What an unselected table does.
   *
   * Relational engines default to structure-without-rows: every table's shape
   * reaches the destination and the user promotes the ones that need data.
   * MongoDB has no such middle setting — `mongodump` writes a collection whole
   * or not at all — so the default there is what `mongodump` itself does,
   * include it. Defaulting to "exclude" instead would mean the obvious action
   * on this page backs up nothing.
   */
  const defaultMode: TableMode = supportsSchemaOnly(engine)
    ? "schema_only"
    : "schema_and_data";

  // Saved sets for this connection, so a 109-table selection is picked once
  // rather than rebuilt on every backup.
  const sets = useQuery({
    queryKey: ["sync-plans", profileId],
    queryFn: () => api.listSyncPlans(profileId),
    enabled: profileId !== "",
  });
  const chosenSet = sets.data?.find((s) => s.id === setId) ?? null;

  // Choosing a set seeds the picker and pins the database it was written
  // against — its table names only mean anything there.
  useEffect(() => {
    if (!chosenSet) return;
    setDatabase(chosenSet.database);
    setModes(modesFromSelections(chosenSet.selections));
  }, [chosenSet]);

  const engineOptions = (): EngineBackupOptions =>
    defaultBackupOptions(engine, pgFormat);

  const start = useMutation({
    mutationFn: () => {
      const rows = tables.data ?? [];
      const request: BackupRequest = {
        common: {
          database,
          selections: materialise(rows, modes, defaultMode),
          output_dir: backupDir.data ?? "",
          compress: true,
          encrypt: false,
          record_row_counts: countRows,
        },
        engine: engineOptions(),
      };
      return api.startBackup(profileId, request);
    },
    // Straight to the job. A backup is minutes of work the user has every
    // reason to watch, and the alternative — a line of text with an id in it —
    // asked them to go and find their own run in a list of fifty.
    onSuccess: (jobId) => {
      noteLaunch(jobId, { title: "Backup", detail: database });
      navigate(`/jobs/${jobId}`);
    },
  });

  const canRun =
    profileId !== "" && database !== "" && (tables.data?.length ?? 0) > 0;

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
                setSetId("");
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
            <span className="field-label">Table set</span>
            <select
              className="field-input"
              value={setId}
              disabled={!profileId}
              onChange={(e) => {
                setSetId(e.target.value);
                if (e.target.value === "") setModes({});
              }}
            >
              <option value="">Pick tables by hand</option>
              {sets.data?.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.name} ({s.database})
                </option>
              ))}
            </select>
          </label>

          <label className="min-w-56 flex-1">
            <span className="field-label">Database</span>
            <select
              className="field-input"
              value={database}
              disabled={!profileId || databases.isLoading || !!chosenSet}
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
            title={`Could not list ${nouns.tables}`}
            detail={(tables.error as Error).message}
          />
        )}

        {!profileId && (
          <div className="panel flex items-center gap-3 p-8 text-sm text-slate-500">
            <Database className="h-5 w-5" />
            Pick a connection to browse its {nouns.tables}.
          </div>
        )}

        {tables.isLoading && database && (
          <p className="text-sm text-slate-500">Introspecting…</p>
        )}

        {tables.data && (
          <>
            <TableSelector
              tables={tables.data}
              value={modes}
              onChange={setModes}
              engine={engine}
              defaultMode={defaultMode}
            />

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
                <label className="flex items-center gap-2 text-xs text-slate-400">
                  Format
                  <select
                    className="rounded-md border border-slate-700 bg-slate-950 px-2 py-1.5 text-xs text-slate-200"
                    value={pgFormat}
                    onChange={(e) => setPgFormat(e.target.value as PgDumpFormat)}
                  >
                    <option value="custom">Custom (selective + parallel restore)</option>
                    <option value="directory">Directory (parallel dump)</option>
                    <option value="plain">Plain SQL (no selective restore)</option>
                  </select>
                </label>
              )}

              <label className="flex items-start gap-2 text-xs text-slate-400">
                <input
                  type="checkbox"
                  className="mt-0.5"
                  checked={countRows}
                  onChange={(e) => setCountRows(e.target.checked)}
                />
                <span>
                  Record row counts
                  <span className="text-slate-600">
                    {" "}
                    — lets a restore drill compare exact numbers instead of
                    only checking each table arrived. Costs a full scan per
                    table, on top of the dump.
                  </span>
                </span>
              </label>

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

            <p className="text-xs text-slate-600">
              To reuse this selection, save it on the Table sets page and pick it
              above next time.
            </p>
          </>
        )}
      </div>
    </>
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
