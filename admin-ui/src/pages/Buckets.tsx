import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { Database, Plus, Trash2, FolderOpen, Globe, Lock } from "lucide-react";
import { api, ApiError, type BucketInfo } from "../lib/api";
import { formatDate } from "../lib/format";
import { Badge, Button, Card, ConfirmModal, EmptyState, Field, Input, Modal, Spinner, useToast } from "../components/ui";
import { PageHeader, TutList } from "../components/PageHeader";

export function Buckets() {
  const [buckets, setBuckets] = useState<BucketInfo[] | null>(null);
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [toDelete, setToDelete] = useState<string | null>(null);
  const toast = useToast();

  const load = useCallback(() => {
    api
      .listBuckets()
      .then((r) => setBuckets(r.buckets))
      .catch((e) => toast("error", e instanceof ApiError ? e.message : "Failed to load buckets"));
  }, [toast]);

  useEffect(load, [load]);

  const create = async () => {
    setBusy(true);
    try {
      await api.createBucket(name.trim());
      toast("success", `Bucket “${name}” created`);
      setCreating(false);
      setName("");
      load();
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Create failed");
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!toDelete) return;
    setBusy(true);
    try {
      await api.deleteBucket(toDelete);
      toast("success", `Bucket “${toDelete}” deleted`);
      setToDelete(null);
      load();
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Delete failed");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div>
      <PageHeader
        title="Buckets"
        description="Create, browse, and delete buckets. Public buckets allow anonymous read access."
        tutorial={
          <TutList
            items={[
              "Click New bucket and enter a DNS-style name (lowercase letters, numbers, hyphens).",
              "A public badge means the bucket is listed in S3_PUBLIC_BUCKETS and allows anonymous GET/HEAD.",
              "Open a bucket to manage its objects in the Object Browser.",
              "Deleting a bucket removes it and all objects inside — this cannot be undone.",
            ]}
          />
        }
        actions={
          <Button variant="primary" onClick={() => setCreating(true)}>
            <Plus className="h-4 w-4" /> New bucket
          </Button>
        }
      />

      {buckets === null ? (
        <Spinner />
      ) : buckets.length === 0 ? (
        <Card className="p-2">
          <EmptyState
            icon={<Database className="h-8 w-8" />}
            title="No buckets yet"
            hint="Buckets are top-level containers for your objects."
            action={
              <Button variant="primary" onClick={() => setCreating(true)}>
                <Plus className="h-4 w-4" /> Create bucket
              </Button>
            }
          />
        </Card>
      ) : (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {buckets.map((b) => (
            <Card key={b.name} className="group flex flex-col p-4">
              <div className="flex items-start justify-between">
                <div className="grid h-10 w-10 place-items-center rounded-lg bg-[var(--color-surface-2)] text-[var(--color-accent)]">
                  <Database className="h-5 w-5" />
                </div>
                {b.public ? (
                  <Badge tone="accent">
                    <Globe className="h-3 w-3" /> public
                  </Badge>
                ) : (
                  <Badge tone="muted">
                    <Lock className="h-3 w-3" /> private
                  </Badge>
                )}
              </div>
              <div className="mt-3 truncate font-semibold" title={b.name}>
                {b.name}
              </div>
              <div className="text-xs text-[var(--color-faint-fg)]">Created {formatDate(b.creation_date)}</div>

              <div className="mt-4 flex gap-2">
                <Link to={`/browse?bucket=${encodeURIComponent(b.name)}`} className="flex-1">
                  <Button variant="secondary" size="sm" className="w-full justify-center">
                    <FolderOpen className="h-4 w-4" /> Open
                  </Button>
                </Link>
                <Button variant="ghost" size="icon" aria-label={`Delete ${b.name}`} onClick={() => setToDelete(b.name)}>
                  <Trash2 className="h-4 w-4 text-[var(--color-danger)]" />
                </Button>
              </div>
            </Card>
          ))}
        </div>
      )}

      <Modal
        open={creating}
        onClose={() => setCreating(false)}
        title="New bucket"
        description="Bucket names should be lowercase and DNS-compatible."
        footer={
          <>
            <Button variant="ghost" onClick={() => setCreating(false)}>
              Cancel
            </Button>
            <Button variant="primary" onClick={create} loading={busy} disabled={!name.trim()}>
              Create
            </Button>
          </>
        }
      >
        <Field label="Bucket name" hint="e.g. assets, user-uploads, backups-2026">
          <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="my-bucket" autoFocus onKeyDown={(e) => e.key === "Enter" && name.trim() && create()} />
        </Field>
      </Modal>

      <ConfirmModal
        open={toDelete !== null}
        onClose={() => setToDelete(null)}
        onConfirm={remove}
        loading={busy}
        title={`Delete bucket “${toDelete}”?`}
        message="This permanently deletes the bucket and every object it contains. This action cannot be undone."
      />
    </div>
  );
}
