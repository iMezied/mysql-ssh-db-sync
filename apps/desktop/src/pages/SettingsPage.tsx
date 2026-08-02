import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import PageHeader from "@/components/PageHeader";
import { api } from "@/lib/api";
import { cn } from "@/lib/utils";
import type {
  AppSettings,
  ImportReport,
  ToolSource,
  ToolStatus,
} from "@/bindings";

export default function SettingsPage() {
  const queryClient = useQueryClient();
  const info = useQuery({ queryKey: ["app-info"], queryFn: api.appInfo });
  const settings = useQuery({
    queryKey: ["app-settings"],
    queryFn: api.getAppSettings,
  });
  const status = useQuery({
    queryKey: ["scheduler-status"],
    queryFn: api.schedulerStatus,
  });

  const save = useMutation({
    mutationFn: (next: AppSettings) => api.setAppSettings(next),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ["app-settings"] });
      void queryClient.invalidateQueries({ queryKey: ["scheduler-status"] });
    },
  });

  const current = settings.data;
  const update = (patch: Partial<AppSettings>) => {
    if (current) save.mutate({ ...current, ...patch });
  };

  return (
    <>
      <PageHeader
        title="Settings"
        description="Background behaviour, application paths and versions."
      />

      <div className="space-y-6 p-6">
        <section className="space-y-3">
          <h2 className="text-sm font-medium text-slate-200">Background</h2>

          <div className="panel divide-y divide-slate-800">
            <Toggle
              label="Run schedules in this app"
              hint={
                status.data?.running
                  ? "The scheduler is running. Schedules fire while the app is open or in the menu bar."
                  : "Off — nothing will run here. Use system cron, or the dbsync daemon, instead."
              }
              checked={current?.scheduler_enabled ?? true}
              disabled={!current || save.isPending}
              onChange={(v) => update({ scheduler_enabled: v })}
            />

            <Toggle
              label="Keep running when the window is closed"
              hint="Closing the window leaves the app in the menu bar so schedules keep firing. With this off, closing the window stops every schedule."
              checked={current?.close_to_tray ?? true}
              disabled={!current || save.isPending}
              onChange={(v) => update({ close_to_tray: v })}
            />

            <Toggle
              label="Launch at login"
              hint="Starts DBSync Studio in the menu bar when you log in, so overnight schedules run without you opening it first."
              checked={current?.launch_at_login ?? false}
              disabled={!current || save.isPending}
              onChange={(v) => update({ launch_at_login: v })}
            />
          </div>

          {save.isError && (
            <p className="text-xs text-red-400">
              {(save.error as Error).message}
            </p>
          )}

          {current && !current.scheduler_enabled && !current.close_to_tray && (
            <p className="max-w-2xl text-xs leading-relaxed text-amber-300/80">
              With both of these off, nothing in Schedules will ever run. That is
              a valid setup if an external cron drives everything — otherwise
              turn one of them back on.
            </p>
          )}
        </section>

        <DatabaseToolsSection />

        <BackupKeySection />

        <SharedConfigSection />

        <CommandLineSection />

        <section className="space-y-3">
          <h2 className="text-sm font-medium text-slate-200">About</h2>
          <dl className="panel divide-y divide-slate-800">
            <Row label="Engine version" value={info.data?.engine_version} />
            <Row label="Application database" value={info.data?.store_path} mono />
          </dl>

          <p className="max-w-2xl text-xs leading-relaxed text-slate-500">
            The <code className="text-slate-400">dbsync</code> CLI reads this same
            database, so connections and schedules created here are available to
            scheduled and CI runs. Credentials live in the OS keychain and are
            never written to this file.
          </p>
        </section>
      </div>
    </>
  );
}

/**
 * The key that encrypted backups are written to.
 *
 * The export deliberately writes to a file and reports only its path. No
 * command in this app returns a secret to the webview — the same rule that
 * governs database passwords — so the key never sits in a JS string where a
 * script in the page, or an open devtools console, could read it.
 */
