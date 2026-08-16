import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import {
  ChevronDown,
  ChevronRight,
  KeyRound,
  Pencil,
  Plus,
  Trash2,
} from "lucide-react";

import PageHeader from "@/components/PageHeader";
import ConnectionTest from "@/components/ConnectionTest";
import EngineMark from "@/components/EngineMark";
import EnvironmentBadge from "@/components/EnvironmentBadge";
import { api, ApiError } from "@/lib/api";
import { cn } from "@/lib/utils";
import type {
  ConnectionProfile,
  DbConfig,
  Engine,
  EnvironmentTag,
} from "@/bindings";
import { DEFAULT_PORT, ENGINE_LABEL } from "@/lib/engineDefaults";

/** A connection being created or edited. `id` is null for a new one. */
type Draft = {
  id: string | null;
  name: string;
  engine: Engine;
  environment: EnvironmentTag;
  ssh_connection_id: string | null;
  db: DbConfig;
};

function emptyDraft(): Draft {
  return {
    id: null,
    name: "",
    engine: "mysql",
    environment: "dev",
    ssh_connection_id: null,
    db: { host: "127.0.0.1", port: DEFAULT_PORT.mysql, user: "", database: null },
  };
}

export default function ProfilesPage() {
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState<Draft | null>(null);
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

  const save = useMutation({
    mutationFn: async (d: Draft) => {
      const { id, ...fields } = d;
      if (id === null) {
        return api.createProfile(fields, password.length > 0 ? password : null);
      }
      // Every key is present on purpose: this form owns all of them, and an
      // explicit null on `ssh_connection_id` is how a tunnel is removed.
      // The password is not here — it is set from the row, so that editing a
      // host cannot silently rewrite a credential the form never showed.
      return api.updateProfile(id, { ...fields, tool_overrides: null });
    },
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
              onEdit={() => {
                setDraft({
                  id: p.id,
                  name: p.name,
                  engine: p.engine,
                  environment: p.environment,
                  ssh_connection_id: p.ssh_connection_id,
                  db: p.db,
                });
                setPassword("");
                setError(null);
              }}
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
              save.mutate(draft);
            }}
          >
            <h2 className="mb-4 text-sm font-semibold text-slate-200">
              {draft.id === null ? "New connection" : `Edit ${draft.name}`}
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
                  {draft.id !== null && (
                    // Said before the save, not after: the host field means a
                    // different thing on each side of this change, and a tunnel
                    // added to a working connection is the moment it silently
                    // stops working.
                    <> Re-test the connection after changing this.</>
                  )}
                </p>
              </label>

              {draft.id === null && (
                <label className="col-span-2">
                  <span className="field-label">
                    Password (stored in keychain)
                  </span>
                  <input
                    className="field-input"
                    type="password"
                    autoComplete="off"
                    placeholder="Leave blank to add later"
                    value={password}
                    onChange={(e) => setPassword(e.target.value)}
                  />
                </label>
              )}
            </div>

            {draft.id !== null && (
              <p className="mt-3 text-xs text-slate-500">
                The password is changed from the connection's own row — expand
                it to set, replace, or clear the stored one.
              </p>
            )}

            {error && <p className="mt-3 text-sm text-red-400">{error}</p>}

            <div className="mt-5 flex gap-2">
              <button
                type="submit"
                disabled={save.isPending}
                className="rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
              >
                {save.isPending ? "Saving…" : "Save connection"}
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
  onEdit,
  onDelete,
  deleting,
}: {
  profile: ConnectionProfile;
  sshName: string | null;
  onEdit: () => void;
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
          <EngineMark engine={profile.engine} />
          <span className="truncate text-sm font-medium text-slate-100">
            {profile.name}
          </span>
          <EnvironmentBadge environment={profile.environment} />
          <span className="text-[10px] uppercase tracking-wide text-slate-500">
            {ENGINE_LABEL[profile.engine]}
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
          onClick={onEdit}
          title="Edit this connection"
          className="rounded p-1.5 text-slate-500 transition hover:bg-slate-800 hover:text-slate-300"
        >
          <Pencil className="h-4 w-4" />
        </button>

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
          <PasswordField
            profileId={profile.id}
            stored={secrets.data?.has_db_password ?? false}
          />
          <ConnectionTest profileId={profile.id} />
        </div>
      )}
    </div>
  );
}

/**
 * Store, replace, or clear the database password for a saved connection.
 *
 * Lives on the row rather than in the edit form because the value only ever
 * travels one way: the form is populated from the profile, and there is
 * nothing to populate this with. It is also the second half of the create
 * form's "leave blank to add later" — without it, there was no later.
 */
function PasswordField({
  profileId,
  stored,
}: {
  profileId: string;
  stored: boolean;
}) {
  const queryClient = useQueryClient();
  const [value, setValue] = useState("");
  const [done, setDone] = useState<string | null>(null);

  const save = useMutation({
    mutationFn: () => api.setProfileSecret(profileId, "db_password", value),
    onSuccess: async () => {
      setDone(value.length === 0 ? "Password cleared." : "Password stored.");
      setValue("");
      await queryClient.invalidateQueries({
        queryKey: ["secret-status", profileId],
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
          Database password — {stored ? "stored in the keychain" : "not stored"}
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
