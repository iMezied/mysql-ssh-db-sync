import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ChevronDown,
  ChevronRight,
  KeyRound,
  Pencil,
  Plus,
  Trash2,
} from "lucide-react";

import PageHeader from "@/components/PageHeader";
import SshEndpointFields from "@/components/SshEndpointFields";
import { HostKeyBanner, Step } from "@/components/TestResult";
import { api, ApiError } from "@/lib/api";
import { cn } from "@/lib/utils";
import type {
  HostKeyPrompt,
  SshConnection,
  SshEndpoint,
  SshReport,
} from "@/bindings";

/** A connection being created or edited. `id` is null for a new one. */
type Draft = {
  id: string | null;
  name: string;
  endpoint: SshEndpoint;
  jump_host_id: string | null;
};

function emptyDraft(): Draft {
  return {
    id: null,
    name: "",
    endpoint: { host: "", port: 22, user: "", auth: { kind: "agent" } },
    jump_host_id: null,
  };
}

function describe(c: SshConnection): string {
  return `${c.endpoint.user}@${c.endpoint.host}:${c.endpoint.port}`;
}

export default function SshPage() {
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState<Draft | null>(null);
  const [passphrase, setPassphrase] = useState("");
  const [error, setError] = useState<string | null>(null);

  const connections = useQuery({
    queryKey: ["ssh-connections"],
    queryFn: api.listSshConnections,
  });

  const invalidate = () =>
    queryClient.invalidateQueries({ queryKey: ["ssh-connections"] });

  const message = (e: unknown) =>
    e instanceof ApiError && e.kind === "duplicate_name"
      ? "An SSH server with that name already exists."
      : e instanceof Error
        ? e.message
        : "Could not save the SSH server.";

  const save = useMutation({
    mutationFn: async (d: Draft) => {
      if (d.id === null) {
        return api.createSshConnection(
          { name: d.name, endpoint: d.endpoint, jump_host_id: d.jump_host_id },
          passphrase.length > 0 ? passphrase : null,
        );
      }
      // Every key is present on purpose: this form owns all three, and an
      // explicit null on `jump_host_id` is how a bastion is removed.
      return api.updateSshConnection(d.id, {
        name: d.name,
        endpoint: d.endpoint,
        jump_host_id: d.jump_host_id,
      });
    },
    onSuccess: async () => {
      setDraft(null);
      setPassphrase("");
      setError(null);
      await invalidate();
      // A rename shows up on every profile that tunnels through it.
      await queryClient.invalidateQueries({ queryKey: ["profiles"] });
    },
    onError: (e: unknown) => setError(message(e)),
  });

  // Chained jumps are not supported, so a server that already routes through
  // one cannot itself be offered as a bastion.
  const jumpOptions = (connections.data ?? []).filter(
    (c) => c.id !== draft?.id && c.jump_host_id === null,
  );

  return (
    <>
      <PageHeader
        title="SSH servers"
        description="Saved once and reused by any number of connections. Editing a server here changes every connection that tunnels through it; key passphrases live in the OS keychain."
        actions={
          <button
            onClick={() => {
              setDraft(emptyDraft());
              setPassphrase("");
              setError(null);
            }}
            className="flex items-center gap-1.5 rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500"
          >
            <Plus className="h-4 w-4" />
            New SSH server
          </button>
        }
      />

      <div className="p-6">
        {connections.isLoading && (
          <p className="text-sm text-slate-500">Loading SSH servers…</p>
        )}

        {connections.isError && (
          <p className="text-sm text-red-400">
            Could not load SSH servers: {(connections.error as Error).message}
          </p>
        )}

        {connections.data?.length === 0 && !draft && (
          <div className="panel p-10 text-center">
            <p className="text-sm text-slate-400">No SSH servers yet.</p>
            <p className="mt-1 text-sm text-slate-500">
              Add one to tunnel a connection through a bastion.
            </p>
          </div>
        )}

        {/* `grid-cols-1` for the same reason as on the connections list: an
            implicit track is floored at min-content, and the truncated
            user@host:port line makes that the width of the whole string. */}
        <div className="grid grid-cols-1 gap-3">
          {connections.data?.map((c) => (
            <ConnectionRow
              key={c.id}
              connection={c}
              jumpHost={
                connections.data.find((j) => j.id === c.jump_host_id) ?? null
              }
              onEdit={() => {
                setDraft({
                  id: c.id,
                  name: c.name,
                  endpoint: c.endpoint,
                  jump_host_id: c.jump_host_id,
                });
                setPassphrase("");
                setError(null);
              }}
            />
          ))}
        </div>

        {draft && (
          <form
            className="panel mt-4 p-5"
            onSubmit={(e) => {
              e.preventDefault();
              save.mutate(draft);
            }}
          >
            <h2 className="mb-4 text-sm font-semibold text-slate-200">
              {draft.id === null ? "New SSH server" : `Edit ${draft.name}`}
            </h2>

            <div className="grid grid-cols-6 gap-3">
              <label className="col-span-4">
                <span className="field-label">Name</span>
                <input
                  className="field-input"
                  autoFocus
                  required
                  placeholder="bastion-eu"
                  value={draft.name}
                  onChange={(e) => setDraft({ ...draft, name: e.target.value })}
                />
              </label>

              <label className="col-span-2">
                <span className="field-label">Jump host (optional)</span>
                <select
                  className="field-input"
                  value={draft.jump_host_id ?? ""}
                  onChange={(e) =>
                    setDraft({ ...draft, jump_host_id: e.target.value || null })
                  }
                >
                  <option value="">Connect directly</option>
                  {jumpOptions.map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.name}
                    </option>
                  ))}
                </select>
              </label>
            </div>

            <div className="mt-3">
              <SshEndpointFields
                value={draft.endpoint}
                onChange={(patch) =>
                  setDraft({ ...draft, endpoint: { ...draft.endpoint, ...patch } })
                }
              />
            </div>

            {draft.id === null && draft.endpoint.auth.kind === "key_file" && (
              <label className="mt-3 block">
                <span className="field-label">
                  Key passphrase (stored in keychain)
                </span>
                <input
                  className="field-input"
                  type="password"
                  autoComplete="off"
                  placeholder="Leave blank if the key has none"
                  value={passphrase}
                  onChange={(e) => setPassphrase(e.target.value)}
                />
              </label>
            )}

            {error && <p className="mt-3 text-sm text-red-400">{error}</p>}

            <div className="mt-5 flex gap-2">
              <button
                type="submit"
                disabled={save.isPending}
                className="rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
              >
                {save.isPending ? "Saving…" : "Save SSH server"}
              </button>
              <button
                type="button"
                onClick={() => {
                  setDraft(null);
                  setPassphrase("");
                  setError(null);
                }}
                className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-300 transition hover:bg-slate-800"
              >
                Cancel
              </button>
            </div>
          </form>
        )}
      </div>
    </>
  );
}

