// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Apple Developer docs-style reference page: a header + a sequence of captioned panels, each
// either free-form prose/code or a table of endpoint-shaped rows.
// See docs/ATLAS_UI_CONTRACT.md's page-archetypes table ("Reference (docs)").
import type { ReactNode } from "react";
import { PageHead, type PageCrumb } from "../PageHead";
import { cx } from "../../lib/format";

export interface DocsRow {
  method: string;
  path: string;
  note: ReactNode;
}

export interface DocsSection {
  title: string;
  /** Free-form content — prose, TerminalPane, etc. Rendered above `rows` when both are given. */
  children?: ReactNode;
  rows?: DocsRow[];
}

export interface DocsPageProps {
  crumbs?: PageCrumb[];
  eyebrow: ReactNode;
  title: ReactNode;
  state?: ReactNode;
  sections: DocsSection[];
}

export function DocsPage({ crumbs, eyebrow, title, state, sections }: DocsPageProps) {
  return (
    <div className="at-stack">
      <PageHead crumbs={crumbs} eyebrow={eyebrow} title={title} state={state} />

      {sections.map((sec) => (
        <div key={sec.title} className="at-panel">
          <div className="at-panel-bar">
            <span className="at-caption">{sec.title}</span>
          </div>
          {sec.children ? <div className="at-docs-body">{sec.children}</div> : null}
          {sec.rows ? (
            <div className="at-docs-table">
              {sec.rows.map((row) => (
                <div key={`${row.method}-${row.path}`} className="at-docs-row">
                  <span className={cx("at-docs-method", row.method.toLowerCase())}>{row.method}</span>
                  <code className="at-docs-path mono">{row.path}</code>
                  <span className="at-docs-note">{row.note}</span>
                </div>
              ))}
            </div>
          ) : null}
        </div>
      ))}
    </div>
  );
}
