// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Observatory: client I/O and capacity, the latency heatmap, backends and pools, and two optional
// canvas views. Canvas colours come from the theme's custom properties.
import { useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { useNavigate } from "react-router-dom";
import {
  useAlerts,
  useBackendsSummary,
  useCephDf,
  useClusters,
  useForecast,
  useHistory,
  usePools,
  useSummary,
} from "../api/hooks";
import { history, iopsMaxGapMs, iopsTrend, onHistory, recordSummary, seed } from "../store/history";
import { depth } from "../lib/depth";
import { poolFill, poolUsage } from "../lib/poolUsage";
import { fmtBytes, fmtBytesOpt, fmtForecastFill, fmtPct, fmtSi } from "../lib/format";
import {
  CapacityRing,
  EmptyState,
  PoolUsageBars,
  TrendChart,
  capacityFromSummary,
  formatBytesPerSecond,
  throughputSeries,
  type PoolUsage,
} from "../graphics";
import { DashboardHero, DashboardModule } from "../ui/templates/DashboardHero";
import IoHeatmap from "./IoHeatmap";
import { navCrumbs } from "../nav/routes";
import { Spinner } from "../ui/kit";
import "./Observatory.css";

/** Capacity turns amber and red at the same points as lib/depth.ts. */
const FILL = { warnAt: 75, failAt: 90 };

// —— optional canvas views ——————————————————————————————————————————

/** Canvas colours, read from the theme tokens at paint time. */
function canvasInk() {
  const s = getComputedStyle(document.documentElement);
  const v = (name: string) => s.getPropertyValue(name).trim();
  return {
    font: v("--a-font") || "sans-serif",
    fill: v("--a-surface-2"),
    track: v("--a-surface-3"),
    line: v("--a-line-strong"),
    ink: v("--a-ink"),
    ink2: v("--a-ink-2"),
    blue: v("--a-blue"),
    series2: v("--a-series-2"),
    warn: v("--a-warn"),
    fail: v("--a-fail"),
  };
}

function fillColour(ink: ReturnType<typeof canvasInk>, pct: number) {
  const { cls } = depth(pct);
  return cls === "dp5" ? ink.fail : cls === "dp4" ? ink.warn : ink.blue;
}

/** Re-renders when the theme changes, so a still canvas is repainted in the new colours. */
function useThemeName() {
  return useSyncExternalStore(
    (notify) => {
      const mo = new MutationObserver(notify);
      mo.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
      return () => mo.disconnect();
    },
    () => document.documentElement.dataset.theme ?? "",
  );
}

type Paint = (ctx: CanvasRenderingContext2D, w: number, h: number, t: number) => void;

/** A canvas sized to its box. It repaints on resize and after every render, and loops if `animate`. */
function useCanvas(paint: Paint, animate: boolean) {
  const box = useRef<HTMLDivElement>(null);
  const cv = useRef<HTMLCanvasElement>(null);
  const paintRef = useRef(paint);
  const redraw = useRef(() => {});
  useLayoutEffect(() => {
    paintRef.current = paint;
    redraw.current();
  });
  useEffect(() => {
    const c = cv.current!,
      b = box.current!,
      ctx = c.getContext("2d");
    if (!ctx) return;
    const DPR = Math.min(devicePixelRatio || 1, 2);
    let W = 0,
      H = 0,
      raf = 0;
    const draw = (t: number) => {
      ctx.clearRect(0, 0, W, H);
      paintRef.current(ctx, W, H, t);
    };
    const fit = () => {
      W = b.clientWidth;
      H = b.clientHeight;
      c.width = W * DPR;
      c.height = H * DPR;
      ctx.setTransform(DPR, 0, 0, DPR, 0, 0);
      draw(performance.now());
    };
    redraw.current = () => draw(performance.now());
    fit();
    const ro = new ResizeObserver(fit);
    ro.observe(b);
    if (animate) {
      const loop = (t: number) => {
        draw(t);
        raf = requestAnimationFrame(loop);
      };
      raf = requestAnimationFrame(loop);
    }
    return () => {
      ro.disconnect();
      cancelAnimationFrame(raf);
      redraw.current = () => {};
    };
  }, [animate]);
  return { box, cv };
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
const GB = 2 ** 30;
const reduced = () => !!window.matchMedia?.("(prefers-reduced-motion:reduce)").matches;

function Orbital({ s, backends, pools, clusters }: { s: any; backends: any[]; pools: any[]; clusters: any[] }) {
  useThemeName();
  const [still] = useState(reduced);
  const pb = poolBackendMap(clusters, backends);
  const surveyed = (s?.raw_capacity_bytes ?? 0) > 0;
  const used = surveyed ? Math.min(1, Math.max(0, s.used_capacity_bytes / s.raw_capacity_bytes)) : 0;
  const usedText = surveyed ? `${Math.round(used * 100)}% used` : "capacity not reported";
  const poolsOf = (type: string) => pools.filter((p) => pb(p) === type);
  const label =
    `Orbital view. Raw capacity ${surveyed ? `${fmtBytes(s.raw_capacity_bytes)}, ` : ""}${usedText}. ` +
    (backends.map((b) => `${b.backend_type}: ${poolsOf(b.backend_type).length} pools`).join(", ") || "No backends") +
    ".";
  const { box, cv } = useCanvas((ctx, W, H, t) => {
    const ink = canvasInk();
    const cx = W / 2,
      cy = H / 2,
      half = Math.min(W, H) / 2;
    const orbit = half * 0.62;
    ctx.textAlign = "center";
    ctx.strokeStyle = ink.line;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.arc(cx, cy, orbit, 0, Math.PI * 2);
    ctx.stroke();
    const coreR = half * 0.34;
    ctx.strokeStyle = ink.track;
    ctx.lineWidth = 10;
    ctx.beginPath();
    ctx.arc(cx, cy, coreR, 0, Math.PI * 2);
    ctx.stroke();
    if (used > 0) {
      ctx.strokeStyle = fillColour(ink, used * 100);
      ctx.lineCap = "round";
      ctx.beginPath();
      ctx.arc(cx, cy, coreR, -Math.PI / 2, -Math.PI / 2 + used * Math.PI * 2);
      ctx.stroke();
      ctx.lineCap = "butt";
    }
    ctx.fillStyle = ink.ink;
    ctx.font = `600 20px ${ink.font}`;
    ctx.fillText(surveyed ? fmtBytes(s.raw_capacity_bytes) : "—", cx, cy);
    ctx.fillStyle = ink.ink2;
    ctx.font = `12px ${ink.font}`;
    ctx.fillText(usedText, cx, cy + 20);
    backends.forEach((b: any, i: number) => {
      const speed = b.backend_type === "nfs" ? 0.03 : 0.045;
      const ang = (i / backends.length) * Math.PI * 2 + 0.4 + (still ? 0 : (speed * t) / 1000);
      const bx = cx + Math.cos(ang) * orbit,
        by = cy + Math.sin(ang) * orbit;
      const r = 14 + 5 * Math.log10(Math.max(10, (b.raw_capacity_bytes || 0) / GB)) - 8;
      ctx.beginPath();
      ctx.arc(bx, by, r, 0, Math.PI * 2);
      ctx.fillStyle = ink.blue;
      ctx.fill();
      ctx.fillStyle = ink.ink;
      ctx.font = `600 12px ${ink.font}`;
      ctx.fillText(String(b.backend_type), bx, by + r + 16);
      const mine = poolsOf(b.backend_type);
      mine.slice(0, 6).forEach((_, j) => {
        const a = ang + (j - mine.length / 2) * 0.18;
        ctx.beginPath();
        ctx.arc(bx + Math.cos(a) * (r + 20), by + Math.sin(a) * (r + 20), 4, 0, Math.PI * 2);
        ctx.fillStyle = ink.series2;
        ctx.fill();
      });
    });
  }, !still);
  return (
    <div ref={box} className="obs-canvas">
      <canvas ref={cv} role="img" aria-label={label} />
    </div>
  );
}

/** A pool's logical size: its total, or (when Ceph reports a fill level) stored plus available. */
function poolSize(p: PoolUsage): number | null {
  if (p.capacityBytes) return p.capacityBytes;
  if (poolFill(p) != null && p.availableBytes != null) return (p.usedBytes ?? 0) + p.availableBytes || null;
  return null;
}

type Box = { x: number; y: number; w: number; h: number };

/** Binary-split treemap: halve the weight along the longer side until each item has its own box. */
function split<T>(items: T[], weight: (item: T) => number, box: Box): [T, Box][] {
  if (items.length <= 1) return items.map((item) => [item, box]);
  const total = items.reduce((sum, item) => sum + weight(item), 0);
  let cut = 1,
    head = weight(items[0]);
  while (cut < items.length - 1 && head + weight(items[cut]) / 2 < total / 2) head += weight(items[cut++]);
  const share = head / total;
  const wide = box.w >= box.h;
  const first = wide ? { ...box, w: box.w * share } : { ...box, h: box.h * share };
  const rest = wide
    ? { ...box, x: box.x + first.w, w: box.w - first.w }
    : { ...box, y: box.y + first.h, h: box.h - first.h };
  return [...split(items.slice(0, cut), weight, first), ...split(items.slice(cut), weight, rest)];
}

function Treemap({ pools }: { pools: PoolUsage[] }) {
  useThemeName();
  const items = pools
    .map((p) => ({ name: p.name, size: poolSize(p), pct: poolFill(p) }))
    .sort((a, b) => (b.size ?? 0) - (a.size ?? 0))
    .slice(0, 24);
  const fillText = (pct: number | null) => (pct == null ? "fill unknown" : `${Math.round(pct)}% full`);
  const label = `Treemap of pools. ${items.map((i) => `${i.name} ${fillText(i.pct)}`).join(", ") || "No pools"}.`;
  const { box, cv } = useCanvas((ctx, W, H) => {
    const ink = canvasInk();
    // A pool of unknown size is drawn like the smallest known one, with a dashed outline.
    const smallest = Math.min(...items.map((i) => i.size ?? Infinity));
    const unit = Number.isFinite(smallest) ? smallest : 1;
    const total = items.reduce((sum, i) => sum + (i.size ?? unit), 0) || 1;
    // Very small pools keep a visible tile.
    const weight = (i: (typeof items)[number]) => Math.max(i.size ?? unit, total * 0.03);
    ctx.textAlign = "left";
    ctx.lineWidth = 1;
    split(items, weight, { x: 0, y: 0, w: W, h: H }).forEach(([it, tile]) => {
      const x = tile.x + 3,
        y = tile.y + 3,
        w = Math.max(0, tile.w - 6),
        h = Math.max(0, tile.h - 6);
      ctx.fillStyle = ink.fill;
      ctx.fillRect(x, y, w, h);
      if (it.pct != null) {
        const fill = Math.min(1, it.pct / 100);
        ctx.fillStyle = fillColour(ink, it.pct);
        ctx.globalAlpha = 0.32;
        ctx.fillRect(x, y + h * (1 - fill), w, h * fill);
        ctx.globalAlpha = 1;
      }
      ctx.strokeStyle = ink.line;
      ctx.setLineDash(it.size == null ? [4, 4] : []);
      ctx.strokeRect(x, y, w, h);
      ctx.setLineDash([]);
      // Labels only where they fit; the canvas label lists every pool.
      if (w < 56 || h < 24) return;
      const chars = Math.max(4, Math.floor((w - 16) / 7));
      ctx.fillStyle = ink.ink;
      ctx.font = `600 12px ${ink.font}`;
      ctx.fillText(it.name.length > chars ? `${it.name.slice(0, chars - 1)}…` : it.name, x + 8, y + 18);
      if (h < 42 || w < 80) return;
      ctx.font = `11px ${ink.font}`;
      ctx.fillText(fillText(it.pct), x + 8, y + 34);
    });
  }, false);
  return (
    <div ref={box} className="obs-canvas">
      <canvas ref={cv} role="img" aria-label={label} />
    </div>
  );
}

const LENSES = [
  { id: "orbital", label: "Orbital", cap: "Large dots are backends; small dots are their pools (up to six each)." },
  {
    id: "treemap",
    label: "Treemap",
    cap: "Pools sized by capacity (for Ceph, stored plus available). The shaded part is how full each one is.",
  },
];

export default function Observatory() {
  const nav = useNavigate();
  const [lens, setLens] = useState<string | null>(null);
  const { data: s } = useSummary();
  const { data: backends } = useBackendsSummary();
  const { data: pools } = usePools();
  const { data: cephDf } = useCephDf();
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
  const surveyed = (s?.raw_capacity_bytes ?? 0) > 0;
  const usedPct = fmtPct(s?.used_capacity_percent);
  const io = s?.client_io;
  const forecastLine = fmtForecastFill(fc?.days_to_full, fc?.growth_bytes_per_day);

  const poolRows = useMemo(
    () => poolUsage(pools, { clusters, backends, cephDf }),
    [pools, clusters, backends, cephDf],
  );
  // The fullest pool whose fill level is actually known.
  const fullest = useMemo(() => {
    let best: { name: string; pct: number } | undefined;
    for (const p of poolRows ?? []) {
      const pct = poolFill(p);
      if (pct != null && (!best || pct > best.pct)) best = { name: p.name, pct };
    }
    return best;
  }, [poolRows]);

  const volumes = `${s?.volumes} volume${s?.volumes === 1 ? "" : "s"}`;
  const stateLine = !s
    ? "Loading…"
    : alerts && alerts.length
      ? [
          `${alerts.length} open alert${alerts.length === 1 ? "" : "s"}`,
          surveyed ? `${usedPct}% of capacity used` : null,
          forecastLine,
        ]
          .filter(Boolean)
          .join(" · ") + "."
      : fullest && fullest.pct >= 60
        ? `Fullest pool ${fullest.name} is ${fmtPct(fullest.pct)}% full${surveyed ? ` · ${usedPct}% of capacity used` : ""}.`
        : surveyed
          ? `${fmtBytesOpt(s.used_capacity_bytes)} of ${fmtBytesOpt(s.raw_capacity_bytes)} used · ${volumes}.`
          : `Capacity not surveyed yet · ${volumes}.`;

  const ready = !!(s && backends && pools);

  // Backends report a total, so used / total is a real fill level. 0 of 0 means "not surveyed".
  const backendRows: PoolUsage[] = (backends ?? []).map((b: any) => {
    const total = b.raw_capacity_bytes || 0;
    const used = b.used_capacity_bytes;
    return {
      id: b.backend_id || b.id,
      name: b.backend_type,
      usedBytes: total > 0 || used > 0 ? used : null,
      capacityBytes: total > 0 ? total : null,
    };
  });

  return (
    <DashboardHero
      crumbs={navCrumbs("observatory")}
      eyebrow="Observability"
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
      <DashboardModule title="Performance and capacity">
        <div className="obs-top">
          <div className="at-panel obs-io">
            <dl className="obs-figures">
              {(["read", "write"] as const).map((dir) => (
                <div key={dir}>
                  <dt>
                    <svg className={`obs-key obs-key-${dir}`} width="18" height="8" aria-hidden="true">
                      <line x1="1" y1="4" x2="17" y2="4" />
                    </svg>
                    {dir === "read" ? "Read" : "Write"}
                  </dt>
                  <dd>
                    {io ? fmtSi(io[`${dir}_ops_total`]) : "—"} {io ? <small>ops total</small> : null}
                  </dd>
                  <dd className="sub">{io ? `${fmtBytes(io[`${dir}_bytes_total`])} total` : "—"}</dd>
                </div>
              ))}
            </dl>
            <TrendChart title="Client IOPS" series={iopsTrend(hist)} maxGapMs={iopsMaxGapMs(hist)} />
            <TrendChart
              title="Client throughput"
              series={throughputSeries(serverHist)}
              formatValue={formatBytesPerSecond}
            />
          </div>
          <div className="at-panel obs-capacity">
            <CapacityRing {...capacityFromSummary(s)} {...FILL} label="Raw capacity" size={260} loading={!s} />
            <p className="at-sub">
              {forecastLine || (surveyed ? "No near-term fill projected." : "Waiting on capacity discovery.")}
            </p>
          </div>
        </div>
      </DashboardModule>

      <DashboardModule title="I/O latency" note="Last 15 minutes, from the node agent">
        <IoHeatmap />
      </DashboardModule>

      <div className="at-breakgrid obs-grid">
        <DashboardModule
          title="Backends"
          note={backends ? `${backends.length} registered` : undefined}
          className="mb-0"
        >
          <div className="at-panel obs-bars">
            {!backends ? (
              <Spinner />
            ) : backendRows.length ? (
              <PoolUsageBars label="Backend capacity" pools={backendRows} onSelect={() => nav("/backends")} {...FILL} />
            ) : (
              <EmptyState art="cluster" title="No backends registered" compact />
            )}
          </div>
        </DashboardModule>

        <DashboardModule title="Fullest pools" note="Select a pool for detail" className="mb-0">
          <div className="at-panel obs-bars">
            <PoolUsageBars
              pools={poolRows}
              loading={!pools}
              limit={8}
              onSelect={(p) => nav(`/pools/${p.id}`)}
              {...FILL}
            />
          </div>
        </DashboardModule>
      </div>

      <DashboardModule title="Views" note="Optional, one at a time">
        <div className="at-chip-row">
          {LENSES.map((t) => (
            <button
              key={t.id}
              type="button"
              className={`at-chip${lens === t.id ? " on" : ""}`}
              aria-pressed={lens === t.id}
              onClick={() => setLens(lens === t.id ? null : t.id)}
            >
              {t.label}
            </button>
          ))}
        </div>
        {lens && (
          <div className="at-panel obs-lens">
            <p className="at-sub">{LENSES.find((t) => t.id === lens)?.cap}</p>
            {!ready ? (
              <Spinner />
            ) : lens === "orbital" ? (
              <Orbital s={s} backends={backends!} pools={pools!} clusters={clusters ?? []} />
            ) : (
              <Treemap pools={poolRows ?? []} />
            )}
          </div>
        )}
      </DashboardModule>
    </DashboardHero>
  );
}
