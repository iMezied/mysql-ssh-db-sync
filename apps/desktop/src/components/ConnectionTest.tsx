import { useState } from "react";
import { useMutation } from "@tanstack/react-query";

import { HostKeyBanner, Step } from "@/components/TestResult";
import { api } from "@/lib/api";
import type { ConnectionReport, HostKeyPrompt } from "@/bindings";

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