/**
 * Sharing configuration with a team.
 *
 * The whole point is what the bundle does *not* contain, so that is what the
 * copy leads with. A file safe to commit is only useful if people believe it
 * is safe to commit.
 */
function SharedConfigSection() {
  const queryClient = useQueryClient();
  const [path, setPath] = useState("");

  const exported = useMutation({ mutationFn: () => api.exportConfigToFile() });
  const preview = useMutation({
    mutationFn: () => api.previewConfigImport(path.trim()),
  });
  const imported = useMutation({
    mutationFn: () => api.importConfig(path.trim()),
    onSuccess: () => {
      // Everything the import may have touched.
      for (const key of [
        "profiles",
        "ssh-connections",
        "sync-plans",
        "destinations",
        "schedules",
      ]) {
        void queryClient.invalidateQueries({ queryKey: [key] });
      }
    },
  });

  const report = imported.data;

  return (
    <section className="space-y-3">
      <h2 className="text-sm font-medium text-slate-200">Shared configuration</h2>

      <div className="panel space-y-4 p-4">
        <p className="max-w-3xl text-xs leading-relaxed text-slate-400">
          A bundle carries connections, SSH servers, sync plans and off-site
          destinations — the shape of the work, not the ability to do it.{" "}
          <strong className="text-slate-200">
            It contains no passwords, no SSH keys and no access keys
          </strong>
          , because the types it is built from have no field one could occupy.
          It is safe to commit to a repository or attach to an onboarding
          document. Whoever imports it supplies their own credentials.
        </p>

        <div className="flex flex-wrap items-center gap-3">
          <button
            type="button"
            className="rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
            disabled={exported.isPending}
            onClick={() => exported.mutate()}
          >
            {exported.isPending ? "Exporting…" : "Export a bundle"}
          </button>
          {exported.data && (
            <span className="font-mono text-xs text-emerald-400">
              → {exported.data}
            </span>
          )}
          {exported.isError && (
            <span className="text-xs text-red-400">
              {(exported.error as Error).message}
            </span>
          )}
        </div>

        <div className="space-y-2 border-t border-slate-800 pt-4">
          <label className="flex flex-col gap-1">
            <span className="field-label">Import a bundle</span>
            <input
              className="field-input w-full max-w-xl font-mono text-xs"
              value={path}
              placeholder="/path/to/dbsync-config.json"
              onChange={(e) => {
                setPath(e.target.value);
                preview.reset();
                imported.reset();
              }}
            />
          </label>

          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              className="rounded-md border border-slate-700 px-3 py-1.5 text-xs text-slate-300 transition hover:bg-slate-800 disabled:opacity-50"
              disabled={!path.trim() || preview.isPending}
              onClick={() => preview.mutate()}
            >
              Preview
            </button>
            <button
              type="button"
              className="rounded-md bg-blue-600 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
              disabled={!preview.isSuccess || imported.isPending}
              onClick={() => imported.mutate()}
              title={
                preview.isSuccess
                  ? undefined
                  : "Preview it first — an import changes this machine"
              }
            >
              {imported.isPending ? "Importing…" : "Import"}
            </button>
          </div>

          {(preview.isError || imported.isError) && (
            <p className="text-xs text-red-400">
              {((preview.error ?? imported.error) as Error).message}
            </p>
          )}

          {preview.data && !report && (
            <p className="text-xs text-slate-400">
              {preview.data.profiles.length} connection(s),{" "}
              {preview.data.ssh_connections.length} SSH server(s),{" "}
              {preview.data.plans.length} plan(s),{" "}
              {preview.data.destinations.length} destination(s), exported by
              DBSync {preview.data.engine_version}. Existing records with the
              same name are updated; nothing is removed.
            </p>
          )}

          {report && <ImportSummary report={report} />}
        </div>
      </div>
    </section>
  );
}

