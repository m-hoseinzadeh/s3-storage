import { useCallback, useEffect, useRef, useState } from "react";
import { DownloadCloud, Play, Eye, Ban, ChevronDown, Save, AlertTriangle } from "lucide-react";
import {
  api,
  ApiError,
  type SyncJob,
  type SyncPreview,
  type SyncRequest,
} from "../lib/api";
import { Badge, Button, Card, Field, Input, Spinner, useToast } from "../components/ui";
import { PageHeader, TutList } from "../components/PageHeader";
import { formatBytes, formatNumber } from "../lib/format";

type Mode = NonNullable<SyncRequest["mode"]>;

// Everything except the credentials. Kept in localStorage so an operator does
// not retype the endpoint and scope on every run; the secret key never goes in
// here, exactly as the server never persists it.
interface Profile {
  endpoint: string;
  region: string;
  accessKey: string;
  pathStyle: boolean;
  caPem: string;
  srcBucket: string;
  srcPrefix: string;
  dstBucket: string;
  dstPrefix: string;
  mode: Mode;
  concurrency: number;
  verifyEtag: boolean;
  createBucket: boolean;
}

const BLANK: Profile = {
  endpoint: "",
  region: "us-east-1",
  accessKey: "",
  pathStyle: true,
  caPem: "",
  srcBucket: "",
  srcPrefix: "",
  dstBucket: "",
  dstPrefix: "",
  mode: "new_and_changed",
  concurrency: 4,
  verifyEtag: false,
  createBucket: false,
};

const STORAGE_KEY = "s3-storage.sync.profile";

const MODES: { value: Mode; label: string; hint: string }[] = [
  {
    value: "new_and_changed",
    label: "New & changed",
    hint: "Copy what is missing, a different size, or newer on the source. The usual choice.",
  },
  { value: "skip_existing", label: "Skip existing", hint: "Never replace an object already here." },
  { value: "overwrite_all", label: "Overwrite all", hint: "Copy every listed object, no comparison." },
];

const TONE: Record<SyncJob["status"], "muted" | "accent" | "danger" | "info"> = {
  running: "info",
  completed: "accent",
  failed: "danger",
  cancelled: "muted",
};

function loadProfile(): Profile {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return raw ? { ...BLANK, ...(JSON.parse(raw) as Partial<Profile>) } : BLANK;
  } catch {
    return BLANK;
  }
}

