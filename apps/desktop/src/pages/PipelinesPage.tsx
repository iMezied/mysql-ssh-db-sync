import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowDown,
  ArrowUp,
  Play,
  Plus,
  ShieldCheck,
  Trash2,
  Workflow,
} from "lucide-react";

import PageHeader from "@/components/PageHeader";
import { api, ApiError } from "@/lib/api";
import { useProgressStore } from "@/lib/jobProgress";
import {
  STEP_LABELS,
  type StepKind,
  describeStep,
  destructiveSignature,
  destructiveTargets,
  isArmed,
  isDestructive,
  moveStep,
  newStep,
  stepProfileId,
  validatePipeline,
} from "@/lib/pipeline";
import { cn } from "@/lib/utils";
import type {
  ConnectionProfile,
  Pipeline,
  PipelineStep,
  TargetNaming,
} from "@/bindings";

const ADDABLE: StepKind[] = [
  "backup",
  "restore",
  "verify",
  "mask",
  "push_offsite",
  "retention",
  "drill",
];

/**
 * Saved chains of actions.
 *
 * The Sync page runs backup-then-restore with a fixed shape, saves nothing, and
 * refuses a destructive target on purpose. This is where "back up production
 * and put it on staging, replacing what is there" gets written down once and
 * pressed afterwards.
 *
 * The editor is a plain vertical list because the data flow is one: a restore
 * consumes what the backup above it wrote. There is nothing to wire, so there
 * are no wires to draw.
 */
export default function PipelinesPage() {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const noteLaunch = useProgressStore((s) => s.noteLaunch);

  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [steps, setSteps] = useState<PipelineStep[]>([]);
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const profiles = useQuery({ queryKey: ["profiles"], queryFn: api.listProfiles });
  const pipelines = useQuery({ queryKey: ["pipelines"], queryFn: api.listPipelines });
  const selected = pipelines.data?.find((p) => p.id === selectedId) ?? null;

  // Loading a pipeline replaces whatever is being edited, unless it has
  // unsaved changes — a refetch must not throw away work in progress.
  useEffect(() => {
    if (!selected || dirty) return;
    setName(selected.name);
    setSteps(selected.steps);
  }, [selected, dirty]);

  const invalidate = () => void queryClient.invalidateQueries({ queryKey: ["pipelines"] });

  const create = useMutation({
    mutationFn: (input: { name: string; steps: PipelineStep[] }) =>
      api.createPipeline(input),
    onSuccess: (p) => {
      setSelectedId(p.id);
      setName(p.name);
      setSteps(p.steps);
      setDirty(false);
      setError(null);
      invalidate();
    },
    onError: (e) => setError(describeError(e)),
  });

  const save = useMutation({
    mutationFn: () => {
      if (!selected) throw new Error("no pipeline selected");
      return api.updatePipeline(selected.id, { name, steps });
    },
    onSuccess: () => {
      setDirty(false);
      setError(null);
      invalidate();
    },
    onError: (e) => setError(describeError(e)),
  });

  const remove = useMutation({
    mutationFn: (id: string) => api.deletePipeline(id),
    onSuccess: () => {
      setSelectedId(null);
      setSteps([]);
      setName("");
      setDirty(false);
      invalidate();
    },
    onError: (e) => setError(describeError(e)),
  });

  const problem = validatePipeline(name, steps, profiles.data ?? []);
  const edit = (next: PipelineStep[]) => {
    setSteps(next);
    setDirty(true);
    setError(null);
  };

  return (
    <>
      <PageHeader
        title="Pipelines"
        description="A chain of actions, saved once and run on demand."
        actions={
          <button
            type="button"
            onClick={() =>
              create.mutate({
                name: nextName(pipelines.data ?? []),
                steps: [],
              })
            }
            disabled={create.isPending}
            className="flex items-center gap-1.5 rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
          >
            <Plus className="h-4 w-4" />
            New pipeline
          </button>
        }
      />

      <div className="space-y-5 p-6">
        {pipelines.isError && (
          <p className="text-sm text-red-400">
            Could not load pipelines: {(pipelines.error as Error).message}
          </p>
        )}

        {pipelines.data?.length === 0 && (
          <div className="panel flex flex-col items-center gap-2 p-10 text-center">
            <Workflow className="h-8 w-8 text-slate-600" />
            <p className="text-sm text-slate-400">No pipelines yet.</p>
            <p className="max-w-md text-xs leading-relaxed text-slate-500">
              A pipeline is an ordered list of steps — back up one connection,
              restore it onto another, check it landed. Unlike the Sync page it
              is saved, and it can replace a database rather than only creating
              a new one.
            </p>
          </div>
        )}

        {(pipelines.data?.length ?? 0) > 0 && (
          <div className="panel divide-y divide-slate-800">
            {pipelines.data?.map((p) => (
              <PipelineRow
                key={p.id}
                pipeline={p}
                active={p.id === selectedId}
                onSelect={() => {
                  setSelectedId(p.id);
                  setDirty(false);
                  setError(null);
                }}
                onDelete={() => remove.mutate(p.id)}
              />
            ))}
          </div>
        )}

        {selected && (
          <section className="space-y-4">
            <div className="flex flex-wrap items-end justify-between gap-3">
              <label className="min-w-56 flex-1">
                <span className="field-label">Name</span>
                <input
                  className="field-input"
                  value={name}
                  onChange={(e) => {
                    setName(e.target.value);
                    setDirty(true);
                  }}
                />
              </label>
              <button
                type="button"
                onClick={() => save.mutate()}
                disabled={!dirty || !!problem || save.isPending}
                className="rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
              >
                {save.isPending ? "Saving…" : dirty ? "Save pipeline" : "Saved"}
              </button>
            </div>

            <StepList
              steps={steps}
              profiles={profiles.data ?? []}
              onChange={edit}
            />

            <AddStep
              profiles={profiles.data ?? []}
              onAdd={(kind, profileId, engine) =>
                edit([...steps, newStep(kind, profileId, engine)])
              }
            />

            {/* The reason Save is off, where the decision is made. The engine
                refuses the same shapes independently; this is so nobody
                assembles six steps and learns on submit that step two could
                never have worked. */}
            {problem && (
              <p className="text-xs leading-relaxed text-amber-300/90">{problem}</p>
            )}
            {error && <ErrorNote title="Could not save" detail={error} />}

            <RunPanel
              pipeline={selected}
              dirty={dirty}
              profiles={profiles.data ?? []}
              onLaunched={(jobId, detail) => {
                noteLaunch(jobId, { title: selected.name, detail });
                navigate(`/jobs/${jobId}`);
              }}
              onArmed={invalidate}
            />
          </section>
        )}
      </div>
    </>
  );
}

