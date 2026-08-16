import type { ConnectionProfile } from "@/bindings";
import EngineMark from "@/components/EngineMark";
import EnvironmentBadge from "@/components/EnvironmentBadge";
import { ENGINE_LABEL } from "@/lib/engineDefaults";
import { cn } from "@/lib/utils";

/**
 * What the connection dropdown above this actually selected.
 *
 * Tauri renders through WKWebView on macOS, where an `<option>` cannot hold
 * markup and ignores nearly all styling — so the dropdown itself can never show
 * a logo or a red production tag, however much it needs to. This is the answer:
 * the select stays a select, and what it chose is restated underneath in full,
 * with the engine's mark, the environment in colour and the address it will
 * actually reach.
 *
 * Renders nothing when nothing is selected, so a call site is one line and does
 * not need its own guard.
 */
export default function ProfileChip({
  profile,
  sshName,
  className,
}: {
  profile: ConnectionProfile | null | undefined;
  /** The tunnel this connection goes through, when the page knows it. */
  sshName?: string | null;
  className?: string;
}) {
  if (!profile) return null;

  return (
    <span
      className={cn(
        "mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-slate-500",
        className,
      )}
    >
      <EngineMark engine={profile.engine} size="sm" />
      <span className="text-slate-300">{ENGINE_LABEL[profile.engine]}</span>
      <EnvironmentBadge environment={profile.environment} />
      <span className="truncate font-mono">
        {profile.db.user}@{profile.db.host}:{profile.db.port}
        {profile.db.database ? `/${profile.db.database}` : ""}
      </span>
      {sshName && <span>· via {sshName}</span>}
    </span>
  );
}
