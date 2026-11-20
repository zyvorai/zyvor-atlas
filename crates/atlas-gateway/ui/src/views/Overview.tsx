// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Shop Deck — Apple shop archetype A (hero + swipe status tiles + elevated boxes).
import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { ArrowUpRight } from "lucide-react";
import { useAlerts, useCephOsdDf, useClusters, useForecast, useHistory, useOsds, usePools, useSummary } from "../api/hooks";
import { history, onHistory, recordSummary, seed } from "../store/history";
import { depth, depthWidth } from "../lib/depth";
import { sendPrompt } from "../lib/prompts";
import { fmtBytes, fmtBytesOpt, fmtBytesParts, fmtForecastFill, fmtPct, fmtSi } from "../lib/format";
import { Echogram } from "../ui/Echogram";
import { SwipeRail } from "../ui/SwipeRail";
import { DashboardHero, DashboardModule } from "../ui/templates/DashboardHero";

function SoundingOrb({ pct }: { pct: number }) {
  const p = Math.max(0, Math.min(100, pct));
  // Circle r=76 → diameter 152; water top y grows as fill increases from bottom.
  const waterY = 110 + 76 - (152 * p) / 100;
  const circ = 2 * Math.PI * 84;
  const dashOff = circ * (1 - p / 100);
  const ticks: string[] = [];
  for (let i = 0; i < 60; i++) {
    const a = (i / 60) * Math.PI * 2;
    const major = i % 5 === 0;
    const r1 = major ? 96 : 100;
    const r2 = 104;
    ticks.push(
      `M${(110 + Math.cos(a) * r1).toFixed(1)} ${(110 + Math.sin(a) * r1).toFixed(1)}L${(110 + Math.cos(a) * r2).toFixed(1)} ${(110 + Math.sin(a) * r2).toFixed(1)}`,
    );
  }
  return (
    <svg width="212" height="212" viewBox="0 0 220 220" role="img" aria-label={`${p} percent occupied`}>
      <defs>
        <clipPath id="orbClip">
          <circle cx="110" cy="110" r="76" />
        </clipPath>
        <linearGradient id="waterGrad" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="var(--d2)" stopOpacity=".72" />
          <stop offset="55%" stopColor="var(--d3)" stopOpacity=".38" />
          <stop offset="100%" stopColor="var(--d4)" stopOpacity=".28" />
        </linearGradient>
      </defs>
      <path d={ticks.join("")} stroke="var(--d2)" strokeWidth="1" fill="none" opacity=".28" />
      <circle cx="110" cy="110" r="92" fill="none" stroke="var(--at-line-2)" />
      <circle cx="110" cy="110" r="76" fill="var(--at-ridge)" stroke="var(--at-line-2)" />
      <g clipPath="url(#orbClip)">
        <rect x="0" y={waterY} width="220" height={220 - waterY} fill="url(#waterGrad)" />
        <line x1="0" y1={waterY} x2="220" y2={waterY} stroke="var(--d1)" strokeWidth="1.6" opacity=".95" />
      </g>
      <circle cx="110" cy="110" r="84" fill="none" stroke="var(--at-line)" strokeWidth="3" />
      <circle
        cx="110"
        cy="110"
        r="84"
        fill="none"
        stroke="var(--d2)"
        strokeWidth="3.5"
        strokeLinecap="round"
        strokeDasharray={circ}
        strokeDashoffset={dashOff}
        transform="rotate(-90 110 110)"
        style={{ filter: "drop-shadow(0 0 10px color-mix(in srgb, var(--d2) 70%, transparent))" }}
      />
      <text
        x="110"
        y="106"
        textAnchor="middle"
        fill="var(--at-ink)"
        style={{ fontFamily: "var(--at-mono)", fontSize: 37, letterSpacing: "-.03em" }}
      >
        {p}%
      </text>
      <text
        x="110"
        y="126"
        textAnchor="middle"
        fill="var(--at-ink-3)"
        style={{ fontFamily: "var(--at-face)", fontSize: 10, letterSpacing: ".19em" }}
      >
        OCCUPIED
      </text>
    </svg>
  );
}

