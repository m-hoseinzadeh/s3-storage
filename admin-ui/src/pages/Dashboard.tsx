import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { Database, Files, HardDrive, Globe, ArrowRight } from "lucide-react";
import { api, ApiError, type Stats } from "../lib/api";
import { formatBytes, formatNumber } from "../lib/format";
import { Badge, Card, EmptyState, Spinner, useToast } from "../components/ui";
import { PageHeader, TutList } from "../components/PageHeader";

export function Dashboard() {
  const [stats, setStats] = useState<Stats | null>(null);
  const [loading, setLoading] = useState(true);
  const toast = useToast();

  useEffect(() => {
    api
      .stats()
      .then(setStats)
      .catch((e) => toast("error", e instanceof ApiError ? e.message : "Failed to load stats"))
      .finally(() => setLoading(false));
  }, [toast]);

  const cards = [
    { label: "Buckets", value: formatNumber(stats?.bucket_count), icon: Database },
    { label: "Objects", value: formatNumber(stats?.object_count), icon: Files },
    { label: "Total size", value: formatBytes(stats?.total_size), icon: HardDrive },
    { label: "Public buckets", value: formatNumber(stats?.public_bucket_count), icon: Globe },
  ];

  const maxSize = Math.max(1, ...(stats?.buckets.map((b) => b.size) ?? [1]));

  return (
    <div>
      <PageHeader
        title="Dashboard"
        description="A live overview of your object storage: buckets, object counts, and disk usage."
        tutorial={
          <TutList
            items={[
              "The cards summarise totals across every bucket.",
              "The usage chart ranks buckets by bytes stored — hover a row for exact numbers.",
              "Click a bucket to open it in the Object Browser.",
              "Counts are gathered by listing each bucket, so very large stores may take a moment.",
            ]}
          />
        }
      />

      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
        {cards.map(({ label, value, icon: Icon }) => (
          <Card key={label} className="p-5">
            <div className="flex items-center justify-between">
              <span className="text-sm text-[var(--color-muted-fg)]">{label}</span>
              <Icon className="h-5 w-5 text-[var(--color-accent)]" />
            </div>
            <div className="mono mt-3 text-2xl font-semibold">{loading ? "…" : value}</div>
          </Card>
        ))}
      </div>

      <Card className="mt-6 p-5">
        <div className="mb-4 flex items-center justify-between">
          <h2 className="font-semibold">Storage usage by bucket</h2>
          <Link to="/buckets" className="focusable inline-flex items-center gap-1 rounded text-sm text-[var(--color-accent)] hover:underline">
            Manage <ArrowRight className="h-3.5 w-3.5" />
          </Link>
        </div>

        {loading ? (
          <Spinner />
        ) : !stats?.buckets.length ? (
          <EmptyState icon={<Database className="h-8 w-8" />} title="No buckets yet" hint="Create your first bucket to get started." />
        ) : (
          <div className="space-y-3">
            {[...stats.buckets]
              .sort((a, b) => b.size - a.size)
              .map((b) => (
                <Link
                  key={b.name}
                  to={`/browse?bucket=${encodeURIComponent(b.name)}`}
                  className="focusable block rounded-[var(--radius)] p-2 transition-colors hover:bg-[var(--color-surface-2)]"
                >
                  <div className="mb-1.5 flex items-center justify-between gap-3 text-sm">
                    <span className="flex items-center gap-2 truncate font-medium">
                      {b.name}
                      {b.public && <Badge tone="accent">public</Badge>}
                    </span>
                    <span className="mono shrink-0 text-[var(--color-muted-fg)]">
                      {formatBytes(b.size)} · {formatNumber(b.objects)} obj
                    </span>
                  </div>
                  <div className="h-2 overflow-hidden rounded-full bg-[var(--color-bg)]">
                    <div
                      className="h-full rounded-full bg-[var(--color-accent)] transition-[width] duration-500"
                      style={{ width: `${Math.max(2, (b.size / maxSize) * 100)}%` }}
                    />
                  </div>
                </Link>
              ))}
          </div>
        )}
      </Card>
    </div>
  );
}