function ImportSummary({ report }: { report: ImportReport }) {
  const created = [
    ...report.ssh_connections_created,
    ...report.profiles_created,
    ...report.plans_created,
    ...report.destinations_created,
  ];
  const updated = [
    ...report.ssh_connections_updated,
    ...report.profiles_updated,
    ...report.plans_updated,
    ...report.destinations_updated,
  ];

  return (
    <div className="space-y-2 text-xs">
      <p className="text-emerald-400">
        {created.length} created, {updated.length} updated.
      </p>

      {/* Named individually. "Some of these need credentials" is not
          something anyone acts on. */}
      {report.needs_credentials.length > 0 && (
        <p className="text-amber-400">
          These connections cannot connect until you set a password:{" "}
          <span className="font-mono">
            {report.needs_credentials.join(", ")}
          </span>
        </p>
      )}
      {report.destinations_needing_keys.length > 0 && (
        <p className="text-amber-400">
          These destinations arrived switched off and need an access key:{" "}
          <span className="font-mono">
            {report.destinations_needing_keys.join(", ")}
          </span>
        </p>
      )}
      {report.ssh_needing_passphrase.length > 0 && (
        <p className="text-amber-400">
          These SSH servers use a key with a passphrase this machine does not
          have:{" "}
          <span className="font-mono">
            {report.ssh_needing_passphrase.join(", ")}
          </span>
        </p>
      )}
      {/* Louder than the others: nothing here failed, which is the problem. A
          tunnelled connection quietly becoming a direct one is only noticed
          when it fails, or worse, when it succeeds against the wrong host. */}
      {report.orphaned_ssh_references.length > 0 && (
        <p className="text-red-400">
          These connections named an SSH server the bundle did not carry, and
          were imported as <strong>direct</strong> connections:{" "}
          <span className="font-mono">
            {report.orphaned_ssh_references.join(", ")}
          </span>
        </p>
      )}
      {report.orphaned_plans.length > 0 && (
        <p className="text-red-400">
          These plans could not be imported:{" "}
          {report.orphaned_plans.join("; ")}
        </p>
      )}
    </div>
  );
}

