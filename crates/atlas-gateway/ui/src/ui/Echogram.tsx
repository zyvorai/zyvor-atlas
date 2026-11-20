// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useMemo } from "react";

export type EchoSample = { readIops?: number; writeIops?: number };

/** Reads above the axis, writes below — Soundings signature chart. */
export function Echogram({
  readOps = 0,
  writeOps = 0,
  hist = [],
  wide = false,
}: {
  readOps?: number;
  writeOps?: number;
  hist?: EchoSample[];
  wide?: boolean;
}) {
  const N = wide ? 96 : 60;
  const viewW = wide ? 960 : 600;
  const viewH = wide ? 120 : 88;
  const W = viewW / N;
  const AX = wide ? 60 : 44;
  const bars = useMemo(() => {
    const src =
      hist.length >= 8
        ? hist.slice(-N)
        : Array.from({ length: N }, (_, i) => ({
            readIops: Math.abs(Math.sin(i * 0.31)) * (readOps % 1000 || 200) + 40,
            writeIops: Math.abs(Math.cos(i * 0.19)) * (writeOps % 1000 || 280) + 50,
          }));
    const padded = [...src];
    while (padded.length < N) padded.unshift({ readIops: 0, writeIops: 0 });
    const slice = padded.slice(-N);
    const maxR = Math.max(1, ...slice.map((d) => d.readIops || 0));
    const maxW = Math.max(1, ...slice.map((d) => d.writeIops || 0));
    const amp = wide ? 48 : 34;
    return slice.map((d) => ({
      up: Math.max(2, ((d.readIops || 0) / maxR) * amp),
      dn: Math.max(2, ((d.writeIops || 0) / maxW) * amp),
    }));
  }, [hist, readOps, writeOps, N, wide]);

  const gid = wide ? "atSweepGradWide" : "atSweepGrad";

  return (
    <div className={`at-echo${wide ? " wide" : ""}`}>
      <svg viewBox={`0 0 ${viewW} ${viewH}`} preserveAspectRatio="none" aria-hidden>
        <defs>
          <linearGradient id={gid} x1="0" y1="0" x2="1" y2="0">
            <stop offset="0%" stopColor="var(--d2)" stopOpacity="0" />
            <stop offset="70%" stopColor="var(--d2)" stopOpacity=".2" />
            <stop offset="100%" stopColor="var(--d1)" stopOpacity=".65" />
          </linearGradient>
        </defs>
        <line className="axis" x1="0" y1={AX} x2={viewW} y2={AX} />
        {bars.map((b, i) => (
          <g key={i}>
            <rect
              x={(i * W + 1).toFixed(1)}
              y={(AX - b.up - 1).toFixed(1)}
              width={(W - 2).toFixed(1)}
              height={b.up.toFixed(1)}
              fill="var(--d1)"
              opacity=".85"
            />
            <rect
              x={(i * W + 1).toFixed(1)}
              y={(AX + 1).toFixed(1)}
              width={(W - 2).toFixed(1)}
              height={b.dn.toFixed(1)}
              fill="var(--d3)"
              opacity=".85"
            />
          </g>
        ))}
        <rect className="sweep" x="0" y="0" width={wide ? 96 : 72} height={viewH} fill={`url(#${gid})`} />
      </svg>
      <div style={{ display: "flex", gap: 20, marginTop: 9 }}>
        <span className="at-io-leg">
          <span className="sw" style={{ background: "var(--d1)" }} />
          read
        </span>
        <span className="at-io-leg">
          <span className="sw" style={{ background: "var(--d3)" }} />
          write
        </span>
        <span className="at-io-leg" style={{ marginLeft: "auto", color: "var(--at-ink-4)" }}>
          session window
        </span>
      </div>
    </div>
  );
}
