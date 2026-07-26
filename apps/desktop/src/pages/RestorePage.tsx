import { useEffect, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { AlertTriangle, Play, ShieldCheck } from "lucide-react";

import PageHeader from "@/components/PageHeader";
import { api } from "@/lib/api";
import { formatBytes, formatTimestamp } from "@/lib/utils";
import type {
  Artifact,
  EngineRestoreOptions,
  RestoreRequest,
  TargetNaming,
} from "@/bindings";

/**
 * Restore an artifact into a database.
 *
 * The page is shaped around one asymmetry: two of the three target strategies
 * can destroy data that this app did not create, and the third cannot destroy
 * anything at all. So the safe one is the default, the destructive ones are
 * labelled with what they destroy rather than with what they do, and the typed
 * confirmation the engine demands is asked for here — where the user can still
 * see which database they are about to lose.
 */

type Strategy = "new_timestamped" | "drop_and_recreate" | "into_existing";

const STRATEGIES: {
  value: Strategy;
  label: string;
  detail: string;
  destructive: boolean;
}[] = [
  {
    value: "new_timestamped",
    label: "New database",
    detail:
      "Creates {prefix}_{timestamp}. Nothing existing is touched, so this can always be undone by dropping it.",
    destructive: false,
  },
  {
    value: "drop_and_recreate",
    label: "Replace a database",
    detail:
      "DROPs the named database if it exists, then recreates it. Everything currently in it is gone.",
    destructive: true,
  },
  {
    value: "into_existing",
    label: "Into an existing database",
    detail:
      "Restores over what is already there. Tables in the artifact are overwritten; tables not in it are left behind.",
    destructive: false,
  },
];

export default function RestorePage() {
  const [profileId, setProfileId] = useState("");
  const [artifactPath, setArtifactPath] = useState("");
  const [strategy, setStrategy] = useState<Strategy>("new_timestamped");
  const [prefix, setPrefix] = useState("");
  const [targetName, setTargetName] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const [verifyChecksum, setVerifyChecksum] = useState(true);
  const [started, setStarted] = useState<string | null>(null);

  const profiles = useQuery({ queryKey: ["profiles"], queryFn: api.listProfiles });
  const artifacts = useQuery({
    queryKey: ["artifacts"],
    queryFn: () => api.listArtifacts(null),
  });

  const profile = profiles.data?.find((p) => p.id === profileId);
  const artifact = artifacts.data?.find((a) => a.path === artifactPath);

  // Default the prefix to the database the artifact came from, so a folder of
  // restores stays readable without anyone having to name each one.
  useEffect(() => {
    if (artifact?.database) setPrefix(artifact.database);
  }, [artifact?.database]);

  const chosen = STRATEGIES.find((s) => s.value === strategy)!;

  // What the engine will demand be typed back, computed the same way it
  // computes it. A timestamped name is generated server-side at the moment of
  // the restore, which is exactly why that strategy never needs confirming.
  const needsConfirmation =
    chosen.destructive ||
    (profile?.environment === "prod" && strategy !== "new_timestamped");

  const engineMismatch =
    artifact?.engine != null &&
    profile != null &&
    artifact.engine !== profile.engine;

  const naming = (): TargetNaming =>
    strategy === "new_timestamped"
      ? { strategy: "new_timestamped", prefix: prefix.trim() }
      : strategy === "drop_and_recreate"
        ? { strategy: "drop_and_recreate", name: targetName.trim() }
        : { strategy: "into_existing", name: targetName.trim() };

  const engineOptions = (): EngineRestoreOptions =>
    profile?.engine === "postgres"
      ? {
          engine: "postgres",
          no_owner: true,
          no_privileges: true,
          parallel_jobs: null,
          only_tables: [],
          clean: false,
        }
      : {
          engine: "mysql",
          foreign_key_checks_off: true,
          unique_checks_off: true,
          autocommit_off: true,
          disable_binlog: false,
          charset: "utf8mb4",
          collation: "utf8mb4_unicode_ci",
        };

  const start = useMutation({
    mutationFn: () => {
      const request: RestoreRequest = {
        artifact_path: artifactPath,
        naming: naming(),
        engine: engineOptions(),
        verify_checksum: verifyChecksum,
        typed_confirmation: needsConfirmation ? confirmation : null,
      };
      return api.startRestore(profileId, request);
    },
    onSuccess: (jobId) => {
      setStarted(jobId);
      setConfirmation("");
    },
  });

  const targetReady =
    strategy === "new_timestamped" ? prefix.trim() !== "" : targetName.trim() !== "";
  const confirmed = !needsConfirmation || confirmation === targetName.trim();
  const canRun =
    profileId !== "" &&
    artifactPath !== "" &&
    targetReady &&
    confirmed &&
    !engineMismatch;

  return (
    <>
      <PageHeader
        title="Restore"
        description="Put an artifact back into a database. Nothing here compares it against a source — that is what a drill does."
      />

      <div className="space-y-5 p-6">
        {(profiles.isError || artifacts.isError) && (
          <ErrorNote
            title="Could not load the page"
            detail={((profiles.error ?? artifacts.error) as Error).message}
          />
        )}

        <section className="flex flex-wrap gap-3">
          <label className="min-w-64 flex-1">
            <span className="field-label">Restore into</span>
            <select
              className="field-input"
              value={profileId}
              onChange={(e) => {
                setProfileId(e.target.value);
                setConfirmation("");
              }}
            >
              <option value="">Select a connection…</option>
              {profiles.data?.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name} ({p.engine}
                  {p.environment === "prod" ? ", production" : ""})
                </option>
              ))}
            </select>
          </label>

          <label className="min-w-64 flex-1">
            <span className="field-label">Artifact</span>
            <select
              className="field-input"
              value={artifactPath}
              onChange={(e) => setArtifactPath(e.target.value)}
            >
              <option value="">Select a backup…</option>
              {artifacts.data?.map((a) => (
                <option key={a.path} value={a.path}>
                  {a.filename}
                </option>
              ))}
            </select>
          </label>
        </section>

        {artifacts.data?.length === 0 && (
          <p className="text-xs text-slate-500">
            The library is empty. Take a backup first, or copy an artifact into
            the backup folder.
          </p>
        )}

        {artifact && <ArtifactSummary artifact={artifact} />}

        {engineMismatch && (
          <ErrorNote
            title="This artifact cannot go into this connection"
            detail={`The backup was taken from ${artifact?.engine} and ${profile?.name} is ${profile?.engine}. Nothing here translates between the two dialects, so the restore is refused rather than half-applied.`}
          />
        )}

        {profileId && artifactPath && (
          <>
            <section className="space-y-3">
              <h2 className="text-sm font-medium text-slate-200">Target</h2>

              <div className="panel divide-y divide-slate-800">
                {STRATEGIES.map((s) => (
                  <label
                    key={s.value}
                    className="flex cursor-pointer items-start gap-3 px-4 py-3"
                  >
                    <input
                      type="radio"
                      className="mt-1"
                      name="strategy"
                      checked={strategy === s.value}
                      onChange={() => {
                        setStrategy(s.value);
                        setConfirmation("");
                      }}
                    />
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2">
                        <span className="text-sm text-slate-200">{s.label}</span>
                        {s.destructive && (
                          <span className="rounded bg-red-500/15 px-1.5 py-0.5 text-[11px] text-red-300">
                            destroys data
                          </span>
                        )}
                      </div>
                      <p className="mt-0.5 text-xs text-slate-500">{s.detail}</p>
                    </div>
                  </label>
                ))}
              </div>

              {strategy === "new_timestamped" ? (
                <label className="flex flex-col gap-1">
                  <span className="field-label">Name prefix</span>
                  <input
                    className="field-input w-72"
                    value={prefix}
                    onChange={(e) => setPrefix(e.target.value)}
                    placeholder="app"
                  />
                  <span className="text-xs text-slate-600">
                    The timestamp is added by the engine when the restore runs.
                  </span>
                </label>
              ) : (
                <label className="flex flex-col gap-1">
                  <span className="field-label">Database name</span>
                  <input
                    className="field-input w-72"
                    value={targetName}
                    onChange={(e) => {
                      setTargetName(e.target.value);
                      setConfirmation("");
                    }}
                    placeholder="dev_app"
                  />
                </label>
              )}
            </section>

            {needsConfirmation && targetName.trim() !== "" && (
              <Confirmation
                target={targetName.trim()}
                value={confirmation}
                onChange={setConfirmation}
                reason={
                  chosen.destructive
                    ? `This drops ${targetName.trim()} and everything in it.`
                    : `${profile?.name} is tagged production, so any restore that is not into a brand-new database is confirmed.`
                }
              />
            )}

            <label className="flex items-start gap-2">
              <input
                type="checkbox"
                className="mt-0.5"
                checked={verifyChecksum}
                onChange={(e) => setVerifyChecksum(e.target.checked)}
              />
              <span className="text-xs text-slate-400">
                Check the artifact against its manifest checksum first.
                <span className="text-slate-600">
                  {" "}
                  Cheap next to a restore, and it catches a truncated or altered
                  file before any of it reaches the server.
                </span>
              </span>
            </label>

            <div className="flex flex-wrap items-center gap-3 border-t border-slate-800 pt-4">
              <button
                onClick={() => start.mutate()}
                disabled={!canRun || start.isPending}
                className="flex items-center gap-1.5 rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
              >
                <Play className="h-4 w-4" />
                {start.isPending ? "Starting…" : "Run restore"}
              </button>

              {!confirmed && (
                <span className="text-xs text-amber-400">
                  Type the database name to confirm.
                </span>
              )}
            </div>

            {start.isError && (
              <ErrorNote
                title="Could not start the restore"
                detail={(start.error as Error).message}
              />
            )}

            {started && (
              <p className="text-xs text-emerald-400">
                Restore started — job {started.slice(0, 8)}. Watch it on the Jobs
                page.
              </p>
            )}

            <p className="max-w-3xl text-xs leading-relaxed text-slate-600">
              A finished restore means the artifact was written to the target
              without error. It does not mean the data matches the source it
              came from — the source has moved on since the backup was taken.
              Use a drill, or a sync with verification, for that.
            </p>
          </>
        )}
      </div>
    </>
  );
}