function ConnectionRow({
  connection,
  jumpHost,
  onEdit,
}: {
  connection: SshConnection;
  jumpHost: SshConnection | null;
  onEdit: () => void;
}) {
  const queryClient = useQueryClient();
  const [expanded, setExpanded] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);

  const status = useQuery({
    queryKey: ["ssh-status", connection.id],
    queryFn: () => api.sshConnectionStatus(connection.id),
  });

  const remove = useMutation({
    mutationFn: () => api.deleteSshConnection(connection.id),
    onSuccess: async () => {
      setDeleteError(null);
      await queryClient.invalidateQueries({ queryKey: ["ssh-connections"] });
    },
    // The refusal names every profile and bastion still pointing at it, which
    // is the whole answer — show it verbatim rather than a generic failure.
    onError: (e: unknown) =>
      setDeleteError(
        e instanceof Error ? e.message : "Could not delete this SSH server.",
      ),
  });

  const usedBy = [
    ...(status.data?.used_by_profiles ?? []),
    ...(status.data?.used_by_jump ?? []),
  ];

  return (
    <div className="panel">
      <div className="flex items-center justify-between gap-4 px-4 py-3">
        <button
          onClick={() => setExpanded((v) => !v)}
          className="shrink-0 text-slate-500 transition hover:text-slate-300"
          title={expanded ? "Hide details" : "Show details"}
        >
          {expanded ? (
            <ChevronDown className="h-4 w-4" />
          ) : (
            <ChevronRight className="h-4 w-4" />
          )}
        </button>

        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate text-sm font-medium text-slate-100">
              {connection.name}
            </span>
            <span className="rounded bg-slate-800 px-1.5 py-0.5 text-[10px] uppercase text-slate-400">
              {connection.endpoint.auth.kind === "agent" ? "agent" : "key file"}
            </span>
          </div>
          <div className="mt-1 truncate font-mono text-xs text-slate-500">
            {describe(connection)}
            {jumpHost ? `  ·  via ${jumpHost.name}` : ""}
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-3">
          <span className="text-xs text-slate-500">
            {usedBy.length === 0
              ? "Unused"
              : `Used by ${usedBy.length} ${usedBy.length === 1 ? "connection" : "connections"}`}
          </span>

          <button
            onClick={onEdit}
            title="Edit this SSH server"
            className="rounded p-1.5 text-slate-500 transition hover:bg-slate-800 hover:text-slate-300"
          >
            <Pencil className="h-4 w-4" />
          </button>

          <button
            onClick={() => remove.mutate()}
            disabled={remove.isPending}
            title="Delete this SSH server and its stored passphrase"
            className="rounded p-1.5 text-slate-500 transition hover:bg-red-500/10 hover:text-red-400 disabled:opacity-40"
          >
            <Trash2 className="h-4 w-4" />
          </button>
        </div>
      </div>

      {deleteError && (
        <p className="border-t border-slate-800 px-4 py-2 text-sm text-red-400">
          {deleteError}
        </p>
      )}

      {expanded && (
        <div className="space-y-4 border-t border-slate-800 px-4 py-3">
          <SshTest connectionId={connection.id} />

          {usedBy.length > 0 && (
            <p className="text-xs text-slate-500">
              Used by <span className="text-slate-300">{usedBy.join(", ")}</span>
              . An edit here reaches all of them.
            </p>
          )}

          {connection.endpoint.auth.kind === "key_file" && (
            <PassphraseField
              connectionId={connection.id}
              stored={status.data?.has_passphrase ?? false}
            />
          )}
        </div>
      )}
    </div>
  );
}

