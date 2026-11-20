// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter } from "react-router-dom";
import App from "./App";
import { Login } from "./views/Login";
import { useUi } from "./store/ui";
import type { NavRole } from "./lib/auth";
import { Toaster } from "./ui/Toaster";
import { ConfirmHost } from "./ui/confirm";
import "./index.css";

// After a successful OIDC round-trip, `/auth/oidc/callback` (server-side) redirects the browser
// back to `/?atlas_token=...&atlas_role=...` — this is a same-origin SPA reload, so pick the
// token up here once on mount, feed it into the same store the password-login flow uses, and
// strip it from the URL immediately (it's a bearer credential; it shouldn't linger in browser
// history or get echoed in a Referer header on the next outbound request).
function useOidcTokenFromUrl() {
  React.useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const token = params.get("atlas_token");
    if (!token) return;
    const roleParam = params.get("atlas_role");
    useUi.getState().setToken(token, true);
    if (roleParam === "admin" || roleParam === "operator" || roleParam === "viewer") {
      useUi.getState().setRole(roleParam as NavRole, true);
    }
    useUi.getState().enter(true);
    params.delete("atlas_token");
    params.delete("atlas_role");
    const rest = params.toString();
    window.history.replaceState({}, "", window.location.pathname + (rest ? `?${rest}` : ""));
  }, []);
}

function Root() {
  useOidcTokenFromUrl();
  const entered = useUi((s) => s.entered);
  return entered ? <App /> : <Login />;
}

const qc = new QueryClient({
  defaultOptions: { queries: { staleTime: 3000, refetchOnWindowFocus: false, retry: 1 } },
});

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <QueryClientProvider client={qc}>
      <BrowserRouter>
        <Root />
        <Toaster />
        <ConfirmHost />
      </BrowserRouter>
    </QueryClientProvider>
  </React.StrictMode>,
);
