import { useState, type ReactNode } from "react";
import { GraduationCap, ChevronDown } from "lucide-react";
import { cn } from "./ui";

// Standard page header: title + one-line description + a collapsible tutorial,
// satisfying the "every page has a description and tutorial" requirement.
export function PageHeader({
  title,
  description,
  tutorial,
  actions,
}: {
  title: string;
  description: string;
  tutorial: ReactNode;
  actions?: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  return (
    <div className="mb-6">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">{title}</h1>
          <p className="mt-1 max-w-2xl text-sm text-[var(--color-muted-fg)]">{description}</p>
        </div>
        <div className="flex items-center gap-2">{actions}</div>
      </div>

      <button
        onClick={() => setOpen((v) => !v)}
        className="focusable mt-3 inline-flex items-center gap-2 rounded-md px-2 py-1 text-xs font-medium text-[var(--color-accent)] hover:bg-[var(--color-accent-soft)] cursor-pointer"
      >
        <GraduationCap className="h-4 w-4" />
        {open ? "Hide tutorial" : "Show tutorial"}
        <ChevronDown className={cn("h-3.5 w-3.5 transition-transform", open && "rotate-180")} />
      </button>

      {open && (
        <div className="card mt-2 max-w-3xl space-y-2 p-4 text-sm leading-relaxed text-[var(--color-muted-fg)] animate-pop">
          {tutorial}
        </div>
      )}
    </div>
  );
}

export function Tut({ children }: { children: ReactNode }) {
  return <p>{children}</p>;
}

export function TutList({ items }: { items: ReactNode[] }) {
  return (
    <ol className="ml-4 list-decimal space-y-1">
      {items.map((it, i) => (
        <li key={i}>{it}</li>
      ))}
    </ol>
  );
}