/** One saved pipeline in the list. */
function PipelineRow({
  pipeline,
  active,
  onSelect,
  onDelete,
}: {
  pipeline: Pipeline;
  active: boolean;
  onSelect: () => void;
  onDelete: () => void;
}) {
  const targets = destructiveTargets(pipeline.steps);
  const armed = isArmed(pipeline);

  return (
    <div
      className={cn(
        "flex items-center gap-4 px-4 py-3",
        active && "bg-blue-600/10",
      )}
    >
      <button
        type="button"
        onClick={onSelect}
        className="min-w-0 flex-1 text-left"
      >
        <span className="flex flex-wrap items-center gap-2">
          <span className="text-sm text-slate-200">{pipeline.name}</span>
          {targets.length > 0 && (
            <span className="rounded bg-red-500/15 px-1.5 py-0.5 text-[10px] uppercase text-red-300">
              destroys data
            </span>
          )}
          {armed && (
            <span className="rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] uppercase text-amber-300">
              runs unattended
            </span>
          )}
        </span>
        <span className="mt-0.5 block text-xs text-slate-500">
          {pipeline.steps.length === 0
            ? "no steps yet"
            : pipeline.steps.map((s) => STEP_LABELS[s.kind]).join(" → ")}
        </span>
      </button>
      <button
        type="button"
        onClick={onDelete}
        title="Delete this pipeline"
        className="shrink-0 rounded p-1.5 text-slate-500 transition hover:bg-slate-800 hover:text-red-300"
      >
        <Trash2 className="h-4 w-4" />
      </button>
    </div>
  );
}

