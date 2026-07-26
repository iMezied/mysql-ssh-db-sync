import { useQuery } from "@tanstack/react-query";

import PageHeader from "@/components/PageHeader";
import { api } from "@/lib/api";

export default function SettingsPage() {
  const info = useQuery({ queryKey: ["app-info"], queryFn: api.appInfo });

  return (
    <>
      <PageHeader
        title="Settings"
        description="Application paths and versions."
      />

      <div className="p-6">
        <dl className="panel divide-y divide-slate-800">
          <Row label="Engine version" value={info.data?.engine_version} />
          <Row label="Application database" value={info.data?.store_path} mono />
        </dl>

        <p className="mt-4 max-w-2xl text-xs leading-relaxed text-slate-500">
          The <code className="text-slate-400">dbsync</code> CLI reads this same
          database, so connections created here are available to scheduled and
          CI runs. Credentials live in the OS keychain and are never written to
          this file.
        </p>
      </div>
    </>
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
