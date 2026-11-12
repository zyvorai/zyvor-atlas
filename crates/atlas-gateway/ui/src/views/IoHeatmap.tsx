// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
// Latency heatmap for the Observatory. Cells are the atlas-io agent's /io/heatmap columns,
// proxied by the gateway as GET /api/atlas/v1/io/heatmap. Colour follows the busiest log2-µs
// bucket in that minute. An unconfigured agent says unavailable; it never looks healthy.
import { useIoHeatmap } from "../api/hooks";

function cellColor(peakUs: number): string {
  if (peakUs <= 0) return "transparent";
  if (peakUs < 1_000) return "color-mix(in srgb, var(--d1, #34d399) 70%, transparent)";
  if (peakUs < 10_000) return "color-mix(in srgb, #fbbf24 75%, transparent)";
  return "color-mix(in srgb, #f87171 80%, transparent)";
}

function latency(us: number): string {
  if (us >= 1000) return `${(us / 1000).toFixed(us >= 10_000 ? 0 : 1)} ms`;
  return `${us} µs`;
}

export default function IoHeatmap() {
  const q = useIoHeatmap();
  const report = q.data;
  const series = report?.status === "ok" ? report.series : [];

  return (
    <div className="at-panel" style={{ padding: "18px 20px 16px" }}>
      <div style={{ display: "flex", justifyContent: "space-between", gap: 16, flexWrap: "wrap" }}>
        <div>
          <div className="at-caption">I/O latency heatmap</div>
          <div className="at-sub" style={{ marginTop: 4 }}>
            One cell per minute, coloured by the busiest latency bucket. From the node agent.
          </div>
        </div>
        <div className="at-sub" style={{ display: "flex", gap: 12, alignItems: "center" }}>
          <span><i style={{ display: "inline-block", width: 10, height: 10, background: cellColor(100), marginRight: 4 }} /> &lt; 1 ms</span>
          <span><i style={{ display: "inline-block", width: 10, height: 10, background: cellColor(2_000), marginRight: 4 }} /> &lt; 10 ms</span>
          <span><i style={{ display: "inline-block", width: 10, height: 10, background: cellColor(20_000), marginRight: 4 }} /> slower</span>
        </div>
      </div>
      {q.isLoading && !report && (
        <div className="at-sub" style={{ marginTop: 14 }}>Loading heatmap…</div>
      )}
      {report?.status === "unavailable" && (
        <div className="at-sub" style={{ marginTop: 14 }}>
          Heatmap unavailable{report.detail ? ` — ${report.detail}` : ""}.
        </div>
      )}
      {q.isError && !report && (
        <div className="at-sub" style={{ marginTop: 14 }}>Heatmap unavailable.</div>
      )}
      {report?.status === "ok" && series.length === 0 && (
        <div className="at-sub" style={{ marginTop: 14 }}>No I/O in the last 15 minutes.</div>
      )}
      {series.length > 0 && (
        <div style={{ marginTop: 14, display: "flex", flexDirection: "column", gap: 8 }}>
          {series.map((s) => (
            <div key={`${s.device}-${s.op}`} style={{ display: "flex", gap: 10, alignItems: "center" }}>
              <div className="mono" style={{ width: 160, flex: "0 0 160px", fontSize: 12 }}>
                {s.device}
                <span className="at-tag" style={{ marginLeft: 6 }}>{s.op}</span>
              </div>
              <div style={{ display: "flex", gap: 2, flexWrap: "wrap" }} aria-label={`${s.device} ${s.op} latency`}>
                {s.columns.map((c) => (
                  <i
                    key={c.start_unix}
                    title={`${c.count} I/Os, peak ${latency(c.peak_us)}`}
                    style={{
                      width: 14,
                      height: 18,
                      borderRadius: 2,
                      background: cellColor(c.peak_us),
                      display: "inline-block",
                    }}
                  />
                ))}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
