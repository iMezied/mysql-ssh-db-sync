/**
 * Building and reading a chain of steps.
 *
 * The rules here mirror `Pipeline::validate` in `engine/src/pipeline.rs`. That
 * duplication is deliberate and bounded: the engine's copy is the one that
 * decides, and it refuses on the way into the store no matter which path wrote
 * it. This copy exists so the builder can disable Save with the reason on
 * screen, instead of letting somebody assemble six steps and learn on submit
 * that step two could never have worked.
 *
 * If the two ever disagree, the engine is right.
 */
import { defaultBackupOptions, defaultRestoreOptions } from "@/lib/engineDefaults";
import type {
  ConnectionProfile,
  Engine,
  Pipeline,
  PipelineStep,
  TargetNaming,
} from "@/bindings";

export type StepKind = PipelineStep["kind"];

export const STEP_LABELS: Record<StepKind, string> = {
  backup: "Back up",
  restore: "Restore",
  verify: "Verify",
  mask: "Mask",
  push_offsite: "Copy off-site",
  retention: "Retention",
  drill: "Drill",
};

/** One sentence saying what a step does, for the editor and the review panel. */
export function describeStep(
  step: PipelineStep,
  profiles: ConnectionProfile[],
): string {
  const nameOf = (id: string) =>
    profiles.find((p) => p.id === id)?.name ?? "a deleted connection";

  switch (step.kind) {
    case "backup":
      return `Back up ${step.database} from ${nameOf(step.profile_id)}`;
    case "restore":
      return `Restore into ${describeTarget(step.naming)} on ${nameOf(step.profile_id)}`;
    case "verify":
      return step.deep
        ? "Compare rows and contents against the source"
        : "Compare row counts against the source";
    case "mask":
      return `Mask ${step.rules?.length ?? 0} column${
        (step.rules?.length ?? 0) === 1 ? "" : "s"
      } on the destination`;
    case "push_offsite":
      return "Copy the artifact off-site";
    case "retention":
      if (step.policy.keep_last != null) {
        return `Keep only the newest ${step.policy.keep_last} backups`;
      }
      if (step.policy.max_age_days != null) {
        return `Delete backups older than ${step.policy.max_age_days} days`;
      }
      return "Apply retention";
    case "drill":
      return `Drill the newest backup on ${nameOf(step.profile_id)}`;
  }
}

/** What a restore will do to the destination, in the words that matter. */
export function describeTarget(naming: TargetNaming): string {
  switch (naming.strategy) {
    case "new_timestamped":
      return `a new ${naming.prefix}_… database`;
    case "drop_and_recreate":
      return `${naming.name}, replacing it`;
    case "into_existing":
      return `the existing ${naming.name}`;
  }
}

export function isDestructive(step: PipelineStep): boolean {
  return step.kind === "restore" && step.naming.strategy === "drop_and_recreate";
}

/** The databases this chain will drop, in step order. */
export function destructiveTargets(steps: PipelineStep[]): string[] {
  return steps.flatMap((s) =>
    s.kind === "restore" && s.naming.strategy === "drop_and_recreate"
      ? [s.naming.name]
      : [],
  );
}

/**
 * What arming a pipeline commits to.
 *
 * Must match `Pipeline::destructive_signature` exactly — the engine compares
 * what the user typed against its own copy, so a different join here would
 * make arming impossible rather than merely wrong.
 */
export function destructiveSignature(steps: PipelineStep[]): string | null {
  const targets = destructiveTargets(steps);
  return targets.length > 0 ? targets.join("\n") : null;
}

/** Whether a saved pipeline may currently run with nobody present. */
export function isArmed(pipeline: Pipeline): boolean {
  const current = destructiveSignature(pipeline.steps);
  return current != null && pipeline.unattended_ack === current;
}

/**
 * Why this chain cannot be saved, or null when it can.
 *
 * One reason at a time, in step order: a list of six complaints is not more
 * useful than the first one, and fixing the first often resolves the rest.
 */
