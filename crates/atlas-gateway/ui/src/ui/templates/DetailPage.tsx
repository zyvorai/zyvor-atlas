// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Apple "Configurator" detail/drill-down page: breadcrumb-heavy header, an optional instrument
// (stat-tile) strip, then free-form content — typically `DashboardModule` blocks (see
// DashboardHero.tsx) for static sections, or a stage rail + active panel for a wizard.
// See docs/ATLAS_UI_CONTRACT.md's page-archetypes table ("Detail").
import type { CSSProperties, ReactNode } from "react";
import { PageHead, type PageCrumb } from "../PageHead";

export interface DetailStat {
  label: ReactNode;
  value: ReactNode;
  unit?: ReactNode;
  delta?: ReactNode;
  style?: CSSProperties;
}

/** The repeated `.at-instrs` stat-tile strip (used at the top of a detail page). */
export function DetailStats({ items }: { items: DetailStat[] }) {
  return (
    <div className="at-instrs">
      {items.map((it, i) => (
        <div key={i} className="at-instr">
          <div className="at-caption">{it.label}</div>
          <div className="at-val md" style={it.style}>
            {it.value}
            {it.unit != null ? <span className="at-unit">{it.unit}</span> : null}
          </div>
          {it.delta != null ? <div className="at-delta">{it.delta}</div> : null}
        </div>
      ))}
    </div>
  );
}

export interface DetailPageProps {
  crumbs?: PageCrumb[];
  eyebrow: ReactNode;
  title: ReactNode;
  state?: ReactNode;
  actions?: ReactNode;
  stats?: DetailStat[];
  /** Passed through to the root div — e.g. "at-stack" for pages that space their body with flex+gap
   * instead of `.at-mod` module blocks (PlanDetail's wizard). */
  className?: string;
  children?: ReactNode;
}

export function DetailPage({ crumbs, eyebrow, title, state, actions, stats, className, children }: DetailPageProps) {
  return (
    <div className={className}>
      <PageHead crumbs={crumbs} eyebrow={eyebrow} title={title} state={state} actions={actions} />
      {stats ? <DetailStats items={stats} /> : null}
      {children}
    </div>
  );
}
