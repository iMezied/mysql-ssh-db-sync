import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle,
  CalendarClock,
  Check,
  Copy,
  Play,
  Plus,
  ShieldCheck,
  Trash2,
  X,
} from "lucide-react";

import PageHeader from "@/components/PageHeader";
import { api } from "@/lib/api";
import { destructiveTargets, isArmed } from "@/lib/pipeline";
import { useProgressStore } from "@/lib/jobProgress";
import { cn } from "@/lib/utils";
import { events } from "@/bindings";
import type {
  ConnectionProfile,
  EngineBackupOptions,
  EngineRestoreOptions,
  NotifyPolicy,
  ScheduleCreate,
  ScheduleKind,
  ScheduleTimezone,
  ScheduleView,
  SyncPlan,
} from "@/bindings";
import { defaultBackupOptions, defaultRestoreOptions } from "@/lib/engineDefaults";

/** Where a new sync schedule starts. */
const DEFAULT_SYNC_CRON = "30 2 * * *";
/** Later than the backup, so a drill checks a finished artifact. */
const DEFAULT_DRILL_CRON = "0 4 * * *";

/** Expressions people actually want, so nobody has to remember the field order. */
const PRESETS = [
  { label: "Every night at 02:30", cron: "30 2 * * *" },
  { label: "Weeknights at 02:30", cron: "30 2 * * 1-5" },
  { label: "Every hour", cron: "@hourly" },
  { label: "Sunday at 03:00", cron: "0 3 * * 0" },
  { label: "1st of the month", cron: "0 4 1 * *" },
] as const;

export default function SchedulesPage() {
  const [creating, setCreating] = useState(false);
  const queryClient = useQueryClient();

  const schedules = useQuery({ queryKey: ["schedules"], queryFn: api.listSchedules });
  const status = useQuery({
    queryKey: ["scheduler-status"],
    queryFn: api.schedulerStatus,
    // The list shows a live "running" badge, so it has to keep up with runs
    // started by the scheduler itself rather than by this window.
    refetchInterval: 5000,
  });

  // A run that finishes in the background must update the list; otherwise the
  // page shows last night's outcome until the user reloads.
  useEffect(() => {
    const unlisten = events.scheduledRunFinished.listen(() => {
      void queryClient.invalidateQueries({ queryKey: ["schedules"] });
      void queryClient.invalidateQueries({ queryKey: ["jobs"] });
    });
    return () => {
      void unlisten.then((f) => f());
    };
  }, [queryClient]);

  return (
    <>
      <PageHeader
        title="Schedules"
        description="Run a backup, a sync, or a restore drill on a timer — unattended."
      />

      <div className="space-y-5 p-6">
        {status.data && !status.data.running && (
          <Warning>
            The in-app scheduler is turned off, so nothing here will run while
            the app is open. Turn it back on in Settings, or drive these
            schedules from system cron using the line each one offers.
          </Warning>
        )}

        <div className="flex items-center justify-between">
          <h2 className="text-sm font-medium text-slate-200">
            {schedules.data?.length ?? 0} schedule
            {schedules.data?.length === 1 ? "" : "s"}
          </h2>
          <button
            onClick={() => setCreating(true)}
            className="flex items-center gap-1.5 rounded-md bg-blue-600 px-3 py-1.5 text-sm font-medium text-white transition hover:bg-blue-500"
          >
            <Plus className="h-4 w-4" />
            New schedule
          </button>
        </div>

        {schedules.data?.length === 0 && !creating && (
          <div className="panel flex flex-col items-center gap-2 p-10 text-center">
            <CalendarClock className="h-8 w-8 text-slate-600" />
            <p className="text-sm text-slate-400">No schedules yet.</p>
            <p className="max-w-md text-xs leading-relaxed text-slate-500">
              A schedule runs a saved sync plan on a cron expression. Create a
              plan on the Sync page first, then point a schedule at it.
            </p>
          </div>
        )}

        <div className="space-y-3">
          {schedules.data?.map((view) => (
            <ScheduleRow key={view.schedule.id} view={view} />
          ))}
        </div>

        {creating && <ScheduleForm onClose={() => setCreating(false)} />}
      </div>
    </>
  );
}

