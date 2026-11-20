// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Apple Store catalog/browse-style index page: header, then a filter row and a content panel
// (table, nested browser, etc.) supplied by the page — this template owns layout only, not the
// table implementation, since list pages vary too much (soundings Table, hand-rolled sortable
// tables with bulk actions, nested browsers) to share one data-table abstraction.
// See docs/ATLAS_UI_CONTRACT.md's page-archetypes table ("Catalog index / Timeline").
import type { ReactNode } from "react";
import { PageHead, type PageCrumb } from "../PageHead";

export interface ListPageProps {
  crumbs?: PageCrumb[];
  eyebrow: ReactNode;
  title: ReactNode;
  state?: ReactNode;
  actions?: ReactNode;
  /** Passed through to the root div — e.g. "at-stack" for pages that space their body with flex+gap. */
  className?: string;
  children: ReactNode;
}

export function ListPage({ crumbs, eyebrow, title, state, actions, className, children }: ListPageProps) {
  return (
    <div className={className}>
      <PageHead crumbs={crumbs} eyebrow={eyebrow} title={title} state={state} actions={actions} />
      {children}
    </div>
  );
}
