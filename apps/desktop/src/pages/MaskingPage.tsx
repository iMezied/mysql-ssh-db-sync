import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, Plus, Trash2 } from "lucide-react";

import PageHeader from "@/components/PageHeader";
import { api } from "@/lib/api";
import type { MaskRule, MaskTransform } from "@/bindings";

/**
 * Masking rules, per sync plan.
 *
 * The page has one job beyond editing rules: making sure nobody leaves it
 * believing their backup files are masked. They are not — masking runs on the
 * destination after the restore — and that is said here rather than buried in
 * documentation somebody will not read before handing an artifact to a
 * contractor.
 */

type TransformKind = MaskTransform["kind"];

const TRANSFORMS: {
  kind: TransformKind;
  label: string;
  detail: string;
  nulls: string;
}[] = [
  {
    kind: "hash",
    label: "Hash",
    detail: "Salted SHA-256, in hex.",
    nulls: "NULL stays NULL",
  },
  {
    kind: "email",
    label: "Email",
    detail: "An address at example.invalid, which can never receive mail.",
    nulls: "NULL stays NULL",
  },
  {
    kind: "phone",
    label: "Phone",
    detail: "A number under +1555, reserved so it cannot ring anyone.",
    nulls: "NULL stays NULL",
  },
  {
    kind: "null",
    label: "Null",
    detail: "Every row set to NULL. Fails on a NOT NULL column.",
    nulls: "—",
  },
  {
    kind: "constant",
    label: "Constant",
    detail: "Every row set to one value.",
    nulls: "NULLs are overwritten too",
  },
];

function describe(t: MaskTransform): string {
  switch (t.kind) {
    case "hash":
      return t.length
        ? `salted SHA-256, first ${t.length} characters`
        : "salted SHA-256";
    case "email":
      return "address at example.invalid";
    case "phone":
      return "number under +1555";
    case "null":
      return "set to NULL";
    case "constant":
      return `set to “${t.value}”`;
  }
}

