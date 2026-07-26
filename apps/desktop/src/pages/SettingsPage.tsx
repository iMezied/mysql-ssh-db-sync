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
