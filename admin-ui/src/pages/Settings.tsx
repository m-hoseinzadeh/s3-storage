import { useCallback, useEffect, useState } from "react";
import { Server, Link2, KeyRound, Info, BookOpen, Save } from "lucide-react";
import { api, ApiError, type ServerConfig } from "../lib/api";
import { Button, Card, Field, Input, Spinner, useToast } from "../components/ui";
import { PageHeader, TutList } from "../components/PageHeader";

const toLines = (s: string) =>
  s
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);

export function Settings() {
  const [config, setConfig] = useState<ServerConfig | null>(null);
  const [domains, setDomains] = useState("");
  const [allowedOrigins, setAllowedOrigins] = useState("");
  const [apiUrl, setApiUrl] = useState("");
  const [ttl, setTtl] = useState("");
  const [saving, setSaving] = useState(false);
  const toast = useToast();

  const apply = useCallback((c: ServerConfig) => {
    setConfig(c);
    setDomains(c.domains.join("\n"));
    setAllowedOrigins(c.allowed_origins.join("\n"));
    setApiUrl(c.api_public_url ?? "");
    setTtl(c.admin_session_ttl_secs != null ? String(c.admin_session_ttl_secs) : "");
  }, []);

  const load = useCallback(() => {
    api
      .config()
      .then(apply)
      .catch((e) => toast("error", e instanceof ApiError ? e.message : "Failed to load settings"));
  }, [apply, toast]);

  useEffect(load, [load]);

  const save = async () => {
    setSaving(true);
    try {
      const ttlNum = ttl.trim() === "" ? undefined : Number(ttl);
      if (ttlNum !== undefined && (!Number.isFinite(ttlNum) || ttlNum <= 0)) {
        throw new ApiError(400, "BadRequest", "Session TTL must be a positive number of seconds");
      }
      await api.updateSettings({
        domains: toLines(domains),
        allowed_origins: toLines(allowedOrigins),
        api_public_url: apiUrl.trim(),
        admin_session_ttl_secs: ttlNum,
      });
      toast("success", "Settings saved");
      load();
    } catch (e) {
      toast("error", e instanceof ApiError ? e.message : "Save failed");
    } finally {
      setSaving(false);
    }
  };

  return (
    <div>
      <PageHeader
        title="Settings & About"
        description="Edit the server's deployment settings. Changes apply live and persist across restarts."
        tutorial={
          <TutList
            items={[
              "These settings are stored in the server's database and can be changed here — no restart needed.",
              "Virtual-host base domains enable <bucket>.<domain> addressing across all buckets.",
              "Public access and a bucket's own custom domains are set per bucket — use the gear button on the Buckets page.",
              "Bind address, ports and credentials are still set via S3_* flags / environment variables at startup.",
            ]}
          />
        }
        actions={
          <Button variant="primary" onClick={save} loading={saving} disabled={!config}>
            <Save className="h-4 w-4" /> Save changes
          </Button>
        }
      />

      {!config ? (
        <Spinner />
      ) : (
        <div className="grid gap-4 lg:grid-cols-2">
          <Card className="p-5">
            <h3 className="mb-3 flex items-center gap-2 text-sm font-semibold">
              <Server className="h-4 w-4 text-[var(--color-accent)]" /> Server (read-only)
            </h3>
            <Row icon={Server} label="Server version" value={`s3-storage ${config.version}`} />
            <Row icon={KeyRound} label="Access key" value={config.access_key} mono />
            <Row icon={Link2} label="Admin path" value={config.admin_path} mono />
          </Card>

          <Card className="p-5">
            <h3 className="mb-3 text-sm font-semibold">Domains</h3>
            <Field label="Virtual-host base domains" hint="One per line, e.g. cdn.example.com — enables <bucket>.<domain>. Per-bucket custom domains are set from the Buckets page.">
              <TextArea value={domains} onChange={setDomains} placeholder={"cdn.example.com"} rows={4} />
            </Field>
            <div className="mt-4">
              <Field label="Allowed CORS origins (public endpoint)" hint="One per line, e.g. https://app.example.com. Sets Access-Control-Allow-Origin so browsers accept fonts and other cross-origin reads from the public endpoint. Use * to allow any origin. Leave blank to send no CORS headers.">
                <TextArea value={allowedOrigins} onChange={setAllowedOrigins} placeholder={"https://app.example.com"} rows={4} />
              </Field>
            </div>
          </Card>

          <Card className="p-5">
            <h3 className="mb-3 flex items-center gap-2 text-sm font-semibold">
              <Link2 className="h-4 w-4 text-[var(--color-accent)]" /> API & sessions
            </h3>
            <Field label="Public API URL" hint="Base URL of the S3 API used to mint presigned links, e.g. https://api.example.com. Leave blank to disable presigning.">
              <Input value={apiUrl} onChange={(e) => setApiUrl(e.target.value)} placeholder="https://api.example.com" />
            </Field>
            <div className="mt-4">
              <Field label="Admin session lifetime (seconds)" hint="How long a login stays valid. Applies to sessions issued after saving.">
                <Input value={ttl} onChange={(e) => setTtl(e.target.value)} placeholder="3600" inputMode="numeric" />
              </Field>
            </div>
          </Card>

          <Card className="p-5 lg:col-span-2">
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

function TextArea({
  value,
  onChange,
  placeholder,
  rows,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  rows?: number;
}) {
  return (
    <textarea
      value={value}
      onChange={(e) => onChange(e.target.value)}
      placeholder={placeholder}
      rows={rows}
      className="focusable mono w-full rounded-[var(--radius)] bg-[var(--color-bg)] border border-[var(--color-border-strong)] px-3 py-2 text-sm text-[var(--color-fg)] placeholder:text-[var(--color-faint-fg)] transition-colors"
    />
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
