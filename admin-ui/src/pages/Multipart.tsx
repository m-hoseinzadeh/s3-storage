import { useCallback, useEffect, useState } from "react";
import { Layers, RefreshCw, ChevronDown, Trash2 } from "lucide-react";
import { api, ApiError, type PartInfo, type UploadSession } from "../lib/api";
import { basename, formatBytes, formatDate } from "../lib/format";
import { Badge, Button, Card, ConfirmModal, EmptyState, Spinner, cn, useToast } from "../components/ui";
import { PageHeader, TutList } from "../components/PageHeader";

export function Multipart() {
  const [sessions, setSessions] = useState<UploadSession[] | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [parts, setParts] = useState<Record<string, PartInfo[]>>({});
  const [toAbort, setToAbort] = useState<UploadSession | null>(null);
  const [busy, setBusy] = useState(false);
  const toast = useToast();

  const load = useCallback(() => {
    api
      .listMultipart()
      .then((r) => setSessions(r.uploads))
      .catch((e) => toast("error", e instanceof ApiError ? e.message : "Failed to load uploads"));
  }, [toast]);

  useEffect(load, [load]);

  const expand = async (s: UploadSession) => {
    if (expanded === s.upload_id) {
      setExpanded(null);
      return;
    }
    setExpanded(s.upload_id);
    if (!parts[s.upload_id] && s.bucket && s.key) {
      try {
        const r = await api.listParts(s.bucket, s.key, s.upload_id);
        setParts((p) => ({ ...p, [s.upload_id]: r.parts }));
      } catch (e) {
        toast("error", e instanceof ApiError ? e.message : "Failed to load parts");
      }
    }
  };

  const abort = async () => {
    if (!toAbort?.bucket || !toAbort.key) return;
    setBusy(true);
    try {
      await api.abortMultipart(toAbort.bucket, toAbort.key, toAbort.upload_id);
      toast("success", "Upload aborted");
      setToAbort(null);
      load();
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Abort failed");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div>
      <PageHeader
        title="Multipart Uploads"
        description="In-progress multipart upload sessions that have not yet been completed or aborted."
        tutorial={
          <TutList
            items={[
              "Large uploads are split into parts; until completed they hold disk space as temporary part files.",
              "Expand a session to inspect its uploaded parts (number, size, ETag).",
              "Abort a stale or stuck session to reclaim its temporary storage.",
              "Sessions are reconstructed by scanning the data root, so the bucket/key is shown when known.",
            ]}
          />
        }
        actions={
          <Button variant="ghost" size="icon" onClick={load} aria-label="Refresh">
            <RefreshCw className="h-4 w-4" />
          </Button>
        }
      />

      {sessions === null ? (
        <Spinner />
      ) : sessions.length === 0 ? (
        <Card className="p-2">
          <EmptyState icon={<Layers className="h-8 w-8" />} title="No in-progress uploads" hint="Multipart sessions appear here while large uploads are underway." />
        </Card>
      ) : (
        <div className="space-y-2">
          {sessions.map((s) => (
            <Card key={s.upload_id} className="overflow-hidden">
              <div className="flex items-center justify-between gap-3 p-4">
                <button onClick={() => expand(s)} className="focusable flex min-w-0 flex-1 items-center gap-3 text-left cursor-pointer">
                  <ChevronDown className={cn("h-4 w-4 shrink-0 text-[var(--color-faint-fg)] transition-transform", expanded === s.upload_id && "rotate-180")} />
                  <div className="min-w-0">
                    <div className="truncate font-medium">{s.key ? basename(s.key) : "(unknown key)"}</div>
                    <div className="mono truncate text-xs text-[var(--color-faint-fg)]">
                      {s.bucket ?? "?"}/{s.key ?? "?"} · {s.upload_id}
                    </div>
                  </div>
                </button>
                <div className="flex shrink-0 items-center gap-3">
                  {s.initiated && <Badge tone="muted">{formatDate(s.initiated)}</Badge>}
                  <Button variant="ghost" size="sm" onClick={() => setToAbort(s)} disabled={!s.bucket || !s.key}>
                    <Trash2 className="h-4 w-4 text-[var(--color-danger)]" /> Abort
                  </Button>
                </div>
              </div>

              {expanded === s.upload_id && (
                <div className="border-t border-[var(--color-border)] bg-[var(--color-bg)]/40 px-4 py-3 animate-pop">
                  {!parts[s.upload_id] ? (
                    <Spinner />
                  ) : parts[s.upload_id].length === 0 ? (
                    <p className="text-sm text-[var(--color-faint-fg)]">No parts uploaded yet.</p>
                  ) : (
                    <table className="w-full text-sm">
                      <thead className="text-left text-xs uppercase text-[var(--color-faint-fg)]">
                        <tr>
                          <th className="py-1.5">Part</th>
                          <th className="py-1.5 text-right">Size</th>
                          <th className="py-1.5">ETag</th>
                          <th className="py-1.5">Modified</th>
                        </tr>
                      </thead>
                      <tbody>
                        {parts[s.upload_id].map((p) => (
                          <tr key={p.part_number} className="border-t border-[var(--color-border)]/50">
                            <td className="mono py-1.5">#{p.part_number}</td>
                            <td className="mono py-1.5 text-right text-[var(--color-muted-fg)]">{formatBytes(p.size)}</td>
                            <td className="mono truncate py-1.5 text-xs text-[var(--color-faint-fg)]">{p.etag}</td>
                            <td className="py-1.5 text-[var(--color-faint-fg)]">{formatDate(p.last_modified)}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  )}
                </div>
              )}
            </Card>
          ))}
        </div>
      )}

      <ConfirmModal
        open={toAbort !== null}
        onClose={() => setToAbort(null)}
        onConfirm={abort}
        loading={busy}
        confirmLabel="Abort upload"
        title="Abort multipart upload?"
        message="This deletes all uploaded parts for this session and cannot be undone."
      />
    </div>
  );
}
