import { useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { AlertTriangle, Check, Minus, ShieldQuestion, X } from "lucide-react";

import { api } from "@/lib/api";
import { cn } from "@/lib/utils";
import type { ConnectionReport, HostKeyPrompt, StepOutcome } from "@/bindings";

/**
 * Runs a profile's connection test and renders each step.
 *
 * A single pass/fail is close to useless here — four different things can be
 * wrong and they need different fixes.
 */
export default function ConnectionTest({ profileId }: { profileId: string }) {
  const [report, setReport] = useState<ConnectionReport | null>(null);
  const [trustError, setTrustError] = useState<string | null>(null);

  const test = useMutation({
    mutationFn: () => api.testConnection(profileId),
    onSuccess: (r) => {
      setReport(r);
      setTrustError(null);
    },
  });

  const trust = useMutation({
    mutationFn: (prompt: HostKeyPrompt) =>
      api.trustHostKey(
        prompt.host_port,
        prompt.algorithm,
        prompt.fingerprint,
        prompt.changed,
      ),
    onSuccess: () => {
      // Pinning only records the key; the user still needs the test to re-run.
      test.mutate();
    },
    onError: (e: unknown) =>
      setTrustError(e instanceof Error ? e.message : "Could not trust this key."),
  });

  return (
    <div className="space-y-3">
      <button
        onClick={() => test.mutate()}
        disabled={test.isPending}
        className="rounded-md border border-slate-700 px-3 py-1.5 text-sm text-slate-200 transition hover:bg-slate-800 disabled:opacity-50"
      >
        {test.isPending ? "Testing…" : "Test connection"}
      </button>

      {test.isError && (
        <p className="text-sm text-red-400">
          {(test.error as Error).message}
        </p>
      )}

      {report && (
        <div className="panel divide-y divide-slate-800">
          <Step label="SSH" outcome={report.ssh} />
          <Step label="Tunnel" outcome={report.tunnel} />
          <Step label="Database" outcome={report.db_ping} />
          <Step label="Catalog" outcome={report.catalog_read} />
          {report.server_version && (
            <div className="flex items-baseline gap-3 px-3 py-2">
              <span className="w-20 shrink-0 text-xs uppercase tracking-wide text-slate-500">
                Version
              </span>
              <span className="font-mono text-xs text-slate-300">
                {report.server_version}
              </span>
            </div>
          )}
        </div>
      )}

      {report?.host_key_prompt && (
        <HostKeyBanner
          prompt={report.host_key_prompt}
          pending={trust.isPending}
          error={trustError}
          onTrust={() => trust.mutate(report.host_key_prompt!)}
        />
      )}
    </div>
  );
}

function Step({ label, outcome }: { label: string; outcome: StepOutcome }) {
  const Icon =
    outcome.status === "ok" ? Check : outcome.status === "failed" ? X : Minus;
  const tone =
    outcome.status === "ok"
      ? "text-emerald-400"
      : outcome.status === "failed"
        ? "text-red-400"
        : "text-slate-600";

  return (
    <div className="flex items-baseline gap-3 px-3 py-2">
      <span className="w-20 shrink-0 text-xs uppercase tracking-wide text-slate-500">
        {label}
      </span>
      <Icon className={cn("h-3.5 w-3.5 shrink-0 translate-y-0.5", tone)} />
      <span
        className={cn(
          "text-xs",
          outcome.status === "failed" ? "text-red-300" : "text-slate-400",
        )}
      >
        {outcome.detail}
      </span>
    </div>
  );
}

/**
 * First contact, or a key that changed.
 *
 * These are deliberately styled very differently: an unknown key is routine,
 * a changed key is what a machine-in-the-middle looks like.
 */
function HostKeyBanner({
  prompt,
  pending,
  error,
  onTrust,
}: {
  prompt: HostKeyPrompt;
  pending: boolean;
  error: string | null;
  onTrust: () => void;
}) {
  return (
    <div
      className={cn(
        "rounded-lg border p-4",
        prompt.changed
          ? "border-red-500/40 bg-red-500/5"
          : "border-amber-500/40 bg-amber-500/5",
      )}
    >
      <div className="flex items-start gap-3">
        {prompt.changed ? (
          <AlertTriangle className="mt-0.5 h-5 w-5 shrink-0 text-red-400" />
        ) : (
          <ShieldQuestion className="mt-0.5 h-5 w-5 shrink-0 text-amber-400" />
        )}

        <div className="min-w-0 flex-1">
          <div
            className={cn(
              "text-sm font-medium",
              prompt.changed ? "text-red-300" : "text-amber-300",
            )}
          >
            {prompt.changed
              ? "This server's host key has CHANGED"
              : "Unrecognised host key"}
          </div>

          <p className="mt-1 text-sm leading-relaxed text-slate-400">
            {prompt.changed ? (
              <>
                The key for{" "}
                <span className="font-mono text-slate-300">
                  {prompt.host_port}
                </span>{" "}
                is not the one previously pinned. This happens after a legitimate
                server rebuild — and it is also exactly what an interception
                attack looks like. Do not continue until you have confirmed the
                new fingerprint with whoever runs the server.
              </>
            ) : (
              <>
                First connection to{" "}
                <span className="font-mono text-slate-300">
                  {prompt.host_port}
                </span>
                . Verify this fingerprint out of band before trusting it.
              </>
            )}
          </p>

          <dl className="mt-3 space-y-1 font-mono text-xs">
            {prompt.previous_fingerprint && (
              <div className="flex gap-2">
                <dt className="w-16 shrink-0 text-slate-500">pinned</dt>
                <dd className="break-all text-slate-500 line-through">
                  {prompt.previous_fingerprint}
                </dd>
              </div>
            )}
            <div className="flex gap-2">
              <dt className="w-16 shrink-0 text-slate-500">offered</dt>
              <dd
                className={cn(
                  "break-all",
                  prompt.changed ? "text-red-300" : "text-slate-200",
                )}
              >
                {prompt.fingerprint}
              </dd>
            </div>
            <div className="flex gap-2">
              <dt className="w-16 shrink-0 text-slate-500">type</dt>
              <dd className="text-slate-400">{prompt.algorithm}</dd>
            </div>
          </dl>

          <p className="mt-3 text-xs text-slate-500">
            Compare against{" "}
            <code className="text-slate-400">
              ssh-keygen -lf /etc/ssh/ssh_host_{prompt.algorithm.replace("ssh-", "")}
              _key.pub
            </code>{" "}
            run on the server itself.
          </p>

          {error && <p className="mt-2 text-xs text-red-400">{error}</p>}

          <button
            onClick={onTrust}
            disabled={pending}
            className={cn(
              "mt-3 rounded-md px-3 py-1.5 text-sm font-medium text-white transition disabled:opacity-50",
              prompt.changed
                ? "bg-red-600 hover:bg-red-500"
                : "bg-amber-600 hover:bg-amber-500",
            )}
          >
            {pending
              ? "Pinning…"
              : prompt.changed
                ? "I verified the new key — replace it"
                : "I verified this fingerprint — trust it"}
          </button>
        </div>
      </div>
    </div>
  );
}
