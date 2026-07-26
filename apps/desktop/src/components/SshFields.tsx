import type { SshAuth, SshConfig, SshEndpoint } from "@/bindings";

/**
 * SSH configuration for a profile, including an optional single-hop jump host.
 *
 * `null` means "connect directly"; the caller owns that distinction because it
 * changes what the database host/port field means (as seen from the SSH host,
 * versus from this machine).
 */
export default function SshFields({
  value,
  onChange,
}: {
  value: SshConfig | null;
  onChange: (next: SshConfig | null) => void;
}) {
  const enabled = value !== null;

  const setEndpoint = (patch: Partial<SshEndpoint>) => {
    if (!value) return;
    onChange({ ...value, ...patch });
  };

  const setJump = (patch: Partial<SshEndpoint> | null) => {
    if (!value) return;
    if (patch === null) {
      onChange({ ...value, jump_host: null });
      return;
    }
    const base: SshEndpoint = value.jump_host ?? {
      host: "",
      port: 22,
      user: "",
      auth: { kind: "agent" },
    };
    onChange({ ...value, jump_host: { ...base, ...patch } });
  };

  return (
    <div className="col-span-2 rounded-md border border-slate-800 p-4">
      <label className="flex items-center gap-2">
        <input
          type="checkbox"
          checked={enabled}
          onChange={(e) =>
            onChange(
              e.target.checked
                ? {
                    host: "",
                    port: 22,
                    user: "",
                    auth: { kind: "agent" },
                    jump_host: null,
                  }
                : null,
            )
          }
          className="h-4 w-4 rounded border-slate-600 bg-slate-900"
        />
        <span className="text-sm text-slate-200">Connect through an SSH tunnel</span>
      </label>

      {enabled && value && (
        <>
          <p className="mt-2 text-xs leading-relaxed text-slate-500">
            With a tunnel, the database host and port above are resolved{" "}
            <em>from the SSH server</em> — usually{" "}
            <code className="text-slate-400">127.0.0.1</code>.
          </p>

          <EndpointFields
            legend="SSH server"
            value={value}
            onChange={setEndpoint}
          />

          <label className="mt-4 flex items-center gap-2">
            <input
              type="checkbox"
              checked={value.jump_host !== null}
              onChange={(e) => setJump(e.target.checked ? {} : null)}
              className="h-4 w-4 rounded border-slate-600 bg-slate-900"
            />
            <span className="text-sm text-slate-200">
              Reach it through a jump host
            </span>
          </label>

          {value.jump_host && (
            <EndpointFields
              legend="Jump host"
              value={value.jump_host}
              onChange={setJump}
            />
          )}
        </>
      )}
    </div>
  );
}

function EndpointFields({
  legend,
  value,
  onChange,
}: {
  legend: string;
  value: SshEndpoint;
  onChange: (patch: Partial<SshEndpoint>) => void;
}) {
  const isKeyFile = value.auth.kind === "key_file";

  return (
    <fieldset className="mt-3">
      <legend className="mb-2 text-xs font-medium uppercase tracking-wide text-slate-500">
        {legend}
      </legend>

      <div className="grid grid-cols-6 gap-3">
        <label className="col-span-3">
          <span className="field-label">Host</span>
          <input
            className="field-input"
            required
            placeholder="bastion.example.com"
            value={value.host}
            onChange={(e) => onChange({ host: e.target.value })}
          />
        </label>

        <label>
          <span className="field-label">Port</span>
          <input
            className="field-input"
            type="number"
            min={1}
            max={65535}
            value={value.port}
            onChange={(e) => onChange({ port: Number(e.target.value) })}
          />
        </label>

        <label className="col-span-2">
          <span className="field-label">User</span>
          <input
            className="field-input"
            required
            placeholder="ubuntu"
            value={value.user}
            onChange={(e) => onChange({ user: e.target.value })}
          />
        </label>

        <label className="col-span-2">
          <span className="field-label">Authentication</span>
          <select
            className="field-input"
            value={value.auth.kind}
            onChange={(e) => {
              const next: SshAuth =
                e.target.value === "agent"
                  ? { kind: "agent" }
                  : {
                      kind: "key_file",
                      path: "~/.ssh/id_ed25519",
                      passphrase_in_keychain: false,
                    };
              onChange({ auth: next });
            }}
          >
            <option value="agent">ssh-agent</option>
            <option value="key_file">Key file</option>
          </select>
        </label>

        {isKeyFile && value.auth.kind === "key_file" && (
          <>
            <label className="col-span-4">
              <span className="field-label">Private key path</span>
              <input
                className="field-input"
                required
                placeholder="~/.ssh/id_ed25519"
                value={value.auth.path}
                onChange={(e) =>
                  onChange({
                    auth: {
                      kind: "key_file",
                      path: e.target.value,
                      passphrase_in_keychain:
                        value.auth.kind === "key_file"
                          ? value.auth.passphrase_in_keychain
                          : false,
                    },
                  })
                }
              />
            </label>

            <label className="col-span-6 flex items-center gap-2">
              <input
                type="checkbox"
                checked={value.auth.passphrase_in_keychain}
                onChange={(e) =>
                  onChange({
                    auth: {
                      kind: "key_file",
                      path:
                        value.auth.kind === "key_file" ? value.auth.path : "",
                      passphrase_in_keychain: e.target.checked,
                    },
                  })
                }
                className="h-4 w-4 rounded border-slate-600 bg-slate-900"
              />
              <span className="text-xs text-slate-400">
                This key has a passphrase (stored in the OS keychain)
              </span>
            </label>
          </>
        )}
      </div>

      {value.auth.kind === "agent" && (
        <p className="mt-2 text-xs text-slate-500">
          Uses the running ssh-agent. Preferred — no key material passes through
          this app.
        </p>
      )}
    </fieldset>
  );
}