function BackupKeySection() {
  const queryClient = useQueryClient();
  const status = useQuery({ queryKey: ["backup-key"], queryFn: api.backupKeyStatus });
  const [exportedTo, setExportedTo] = useState<string | null>(null);
  const [recipients, setRecipients] = useState<string | null>(null);

  const invalidate = () => {
    void queryClient.invalidateQueries({ queryKey: ["backup-key"] });
  };

  const generate = useMutation({ mutationFn: api.generateBackupKey, onSuccess: invalidate });
  const exportKey = useMutation({
    mutationFn: api.exportBackupKeyToFile,
    onSuccess: (path) => {
      setExportedTo(path);
      invalidate();
    },
  });
  const saveRecipients = useMutation({
    mutationFn: (keys: string[]) => api.setBackupKeyRecipients(keys),
    onSuccess: invalidate,
  });

  const key = status.data;
  const recipientText =
    recipients ?? (key?.extra_recipients ?? []).join("\n");

  return (
    <section className="space-y-3">
      <h2 className="text-sm font-medium text-slate-200">Backup encryption</h2>

      <div className="panel space-y-4 p-4">
        {status.isPending ? (
          <p className="text-xs text-slate-500">Reading the key…</p>
        ) : status.isError ? (
          // Deliberately not folded into the "no key yet" branch. Reading the
          // key touches the OS keychain, and a locked keychain fails here — a
          // UI that answered "you have no key" would be telling the user
          // something false about a secret they may well have.
          <div className="space-y-1.5">
            <p className="text-xs text-red-400">
              Could not read the encryption key: {(status.error as Error).message}
            </p>
            <p className="max-w-2xl text-xs leading-relaxed text-slate-500">
              This does not mean there is no key — it means the app could not
              check. Unlocking the OS keychain and reopening this page is the
              usual fix.
            </p>
          </div>
        ) : !key?.exists ? (
          <>
            <p className="max-w-2xl text-xs leading-relaxed text-slate-500">
              No key yet. Encrypted backups are blocked until one exists and has
              been exported — an artifact encrypted to a key nobody has a copy of
              passes every integrity check and is unreadable forever.
            </p>
            <button
              onClick={() => generate.mutate()}
              disabled={generate.isPending}
              className="rounded-md bg-blue-600 px-3 py-1.5 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
            >
              {generate.isPending ? "Generating…" : "Generate a key"}
            </button>
          </>
        ) : (
          <>
            <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-xs">
              <dt className="text-slate-500">Public key</dt>
              <dd className="break-all font-mono text-slate-300">{key.public}</dd>
            </dl>

            {!key.exported ? (
              <div className="space-y-2 rounded-md border border-amber-500/30 bg-amber-500/5 p-3">
                <p className="text-xs leading-relaxed text-amber-200/90">
                  <strong className="font-semibold">Not exported yet.</strong>{" "}
                  Encrypted backups stay blocked until you have a copy of this
                  key somewhere other than this machine. Losing it makes every
                  encrypted artifact permanently unreadable.
                </p>
                <button
                  onClick={() => exportKey.mutate()}
                  disabled={exportKey.isPending}
                  className="rounded-md bg-blue-600 px-3 py-1.5 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
                >
                  {exportKey.isPending ? "Writing…" : "Export the key to a file"}
                </button>
              </div>
            ) : (
              <p className="text-xs text-slate-500">
                Exported at least once. Encrypted backups are allowed.{" "}
                <button
                  className="text-blue-400 underline-offset-2 hover:underline"
                  onClick={() => exportKey.mutate()}
                  disabled={exportKey.isPending}
                >
                  Export again
                </button>
              </p>
            )}

            {exportedTo && (
              <div className="space-y-1.5 rounded-md border border-slate-800 bg-slate-950 p-3">
                <p className="text-xs text-slate-300">
                  Written to{" "}
                  <code className="text-slate-200">{exportedTo}</code>
                </p>
                <p className="text-xs leading-relaxed text-amber-300/90">
                  Move it into a password manager or offline storage and delete
                  the file. It is readable only by you, but it is still a
                  plaintext secret sitting on the same machine as the backups it
                  decrypts.
                </p>
              </div>
            )}

            <div className="space-y-2">
              <label className="field-label" htmlFor="extra-recipients">
                Additional recipients
              </label>
              <textarea
                id="extra-recipients"
                rows={3}
                className="field-input font-mono text-xs"
                placeholder="age1..."
                value={recipientText}
                onChange={(e) => setRecipients(e.target.value)}
              />
              <p className="max-w-2xl text-xs leading-relaxed text-slate-500">
                One <code className="text-slate-400">age1…</code> public key per
                line. These can decrypt <em>future</em> backups as well as you —
                a colleague's key here is what makes a restore possible when you
                are unreachable. This installation's own key is always included.
              </p>
              <button
                onClick={() =>
                  saveRecipients.mutate(
                    recipientText
                      .split("\n")
                      .map((l) => l.trim())
                      .filter(Boolean),
                  )
                }
                disabled={saveRecipients.isPending || recipients === null}
                className="rounded-md border border-slate-700 px-3 py-1.5 text-sm text-slate-300 transition hover:bg-slate-800 disabled:opacity-50"
              >
                {saveRecipients.isPending ? "Saving…" : "Save recipients"}
              </button>
            </div>
          </>
        )}

        {[generate, exportKey, saveRecipients].map((m, i) =>
          m.isError ? (
            <p key={i} className="text-xs text-red-400">
              {(m.error as Error).message}
            </p>
          ) : null,
        )}
      </div>
    </section>
  );
}

/**
 * Install the bundled `dbsync` somewhere a terminal can find it.
 *
 * Worth its own section because every schedule offers a crontab line that
 * invokes it — without this the first thing that line asks of the user is to
 * go and find a binary they were never given.
 */
/** The three ways to supply the client binaries, in the order they are offered. */
const SOURCE_KINDS = [
  {
    kind: "local" as const,
    label: "Installed on this Mac",
    blurb: "Binaries found on this machine. Needs no container runtime.",
  },
  {
    kind: "docker_exec" as const,
    label: "A running container",
    blurb:
      "Borrow the tools from a container you already have up. Nothing to download — but it stops working when that container does.",
  },
  {
    kind: "docker_run" as const,
    label: "A container image",
    blurb:
      "Start a throwaway container per job. Always the right client, and nothing else has to stay running.",
  },
];