function ScheduleRow({ view }: { view: ScheduleView }) {
  const { schedule } = view;
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const noteLaunch = useProgressStore((s) => s.noteLaunch);
  const [crontab, setCrontab] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const invalidate = () =>
    void queryClient.invalidateQueries({ queryKey: ["schedules"] });

  const toggle = useMutation({
    mutationFn: () =>
      api.updateSchedule(schedule.id, {
        enabled: !schedule.enabled,
        name: null,
        cron: null,
        timezone: null,
        action: null,
        notify: null,
        catch_up: null,
      }),
    onSuccess: invalidate,
  });

  const runNow = useMutation({
    mutationFn: () => api.runScheduleNow(schedule.id),
    // A run started by hand is watched like any other; the scheduler's own
    // ticks are not, which is why only this path navigates.
    onSuccess: (jobId) => {
      invalidate();
      noteLaunch(jobId, {
        title: SCHEDULE_KIND_LABELS[schedule.kind ?? "sync"],
        detail: schedule.name,
      });
      navigate(`/jobs/${jobId}`);
    },
  });

  const remove = useMutation({
    mutationFn: () => api.deleteSchedule(schedule.id),
    onSuccess: invalidate,
  });

  const showCrontab = useMutation({
    mutationFn: () => api.crontabLine(schedule.id),
    onSuccess: setCrontab,
  });

  return (
    <div className="panel space-y-3 p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium text-slate-100">
              {schedule.name}
            </span>
            {schedule.kind === "drill" && (
              // Distinguished because the two do opposite things: a sync
              // produces backups, a drill checks them. A list that showed
              // them alike would make "we have four schedules" say nothing
              // about whether any of them verifies anything.
              <span className="rounded bg-emerald-500/15 px-1.5 py-0.5 text-[10px] uppercase text-emerald-300">
                drill
              </span>
            )}
            {!schedule.enabled && (
              <span className="rounded bg-slate-700 px-1.5 py-0.5 text-[10px] uppercase text-slate-300">
                paused
              </span>
            )}
            {view.running && (
              <span className="rounded bg-blue-500/20 px-1.5 py-0.5 text-[10px] uppercase text-blue-300">
                running
              </span>
            )}
            <Outcome value={schedule.last_outcome} />
          </div>

          <p className="mt-1 text-xs text-slate-400">
            <code className="text-slate-300">{schedule.cron}</code> ·{" "}
            {view.description} · {schedule.timezone === "utc" ? "UTC" : "local time"}
          </p>

          <p className="mt-0.5 text-xs text-slate-500">
            {/* A drill has a destination profile too — it has to restore
                somewhere — so this cannot be decided by that field alone,
                or every drill would read as a cross-server sync. */}
            {schedule.kind === "drill"
              ? "Restores the newest backup and checks it"
              : schedule.dest_profile_id
                ? "Sync to another server"
                : "Backup only"}
            {" · next "}
            {view.next_run_at
              ? new Date(view.next_run_at).toLocaleString()
              : "—"}
            {schedule.last_run_at &&
              ` · last ${new Date(schedule.last_run_at).toLocaleString()}`}
          </p>
        </div>

        <div className="flex shrink-0 items-center gap-1.5">
          <SmallButton
            onClick={() => runNow.mutate()}
            disabled={runNow.isPending || view.running}
            title="Run once now, without affecting the next scheduled run"
          >
            <Play className="h-3.5 w-3.5" />
            Run now
          </SmallButton>
          <SmallButton onClick={() => toggle.mutate()} disabled={toggle.isPending}>
            {schedule.enabled ? "Pause" : "Resume"}
          </SmallButton>
          <SmallButton
            onClick={() => showCrontab.mutate()}
            title="Get a line for system cron instead"
          >
            crontab
          </SmallButton>
          <SmallButton
            onClick={() => {
              if (
                confirm(
                  `Delete the schedule "${schedule.name}"? Backups it already made are kept.`,
                )
              ) {
                remove.mutate();
              }
            }}
            danger
          >
            <Trash2 className="h-3.5 w-3.5" />
          </SmallButton>
        </div>
      </div>

      {runNow.isError && (
        <p className="text-xs text-red-400">{(runNow.error as Error).message}</p>
      )}

      {crontab && (
        <div className="space-y-2 border-t border-slate-800 pt-3">
          <div className="flex items-center gap-2">
            <code className="min-w-0 flex-1 overflow-x-auto whitespace-pre rounded bg-slate-950 px-2 py-1.5 font-mono text-[11px] text-slate-300">
              {crontab}
            </code>
            <SmallButton
              onClick={() => {
                void navigator.clipboard.writeText(crontab);
                setCopied(true);
                setTimeout(() => setCopied(false), 1500);
              }}
            >
              {copied ? (
                <Check className="h-3.5 w-3.5" />
              ) : (
                <Copy className="h-3.5 w-3.5" />
              )}
            </SmallButton>
            <SmallButton onClick={() => setCrontab(null)}>
              <X className="h-3.5 w-3.5" />
            </SmallButton>
          </div>
          <p className="text-[11px] leading-relaxed text-slate-500">
            Needs <code className="text-slate-400">dbsync</code> on your PATH.
            Pause this schedule first, or both this app and cron will run it.
            Cron reads the expression in local time, and can only reach the
            keychain while your login session is unlocked.
          </p>
        </div>
      )}
    </div>
  );
}

