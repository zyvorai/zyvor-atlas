// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import type { ReactNode } from "react";
import { SpotIllustration, type SpotKind } from "./SpotIllustration";
import "./graphics.css";

export interface EmptyStateProps {
  art: SpotKind;
  title: string;
  description?: ReactNode;
  /** Actions, e.g. a "Create volume" button. */
  children?: ReactNode;
  /** Smaller art and spacing, for use inside a card. */
  compact?: boolean;
  className?: string;
}

export function EmptyState({ art, title, description, children, compact = false, className = "" }: EmptyStateProps) {
  return (
    <div className={`ag ag-empty${compact ? " ag-empty-compact" : ""} ${className}`.trim()}>
      <SpotIllustration kind={art} size={compact ? 112 : 176} />
      <h3 className="ag-empty-title">{title}</h3>
      {description && <p className="ag-empty-text">{description}</p>}
      {children && <div className="ag-empty-actions">{children}</div>}
    </div>
  );
}

/** The shared "still waiting" / "nothing reported" body of the data graphics. */
export function DataPlaceholder({
  loading,
  art,
  title,
  description,
  className,
}: {
  loading?: boolean;
  art: SpotKind;
  title: string;
  description?: string;
  className?: string;
}) {
  return (
    <div role="status" className={className}>
      <EmptyState
        art={art}
        title={loading ? "Loading…" : title}
        description={loading ? undefined : description}
        compact
      />
    </div>
  );
}
