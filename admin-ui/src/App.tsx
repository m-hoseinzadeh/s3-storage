import { Navigate, Route, Routes } from "react-router-dom";
import { useAuth } from "./context/auth";
import { Layout } from "./components/Layout";
import { Spinner } from "./components/ui";
import { Login } from "./pages/Login";
import { Dashboard } from "./pages/Dashboard";
import { Buckets } from "./pages/Buckets";
import { Browser } from "./pages/Browser";
import { Multipart } from "./pages/Multipart";
import { Settings } from "./pages/Settings";
import type { ReactNode } from "react";

function RequireAuth({ children }: { children: ReactNode }) {
  const { ready, accessKey } = useAuth();
  if (!ready) return <div className="grid min-h-dvh place-items-center"><Spinner label="Loading…" /></div>;
  if (!accessKey) return <Navigate to="/login" replace />;
  return <>{children}</>;
}

export function App() {
  return (
    <Routes>
      <Route path="/login" element={<Login />} />
      <Route
        element={
          <RequireAuth>
            <Layout />
          </RequireAuth>
        }
      >
        <Route index element={<Dashboard />} />
        <Route path="buckets" element={<Buckets />} />
        <Route path="browse" element={<Browser />} />
        <Route path="multipart" element={<Multipart />} />
        <Route path="settings" element={<Settings />} />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
