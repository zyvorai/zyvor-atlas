// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { FILL_WORD, fillLevel, formatBytes, formatPercent, isMeasure, percentOf, type FillThresholds } from "./format";
import { StatusGlyph } from "./HealthIndicator";
import "./graphics.css";

const R = 84;
const CIRCUMFERENCE = 2 * Math.PI * R;

export interface CapacityRingProps extends FillThresholds {
  usedBytes?: number | null;
  /** Total capacity. Missing or zero means "not reported". */
  totalBytes?: number | null;
  /** What the ring measures. Default "Capacity". */
  label?: string;
  /** Maximum diameter in px; shrinks with its container. Default 220. */
  size?: number;
  loading?: boolean;
  className?: string;
}

export function CapacityRing({
  usedBytes,
  totalBytes,
  label = "Capacity",
  size = 220,
  warnAt,
  failAt,
  loading,
  className = "",
}: CapacityRingProps) {
  const pct = percentOf(usedBytes, totalBytes);
  const level = pct === null ? "normal" : fillLevel(pct, { warnAt, failAt });
  // "0 of 0" is how an unsurveyed backend looks, not an empty one.
  const used = isMeasure(usedBytes) && !(usedBytes === 0 && totalBytes === 0) ? formatBytes(usedBytes) : null;
  const total = isMeasure(totalBytes) && totalBytes > 0 ? formatBytes(totalBytes) : null;
  const free =
    pct !== null && (totalBytes as number) >= (usedBytes as number)
      ? formatBytes((totalBytes as number) - (usedBytes as number))
      : null;

  let detail: string;
  if (loading && pct === null) detail = "Loading…";
  else if (pct !== null) detail = `${used} of ${total} used`;
  else if (used) detail = `${used} used · total not reported`;
  else if (total) detail = `${total} total · usage not reported`;
  else detail = "Capacity not reported";

  const word = level === "normal" ? null : FILL_WORD[level];
  const summary =
    pct === null ? `${label}: ${detail}` : `${label}: ${formatPercent(pct)} full, ${detail}${word ? `. ${word}` : ""}`;
  const arc = pct === null ? 0 : (Math.min(100, pct) / 100) * CIRCUMFERENCE;

  return (
    <figure className={`ag ag-ring ag-level-${level} ${className}`.trim()} data-level={level}>
      <svg
        className="ag-ring-svg"
        style={{ maxWidth: size }}
        viewBox="0 0 200 200"
        role="img"
        aria-label={summary}
      >
        <circle className={pct === null ? "ag-ring-unknown" : "ag-ring-track"} cx="100" cy="100" r={R} />
        {arc > 0 && (
          <circle
            className="ag-ring-arc"
            cx="100"
            cy="100"
            r={R}
            strokeDasharray={`${arc} ${CIRCUMFERENCE}`}
            transform="rotate(-90 100 100)"
          />
        )}
        <text className="ag-ring-value" x="100" y="108" textAnchor="middle">
          {pct === null ? "—" : formatPercent(pct)}
        </text>
        <text className="ag-ring-caption" x="100" y="134" textAnchor="middle">
          {pct === null ? "unknown" : "used"}
        </text>
      </svg>
      <figcaption className="ag-ring-text">
        <span className="ag-ring-label">{label}</span>
        <span>{detail}</span>
        {free && <span className="ag-muted">{free} free</span>}
        {word && (
          <span className="ag-flag">
            <StatusGlyph tone={level === "fail" ? "fail" : "warn"} size={14} />
            {word}
          </span>
        )}
      </figcaption>
    </figure>
  );
}
