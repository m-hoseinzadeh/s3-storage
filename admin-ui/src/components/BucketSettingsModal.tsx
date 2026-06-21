import { useCallback, useEffect, useState } from "react";
import { Globe, Lock, Plus, Trash2 } from "lucide-react";
import { api, ApiError, type BucketInfo } from "../lib/api";
import { Badge, Button, Field, Input, Modal, Spinner, useToast } from "./ui";

// Parse the `host=bucket` domain-map entries that point at `bucket`, returning
// just the host parts.
function hostsForBucket(domainMap: string[], bucket: string): string[] {
  return domainMap
    .map((entry) => {
      const eq = entry.indexOf("=");
      if (eq < 0) return null;
      return { host: entry.slice(0, eq).trim(), bucket: entry.slice(eq + 1).trim() };
    })
    .filter((e): e is { host: string; bucket: string } => !!e && e.bucket === bucket)
    .map((e) => e.host);
}

export function BucketSettingsModal({
  bucket,
  onClose,
  onSaved,
}: {
  bucket: BucketInfo | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [isPublic, setIsPublic] = useState(false);
  const [hosts, setHosts] = useState<string[]>([]);
  const [newHost, setNewHost] = useState("");
  const toast = useToast();

  const name = bucket?.name ?? "";

  const load = useCallback(() => {
    if (!bucket) return;
    setLoading(true);
    api
      .config()
      .then((cfg) => {
        setIsPublic(cfg.public_buckets.includes(bucket.name));
        setHosts(hostsForBucket(cfg.domain_map, bucket.name));
      })
      .catch((e) => toast("error", e instanceof ApiError ? e.message : "Failed to load bucket settings"))
      .finally(() => setLoading(false));
  }, [bucket, toast]);

  useEffect(() => {
    setNewHost("");
    load();
  }, [load]);

  const addHost = () => {
    const host = newHost.trim().toLowerCase();
    if (!host) return;
    if (host.includes("=")) {
      toast("error", "A domain can't contain '='");
      return;
    }
    if (hosts.includes(host)) {
      toast("error", `${host} is already mapped to this bucket`);
      return;
    }
    setHosts((h) => [...h, host]);
    setNewHost("");
  };

  const removeHost = (host: string) => setHosts((h) => h.filter((x) => x !== host));

  const save = async () => {
    if (!bucket) return;
    setSaving(true);
    try {
      // Re-read the current config so concurrent edits to other buckets aren't clobbered.
      const cfg = await api.config();

      const publicSet = new Set(cfg.public_buckets);
      if (isPublic) publicSet.add(bucket.name);
      else publicSet.delete(bucket.name);

      // Keep every mapping that doesn't target this bucket, then re-append ours.
      const otherEntries = cfg.domain_map.filter((entry) => {
        const eq = entry.indexOf("=");
        return eq < 0 || entry.slice(eq + 1).trim() !== bucket.name;
      });
      const ourEntries = hosts.map((host) => `${host}=${bucket.name}`);

      await api.updateSettings({
        public_buckets: [...publicSet],
        domain_map: [...otherEntries, ...ourEntries],
      });
      toast("success", `Settings for “${bucket.name}” saved`);
      onSaved();
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Save failed");
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      open={bucket !== null}
      onClose={onClose}
      title={`Settings — ${name}`}
      description="Control anonymous access and the domains that point at this bucket."
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" onClick={save} loading={saving} disabled={loading}>
            Save changes
          </Button>
        </>
      }
    >
      {loading ? (
        <Spinner />
      ) : (
        <>
          <Field label="Public access" hint="Public buckets allow anonymous GET/HEAD on the public port.">
            <button
              type="button"
              onClick={() => setIsPublic((p) => !p)}
              aria-label={`Make ${name} ${isPublic ? "private" : "public"}`}
              className="focusable cursor-pointer rounded-full"
            >
              {isPublic ? (
                <Badge tone="accent">
                  <Globe className="h-3 w-3" /> public
                </Badge>
              ) : (
                <Badge tone="muted">
                  <Lock className="h-3 w-3" /> private
                </Badge>
              )}
            </button>
          </Field>

          <Field label="Custom domains" hint="Each host points straight at this bucket, e.g. files.example.com.">
            <div className="space-y-2">
              {hosts.length > 0 && (
                <ul className="space-y-1.5">
                  {hosts.map((host) => (
                    <li
                      key={host}
                      className="flex items-center justify-between gap-2 rounded-[var(--radius)] border border-[var(--color-border)] bg-[var(--color-bg)] px-3 py-1.5"
                    >
                      <span className="mono truncate text-sm" title={host}>
                        {host}
                      </span>
                      <Button
                        variant="ghost"
                        size="icon"
                        aria-label={`Remove ${host}`}
                        onClick={() => removeHost(host)}
                      >
                        <Trash2 className="h-4 w-4 text-[var(--color-danger)]" />
                      </Button>
                    </li>
                  ))}
                </ul>
              )}
              <div className="flex gap-2">
                <Input
                  value={newHost}
                  onChange={(e) => setNewHost(e.target.value)}
                  placeholder="files.example.com"
                  onKeyDown={(e) => e.key === "Enter" && (e.preventDefault(), addHost())}
                />
                <Button variant="secondary" onClick={addHost} disabled={!newHost.trim()}>
                  <Plus className="h-4 w-4" /> Add
                </Button>
              </div>
            </div>
          </Field>
        </>
      )}
    </Modal>
  );
}
