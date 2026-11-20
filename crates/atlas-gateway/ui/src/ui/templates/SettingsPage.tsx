// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Apple System Settings-style preference pane: a panel of labelled radio-tile blocks.
// See docs/ATLAS_UI_CONTRACT.md's page-archetypes table ("Reference (settings)").
import type { ReactNode } from "react";
import { PageHead, type PageCrumb } from "../PageHead";
import { cx } from "../../lib/format";

export interface SettingsChoice<T extends string = string> {
  id: T;
  title: string;
  hint: string;
}

export interface SettingsBlock<T extends string = string> {
  label: string;
  hint: ReactNode;
  value: T;
  onChange: (v: T) => void;
  options: SettingsChoice<T>[];
  ariaLabel: string;
  /** Grid width for the option tiles — defaults to the option count, capped at 3. */
  columns?: 2 | 3;
}

export interface SettingsPageProps {
  crumbs?: PageCrumb[];
  eyebrow: ReactNode;
  title: ReactNode;
  state?: ReactNode;
  /** Caption on the panel bar above the blocks, e.g. "Appearance". */
  panelLabel: string;
  blocks: SettingsBlock<any>[];
  /** Escape hatch for content that isn't a radio-tile block (e.g. a form or table). */
  children?: ReactNode;
}

export function SettingsPage({ crumbs, eyebrow, title, state, panelLabel, blocks, children }: SettingsPageProps) {
  return (
    <div className="at-stack">
      <PageHead crumbs={crumbs} eyebrow={eyebrow} title={title} state={state} />

      <div className="at-panel">
        <div className="at-panel-bar">
          <span className="at-caption">{panelLabel}</span>
        </div>
        <div className="at-settings-body">
          {blocks.map((block) => (
            <div key={block.label} className="at-settings-block">
              <div className="at-settings-label">{block.label}</div>
              <p className="at-settings-hint">{block.hint}</p>
              <div
                className={cx("at-choice-grid", (block.columns ?? block.options.length) === 2 && "at-choice-grid-2")}
                role="radiogroup"
                aria-label={block.ariaLabel}
              >
                {block.options.map((opt) => (
                  <button
                    key={opt.id}
                    type="button"
                    role="radio"
                    aria-checked={block.value === opt.id}
                    className={cx("at-choice", block.value === opt.id && "on")}
                    onClick={() => block.onChange(opt.id)}
                  >
                    <span className="at-choice-title">{opt.title}</span>
                    <span className="at-choice-hint">{opt.hint}</span>
                  </button>
                ))}
              </div>
            </div>
          ))}
        </div>
      </div>

      {children}
    </div>
  );
}
