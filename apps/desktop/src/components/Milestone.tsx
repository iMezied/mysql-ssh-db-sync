import { Construction } from "lucide-react";

/**
 * Placeholder for a screen whose engine support has not landed yet.
 *
 * Deliberately explicit about what is missing: a fake-looking UI that silently
 * does nothing is worse than an honest gap, especially for a tool that touches
 * production databases.
 */
export default function Milestone({
  milestone,
  children,
}: {
  milestone: string;
  children: React.ReactNode;
}) {
  return (
    <div className="panel mx-6 my-6 p-6">
      <div className="flex items-start gap-3">
        <Construction className="mt-0.5 h-5 w-5 shrink-0 text-amber-400" />
        <div>
          <div className="text-sm font-medium text-slate-200">
            Arrives in {milestone}
          </div>
          <div className="mt-1 text-sm leading-relaxed text-slate-400">
            {children}
          </div>
        </div>
      </div>
    </div>
  );
}