export default function MaskingPage() {
  const queryClient = useQueryClient();
  const profiles = useQuery({ queryKey: ["profiles"], queryFn: api.listProfiles });

  const [profileId, setProfileId] = useState<string>("");
  const [planId, setPlanId] = useState<string>("");

  useEffect(() => {
    const first = profiles.data?.[0];
    if (!profileId && first) setProfileId(first.id);
  }, [profiles.data, profileId]);

  const plans = useQuery({
    queryKey: ["sync-plans", profileId],
    queryFn: () => api.listSyncPlans(profileId),
    enabled: Boolean(profileId),
  });

  useEffect(() => {
    // Reset rather than keep a plan from the previous connection selected.
    if (plans.data && !plans.data.some((p) => p.id === planId)) {
      setPlanId(plans.data[0]?.id ?? "");
    }
  }, [plans.data, planId]);

  const plan = useMemo(
    () => plans.data?.find((p) => p.id === planId),
    [plans.data, planId],
  );

  const tablesWithData = useMemo(
    () =>
      (plan?.selections ?? [])
        .filter((s) => s.mode === "schema_and_data")
        .map((s) => s.name),
    [plan],
  );

  const preview = useQuery({
    queryKey: ["masking-preview", planId, plan?.revision],
    queryFn: () => api.maskingPreview(planId),
    enabled: Boolean(planId) && Boolean(plan?.masking?.length),
  });

  const save = useMutation({
    mutationFn: (rules: MaskRule[]) => api.setSyncPlanMasking(planId, rules),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ["sync-plans", profileId] });
      void queryClient.invalidateQueries({ queryKey: ["masking-preview"] });
    },
  });

  const rules = plan?.masking ?? [];

  const removeRule = (table: string, column: string) =>
    save.mutate(rules.filter((r) => !(r.table === table && r.column === column)));

  return (
    <>
      <PageHeader
        title="Masking"
        description="Rewrite columns on the destination so a production copy can be shared."
      />

      <div className="space-y-6 p-6">
        <Warning />

        {(profiles.isError || plans.isError) && (
          // Without this the pickers just read "None", which is
          // indistinguishable from having no connections — and on this page
          // "nothing is listed" would quietly imply "nothing needs masking".
          <p className="text-xs text-red-400">
            Could not load{" "}
            {profiles.isError ? "connections" : "this connection's plans"}:{" "}
            {((profiles.error ?? plans.error) as Error).message}
          </p>
        )}

        <section className="flex flex-wrap gap-3">
          <Select
            label="Connection"
            value={profileId}
            onChange={setProfileId}
            options={(profiles.data ?? []).map((p) => ({
              value: p.id,
              label: `${p.name} (${p.engine})`,
            }))}
          />
          <Select
            label="Plan"
            value={planId}
            onChange={setPlanId}
            options={(plans.data ?? []).map((p) => ({
              value: p.id,
              label: `${p.name} — ${p.database}`,
            }))}
            empty="This connection has no sync plans yet."
          />
        </section>

        {plan && (
          <>
            <section className="space-y-3">
              <h2 className="text-sm font-medium text-slate-200">
                Rules{" "}
                <span className="font-normal text-slate-500">
                  revision {plan.revision}
                </span>
              </h2>

              <div className="panel divide-y divide-slate-800">
                {rules.length === 0 && (
                  <p className="px-4 py-6 text-sm text-slate-500">
                    No columns are masked. A sync of this plan copies production
                    values to the destination as they are.
                  </p>
                )}

                {rules.map((rule) => {
                  const inert = !tablesWithData.includes(rule.table);
                  return (
                    <div
                      key={`${rule.table}.${rule.column}`}
                      className="flex items-start gap-3 px-4 py-3"
                    >
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-2">
                          <span className="font-mono text-sm text-slate-200">
                            {rule.table}.{rule.column}
                          </span>
                          {inert && (
                            <span className="rounded bg-amber-500/15 px-1.5 py-0.5 text-[11px] text-amber-300">
                              will not run
                            </span>
                          )}
                        </div>
                        <p className="mt-0.5 text-xs text-slate-500">
                          {describe(rule.transform)}
                          {inert &&
                            " — this plan does not copy that table with data, so nothing reaches the destination to mask."}
                        </p>
                      </div>
                      <button
                        type="button"
                        className="rounded p-1.5 text-slate-500 transition hover:bg-slate-800 hover:text-red-400"
                        title="Remove this rule"
                        disabled={save.isPending}
                        onClick={() => removeRule(rule.table, rule.column)}
                      >
                        <Trash2 className="h-4 w-4" />
                      </button>
                    </div>
                  );
                })}
              </div>

              {save.isError && (
                <p className="text-xs text-red-400">
                  {(save.error as Error).message}
                </p>
              )}
            </section>

            <AddRule
              tables={tablesWithData}
              disabled={save.isPending}
              onAdd={(rule) =>
                save.mutate([
                  // Replace rather than duplicate: two rules on one column are
                  // refused at run time, and the second edit is almost always
                  // meant as a correction.
                  ...rules.filter(
                    (r) => !(r.table === rule.table && r.column === rule.column),
                  ),
                  rule,
                ])
              }
            />

            {preview.data && rules.length > 0 && (
              <section className="space-y-3">
                <h2 className="text-sm font-medium text-slate-200">
                  What the destination will run
                </h2>
                <p className="max-w-3xl text-xs leading-relaxed text-slate-500">
                  The salt and any constants are bound parameters, never
                  literals, so this is safe to paste into a ticket. Every count
                  the read-back returns must be zero or the sync stops and the
                  destination is dropped.
                </p>
                <pre className="panel overflow-x-auto p-4 text-xs leading-relaxed text-slate-400">
                  {[...preview.data.updates, "", ...preview.data.checks]
                    .map((s) => (s ? `${s};` : ""))
                    .join("\n")}
                </pre>
              </section>
            )}
          </>
        )}
      </div>
    </>
  );
}

function Warning() {
  return (
    <div className="flex gap-3 rounded-lg border border-amber-500/30 bg-amber-500/5 p-4">
      <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-amber-400" />
      <div className="space-y-2 text-xs leading-relaxed text-amber-200/90">
        <p>
          <strong className="font-semibold">
            Masking does not protect the backup file.
          </strong>{" "}
          It runs on the destination after the restore lands, because
          mysqldump and pg_dump cannot transform a column. The artifact this
          sync writes holds the real data and is exactly as sensitive as the
          source — encrypt it, and do not hand it to anyone who is only
          cleared to see the masked copy.
        </p>
        <p>
          Masking is verified: the destination is read back and every unmasked
          row counted. If anything fails, the destination database is dropped
          rather than left half-masked.
        </p>
        <p>
          Matching values mask to matching pseudonyms, so joins survive. That
          also makes this pseudonymisation, not anonymisation — someone holding
          the masked data and the salt can confirm a guess.
        </p>
      </div>
    </div>
  );
}