/** The ordered steps, each with its own inline editor. */
function StepList({
  steps,
  profiles,
  onChange,
}: {
  steps: PipelineStep[];
  profiles: ConnectionProfile[];
  onChange: (next: PipelineStep[]) => void;
}) {
  if (steps.length === 0) {
    return (
      <div className="panel p-8 text-center text-sm text-slate-500">
        No steps yet. Add a backup to start the chain.
      </div>
    );
  }

  return (
    <div className="panel divide-y divide-slate-800">
      {steps.map((step, i) => (
        <div key={i} className="space-y-2 px-4 py-3">
          <div className="flex items-center gap-3">
            <span className="w-4 shrink-0 text-xs tabular-nums text-slate-600">
              {i + 1}
            </span>
            <span className="min-w-0 flex-1">
              <span className="flex flex-wrap items-center gap-2">
                <span className="text-sm text-slate-200">
                  {describeStep(step, profiles)}
                </span>
                {isDestructive(step) && (
                  <span className="rounded bg-red-500/15 px-1.5 py-0.5 text-[10px] uppercase text-red-300">
                    destroys data
                  </span>
                )}
              </span>
              <span className="mt-0.5 block text-[11px] uppercase tracking-wide text-slate-600">
                {STEP_LABELS[step.kind]}
              </span>
            </span>

            <div className="flex shrink-0 items-center gap-1">
              <IconButton
                label="Move up"
                disabled={i === 0}
                onClick={() => onChange(moveStep(steps, i, i - 1))}
              >
                <ArrowUp className="h-3.5 w-3.5" />
              </IconButton>
              <IconButton
                label="Move down"
                disabled={i === steps.length - 1}
                onClick={() => onChange(moveStep(steps, i, i + 1))}
              >
                <ArrowDown className="h-3.5 w-3.5" />
              </IconButton>
              <IconButton
                label="Remove this step"
                danger
                onClick={() => onChange(steps.filter((_, j) => j !== i))}
              >
                <Trash2 className="h-3.5 w-3.5" />
              </IconButton>
            </div>
          </div>

          <StepFields
            step={step}
            profiles={profiles}
            onChange={(next) =>
              onChange(steps.map((s, j) => (j === i ? next : s)))
            }
          />
        </div>
      ))}
    </div>
  );
}

/** The editable options for one step. */
function StepFields({
  step,
  profiles,
  onChange,
}: {
  step: PipelineStep;
  profiles: ConnectionProfile[];
  onChange: (next: PipelineStep) => void;
}) {
  const profileId = stepProfileId(step);

  const connection = profileId != null && (
    <label className="min-w-48 flex-1">
      <span className="field-label">Connection</span>
      <select
        className="field-input"
        value={profileId}
        onChange={(e) => onChange({ ...step, profile_id: e.target.value } as PipelineStep)}
      >
        <option value="">Choose a connection…</option>
        {profiles.map((p) => (
          <option key={p.id} value={p.id}>
            {p.name}
          </option>
        ))}
      </select>
    </label>
  );

  return (
    <div className="flex flex-wrap gap-3 pl-7">
      {connection}

      {step.kind === "backup" && (
        <label className="min-w-48 flex-1">
          <span className="field-label">Database</span>
          <input
            className="field-input"
            value={step.database}
            placeholder="shop"
            onChange={(e) => onChange({ ...step, database: e.target.value })}
          />
        </label>
      )}

      {step.kind === "restore" && (
        <TargetFields
          naming={step.naming}
          onChange={(naming) => onChange({ ...step, naming })}
        />
      )}

      {step.kind === "verify" && (
        <Check
          label="Also compare contents, not just counts"
          hint="Reads every row on both sides. Slower, and the only thing that catches the right number of rows holding the wrong bytes."
          checked={step.deep ?? false}
          onChange={(deep) => onChange({ ...step, deep })}
        />
      )}

      {step.kind === "retention" && (
        <label className="min-w-48">
          <span className="field-label">Keep the newest</span>
          <input
            className="field-input"
            type="number"
            min={1}
            value={step.policy.keep_last ?? ""}
            onChange={(e) =>
              onChange({
                ...step,
                policy: {
                  ...step.policy,
                  keep_last: e.target.value ? Number(e.target.value) : null,
                },
              })
            }
          />
        </label>
      )}

      {step.kind === "drill" && (
        <Check
          label="Leave the scratch database behind on failure"
          hint="For inspecting what went wrong. Otherwise it is always dropped."
          checked={step.keep_on_failure ?? false}
          onChange={(keep_on_failure) => onChange({ ...step, keep_on_failure })}
        />
      )}

      {step.kind === "mask" && (
        <p className="text-xs leading-relaxed text-slate-500">
          Masking rules are edited on the table set this pipeline backs up.
          Masking rewrites the destination — the artifact still holds the real
          data.
        </p>
      )}

      {step.kind === "push_offsite" && (
        <p className="text-xs text-slate-500">
          Sends the artifact to every enabled off-site destination.
        </p>
      )}
    </div>
  );
}