/**
 * The create form.
 *
 * Editing an existing schedule is deliberately limited to pause/resume and
 * delete: everything else about a schedule comes from its plan, and changing a
 * plan is a first-class operation on the Sync page rather than something buried
 * in a second editor that could drift from it.
 */
/**
 * Exhaustive on purpose. The two-way ternary this replaced rendered a pipeline
 * schedule as "Scheduled run" — a third kind has to be a compile error, not a
 * wrong label on the row that tells somebody what runs at 04:00.
 */
const SCHEDULE_KIND_LABELS: Record<ScheduleKind, string> = {
  sync: "Scheduled run",
  drill: "Drill",
  pipeline: "Pipeline",
};

function ScheduleForm({ onClose }: { onClose: () => void }) {
  const queryClient = useQueryClient();

  const [kind, setKind] = useState<ScheduleKind>("sync");
  const [name, setName] = useState("");
  const [sourceId, setSourceId] = useState("");
  const [planId, setPlanId] = useState("");
  const [destId, setDestId] = useState("");
  const [cron, setCron] = useState(DEFAULT_SYNC_CRON);
  const [timezone, setTimezone] = useState<ScheduleTimezone>("local");
  const [verify, setVerify] = useState(true);
  const [catchUp, setCatchUp] = useState(true);
  const [keepLast, setKeepLast] = useState<number | null>(7);
  const [notify, setNotify] = useState<NotifyPolicy>("on_failure");
  const [webhook, setWebhook] = useState("");
  const [prefix, setPrefix] = useState("scheduled");
  const [countRows, setCountRows] = useState(false);
  // Drill-only.
  const [drillProfileId, setDrillProfileId] = useState("");
  const [drillDeep, setDrillDeep] = useState(false);
  const [drillKeepOnFailure, setDrillKeepOnFailure] = useState(false);
  // Pipeline-only.
  const [pipelineId, setPipelineId] = useState("");

  const isDrill = kind === "drill";
  const isPipeline = kind === "pipeline";

  const profiles = useQuery({ queryKey: ["profiles"], queryFn: api.listProfiles });
  const backupDir = useQuery({
    queryKey: ["backup-dir"],
    queryFn: api.backupDirectory,
  });
  const pipelines = useQuery({
    queryKey: ["pipelines"],
    queryFn: api.listPipelines,
  });
  const chosenPipeline = pipelines.data?.find((p) => p.id === pipelineId) ?? null;
  // Refused by the engine, so saying it here is about the fix being one click
  // away rather than about enforcement.
  const needsArming =
    chosenPipeline != null &&
    destructiveTargets(chosenPipeline.steps).length > 0 &&
    !isArmed(chosenPipeline);

  const plans = useQuery({
    queryKey: ["sync-plans", sourceId],
    queryFn: () => api.listSyncPlans(sourceId),
    enabled: sourceId !== "",
  });

  // Every "when does this actually run" answer comes from the engine, so the
  // preview cannot disagree with the scheduler.
  const preview = useQuery({
    queryKey: ["cron-preview", cron, timezone],
    queryFn: () => api.previewCron(cron, timezone),
    enabled: cron.trim() !== "",
    retry: false,
  });

  const source = profiles.data?.find((p) => p.id === sourceId);
  const drillProfile = profiles.data?.find((p) => p.id === drillProfileId);
  const dest = profiles.data?.find((p) => p.id === destId);
  const plan = plans.data?.find((p: SyncPlan) => p.id === planId);

  const engineMismatch =
    source && dest && source.engine !== dest.engine ? { source, dest } : null;

  const engineBackupOptions = (): EngineBackupOptions =>
    defaultBackupOptions(source?.engine ?? "mysql");

  /// A drill never dumps anything; these only satisfy the shared action shape,
  /// and the engine derives its restore options from the same engine tag.
  const drillBackupOptions = (): EngineBackupOptions =>
    defaultBackupOptions(drillProfile?.engine ?? "mysql");

  const engineRestoreOptions = (): EngineRestoreOptions =>
    defaultRestoreOptions(source?.engine ?? "mysql");

  const create = useMutation({
    mutationFn: () => {
      const input: ScheduleCreate = isDrill
        ? {
            kind: "drill",
            name,
            // A drill has no plan: it restores whatever artifact is newest,
            // and the artifact already fixes what it contains.
            plan_id: null,
            dest_profile_id: drillProfileId,
            cron,
            timezone,
            action: {
              output_dir: backupDir.data ?? "",
              // Backup-shaped fields a drill does not use. The engine's
              // validate() is what keeps the combination honest.
              compress: true,
              encrypt: false,
              backup: drillBackupOptions(),
              // Refused by the engine for a drill: the scratch database name
              // is generated there, and nothing else is droppable.
              restore: null,
              verify: true,
              deep_verify: drillDeep,
              retention: null,
              // A drill dumps nothing, so there is nothing to count.
              record_row_counts: false,
              keep_on_failure: drillKeepOnFailure,
            },
            webhook_url: webhook.trim() === "" ? null : webhook.trim(),
            notify,
            catch_up: false,
            enabled: true,
          }
        : isPipeline
        ? {
            kind: "pipeline",
            name,
            // The steps carry their own connections and table selections, so
            // the engine refuses a schedule that also names them here.
            plan_id: null,
            dest_profile_id: null,
            pipeline_id: pipelineId,
            cron,
            timezone,
            action: {
              output_dir: backupDir.data ?? "",
              compress: true,
              encrypt: false,
              backup: engineBackupOptions(),
              restore: null,
              verify: false,
              retention: null,
              record_row_counts: false,
              keep_on_failure: false,
            },
            webhook_url: webhook.trim() === "" ? null : webhook.trim(),
            notify,
            catch_up: catchUp,
            enabled: true,
          }
        : {
        kind: "sync",
        name,
        plan_id: planId,
        dest_profile_id: destId || null,
        cron,
        timezone,
        action: {
          output_dir: backupDir.data ?? "",
          compress: true,
          encrypt: false,
          backup: engineBackupOptions(),
          // A scheduled restore is always into a fresh timestamped database.
          // Nobody is present at 03:00 to confirm dropping one, and the engine
          // refuses a destructive target on a schedule for exactly that reason.
          restore: destId
            ? {
                naming: { strategy: "new_timestamped", prefix },
                options: engineRestoreOptions(),
              }
            : null,
          verify: destId ? verify : false,
          retention: keepLast ? { keep_last: keepLast, max_age_days: null } : null,
          record_row_counts: countRows,
          keep_on_failure: false,
        },
        webhook_url: webhook.trim() === "" ? null : webhook.trim(),
        notify,
        catch_up: catchUp,
        enabled: true,
      };
      return api.createSchedule(input);
    },
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ["schedules"] });
      onClose();
    },
  });

  const cronValid = preview.isSuccess;
  const ready = isPipeline
    ? name.trim() !== "" && pipelineId !== "" && cronValid && !needsArming
    : isDrill
    ? name.trim() !== "" && drillProfileId !== "" && cronValid
    : name.trim() !== "" &&
      planId !== "" &&
      cronValid &&
      !engineMismatch &&
      (destId === "" || destId !== sourceId);

  const nextRuns = useMemo(
    () => preview.data?.next_runs.slice(0, 5) ?? [],
    [preview.data],
  );

  return (
    <section className="panel space-y-4 p-4">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold text-slate-200">New schedule</h2>
        <SmallButton onClick={onClose}>
          <X className="h-3.5 w-3.5" />
        </SmallButton>
      </div>

      <div className="grid gap-3 sm:grid-cols-2">
        <label className="block">
          <span className="field-label">What it does</span>
          <select
            className="field-input"
            value={kind}
            onChange={(e) => {
              const next = e.target.value as ScheduleKind;
              setKind(next);
              // A drill checks the newest artifact, so running it at the same
              // time as the backup that produces one means checking last
              // night's while tonight's is still being written. Nudged later
              // by default; the field is still free.
              if (next === "drill" && cron === DEFAULT_SYNC_CRON) {
                setCron(DEFAULT_DRILL_CRON);
              } else if (next === "sync" && cron === DEFAULT_DRILL_CRON) {
                setCron(DEFAULT_SYNC_CRON);
              }
            }}
          >
            <option value="sync">Back up — and optionally restore elsewhere</option>
            <option value="drill">Drill — prove the newest backup restores</option>
            <option value="pipeline">Pipeline — run a saved chain of actions</option>
          </select>
        </label>

        {isPipeline && (
          <label className="block">
            <span className="field-label">Pipeline</span>
            <select
              className="field-input"
              value={pipelineId}
              onChange={(e) => setPipelineId(e.target.value)}
            >
              <option value="">Choose a pipeline…</option>
              {pipelines.data?.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
            {needsArming && chosenPipeline && (
              // The engine refuses this outright. Saying it here is so the
              // fix — authorising it on the Pipelines page — is one click
              // away rather than discovered on Save.
              <span className="mt-1 block text-xs leading-relaxed text-amber-300/90">
                {chosenPipeline.name} replaces{" "}
                {destructiveTargets(chosenPipeline.steps).join(", ")}, and
                nobody is present at the scheduled time to confirm that.
                Authorise it for unattended runs on the Pipelines page first.
              </span>
            )}
          </label>
        )}

        <label className="block">
          <span className="field-label">Name</span>
          <input
            className="field-input"
            value={name}
            placeholder={isDrill ? "Nightly drill" : "Nightly staging refresh"}
            onChange={(e) => setName(e.target.value)}
          />
        </label>
      </div>

      {isDrill && <DrillExplainer />}

      {isDrill ? (
        <>
          <ProfilePicker
            label="Restore into"
            value={drillProfileId}
            profiles={profiles.data ?? []}
            onChange={setDrillProfileId}
          />

          {drillProfile?.environment === "prod" && (
            <Warning>
              {drillProfile.name} is tagged <strong>production</strong>. A drill
              only ever creates its own scratch database and drops it again —
              but it does run a full restore against this server every night,
              which is load you may not want there.
            </Warning>
          )}

          <div className="space-y-2">
            <Checkbox checked={drillDeep} onChange={setDrillDeep}>
              Read every row, not just count them — catches corruption a
              <code className="mx-1 text-slate-400">COUNT(*)</code> never
              touches, at the cost of a full scan.
            </Checkbox>
            <Checkbox
              checked={drillKeepOnFailure}
              onChange={setDrillKeepOnFailure}
            >
              Leave the scratch database behind when the drill fails, so the
              wreckage can be inspected. A passing drill always cleans up.
            </Checkbox>
          </div>
        </>
      ) : (
      <>
      <div className="grid grid-cols-2 gap-4">
        <ProfilePicker
          label="Source"
          value={sourceId}
          profiles={profiles.data ?? []}
          onChange={(id) => {
            setSourceId(id);
            setPlanId("");
          }}
        />
        <label>
          <span className="field-label">Plan</span>
          <select
            className="field-input"
            value={planId}
            disabled={!sourceId || plans.isLoading}
            onChange={(e) => setPlanId(e.target.value)}
          >
            <option value="">
              {!sourceId
                ? "Pick a source first…"
                : plans.isLoading
                  ? "Loading…"
                  : "Select a plan…"}
            </option>
            {plans.data?.map((p: SyncPlan) => (
              <option key={p.id} value={p.id}>
                {p.name} ({p.database}, {p.selections.length} tables)
              </option>
            ))}
          </select>
        </label>
      </div>

      {sourceId && plans.data?.length === 0 && (
        <Warning>
          {source?.name} has no table sets. Build one on the Table sets page — a
          schedule needs a named table selection so it keeps backing up the same
          thing every night.
        </Warning>
      )}

      <div className="grid grid-cols-2 gap-4">
        <ProfilePicker
          label="Destination (optional — leave empty to back up only)"
          value={destId}
          profiles={profiles.data ?? []}
          exclude={sourceId}
          allowNone
          onChange={setDestId}
        />
        {destId && (
          <label>
            <span className="field-label">Target database prefix</span>
            <input
              className="field-input"
              value={prefix}
              onChange={(e) => setPrefix(e.target.value)}
            />
          </label>
        )}
      </div>

      {engineMismatch && (
        <Warning>
          {engineMismatch.source.name} is {engineMismatch.source.engine} and{" "}
          {engineMismatch.dest.name} is {engineMismatch.dest.engine}. Copying
          between engines is a migration, not a sync.
        </Warning>
      )}

      {dest?.environment === "prod" && (
        <Warning>
          The destination is tagged <strong>production</strong>. Scheduled runs
          always create a new timestamped database and never drop an existing
          one, but check you meant this server.
        </Warning>
      )}
      </>
      )}

      {/* ── When ─────────────────────────────────────────────────────── */}

      <div className="space-y-2 border-t border-slate-800 pt-4">
        <div className="flex flex-wrap items-end gap-3">
          <label className="w-52">
            <span className="field-label">Cron expression</span>
            <input
              className={cn(
                "field-input font-mono",
                cron.trim() !== "" && preview.isError && "border-red-500/60",
              )}
              value={cron}
              onChange={(e) => setCron(e.target.value)}
            />
          </label>

          <label className="w-32">
            <span className="field-label">Clock</span>
            <select
              className="field-input"
              value={timezone}
              onChange={(e) => setTimezone(e.target.value as ScheduleTimezone)}
            >
              <option value="local">Local time</option>
              <option value="utc">UTC</option>
            </select>
          </label>

          <div className="flex flex-wrap gap-1.5 pb-2">
            {PRESETS.map((p) => (
              <button
                key={p.cron}
                onClick={() => setCron(p.cron)}
                className="rounded-md border border-slate-700 px-2 py-1 text-[11px] text-slate-300 transition hover:bg-slate-800"
              >
                {p.label}
              </button>
            ))}
          </div>
        </div>

        {preview.isError && cron.trim() !== "" && (
          <p className="text-xs text-red-400">
            {(preview.error as Error).message}
          </p>
        )}

        {preview.data && (
          <div className="rounded-md border border-slate-800 bg-slate-950 p-3">
            <p className="text-xs text-slate-300">{preview.data.description}</p>
            <p className="mt-1.5 text-[11px] uppercase tracking-wide text-slate-500">
              Next runs
            </p>
            <ul className="mt-1 space-y-0.5">
              {nextRuns.map((t) => (
                <li key={t} className="font-mono text-[11px] text-slate-400">
                  {new Date(t).toLocaleString()}
                </li>
              ))}
            </ul>
            {timezone === "local" && (
              <p className="mt-2 text-[11px] leading-relaxed text-slate-500">
                Local time follows daylight saving: a time inside the
                spring-forward hour does not exist that day and will not run.
                Choose UTC if the run must happen every 24 hours exactly.
              </p>
            )}
          </div>
        )}
      </div>

      {/* ── Afterwards ───────────────────────────────────────────────── */}

      <div className="flex flex-wrap items-center gap-x-6 gap-y-3 border-t border-slate-800 pt-4">
        {destId && (
          <Checkbox checked={verify} onChange={setVerify}>
            Verify row counts after each run
          </Checkbox>
        )}

        {/* Both are about producing and keeping artifacts. A drill produces
            none — it reads the newest one — so showing either would offer a
            control that does nothing, which is the quiet kind of wrong this
            project keeps removing. A missed drill is also not worth making up
            at 09:00: the next night's run answers the same question. */}
        {!isDrill && (
          <>
            <Checkbox checked={catchUp} onChange={setCatchUp}>
              Catch up a run missed while the machine was asleep
            </Checkbox>

            <Checkbox checked={countRows} onChange={setCountRows}>
              Record row counts, so a drill can compare exact numbers
            </Checkbox>

            <label className="flex items-center gap-2 text-xs text-slate-300">
              <input
                type="checkbox"
                checked={keepLast !== null}
                onChange={(e) => setKeepLast(e.target.checked ? 7 : null)}
                className="h-4 w-4 rounded border-slate-600 bg-slate-900"
              />
              Keep only the newest
              <input
                type="number"
                min={1}
                disabled={keepLast === null}
                value={keepLast ?? 7}
                onChange={(e) => setKeepLast(Number(e.target.value))}
                className="w-14 rounded border border-slate-700 bg-slate-950 px-1.5 py-1 text-xs disabled:opacity-40"
              />
              backups
            </label>
          </>
        )}
      </div>

      <div className="flex flex-wrap items-end gap-4">
        <label className="w-44">
          <span className="field-label">Notify me</span>
          <select
            className="field-input"
            value={notify}
            onChange={(e) => setNotify(e.target.value as NotifyPolicy)}
          >
            <option value="on_failure">Only on failure</option>
            <option value="always">Every run</option>
            <option value="never">Never</option>
          </select>
        </label>

        <label className="min-w-0 flex-1">
          <span className="field-label">Webhook (optional)</span>
          <input
            className="field-input font-mono text-xs"
            value={webhook}
            placeholder="https://hooks.example.com/…"
            onChange={(e) => setWebhook(e.target.value)}
          />
        </label>
      </div>

      {webhook.trim() !== "" && (
        <p className="text-[11px] leading-relaxed text-slate-500">
          Each run POSTs a JSON summary here: schedule name, outcome, duration,
          row-count verification and the artifact's file name. It never includes
          hostnames, usernames, passwords or file paths.
        </p>
      )}

      {plan && (
        <div className="rounded-md border border-slate-800 bg-slate-950 p-3 text-xs text-slate-400">
          Backs up <span className="font-mono text-slate-300">{plan.database}</span>{" "}
          — {plan.selections.filter((s) => s.mode === "schema_and_data").length}{" "}
          table(s) with data, {plan.selections.length} in the plan.
          {destId ? " Restored into a new timestamped database." : ""}
        </div>
      )}

      <div className="flex items-center gap-3 border-t border-slate-800 pt-3">
        <button
          onClick={() => create.mutate()}
          disabled={!ready || create.isPending}
          className="rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
        >
          {create.isPending ? "Saving…" : "Create schedule"}
        </button>
        <button
          onClick={onClose}
          className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-300 transition hover:bg-slate-800"
        >
          Cancel
        </button>
        {create.isError && (
          <span className="text-xs text-red-400">
            {(create.error as Error).message}
          </span>
        )}
      </div>
    </section>
  );
}

