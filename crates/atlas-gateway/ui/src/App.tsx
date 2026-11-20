// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { Link, Route, Routes } from "react-router-dom";
import { Compass, Lock } from "lucide-react";
import { Shell } from "./shell/Shell";
import { APP_ROUTES, type NavModule } from "./nav/routes";
import { canModuleAccess } from "./nav/modules";
import { useUi } from "./store/ui";

function NotFound() {
  return (
    <div className="at-empty-box max-w-lg mx-auto mt-10">
      <Compass size={28} style={{ color: "var(--at-ink-4)" }} aria-hidden />
      <div className="at-empty-title">Page not found</div>
      <p className="at-empty-copy">There&apos;s nothing at this address.</p>
      <Link to="/" className="at-btn primary">
        Back to Overview

      </Link>
    </div>
  );
}

function AccessDenied({ module }: { module: NavModule }) {
  const label = module.minRole === "admin" ? "Admin" : module.minRole === "operator" ? "Operator" : "Viewer";
  return (
    <div className="at-empty-box max-w-lg mx-auto mt-8">
      <Lock size={28} style={{ color: "var(--at-ink-4)" }} aria-hidden />
      <div className="at-empty-title">{label} access required</div>
      <p className="at-empty-copy">
        Your session does not have permission to open <strong>{module.label}</strong>. Ask an admin to
        upgrade your role or use a token with {label.toLowerCase()} privileges.
      </p>
      <Link to="/" className="at-btn primary">
        Back to Overview

      </Link>
    </div>
  );
}

function GuardedRoute({ module }: { module: NavModule }) {
  const roleLevel = useUi((s) => s.roleLevel);
  if (!canModuleAccess(module, roleLevel)) return <AccessDenied module={module} />;
  const El = module.element!;
  return <El />;
}

export default function App() {
  return (
    <Routes>
      <Route element={<Shell />}>
        {APP_ROUTES.map((m) => (
          <Route key={m.id} path={m.path} element={<GuardedRoute module={m} />} />
        ))}
        <Route path="*" element={<NotFound />} />
      </Route>
    </Routes>
  );
}