export function Sync() {
  const [p, setP] = useState<Profile>(loadProfile);
  const [secretKey, setSecretKey] = useState("");
  const [advanced, setAdvanced] = useState(false);
  const [busy, setBusy] = useState(false);
  const [job, setJob] = useState<SyncJob | null>(null);
  const [preview, setPreview] = useState<SyncPreview | null>(null);
  const [history, setHistory] = useState<SyncJob[]>([]);
  const toast = useToast();

  const set = <K extends keyof Profile>(key: K, value: Profile[K]) => setP((prev) => ({ ...prev, [key]: value }));

  const request = useCallback(
    (): SyncRequest => ({
      endpoint: p.endpoint.trim(),
      region: p.region.trim() || undefined,
      access_key: p.accessKey.trim(),
      secret_key: secretKey,
      path_style: p.pathStyle,
      ca_pem: p.caPem.trim() || null,
      src_bucket: p.srcBucket.trim(),
      src_prefix: p.srcPrefix.trim() || undefined,
      dst_bucket: p.dstBucket.trim(),
      dst_prefix: p.dstPrefix.trim() || undefined,
      mode: p.mode,
      concurrency: p.concurrency,
      verify_etag: p.verifyEtag,
      create_bucket: p.createBucket,
    }),
    [p, secretKey],
  );

  const refreshHistory = useCallback(() => {
    api
      .syncHistory()
      .then((r) => setHistory(r.runs))
      .catch(() => {});
  }, []);

  // Poll while a run is live. The interval is cleared as soon as the run is
  // terminal, so an idle panel makes one request and then stops.
  const running = job?.status === "running";
  const timer = useRef<number | null>(null);
  useEffect(() => {
    const tick = () =>
      api
        .syncCurrent()
        .then((r) => setJob(r.job))
        .catch(() => {});
    tick();
    refreshHistory();
    if (!running) return;
    timer.current = window.setInterval(tick, 1000);
    return () => {
      if (timer.current !== null) window.clearInterval(timer.current);
    };
  }, [running, refreshHistory]);

  useEffect(() => {
    if (job && job.status !== "running") refreshHistory();
  }, [job?.status, job, refreshHistory]);

  const saveProfile = () => {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(p));
      toast("success", "Connection details saved in this browser (never the secret key)");
    } catch {
      toast("error", "Could not save to this browser's storage");
    }
  };

  const guard = (): boolean => {
    if (!p.endpoint.trim() || !p.accessKey.trim() || !secretKey) {
      toast("error", "Endpoint, access key and secret key are all required");
      return false;
    }
    if (!p.srcBucket.trim() || !p.dstBucket.trim()) {
      toast("error", "Both a source and a destination bucket are required");
      return false;
    }
    return true;
  };

  const runPreview = async () => {
    if (!guard()) return;
    setBusy(true);
    setPreview(null);
    try {
      setPreview(await api.syncPreview(request()));
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Preview failed");
    } finally {
      setBusy(false);
    }
  };

  const start = async () => {
    if (!guard()) return;
    setBusy(true);
    try {
      const r = await api.syncStart(request());
      setJob(r.job);
      setPreview(null);
      toast("success", "Sync started");
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Could not start the sync");
    } finally {
      setBusy(false);
    }
  };

  const cancel = async () => {
    if (!job) return;
    try {
      await api.syncCancel(job.id);
      toast("info", "Cancelling — objects already copied stay put");
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Could not cancel");
    }
  };

  return (
    <div>
      <PageHeader
        title="Sync from Remote"
        description="Pull objects out of a MinIO or S3-compatible bucket into this server. Re-running copies only what changed."
        tutorial={
          <TutList
            items={[
              "Enter the source endpoint and its credentials. They are used for this run only and are never stored on the server.",
              "Pick the source bucket (and optionally a prefix) and where it should land here.",
              "Press Preview to see exactly what would be copied and how many bytes it is, before committing.",
              "Runs are incremental: a second run over the same source copies only what is new or changed.",
              "A cancelled or interrupted run is safe to re-run — only whole objects are ever written, so it simply picks up the rest.",
              "For a source with a private or self-signed certificate, paste its CA certificate under Advanced.",
            ]}
          />
        }
        actions={
          <Button variant="secondary" onClick={saveProfile}>
            <Save className="h-4 w-4" /> Save connection
          </Button>
        }
      />

      <div className="grid gap-5 lg:grid-cols-2">
        <Card className="p-5">
          <h2 className="mb-4 flex items-center gap-2 font-semibold">
            <DownloadCloud className="h-4 w-4 text-[var(--color-accent)]" /> Source
          </h2>
          <div className="space-y-4">
            <Field label="Endpoint" hint="For example https://minio.internal:9000">
              <Input
                value={p.endpoint}
                onChange={(e) => set("endpoint", e.target.value)}
                placeholder="https://minio.internal:9000"
              />
            </Field>
            <div className="grid gap-4 sm:grid-cols-2">
              <Field label="Access key">
                <Input value={p.accessKey} onChange={(e) => set("accessKey", e.target.value)} autoComplete="off" />
              </Field>
              <Field label="Secret key" hint="Used for this run only; never saved.">
                <Input
                  type="password"
                  value={secretKey}
                  onChange={(e) => setSecretKey(e.target.value)}
                  autoComplete="new-password"
                />
              </Field>
            </div>
            <div className="grid gap-4 sm:grid-cols-2">
              <Field label="Region" hint="Only matters if the source sets MINIO_REGION.">
                <Input value={p.region} onChange={(e) => set("region", e.target.value)} />
              </Field>
              <Check
                label="Path-style addressing"
                hint="Leave on for MinIO."
                checked={p.pathStyle}
                onChange={(v) => set("pathStyle", v)}
              />
            </div>

            <button
              type="button"
              onClick={() => setAdvanced((v) => !v)}
              className="flex items-center gap-1.5 text-sm text-[var(--color-muted-fg)] hover:text-[var(--color-fg)]"
            >
              <ChevronDown className={`h-4 w-4 transition-transform ${advanced ? "rotate-180" : ""}`} />
              Advanced
            </button>
            {advanced && (
              <Field
                label="CA certificate (PEM)"
                hint="Trusted in addition to the system roots, for a private or self-signed source certificate."
              >
                <textarea
                  value={p.caPem}
                  onChange={(e) => set("caPem", e.target.value)}
                  rows={5}
                  placeholder="-----BEGIN CERTIFICATE-----"
                  className="focusable mono w-full rounded-[var(--radius)] border border-[var(--color-border-strong)] bg-[var(--color-bg)] px-3 py-2 text-sm text-[var(--color-fg)] placeholder:text-[var(--color-faint-fg)]"
                />
              </Field>
            )}
          </div>
        </Card>

        <Card className="p-5">
          <h2 className="mb-4 font-semibold">Scope &amp; options</h2>
          <div className="space-y-4">
            <div className="grid gap-4 sm:grid-cols-2">
              <Field label="Source bucket">
                <Input value={p.srcBucket} onChange={(e) => set("srcBucket", e.target.value)} />
              </Field>
              <Field label="Source prefix" hint="Optional, e.g. 2026/">
                <Input value={p.srcPrefix} onChange={(e) => set("srcPrefix", e.target.value)} />
              </Field>
              <Field label="Destination bucket">
                <Input value={p.dstBucket} onChange={(e) => set("dstBucket", e.target.value)} />
              </Field>
              <Field label="Destination prefix" hint="Optional.">
                <Input value={p.dstPrefix} onChange={(e) => set("dstPrefix", e.target.value)} />
              </Field>
            </div>

            <Field label="Mode">
              <div className="space-y-1.5">
                {MODES.map((m) => (
                  <label key={m.value} className="flex cursor-pointer items-start gap-2.5 text-sm">
                    <input
                      type="radio"
                      name="sync-mode"
                      className="mt-1"
                      checked={p.mode === m.value}
                      onChange={() => set("mode", m.value)}
                    />
                    <span>
                      <span className="font-medium">{m.label}</span>
                      <span className="block text-xs text-[var(--color-faint-fg)]">{m.hint}</span>
                    </span>
                  </label>
                ))}
              </div>
            </Field>

            <Field label={`Parallel copies: ${p.concurrency}`}>
              <input
                type="range"
                min={1}
                max={16}
                value={p.concurrency}
                onChange={(e) => set("concurrency", Number(e.target.value))}
                className="w-full"
              />
            </Field>

            <Check
              label="Create the destination bucket if it does not exist"
              checked={p.createBucket}
              onChange={(v) => set("createBucket", v)}
            />
            <Check
              label="Verify checksums"
              hint="Re-reads every local object to compare hashes, so a run costs far more. Objects uploaded to the source in multiple parts cannot be verified this way and fall back to size and time."
              checked={p.verifyEtag}
              onChange={(v) => set("verifyEtag", v)}
            />

            <div className="flex gap-2 pt-1">
              <Button variant="secondary" onClick={runPreview} loading={busy} disabled={running}>
                <Eye className="h-4 w-4" /> Preview
              </Button>
              <Button variant="primary" onClick={start} loading={busy} disabled={running}>
                <Play className="h-4 w-4" /> Start sync
              </Button>
            </div>
          </div>
        </Card>
      </div>

      {preview && <PreviewPanel preview={preview} />}
      {job && <JobPanel job={job} onCancel={cancel} />}
      {history.length > 0 && <HistoryPanel runs={history} activeId={job?.id} />}
    </div>
  );
}