function Outcome({ value }: { value: string | null }) {
  if (!value) return null;
  const styles: Record<string, string> = {
    success: "bg-emerald-500/15 text-emerald-300",
    failed: "bg-red-500/15 text-red-300",
    cancelled: "bg-slate-700 text-slate-300",
  };
  return (
    <span
      className={cn(
        "rounded px-1.5 py-0.5 text-[10px] uppercase",
        styles[value] ?? "bg-slate-700 text-slate-300",
      )}
    >
      {value}
    </span>
  );
}

function SmallButton({
  children,
  onClick,
  disabled,
  danger,
  title,
}: {
  children: React.ReactNode;
  onClick: () => void;
  disabled?: boolean;
  danger?: boolean;
  title?: string;
}) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      title={title}
      className={cn(
        "flex items-center gap-1 rounded-md border px-2 py-1 text-xs transition disabled:opacity-40",
        danger
          ? "border-red-900/60 text-red-300 hover:bg-red-950/40"
          : "border-slate-700 text-slate-300 hover:bg-slate-800",
      )}
    >
      {children}
    </button>
  );
}

function Checkbox({
  checked,
  onChange,
  children,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  children: React.ReactNode;
}) {
  return (
    <label className="flex items-center gap-2 text-xs text-slate-300">
      <input
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
        className="h-4 w-4 rounded border-slate-600 bg-slate-900"
      />
      {children}
    </label>
  );
}

