import { NavLink } from "react-router-dom";
import {
  Archive,
  ArrowLeftRight,
  CloudUpload,
  CalendarClock,
  Database,
  DownloadCloud,
  EyeOff,
  ListChecks,
  Settings,
  UploadCloud,
} from "lucide-react";

import { cn } from "@/lib/utils";

const links = [
  { to: "/profiles", label: "Connections", icon: Database },
  { to: "/backup", label: "Backup", icon: DownloadCloud },
  { to: "/restore", label: "Restore", icon: UploadCloud },
  { to: "/sync", label: "Sync", icon: ArrowLeftRight },
  { to: "/schedules", label: "Schedules", icon: CalendarClock },
  { to: "/masking", label: "Masking", icon: EyeOff },
  { to: "/library", label: "Library", icon: Archive },
  { to: "/offsite", label: "Off-site", icon: CloudUpload },
  { to: "/jobs", label: "Jobs", icon: ListChecks },
  { to: "/settings", label: "Settings", icon: Settings },
] as const;

export default function Sidebar() {
  return (
    <nav className="flex w-56 shrink-0 flex-col border-r border-slate-800 bg-slate-900">
      <div className="flex items-center gap-2 px-4 py-4">
        <div className="grid h-8 w-8 place-items-center rounded-lg bg-blue-600">
          <Database className="h-4 w-4 text-white" />
        </div>
        <div className="leading-tight">
          <div className="text-sm font-semibold text-slate-100">DBSync</div>
          <div className="text-[11px] text-slate-500">Studio</div>
        </div>
      </div>

      <div className="flex flex-1 flex-col gap-0.5 px-2 py-2">
        {links.map(({ to, label, icon: Icon }) => (
          <NavLink
            key={to}
            to={to}
            className={({ isActive }) =>
              cn(
                "flex items-center gap-2.5 rounded-md px-3 py-2 text-sm transition",
                isActive
                  ? "bg-blue-600/15 font-medium text-blue-300"
                  : "text-slate-400 hover:bg-slate-800 hover:text-slate-200",
              )
            }
          >
            <Icon className="h-4 w-4" />
            {label}
          </NavLink>
        ))}
      </div>
    </nav>
  );
}
