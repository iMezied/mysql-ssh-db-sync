import type { ReactNode } from "react";

export default function PageHeader({
  title,
  description,
  actions,
}: {
  title: string;
  description?: string;
  actions?: ReactNode;
}) {
  return (
    <header className="flex items-start justify-between gap-4 border-b border-slate-800 px-6 py-5">
      <div>
        <h1 className="text-lg font-semibold text-slate-100">{title}</h1>
        {description && (
          <p className="mt-1 max-w-2xl text-sm text-slate-400">{description}</p>
        )}
      </div>
      {actions && <div className="flex shrink-0 gap-2">{actions}</div>}
    </header>
  );
}