function ProfilePicker({
  label,
  value,
  profiles,
  exclude,
  allowNone,
  onChange,
}: {
  label: string;
  value: string;
  profiles: ConnectionProfile[];
  exclude?: string;
  allowNone?: boolean;
  onChange: (id: string) => void;
}) {
  return (
    <label>
      <span className="field-label">{label}</span>
      <select
        className="field-input"
        value={value}
        onChange={(e) => onChange(e.target.value)}
      >
        <option value="">{allowNone ? "None — back up only" : "Select…"}</option>
        {profiles
          .filter((p) => !exclude || p.id !== exclude)
          .map((p) => (
            <option key={p.id} value={p.id}>
              {p.name} ({p.engine}, {p.environment})
            </option>
          ))}
      </select>
    </label>
  );
}

/**
 * What a drill is, said where somebody is deciding whether to create one.
 *
 * The value is entirely in it being automatic, and the reason it needs
 * explaining is that "restore into a scratch database every night" sounds
 * alarming until you know it cannot touch anything else.
 */
function DrillExplainer() {
  return (
    <div className="flex gap-3 rounded-lg border border-slate-700 bg-slate-800/40 p-4">
      <ShieldCheck className="mt-0.5 h-4 w-4 shrink-0 text-emerald-400" />
      <div className="space-y-2 text-xs leading-relaxed text-slate-300">
        <p>
          A backup is a belief until it has been restored. A checksum proves the
          bytes are the bytes that were written; it says nothing about whether
          the dump was coherent or whether a server will accept it.
        </p>
        <p>
          Each run takes the <strong>newest</strong> artifact in the backup
          folder, restores it into a scratch database, checks it against its own
          manifest, and drops it again. The scratch name is generated by the
          engine, and a drill refuses to drop anything that does not match the
          name it generated — so it cannot reach an existing database.
        </p>
        <p className="text-slate-500">
          It checks the artifact against its manifest, not against the live
          source. The source has moved on since the backup was taken, so
          comparing to it would report normal drift as corruption.
        </p>
      </div>
    </div>
  );
}

function Warning({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex items-start gap-2 rounded-md border border-amber-500/40 bg-amber-500/5 p-3">
      <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-amber-400" />
      <p className="text-xs leading-relaxed text-amber-200/90">{children}</p>
    </div>
  );
}
