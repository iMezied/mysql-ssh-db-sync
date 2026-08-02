import type { JobKind } from "@/bindings";

/** Job kinds as a heading. `verify` is what the app elsewhere calls a drill. */
export const KIND_LABELS: Record<JobKind, string> = {
  backup: "Backup",
  restore: "Restore",
  verify: "Drill",
  sync: "Sync",
};

/** One line of "what this run was asked to do". */
export type JobFact = { label: string; value: string };

/**
 * What a job was asked to do, read back out of its stored request.
 *
 * The request is stored as opaque JSON precisely so its shape can change
 * without a migration, which means anything read here may be missing or a
 * different type than expected. Every field is therefore optional on the way
 * out: a job whose options this build cannot make sense of shows fewer facts,
 * never a crashed page.
 */
export function summariseOptions(kind: JobKind, optionsJson: string): JobFact[] {
  const options = asRecord(parseJson(optionsJson));
  if (!options) return [];

  switch (kind) {
    case "backup":
      return backupFacts(asRecord(options.common));
    case "restore":
      return restoreFacts(options);
    case "sync":
    case "verify":
      return [
        ...backupFacts(asRecord(asRecord(options.backup)?.common)),
        ...fact("Target", describeNaming(options.naming)),
        ...fact("Verification", verificationOf(options)),
      ];
  }
}

function backupFacts(common: Record<string, unknown> | undefined): JobFact[] {
  const raw = common?.selections;
  const selections: unknown[] | null = Array.isArray(raw) ? raw : null;
  const withData = selections?.filter(
    (s) => asRecord(s)?.mode === "schema_and_data",
  ).length;

  return [
    ...fact("Database", asString(common?.database)),
    ...fact(
      "Tables",
      selections && withData != null
        ? `${withData} of ${selections.length} with data`
        : undefined,
    ),
  ];
}

function restoreFacts(options: Record<string, unknown>): JobFact[] {
  return [
    ...fact("Artifact", basename(asString(options.artifact_path))),
    ...fact("Target", describeNaming(options.naming)),
    ...fact(
      "Checksum",
      options.verify_checksum === true
        ? "checked before the restore"
        : options.verify_checksum === false
          ? "not checked"
          : undefined,
    ),
  ];
}

/** Which database a restore lands in, and whether anything is destroyed. */
export function describeNaming(naming: unknown): string | undefined {
  const value = asRecord(naming);
  if (!value) return undefined;

  switch (asString(value.strategy)) {
    case "new_timestamped": {
      const prefix = asString(value.prefix);
      return prefix ? `${prefix}_… (a new database)` : "a new database";
    }
    case "drop_and_recreate": {
      const name = asString(value.name);
      return name ? `${name} (dropped and recreated)` : undefined;
    }
    case "into_existing": {
      const name = asString(value.name);
      return name ? `${name} (restored over)` : undefined;
    }
    default:
      return undefined;
  }
}

function verificationOf(options: Record<string, unknown>): string | undefined {
  if (options.verify !== true) {
    return options.verify === false ? "none" : undefined;
  }
  return options.deep_verify === true
    ? "row counts and table contents"
    : "row counts";
}

/**
 * A stored log split into lines.
 *
 * The durable log is one string built from the same events that stream to a
 * live page, so a finished job from a previous session still has a timeline —
 * just without the colour, since the level is inside the text by then.
 */
export function logLines(log: string | null | undefined): string[] {
  if (!log) return [];
  return log.split("\n").filter((line) => line.trim() !== "");
}

/** The file name out of a path, for a heading that has no room for the rest. */
export function basename(path: string | undefined): string | undefined {
  if (!path) return undefined;
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

function fact(label: string, value: string | undefined): JobFact[] {
  return value ? [{ label, value }] : [];
}

function parseJson(json: string): unknown {
  try {
    return JSON.parse(json);
  } catch {
    return null;
  }
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" && value !== "" ? value : undefined;
}
