// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { num } from "../lib/format";

export type ThroughputSample = { readIops?: number; writeIops?: number };

const W = 600;
const H = 150;
const TOP = 8;

/**
 * IOPS drawn from the samples that were actually measured. A rate is the difference between two
 * counter readings, so the first sample of the history has none and is left out; with fewer than
 * two measured samples the chart says so instead of drawing anything.
 */
export function measuredSamples(hist: ThroughputSample[]): ThroughputSample[] {
  return hist.slice(1);
}

function path(values: number[], max: number): string {
  const step = W / (values.length - 1);
  return values
    .map((v, i) => `${i ? "L" : "M"}${(i * step).toFixed(1)} ${(H - (v / max) * (H - TOP)).toFixed(1)}`)
    .join("");
}

/** Read and write IOPS over the history window, on one shared scale. */
export function ThroughputChart({ hist = [], wide = false }: { hist?: ThroughputSample[]; wide?: boolean }) {
  const samples = measuredSamples(hist);
  if (samples.length < 2) {
    return <div className="a-chart-empty">Collecting samples. Throughput appears after the next readings.</div>;
  }
  const reads = samples.map((s) => s.readIops || 0);
  const writes = samples.map((s) => s.writeIops || 0);
  const peak = Math.max(...reads, ...writes);
  const max = peak || 1;
  const read = path(reads, max);
  const lastRead = Math.round(reads[reads.length - 1]);
  const lastWrite = Math.round(writes[writes.length - 1]);

  return (
    <div className={wide ? "a-chart wide" : "a-chart"}>
      <svg
        viewBox={`0 0 ${W} ${H}`}
        preserveAspectRatio="none"
        role="img"
        aria-label={`IOPS over ${samples.length} samples: latest ${lastRead} read, ${lastWrite} write, peak ${Math.round(peak)}`}
      >
        {[0.25, 0.5, 0.75].map((f) => (
          <line key={f} className="grid-line" x1="0" x2={W} y1={H * f} y2={H * f} />
        ))}
        <line className="grid-line" x1="0" x2={W} y1={H - 0.5} y2={H - 0.5} />
        <path className="area" d={`${read}L${W} ${H}L0 ${H}Z`} />
        <path className="line read" d={read} />
        <path className="line write" d={path(writes, max)} />
      </svg>
      <div className="a-chart-legend">
        <span className="at-io-leg">
          <span className="sw" style={{ background: "var(--a-blue)" }} />
          Read {num(lastRead)} IOPS
        </span>
        <span className="at-io-leg">
          <span className="sw" style={{ background: "var(--a-series-2)" }} />
          Write {num(lastWrite)} IOPS
        </span>
        <span className="peak">Peak {num(Math.round(peak))}</span>
      </div>
    </div>
  );
}
