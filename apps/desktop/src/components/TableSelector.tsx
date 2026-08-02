import { useMemo, useState } from "react";
import { AlertTriangle, Search } from "lucide-react";

import { cn, formatBytes } from "@/lib/utils";
import { ENGINE_NOUNS, supportsSchemaOnly } from "@/lib/engineDefaults";
import { MODE_LABELS, MODE_STYLES, keyOf } from "@/lib/tableSelection";
import type { Engine, TableInfo, TableMode } from "@/bindings";

/**
 * Choosing what happens to each table.
 *
 * One component rather than the copy Backup and Sync each had: they disagreed
 * about the default for MongoDB, and Sync's disagreement made every Mongo sync
 * fail validation before it started. A third copy for the table-sets editor
 * would have been the third chance to get it wrong.
 *
 * Controlled and sparse — `value` holds only the tables the user has touched,
 * and everything else reads as `defaultMode`. The caller decides what that
 * means and materialises the full list with `materialise()`.
 */
export default function TableSelector({
  tables,
  value,
  onChange,
  engine,
  defaultMode,
}: {
  tables: TableInfo[];
  value: Record<string, TableMode>;
  onChange: (next: Record<string, TableMode>) => void;
  engine: Engine;
  defaultMode: TableMode;
}) {
  const [filter, setFilter] = useState("");
  const nouns = ENGINE_NOUNS[engine];

  // `mongodump` writes a collection whole or not at all, so offering
  // "schema only" there would offer something the engine refuses.
  const availableModes = (Object.keys(MODE_LABELS) as TableMode[]).filter(
    (m) => m !== "schema_only" || supportsSchemaOnly(engine),
  );

  const visible = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    if (!needle) return tables;
    return tables.filter((t) => keyOf(t).toLowerCase().includes(needle));
  }, [tables, filter]);

  const modeOf = (t: TableInfo) => value[keyOf(t)] ?? defaultMode;

  const counts = useMemo(
    () => ({
      withData: tables.filter((t) => modeOf(t) === "schema_and_data").length,
      schemaOnly: tables.filter((t) => modeOf(t) === "schema_only").length,
      excluded: tables.filter((t) => modeOf(t) === "exclude").length,
      total: tables.length,
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [tables, value, defaultMode],
  );

  const nonTransactionalSelected = useMemo(
    () =>
      tables.filter((t) => !t.transactional && modeOf(t) === "schema_and_data"),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [tables, value, defaultMode],
  );

  // Applies to what is *visible*, so a filter plus a bulk button is how a set
  // gets built without clicking a hundred rows.
  const setVisibleTo = (mode: TableMode) => {
    const next = { ...value };
    for (const t of visible) next[keyOf(t)] = mode;
    onChange(next);
  };

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-3">
        <div className="relative min-w-64 flex-1">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 h-4 w-4 text-slate-600" />
          <input
            className="field-input pl-8"
            placeholder={`Filter ${tables.length} ${nouns.tables}…`}
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
        </div>

        <div className="flex gap-1.5">
          {availableModes.map((mode) => (
            <button
              key={mode}
              type="button"
              onClick={() => setVisibleTo(mode)}
              className="rounded-md border border-slate-700 px-2.5 py-1.5 text-xs text-slate-300 transition hover:bg-slate-800"
              title={
                filter
                  ? `Apply to the ${visible.length} filtered ${nouns.tables}`
                  : `Apply to all ${nouns.tables}`
              }
            >
              All &rarr; {MODE_LABELS[mode]}
            </button>
          ))}
        </div>
      </div>

      <div className="flex flex-wrap gap-4 text-xs text-slate-500">
        <span>
          <strong className="text-blue-300">{counts.withData}</strong> with data
        </span>
        <span>
          <strong className="text-slate-300">{counts.schemaOnly}</strong> schema
          only
        </span>
        <span>
          <strong className="text-slate-400">{counts.excluded}</strong> excluded
        </span>
        <span>{counts.total} total</span>
      </div>

      {engine === "mongo" && (
        <Warning>
          A <code>mongodump</code> of a database being written to is consistent{" "}
          <em>within</em> each collection, not across them. Point-in-time capture
          needs the oplog, which needs a replica set — on a standalone server it
          is not available at all.
        </Warning>
      )}

      {nonTransactionalSelected.length > 0 && (
        <Warning>
          {nonTransactionalSelected.length} selected table
          {nonTransactionalSelected.length === 1 ? " is" : "s are"} not InnoDB (
          {nonTransactionalSelected.map((t) => t.name).join(", ")}). These are
          not covered by <code>--single-transaction</code>, so a consistent
          snapshot cannot be guaranteed for them.
        </Warning>
      )}

      <div className="panel divide-y divide-slate-800">
        {visible.length === 0 ? (
          <p className="p-6 text-center text-sm text-slate-500">
            No {nouns.tables} match “{filter}”.
          </p>
        ) : (
          visible.map((t) => (
            <TableRow
              key={keyOf(t)}
              table={t}
              mode={modeOf(t)}
              modes={availableModes}
              onMode={(mode) => onChange({ ...value, [keyOf(t)]: mode })}
            />
          ))
        )}
      </div>
    </div>
  );
}

function TableRow({
  table,
  mode,
  modes,
  onMode,
}: {
  table: TableInfo;
  mode: TableMode;
  modes: TableMode[];
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

      {/* Driven by `modes`, not by every key of MODE_LABELS — the old row
          rendered a "Schema only" button on MongoDB that the engine refuses. */}
      <div className="flex shrink-0 gap-1">
        {modes.map((m) => (
          <button
            key={m}
            type="button"
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

function Warning({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex items-start gap-2 rounded-md border border-amber-500/40 bg-amber-500/5 p-3">
      <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-amber-400" />
      <p className="text-xs leading-relaxed text-amber-200/90">{children}</p>
    </div>
  );
}