function Check({
  label,
  hint,
  checked,
  onChange,
}: {
  label: string;
  hint?: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <label className="flex cursor-pointer items-start gap-2.5 text-sm">
      <input type="checkbox" className="mt-1" checked={checked} onChange={(e) => onChange(e.target.checked)} />
      <span>
        <span className="font-medium">{label}</span>
        {hint && <span className="block text-xs text-[var(--color-faint-fg)]">{hint}</span>}
      </span>
    </label>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="text-xs text-[var(--color-faint-fg)]">{label}</div>
      <div className="mono text-lg font-semibold">{value}</div>
    </div>
  );
}

function PreviewPanel({ preview }: { preview: SyncPreview }) {
  return (
    <Card className="mt-5 p-5">
      <h2 className="mb-4 font-semibold">Preview — nothing has been copied</h2>
      <div className="mb-4 grid grid-cols-2 gap-4 sm:grid-cols-4">
        <Stat label="Listed" value={formatNumber(preview.listed)} />
        <Stat label="Would copy" value={formatNumber(preview.to_copy)} />
        <Stat label="Would skip" value={formatNumber(preview.to_skip)} />
        <Stat label="Would transfer" value={formatBytes(preview.bytes_to_copy)} />
      </div>
      <ActionTable rows={preview.actions} truncated={preview.actions_truncated} />
    </Card>
  );
}

function ActionTable({ rows, truncated }: { rows: SyncPreview["actions"]; truncated: boolean }) {
  if (rows.length === 0) return <p className="text-sm text-[var(--color-muted-fg)]">Nothing to do.</p>;
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-sm">
        <thead className="text-left text-xs text-[var(--color-faint-fg)]">
          <tr>
            <th className="py-1.5 pr-3">Key</th>
            <th className="py-1.5 pr-3">Lands as</th>
            <th className="py-1.5 pr-3">Size</th>
            <th className="py-1.5">Action</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.key} className="border-t border-[var(--color-border)]">
              <td className="mono py-1.5 pr-3">{r.key}</td>
              <td className="mono py-1.5 pr-3 text-[var(--color-muted-fg)]">{r.dst_key}</td>
              <td className="py-1.5 pr-3">{formatBytes(r.size)}</td>
              <td className="py-1.5">
                <Badge tone={r.action === "copy" ? "accent" : "muted"}>
                  {r.action} · {r.reason.replace(/_/g, " ")}
                </Badge>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {truncated && (
        <p className="mt-3 text-xs text-[var(--color-faint-fg)]">
          Only the first {rows.length} objects are listed; the totals above cover the whole run.
        </p>
      )}
    </div>
  );
}

