// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Throughput / latency over time: an area chart with a crosshair readout, or a bare sparkline.
// One y-axis, at most two series (solid and dashed), gaps stay gaps.
import { useState, type KeyboardEvent, type PointerEvent } from "react";
import { DataPlaceholder } from "./EmptyState";
import { formatBytes, formatClock, isMeasure, plural } from "./format";
import { useSvgId } from "./SpotIllustration";
import type { TrendPoint, TrendSeries } from "./types";
import { useElementWidth } from "./useElementWidth";
import "./graphics.css";

export const formatBytesPerSecond = (v: number) => `${formatBytes(v)}/s`;
export const formatMicros = (v: number) =>
  v >= 1000 ? `${(v / 1000).toFixed(v >= 10000 ? 0 : 1)} ms` : `${Math.round(v)} µs`;
const formatPlain = (v: number) => v.toLocaleString(undefined, { maximumFractionDigits: v < 10 ? 2 : 0 });

const MAX_SERIES = 2;
const TABLE_ROWS = 60;

/** Next 1 / 2 / 2.5 / 5 step at or above `v`, for a readable axis top. */
export function niceCeil(v: number): number {
  if (v <= 0) return 1;
  const unit = 10 ** Math.floor(Math.log10(v));
  const step = [1, 2, 2.5, 5, 10].find((s) => s * unit >= v) ?? 10;
  return step * unit;
}

function defaultMaxGap(points: readonly TrendPoint[]): number {
  const steps = points.slice(1).map((p, i) => p.t - points[i].t).sort((a, b) => a - b);
  return steps.length ? steps[Math.floor(steps.length / 2)] * 3 : Infinity;
}

/** Runs of consecutive measurements; a `null` value or a long silence starts a new run. */
export function segments(points: readonly TrendPoint[], maxGapMs?: number): { t: number; v: number }[][] {
  const limit = maxGapMs ?? defaultMaxGap(points);
  const out: { t: number; v: number }[][] = [];
  let run: { t: number; v: number }[] = [];
  for (const p of points) {
    if (!isMeasure(p.v)) {
      if (run.length) out.push(run);
      run = [];
      continue;
    }
    if (run.length && p.t - run[run.length - 1].t > limit) {
      out.push(run);
      run = [];
    }
    run.push({ t: p.t, v: p.v });
  }
  if (run.length) out.push(run);
  return out;
}

export interface TrendChartProps {
  /** Names the chart, e.g. "Client throughput". */
  title: string;
  /** Up to two series sharing one unit (see `throughputSeries`, `latencySeries`). */
  series?: readonly TrendSeries[] | null;
  formatValue?: (v: number) => string;
  variant?: "area" | "sparkline";
  /** Plot height in px. Default 220 (area) or 44 (sparkline). */
  height?: number;
  /** Longest silence a line may bridge. Default: three times the median sample spacing. */
  maxGapMs?: number;
  loading?: boolean;
  className?: string;
}

