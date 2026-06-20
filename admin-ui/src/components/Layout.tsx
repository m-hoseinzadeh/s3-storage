import { NavLink, Outlet, useNavigate } from "react-router-dom";
import { LayoutDashboard, Database, FolderOpen, Layers, Settings, LogOut, HardDrive } from "lucide-react";
import { api } from "../lib/api";
import { useAuth } from "../context/auth";
import { Button, cn, useToast } from "./ui";

const nav = [
  { to: "/", label: "Dashboard", icon: LayoutDashboard, end: true },
  { to: "/buckets", label: "Buckets", icon: Database, end: false },
  { to: "/browse", label: "Object Browser", icon: FolderOpen, end: false },
  { to: "/multipart", label: "Multipart Uploads", icon: Layers, end: false },
  { to: "/settings", label: "Settings", icon: Settings, end: false },
];

export function Layout() {
  const { accessKey, setAccessKey } = useAuth();
  const navigate = useNavigate();
  const toast = useToast();

  const logout = async () => {
    await api.logout().catch(() => {});
    setAccessKey(null);
    toast("info", "Signed out");
    navigate("/login");
  };

  return (
    <div className="flex min-h-dvh">
      <aside className="sticky top-0 flex h-dvh w-64 shrink-0 flex-col border-r border-[var(--color-border)] bg-[var(--color-surface)]/60 backdrop-blur">
        <div className="flex items-center gap-2.5 px-5 py-5">
          <div className="grid h-9 w-9 place-items-center rounded-lg bg-[var(--color-accent-soft)] text-[var(--color-accent)]">
            <HardDrive className="h-5 w-5" />
          </div>
          <div className="leading-tight">
            <div className="font-semibold">s3-storage</div>
            <div className="text-xs text-[var(--color-faint-fg)]">admin console</div>
          </div>
        </div>

        <nav className="flex-1 space-y-1 px-3 py-2">
          {nav.map(({ to, label, icon: Icon, end }) => (
            <NavLink
              key={to}
              to={to}
              end={end}
              className={({ isActive }) =>
                cn(
                  "focusable flex items-center gap-3 rounded-[var(--radius)] px-3 py-2 text-sm font-medium transition-colors",
                  isActive
                    ? "bg-[var(--color-accent-soft)] text-[var(--color-accent)]"
                    : "text-[var(--color-muted-fg)] hover:bg-[var(--color-surface-2)] hover:text-[var(--color-fg)]",
                )
              }
            >
              <Icon className="h-[18px] w-[18px]" />
              {label}
            </NavLink>
          ))}
        </nav>

        <div className="border-t border-[var(--color-border)] p-3">
          <div className="mb-2 px-2">
            <div className="text-xs text-[var(--color-faint-fg)]">Signed in as</div>
            <div className="mono truncate text-sm text-[var(--color-fg)]" title={accessKey ?? ""}>
              {accessKey}
            </div>
          </div>
          <Button variant="ghost" size="sm" className="w-full justify-start" onClick={logout}>
            <LogOut className="h-4 w-4" />
            Sign out
          </Button>
        </div>
      </aside>

      <main className="min-w-0 flex-1">
        <div className="mx-auto max-w-7xl px-6 py-8">
          <Outlet />
        </div>
      </main>
    </div>
  );
}
