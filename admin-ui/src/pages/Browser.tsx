import { useCallback, useEffect, useMemo, useRef, useState, type DragEvent } from "react";
import { useSearchParams } from "react-router-dom";
import {
  Upload,
  FolderPlus,
  RefreshCw,
  Folder,
  File,
  Download,
  Trash2,
  Share2,
  Info,
  ChevronRight,
  Home,
  Database,
  Copy as CopyIcon,
  FileArchive,
  X,
} from "lucide-react";
import {
  api,
  ApiError,
  uploadFile,
  type BucketInfo,
  type Listing,
  type ObjectHead,
} from "../lib/api";
import { basename, formatBytes, formatDate } from "../lib/format";
import {
  Badge,
  Button,
  Card,
  ConfirmModal,
  EmptyState,
  Field,
  Input,
  Modal,
  Spinner,
  cn,
  useToast,
} from "../components/ui";
import { PageHeader, TutList } from "../components/PageHeader";

interface UploadItem {
  name: string;
  pct: number;
  error?: string;
}

export function Browser() {
  const [params, setParams] = useSearchParams();
  const bucket = params.get("bucket") ?? "";
  const prefix = params.get("prefix") ?? "";

  const [buckets, setBuckets] = useState<BucketInfo[]>([]);
  const [listing, setListing] = useState<Listing | null>(null);
  const [loading, setLoading] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [uploads, setUploads] = useState<UploadItem[]>([]);
  const [dragging, setDragging] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);
  const toast = useToast();

  // modals
  const [newFolder, setNewFolder] = useState(false);
  const [folderName, setFolderName] = useState("");
  const [details, setDetails] = useState<string | null>(null);
  const [transfer, setTransfer] = useState<string | null>(null);
  const [extract, setExtract] = useState<string | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<string[] | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.listBuckets().then((r) => setBuckets(r.buckets)).catch(() => {});
  }, []);

  const load = useCallback(() => {
    if (!bucket) return;
    setLoading(true);
    setSelected(new Set());
    api
      .listObjects(bucket, prefix, "/")
      .then(setListing)
      .catch((e) => toast("error", e instanceof ApiError ? e.message : "Failed to list objects"))
      .finally(() => setLoading(false));
  }, [bucket, prefix, toast]);

  useEffect(load, [load]);

  const setPrefix = (p: string) => setParams({ bucket, ...(p ? { prefix: p } : {}) });
  const setBucket = (b: string) => setParams(b ? { bucket: b } : {});

  const crumbs = prefix.split("/").filter(Boolean);

  // files at this level (exclude the folder placeholder object equal to prefix)
  const files = useMemo(
    () => (listing?.objects ?? []).filter((o) => o.key !== prefix && !o.key.endsWith("/")),
    [listing, prefix],
  );
  const folders = listing?.prefixes ?? [];

  // ---- uploads ----
  const doUpload = async (fileList: FileList | File[]) => {
    const arr = Array.from(fileList);
    for (const file of arr) {
      const key = prefix + file.name;
      setUploads((u) => [...u, { name: file.name, pct: 0 }]);
      try {
        await uploadFile(api.uploadUrl(bucket, key, file.type || undefined), file, (pct) =>
          setUploads((u) => u.map((x) => (x.name === file.name ? { ...x, pct } : x))),
        );
        setUploads((u) => u.map((x) => (x.name === file.name ? { ...x, pct: 100 } : x)));
      } catch (e) {
        setUploads((u) => u.map((x) => (x.name === file.name ? { ...x, error: e instanceof ApiError ? e.message : "failed" } : x)));
      }
    }
    toast("success", `Uploaded ${arr.length} file${arr.length > 1 ? "s" : ""}`);
    setTimeout(() => setUploads([]), 2500);
    load();
  };

  const onDrop = (e: DragEvent) => {
    e.preventDefault();
    setDragging(false);
    if (e.dataTransfer.files.length) doUpload(e.dataTransfer.files);
  };

  // ---- selection ----
  const toggle = (key: string) =>
    setSelected((s) => {
      const n = new Set(s);
      n.has(key) ? n.delete(key) : n.add(key);
      return n;
    });
  const allSelected = files.length > 0 && files.every((f) => selected.has(f.key));
  const toggleAll = () => setSelected(allSelected ? new Set() : new Set(files.map((f) => f.key)));

  const createFolder = async () => {
    setBusy(true);
    try {
      await api.createFolder(bucket, prefix + folderName.trim());
      toast("success", "Folder created");
      setNewFolder(false);
      setFolderName("");
      load();
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Failed");
    } finally {
      setBusy(false);
    }
  };

  const doDelete = async () => {
    if (!confirmDelete) return;
    setBusy(true);
    try {
      if (confirmDelete.length === 1) await api.deleteObject(bucket, confirmDelete[0]);
      else await api.batchDelete(bucket, confirmDelete);
      toast("success", `Deleted ${confirmDelete.length} object${confirmDelete.length > 1 ? "s" : ""}`);
      setConfirmDelete(null);
      load();
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Delete failed");
    } finally {
      setBusy(false);
    }
  };

  // ---- no bucket selected ----
  if (!bucket) {
    return (
      <div>
        <PageHeader
          title="Object Browser"
          description="Browse, upload, and manage objects inside a bucket."
          tutorial={<TutList items={["Pick a bucket below to start.", "Inside a bucket you can upload, download, organise into folders, edit metadata, and create share links."]} />}
        />
        <Card className="p-2">
          {buckets.length === 0 ? (
            <EmptyState icon={<Database className="h-8 w-8" />} title="No buckets" hint="Create a bucket first from the Buckets page." />
          ) : (
            <div className="grid gap-2 p-2 sm:grid-cols-2 lg:grid-cols-3">
              {buckets.map((b) => (
                <button
                  key={b.name}
                  onClick={() => setBucket(b.name)}
                  className="focusable flex items-center gap-3 rounded-[var(--radius)] border border-[var(--color-border)] p-3 text-left transition-colors hover:bg-[var(--color-surface-2)] cursor-pointer"
                >
                  <Database className="h-5 w-5 text-[var(--color-accent)]" />
                  <span className="truncate font-medium">{b.name}</span>
                  {b.public && <Badge tone="accent">public</Badge>}
                </button>
              ))}
            </div>
          )}
        </Card>
      </div>
    );
  }

  return (
    <div onDragOver={(e) => { e.preventDefault(); setDragging(true); }} onDragLeave={() => setDragging(false)} onDrop={onDrop}>
      <PageHeader
        title="Object Browser"
        description="Upload, download, organise, and share objects. Drag files anywhere to upload."
        tutorial={
          <TutList
            items={[
              "Use the breadcrumb to navigate folders; click a folder row to open it.",
              "Drag-and-drop files (or use Upload) — large files stream straight to disk.",
              "Select rows with the checkboxes to delete in bulk.",
              "Open Details on any object to view checksums, edit metadata, copy/move/rename, or create a presigned share link.",
            ]}
          />
        }
        actions={
          <>
            <Button variant="ghost" size="icon" onClick={load} aria-label="Refresh">
              <RefreshCw className={cn("h-4 w-4", loading && "animate-spin")} />
            </Button>
            <Button variant="secondary" onClick={() => setNewFolder(true)}>
              <FolderPlus className="h-4 w-4" /> Folder
            </Button>
            <Button variant="primary" onClick={() => fileInput.current?.click()}>
              <Upload className="h-4 w-4" /> Upload
            </Button>
            <input ref={fileInput} type="file" multiple hidden onChange={(e) => e.target.files && doUpload(e.target.files)} />
          </>
        }
      />

      {/* breadcrumb */}
      <div className="mb-3 flex flex-wrap items-center gap-1 text-sm">
        <button onClick={() => setBucket("")} className="focusable rounded px-1.5 py-1 text-[var(--color-muted-fg)] hover:text-[var(--color-fg)] cursor-pointer">
          <Database className="mr-1 inline h-4 w-4" />
          buckets
        </button>
        <ChevronRight className="h-4 w-4 text-[var(--color-faint-fg)]" />
        <button onClick={() => setPrefix("")} className="focusable rounded px-1.5 py-1 font-medium hover:text-[var(--color-accent)] cursor-pointer">
          <Home className="mr-1 inline h-4 w-4" />
          {bucket}
        </button>
        {crumbs.map((c, i) => (
          <span key={i} className="flex items-center gap-1">
            <ChevronRight className="h-4 w-4 text-[var(--color-faint-fg)]" />
            <button
              onClick={() => setPrefix(crumbs.slice(0, i + 1).join("/") + "/")}
              className="focusable rounded px-1.5 py-1 hover:text-[var(--color-accent)] cursor-pointer"
            >
              {c}
            </button>
          </span>
        ))}
      </div>

      {/* batch bar */}
      {selected.size > 0 && (
        <div className="mb-3 flex items-center justify-between rounded-[var(--radius)] border border-[var(--color-border-strong)] bg-[var(--color-surface-2)] px-4 py-2 animate-pop">
          <span className="text-sm">{selected.size} selected</span>
          <div className="flex gap-2">
            <Button variant="ghost" size="sm" onClick={() => setSelected(new Set())}>
              Clear
            </Button>
            <Button variant="danger" size="sm" onClick={() => setConfirmDelete([...selected])}>
              <Trash2 className="h-4 w-4" /> Delete
            </Button>
          </div>
        </div>
      )}

      <Card className="overflow-hidden">
        {loading ? (
          <Spinner label="Loading objects…" />
        ) : folders.length === 0 && files.length === 0 ? (
          <EmptyState
            icon={<Folder className="h-8 w-8" />}
            title="This folder is empty"
            hint="Drag files here or use the Upload button."
            action={
              <Button variant="primary" onClick={() => fileInput.current?.click()}>
                <Upload className="h-4 w-4" /> Upload files
              </Button>
            }
          />
        ) : (
          <table className="w-full text-sm">
            <thead className="border-b border-[var(--color-border)] text-left text-xs uppercase tracking-wide text-[var(--color-faint-fg)]">
              <tr>
                <th className="w-10 px-4 py-2.5">
                  <input type="checkbox" checked={allSelected} onChange={toggleAll} className="accent-[var(--color-accent)] cursor-pointer" aria-label="Select all" />
                </th>
                <th className="px-2 py-2.5">Name</th>
                <th className="px-2 py-2.5 text-right">Size</th>
                <th className="hidden px-2 py-2.5 md:table-cell">Modified</th>
                <th className="px-4 py-2.5 text-right">Actions</th>
              </tr>
            </thead>
            <tbody>
              {folders.map((f) => (
                <tr key={f} className="border-b border-[var(--color-border)]/60 hover:bg-[var(--color-surface-2)]">
                  <td className="px-4 py-2.5" />
                  <td className="px-2 py-2.5">
                    <button onClick={() => setPrefix(f)} className="focusable flex items-center gap-2 font-medium hover:text-[var(--color-accent)] cursor-pointer">
                      <Folder className="h-4 w-4 text-[var(--color-info)]" />
                      {basename(f)}/
                    </button>
                  </td>
                  <td className="px-2 py-2.5 text-right text-[var(--color-faint-fg)]">—</td>
                  <td className="hidden px-2 py-2.5 text-[var(--color-faint-fg)] md:table-cell">—</td>
                  <td className="px-4 py-2.5" />
                </tr>
              ))}
              {files.map((o) => (
                <tr key={o.key} className="border-b border-[var(--color-border)]/60 hover:bg-[var(--color-surface-2)]">
                  <td className="px-4 py-2.5">
                    <input type="checkbox" checked={selected.has(o.key)} onChange={() => toggle(o.key)} className="accent-[var(--color-accent)] cursor-pointer" aria-label={`Select ${o.key}`} />
                  </td>
                  <td className="px-2 py-2.5">
                    <span className="flex items-center gap-2">
                      <File className="h-4 w-4 text-[var(--color-muted-fg)]" />
                      <span className="truncate">{basename(o.key)}</span>
                    </span>
                  </td>
                  <td className="mono px-2 py-2.5 text-right text-[var(--color-muted-fg)]">{formatBytes(o.size)}</td>
                  <td className="hidden px-2 py-2.5 text-[var(--color-faint-fg)] md:table-cell">{formatDate(o.last_modified)}</td>
                  <td className="px-4 py-2.5">
                    <div className="flex items-center justify-end gap-0.5">
                      <a href={api.downloadUrl(bucket, o.key)} className="focusable rounded p-2 text-[var(--color-muted-fg)] hover:bg-[var(--color-bg)] hover:text-[var(--color-fg)]" aria-label="Download" title="Download">
                        <Download className="h-4 w-4" />
                      </a>
                      {o.key.toLowerCase().endsWith(".zip") && (
                        <button onClick={() => setExtract(o.key)} className="focusable rounded p-2 text-[var(--color-muted-fg)] hover:bg-[var(--color-bg)] hover:text-[var(--color-fg)] cursor-pointer" aria-label="Extract" title="Extract archive">
                          <FileArchive className="h-4 w-4" />
                        </button>
                      )}
                      <button onClick={() => setDetails(o.key)} className="focusable rounded p-2 text-[var(--color-muted-fg)] hover:bg-[var(--color-bg)] hover:text-[var(--color-fg)] cursor-pointer" aria-label="Details" title="Details, metadata & share">
                        <Info className="h-4 w-4" />
                      </button>
                      <button onClick={() => setConfirmDelete([o.key])} className="focusable rounded p-2 text-[var(--color-muted-fg)] hover:bg-[var(--color-bg)] hover:text-[var(--color-danger)] cursor-pointer" aria-label="Delete" title="Delete">
                        <Trash2 className="h-4 w-4" />
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {listing?.is_truncated && (
          <div className="border-t border-[var(--color-border)] px-4 py-2 text-center text-xs text-[var(--color-faint-fg)]">
            Showing the first page of results — refine with folders to see more.
          </div>
        )}
      </Card>

      {/* upload progress */}
      {uploads.length > 0 && (
        <div className="fixed bottom-4 left-1/2 z-40 w-96 -translate-x-1/2">
          <Card className="p-3">
            <div className="mb-2 text-xs font-medium text-[var(--color-muted-fg)]">Uploading {uploads.length} file(s)</div>
            <div className="space-y-2">
              {uploads.map((u) => (
                <div key={u.name}>
                  <div className="mb-1 flex justify-between text-xs">
                    <span className="truncate">{u.name}</span>
                    <span className={u.error ? "text-[var(--color-danger)]" : "text-[var(--color-muted-fg)]"}>{u.error ?? `${u.pct}%`}</span>
                  </div>
                  <div className="h-1.5 overflow-hidden rounded-full bg-[var(--color-bg)]">
                    <div className="h-full bg-[var(--color-accent)] transition-[width]" style={{ width: `${u.pct}%` }} />
                  </div>
                </div>
              ))}
            </div>
          </Card>
        </div>
      )}

      {/* drag overlay */}
      {dragging && (
        <div className="pointer-events-none fixed inset-0 z-50 grid place-items-center bg-black/50 backdrop-blur-sm">
          <div className="rounded-2xl border-2 border-dashed border-[var(--color-accent)] bg-[var(--color-surface)] px-12 py-10 text-center">
            <Upload className="mx-auto h-10 w-10 text-[var(--color-accent)]" />
            <p className="mt-3 font-medium">Drop to upload to {prefix || bucket}</p>
          </div>
        </div>
      )}

      {/* new folder modal */}
      <Modal
        open={newFolder}
        onClose={() => setNewFolder(false)}
        title="New folder"
        description={`Creates a folder under ${prefix || bucket}.`}
        footer={
          <>
            <Button variant="ghost" onClick={() => setNewFolder(false)}>Cancel</Button>
            <Button variant="primary" onClick={createFolder} loading={busy} disabled={!folderName.trim()}>Create</Button>
          </>
        }
      >
        <Field label="Folder name">
          <Input value={folderName} onChange={(e) => setFolderName(e.target.value)} placeholder="images" autoFocus />
        </Field>
      </Modal>

      {details && (
        <DetailsModal
          bucket={bucket}
          objectKey={details}
          buckets={buckets}
          onClose={() => setDetails(null)}
          onChanged={() => { setDetails(null); load(); }}
          onTransfer={() => { setTransfer(details); setDetails(null); }}
        />
      )}

      {transfer && (
        <TransferModal bucket={bucket} objectKey={transfer} buckets={buckets} onClose={() => setTransfer(null)} onDone={() => { setTransfer(null); load(); }} />
      )}

      {extract && (
        <ExtractModal bucket={bucket} objectKey={extract} onClose={() => setExtract(null)} onDone={() => { setExtract(null); load(); }} />
      )}

      <ConfirmModal
        open={confirmDelete !== null}
        onClose={() => setConfirmDelete(null)}
        onConfirm={doDelete}
        loading={busy}
        title={`Delete ${confirmDelete?.length ?? 0} object(s)?`}
        message="The selected objects will be permanently removed. This cannot be undone."
      />
    </div>
  );
}

// ---- Details / metadata / checksums / presign ----

function DetailsModal({
  bucket,
  objectKey,
  buckets,
  onClose,
  onChanged,
  onTransfer,
}: {
  bucket: string;
  objectKey: string;
  buckets: BucketInfo[];
  onClose: () => void;
  onChanged: () => void;
  onTransfer: () => void;
}) {
  void buckets;
  const [head, setHead] = useState<ObjectHead | null>(null);
  const [contentType, setContentType] = useState("");
  const [cacheControl, setCacheControl] = useState("");
  const [meta, setMeta] = useState<[string, string][]>([]);
  const [saving, setSaving] = useState(false);
  const [link, setLink] = useState<string | null>(null);
  const [expires, setExpires] = useState(3600);
  const toast = useToast();

  useEffect(() => {
    api
      .head(bucket, objectKey)
      .then((h) => {
        setHead(h);
        setContentType(h.content_type ?? "");
        setCacheControl(h.cache_control ?? "");
        setMeta(Object.entries(h.metadata ?? {}));
      })
      .catch((e) => toast("error", e instanceof ApiError ? e.message : "Failed to load object"));
  }, [bucket, objectKey, toast]);

  const save = async () => {
    setSaving(true);
    try {
      await api.updateMetadata({
        bucket,
        key: objectKey,
        content_type: contentType || undefined,
        cache_control: cacheControl || undefined,
        metadata: Object.fromEntries(meta.filter(([k]) => k.trim())),
      });
      toast("success", "Metadata saved");
      onChanged();
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Save failed");
    } finally {
      setSaving(false);
    }
  };

  const makeLink = async (method: "GET" | "PUT") => {
    try {
      const r = await api.presign(bucket, objectKey, method, expires);
      setLink(r.url);
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Presign failed");
    }
  };

  const checksums = Object.entries(head?.checksums ?? {}).filter(([, v]) => v);

  return (
    <Modal open onClose={onClose} title={basename(objectKey)} description={objectKey}>
      {!head ? (
        <Spinner />
      ) : (
        <div className="space-y-5">
          <div className="grid grid-cols-2 gap-3 text-sm">
            <Info2 label="Size" value={formatBytes(head.content_length)} />
            <Info2 label="Modified" value={formatDate(head.last_modified)} />
            <Info2 label="ETag" value={head.etag ?? "—"} mono />
            <Info2 label="Content-Type" value={head.content_type ?? "—"} />
          </div>

          {checksums.length > 0 && (
            <div>
              <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-faint-fg)]">Checksums</h3>
              <div className="space-y-1">
                {checksums.map(([k, v]) => (
                  <div key={k} className="flex items-center justify-between gap-2 rounded bg-[var(--color-bg)] px-2 py-1 text-xs">
                    <span className="uppercase text-[var(--color-muted-fg)]">{k}</span>
                    <span className="mono truncate">{v}</span>
                  </div>
                ))}
              </div>
            </div>
          )}

          <div>
            <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-faint-fg)]">Metadata</h3>
            <div className="grid grid-cols-2 gap-2">
              <Field label="Content-Type"><Input value={contentType} onChange={(e) => setContentType(e.target.value)} placeholder="application/octet-stream" /></Field>
              <Field label="Cache-Control"><Input value={cacheControl} onChange={(e) => setCacheControl(e.target.value)} placeholder="max-age=3600" /></Field>
            </div>
            <div className="mt-3 space-y-2">
              <div className="text-xs text-[var(--color-faint-fg)]">Custom metadata (x-amz-meta-*)</div>
              {meta.map(([k, v], i) => (
                <div key={i} className="flex gap-2">
                  <Input value={k} placeholder="key" onChange={(e) => setMeta((m) => m.map((row, j) => (j === i ? [e.target.value, row[1]] : row)))} />
                  <Input value={v} placeholder="value" onChange={(e) => setMeta((m) => m.map((row, j) => (j === i ? [row[0], e.target.value] : row)))} />
                  <Button variant="ghost" size="icon" onClick={() => setMeta((m) => m.filter((_, j) => j !== i))} aria-label="Remove"><X className="h-4 w-4" /></Button>
                </div>
              ))}
              <Button variant="ghost" size="sm" onClick={() => setMeta((m) => [...m, ["", ""]])}>+ Add field</Button>
            </div>
            <div className="mt-3 flex justify-end">
              <Button variant="primary" size="sm" onClick={save} loading={saving}>Save metadata</Button>
            </div>
          </div>

          <div>
            <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-faint-fg)]">Presigned share link</h3>
            <div className="flex items-end gap-2">
              <Field label="Expires (seconds)"><Input type="number" value={expires} onChange={(e) => setExpires(Number(e.target.value) || 3600)} /></Field>
              <Button variant="secondary" size="sm" onClick={() => makeLink("GET")}><Share2 className="h-4 w-4" /> GET link</Button>
              <Button variant="secondary" size="sm" onClick={() => makeLink("PUT")}>PUT link</Button>
            </div>
            {link && (
              <div className="mt-2 flex items-center gap-2 rounded bg-[var(--color-bg)] p-2">
                <span className="mono truncate text-xs">{link}</span>
                <Button variant="ghost" size="icon" onClick={() => { navigator.clipboard?.writeText(link); toast("success", "Link copied"); }} aria-label="Copy"><CopyIcon className="h-4 w-4" /></Button>
              </div>
            )}
          </div>

          <div className="flex flex-wrap justify-between gap-2 border-t border-[var(--color-border)] pt-4">
            <a href={api.downloadUrl(bucket, objectKey)}>
              <Button variant="secondary" size="sm"><Download className="h-4 w-4" /> Download</Button>
            </a>
            <Button variant="outline" size="sm" onClick={onTransfer}><CopyIcon className="h-4 w-4" /> Copy / Move / Rename</Button>
          </div>
        </div>
      )}
    </Modal>
  );
}