/** Store or clear the passphrase for a saved key. The value only goes one way. */
function PassphraseField({
  connectionId,
  stored,
}: {
  connectionId: string;
  stored: boolean;
}) {
  const queryClient = useQueryClient();
  const [value, setValue] = useState("");
  const [done, setDone] = useState<string | null>(null);

  const save = useMutation({
    mutationFn: () => api.setSshConnectionPassphrase(connectionId, value),
    onSuccess: async () => {
      setDone(value.length === 0 ? "Passphrase cleared." : "Passphrase stored.");
      setValue("");
      await queryClient.invalidateQueries({
        queryKey: ["ssh-status", connectionId],
      });
    },
  });

  return (
    <div className="flex items-end gap-2">
      <label className="flex-1">
        <span className="field-label flex items-center gap-1.5">
          <KeyRound
            className={cn(
              "h-3.5 w-3.5",
              stored ? "text-emerald-400" : "text-slate-600",
            )}
          />
          Key passphrase — {stored ? "stored in the keychain" : "not stored"}
        </span>
        <input
          className="field-input"
          type="password"
          autoComplete="off"
          placeholder={stored ? "Replace it, or save empty to clear" : "Not set"}
          value={value}
          onChange={(e) => {
            setValue(e.target.value);
            setDone(null);
          }}
        />
      </label>

      <button
        onClick={() => save.mutate()}
        disabled={save.isPending}
        className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 transition hover:bg-slate-800 disabled:opacity-50"
      >
        {save.isPending ? "Saving…" : "Save"}
      </button>

      {done && <span className="pb-2 text-xs text-emerald-400">{done}</span>}
      {save.isError && (
        <span className="pb-2 text-xs text-red-400">
          {(save.error as Error).message}
        </span>
      )}
    </div>
  );
}

/**
 * Tests the SSH hop on its own.
 *
 * Worth having separately from the profile test: it separates "the bastion is
 * unreachable" from "the database refused us", which otherwise look the same
 * from a profile that fails at step one.
 */
function SshTest({ connectionId }: { connectionId: string }) {
  const [report, setReport] = useState<SshReport | null>(null);
  const [trustError, setTrustError] = useState<string | null>(null);

  const test = useMutation({
    mutationFn: () => api.testSshConnection(connectionId),
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
    onSuccess: () => test.mutate(),
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
        {test.isPending ? "Testing…" : "Test SSH"}
      </button>

      {test.isError && (
        <p className="text-sm text-red-400">{(test.error as Error).message}</p>
      )}

      {report && (
        <div className="panel divide-y divide-slate-800">
          <Step label="SSH" outcome={report.ssh} />
          {report.authenticated_as && (
            <div className="flex items-baseline gap-3 px-3 py-2">
              <span className="w-20 shrink-0 text-xs uppercase tracking-wide text-slate-500">
                As
              </span>
              <span className="font-mono text-xs text-slate-300">
                {report.authenticated_as}
              </span>
            </div>
          )}
          {report.host_key && (
            <div className="flex items-baseline gap-3 px-3 py-2">
              <span className="w-20 shrink-0 text-xs uppercase tracking-wide text-slate-500">
                Host key
              </span>
              <span className="break-all font-mono text-xs text-slate-400">
                {report.host_key}
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
