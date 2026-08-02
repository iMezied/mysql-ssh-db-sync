import type { TableInfo, TableMode, TableSelection } from "@/bindings";

export const MODE_LABELS: Record<TableMode, string> = {
  schema_and_data: "Schema + data",
  schema_only: "Schema only",
  exclude: "Exclude",
};

export const MODE_STYLES: Record<TableMode, string> = {
  schema_and_data: "bg-blue-600 text-white",
  schema_only: "bg-slate-700 text-slate-200",
  exclude: "bg-slate-800 text-slate-500",
};

/**
 * How a table is named in a selection.
 *
 * Schema-qualified where the engine has schemas. A bare name in PostgreSQL
 * matches in *every* schema, which would silently pull data from a same-named
 * table in a schema nobody meant to touch.
 */
export function keyOf(table: TableInfo): string {
  return table.schema ? `${table.schema}.${table.name}` : table.name;
}

/**
 * Turn a sparse map of user choices into the full list the engine consumes.
 *
 * The map only holds tables the user actually clicked; everything else takes
 * `defaultMode`. Materialising over the whole introspected list — rather than
 * sending the sparse map — is what makes the request say the same thing on
 * every engine, because an unmentioned table means different things to
 * `mysqldump` and `pg_dump`.
 */
export function materialise(
  tables: TableInfo[],
  modes: Record<string, TableMode>,
  defaultMode: TableMode,
): TableSelection[] {
  return tables.map((t) => ({
    name: keyOf(t),
    mode: modes[keyOf(t)] ?? defaultMode,
    where_filter: null,
  }));
}

/** Load a saved set back into the picker's sparse map. */
export function modesFromSelections(
  selections: TableSelection[],
): Record<string, TableMode> {
  const modes: Record<string, TableMode> = {};
  for (const s of selections) modes[s.name] = s.mode;
  return modes;
}

/**
 * Tables the source has that a saved set does not mention.
 *
 * The engine gives these `schema_and_data` — a set names exceptions, so
 * anything it stays silent about carries data. Shown in the editor because
 * silently changing what a nightly backup contains is worth saying out loud.
 * Mirrors `plan::expand_selections`, including its `public.` matching rule.
 */
export function unlistedTables(
  tables: TableInfo[],
  selections: TableSelection[],
): string[] {
  return tables
    .map(keyOf)
    .filter(
      (name) =>
        !selections.some(
          (s) => s.name === name || `public.${s.name}` === name,
        ),
    );
}
