import type { SshAuth, SshEndpoint } from "@/bindings";

/**
 * Where an SSH server is and how to authenticate to it.
 *
 * Only the endpoint: which bastion it goes through is a reference to another
 * saved connection, so it belongs to the page that knows the whole list.
 */
export default function SshEndpointFields({
  value,
  onChange,
}: {
  value: SshEndpoint;
  onChange: (patch: Partial<SshEndpoint>) => void;
}) {
  const keyFile = value.auth.kind === "key_file" ? value.auth : null;

  return (
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

      {keyFile && (
        <>
          <label className="col-span-4">
            <span className="field-label">Private key path</span>
            <input
              className="field-input"
              required
              placeholder="~/.ssh/id_ed25519"
              value={keyFile.path}
              onChange={(e) =>
                onChange({ auth: { ...keyFile, path: e.target.value } })
              }
            />
          </label>

          <label className="col-span-6 flex items-center gap-2">
            <input
              type="checkbox"
              checked={keyFile.passphrase_in_keychain}
              onChange={(e) =>
                onChange({
                  auth: { ...keyFile, passphrase_in_keychain: e.target.checked },
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

      {value.auth.kind === "agent" && (
        <p className="col-span-6 text-xs text-slate-500">
          Uses the running ssh-agent. Preferred — no key material passes through
          this app.
        </p>
      )}
    </div>
  );
}
