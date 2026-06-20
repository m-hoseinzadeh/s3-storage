import { useState, type FormEvent } from "react";
import { Navigate, useNavigate } from "react-router-dom";
import { Eye, EyeOff, HardDrive, KeyRound, Lock, ShieldCheck } from "lucide-react";
import { api, ApiError } from "../lib/api";
import { useAuth } from "../context/auth";
import { Button, Card, Field, Input, useToast } from "../components/ui";

export function Login() {
  const { ready, accessKey, setAccessKey } = useAuth();
  const [ak, setAk] = useState("");
  const [sk, setSk] = useState("");
  const [show, setShow] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const navigate = useNavigate();
  const toast = useToast();

  if (ready && accessKey) return <Navigate to="/" replace />;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const r = await api.login(ak, sk);
      setAccessKey(r.access_key);
      toast("success", "Welcome back");
      navigate("/");
    } catch (err) {
      setError(err instanceof ApiError ? err.message : "Login failed");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="grid min-h-dvh place-items-center px-4">
      <div className="w-full max-w-md animate-pop">
        <div className="mb-6 flex items-center gap-3">
          <div className="grid h-11 w-11 place-items-center rounded-xl bg-[var(--color-accent-soft)] text-[var(--color-accent)]">
            <HardDrive className="h-6 w-6" />
          </div>
          <div>
            <h1 className="text-xl font-bold">s3-storage admin</h1>
            <p className="text-sm text-[var(--color-faint-fg)]">Sign in with your S3 credentials</p>
          </div>
        </div>

        <Card className="p-6">
          <form onSubmit={submit} className="space-y-4">
            <Field label="Access key">
              <div className="relative">
                <KeyRound className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-[var(--color-faint-fg)]" />
                <Input
                  className="pl-9"
                  value={ak}
                  onChange={(e) => setAk(e.target.value)}
                  placeholder="S3_ACCESS_KEY"
                  autoComplete="username"
                  autoFocus
                  required
                />
              </div>
            </Field>

            <Field label="Secret key">
              <div className="relative">
                <Lock className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-[var(--color-faint-fg)]" />
                <Input
                  className="px-9"
                  type={show ? "text" : "password"}
                  value={sk}
                  onChange={(e) => setSk(e.target.value)}
                  placeholder="S3_SECRET_KEY"
                  autoComplete="current-password"
                  required
                />
                <button
                  type="button"
                  onClick={() => setShow((v) => !v)}
                  className="focusable absolute right-2 top-1/2 -translate-y-1/2 rounded p-1.5 text-[var(--color-faint-fg)] hover:text-[var(--color-fg)] cursor-pointer"
                  aria-label={show ? "Hide secret key" : "Show secret key"}
                >
                  {show ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
                </button>
              </div>
            </Field>

            {error && (
              <p role="alert" className="rounded-md bg-[var(--color-danger-soft)] px-3 py-2 text-sm text-[#fca5a5]">
                {error}
              </p>
            )}

            <Button type="submit" variant="primary" className="w-full justify-center" loading={busy}>
              Sign in
            </Button>
          </form>
        </Card>

        <p className="mt-4 flex items-center justify-center gap-1.5 text-xs text-[var(--color-faint-fg)]">
          <ShieldCheck className="h-3.5 w-3.5" />
          Credentials are verified server-side; a session cookie keeps you signed in.
        </p>
      </div>
    </div>
  );
}