/** The three target strategies, in increasing order of danger. */
function TargetFields({
  naming,
  onChange,
}: {
  naming: TargetNaming;
  onChange: (next: TargetNaming) => void;
}) {
  return (
    <>
      <label className="min-w-48 flex-1">
        <span className="field-label">Target</span>
        <select
          className="field-input"
          value={naming.strategy}
          onChange={(e) => {
            const strategy = e.target.value as TargetNaming["strategy"];
            const existing =
              naming.strategy === "new_timestamped" ? naming.prefix : naming.name;
            onChange(
              strategy === "new_timestamped"
                ? { strategy, prefix: existing || "copy" }
                : { strategy, name: existing || "" },
            );
          }}
        >
          <option value="new_timestamped">New database each run</option>
          <option value="drop_and_recreate">Replace a database</option>
          <option value="into_existing">Into an existing database</option>
        </select>
      </label>

      <label className="min-w-48 flex-1">
        <span className="field-label">
          {naming.strategy === "new_timestamped" ? "Name prefix" : "Database"}
        </span>
        <input
          className="field-input"
          value={naming.strategy === "new_timestamped" ? naming.prefix : naming.name}
          onChange={(e) =>
            onChange(
              naming.strategy === "new_timestamped"
                ? { strategy: "new_timestamped", prefix: e.target.value }
                : { strategy: naming.strategy, name: e.target.value },
            )
          }
        />
      </label>

      {naming.strategy === "drop_and_recreate" && (
        <p className="w-full text-xs leading-relaxed text-red-300/80">
          This drops {naming.name || "the database"} and recreates it. Every run
          asks for the name to be typed back.
        </p>
      )}
    </>
  );
}

/** Add a step of any kind to the end of the chain. */
function AddStep({
  profiles,
  onAdd,
}: {
  profiles: ConnectionProfile[];
  onAdd: (kind: StepKind, profileId: string, engine: ConnectionProfile["engine"]) => void;
}) {
  const first = profiles[0];

  return (
    <div className="flex flex-wrap items-center gap-2">
      <span className="text-xs uppercase tracking-wide text-slate-600">Add</span>
      {ADDABLE.map((kind) => (
        <button
          key={kind}
          type="button"
          onClick={() => onAdd(kind, first?.id ?? "", first?.engine ?? "mysql")}
          className="rounded-md border border-slate-700 px-2.5 py-1.5 text-xs text-slate-300 transition hover:bg-slate-800"
        >
          {STEP_LABELS[kind]}
        </button>
      ))}
    </div>
  );
}

/**
 * Running a chain, and authorising it to run without anybody watching.
 *
 * The confirmation is asked for every run, and cleared afterwards — coming back
 * to this page must not leave somebody one click from replacing a database.
 */