function Info2({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="rounded bg-[var(--color-bg)] px-3 py-2">
      <div className="text-xs text-[var(--color-faint-fg)]">{label}</div>
      <div className={cn("truncate", mono && "mono text-xs")} title={value}>{value}</div>
    </div>
  );
}

// ---- Copy / Move / Rename ----

function TransferModal({
  bucket,
  objectKey,
  buckets,
  onClose,
  onDone,
}: {
  bucket: string;
  objectKey: string;
  buckets: BucketInfo[];
  onClose: () => void;
  onDone: () => void;
}) {
  const [dstBucket, setDstBucket] = useState(bucket);
  const [dstKey, setDstKey] = useState(objectKey);
  const [move, setMove] = useState(false);
  const [busy, setBusy] = useState(false);
  const toast = useToast();

  const run = async () => {
    setBusy(true);
    try {
      if (move) await api.move(bucket, objectKey, dstBucket, dstKey);
      else await api.copy(bucket, objectKey, dstBucket, dstKey);
      toast("success", move ? "Object moved" : "Object copied");
      onDone();
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Transfer failed");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open
      onClose={onClose}
      title="Copy / Move / Rename"
      description="Set a destination. Keep the same bucket and change the key to rename."
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={run} loading={busy} disabled={!dstKey.trim() || (dstBucket === bucket && dstKey === objectKey)}>
            {move ? "Move" : "Copy"}
          </Button>
        </>
      }
    >
      <Field label="Destination bucket">
        <select
          value={dstBucket}
          onChange={(e) => setDstBucket(e.target.value)}
          className="focusable h-10 w-full rounded-[var(--radius)] border border-[var(--color-border-strong)] bg-[var(--color-bg)] px-3 text-sm cursor-pointer"
        >
          {buckets.map((b) => (
            <option key={b.name} value={b.name}>{b.name}</option>
          ))}
        </select>
      </Field>
      <Field label="Destination key"><Input value={dstKey} onChange={(e) => setDstKey(e.target.value)} className="mono" /></Field>
      <label className="flex cursor-pointer items-center gap-2 text-sm">
        <input type="checkbox" checked={move} onChange={(e) => setMove(e.target.checked)} className="accent-[var(--color-accent)]" />
        Delete the original after copying (move)
      </label>
    </Modal>
  );
}

