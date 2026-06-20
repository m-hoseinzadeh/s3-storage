import { createContext, useContext, useEffect, useState, type ReactNode } from "react";
import { api } from "../lib/api";

interface AuthState {
  ready: boolean;
  accessKey: string | null;
  setAccessKey: (k: string | null) => void;
}

const Ctx = createContext<AuthState>({ ready: false, accessKey: null, setAccessKey: () => {} });
export const useAuth = () => useContext(Ctx);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [ready, setReady] = useState(false);
  const [accessKey, setAccessKey] = useState<string | null>(null);

  useEffect(() => {
    api
      .session()
      .then((s) => setAccessKey(s.access_key))
      .catch(() => setAccessKey(null))
      .finally(() => setReady(true));
  }, []);

  return <Ctx.Provider value={{ ready, accessKey, setAccessKey }}>{children}</Ctx.Provider>;
}