function ArtifactSummary({ artifact }: { artifact: Artifact }) {
  return (
    <div className="panel px-4 py-3">
      <div className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-slate-400">
        <span className="font-mono text-slate-200">{artifact.filename}</span>
        <span>{formatBytes(artifact.size_bytes)}</span>
        <span>{formatTimestamp(artifact.modified_at)}</span>
        {artifact.database && (
          <span>
            {artifact.database}
            {artifact.engine ? ` · ${artifact.engine}` : ""}
          </span>
        )}
        {artifact.source_profile_name && (
          <span>from {artifact.source_profile_name}</span>
        )}
        {artifact.table_count != null && (
          <span>
            {artifact.tables_with_data ?? 0} of {artifact.table_count} tables with
            data
          </span>
        )}
      </div>

      {!artifact.has_manifest && (
        <p className="mt-2 flex items-start gap-1.5 text-xs text-amber-400">
          <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          No manifest alongside this file. Its contents, its engine and its
          checksum are all unknown, so nothing can be checked before the restore
          or after it.
        </p>
      )}
    </div>
  );
}

function Confirmation({
  target,
  value,
  onChange,
  reason,
}: {
  target: string;
  value: string;
  onChange: (v: string) => void;
  reason: string;
}) {
  const matches = value === target;
  return (
    <div className="space-y-2 rounded-lg border border-red-500/30 bg-red-500/5 p-4">
      <p className="flex items-start gap-2 text-xs leading-relaxed text-red-200/90">
        <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-red-400" />
        <span>
          {reason} Type <code className="font-mono text-red-200">{target}</code>{" "}
          to confirm.
        </span>
      </p>
      <div className="flex items-center gap-2">
        <input
          className="field-input w-72"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          placeholder={target}
          autoComplete="off"
          spellCheck={false}
        />
        {matches && (
          <span className="flex items-center gap-1 text-xs text-emerald-400">
            <ShieldCheck className="h-3.5 w-3.5" />
            confirmed
          </span>
        )}
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
