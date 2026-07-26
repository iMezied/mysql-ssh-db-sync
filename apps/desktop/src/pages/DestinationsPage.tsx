import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle,
  Check,
  CloudUpload,
  KeyRound,
  Plus,
  Trash2,
  X,
} from "lucide-react";

import PageHeader from "@/components/PageHeader";
import { api } from "@/lib/api";
import type { DestinationCheck, DestinationView } from "@/bindings";

/**
 * Off-site destinations.
 *
 * The page has two jobs beyond editing rows. The first is making the
 * unusable-but-configured state impossible to miss: a destination with no
 * stored credential looks exactly like a working one everywhere else, and the
 * whole point of having one is the belief that a second copy exists. The
 * second is saying plainly that a failed upload fails the backup, so nobody is
 * surprised by a red job the first time a bucket is unreachable.
 */

/** Presets, so nobody has to look up an endpoint format. */
const PRESETS: {
  label: string;
  endpoint: string;
  region: string;
  pathStyle: boolean;
  hint?: string;
}[] = [
  {
    label: "Amazon S3",
    endpoint: "https://s3.REGION.amazonaws.com",
    region: "eu-west-1",
    pathStyle: false,
    hint: "Replace REGION in the endpoint with the same region below.",
  },
  {
    label: "Cloudflare R2",
    endpoint: "https://ACCOUNT_ID.r2.cloudflarestorage.com",
    region: "auto",
    pathStyle: false,
    hint: "Replace ACCOUNT_ID with your Cloudflare account id.",
  },
  {
    label: "Backblaze B2",
    endpoint: "https://s3.us-west-002.backblazeb2.com",
    region: "us-west-002",
    pathStyle: false,
  },
  {
    label: "MinIO / self-hosted",
    endpoint: "http://127.0.0.1:9000",
    region: "us-east-1",
    pathStyle: true,
    hint: "Plaintext http:// is only accepted for an address on this machine.",
  },
];

export default function DestinationsPage() {
  const queryClient = useQueryClient();
  const destinations = useQuery({
    queryKey: ["destinations"],
    queryFn: api.listDestinations,
  });

  const [adding, setAdding] = useState(false);
  const invalidate = () =>
    void queryClient.invalidateQueries({ queryKey: ["destinations"] });

  return (
    <>
      <PageHeader
        title="Off-site"
        description="Send a copy of every backup somewhere that is not this machine."
      />

      <div className="space-y-6 p-6">
        <Explainer />

        {destinations.isError && (
          // Without this the list reads "none configured", which is
          // indistinguishable from "your backups are not being copied
          // anywhere" — and one of those is a much bigger problem.
          <p className="text-xs text-red-400">
            Could not load destinations:{" "}
            {(destinations.error as Error).message}
          </p>
        )}

        <section className="space-y-3">
          <div className="flex items-center justify-between">
            <h2 className="text-sm font-medium text-slate-200">Destinations</h2>
            <button
              type="button"
              className="inline-flex items-center gap-1.5 rounded-md bg-blue-600 px-3 py-1.5 text-sm font-medium text-white transition hover:bg-blue-500"
              onClick={() => setAdding((v) => !v)}
            >
              <Plus className="h-4 w-4" />
              Add
            </button>
          </div>

          {adding && (
            <AddDestination
              onDone={() => {
                setAdding(false);
                invalidate();
              }}
              onCancel={() => setAdding(false)}
            />
          )}

          <div className="panel divide-y divide-slate-800">
            {destinations.isPending && (
              <p className="px-4 py-6 text-sm text-slate-500">Loading…</p>
            )}

            {destinations.data?.length === 0 && (
              <p className="px-4 py-6 text-sm text-slate-500">
                No destinations. Backups are being kept only on this machine —
                one disk failure away from not existing.
              </p>
            )}

            {destinations.data?.map((d) => (
              <Row key={d.id} destination={d} onChanged={invalidate} />
            ))}
          </div>
        </section>
      </div>
    </>
  );
}