export function validatePipeline(
  name: string,
  steps: PipelineStep[],
  profiles: ConnectionProfile[] = [],
): string | null {
  if (!name.trim()) return "Give this pipeline a name.";
  if (steps.length === 0) return "Add at least one step.";

  let seenBackup = false;
  let seenRestore = false;

  for (const [i, step] of steps.entries()) {
    const n = i + 1;
    switch (step.kind) {
      case "backup":
        if (!step.database.trim()) return `Step ${n} names no database.`;
        seenBackup = true;
        break;
      case "restore":
        if ((step.source?.from ?? "previous_step") === "previous_step" && !seenBackup) {
          return `Step ${n} restores what a backup produced, but no earlier step makes one.`;
        }
        seenRestore = true;
        break;
      case "verify":
        if (!seenRestore) return `Step ${n} verifies, but nothing has been restored yet.`;
        if (!seenBackup) {
          return `Step ${n} compares against the source a backup came from, and this pipeline restores a file it did not back up.`;
        }
        break;
      case "mask":
        if (!seenRestore) return `Step ${n} masks, but nothing has been restored yet.`;
        if ((step.rules?.length ?? 0) === 0) return `Step ${n} masks nothing.`;
        break;
      case "push_offsite":
        if (!seenBackup) return `Step ${n} copies off-site, but no earlier step makes an artifact.`;
        break;
      case "retention":
        if (!seenBackup) return `Step ${n} applies retention, but no earlier step makes an artifact.`;
        if (step.policy.keep_last == null && step.policy.max_age_days == null) {
          return `Step ${n} applies a retention policy that keeps everything.`;
        }
        break;
      case "drill":
        break;
    }
  }

  // Masking can only promise "masked or dropped", and it will not drop a
  // database this run did not create.
  let lastNaming: TargetNaming | null = null;
  for (const [i, step] of steps.entries()) {
    if (step.kind === "restore") lastNaming = step.naming;
    if (step.kind === "mask" && lastNaming?.strategy === "into_existing") {
      return `Step ${i + 1} masks ${lastNaming.name}, which this pipeline restores into without dropping. Masking cannot be made safe there.`;
    }
  }

  const targets = destructiveTargets(steps);
  const repeated = targets.find((t, i) => targets.indexOf(t) !== i);
  if (repeated) {
    return `Two steps replace ${repeated}; the second would destroy what the first restored.`;
  }

  // Nothing here translates dialects, so a chain that dumps one engine and
  // replays it into another is a migration wearing a pipeline's clothes.
  let carried: Engine | null = null;
  for (const [i, step] of steps.entries()) {
    if (step.kind === "backup") {
      carried = profiles.find((p) => p.id === step.profile_id)?.engine ?? null;
    }
    if (step.kind === "restore" && (step.source?.from ?? "previous_step") === "previous_step") {
      const dest = profiles.find((p) => p.id === step.profile_id)?.engine ?? null;
      if (carried && dest && carried !== dest) {
        return `Step ${i + 1} restores a ${carried} backup into a ${dest} connection. Nothing translates between them.`;
      }
    }
  }

  for (const [i, step] of steps.entries()) {
    const id = stepProfileId(step);
    if (id && profiles.length > 0 && !profiles.some((p) => p.id === id)) {
      return `Step ${i + 1} names a connection that no longer exists.`;
    }
  }

  return null;
}

export function stepProfileId(step: PipelineStep): string | null {
  switch (step.kind) {
    case "backup":
    case "restore":
    case "drill":
      return step.profile_id;
    default:
      return null;
  }
}

/** A step of this kind with defaults that are safe to save as they are. */
export function newStep(
  kind: StepKind,
  profileId: string,
  engine: Engine,
): PipelineStep {
  switch (kind) {
    case "backup":
      return {
        kind: "backup",
        profile_id: profileId,
        database: "",
        plan_id: null,
        selections: [],
        output_dir: null,
        compress: true,
        encrypt: false,
        record_row_counts: false,
        engine: defaultBackupOptions(engine),
      };
    case "restore":
      return {
        kind: "restore",
        profile_id: profileId,
        source: { from: "previous_step" },
        // The non-destructive strategy, always. Replacing a database is a
        // choice somebody makes on purpose, never a default they inherit.
        naming: { strategy: "new_timestamped", prefix: "copy" },
        engine: defaultRestoreOptions(engine),
        verify_checksum: true,
      };
    case "verify":
      return { kind: "verify", deep: false };
    case "mask":
      return { kind: "mask", rules: [] };
    case "push_offsite":
      return { kind: "push_offsite" };
    case "retention":
      return { kind: "retention", policy: { keep_last: 7, max_age_days: null } };
    case "drill":
      return {
        kind: "drill",
        profile_id: profileId,
        artifact_dir: null,
        deep: false,
        keep_on_failure: false,
      };
  }
}

/** Move a step, returning a new array. Out-of-range moves are no-ops. */
export function moveStep(
  steps: PipelineStep[],
  from: number,
  to: number,
): PipelineStep[] {
  if (from === to || from < 0 || to < 0 || from >= steps.length || to >= steps.length) {
    return steps;
  }
  const next = [...steps];
  const [moved] = next.splice(from, 1);
  if (!moved) return steps;
  next.splice(to, 0, moved);
  return next;
}
