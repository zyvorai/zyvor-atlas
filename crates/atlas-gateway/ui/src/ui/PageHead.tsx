// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import type { ReactNode } from "react";
import { Link } from "react-router-dom";

export type PageCrumb = { label: ReactNode; to?: string };

/** Shop page grammar: optional crumbs → eyebrow → large title → muted state. */
export function PageHead({
  crumbs,
  eyebrow,
  title,
  state,
  actions,
}: {
  crumbs?: PageCrumb[];
  eyebrow: ReactNode;
  title: ReactNode;
  state?: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <div className="at-head">
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
        <div className="at-eyebrow">
          <span className="tick" />
          {eyebrow}
        </div>
        <h1 className="at-title">{title}</h1>
        {state != null && state !== false ? <p className="at-state">{state}</p> : null}
      </div>
      {actions ? <div className="at-head-actions">{actions}</div> : null}
    </div>
  );
}
