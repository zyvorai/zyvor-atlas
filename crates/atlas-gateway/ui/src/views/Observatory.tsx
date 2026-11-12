// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
// Observatory — shop archetype D: charts inside elevated boxes (+ optional swipe lenses).
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import {
  useAlerts,
  useBackendsSummary,
  useClusters,
  useForecast,
  useHistory,
  usePools,
  useSummary,
} from "../api/hooks";
import { history, onHistory, recordSummary, seed } from "../store/history";
import { depth, depthWidth } from "../lib/depth";
import { fmtBytes, fmtBytesOpt, fmtForecastFill, fmtPct, fmtSi } from "../lib/format";
import { Echogram } from "../ui/Echogram";
import { DashboardHero, DashboardModule } from "../ui/templates/DashboardHero";
import IoHeatmap from "./IoHeatmap";
import { navCrumbs } from "../nav/routes";
import { Spinner } from "../ui/kit";

// —— retained canvas lenses (secondary) ————————————————————————————————
const KC: Record<string, string> = {
  rbd: "#38BDF8",
  cephfs_data: "#5bd8ff",
  cephfs_metadata: "#5bd8ff",
  rgw: "#A78BFA",
  nfs_export: "#34D399",
  zpool: "#FBBF24",
  other: "#6b8bb5",
};
const kc = (k: string) => KC[k] || KC.other;
const bcol = (t: string) => (t === "nfs" ? "#34D399" : t === "zfs" ? "#FBBF24" : "#38BDF8");
function canvasInk() {
  const s = getComputedStyle(document.documentElement);
  const v = (n: string, fb: string) => s.getPropertyValue(n).trim() || fb;
  const day = document.documentElement.dataset.uiShell === "day" || document.documentElement.dataset.uiShell === "apple-lite";
  return {
    day,
    ink: v("--at-ink", day ? "#1d1d1f" : "#eaf6ff"),
    ink2: v("--at-ink-2", day ? "#424245" : "#8AA0BD"),
    ink3: v("--at-ink-3", day ? "#6e6e73" : "#8AA0BD"),
    ridge: v("--at-ridge", day ? "#f0f0f2" : "rgba(16,28,37,.9)"),
    line: v("--at-line-2", day ? "rgba(0,0,0,0.12)" : "rgba(120,180,200,.2)"),
    starHi: day ? "#6e6e73" : "#bfe0ff",
    starLo: day ? "#a1a1a6" : "#7f9bc4",
    orbit: day ? "rgba(0,0,0,0.08)" : "rgba(56,90,140,.16)",
    core: day ? "rgba(0,0,0,0.12)" : "rgba(56,90,140,.28)",
    label: day ? "#1d1d1f" : "#dff2ff",
    cyan0: v("--d2", "#0071e3"),
    cyan1: day ? "#0077ed" : "#2563EB",
  };
}
function poolBackendMap(clusters: any[], backends: any[]) {
  const clById: Record<string, string> = {};
  (clusters || []).forEach((c) => (clById[c.id] = c.backend_id));
  const typeByBackend: Record<string, string> = {};
  (backends || []).forEach((b) => (typeByBackend[b.backend_id] = b.backend_type));
  return (pool: any): string => {
    const bt = typeByBackend[clById[pool.cluster_id]];
    if (bt) return bt;
    if (pool.kind === "nfs_export") return "nfs";
    if (pool.kind === "zpool") return "zfs";
    return "ceph";
  };
}
const TB = 2 ** 40,
  GB = 2 ** 30,
  MB = 2 ** 20,
  KB = 2 ** 10;
function tb(n: number) {
  if (n >= TB) return (n / TB).toFixed(n >= 10 * TB ? 1 : 2) + " TiB";
  if (n >= GB) return (n / GB).toFixed(n >= 100 * GB ? 0 : 1) + " GiB";
  if (n >= MB) return (n / MB).toFixed(1) + " MiB";
  return (n / KB).toFixed(0) + " KiB";
}
const reduced = () => matchMedia("(prefers-reduced-motion:reduce)").matches;

