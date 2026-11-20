// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Apple product-hero / tech-specs dashboard page: header, then a sequence of `.at-mod` module
// blocks (an optional titled bar over free-form content). Covers both "full hero" pages
// (Overview) and instrument-panel-only ops consoles (Ceph, Maintenance, DR, Observatory) — the
// difference is only which modules a page composes, not the template.
// See docs/ATLAS_UI_CONTRACT.md's page-archetypes table ("Product hero" / "Instrument / Ops console").
import type { ReactNode } from "react";
import { PageHead, type PageCrumb } from "../PageHead";
import { cx } from "../../lib/format";

export interface DashboardHeroProps {
  crumbs?: PageCrumb[];
  eyebrow: ReactNode;
  title: ReactNode;
  state?: ReactNode;
  actions?: ReactNode;
  /** Passed through to the root div — e.g. "at-stack" for pages that space their body with flex+gap
   * instead of `.at-mod` module blocks (Maintenance, DR). */
  className?: string;
  children: ReactNode;
}

export function DashboardHero({ crumbs, eyebrow, title, state, actions, className, children }: DashboardHeroProps) {
  return (
    <div className={className}>
      <PageHead crumbs={crumbs} eyebrow={eyebrow} title={title} state={state} actions={actions} />
      {children}
    </div>
  );
}

export interface DashboardModuleProps {
  /** Omit for a bare module (e.g. the hero capacity module) — no `.at-modhead` label bar. */
  title?: ReactNode;
  note?: ReactNode;
  className?: string;
  children: ReactNode;
}

/** The repeated `.at-mod` block: an optional titled bar over free-form module content. */
export function DashboardModule({ title, note, className, children }: DashboardModuleProps) {
  return (
    <div className={cx("at-mod", className)}>
      {title != null ? (
        <div className="at-modhead">
          <span className="at-modtitle">{title}</span>
          <span className="at-modrule" />
          {note != null ? <span className="at-modnote">{note}</span> : null}
        </div>
      ) : null}
      {children}
    </div>
  );
}
