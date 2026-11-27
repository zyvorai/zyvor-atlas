// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { DataPlaceholder } from "./EmptyState";
import {
  FILL_WORD,
  fillLevel,
  formatBytes,
  formatPercent,
  isMeasure,
  percentOf,
  plural,
  type FillThresholds,
} from "./format";
import { healthMeta, StatusGlyph } from "./HealthIndicator";
import type { PoolUsage } from "./types";
import "./graphics.css";

function poolPercent(p: PoolUsage): number | null {
  return isMeasure(p.percentUsed) ? p.percentUsed : percentOf(p.usedBytes, p.capacityBytes);
}

function poolDetail(p: PoolUsage): string {
  const used = isMeasure(p.usedBytes) ? formatBytes(p.usedBytes) : null;
  const capacity = isMeasure(p.capacityBytes) && p.capacityBytes > 0 ? formatBytes(p.capacityBytes) : null;
  const available = isMeasure(p.availableBytes) ? formatBytes(p.availableBytes) : null;
  if (used && capacity) return `${used} of ${capacity}`;
  const usedText = used ? `${used} used` : "usage not reported";
  if (available) return `${usedText} · ${available} available`;
  if (capacity) return `${usedText} · ${capacity} total`;
  return used ? `${usedText} · capacity not reported` : usedText;
}

export interface PoolUsageBarsProps extends FillThresholds {
  /** See `poolUsageFromPools` and `poolUsageFromCephDf`. */
  pools?: readonly PoolUsage[] | null;
  /** Accessible name of the list. Default "Pool usage". */
  label?: string;
  /** Show only the fullest N pools; the rest are counted in text. */
  limit?: number;
  /** Makes each row a button. */
  onSelect?: (pool: PoolUsage) => void;
  loading?: boolean;
  className?: string;
}

export function PoolUsageBars({
  pools,
  label = "Pool usage",
  limit,
  onSelect,
  warnAt,
  failAt,
  loading,
  className = "",
}: PoolUsageBarsProps) {
  if (!pools || pools.length === 0) {
    return (
      <DataPlaceholder
        loading={loading}
        art="pool"
        title="No pools reported"
        description="Pools appear here once discovery returns them."
        className={className}
      />
    );
  }

  // Fullest first; pools whose fill is unknown go last rather than posing as empty.
  const rows = pools
    .map((pool) => ({ pool, pct: poolPercent(pool) }))
    .sort((a, b) => (b.pct ?? -1) - (a.pct ?? -1) || a.pool.name.localeCompare(b.pool.name));
  const shown = limit && limit > 0 ? rows.slice(0, limit) : rows;
  const rest = rows.length - shown.length;

  return (
    <div className={`ag ag-pools ${className}`.trim()}>
      <ul aria-label={label}>
        {shown.map(({ pool, pct }) => {
          const level = pct === null ? "normal" : fillLevel(pct, { warnAt, failAt });
          const word = level === "normal" ? null : FILL_WORD[level];
          const health = pool.health === "warn" || pool.health === "critical" ? healthMeta(pool.health) : null;
          const body = (
            <>
              <span className="ag-pool-head">
                <span className="ag-pool-name">{pool.name}</span>
                <span className="ag-pool-pct">{pct === null ? "Fill unknown" : formatPercent(pct)}</span>
              </span>
              <span
                className={`ag-bar${pct === null ? " ag-bar-unknown" : ""}`}
                aria-hidden="true"
                title={pct === null ? "Fill level not reported" : `${pool.name}: ${formatPercent(pct)} full`}
              >
                {pct !== null && pct > 0 && (
                  <span className="ag-bar-fill" style={{ width: `${Math.max(1, Math.min(100, pct))}%` }} />
                )}
              </span>
              <span className="ag-pool-foot">
                <span className="ag-muted">{poolDetail(pool)}</span>
                {word && (
                  <span className="ag-flag">
                    <StatusGlyph tone={level === "fail" ? "fail" : "warn"} size={12} />
                    {word}
                  </span>
                )}
                {health && (
                  <span className="ag-flag">
                    <StatusGlyph tone={health.tone} size={12} />
                    Health: {health.word}
                  </span>
                )}
              </span>
            </>
          );
          return (
            <li key={pool.id} className={`ag-pool ag-level-${level}`} data-level={level}>
              {onSelect ? (
                <button type="button" className="ag-pool-row ag-pool-button" onClick={() => onSelect(pool)}>
                  {body}
                </button>
              ) : (
                <div className="ag-pool-row">{body}</div>
              )}
            </li>
          );
        })}
      </ul>
      {rest > 0 && <p className="ag-muted ag-pools-rest">{plural(rest, "more pool")} not shown</p>}
    </div>
  );
}
