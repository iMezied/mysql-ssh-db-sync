import type { EnvironmentTag } from "@/bindings";
import { cn } from "@/lib/utils";

/**
 * Production, staging or development, said in colour.
 *
 * This lived inside `ProfilesPage` as a local constant used exactly once, which
 * meant the one page where you are least likely to make a mistake — the list of
 * connections, where you are reading rather than acting — was the only page
 * that showed the tag at all. Every dropdown that picks a connection to back
 * up, restore over or sync into showed the same plain text for `dev-local` and
 * `prod-eu`.
 *
 * Colour is the signal here, so the words stay too: red on amber on green is
 * not readable to everyone, and "PROD" is.
 */
const ENV_STYLES: Record<EnvironmentTag, string> = {
  prod: "bg-red-500/15 text-red-300 ring-red-500/30",
  staging: "bg-amber-500/15 text-amber-300 ring-amber-500/30",
  dev: "bg-emerald-500/15 text-emerald-300 ring-emerald-500/30",
};

export default function EnvironmentBadge({
  environment,
  className,
}: {
  environment: EnvironmentTag;
  className?: string;
}) {
  return (
    <span
      className={cn(
        "rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase ring-1",
        ENV_STYLES[environment],
        className,
      )}
    >
      {environment}
    </span>
  );
}

export { ENV_STYLES };