function useCanvas(run: (ctx: CanvasRenderingContext2D, api: { w(): number; h(): number }) => () => void, deps: unknown[]) {
  const box = useRef<HTMLDivElement>(null);
  const cv = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    const c = cv.current!,
      b = box.current!,
      ctx = c.getContext("2d")!;
    const DPR = Math.min(devicePixelRatio || 1, 2);
    let W = 0,
      H = 0;
    const fit = () => {
      W = b.clientWidth;
      H = b.clientHeight;
      c.width = W * DPR;
      c.height = H * DPR;
      ctx.setTransform(DPR, 0, 0, DPR, 0, 0);
    };
    fit();
    const ro = new ResizeObserver(fit);
    ro.observe(b);
    const cleanup = run(ctx, { w: () => W, h: () => H });
    return () => {
      ro.disconnect();
      cleanup && cleanup();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  return { box, cv };
}

function Orbital({ data }: { data: any }) {
  // Mirrors the latest `data` into a ref so the animation loop below (started once by
  // useCanvas, not re-created per render) can always read current data without needing it in
  // a dependency array. Writing ref.current during render is disallowed; useLayoutEffect keeps
  // it fresh before the next paint instead.
  const ref = useRef(data);
  useLayoutEffect(() => {
    ref.current = data;
  }, [data]);
  const { box, cv } = useCanvas((ctx, api) => {
    const stars = () => {
      const n = Math.round((api.w() * api.h()) / 13000);
      const a: any[] = [];
      for (let i = 0; i < n; i++)
        a.push({ x: Math.random() * api.w(), y: Math.random() * api.h(), z: Math.random(), ph: Math.random() * 6.28 });
      return a;
    };
    let S = stars();
    let lastW = api.w();
    let raf = 0;
    const red = reduced();
    const draw = (t: number) => {
      const W = api.w(),
        H = api.h();
      if (W !== lastW) {
        S = stars();
        lastW = W;
      }
      const { s, backends, pools, clusters } = ref.current;
      const pb = poolBackendMap(clusters, backends);
      const cx = W / 2,
        cy = H / 2,
        half = Math.min(W, H) / 2;
      ctx.clearRect(0, 0, W, H);
      const ink = canvasInk();
      for (const st of S) {
        const tw = red ? 0.7 : 0.45 + 0.55 * Math.abs(Math.sin(t * 0.001 + st.ph));
        ctx.globalAlpha = (0.25 + 0.6 * st.z) * tw * (ink.day ? 0.55 : 1);
        ctx.fillStyle = st.z > 0.85 ? ink.starHi : ink.starLo;
        ctx.fillRect(st.x, st.y, st.z > 0.9 ? 1.6 : 1, st.z > 0.9 ? 1.6 : 1);
      }
      ctx.globalAlpha = 1;
      const orbit = half * 0.52;
      ctx.strokeStyle = ink.orbit;
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.arc(cx, cy, orbit, 0, 6.283);
      ctx.stroke();
      const used = s.raw_capacity_bytes > 0 ? Math.min(1, Math.max(0, s.used_capacity_bytes / s.raw_capacity_bytes)) : 0;
      const coreR = half * 0.2;
      ctx.strokeStyle = ink.core;
      ctx.lineWidth = 7;
      ctx.beginPath();
      ctx.arc(cx, cy, coreR, 0, 6.283);
      ctx.stroke();
      const g = ctx.createLinearGradient(cx - coreR, cy - coreR, cx + coreR, cy + coreR);
      g.addColorStop(0, ink.cyan0);
      g.addColorStop(1, ink.cyan1);
      ctx.strokeStyle = g;
      ctx.lineCap = "round";
      ctx.beginPath();
      ctx.arc(cx, cy, coreR, -Math.PI / 2, -Math.PI / 2 + used * 6.283);
      ctx.stroke();
      ctx.lineCap = "butt";
      ctx.textAlign = "center";
      ctx.fillStyle = ink.ink;
      ctx.font = "700 26px ui-monospace,Menlo,monospace";
      ctx.fillText(tb(s.raw_capacity_bytes), cx, cy - 2);
      ctx.fillStyle = ink.ink3;
      ctx.font = "10px ui-monospace,Menlo,monospace";
      ctx.fillText("RAW · " + Math.round(used * 100) + "% ENGAGED", cx, cy + 18);
      const bs = backends || [];
      bs.forEach((B: any, i: number) => {
        B._ang = B._ang == null ? (i / bs.length) * 6.283 + 0.4 : B._ang + (red ? 0 : (B.backend_type === "nfs" ? 0.03 : 0.045) * 0.016);
        const bx = cx + Math.cos(B._ang) * orbit,
          by = cy + Math.sin(B._ang) * orbit;
        const r = 14 + 5 * Math.log10(Math.max(10, B.raw_capacity_bytes / GB)) - 8;
        const col = bcol(B.backend_type);
        ctx.beginPath();
        ctx.arc(bx, by, r, 0, 6.283);
        ctx.fillStyle = col;
        ctx.globalAlpha = 0.85;
        ctx.fill();
        ctx.globalAlpha = 1;
        ctx.fillStyle = ink.label;
        ctx.font = "600 10px ui-monospace,Menlo,monospace";
        ctx.fillText(B.backend_type.toUpperCase(), bx, by + r + 14);
        const mp = (pools || []).filter((p: any) => pb(p) === B.backend_type);
        mp.slice(0, 6).forEach((p: any, j: number) => {
          const a = B._ang + (j - mp.length / 2) * 0.18;
          const mx = bx + Math.cos(a) * (r + 18),
            my = by + Math.sin(a) * (r + 18);
          ctx.beginPath();
          ctx.arc(mx, my, 3.2, 0, 6.283);
          ctx.fillStyle = kc(p.kind);
          ctx.fill();
        });
      });
      if (!red) raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  }, [data.backends, data.pools, data.clusters, data.s]);
  return (
    <div ref={box} className="absolute inset-0">
      <canvas ref={cv} className="w-full h-full block" />
    </div>
  );
}

function Treemap({ data }: { data: any }) {
  const box = useRef<HTMLDivElement>(null);
  const cv = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    const pools = data.pools || [];
    const c = cv.current!,
      b = box.current!,
      ctx = c.getContext("2d")!;
    const DPR = Math.min(devicePixelRatio || 1, 2);
    const fit = () => {
      const W = b.clientWidth,
        H = b.clientHeight;
      c.width = W * DPR;
      c.height = H * DPR;
      ctx.setTransform(DPR, 0, 0, DPR, 0, 0);
      ctx.clearRect(0, 0, W, H);
      const items = pools
        .map((p: any) => {
          const used = p.used_bytes || 0;
          return { n: p.name, k: p.kind, max: (p.max_bytes || 0) + used || 1, used };
        })
        .sort((a: any, b: any) => b.max - a.max)
        .slice(0, 24);
      const total = items.reduce((s: number, x: any) => s + x.max, 0) || 1;
      let x = 8,
        y = 8,
        rowH = 0,
        rowW = 0;
      const maxW = W - 16;
      items.forEach((it: any) => {
        const area = (it.max / total) * (maxW * (H - 16));
        let w = Math.sqrt(area * 1.6);
        let h = area / Math.max(w, 1);
        if (x + w > W - 8) {
          x = 8;
          y += rowH + 6;
          rowH = 0;
        }
        w = Math.min(w, maxW);
        h = Math.max(28, Math.min(h, 90));
        rowH = Math.max(rowH, h);
        const ink = canvasInk();
        ctx.fillStyle = ink.ridge;
        ctx.strokeStyle = ink.line;
        ctx.fillRect(x, y, w, h);
        ctx.strokeRect(x, y, w, h);
        const fill = Math.min(1, it.used / it.max);
        ctx.fillStyle = kc(it.k);
        ctx.globalAlpha = 0.55;
        ctx.fillRect(x, y + h * (1 - fill), w, h * fill);
        ctx.globalAlpha = 1;
        ctx.fillStyle = ink.ink;
        ctx.font = "600 11px ui-monospace,Menlo,monospace";
        ctx.fillText(it.n.slice(0, 18), x + 6, y + 16);
        x += w + 6;
        rowW = x;
      });
      void rowW;
    };
    fit();
    const ro = new ResizeObserver(fit);
    ro.observe(b);
    return () => ro.disconnect();
  }, [data.pools]);
  return (
    <div ref={box} className="absolute inset-0">
      <canvas ref={cv} className="w-full h-full block" />
    </div>
  );
}