/**
 * Where the dump and restore binaries come from.
 *
 * Worth a whole section because a missing `mysqldump` is otherwise invisible
 * until a backup fails on it — and the failure names the binary without saying
 * that Docker would do just as well.
 */
function DatabaseToolsSection() {
  const queryClient = useQueryClient();
  const settings = useQuery({
    queryKey: ["app-settings"],
    queryFn: api.getAppSettings,
  });
  const tools = useQuery({
    queryKey: ["tools"],
    queryFn: api.discoverTools,
    // Every probe runs a binary, and with a container source that starts a
    // container apiece. Far too expensive to refetch on a window focus.
    staleTime: 60_000,
    refetchOnWindowFocus: false,
  });
  const containers = useQuery({
    queryKey: ["docker-containers"],
    queryFn: api.listDockerContainers,
    retry: false,
  });

  const source = settings.data?.tool_source;

  const saveSource = useMutation({
    mutationFn: async (next: ToolSource) => {
      if (!settings.data) throw new Error("settings have not loaded yet");
      return api.setAppSettings({ ...settings.data, tool_source: next });
    },
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ["app-settings"] });
      void queryClient.invalidateQueries({ queryKey: ["tools"] });
    },
  });

  const install = useMutation({
    mutationFn: (formula: string) => api.installToolWithBrew(formula),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["tools"] }),
  });

  const missingRequired =
    tools.data?.filter((t) => t.required && !t.location) ?? [];

  return (
    <section className="space-y-3">
      <h2 className="text-sm font-medium text-slate-200">Database tools</h2>

      <div className="panel space-y-4 p-4">
        <p className="max-w-2xl text-xs leading-relaxed text-slate-500">
          Dumps and restores run the vendors' own clients —{" "}
          <code className="text-slate-400">mysqldump</code>,{" "}
          <code className="text-slate-400">pg_dump</code>,{" "}
          <code className="text-slate-400">mongodump</code>. This app never
          bundles them; point it at binaries on this Mac, or let Docker supply
          them.
        </p>

        <div className="space-y-2">
          {SOURCE_KINDS.map(({ kind, label, blurb }) => (
            <label
              key={kind}
              className={cn(
                "flex cursor-pointer gap-3 rounded-md border p-3 transition",
                source?.kind === kind
                  ? "border-blue-500/60 bg-blue-500/5"
                  : "border-slate-800 hover:bg-slate-800/40",
              )}
            >
              <input
                type="radio"
                name="tool-source"
                className="mt-0.5 h-4 w-4"
                checked={source?.kind === kind}
                onChange={() =>
                  saveSource.mutate(
                    kind === "local"
                      ? { kind: "local" }
                      : kind === "docker_exec"
                        ? {
                            kind: "docker_exec",
                            container: containers.data?.[0]?.name ?? "",
                            bin_dir: null,
                          }
                        : { kind: "docker_run", image: "mysql:8" },
                  )
                }
              />
              <div className="space-y-0.5">
                <div className="text-sm text-slate-200">{label}</div>
                <div className="text-xs leading-relaxed text-slate-500">
                  {blurb}
                </div>
              </div>
            </label>
          ))}
        </div>

        {source?.kind === "docker_exec" && (
          <ExecSourceFields
            source={source}
            containers={containers.data ?? []}
            error={
              containers.error instanceof Error
                ? containers.error.message
                : containers.error
                  ? String(containers.error)
                  : null
            }
            onChange={(next) => saveSource.mutate(next)}
          />
        )}

        {source?.kind === "docker_run" && (
          <label className="block max-w-sm">
            <span className="field-label">Image</span>
            <input
              className="field-input"
              defaultValue={source.image}
              placeholder="mysql:8"
              onBlur={(e) =>
                e.target.value !== source.image &&
                saveSource.mutate({ kind: "docker_run", image: e.target.value })
              }
            />
          </label>
        )}

        {missingRequired.length > 0 && (
          <p className="text-xs text-amber-300/90">
            Missing:{" "}
            {missingRequired.map((t) => t.binary).join(", ")}. Backups for those
            engines will fail immediately — which is the point, rather than
            failing an hour in.
          </p>
        )}

        <div className="divide-y divide-slate-800 rounded-md border border-slate-800">
          {tools.isPending ? (
            <p className="p-3 text-xs text-slate-500">Looking for the tools…</p>
          ) : (
            tools.data?.map((tool) => (
              <ToolRow
                key={tool.binary}
                tool={tool}
                installing={
                  install.isPending && install.variables === tool.brew_formula
                }
                onInstall={() =>
                  tool.brew_formula && install.mutate(tool.brew_formula)
                }
              />
            ))
          )}
        </div>

        {install.error && (
          <p className="text-xs text-red-400">{String(install.error)}</p>
        )}
      </div>
    </section>
  );
}

