import { useId, useState } from "react";
import { homeDir } from "@tauri-apps/api/path";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen } from "lucide-react";

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
  const keyPathId = useId();
  const [browseError, setBrowseError] = useState<string | null>(null);

  async function browseForKey() {
    if (!keyFile) return;
    setBrowseError(null);
    try {
      // Tauri has returned this both with and without a trailing separator
      // across versions; strip it so the joins below never double up.
      const home = (await homeDir()).replace(/\/+$/, "");
      const picked = await open({
        title: "Select SSH private key",
        multiple: false,
        directory: false,
        // No extension filter: `id_ed25519` and friends have no suffix, so
        // any filter we invented would hide the very files being looked for.
        defaultPath: startingDir(keyFile.path, home),
      });
      if (typeof picked === "string") {
        onChange({ auth: { ...keyFile, path: collapseHome(picked, home) } });
      }
    } catch (e) {
      setBrowseError(
        e instanceof Error ? e.message : "Could not open the file picker.",
      );
    }
  }

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
          {/* A div rather than a label, because the Browse button is a sibling
              of the input: a button inside a label steals the label's click. */}
          <div className="col-span-4">
            <label className="field-label" htmlFor={keyPathId}>
              Private key path
            </label>
            <div className="flex gap-2">
              <input
                id={keyPathId}
                className="field-input"
                required
                placeholder="~/.ssh/id_ed25519"
                value={keyFile.path}
                onChange={(e) =>
                  onChange({ auth: { ...keyFile, path: e.target.value } })
                }
              />
              <button
                type="button"
                onClick={browseForKey}
                title="Browse for a private key file"
                className="flex shrink-0 items-center gap-1.5 rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-300 transition hover:bg-slate-800"
              >
                <FolderOpen className="h-4 w-4" />
                Browse…
              </button>
            </div>
            {browseError && (
              <p className="mt-1 text-xs text-red-400">{browseError}</p>
            )}
          </div>

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

/**
 * Where the file picker should open.
 *
 * `~/.ssh` is a dot-directory, and every native picker hides those. Opening
 * *inside* it is what makes the keys visible at all — landing in the home
 * folder instead leaves the user to guess at Cmd+Shift+. (macOS) or Ctrl+H
 * (GTK). Whatever is already typed wins, so re-opening the picker returns to
 * the key it last chose.
 */
function startingDir(current: string, home: string): string {
  const typed = expandHome(current.trim(), home);
  return typed || `${home}/.ssh`;
}

/** Expand a leading `~`, matching what the engine does before opening the key. */
function expandHome(path: string, home: string): string {
  return path.startsWith("~/") ? `${home}/${path.slice(2)}` : path;
}

/**
 * The inverse: store `~/.ssh/id_ed25519`, not `/Users/someone/.ssh/…`.
 *
 * The engine expands `~` itself, and the short form is what survives being
 * copied to another machine or another account.
 */
function collapseHome(path: string, home: string): string {
  return path.startsWith(`${home}/`) ? `~/${path.slice(home.length + 1)}` : path;
}