function Explainer() {
  return (
    <div className="flex gap-3 rounded-lg border border-slate-700 bg-slate-800/40 p-4">
      <CloudUpload className="mt-0.5 h-4 w-4 shrink-0 text-blue-400" />
      <div className="space-y-2 text-xs leading-relaxed text-slate-300">
        <p>
          Every backup is uploaded to each enabled destination as soon as it is
          written, with its manifest, and read back to confirm it arrived at the
          size that was sent.
        </p>
        <p>
          <strong className="font-semibold text-slate-200">
            A failed upload fails the job.
          </strong>{" "}
          The local artifact is still there and the log says where — but a
          backup that never reached the destination it was configured to reach
          has not done what it said, so it is not recorded as a success.
          Retention is skipped too: the older local copies are the only ones
          left.
        </p>
        <p className="text-slate-500">
          Anything speaking the S3 API works. The secret access key is stored in
          your OS keychain, never in the app database, and no screen in this app
          can show it back to you.
        </p>
      </div>
    </div>
  );
}

function Row({
  destination,
  onChanged,
}: {
  destination: DestinationView;
  onChanged: () => void;
}) {
  const [check, setCheck] = useState<DestinationCheck | null>(null);
  const [rekeying, setRekeying] = useState(false);

  const test = useMutation({
    mutationFn: () => api.testDestination(destination.id),
    onSuccess: setCheck,
  });

  const toggle = useMutation({
    mutationFn: (enabled: boolean) =>
      api.updateDestination(destination.id, {
        name: null,
        kind: null,
        enabled,
        retention: null,
      }),
    onSuccess: onChanged,
  });

  const remove = useMutation({
    mutationFn: () => api.deleteDestination(destination.id),
    onSuccess: onChanged,
  });

  const retention = describeRetention(destination.retention);

  return (
    <div className="space-y-3 px-4 py-3">
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-sm font-medium text-slate-200">
              {destination.name}
            </span>
            <span className="font-mono text-xs text-slate-500">
              {destination.location}
            </span>
            {!destination.enabled && (
              <span className="rounded bg-slate-700 px-1.5 py-0.5 text-[11px] text-slate-300">
                disabled
              </span>
            )}
            {!destination.has_credential && (
              // The state this page exists to surface. Everything else about
              // the row looks identical to a destination that works.
              <span className="rounded bg-red-500/15 px-1.5 py-0.5 text-[11px] text-red-300">
                no key — uploads will fail
              </span>
            )}
          </div>
          <p className="mt-0.5 text-xs text-slate-500">{retention}</p>
        </div>

        <div className="flex shrink-0 items-center gap-1">
          <button
            type="button"
            className="rounded px-2 py-1 text-xs text-slate-400 transition hover:bg-slate-800 hover:text-slate-200 disabled:opacity-50"
            disabled={test.isPending}
            onClick={() => test.mutate()}
          >
            {test.isPending ? "Checking…" : "Test"}
          </button>
          <button
            type="button"
            className="rounded px-2 py-1 text-xs text-slate-400 transition hover:bg-slate-800 hover:text-slate-200 disabled:opacity-50"
            disabled={toggle.isPending}
            onClick={() => toggle.mutate(!destination.enabled)}
          >
            {destination.enabled ? "Disable" : "Enable"}
          </button>
          <button
            type="button"
            className="rounded p-1.5 text-slate-500 transition hover:bg-slate-800 hover:text-slate-200"
            title="Replace the secret access key"
            onClick={() => setRekeying((v) => !v)}
          >
            <KeyRound className="h-4 w-4" />
          </button>
          <button
            type="button"
            className="rounded p-1.5 text-slate-500 transition hover:bg-slate-800 hover:text-red-400 disabled:opacity-50"
            title="Remove this destination and its stored key"
            disabled={remove.isPending}
            onClick={() => remove.mutate()}
          >
            <Trash2 className="h-4 w-4" />
          </button>
        </div>
      </div>

      {rekeying && (
        <SetCredential
          id={destination.id}
          onDone={() => {
            setRekeying(false);
            setCheck(null);
            onChanged();
          }}
        />
      )}

      {check && (
        <p
          className={`flex items-start gap-1.5 text-xs ${
            check.ok ? "text-emerald-400" : "text-red-400"
          }`}
        >
          {check.ok ? (
            <Check className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          ) : (
            <X className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          )}
          <span>
            {check.detail}
            {check.ok && (
              // Said explicitly, because "Test passed" invites the reading
              // that uploads are proven to work, and they are not.
              <span className="text-slate-500">
                {" "}
                — this checks reading, not writing. Only a real backup proves
                the key can upload.
              </span>
            )}
          </span>
        </p>
      )}

      {(test.isError || toggle.isError || remove.isError) && (
        <p className="text-xs text-red-400">
          {
            ((test.error ?? toggle.error ?? remove.error) as Error).message
          }
        </p>
      )}
    </div>
  );
}

