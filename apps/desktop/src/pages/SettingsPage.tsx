import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import PageHeader from "@/components/PageHeader";
import { api } from "@/lib/api";
import type { AppSettings } from "@/bindings";

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
 * Install the bundled `dbsync` somewhere a terminal can find it.
 *
 * Worth its own section because every schedule offers a crontab line that
 * invokes it — without this the first thing that line asks of the user is to
 * go and find a binary they were never given.
 */
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
