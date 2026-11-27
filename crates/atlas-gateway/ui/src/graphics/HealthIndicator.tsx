// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// A state is always shape + word + colour, never colour alone.
import type { ClusterHealthState, Health } from "../api/types";
import "./graphics.css";

export type HealthValue = Health | ClusterHealthState;
export type Tone = "ok" | "info" | "warn" | "fail" | "unknown";

const STATES: Record<HealthValue, { tone: Tone; word: string }> = {
  ok: { tone: "ok", word: "Healthy" },
  healthy: { tone: "ok", word: "Healthy" },
  warn: { tone: "warn", word: "Warning" },
  degraded: { tone: "warn", word: "Degraded" },
  rebuilding: { tone: "info", word: "Rebuilding" },
  at_risk: { tone: "fail", word: "At risk" },
  critical: { tone: "fail", word: "Critical" },
  unknown: { tone: "unknown", word: "Unknown" },
};

export function healthMeta(health?: string | null): { tone: Tone; word: string } {
  if (!health) return { tone: "unknown", word: "Not reported" };
  return Object.hasOwn(STATES, health) ? STATES[health as HealthValue] : STATES.unknown;
}

/** The bare 16×16 state glyph. Decorative: always pair it with the state's word. */
export function StatusGlyph({ tone, size = 16, x, y }: { tone: Tone; size?: number; x?: number; y?: number }) {
  return (
    <svg
      className={`ag-glyph ag-tone-${tone}`}
      data-glyph={tone}
      x={x}
      y={y}
      width={size}
      height={size}
      viewBox="0 0 16 16"
      aria-hidden="true"
      focusable="false"
    >
      {tone === "ok" && (
        <>
          <circle className="ag-glyph-shape" cx="8" cy="8" r="7.5" />
          <path className="ag-glyph-mark" d="M4.6 8.3l2.3 2.3 4.5-4.8" />
        </>
      )}
      {tone === "info" && (
        <>
          <circle className="ag-glyph-shape" cx="8" cy="8" r="7.5" />
          <path className="ag-glyph-mark" d="M11.6 8a3.6 3.6 0 1 1-1.2-2.7M10.8 3.2v2.4H8.4" />
        </>
      )}
      {tone === "warn" && (
        <>
          <path className="ag-glyph-shape" d="M8 .8 15.6 14.6H.4Z" />
          <path className="ag-glyph-mark" d="M8 5.8v3.8M8 12v.2" />
        </>
      )}
      {tone === "fail" && (
        <>
          <path className="ag-glyph-shape" d="M4.9.5h6.2l4.4 4.4v6.2l-4.4 4.4H4.9L.5 11.1V4.9Z" />
          <path className="ag-glyph-mark" d="M5.4 5.4l5.2 5.2M10.6 5.4l-5.2 5.2" />
        </>
      )}
      {tone === "unknown" && (
        <>
          <circle className="ag-glyph-hollow" cx="8" cy="8" r="7" />
          <path className="ag-glyph-question" d="M6.1 6.2a1.9 1.9 0 1 1 2.7 1.7c-.6.3-.8.7-.8 1.4M8 11.6v.2" />
        </>
      )}
    </svg>
  );
}

export interface HealthIndicatorProps {
  /** API health or rollup state. Missing renders "Not reported". */
  health?: HealthValue | null;
  /** What is being described, e.g. "Cluster". */
  label?: string;
  /** A reason or summary shown after the state word. */
  detail?: string | null;
  size?: "sm" | "lg";
  /** Show only the glyph; the words become its accessible name and tooltip. */
  iconOnly?: boolean;
  className?: string;
}

export function HealthIndicator({
  health,
  label,
  detail,
  size = "sm",
  iconOnly = false,
  className = "",
}: HealthIndicatorProps) {
  const { tone, word } = healthMeta(health);
  const name = `${label ? `${label}: ` : ""}${word}`;
  const glyphSize = size === "lg" ? 28 : 16;
  const rootClass = `ag ag-health ag-health-${size} ag-tone-${tone} ${className}`.trim();

  if (iconOnly) {
    const full = detail ? `${name}. ${detail}` : name;
    return (
      <span className={rootClass} data-tone={tone} role="img" aria-label={full} title={full}>
        <StatusGlyph tone={tone} size={glyphSize} />
      </span>
    );
  }
  return (
    <span className={rootClass} data-tone={tone}>
      <span className="ag-health-icon">
        <StatusGlyph tone={tone} size={glyphSize} />
      </span>
      <span className="ag-health-text">
        <span className="ag-health-word">{name}</span>
        {detail && <span className="ag-health-detail">{detail}</span>}
      </span>
    </span>
  );
}