function JobPanel({ job, onCancel }: { job: SyncJob; onCancel: () => void }) {
  const running = job.status === "running";
  return (
    <Card className="mt-5 p-5">
      <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
        <h2 className="flex items-center gap-2 font-semibold">
          Current run <Badge tone={TONE[job.status]}>{job.status}</Badge>
        </h2>
        {running ? (
          <div className="flex items-center gap-3">
            <Spinner />
            <Button variant="danger" onClick={onCancel}>
              <Ban className="h-4 w-4" /> Cancel
            </Button>
          </div>
        ) : null}
      </div>

      <p className="mono mb-4 text-xs text-[var(--color-muted-fg)]">
        {job.source} → {job.destination}
      </p>

      <div className="grid grid-cols-2 gap-4 sm:grid-cols-5">
        <Stat label="Listed" value={formatNumber(job.listed)} />
        <Stat label="Copied" value={formatNumber(job.copied)} />
        <Stat label="Skipped" value={formatNumber(job.skipped)} />
        <Stat label="Failed" value={formatNumber(job.failed)} />
        <Stat label="Transferred" value={formatBytes(job.bytes)} />
      </div>

      {running && (
        <>
          {job.current_key && (
            <p className="mono mt-4 truncate text-xs text-[var(--color-faint-fg)]">Copying {job.current_key}…</p>
          )}
          <p className="mt-2 flex items-center gap-1.5 text-xs text-[var(--color-faint-fg)]">
            <AlertTriangle className="h-3.5 w-3.5" />
            This run stops if the server restarts. Re-running continues from where it left off.
          </p>
        </>
      )}

      {job.error && (
        <p className="mt-4 rounded-[var(--radius)] bg-[var(--color-danger-soft,transparent)] text-sm text-[var(--color-danger,inherit)]">
          {job.error}
        </p>
      )}

      {job.errors.length > 0 && (
        <div className="mt-4">
          <h3 className="mb-2 text-sm font-medium">Objects that failed</h3>
          <ul className="space-y-1 text-xs">
            {job.errors.map((e) => (
              <li key={e.key} className="mono">
                <span className="text-[var(--color-fg)]">{e.key}</span>{" "}
                <span className="text-[var(--color-muted-fg)]">— {e.message}</span>
              </li>
            ))}
          </ul>
          {job.errors_truncated && (
            <p className="mt-2 text-xs text-[var(--color-faint-fg)]">
              Only the first failures are listed; the counter above is complete.
            </p>
          )}
        </div>
      )}
    </Card>
  );
}

function HistoryPanel({ runs, activeId }: { runs: SyncJob[]; activeId?: string }) {
  const past = runs.filter((r) => r.id !== activeId);
  if (past.length === 0) return null;
  return (
    <Card className="mt-5 p-5">
      <h2 className="mb-3 font-semibold">Recent runs</h2>
      <div className="overflow-x-auto">
        <table className="w-full text-sm">
          <thead className="text-left text-xs text-[var(--color-faint-fg)]">
            <tr>
              <th className="py-1.5 pr-3">Source</th>
              <th className="py-1.5 pr-3">Copied</th>
              <th className="py-1.5 pr-3">Skipped</th>
              <th className="py-1.5 pr-3">Failed</th>
              <th className="py-1.5 pr-3">Transferred</th>
              <th className="py-1.5">Status</th>
            </tr>
          </thead>
          <tbody>
            {past.map((r) => (
              <tr key={r.id} className="border-t border-[var(--color-border)]">
                <td className="mono py-1.5 pr-3 text-xs">{r.source}</td>
                <td className="py-1.5 pr-3">{formatNumber(r.copied)}</td>
                <td className="py-1.5 pr-3">{formatNumber(r.skipped)}</td>
                <td className="py-1.5 pr-3">{formatNumber(r.failed)}</td>
                <td className="py-1.5 pr-3">{formatBytes(r.bytes)}</td>
                <td className="py-1.5">
                  <Badge tone={TONE[r.status]}>{r.status}</Badge>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </Card>
  );
}