function RunPanel({
  pipeline,
  dirty,
  profiles,
  onLaunched,
  onArmed,
}: {
  pipeline: Pipeline;
  /** True when the editor holds edits the saved definition does not. */
  dirty: boolean;
  profiles: ConnectionProfile[];
  onLaunched: (jobId: string, detail: string) => void;
  onArmed: () => void;
}) {
  const [typed, setTyped] = useState<string[]>([]);
  const [armText, setArmText] = useState("");
  const [error, setError] = useState<string | null>(null);

  // The saved definition is what runs, not what is on screen.
  const targets = destructiveTargets(pipeline.steps);
  const signature = destructiveSignature(pipeline.steps);
  const armed = isArmed(pipeline);

  const confirmed = targets.every((t, i) => (typed[i] ?? "").trim() === t);
  const runnable = !dirty && pipeline.steps.length > 0 && confirmed;

  const run = useMutation({
    mutationFn: () => api.startPipeline(pipeline.id, targets.map((_, i) => typed[i] ?? "")),
    onSuccess: (jobId) => {
      setTyped([]);
      setError(null);
      onLaunched(
        jobId,
        pipeline.steps
          .map((s) => describeStep(s, profiles))
          .slice(0, 2)
          .join(" · "),
      );
    },
    onError: (e) => setError(describeError(e)),
  });

  const arm = useMutation({
    mutationFn: (value: string | null) => api.armPipeline(pipeline.id, value),
    onSuccess: () => {
      setArmText("");
      setError(null);
      onArmed();
    },
    onError: (e) => setError(describeError(e)),
  });

  return (
    <section className="panel space-y-3 p-4">
      <h2 className="text-sm font-medium text-slate-200">Run</h2>

      {dirty && (
        <p className="text-xs text-amber-300/90">
          Save first — a run uses the saved pipeline, not the edits on screen.
        </p>
      )}

      {targets.map((target, i) => (
        <label key={`${target}-${i}`} className="block">
          <span className="field-label">
            Type <span className="font-mono text-red-300">{target}</span> to
            confirm step {stepNumberOf(pipeline.steps, i)} will drop it
          </span>
          <input
            className="field-input"
            value={typed[i] ?? ""}
            placeholder={target}
            onChange={(e) => {
              const next = [...typed];
              next[i] = e.target.value;
              setTyped(next);
            }}
          />
        </label>
      ))}

      <button
        type="button"
        onClick={() => run.mutate()}
        disabled={!runnable || run.isPending}
        className="flex items-center gap-1.5 rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
      >
        <Play className="h-4 w-4" />
        {run.isPending ? "Starting…" : "Run pipeline"}
      </button>

      {signature && (
        <div className="space-y-2 border-t border-slate-800 pt-3">
          <h3 className="flex items-center gap-1.5 text-xs font-semibold uppercase tracking-wide text-slate-500">
            <ShieldCheck className="h-3.5 w-3.5" />
            Unattended runs
          </h3>

          {armed ? (
            <>
              <p className="text-xs leading-relaxed text-amber-300/90">
                A schedule or <span className="font-mono">dbsync</span> may run
                this and drop {targets.join(", ")} with nobody present.
              </p>
              <button
                type="button"
                onClick={() => arm.mutate(null)}
                disabled={arm.isPending}
                className="rounded-md border border-slate-700 px-2.5 py-1.5 text-xs text-slate-300 transition hover:bg-slate-800 disabled:opacity-50"
              >
                Withdraw
              </button>
            </>
          ) : (
            <>
              <p className="text-xs leading-relaxed text-slate-500">
                Cron cannot answer a prompt. Typing the name{" "}
                {targets.length === 1 ? "" : "s "}
                here once lets this run unattended. Editing a target undoes it.
              </p>
              <div className="flex flex-wrap items-end gap-2">
                <label className="min-w-56 flex-1">
                  <span className="field-label">
                    Type {targets.join(", ")} to authorise
                  </span>
                  <input
                    className="field-input"
                    value={armText}
                    placeholder={targets.join(", ")}
                    onChange={(e) => setArmText(e.target.value)}
                  />
                </label>
                <button
                  type="button"
                  onClick={() => arm.mutate(signature)}
                  disabled={
                    arm.isPending ||
                    armText.trim() !== targets.join(", ") ||
                    dirty
                  }
                  className="rounded-md border border-slate-700 px-2.5 py-1.5 text-sm text-slate-300 transition hover:bg-slate-800 disabled:opacity-50"
                >
                  Authorise
                </button>
              </div>
            </>
          )}
        </div>
      )}

      {error && <ErrorNote title="Could not run" detail={error} />}
    </section>
  );
}

/** Which step number the nth destructive target belongs to. */
function stepNumberOf(steps: PipelineStep[], destructiveIndex: number): number {
  let seen = 0;
  for (const [i, step] of steps.entries()) {
    if (isDestructive(step)) {
      if (seen === destructiveIndex) return i + 1;
      seen += 1;
    }
  }
  return destructiveIndex + 1;
}

function nextName(existing: Pipeline[]): string {
  const base = "New pipeline";
  if (!existing.some((p) => p.name === base)) return base;
  for (let n = 2; ; n += 1) {
    const candidate = `${base} ${n}`;
    if (!existing.some((p) => p.name === candidate)) return candidate;
  }
}

function describeError(e: unknown): string {
  if (e instanceof ApiError && e.kind === "duplicate_name") {
    return "A pipeline with that name already exists.";
  }
  return String(e instanceof Error ? e.message : e);
}

function IconButton({
  label,
  danger,
  disabled,
  onClick,
  children,
}: {
  label: string;
  danger?: boolean;
  disabled?: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      disabled={disabled}
      onClick={onClick}
      className={cn(
        "rounded p-1.5 text-slate-500 transition hover:bg-slate-800 disabled:opacity-30",
        danger ? "hover:text-red-300" : "hover:text-slate-200",
      )}
    >
      {children}
    </button>
  );
}

function Check({
  label,
  hint,
  checked,
  onChange,
}: {
  label: string;
  hint?: string;
  checked: boolean;
  onChange: (next: boolean) => void;
}) {
  return (
    <label className="flex w-full items-start gap-2">
      <input
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
        className="mt-0.5 h-4 w-4 shrink-0 rounded border-slate-700 bg-slate-950"
      />
      <span>
        <span className="block text-sm text-slate-300">{label}</span>
        {hint && (
          <span className="block text-xs leading-relaxed text-slate-500">{hint}</span>
        )}
      </span>
    </label>
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