export function TrendChart({
  title,
  series,
  formatValue = formatPlain,
  variant = "area",
  height,
  maxGapMs,
  loading,
  className = "",
}: TrendChartProps) {
  const spark = variant === "sparkline";
  const [ref, W] = useElementWidth(spark ? 160 : 640);
  const wash = useSvgId("ag-wash");
  const [activeT, setActiveT] = useState<number | null>(null);

  const drawn = (series ?? []).slice(0, MAX_SERIES).map((s) => {
    const points = s.points.filter((p) => Number.isFinite(p.t)).sort((a, b) => a.t - b.t);
    return { ...s, points, measured: points.filter((p) => isMeasure(p.v)) as { t: number; v: number }[] };
  });

  if (!drawn.some((s) => s.measured.length >= 2)) {
    if (spark) {
      return (
        <span role="status" className={`ag ag-spark-empty ${className}`.trim()}>
          {loading ? "Loading…" : `${title}: not enough samples`}
        </span>
      );
    }
    return (
      <DataPlaceholder
        loading={loading}
        art="chart"
        title="Not enough samples yet"
        description={`${title} needs at least two measurements.`}
        className={className}
      />
    );
  }

  const H = height ?? (spark ? 44 : 220);
  const pad = spark ? { l: 4, r: 6, t: 6, b: 4 } : { l: 4, r: 8, t: 20, b: 24 };
  const times = [...new Set(drawn.flatMap((s) => s.points.map((p) => p.t)))].sort((a, b) => a - b);
  const t0 = times[0];
  const t1 = times[times.length - 1];
  const peak = Math.max(...drawn.flatMap((s) => s.measured.map((p) => p.v)));
  const top = spark ? peak || 1 : niceCeil(peak);
  const base = H - pad.b;
  const x = (t: number) => pad.l + ((t - t0) / (t1 - t0 || 1)) * (W - pad.l - pad.r);
  const y = (v: number) => base - (v / top) * (base - pad.t);
  const ticks = peak > 0 ? [0, 0.5, 1] : [0];
  const at = (p: { t: number; v: number }) => `${x(p.t).toFixed(1)} ${y(p.v).toFixed(1)}`;

  const summary = `${title}. ${drawn
    .map((s) => {
      if (!s.measured.length) return `${s.label}: no samples.`;
      const values = s.measured.map((p) => p.v);
      const last = s.measured[s.measured.length - 1];
      return (
        `${s.label}: ${formatValue(last.v)} at ${formatClock(last.t)}, ` +
        `range ${formatValue(Math.min(...values))} to ${formatValue(Math.max(...values))}.`
      );
    })
    .join(" ")} From ${formatClock(t0)} to ${formatClock(t1)}.`;

  const valueAt = (points: readonly TrendPoint[], t: number) => {
    const v = points.find((p) => p.t === t)?.v;
    return isMeasure(v) ? v : null;
  };

  const plot = (
    <svg className="ag-trend-svg" width="100%" height={H} viewBox={`0 0 ${W} ${H}`} role="img" aria-label={summary}>
      <defs>
        <linearGradient id={wash} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" className="ag-stop-wash-top" />
          <stop offset="1" className="ag-stop-wash-bottom" />
        </linearGradient>
      </defs>
      {!spark &&
        ticks.map((f) => (
          <line key={f} className="ag-grid" x1={pad.l} x2={W - pad.r} y1={y(top * f)} y2={y(top * f)} />
        ))}
      {drawn.map((s, i) => {
        const last = s.measured[s.measured.length - 1];
        return (
          <g key={s.id} className={`ag-series ag-series-${i}`} data-series={s.id}>
            {segments(s.points, maxGapMs).map((run) => {
              if (run.length === 1) {
                return <circle key={run[0].t} className="ag-point" cx={x(run[0].t)} cy={y(run[0].v)} r="2.5" />;
              }
              const line = `M${run.map(at).join("L")}`;
              return (
                <g key={run[0].t}>
                  {i === 0 && (
                    <path
                      className="ag-area"
                      fill={`url(#${wash})`}
                      d={`${line}L${x(run[run.length - 1].t).toFixed(1)} ${base}L${x(run[0].t).toFixed(1)} ${base}Z`}
                    />
                  )}
                  <path className="ag-line" d={line} />
                </g>
              );
            })}
            {last && <circle className="ag-end" cx={x(last.t)} cy={y(last.v)} r={spark ? 3 : 4} />}
          </g>
        );
      })}
      {!spark && (
        <g>
          {/* Drawn after the series, with a surface halo, so a line never strikes through a label. */}
          {ticks.map((f) => (
            <text key={f} className="ag-axis" x={pad.l} y={y(top * f) - 5}>
              {formatValue(top * f)}
            </text>
          ))}
          <text className="ag-axis" x={pad.l} y={H - 6}>
            {formatClock(t0)}
          </text>
          <text className="ag-axis" x={W - pad.r} y={H - 6} textAnchor="end">
            {formatClock(t1)}
          </text>
        </g>
      )}
      {!spark && activeT !== null && times.includes(activeT) && (
        <g data-testid="crosshair">
          <line className="ag-crosshair" x1={x(activeT)} x2={x(activeT)} y1={pad.t} y2={base} />
          {drawn.map((s, i) => {
            const v = valueAt(s.points, activeT);
            return v === null ? null : (
              <circle key={s.id} className={`ag-end ag-series-${i}`} cx={x(activeT)} cy={y(v)} r="4.5" />
            );
          })}
        </g>
      )}
    </svg>
  );

  if (spark) {
    return (
      <div ref={ref} className={`ag ag-trend ag-trend-sparkline ${className}`.trim()}>
        {plot}
      </div>
    );
  }

  const readT = activeT !== null && times.includes(activeT) ? activeT : t1;
  const extra = (series?.length ?? 0) - drawn.length;

  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    if (!box.width) return;
    const px = ((e.clientX - box.left) / box.width) * W;
    let best = times[0];
    for (const t of times) if (Math.abs(x(t) - px) < Math.abs(x(best) - px)) best = t;
    setActiveT(best);
  };
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const i = times.indexOf(readT);
    const next =
      e.key === "ArrowLeft"
        ? times[Math.max(0, i - 1)]
        : e.key === "ArrowRight"
          ? times[Math.min(times.length - 1, i + 1)]
          : e.key === "Home"
            ? t0
            : e.key === "End"
              ? t1
              : e.key === "Escape"
                ? null
                : undefined;
    if (next === undefined) return;
    e.preventDefault();
    setActiveT(next);
  };

  return (
    <figure className={`ag ag-trend ag-trend-area ${className}`.trim()}>
      <figcaption className="ag-trend-head">
        <span className="ag-trend-title">{title}</span>
        <span className="ag-trend-readout" aria-live={activeT === null ? "off" : "polite"}>
          <span className="ag-muted">
            {activeT === null ? "Latest " : "At "}
            {formatClock(readT)}
          </span>
          {drawn.map((s, i) => {
            const v = valueAt(s.points, readT);
            return (
              <span key={s.id} className="ag-trend-key">
                <svg className={`ag-key ag-series-${i}`} width="18" height="8" aria-hidden="true">
                  <line className="ag-line" x1="1" y1="4" x2="17" y2="4" />
                </svg>
                <span>{s.label}</span>
                <strong>{v === null ? "no sample" : formatValue(v)}</strong>
              </span>
            );
          })}
        </span>
      </figcaption>
      <div
        ref={ref}
        className="ag-trend-plot"
        tabIndex={0}
        role="group"
        aria-label={`${title}: left and right arrow keys step through samples`}
        onPointerMove={onPointerMove}
        onPointerLeave={() => setActiveT(null)}
        onKeyDown={onKeyDown}
        onBlur={() => setActiveT(null)}
      >
        {plot}
      </div>
      {extra > 0 && <p className="ag-muted">{plural(extra, "more series", "more series")} not shown</p>}
      <details className="ag-table">
        <summary>Data table</summary>
        {times.length > TABLE_ROWS && <p className="ag-muted">Latest {TABLE_ROWS} samples</p>}
        <div className="ag-table-scroll">
          <table>
            <thead>
              <tr>
                <th scope="col">Time</th>
                {drawn.map((s) => (
                  <th key={s.id} scope="col">
                    {s.label}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {times.slice(-TABLE_ROWS).map((t) => (
                <tr key={t}>
                  <th scope="row">{formatClock(t)}</th>
                  {drawn.map((s) => {
                    const v = valueAt(s.points, t);
                    return <td key={s.id}>{v === null ? "no sample" : formatValue(v)}</td>;
                  })}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </details>
    </figure>
  );
}