// ---- Extract ZIP archive ----

function ExtractModal({
  bucket,
  objectKey,
  onClose,
  onDone,
}: {
  bucket: string;
  objectKey: string;
  onClose: () => void;
  onDone: () => void;
}) {
  // Default to a folder named after the archive, beside it: "a/b/site.zip" -> "a/b/site/".
  const parent = objectKey.includes("/") ? objectKey.slice(0, objectKey.lastIndexOf("/") + 1) : "";
  const stem = basename(objectKey).replace(/\.zip$/i, "");
  const [dest, setDest] = useState(`${parent}${stem}/`);
  const [overwrite, setOverwrite] = useState(false);
  const [busy, setBusy] = useState(false);
  const toast = useToast();

  const run = async () => {
    setBusy(true);
    try {
      const r = await api.extract(bucket, objectKey, dest.trim(), overwrite);
      const msg =
        r.skipped_count > 0
          ? `Extracted ${r.extracted_count} file(s), skipped ${r.skipped_count} existing`
          : `Extracted ${r.extracted_count} file(s)`;
      toast("success", msg);
      onDone();
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Extraction failed");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open
      onClose={onClose}
      title="Extract archive"
      description={`Unpack ${basename(objectKey)} into individual objects in this bucket.`}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={run} loading={busy}>
            <FileArchive className="h-4 w-4" /> Extract
          </Button>
        </>
      }
    >
      <Field label="Destination prefix (folder)">
        <Input value={dest} onChange={(e) => setDest(e.target.value)} placeholder="leave blank for bucket root" className="mono" autoFocus />
      </Field>
      <p className="-mt-1 mb-3 text-xs text-[var(--color-faint-fg)]">
        Folders inside the archive are preserved. The archive itself is left in place.
      </p>
      <label className="flex cursor-pointer items-center gap-2 text-sm">
        <input type="checkbox" checked={overwrite} onChange={(e) => setOverwrite(e.target.checked)} className="accent-[var(--color-accent)]" />
        Overwrite objects that already exist
      </label>
    </Modal>
  );
}
