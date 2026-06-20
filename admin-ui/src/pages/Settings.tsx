import { useEffect, useState } from "react";
import { Server, Globe, Link2, KeyRound, Info, BookOpen } from "lucide-react";
import { api, ApiError, type ServerConfig } from "../lib/api";
import { Badge, Card, Spinner, useToast } from "../components/ui";
import { PageHeader, TutList } from "../components/PageHeader";

export function Settings() {
  const [config, setConfig] = useState<ServerConfig | null>(null);
  const toast = useToast();

  useEffect(() => {
    api
      .config()
      .then(setConfig)
      .catch((e) => toast("error", e instanceof ApiError ? e.message : "Failed to load config"));
  }, [toast]);

  return (
    <div>
      <PageHeader
        title="Settings & About"
        description="Read-only view of the server's runtime configuration and how the admin panel works."
        tutorial={
          <TutList
            items={[
              "These values come from the server's CLI flags / environment variables and cannot be changed here.",
              "To change them, update S3_* variables (or flags) and restart the server.",
              "Public buckets allow anonymous GET/HEAD; everything else requires credentials or a presigned URL.",
            ]}
          />
        }
      />

      {!config ? (
        <Spinner />
      ) : (
        <div className="grid gap-4 lg:grid-cols-2">
          <Card className="p-5">
            <Row icon={Server} label="Server version" value={`s3-storage ${config.version}`} />
            <Row icon={KeyRound} label="Access key" value={config.access_key} mono />
            <Row icon={Link2} label="Admin path" value={config.admin_path} mono />
          </Card>

          <Card className="p-5">
            <h3 className="mb-3 flex items-center gap-2 text-sm font-semibold">
              <Globe className="h-4 w-4 text-[var(--color-accent)]" /> Public buckets
            </h3>
            {config.public_buckets.length ? (
              <div className="flex flex-wrap gap-2">
                {config.public_buckets.map((b) => (
                  <Badge key={b} tone="accent">{b}</Badge>
                ))}
              </div>
            ) : (
              <p className="text-sm text-[var(--color-faint-fg)]">None — all buckets are private.</p>
            )}
          </Card>

          <Card className="p-5">
            <h3 className="mb-3 text-sm font-semibold">Virtual-host domains</h3>
            {config.domains.length ? (
              <ul className="space-y-1 text-sm">{config.domains.map((d) => <li key={d} className="mono">{d}</li>)}</ul>
            ) : (
              <p className="text-sm text-[var(--color-faint-fg)]">None configured.</p>
            )}
            <h3 className="mb-2 mt-4 text-sm font-semibold">Custom domain map</h3>
            {config.domain_map.length ? (
              <ul className="space-y-1 text-sm">{config.domain_map.map((d) => <li key={d} className="mono">{d}</li>)}</ul>
            ) : (
              <p className="text-sm text-[var(--color-faint-fg)]">None configured.</p>
            )}
          </Card>

          <Card className="p-5">
            <h3 className="mb-3 flex items-center gap-2 text-sm font-semibold">
              <BookOpen className="h-4 w-4 text-[var(--color-accent)]" /> About this panel
            </h3>
            <div className="space-y-2 text-sm leading-relaxed text-[var(--color-muted-fg)]">
              <p className="flex gap-2"><Info className="mt-0.5 h-4 w-4 shrink-0 text-[var(--color-faint-fg)]" /> The admin panel is served by the same binary, under the admin path, and talks to a JSON API that reuses the storage backend directly.</p>
              <p>It covers every operation the server supports: bucket and object CRUD, copy/move, batch delete, folders, metadata, checksums, presigned URLs, and multipart session management.</p>
              <p>Authentication uses your S3 access/secret key; a signed, HTTP-only session cookie keeps you signed in.</p>
            </div>
          </Card>
        </div>
      )}
    </div>
  );
}

function Row({ icon: Icon, label, value, mono }: { icon: typeof Server; label: string; value: string; mono?: boolean }) {
  return (
    <div className="flex items-center justify-between gap-3 border-b border-[var(--color-border)] py-2.5 last:border-0">
      <span className="flex items-center gap-2 text-sm text-[var(--color-muted-fg)]">
        <Icon className="h-4 w-4 text-[var(--color-faint-fg)]" /> {label}
      </span>
      <span className={mono ? "mono truncate text-sm" : "truncate text-sm"} title={value}>{value}</span>
    </div>
  );
}
