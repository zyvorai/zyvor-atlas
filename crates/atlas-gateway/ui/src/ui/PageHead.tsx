// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import type { ReactNode } from "react";
import { Link } from "react-router-dom";

export type PageCrumb = { label: ReactNode; to?: string };

/** Page head: optional crumbs, eyebrow, large headline, one-line state and actions. `hero` centres it. */
export function PageHead({
  crumbs,
  eyebrow,
  title,
  state,
  actions,
  hero,
}: {
  crumbs?: PageCrumb[];
  eyebrow: ReactNode;
  title: ReactNode;
  state?: ReactNode;
  actions?: ReactNode;
  hero?: boolean;
}) {
  return (
    <div className={hero ? "at-head hero" : "at-head"}>
      <div>
        {crumbs && crumbs.length > 0 ? (
          <nav className="at-crumbs" aria-label="Breadcrumb">
            {crumbs.map((c, i) => (
              <span key={i} className="at-crumb-wrap">
                {i > 0 ? <span className="at-crumb-sep" aria-hidden>·</span> : null}
                {c.to ? (
                  <Link to={c.to} className="at-crumb-link">
                    {c.label}
                  </Link>
                ) : (
                  <span className="at-crumb-current">{c.label}</span>
                )}
              </span>
            ))}
          </nav>
        ) : null}
        <div className="at-eyebrow">{eyebrow}</div>
        <h1 className="at-title">{title}</h1>
        {state != null && state !== false ? <p className="at-state">{state}</p> : null}
      </div>
      {actions ? <div className="at-head-actions">{actions}</div> : null}
    </div>
  );
}
