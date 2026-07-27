import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { ChevronDown, ChevronRight, KeyRound, Plus, Trash2 } from "lucide-react";

import PageHeader from "@/components/PageHeader";
import ConnectionTest from "@/components/ConnectionTest";
import { api, ApiError } from "@/lib/api";
import { cn } from "@/lib/utils";
import type {
  ConnectionProfile,
  DbConfig,
  Engine,
  EnvironmentTag,
  ProfileCreate,
  SshConnection,
} from "@/bindings";
import { DEFAULT_PORT, ENGINE_LABEL } from "@/lib/engineDefaults";

const ENV_STYLES: Record<EnvironmentTag, string> = {
  prod: "bg-red-500/15 text-red-300 ring-red-500/30",
  staging: "bg-amber-500/15 text-amber-300 ring-amber-500/30",
  dev: "bg-emerald-500/15 text-emerald-300 ring-emerald-500/30",
};

function emptyDraft(): ProfileCreate & { db: DbConfig } {
  return {
    name: "",
    engine: "mysql",
    environment: "dev",
    ssh_connection_id: null,
    db: { host: "127.0.0.1", port: DEFAULT_PORT.mysql, user: "", database: null },
  };
}

export default function ProfilesPage() {
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState<(ProfileCreate & { db: DbConfig }) | null>(null);
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);

  const profiles = useQuery({
    queryKey: ["profiles"],
    queryFn: api.listProfiles,
  });

  // Named here so a row can say *which* server it tunnels through, and so the
  // form can offer the saved ones rather than asking for the same host twice.
  const sshConnections = useQuery({
    queryKey: ["ssh-connections"],
    queryFn: api.listSshConnections,
  });
  const sshNames = new Map(
    (sshConnections.data ?? []).map((c) => [c.id, c.name] as const),
  );

  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["profiles"] });

  const create = useMutation({
    mutationFn: (input: ProfileCreate) =>
      api.createProfile(input, password.length > 0 ? password : null),
    onSuccess: async () => {
      setDraft(null);
      setPassword("");
      setError(null);
      await invalidate();
    },
    onError: (e: unknown) =>
      setError(
        e instanceof ApiError && e.kind === "duplicate_name"
          ? "A connection with that name already exists."
          : e instanceof Error
            ? e.message
            : "Could not save the connection.",
      ),
  });

  const remove = useMutation({
    mutationFn: api.deleteProfile,
    onSuccess: invalidate,
  });

  return (
    <>
      <PageHeader
        title="Connections"
        description="Each connection describes how to reach one server. Passwords are stored in the OS keychain, never in the app database or a config file."
        actions={
          <button
            onClick={() => {
              setDraft(emptyDraft());
              setError(null);
            }}
            className="flex items-center gap-1.5 rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500"
          >
            <Plus className="h-4 w-4" />
            New connection
          </button>
        }
      />

      <div className="p-6">
        {profiles.isLoading && (
          <p className="text-sm text-slate-500">Loading connections…</p>
        )}

        {profiles.isError && (
          <p className="text-sm text-red-400">
            Could not load connections: {(profiles.error as Error).message}
          </p>
        )}

        {profiles.data?.length === 0 && !draft && (
          <div className="panel p-10 text-center">
            <p className="text-sm text-slate-400">No connections yet.</p>
            <p className="mt-1 text-sm text-slate-500">
              Add the source server you back up from to get started.
            </p>
          </div>
        )}

        <div className="grid gap-3">
          {profiles.data?.map((p) => (
            <ProfileRow
              key={p.id}
              profile={p}
              sshName={
                p.ssh_connection_id
                  ? (sshNames.get(p.ssh_connection_id) ?? "an SSH server")
                  : null
              }
              connections={sshConnections.data ?? []}
              onDelete={() => remove.mutate(p.id)}
              deleting={remove.isPending && remove.variables === p.id}
            />
          ))}
        </div>

        {draft && (
          <form
            className="panel mt-4 p-5"
            onSubmit={(e) => {
              e.preventDefault();
              create.mutate(draft);
            }}
          >
            <h2 className="mb-4 text-sm font-semibold text-slate-200">
              New connection
            </h2>

            <div className="grid grid-cols-2 gap-4">
              <label className="col-span-2">
                <span className="field-label">Name</span>
                <input
                  className="field-input"
                  autoFocus
                  required
                  placeholder="prod-germany"
                  value={draft.name}
                  onChange={(e) => setDraft({ ...draft, name: e.target.value })}
                />
              </label>

              <label>
                <span className="field-label">Engine</span>
                <select
                  className="field-input"
                  value={draft.engine}
                  onChange={(e) => {
                    const engine = e.target.value as Engine;
                    setDraft({
                      ...draft,
                      engine,
                      // Follow the engine's default port unless it was edited.
                      db: {
                        ...draft.db,
                        port:
                          draft.db.port === DEFAULT_PORT[draft.engine]
                            ? DEFAULT_PORT[engine]
                            : draft.db.port,
                      },
                    });
                  }}
                >
                  {(Object.keys(ENGINE_LABEL) as Engine[]).map((e) => (
                    <option key={e} value={e}>
                      {ENGINE_LABEL[e]}
                    </option>
                  ))}
                </select>
              </label>

              <label>
                <span className="field-label">Environment</span>
                <select
                  className="field-input"
                  value={draft.environment}
                  onChange={(e) =>
                    setDraft({
                      ...draft,
                      environment: e.target.value as EnvironmentTag,
                    })
                  }
                >
                  <option value="dev">Development</option>
                  <option value="staging">Staging</option>
                  <option value="prod">Production</option>
                </select>
              </label>

              <label>
                <span className="field-label">Host</span>
                <input
                  className="field-input"
                  required
                  value={draft.db.host}
                  onChange={(e) =>
                    setDraft({ ...draft, db: { ...draft.db, host: e.target.value } })
                  }
                />
              </label>

              <label>
                <span className="field-label">Port</span>
                <input
                  className="field-input"
                  type="number"
                  required
                  min={1}
                  max={65535}
                  value={draft.db.port}
                  onChange={(e) =>
                    setDraft({
                      ...draft,
                      db: { ...draft.db, port: Number(e.target.value) },
                    })
                  }
                />
              </label>

              <label>
                <span className="field-label">User</span>
                <input
                  className="field-input"
                  required
                  value={draft.db.user}
                  onChange={(e) =>
                    setDraft({ ...draft, db: { ...draft.db, user: e.target.value } })
                  }
                />
              </label>

              <label>
                <span className="field-label">Database (optional)</span>
                <input
                  className="field-input"
                  value={draft.db.database ?? ""}
                  onChange={(e) =>
                    setDraft({
                      ...draft,
                      db: { ...draft.db, database: e.target.value || null },
                    })
                  }
                />
              </label>

              <label className="col-span-2">
                <span className="field-label">SSH tunnel</span>
                <select
                  className="field-input"
                  value={draft.ssh_connection_id ?? ""}
                  onChange={(e) =>
                    setDraft({
                      ...draft,
                      ssh_connection_id: e.target.value || null,
                    })
                  }
                >
                  <option value="">No tunnel — connect directly</option>
                  {sshConnections.data?.map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.name} ({c.endpoint.user}@{c.endpoint.host})
                    </option>
                  ))}
                </select>
                <p className="mt-1.5 text-xs leading-relaxed text-slate-500">
                  {draft.ssh_connection_id ? (
                    <>
                      Through a tunnel, the host and port above are resolved{" "}
                      <em>from the SSH server</em> — usually{" "}
                      <code className="text-slate-400">127.0.0.1</code>.
                    </>
                  ) : (
                    <>
                      SSH servers are saved once and shared.{" "}
                      <Link to="/ssh" className="text-blue-400 hover:underline">
                        Add one
                      </Link>{" "}
                      to tunnel through a bastion.
                    </>
                  )}
                </p>
              </label>

              <label className="col-span-2">
                <span className="field-label">Password (stored in keychain)</span>
                <input
                  className="field-input"
                  type="password"
                  autoComplete="off"
                  placeholder="Leave blank to add later"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                />
              </label>
            </div>

            {error && <p className="mt-3 text-sm text-red-400">{error}</p>}

            <div className="mt-5 flex gap-2">
              <button
                type="submit"
                disabled={create.isPending}
                className="rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
              >
                {create.isPending ? "Saving…" : "Save connection"}
              </button>
              <button
                type="button"
                onClick={() => {
                  setDraft(null);
                  setPassword("");
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

function ProfileRow({
  profile,
  sshName,
  connections,
  onDelete,
  deleting,
}: {
  profile: ConnectionProfile;
  sshName: string | null;
  connections: SshConnection[];
  onDelete: () => void;
  deleting: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  const secrets = useQuery({
    queryKey: ["secret-status", profile.id],
    queryFn: () => api.profileSecretStatus(profile.id),
  });

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
            {profile.name}
          </span>
          <span
            className={cn(
              "rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase ring-1",
              ENV_STYLES[profile.environment],
            )}
          >
            {profile.environment}
          </span>
          <span className="rounded bg-slate-800 px-1.5 py-0.5 text-[10px] uppercase text-slate-400">
            {profile.engine}
          </span>
        </div>
        <div className="mt-1 truncate font-mono text-xs text-slate-500">
          {profile.db.user}@{profile.db.host}:{profile.db.port}
          {profile.db.database ? `/${profile.db.database}` : ""}
          {sshName ? `  ·  via ${sshName}` : ""}
        </div>
      </div>

      <div className="flex shrink-0 items-center gap-3">
        <span
          className="flex items-center gap-1 text-xs"
          title={
            secrets.data?.has_db_password
              ? "Password stored in the OS keychain"
              : "No password stored yet"
          }
        >
          <KeyRound
            className={cn(
              "h-3.5 w-3.5",
              secrets.data?.has_db_password ? "text-emerald-400" : "text-slate-600",
            )}
          />
          <span className="text-slate-500">
            {secrets.data?.has_db_password ? "Key set" : "No key"}
          </span>
        </span>

        <button
          onClick={onDelete}
          disabled={deleting}
          title="Delete connection and purge its keychain entries"
          className="rounded p-1.5 text-slate-500 transition hover:bg-red-500/10 hover:text-red-400 disabled:opacity-40"
        >
          <Trash2 className="h-4 w-4" />
        </button>
      </div>
      </div>

      {expanded && (
        <div className="space-y-4 border-t border-slate-800 px-4 py-3">
          <TunnelSelect profile={profile} connections={connections} />
          <ConnectionTest profileId={profile.id} />
        </div>
      )}
    </div>
  );
}

/**
 * Re-points an existing connection at a different tunnel, or removes it.
 *
 * Only reachable once a connection exists, which is when the question actually
 * comes up: a bastion is introduced, retired, or replaced long after the
 * database it fronts was first configured.
 */
function TunnelSelect({
  profile,
  connections,
}: {
  profile: ConnectionProfile;
  connections: SshConnection[];
}) {
  const queryClient = useQueryClient();
  const [error, setError] = useState<string | null>(null);

  const attach = useMutation({
    // Presence is what carries the meaning here: an explicit null detaches,
    // and omitting the key would leave the tunnel exactly as it was.
    mutationFn: (id: string | null) =>
      api.updateProfile(profile.id, { ssh_connection_id: id }),
    onSuccess: async () => {
      setError(null);
      await queryClient.invalidateQueries({ queryKey: ["profiles"] });
    },
    onError: (e: unknown) =>
      setError(e instanceof Error ? e.message : "Could not change the tunnel."),
  });

  return (
    <div>
      <label className="block max-w-md">
        <span className="field-label">SSH tunnel</span>
        <select
          className="field-input"
          disabled={attach.isPending}
          value={profile.ssh_connection_id ?? ""}
          onChange={(e) => attach.mutate(e.target.value || null)}
        >
          <option value="">No tunnel — connect directly</option>
          {connections.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name} ({c.endpoint.user}@{c.endpoint.host})
            </option>
          ))}
        </select>
      </label>

      <p className="mt-1.5 text-xs leading-relaxed text-slate-500">
        {/* Said here rather than after the fact: the host field means a
            different thing on each side of this change, and a tunnel added to
            a working connection is the moment it silently stops working. */}
        The host and port above are resolved <em>from the SSH server</em> when a
        tunnel is set, and from this machine when it is not. Re-test the
        connection after changing this.
      </p>

      {error && <p className="mt-2 text-xs text-red-400">{error}</p>}
    </div>
  );
}