function SetCredential({ id, onDone }: { id: string; onDone: () => void }) {
  const [secret, setSecret] = useState("");
  const save = useMutation({
    mutationFn: () => api.setDestinationCredential(id, secret),
    onSuccess: () => {
      setSecret("");
      onDone();
    },
  });

  return (
    <div className="space-y-2 rounded-md bg-slate-800/60 p-3">
      <label className="flex flex-col gap-1">
        <span className="field-label">New secret access key</span>
        <input
          type="password"
          className="field-input w-full max-w-md"
          value={secret}
          onChange={(e) => setSecret(e.target.value)}
          placeholder="wJalrXUtnFEMI/K7MDENG/…"
        />
      </label>
      <div className="flex items-center gap-2">
        <button
          type="button"
          className="rounded-md bg-blue-600 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
          disabled={!secret.trim() || save.isPending}
          onClick={() => save.mutate()}
        >
          Save key
        </button>
        <span className="text-xs text-slate-500">
          Goes straight to the keychain. It is never stored in the app database
          and cannot be read back here.
        </span>
      </div>
      {save.isError && (
        <p className="text-xs text-red-400">{(save.error as Error).message}</p>
      )}
    </div>
  );
}

function AddDestination({
  onDone,
  onCancel,
}: {
  onDone: () => void;
  onCancel: () => void;
}) {
  const [preset, setPreset] = useState(PRESETS[0]!);
  const [name, setName] = useState("");
  const [endpoint, setEndpoint] = useState(PRESETS[0]!.endpoint);
  const [region, setRegion] = useState(PRESETS[0]!.region);
  const [bucket, setBucket] = useState("");
  const [prefix, setPrefix] = useState("");
  const [pathStyle, setPathStyle] = useState(PRESETS[0]!.pathStyle);
  const [accessKeyId, setAccessKeyId] = useState("");
  const [secret, setSecret] = useState("");
  const [keepLast, setKeepLast] = useState("");
  const [maxAgeDays, setMaxAgeDays] = useState("");

  const applyPreset = (label: string) => {
    const next = PRESETS.find((p) => p.label === label);
    if (!next) return;
    setPreset(next);
    setEndpoint(next.endpoint);
    setRegion(next.region);
    setPathStyle(next.pathStyle);
  };

  const create = useMutation({
    mutationFn: () =>
      api.createDestination(
        {
          name: name.trim(),
          kind: {
            kind: "s3",
            endpoint: endpoint.trim(),
            region: region.trim(),
            bucket: bucket.trim(),
            prefix: prefix.trim(),
            path_style: pathStyle,
            access_key_id: accessKeyId.trim(),
          },
          enabled: true,
          retention: {
            keep_last: keepLast ? Number(keepLast) : null,
            max_age_days: maxAgeDays ? Number(maxAgeDays) : null,
          },
        },
        secret,
      ),
    onSuccess: onDone,
  });

  const ready =
    name.trim() && endpoint.trim() && bucket.trim() && accessKeyId.trim() && secret;

  return (
    <div className="panel space-y-4 p-4">
      <div className="flex flex-wrap gap-3">
        <label className="flex flex-col gap-1">
          <span className="field-label">Provider</span>
          <select
            className="field-input w-52"
            value={preset.label}
            onChange={(e) => applyPreset(e.target.value)}
          >
            {PRESETS.map((p) => (
              <option key={p.label} value={p.label}>
                {p.label}
              </option>
            ))}
          </select>
        </label>

        <label className="flex flex-col gap-1">
          <span className="field-label">Name</span>
          <input
            className="field-input w-52"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="off-site"
          />
        </label>
      </div>

      {preset.hint && <p className="text-xs text-slate-500">{preset.hint}</p>}

      <div className="flex flex-wrap gap-3">
        <label className="flex flex-col gap-1">
          <span className="field-label">Endpoint</span>
          <input
            className="field-input w-96"
            value={endpoint}
            onChange={(e) => setEndpoint(e.target.value)}
          />
        </label>

        <label className="flex flex-col gap-1">
          <span className="field-label">Region</span>
          <input
            className="field-input w-40"
            value={region}
            onChange={(e) => setRegion(e.target.value)}
          />
        </label>
      </div>

      <div className="flex flex-wrap gap-3">
        <label className="flex flex-col gap-1">
          <span className="field-label">Bucket</span>
          <input
            className="field-input w-52"
            value={bucket}
            onChange={(e) => setBucket(e.target.value)}
            placeholder="acme-backups"
          />
        </label>

        <label className="flex flex-col gap-1">
          <span className="field-label">Prefix (optional)</span>
          <input
            className="field-input w-52"
            value={prefix}
            onChange={(e) => setPrefix(e.target.value)}
            placeholder="prod"
          />
        </label>

        <label className="flex items-center gap-2 self-end pb-2">
          <input
            type="checkbox"
            checked={pathStyle}
            onChange={(e) => setPathStyle(e.target.checked)}
          />
          <span className="text-xs text-slate-400">Path-style addressing</span>
        </label>
      </div>

      <div className="flex flex-wrap gap-3">
        <label className="flex flex-col gap-1">
          <span className="field-label">Access key id</span>
          <input
            className="field-input w-64"
            value={accessKeyId}
            onChange={(e) => setAccessKeyId(e.target.value)}
            placeholder="AKIA…"
          />
        </label>

        <label className="flex flex-col gap-1">
          <span className="field-label">Secret access key</span>
          <input
            type="password"
            className="field-input w-64"
            value={secret}
            onChange={(e) => setSecret(e.target.value)}
          />
        </label>
      </div>

      <div className="flex flex-wrap items-end gap-3">
        <label className="flex flex-col gap-1">
          <span className="field-label">Keep newest (optional)</span>
          <input
            className="field-input w-40"
            inputMode="numeric"
            value={keepLast}
            onChange={(e) => setKeepLast(e.target.value.replace(/[^0-9]/g, ""))}
            placeholder="30"
          />
        </label>

        <label className="flex flex-col gap-1">
          <span className="field-label">Max age in days (optional)</span>
          <input
            className="field-input w-40"
            inputMode="numeric"
            value={maxAgeDays}
            onChange={(e) =>
              setMaxAgeDays(e.target.value.replace(/[^0-9]/g, ""))
            }
            placeholder="90"
          />
        </label>

        <p className="max-w-sm pb-2 text-xs text-slate-500">
          Applies to this bucket only, separately from local retention. The
          newest backup is never deleted, whatever these say.
        </p>
      </div>

      <div className="flex items-center gap-2">
        <button
          type="button"
          className="rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white transition hover:bg-blue-500 disabled:opacity-50"
          disabled={!ready || create.isPending}
          onClick={() => create.mutate()}
        >
          {create.isPending ? "Adding…" : "Add destination"}
        </button>
        <button
          type="button"
          className="rounded-md px-3 py-2 text-sm text-slate-400 transition hover:text-slate-200"
          onClick={onCancel}
        >
          Cancel
        </button>
      </div>

      {create.isError && (
        <p className="flex items-start gap-1.5 text-xs text-red-400">
          <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          {(create.error as Error).message}
        </p>
      )}
    </div>
  );
}

function describeRetention(policy: {
  keep_last: number | null;
  max_age_days: number | null;
}): string {
  const { keep_last: keep, max_age_days: days } = policy;
  if (keep && days) return `Keeps the newest ${keep}, and at most ${days} days.`;
  if (keep) return `Keeps the newest ${keep}.`;
  if (days) return `Keeps ${days} days.`;
  return "Keeps everything.";
}
