import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { FileUp, Layers, Pencil, Plus, Trash2 } from "lucide-react";

import PageHeader from "@/components/PageHeader";
import TableSelector from "@/components/TableSelector";
import { api, ApiError } from "@/lib/api";
import { supportsSchemaOnly } from "@/lib/engineDefaults";
import {
  keyOf,
  materialise,
  modesFromSelections,
  unlistedTables,
} from "@/lib/tableSelection";
import type { SyncPlan, TableMode } from "@/bindings";

/**
 * Named, reusable table selections — "table sets" to the user, `SyncPlan` in
 * the store.
 *
 * Its own page rather than a panel on Backup because four things consume a set:
 * Backup, Sync, Schedules and Restore. Owning the editor on one of them forces
 * the other three to say "go and build it over there", which is exactly the
 * dead end the Schedules page has been pointing at.
 */
export default function TableSetsPage() {
  const queryClient = useQueryClient();
  const [profileId, setProfileId] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [modes, setModes] = useState<Record<string, TableMode>>({});
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const profiles = useQuery({ queryKey: ["profiles"], queryFn: api.listProfiles });
  const profile = profiles.data?.find((p) => p.id === profileId);
  const engine = profile?.engine ?? "mysql";

  const sets = useQuery({
    queryKey: ["sync-plans", profileId],
    queryFn: () => api.listSyncPlans(profileId),
    enabled: !!profileId,
  });
  const selected = sets.data?.find((s) => s.id === selectedId) ?? null;

  const tables = useQuery({
    queryKey: ["tables", profileId, selected?.database],
    queryFn: () => api.listTables(profileId, selected!.database),
    enabled: !!profileId && !!selected,
  });

  // Loading a set replaces whatever was being edited. Guarded on `dirty` so an
  // unsaved edit is not thrown away by a refetch of the same set.
  useEffect(() => {
    if (!selected || dirty) return;
    setModes(modesFromSelections(selected.selections));
  }, [selected, dirty]);

  const save = useMutation({
    mutationFn: async () => {
      if (!selected) throw new Error("no set selected");
      const rows = tables.data ?? [];
      // Materialised over every table so the stored set is explicit. It is
      // still completed at run time against whatever the source holds then.
      return api.updateSyncPlan(
        selected.id,
        materialise(rows, modes, "schema_and_data"),
      );
    },
    onSuccess: () => {
      setDirty(false);
      setError(null);
      void queryClient.invalidateQueries({ queryKey: ["sync-plans", profileId] });
    },
    onError: (e) => setError(String(e)),
  });

  const rename = useMutation({
    mutationFn: ({ id, name }: { id: string; name: string }) =>
      api.renameSyncPlan(id, name),
    onSuccess: () => {
      setError(null);
      void queryClient.invalidateQueries({ queryKey: ["sync-plans", profileId] });
    },
    onError: (e) =>
      setError(
        e instanceof ApiError && e.kind === "duplicate_name"
          ? "This connection already has a set with that name."
          : String(e),
      ),
  });

  /**
   * Import a legacy `tables.conf`.
   *
   * The old Bash tool's format: one table per line, everything listed carries
   * data. Read here rather than passed as a path because the command takes
   * contents — the file may be anywhere the picker can reach.
   *
   * The source's table list goes with it, and the import is refused without
   * one. The file only names what carries data, so the engine has to state the
   * omitted tables as schema-only itself; left implicit they would be completed
   * at run time as schema+data, and the import would mean the inverse of the
   * file. The button is disabled until the introspection lands, so this guard
   * is the second line rather than the first.
   */
  const importConf = useMutation({
    mutationFn: async (file: File) => {
      if (!selected) throw new Error("choose a set to import into");
      const rows = tables.data ?? [];
      if (rows.length === 0) {
        throw new Error(
          "the source's table list has not loaded yet — wait for it, or check the connection",
        );
      }
      const selections = await api.importTablesConf(
        await file.text(),
        rows.map(keyOf),
      );
      return api.updateSyncPlan(selected.id, selections);
    },
    onSuccess: (set) => {
      setModes(modesFromSelections(set.selections));
      setDirty(false);
      setError(null);
      void queryClient.invalidateQueries({ queryKey: ["sync-plans", profileId] });
    },
    onError: (e) => setError(String(e)),
  });

  const remove = useMutation({
    mutationFn: (id: string) => api.deleteSyncPlan(id),
    onSuccess: () => {
      setSelectedId(null);
      void queryClient.invalidateQueries({ queryKey: ["sync-plans", profileId] });
    },
  });

  const tablesLoaded = (tables.data?.length ?? 0) > 0;
  const unlisted = selected
    ? unlistedTables(tables.data ?? [], selected.selections)
    : [];

  return (
    <>
      <PageHeader
        title="Table sets"
        description="Named table selections you can reuse for a backup, a sync or a schedule — instead of re-picking every table each time."
      />

      <div className="space-y-5 p-6">
        <div className="flex flex-wrap items-end gap-3">
          <label className="min-w-56 flex-1">
            <span className="field-label">Connection</span>
            <select
              className="field-input"
              value={profileId}
              onChange={(e) => {
                setProfileId(e.target.value);
                setSelectedId(null);
                setModes({});
                setDirty(false);
              }}
            >
              <option value="">Choose a connection…</option>
              {profiles.data?.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
          </label>
        </div>

        {profileId && (
          <NewSetForm
            profileId={profileId}
            onCreated={(set) => {
              setSelectedId(set.id);
              setModes(modesFromSelections(set.selections));
              setDirty(false);
            }}
          />
        )}

        {profileId && sets.data?.length === 0 && (
          <div className="panel flex items-center gap-3 p-8 text-sm text-slate-500">
            <Layers className="h-5 w-5" />
            No table sets yet for {profile?.name}.
          </div>
        )}

        {(sets.data?.length ?? 0) > 0 && (
          <div className="panel divide-y divide-slate-800">
            {sets.data?.map((set) => (
              <SetRow
                key={set.id}
                set={set}
                active={set.id === selectedId}
                onSelect={() => {
                  setSelectedId(set.id);
                  setDirty(false);
                }}
                onRename={(name) => rename.mutate({ id: set.id, name })}
                onDelete={() => remove.mutate(set.id)}
              />
            ))}
          </div>
        )}

        {selected && (
          <section className="space-y-3">
            <div className="flex flex-wrap items-baseline justify-between gap-2">
              <h2 className="text-sm font-medium text-slate-200">
                {selected.name}{" "}
                <span className="text-slate-500">— {selected.database}</span>
              </h2>
              <button
                type="button"
                onClick={() => save.mutate()}
                disabled={!dirty || save.isPending}
                className="rounded-md bg-blue-600 px-3 py-1.5 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
              >
                {save.isPending ? "Saving…" : dirty ? "Save set" : "Saved"}
              </button>
            </div>

            {/* The rule, stated where the decision is made. It is the one thing
                about a set that is not visible from the rows themselves. */}
            <p className="text-xs leading-relaxed text-slate-500">
              Tables this set does not list are backed up{" "}
              <strong className="text-slate-400">with their data</strong>.
              {unlisted.length > 0 && (
                <>
                  {" "}
                  <span className="text-amber-300/90">
                    {unlisted.length} table
                    {unlisted.length === 1 ? "" : "s"} on this server{" "}
                    {unlisted.length === 1 ? "is" : "are"} not in this set and
                    will be included in full.
                  </span>{" "}
                  Use <em>Exclude everything else</em> to pin the set down.
                </>
              )}
            </p>

            {tables.data && (
              <>
                <div className="flex flex-wrap items-center gap-2">
                <button
                  type="button"
                  onClick={() => {
                    const next = { ...modes };
                    for (const t of tables.data ?? []) {
                      if (next[keyOf(t)] == null) next[keyOf(t)] = "exclude";
                    }
                    setModes(next);
                    setDirty(true);
                  }}
                  className="rounded-md border border-slate-700 px-2.5 py-1.5 text-xs text-slate-300 transition hover:bg-slate-800"
                  title="Mark every table this set does not already mention as excluded"
                >
                  Exclude everything else
                </button>

                {/*
                  Held back until the table list is in. The file names only the
                  tables that carry data, so completing it — everything else
                  schema-only — needs to know what "everything else" is.
                */}
                <label
                  className={`rounded-md border border-slate-700 px-2.5 py-1.5 text-xs transition ${
                    tablesLoaded
                      ? "cursor-pointer text-slate-300 hover:bg-slate-800"
                      : "cursor-not-allowed text-slate-600"
                  }`}
                  title={
                    tablesLoaded
                      ? "Tables named in the file carry data; every other table on the source is set to schema only"
                      : "Waiting for the source's table list"
                  }
                >
                  <span className="flex items-center gap-1.5">
                    <FileUp className="h-3.5 w-3.5" />
                    {importConf.isPending ? "Importing…" : "Import tables.conf"}
                  </span>
                  <input
                    type="file"
                    accept=".conf,.txt"
                    className="hidden"
                    disabled={!tablesLoaded}
                    onChange={(e) => {
                      const file = e.target.files?.[0];
                      // Cleared so picking the same file twice still fires.
                      e.target.value = "";
                      if (file) importConf.mutate(file);
                    }}
                  />
                </label>
                </div>

                <TableSelector
                  tables={tables.data}
                  value={modes}
                  onChange={(next) => {
                    setModes(next);
                    setDirty(true);
                  }}
                  engine={engine}
                  defaultMode={
                    supportsSchemaOnly(engine) ? "schema_and_data" : "schema_and_data"
                  }
                />
              </>
            )}

            {tables.isLoading && (
              <p className="text-sm text-slate-500">Introspecting…</p>
            )}
            {tables.isError && (
              <p className="text-sm text-red-400">
                Could not list tables: {(tables.error as Error).message}
              </p>
            )}
            {error && <p className="text-sm text-red-400">{error}</p>}
          </section>
        )}
      </div>
    </>
  );
}

function SetRow({
  set,
  active,
  onSelect,
  onRename,
  onDelete,
}: {
  set: SyncPlan;
  active: boolean;
  onSelect: () => void;
  onRename: (name: string) => void;
  onDelete: () => void;
}) {
  const withData = set.selections.filter(
    (s) => s.mode === "schema_and_data",
  ).length;

  return (
    <div
      className={`flex items-center gap-4 px-4 py-3 ${active ? "bg-blue-600/10" : ""}`}
    >
      <button
        type="button"
        onClick={onSelect}
        className="min-w-0 flex-1 text-left"
      >
        <div className="text-sm text-slate-200">{set.name}</div>
        <div className="mt-0.5 text-xs text-slate-500">
          {set.database} · {set.selections.length} listed, {withData} with data ·
          revision {set.revision}
        </div>
      </button>
      <button
        type="button"
        onClick={() => {
          const name = window.prompt("Rename this table set", set.name);
          if (name && name.trim() && name.trim() !== set.name) {
            onRename(name.trim());
          }
        }}
        title="Rename this set"
        className="shrink-0 rounded p-1.5 text-slate-500 transition hover:bg-slate-800 hover:text-slate-200"
      >
        <Pencil className="h-4 w-4" />
      </button>
      <button
        type="button"
        onClick={onDelete}
        title="Delete this set"
        className="shrink-0 rounded p-1.5 text-slate-500 transition hover:bg-slate-800 hover:text-red-300"
      >
        <Trash2 className="h-4 w-4" />
      </button>
    </div>
  );
}

function NewSetForm({
  profileId,
  onCreated,
}: {
  profileId: string;
  onCreated: (set: SyncPlan) => void;
}) {
  const queryClient = useQueryClient();
  const [name, setName] = useState("");
  const [database, setDatabase] = useState("");
  const [error, setError] = useState<string | null>(null);

  const databases = useQuery({
    queryKey: ["databases", profileId],
    queryFn: () => api.listDatabases(profileId),
    enabled: !!profileId,
  });

  const create = useMutation({
    mutationFn: () =>
      api.createSyncPlan({
        profile_id: profileId,
        name: name.trim(),
        database,
        // Empty means "everything, with data" once expanded. A set starts by
        // saying nothing and is narrowed from there.
        selections: [],
        masking: [],
      }),
    onSuccess: (set) => {
      setName("");
      setError(null);
      void queryClient.invalidateQueries({ queryKey: ["sync-plans", profileId] });
      onCreated(set);
    },
    onError: (e) =>
      setError(
        e instanceof ApiError && e.kind === "duplicate_name"
          ? `This connection already has a set called “${name.trim()}”.`
          : String(e),
      ),
  });

  return (
    <form
      className="flex flex-wrap items-end gap-3"
      onSubmit={(e) => {
        e.preventDefault();
        create.mutate();
      }}
    >
      <label className="min-w-48 flex-1">
        <span className="field-label">New set</span>
        <input
          className="field-input"
          placeholder="core tables"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
      </label>
      <label className="min-w-48">
        <span className="field-label">Database</span>
        <select
          className="field-input"
          value={database}
          onChange={(e) => setDatabase(e.target.value)}
        >
          <option value="">Choose…</option>
          {databases.data?.map((d) => (
            <option key={d.name} value={d.name}>
              {d.name}
            </option>
          ))}
        </select>
      </label>
      <button
        type="submit"
        disabled={!name.trim() || !database || create.isPending}
        className="flex items-center gap-1.5 rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
      >
        <Plus className="h-4 w-4" />
        {create.isPending ? "Creating…" : "Create set"}
      </button>
      {error && <p className="w-full text-sm text-red-400">{error}</p>}
    </form>
  );
}