export default function Overview() {
  const nav = useNavigate();
  const { data: s } = useSummary();
  const { data: clusters } = useClusters();
  const { data: pools } = usePools();
  const { data: osds } = useOsds();
  const { data: osdDf } = useCephOsdDf();
  const { data: alerts } = useAlerts("open");
  const { data: forecast } = useForecast();
  const io = s?.client_io;
  const rc = s?.recovery;
  const recovering = (rc?.pg_recovering || 0) + (rc?.pg_backfilling || 0);
  const [, setTick] = useState(0);
  const { data: serverHist } = useHistory();
  useEffect(() => onHistory(() => setTick((t) => t + 1)), []);
  useEffect(() => {
    if (serverHist) seed(serverHist);
  }, [serverHist]);
  useEffect(() => {
    if (s) recordSummary(s);
  }, [s]);
  const hist = history();

  const osdRows = useMemo(() => {
    const byNum: Record<number, { used: number; cap: number }> = {};
    for (const n of osdDf?.nodes || []) {
      if (typeof n?.id !== "number") continue;
      byNum[n.id] = { used: (n.kb_used || 0) * 1024, cap: (n.kb || 0) * 1024 };
    }
    return (osds || []).map((o) => {
      const live = byNum[o.osd_num ?? o.id];
      if (!live) return o;
      return { ...o, used_bytes: o.used_bytes ?? live.used, capacity_bytes: o.capacity_bytes ?? live.cap };
    });
  }, [osds, osdDf]);

  const usedPct = fmtPct(s?.used_capacity_percent);
  const cluster = clusters?.[0];
  const clusterName = cluster?.name || "cluster";
  const hostCount = new Set(osdRows.map((o) => o.host).filter(Boolean)).size;

  const basinPools = useMemo(() => {
    return (pools || [])
      .map((p) => {
        const used = p.used_bytes || 0;
        const total = (p.max_bytes || 0) + used;
        const pct = total > 0 ? (used / total) * 100 : 0;
        return { ...p, used, total, pct };
      })
      .sort((a, b) => b.pct - a.pct);
  }, [pools]);

  const deepest = basinPools[0];
  const deepestDepth = deepest ? depth(deepest.pct) : null;
  const forecastLine = fmtForecastFill(forecast?.days_to_full, forecast?.growth_bytes_per_day);
  const health = cluster?.health || "unknown";
  const hasSurvey = (s?.raw_capacity_bytes ?? 0) > 0;
  const usedParts = hasSurvey ? fmtBytesParts(s?.used_capacity_bytes) : null;

  const stateLine = useMemo(() => {
    const used = fmtBytesOpt(hasSurvey ? s?.used_capacity_bytes : null);
    const raw = fmtBytesOpt(hasSurvey ? s?.raw_capacity_bytes : null);
    const vols = s?.volumes ?? 0;
    const parts: string[] = [];
    if (!hasSurvey) {
      parts.push("Capacity not surveyed yet");
    } else {
      parts.push(
        `<b>${used}</b> occupied of ${raw} surveyed. ${vols} volume${vols === 1 ? "" : "s"} online`,
      );
    }
    if (health === "warn" || health === "critical") {
      const tip = alerts?.[0]?.title;
      const cls = health === "critical" ? "at-state-crit" : "at-state-warn";
      parts.push(
        tip
          ? `Cluster reports <b class="${cls}">${health}</b> — ${tip}`
          : `Cluster reports <b class="${cls}">${health}</b>`,
      );
    } else if (deepest && deepest.pct >= 60) {
      parts.push(
        `One pool — <b>${deepest.name}</b> — is on the ${deepestDepth?.name} (${Math.round(deepest.pct)}%)`,
      );
    } else if (recovering > 0) {
      parts.push(`<b>${recovering}</b> PG(s) rebuilding`);
    } else if (hasSurvey) {
      parts.push("everything else is quiet");
    }
    return parts.join(". ") + ".";
  }, [s, hasSurvey, health, alerts, deepest, deepestDepth, recovering]);

  const osdUp = osdRows.filter((o) => o.up).length;
  const osdTotal = osdRows.length;
  const seabedCells = useMemo(() => {
    // Honest proxy: OSD health tiles (not true PG map until Ceph PG dump is wired).
    const n = Math.max(osdTotal, 32);
    return Array.from({ length: n }, (_, i) => {
      const o = osdRows[i % Math.max(osdTotal, 1)];
      const cls = !o ? "dp2" : !o.up ? "dp5" : o.in_cluster ? "dp1" : "dp3";
      return { cls, title: o ? `osd.${o.osd_num ?? o.id} · ${o.up ? "up" : "down"}` : `cell ${i + 1}` };
    });
  }, [osdRows, osdTotal]);

  const ledger = useMemo(() => {
    const items: { t: string; cls: string; html: string }[] = [];
    for (const a of (alerts || []).slice(0, 4)) {
      const t = a.created_at ? new Date(a.created_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : "—";
      const cls = a.severity === "critical" ? "dp5" : a.severity === "warning" ? "dp4" : "dp3";
      items.push({
        t,
        cls,
        html: `${a.title}${a.resource_id ? ` · <code>${a.resource_id}</code>` : ""}`,
      });
    }
    if (deepest && deepest.pct >= 40) {
      items.push({
        t: "now",
        cls: deepestDepth?.cls || "dp3",
        html: `Pool <code>${deepest.name}</code> at ${Math.round(deepest.pct)}% — ${deepestDepth?.name}`,
      });
    }
    if (forecastLine) {
      items.push({ t: "fwd", cls: "dp2", html: forecastLine });
    }
    return items.slice(0, 6);
  }, [alerts, deepest, deepestDepth, forecastLine]);

  return (
    <DashboardHero
      eyebrow={
        <>
          CLUSTER · {clusterName}
          {hostCount ? ` · ${hostCount} host${hostCount === 1 ? "" : "s"}` : ""}
        </>
      }
      title="Overview"
      state={<span dangerouslySetInnerHTML={{ __html: stateLine }} />}
      actions={
        <button type="button" className="at-btn primary" onClick={() => nav("/volumes")}>
          New volume
        </button>
      }
    >
      {/* Hero capacity module */}
      <DashboardModule>
        <div className="at-sounding">
          <div className="at-sound-orb">
            <SoundingOrb pct={usedPct} />
          </div>
          <div className="at-sound-read">
            <div className="at-caption">Surveyed capacity</div>
            <div className="at-val xl">
              {usedParts ? (
                <>
                  {usedParts.mag}
                  <span className="at-unit">{usedParts.unit} used</span>
                </>
              ) : (
                <>
                  —
                  <span className="at-unit">not surveyed</span>
                </>
              )}
            </div>
            <div className="at-sub">
              {hasSurvey
                ? `of ${fmtBytesOpt(s?.raw_capacity_bytes)} raw · ${fmtBytesOpt(s?.available_capacity_bytes)} free`
                : "Waiting on cluster capacity discovery"}
            </div>
            <div className="at-strata" title="used / free">
              <i className="used" style={{ width: `${hasSurvey ? usedPct : 0}%` }} />
            </div>
            <div className="at-strata-key">
              <span className="at-skey">
                <span className="sw" style={{ background: "linear-gradient(90deg,var(--d2),var(--d3))" }} />
                Used <b>{hasSurvey ? fmtBytesOpt(s?.used_capacity_bytes) : "—"}</b>
              </span>
              <span className="at-skey">
                <span className="sw" style={{ background: "var(--at-ridge)" }} />
                Free <b>{hasSurvey ? fmtBytesOpt(s?.available_capacity_bytes) : "—"}</b>
              </span>
            </div>
            <div className="at-sub" style={{ marginTop: 16 }}>
              {forecastLine || "Usage steady — no near-term fill"}
            </div>
          </div>
          <div className="at-sound-io">
            <div style={{ display: "flex", justifyContent: "space-between", gap: 24, flexWrap: "wrap" }}>
              <div>
                <div className="at-caption">Client throughput</div>
                <div className="at-io-split">
                  <div>
                    <div className="at-val lg" style={{ color: "var(--d1)" }}>
                      {fmtSi(io?.read_ops_total)}
                      <span className="at-unit">ops read</span>
                    </div>
                    <div className="at-sub" style={{ marginTop: 6 }}>
                      {fmtBytes(io?.read_bytes_total)}
                    </div>
                  </div>
                  <div>
                    <div className="at-val lg" style={{ color: "var(--d3)" }}>
                      {fmtSi(io?.write_ops_total)}
                      <span className="at-unit">ops write</span>
                    </div>
                    <div className="at-sub" style={{ marginTop: 6 }}>
                      {fmtBytes(io?.write_bytes_total)}
                    </div>
                  </div>
                </div>
              </div>
            </div>
            <Echogram readOps={io?.read_ops_total || 0} writeOps={io?.write_ops_total || 0} hist={hist} />
          </div>
        </div>
      </DashboardModule>

      {/* Swipe status strip */}
      <DashboardModule title="At a glance" note="swipe">
        <SwipeRail label="Inventory status">
          <button type="button" className="at-cell" onClick={() => nav("/volumes")}>
            <span className="at-cell-go">
              <ArrowUpRight size={14} />
            </span>
            <div className="at-caption">Volumes</div>
            <div className="at-val md">
              {s?.volumes ?? 0}
              <span className="at-unit">online</span>
            </div>
            {!s?.volumes ? (
              <div className="at-fill-hint">
                No volumes yet.{" "}
                <button type="button" onClick={() => nav("/volumes")}>
                  Create a volume →
                </button>
              </div>
            ) : (
              <div className="at-delta">block + filesystem</div>
            )}
          </button>
          <button type="button" className="at-cell" onClick={() => nav("/snapshots")}>
            <span className="at-cell-go">
              <ArrowUpRight size={14} />
            </span>
            <div className="at-caption">Snapshots</div>
            <div className={`at-val md ${(s?.snapshots || 0) === 0 ? "at-empty" : ""}`}>{s?.snapshots ?? 0}</div>
            {(s?.snapshots || 0) === 0 ? (
              <div className="at-fill-hint">
                No point-in-time copies yet.{" "}
                <button
                  type="button"
                  onClick={(e) => {
                    e.stopPropagation();
                    sendPrompt("Create a nightly snapshot schedule with 14-day retention for the busiest pools.");
                  }}
                >
                  Schedule nightly snapshots →
                </button>
              </div>
            ) : (
              <div className="at-delta">protection points</div>
            )}
          </button>
          <button type="button" className="at-cell" onClick={() => nav("/buckets")}>
            <span className="at-cell-go">
              <ArrowUpRight size={14} />
            </span>
            <div className="at-caption">Buckets / Backups</div>
            <div className={`at-val md ${(s?.buckets || 0) === 0 && (s?.backups || 0) === 0 ? "at-empty" : ""}`}>
              {s?.buckets ?? 0} <span className="at-unit">/ {s?.backups ?? 0}</span>
            </div>
            {(s?.buckets || 0) === 0 && (s?.backups || 0) === 0 ? (
              <div className="at-fill-hint">
                Object gateway unused.{" "}
                <button type="button" onClick={() => nav("/buckets")}>
                  Create first bucket →
                </button>
              </div>
            ) : (
              <div className="at-delta">RGW · export-diff</div>
            )}
          </button>
          <button type="button" className="at-cell" onClick={() => nav("/ceph")}>
            <span className="at-cell-go">
              <ArrowUpRight size={14} />
            </span>
            <div className="at-caption">Recovery</div>
            <div className="at-val md">
              {recovering}
              <span className="at-unit">PG rebuilding</span>
            </div>
            <div className="at-delta">
              {rc?.objects_degraded ?? 0} degraded · {rc?.objects_unfound ?? 0} unfound
            </div>
          </button>
          <button type="button" className="at-cell" onClick={() => nav("/ceph")}>
            <span className="at-cell-go">
              <ArrowUpRight size={14} />
            </span>
            <div className="at-caption">OSDs</div>
            <div className="at-val md">
              {osdUp}
              <span className="at-unit">/ {osdTotal} up</span>
            </div>
            <div className="at-delta">
              {osdRows.filter((o) => o.in_cluster).length} in cluster
            </div>
          </button>
        </SwipeRail>
      </DashboardModule>

      {/* Pool basins */}
      <DashboardModule title="Pool soundings" note={`${basinPools.length} pools · sorted by depth`}>
        <div className="at-panel">
          {basinPools.length === 0 && (
            <div className="at-empty-box" style={{ boxShadow: "none", border: "none" }}>
              <div className="at-empty-title">No pools discovered yet</div>
            </div>
          )}
          {basinPools.map((p) => {
            const d = depth(p.pct);
            return (
              <button key={p.id} type="button" className="at-basin" onClick={() => nav(`/pools/${p.id}`)}>
                <div className="at-basin-id">
                  <div className="at-basin-name">
                    {p.name}
                    <span className="at-tag">{p.kind || "pool"}</span>
                    <span className="at-tag" style={{ borderColor: "transparent", background: "transparent", color: "var(--at-ink-4)" }}>
                      {d.name}
                    </span>
                  </div>
                  <div className="at-trough">
                    <span className="tick" style={{ left: "25%" }} />
                    <span className="tick" style={{ left: "50%" }} />
                    <span className="tick" style={{ left: "75%" }} />
                    <span className={`at-level ${d.cls}`} style={{ width: depthWidth(p.pct) }} />
                  </div>
                </div>
                <div className="at-basin-read">
                  {fmtBytes(p.used)}
                  <small>of {fmtBytes(p.total)}</small>
                </div>
                <div className={`at-basin-pct ${d.cls} fg`}>
                  {Math.round(p.pct)}
                  <span style={{ fontSize: 11, color: "var(--at-ink-4)" }}>%</span>
                </div>
              </button>
            );
          })}
        </div>
      </DashboardModule>

      <div className="at-2col">
        <DashboardModule title="Seabed" note={`${osdTotal} OSDs · health tiles`}>
          <div className="at-panel">
            <div className="at-seabed">
              {seabedCells.map((c, i) => (
                <span key={i} className={`at-pg ${c.cls}`} title={c.title} style={{ opacity: 0.42 + (i % 5) * 0.1 }} />
              ))}
            </div>
          </div>
        </DashboardModule>

        <DashboardModule title="Ledger" note="alerts · soundings">
          <div className="at-panel at-ledger">
            {ledger.length === 0 && (
              <div style={{ padding: 24, color: "var(--at-ink-4)", fontSize: 13 }}>No recent exceptions.</div>
            )}
            {ledger.map((e, i) => (
              <div key={i} className="at-entry">
                <time>{e.t}</time>
                <span className={`mk ${e.cls}`} />
                <span className="txt" dangerouslySetInnerHTML={{ __html: e.html }} />
              </div>
            ))}
          </div>
        </DashboardModule>
      </div>

      {/* Compact cluster row for operators who still want the table */}
      <DashboardModule title="Clusters" note={`${clusters?.length || 0} registered`}>
        <div className="at-panel">
          {(clusters || []).map((c) => (
            <button
              key={c.id}
              type="button"
              className="at-basin"
              style={{ gridTemplateColumns: "1fr 1fr 1fr 1fr" }}
              onClick={() => nav("/cluster")}
            >
              <span className="mono" style={{ color: "var(--at-ink)" }}>
                {c.name}
              </span>
              <span className="mono" style={{ color: "var(--at-ink-2)" }}>
                {c.health}
              </span>
              <span className="mono" style={{ color: "var(--at-ink-2)" }}>
                {fmtBytesOpt(c.raw_capacity_bytes)}
              </span>
              <span className="mono" style={{ color: "var(--at-ink-2)", textAlign: "right" }}>
                {fmtBytesOpt(c.used_capacity_bytes)} used
              </span>
            </button>
          ))}
          {!clusters?.length && (
            <div style={{ padding: 24, color: "var(--at-ink-4)", fontSize: 13 }}>No clusters.</div>
          )}
        </div>
      </DashboardModule>
    </DashboardHero>
  );
}