function AddRule({
  tables,
  disabled,
  onAdd,
}: {
  tables: string[];
  disabled: boolean;
  onAdd: (rule: MaskRule) => void;
}) {
  const [table, setTable] = useState("");
  const [column, setColumn] = useState("");
  const [kind, setKind] = useState<TransformKind>("email");
  const [value, setValue] = useState("");
  const [length, setLength] = useState("");

  const chosen = TRANSFORMS.find((t) => t.kind === kind)!;
  const ready =
    table.trim() && column.trim() && (kind !== "constant" || value.length > 0);

  const submit = () => {
    if (!ready) return;
    let transform: MaskTransform;
    switch (kind) {
      case "hash":
        transform = { kind: "hash", length: length ? Number(length) : null };
        break;
      case "constant":
        transform = { kind: "constant", value };
        break;
      default:
        transform = { kind } as MaskTransform;
    }
    onAdd({ table: table.trim(), column: column.trim(), transform });
    setColumn("");
    setValue("");
  };

  return (
    <section className="space-y-3">
      <h2 className="text-sm font-medium text-slate-200">Add a rule</h2>

      <div className="panel space-y-3 p-4">
        <div className="flex flex-wrap gap-3">
          <label className="flex flex-col gap-1">
            <span className="field-label">Table</span>
            <input
              list="masking-tables"
              className="field-input w-56"
              value={table}
              onChange={(e) => setTable(e.target.value)}
              placeholder="users"
            />
            <datalist id="masking-tables">
              {tables.map((t) => (
                <option key={t} value={t} />
              ))}
            </datalist>
          </label>

          <label className="flex flex-col gap-1">
            <span className="field-label">Column</span>
            <input
              className="field-input w-56"
              value={column}
              onChange={(e) => setColumn(e.target.value)}
              placeholder="email"
            />
          </label>

          <label className="flex flex-col gap-1">
            <span className="field-label">Transform</span>
            <select
              className="field-input w-44"
              value={kind}
              onChange={(e) => setKind(e.target.value as TransformKind)}
            >
              {TRANSFORMS.map((t) => (
                <option key={t.kind} value={t.kind}>
                  {t.label}
                </option>
              ))}
            </select>
          </label>

          {kind === "hash" && (
            <label className="flex flex-col gap-1">
              <span className="field-label">Length (optional)</span>
              <input
                className="field-input w-32"
                inputMode="numeric"
                value={length}
                onChange={(e) =>
                  setLength(e.target.value.replace(/[^0-9]/g, ""))
                }
                placeholder="64"
              />
            </label>
          )}

          {kind === "constant" && (
            <label className="flex flex-col gap-1">
              <span className="field-label">Value</span>
              <input
                className="field-input w-56"
                value={value}
                onChange={(e) => setValue(e.target.value)}
                placeholder="redacted"
              />
            </label>
          )}
        </div>

        <p className="text-xs text-slate-500">
          {chosen.detail} <span className="text-slate-600">{chosen.nulls}</span>
        </p>

        <button
          type="button"
          className="inline-flex items-center gap-1.5 rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
          disabled={!ready || disabled}
          onClick={submit}
        >
          <Plus className="h-4 w-4" />
          Add rule
        </button>

        <p className="text-xs text-slate-600">
          The column name is checked against the source before the next sync
          starts. A rule naming a column that does not exist stops the run
          rather than silently protecting nothing.
        </p>
      </div>
    </section>
  );
}

function Select({
  label,
  value,
  onChange,
  options,
  empty,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  options: { value: string; label: string }[];
  empty?: string;
}) {
  return (
    <label className="flex flex-col gap-1">
      <span className="field-label">{label}</span>
      {options.length === 0 ? (
        <span className="field-input w-72 text-slate-600">{empty ?? "None"}</span>
      ) : (
        <select
          className="field-input w-72"
          value={value}
          onChange={(e) => onChange(e.target.value)}
        >
          {options.map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </select>
      )}
    </label>
  );
}
