import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useState,
  type ButtonHTMLAttributes,
  type InputHTMLAttributes,
  type ReactNode,
} from "react";
import { X, Loader2, CheckCircle2, AlertTriangle, Info } from "lucide-react";

export function cn(...parts: (string | false | null | undefined)[]): string {
  return parts.filter(Boolean).join(" ");
}

// ---- Button ----

type Variant = "primary" | "secondary" | "ghost" | "danger" | "outline";
type Size = "sm" | "md" | "icon";

const variants: Record<Variant, string> = {
  primary:
    "bg-[var(--color-accent)] text-[var(--color-accent-fg)] hover:brightness-110 font-semibold shadow-[0_0_0_1px_rgba(34,197,94,0.25),0_6px_20px_-8px_rgba(34,197,94,0.6)]",
  secondary: "bg-[var(--color-elevated)] text-[var(--color-fg)] hover:bg-[#243149] border border-[var(--color-border-strong)]",
  outline: "bg-transparent text-[var(--color-fg)] border border-[var(--color-border-strong)] hover:bg-[var(--color-surface-2)]",
  ghost: "bg-transparent text-[var(--color-muted-fg)] hover:bg-[var(--color-surface-2)] hover:text-[var(--color-fg)]",
  danger: "bg-[var(--color-danger)] text-white hover:brightness-110 font-semibold",
};

const sizes: Record<Size, string> = {
  sm: "h-8 px-3 text-sm gap-1.5",
  md: "h-10 px-4 text-sm gap-2",
  icon: "h-9 w-9 justify-center",
};

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
  loading?: boolean;
}

export function Button({ variant = "secondary", size = "md", loading, className, children, disabled, ...rest }: ButtonProps) {
  return (
    <button
      className={cn(
        "focusable inline-flex items-center rounded-[var(--radius)] transition-[filter,background-color,color] duration-150 cursor-pointer select-none disabled:opacity-50 disabled:cursor-not-allowed",
        variants[variant],
        sizes[size],
        className,
      )}
      disabled={disabled || loading}
      {...rest}
    >
      {loading && <Loader2 className="h-4 w-4 animate-spin" />}
      {children}
    </button>
  );
}

// ---- Input ----

export function Input({ className, ...rest }: InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={cn(
        "focusable h-10 w-full rounded-[var(--radius)] bg-[var(--color-bg)] border border-[var(--color-border-strong)] px-3 text-sm text-[var(--color-fg)] placeholder:text-[var(--color-faint-fg)] transition-colors",
        className,
      )}
      {...rest}
    />
  );
}

export function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label className="block space-y-1.5">
      <span className="text-sm font-medium text-[var(--color-fg)]">{label}</span>
      {children}
      {hint && <span className="block text-xs text-[var(--color-faint-fg)]">{hint}</span>}
    </label>
  );
}

// ---- Card ----

export function Card({ className, children }: { className?: string; children: ReactNode }) {
  return <div className={cn("card", className)}>{children}</div>;
}

// ---- Badge ----

export function Badge({ tone = "muted", children }: { tone?: "muted" | "accent" | "danger" | "info"; children: ReactNode }) {
  const tones = {
    muted: "bg-[var(--color-surface-2)] text-[var(--color-muted-fg)] border-[var(--color-border)]",
    accent: "bg-[var(--color-accent-soft)] text-[var(--color-accent)] border-[rgba(34,197,94,0.3)]",
    danger: "bg-[var(--color-danger-soft)] text-[#fca5a5] border-[rgba(239,68,68,0.3)]",
    info: "bg-[#0d2a3a] text-[var(--color-info)] border-[rgba(56,189,248,0.3)]",
  };
  return (
    <span className={cn("inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-xs font-medium", tones[tone])}>
      {children}
    </span>
  );
}

// ---- Spinner / states ----

export function Spinner({ label }: { label?: string }) {
  return (
    <div className="flex items-center justify-center gap-2 py-12 text-[var(--color-muted-fg)]">
      <Loader2 className="h-5 w-5 animate-spin" />
      {label && <span className="text-sm">{label}</span>}
    </div>
  );
}

export function EmptyState({ icon, title, hint, action }: { icon?: ReactNode; title: string; hint?: string; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-center justify-center gap-3 py-16 text-center">
      {icon && <div className="text-[var(--color-faint-fg)]">{icon}</div>}
      <p className="text-sm font-medium text-[var(--color-fg)]">{title}</p>
      {hint && <p className="max-w-sm text-sm text-[var(--color-faint-fg)]">{hint}</p>}
      {action}
    </div>
  );
}

// ---- Modal ----

export function Modal({
  open,
  onClose,
  title,
  description,
  children,
  footer,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  description?: string;
  children?: ReactNode;
  footer?: ReactNode;
}) {
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center p-4">
      <div className="absolute inset-0 bg-black/60 backdrop-blur-sm animate-fade" onClick={onClose} />
      <div className="card relative z-10 w-full max-w-lg p-5 animate-pop">
        <div className="mb-4 flex items-start justify-between gap-4">
          <div>
            <h2 className="text-lg font-semibold">{title}</h2>
            {description && <p className="mt-1 text-sm text-[var(--color-muted-fg)]">{description}</p>}
          </div>
          <Button variant="ghost" size="icon" onClick={onClose} aria-label="Close">
            <X className="h-4 w-4" />
          </Button>
        </div>
        <div className="space-y-4">{children}</div>
        {footer && <div className="mt-6 flex justify-end gap-2">{footer}</div>}
      </div>
    </div>
  );
}

// ---- Toasts ----

type Toast = { id: number; kind: "success" | "error" | "info"; message: string };
const ToastCtx = createContext<(kind: Toast["kind"], message: string) => void>(() => {});
export const useToast = () => useContext(ToastCtx);

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const push = useCallback((kind: Toast["kind"], message: string) => {
    const id = Date.now() + Math.random();
    setToasts((t) => [...t, { id, kind, message }]);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 4000);
  }, []);

  const icon = { success: CheckCircle2, error: AlertTriangle, info: Info };
  const color = { success: "var(--color-accent)", error: "var(--color-danger)", info: "var(--color-info)" };

  return (
    <ToastCtx.Provider value={push}>
      {children}
      <div className="fixed bottom-4 right-4 z-[100] flex w-80 flex-col gap-2" aria-live="polite">
        {toasts.map((t) => {
          const Icon = icon[t.kind];
          return (
            <div key={t.id} className="card flex items-start gap-3 p-3 animate-pop shadow-xl">
              <Icon className="mt-0.5 h-5 w-5 shrink-0" style={{ color: color[t.kind] }} />
              <p className="text-sm text-[var(--color-fg)] break-words">{t.message}</p>
            </div>
          );
        })}
      </div>
    </ToastCtx.Provider>
  );
}

// ---- Confirm dialog hook helper ----

export function ConfirmModal({
  open,
  onClose,
  onConfirm,
  title,
  message,
  confirmLabel = "Delete",
  loading,
}: {
  open: boolean;
  onClose: () => void;
  onConfirm: () => void;
  title: string;
  message: string;
  confirmLabel?: string;
  loading?: boolean;
}) {
  return (
    <Modal
      open={open}
      onClose={onClose}
      title={title}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="danger" onClick={onConfirm} loading={loading}>
            {confirmLabel}
          </Button>
        </>
      }
    >
      <p className="text-sm text-[var(--color-muted-fg)]">{message}</p>
    </Modal>
  );
}
