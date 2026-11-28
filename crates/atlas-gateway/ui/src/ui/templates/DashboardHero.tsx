// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Dashboard page: header, then a sequence of `.at-mod` module blocks (an optional headline over
// free-form content). `hero` gives the centred product-style headline (Overview); without it the
// same template serves dense ops consoles (Ceph, Maintenance, DR, Observatory).
// See docs/ATLAS_UI_CONTRACT.md's page-archetypes table ("Product hero" / "Ops console").
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
  /** Centred hero headline instead of the left-aligned page head. */
  hero?: boolean;
  children: ReactNode;
}

export function DashboardHero({ crumbs, eyebrow, title, state, actions, className, hero, children }: DashboardHeroProps) {
  return (
    <div className={className}>
      <PageHead crumbs={crumbs} eyebrow={eyebrow} title={title} state={state} actions={actions} hero={hero} />
      {children}
    </div>
  );
}

export interface DashboardModuleProps {
  /** Omit for a bare module with no `.at-modhead` headline. */
  title?: ReactNode;
  note?: ReactNode;
  className?: string;
  children: ReactNode;
}

/** The repeated `.at-mod` block: an optional headline over free-form module content. */
export function DashboardModule({ title, note, className, children }: DashboardModuleProps) {
  return (
    <div className={cx("at-mod", className)}>
      {title != null ? (
        <div className="at-modhead">
          <h2 className="at-modtitle">{title}</h2>
          <span className="at-modrule" />
          {note != null ? <span className="at-modnote">{note}</span> : null}
        </div>
      ) : null}
      {children}
    </div>
  );
}
