// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Latency heatmap for the Observatory. Cells are the atlas-io agent's /io/heatmap columns,
// proxied by the gateway as GET /api/atlas/v1/io/heatmap. A cell shows the busiest log2-µs
// bucket in that minute, by colour and by bar height. An unconfigured agent says unavailable;
// it never looks healthy.
import { useIoHeatmap } from "../api/hooks";
import type { IoHeatSeries } from "../api/types";
import { EmptyState } from "../graphics";
import "./IoHeatmap.css";

const BANDS = [
  { id: "fast", label: "Under 1 ms" },
  { id: "slow", label: "1 to 10 ms" },
  { id: "slower", label: "10 ms or more" },
  { id: "none", label: "No I/O" },
] as const;

function band(peakUs: number): (typeof BANDS)[number]["id"] {
  if (peakUs <= 0) return "none";
  if (peakUs < 1_000) return "fast";
  if (peakUs < 10_000) return "slow";
  return "slower";
}

function latency(us: number): string {
  if (us >= 1000) return `${(us / 1000).toFixed(us >= 10_000 ? 0 : 1)} ms`;
  return `${us} µs`;
}

function describe(s: IoHeatSeries): string {
  const total = s.columns.reduce((sum, c) => sum + c.count, 0);
  const peak = Math.max(0, ...s.columns.map((c) => c.peak_us));
  return `${s.device} ${s.op} latency: ${total} I/Os over ${s.columns.length} minutes, peak ${peak > 0 ? latency(peak) : "none"}`;
}

export default function IoHeatmap() {
  const q = useIoHeatmap();
  const report = q.data;
  const series = report?.status === "ok" ? report.series : [];

  return (
    <div className="at-panel hm">
      {q.isLoading && !report && <p className="hm-note">Loading heatmap…</p>}
      {report?.status === "unavailable" && (
        <EmptyState art="chart" title="Heatmap unavailable" description={report.detail ?? undefined} compact />
      )}
      {q.isError && !report && <EmptyState art="chart" title="Heatmap unavailable" compact />}
      {report?.status === "ok" && series.length === 0 && (
        <EmptyState art="chart" title="No I/O in the last 15 minutes" compact />
      )}
      {series.length > 0 && (
        <>
          <ul className="hm-legend" aria-label="Peak latency per minute">
            {BANDS.map((b) => (
              <li key={b.id}>
                <i className="hm-cell" data-band={b.id} aria-hidden />
                {b.label}
              </li>
            ))}
          </ul>
          {series.map((s) => (
            <div key={`${s.device}-${s.op}`} className="hm-row">
              <div className="hm-name mono">
                {s.device}
                <span className="at-tag">{s.op}</span>
              </div>
              <div className="hm-cells" role="img" aria-label={describe(s)}>
                {s.columns.map((c) => (
                  <i
                    key={c.start_unix}
                    className="hm-cell"
                    data-band={band(c.peak_us)}
                    title={`${c.count} I/Os, peak ${latency(c.peak_us)}`}
                  />
                ))}
              </div>
            </div>
          ))}
        </>
      )}
    </div>
  );
}