function ExecSourceFields({
  source,
  containers,
  error,
  onChange,
}: {
  source: Extract<ToolSource, { kind: "docker_exec" }>;
  containers: { name: string; image: string }[];
  error: string | null;
  onChange: (next: ToolSource) => void;
}) {
  return (
    <div className="grid max-w-2xl grid-cols-2 gap-3">
      <label>
        <span className="field-label">Running container</span>
        <select
          className="field-input"
          value={source.container}
          onChange={(e) =>
            onChange({ ...source, container: e.target.value })
          }
        >
          <option value="">Choose a container…</option>
          {containers.map((c) => (
            <option key={c.name} value={c.name}>
              {c.name} ({c.image})
            </option>
          ))}
        </select>
        {/* An empty list and an unreachable Docker need different advice —
            and the engine's message is the specific one, naming where it
            looked. Guessing "is it installed and running?" was actively
            wrong for the case this most often is: Docker up, containers
            running, and the app started from Finder without a PATH that
            reaches the client. */}
        {error ? (
          <span className="mt-1 block text-xs leading-relaxed text-amber-300/90">
            {error}
          </span>
        ) : containers.length === 0 ? (
          <span className="mt-1 block text-xs text-slate-500">
            Nothing is running right now. Start a container and reopen this page.
          </span>
        ) : null}
      </label>

      <label>
        <span className="field-label">Binary directory (optional)</span>
        <input
          className="field-input"
          defaultValue={source.bin_dir ?? ""}
          placeholder="on the container's PATH"
          onBlur={(e) =>
            onChange({ ...source, bin_dir: e.target.value.trim() || null })
          }
        />
      </label>
    </div>
  );
}

function ToolRow({
  tool,
  installing,
  onInstall,
}: {
  tool: ToolStatus;
  installing: boolean;
  onInstall: () => void;
}) {
  return (
    <div className="flex items-center gap-3 px-3 py-2">
      <span
        className={cn(
          "h-1.5 w-1.5 shrink-0 rounded-full",
          tool.location
            ? "bg-emerald-400"
            : tool.required
              ? "bg-red-400"
              : "bg-slate-600",
        )}
      />
      <span className="w-32 shrink-0 font-mono text-xs text-slate-300">
        {tool.binary}
      </span>
      <span className="w-16 shrink-0 text-xs tabular-nums text-slate-400">
        {tool.version ?? ""}
      </span>
      <span className="flex-1 truncate font-mono text-[11px] text-slate-600">
        {tool.location ?? (tool.required ? "not found" : "not found (optional)")}
      </span>
      {!tool.location && tool.brew_formula && (
        <button
          type="button"
          onClick={onInstall}
          disabled={installing}
          className="shrink-0 rounded border border-slate-700 px-2 py-1 text-xs text-slate-300 transition hover:bg-slate-800 disabled:opacity-50"
        >
          {installing ? "Installing…" : `brew install ${tool.brew_formula}`}
        </button>
      )}
    </div>
  );
}