const LENSES = [
  { id: "orbital", label: "Orbital", cap: "Backends orbit; pools are moons." },
  { id: "treemap", label: "Treemap", cap: "Pools sized by provisioned capacity." },
];

export default function Observatory() {
  const nav = useNavigate();
  const [lens, setLens] = useState<string | null>(null);
  const { data: s } = useSummary();
  const { data: backends } = useBackendsSummary();
  const { data: pools } = usePools();
  const { data: fc } = useForecast();
  const { data: serverHist } = useHistory();
  const { data: clusters } = useClusters();
  const { data: alerts } = useAlerts("open");
  const [, setTick] = useState(0);

  useEffect(() => onHistory(() => setTick((t) => t + 1)), []);
  useEffect(() => {
    if (serverHist) seed(serverHist);
  }, [serverHist]);
  useEffect(() => {
    if (s) recordSummary(s);
  }, [s]);

  const hist = history();
  const usedPct = fmtPct(s?.used_capacity_percent);
  const io = s?.client_io;
  const forecastLine = fmtForecastFill(fc?.days_to_full, fc?.growth_bytes_per_day);

  const deepest = useMemo(() => {
    return (pools || [])
      .map((p) => {
        const max = p.max_bytes || 0;
        const used = p.used_bytes || 0;
        const pct = max > 0 ? (used / max) * 100 : 0;
        return { ...p, pct };
      })
      .sort((a, b) => b.pct - a.pct)[0];
  }, [pools]);

  const stateLine = !s
    ? "Opening the observatory…"
    : alerts && alerts.length
      ? `${alerts.length} open alert${alerts.length === 1 ? "" : "s"} · estate at ${usedPct}% · ${forecastLine || "usage steady"}.`
      : deepest && deepest.pct >= 60
        ? `Deepest pool ${deepest.name} at ${fmtPct(deepest.pct)}% · estate ${usedPct}% occupied.`
        : (s.raw_capacity_bytes ?? 0) > 0
          ? `${fmtBytesOpt(s.used_capacity_bytes)} of ${fmtBytesOpt(s.raw_capacity_bytes)} surveyed · ${s.volumes} volumes online.`
          : `Capacity not surveyed yet · ${s.volumes} volumes online.`;

  const ready = !!(s && backends && pools);
  const base = { s, backends, pools, fc, hist: serverHist, clusters, alerts };

  const backendRows = useMemo(() => {
    return (backends || []).map((b: any) => {
      const raw = b.raw_capacity_bytes || 0;
      const used = b.used_capacity_bytes || 0;
      const pct = raw > 0 ? (used / raw) * 100 : 0;
      return { ...b, pct, d: depth(pct) };
    });
  }, [backends]);

  const poolRows = useMemo(() => {
    return (pools || [])
      .map((p) => {
        const max = p.max_bytes || 0;
        const used = p.used_bytes || 0;
        const pct = max > 0 ? (used / max) * 100 : 0;
        return { ...p, pct, d: depth(pct) };
      })
      .sort((a, b) => b.pct - a.pct)
      .slice(0, 8);
  }, [pools]);

  return (
    <DashboardHero
      crumbs={navCrumbs("observatory")}
      eyebrow="TELEMETRY · ESTATE"
      title="Observatory"
      state={stateLine}
      actions={
        <>
          <button type="button" className="at-btn" onClick={() => nav("/")}>
            Overview
          </button>
          <button type="button" className="at-btn primary" onClick={() => nav("/alerts")}>
            Alerts{alerts?.length ? ` · ${alerts.length}` : ""}
          </button>
        </>
      }
    >
      <DashboardModule>
        <div className="at-panel" style={{ padding: "18px 20px 14px" }}>
          <div style={{ display: "flex", justifyContent: "space-between", gap: 24, flexWrap: "wrap", marginBottom: 12 }}>
            <div>
              <div className="at-caption">Client IO echogram</div>
              <div className="at-io-split" style={{ marginTop: 6 }}>
                <div>
                  <div className="at-val md" style={{ color: "var(--d1)" }}>
                    {fmtSi(io?.read_ops_total)}
                    <span className="at-unit">ops read</span>
                  </div>
                  <div className="at-sub">{fmtBytes(io?.read_bytes_total)}</div>
                </div>
                <div>
                  <div className="at-val md" style={{ color: "var(--d3)" }}>
                    {fmtSi(io?.write_ops_total)}
                    <span className="at-unit">ops write</span>
                  </div>
                  <div className="at-sub">{fmtBytes(io?.write_bytes_total)}</div>
                </div>
              </div>
            </div>
            <div style={{ textAlign: "right" }}>
              <div className="at-caption">Capacity</div>
              <div className="at-val md">
                {usedPct}
                <span className="at-unit">% occupied</span>
              </div>
              <div className="at-sub">{forecastLine || "Usage steady — no near-term fill"}</div>
            </div>
          </div>
          <Echogram wide readOps={io?.read_ops_total || 0} writeOps={io?.write_ops_total || 0} hist={hist} />
        </div>
      </DashboardModule>

      <DashboardModule>
        <IoHeatmap />
      </DashboardModule>

      <div className="at-breakgrid">
        <DashboardModule title="Backends" note={`${backendRows.length} registered`} className="mb-0">
          <div className="at-panel">
            {!ready ? (
              <div style={{ padding: 24 }}>
                <Spinner />
              </div>
            ) : (
              backendRows.map((b: any) => (
                <button
                  key={b.backend_id || b.id}
                  type="button"
                  className="at-basin"
                  onClick={() => nav("/backends")}
                >
                  <div className="at-basin-id">
                    <div className="at-basin-name">
                      <span className="mono">{b.backend_type?.toUpperCase?.() || b.backend_type}</span>
                      <span className="at-tag">{b.d.name.toUpperCase()}</span>
                    </div>
                    <div className="at-trough">
                      <i className={`at-level ${b.d.cls}`} style={{ width: depthWidth(b.pct) }} />
                    </div>
                  </div>
                  <div className="at-basin-read">
                    {fmtBytesOpt(b.used_capacity_bytes)} of {fmtBytesOpt(b.raw_capacity_bytes)}
                  </div>
                  <div className={`at-basin-pct ${b.d.cls} fg`}>{fmtPct(b.pct)}%</div>
                </button>
              ))
            )}
          </div>
        </DashboardModule>

        <DashboardModule title="Deepest pools" note="sorted by depth" className="mb-0">
          <div className="at-panel">
            {poolRows.map((p) => (
              <button key={p.id} type="button" className="at-basin" onClick={() => nav(`/pools/${p.id}`)}>
                <div className="at-basin-id">
                  <div className="at-basin-name">
                    <span className="mono">{p.name}</span>
                    <span className="at-tag">{p.kind}</span>
                  </div>
                  <div className="at-trough">
                    <i className={`at-level ${p.d.cls}`} style={{ width: depthWidth(p.pct) }} />
                  </div>
                </div>
                <div className="at-basin-read">
                  {fmtBytesOpt(p.used_bytes)} of {fmtBytesOpt(p.max_bytes)}
                </div>
                <div className={`at-basin-pct ${p.d.cls} fg`}>{fmtPct(p.pct)}%</div>
              </button>
            ))}
            {!poolRows.length && ready && (
              <div style={{ padding: 20, color: "var(--at-ink-4)", fontSize: 13 }}>No pools discovered.</div>
            )}
          </div>
        </DashboardModule>
      </div>

      <DashboardModule title="Lenses" note="optional canvases — one at a time">
        <div className="at-chip-row">
          {LENSES.map((t) => (
            <button
              key={t.id}
              type="button"
              className={`at-chip${lens === t.id ? " on" : ""}`}
              onClick={() => setLens(lens === t.id ? null : t.id)}
            >
              {t.label}
            </button>
          ))}
        </div>
        {lens && (
          <div
            className="at-panel relative overflow-hidden"
            style={{
              height: "min(52vh, 480px)",
              minHeight: 320,
              background: "radial-gradient(1000px 700px at 28% 16%, #0a1526 0%, rgba(10,21,38,0) 60%), #04070A",
            }}
          >
            <div className="at-sub" style={{ position: "absolute", top: 12, left: 16, zIndex: 2, margin: 0 }}>
              {LENSES.find((t) => t.id === lens)?.cap}
            </div>
            {!ready ? (
              <div className="absolute inset-0 grid place-items-center">
                <Spinner />
              </div>
            ) : (
              <>
                {lens === "orbital" && <Orbital data={base} />}
                {lens === "treemap" && <Treemap data={base} />}
              </>
            )}
          </div>
        )}
      </DashboardModule>
    </DashboardHero>
  );
}