function CommandLineSection() {
  const queryClient = useQueryClient();
  const status = useQuery({ queryKey: ["cli-status"], queryFn: api.cliStatus });

  const install = useMutation({
    mutationFn: api.installCli,
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ["cli-status"] });
      void queryClient.invalidateQueries({ queryKey: ["schedules"] });
    },
  });

  const s = status.data;
  const done = install.data;

  return (
    <section className="space-y-3">
      <h2 className="text-sm font-medium text-slate-200">Command-line tool</h2>

      <div className="panel space-y-3 p-4">
        <p className="max-w-2xl text-xs leading-relaxed text-slate-500">
          <code className="text-slate-400">dbsync</code> does everything this app
          does, headlessly — it reads the same connections, plans and schedules.
          Installing it puts it on your <code className="text-slate-400">PATH</code>{" "}
          so the crontab line each schedule offers works as written.
        </p>

        {s?.linked_to_bundle ? (
          <p className="text-xs text-emerald-400">
            Installed at <code>{s.installed_path}</code> — pointing at this app.
          </p>
        ) : s?.installed_path ? (
          <p className="text-xs text-amber-300/90">
            A different <code>dbsync</code> is already on your PATH at{" "}
            <code>{s.installed_path}</code>. Installing will not replace it
            unless it is a link.
          </p>
        ) : s && !s.bundled_path ? (
          <p className="text-xs text-slate-500">
            This build does not ship the CLI. Build it with{" "}
            <code className="text-slate-400">cargo build -p db-sync-cli</code>.
          </p>
        ) : null}

        {s?.bundled_path && !s.linked_to_bundle && (
          <button
            onClick={() => install.mutate()}
            disabled={install.isPending}
            className="rounded-md bg-blue-600 px-3 py-1.5 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
          >
            {install.isPending ? "Installing…" : "Install command-line tool"}
          </button>
        )}

        {done && (
          <div className="space-y-1.5">
            <p className="text-xs text-slate-300">
              Linked at <code className="text-slate-200">{done.path}</code>
            </p>
            {!done.on_path && !done.manual_command && (
              <p className="text-xs text-amber-300/90">
                That directory is not on your PATH yet. Add it with{" "}
                <code>export PATH="$HOME/.local/bin:$PATH"</code> in your shell
                profile.
              </p>
            )}
            {done.manual_command && (
              <>
                <p className="text-xs text-amber-300/90">
                  Nothing writable was available, so run this yourself:
                </p>
                <code className="block overflow-x-auto whitespace-pre rounded bg-slate-950 px-2 py-1.5 font-mono text-[11px] text-slate-300">
                  {done.manual_command}
                </code>
              </>
            )}
          </div>
        )}

        {install.isError && (
          <p className="text-xs text-red-400">
            {(install.error as Error).message}
          </p>
        )}
      </div>
    </section>
  );
}

function Toggle({
  label,
  hint,
  checked,
  disabled,
  onChange,
}: {
  label: string;
  hint: string;
  checked: boolean;
  disabled?: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <label className="flex cursor-pointer items-start gap-3 px-4 py-3">
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
        className="mt-0.5 h-4 w-4 shrink-0 rounded border-slate-600 bg-slate-900 disabled:opacity-40"
      />
      <span className="min-w-0">
        <span className="block text-sm text-slate-200">{label}</span>
        <span className="mt-0.5 block text-xs leading-relaxed text-slate-500">
          {hint}
        </span>
      </span>
    </label>
  );
}

function Row({
  label,
  value,
  mono,
}: {
  label: string;
  value?: string;
  mono?: boolean;
}) {
  return (
    <div className="flex items-baseline gap-4 px-4 py-3">
      <dt className="w-44 shrink-0 text-xs uppercase tracking-wide text-slate-500">
        {label}
      </dt>
      <dd
        className={
          mono
            ? "break-all font-mono text-xs text-slate-300"
            : "text-sm text-slate-300"
        }
      >
        {value ?? "…"}
      </dd>
    </div>
  );
}
