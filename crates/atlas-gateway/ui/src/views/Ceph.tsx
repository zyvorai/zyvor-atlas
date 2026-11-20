// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Ceph — live cluster introspection: health checks, quorum/daemons, PG states, client I/O,
// the CRUSH OSD tree, and per-pool df. Backed by /ceph/status, /ceph/osd-tree, /ceph/df.
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { ChevronDown, ChevronRight } from "lucide-react";
import {
  useCephDf,
  useCephHealthRollup,
  useCephOsdDf,
  useCephOsdTree,
  useCephStatus,
  usePools,
} from "../api/hooks";
import { Badge, TerminalPane } from "../ui/kit";
import { DashboardHero, DashboardModule } from "../ui/templates/DashboardHero";
import { navCrumbs } from "../nav/routes";
import { SwipeRail } from "../ui/SwipeRail";
import { Table } from "../ui/Table";
import { depth, depthWidth } from "../lib/depth";
import { fmtBytes, fmtPct, fmtSi, num } from "../lib/format";

const healthKind = (s?: string) => (s === "HEALTH_OK" ? "success" : s === "HEALTH_ERR" ? "danger" : "warning");
const ROLLUP_LABEL: Record<string, string> = {
  healthy: "Healthy",
  degraded: "Degraded",
  rebuilding: "Rebuilding",
  at_risk: "At Risk",
  critical: "Critical",
};
const rollupKind = (s?: string) =>
  s === "healthy"
    ? "success"
    : s === "degraded"
      ? "warning"
      : s === "rebuilding"
        ? "info"
        : s === "at_risk"
          ? "at-risk"
          : s === "critical"
            ? "danger"
            : "neutral";
const PG_COLOR = (state: string) =>
  state.includes("clean")
    ? "var(--d1)"
    : state.includes("degraded") || state.includes("undersized")
      ? "var(--at-warn)"
      : state.includes("down") || state.includes("stale") || state.includes("incomplete")
        ? "var(--at-fail)"
        : "var(--d3)";

function bps(n?: number) {
  return fmtBytes(n || 0) + "/s";
}

export default function Ceph() {
  const nav = useNavigate();
  const { data: st } = useCephStatus();
  const { data: rollup } = useCephHealthRollup();
  const { data: tree } = useCephOsdTree();
  const { data: df } = useCephDf();
  const { data: osdDf } = useCephOsdDf();
  const { data: pools } = usePools();

  const util: Record<number, any> = {};
  (osdDf?.nodes || []).forEach((o: any) => (util[o.id] = o));
  const fullest = (osdDf?.nodes || [])
    .filter((o: any) => o.status === "up")
    .sort((a: any, b: any) => (b.utilization || 0) - (a.utilization || 0))[0];
  const utilColor = (p: number) => (p >= 85 ? "var(--at-fail)" : p >= 70 ? "var(--at-warn)" : "var(--d1)");

  const health = st?.health?.status;
  const checks: any[] = st?.health?.checks ? Object.entries(st.health.checks).map(([k, v]: any) => ({ k, ...v })) : [];
  const healthWhy =
    checks[0]?.summary?.message ||
    checks[0]?.k ||
    (health === "HEALTH_WARN" ? "Often too-few OSDs or undersized PGs on lab clusters" : undefined);
  const osd = st?.osdmap || {};
  const pgs: any[] = st?.pgmap?.pgs_by_state || [];
  const totalPg = st?.pgmap?.num_pgs || pgs.reduce((a, p) => a + p.count, 0) || 1;
  const io = st?.pgmap;

  const nodes: any[] = tree?.nodes || [];
  const byId: Record<number, any> = {};
  nodes.forEach((n) => (byId[n.id] = n));
  const roots = nodes.filter((n) => n.type === "root");

  const [collapsed, setCollapsed] = useState<Set<number>>(new Set());
  const toggleNode = (id: number) =>
    setCollapsed((s) => {
      const n = new Set(s);
      if (n.has(id)) n.delete(id);
      else n.add(id);
      return n;
    });

  const inventoryPools = pools || [];

  return (
    <DashboardHero
      crumbs={navCrumbs("ceph")}
      eyebrow="INFRA · OPS CONSOLE"
      title="Ceph"
      state={
        health
          ? `${rollup ? `${ROLLUP_LABEL[rollup.state] || rollup.state} — ${rollup.summary}. ` : ""}${health}${healthWhy ? ` — ${healthWhy}` : ""}${fullest ? ` · fullest OSD ${Math.round(fullest.utilization || 0)}%` : ""}.`
          : "Waiting on ceph status…"
      }
      actions={
        <button type="button" className="at-btn" onClick={() => nav("/cluster")}>
          Cluster inventory
        </button>
      }
    >
      <DashboardModule>
        <SwipeRail label="Ceph instruments">
        <div className="at-instr">
          <div className="at-caption">Atlas rollup</div>
          <div className="at-val md">
            <Badge kind={rollupKind(rollup?.state)} dot title={rollup?.reasons?.join("; ")}>
              {rollup ? ROLLUP_LABEL[rollup.state] || rollup.state : "…"}
            </Badge>
          </div>
          <div className="at-delta">{rollup?.summary || "—"}</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Health</div>
          <div className="at-val md">
            <Badge kind={healthKind(health)} dot title={healthWhy}>
              {health || "…"}
            </Badge>
          </div>
          <div className="at-delta">
            {healthWhy ||
              (st?.quorum_names ? `mon quorum ${st.quorum_names.length}/${st.monmap?.num_mons ?? "?"}` : "—")}
          </div>
        </div>
        <div className="at-instr">
          <div className="at-caption">OSDs up / in</div>
          <div className="at-val md">
            {num(osd.num_up_osds)}
            <span className="at-unit">/ {num(osd.num_in_osds)}</span>
          </div>
          <div className="at-delta">{num(osd.num_osds)} total</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">{fullest ? "Fullest OSD" : "Placement groups"}</div>
          <div className="at-val md" style={fullest ? { color: utilColor(fullest.utilization) } : undefined}>
            {fullest ? `${fullest.utilization.toFixed(1)}%` : num(totalPg)}
          </div>
          <div className="at-delta">
            {fullest
              ? `${fullest.name} · avg ${(osdDf?.summary?.average_utilization ?? 0).toFixed(1)}%`
              : `${pgs.length} state(s)`}
          </div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Client I/O</div>
          <div className="at-val md" style={{ fontSize: 18 }}>
            {bps(io?.read_bytes_sec)}
            <span className="at-unit">read</span>
          </div>
          <div className="at-delta">
            {bps(io?.write_bytes_sec)} write · {fmtSi(io?.read_op_per_sec)}/{fmtSi(io?.write_op_per_sec)} ops
          </div>
        </div>
        </SwipeRail>
      </DashboardModule>

      {checks.length > 0 && (
        <DashboardModule>
          <div className="at-panel">
            <div className="at-panel-bar">
              <span className="at-caption">Health checks</span>
              <span className="grow" />
              <Badge kind="warning">{checks.length}</Badge>
            </div>
            {checks.map((c) => (
              <div key={c.k} className="at-list-row">
                <Badge kind={c.severity === "HEALTH_ERR" ? "danger" : "warning"}>{c.k}</Badge>
                <span style={{ fontSize: 13, color: "var(--at-ink-3)" }}>{c.summary?.message || ""}</span>
              </div>
            ))}
          </div>
        </DashboardModule>
      )}

      <DashboardModule>
        <div className="at-panel">
          <div className="at-panel-bar">
            <span className="at-caption">Placement-group states</span>
            <span className="grow" />
            <span className="at-sub" style={{ margin: 0 }}>
              {num(totalPg)} PGs
            </span>
          </div>
          <div style={{ padding: 16 }}>
            <div className="at-pg-bar">
              {pgs.map((p) => (
                <div
                  key={p.state_name}
                  style={{
                    width: `${(p.count / totalPg) * 100}%`,
                    background: PG_COLOR(p.state_name),
                  }}
                  title={`${p.state_name} · ${p.count}`}
                />
              ))}
            </div>
            <div style={{ display: "flex", flexWrap: "wrap", gap: 8 }}>
              {pgs.map((p) => (
                <span key={p.state_name} className="at-pg-chip">
                  <span style={{ width: 8, height: 8, background: PG_COLOR(p.state_name) }} />
                  {p.state_name}
                  <b style={{ color: "var(--at-ink)" }}>{p.count}</b>
                </span>
              ))}
              {!pgs.length && <span style={{ color: "var(--at-ink-4)", fontSize: 13 }}>No PG state data.</span>}
            </div>
          </div>
        </div>
      </DashboardModule>

      <DashboardModule>
        <div className="at-panel">
          <div className="at-panel-bar">
            <span className="at-caption">CRUSH map · OSD tree</span>
            <span className="grow" />
            <span className="at-sub" style={{ margin: 0 }}>
              {nodes.filter((n) => n.type === "osd").length} OSDs
            </span>
          </div>
          <div style={{ padding: 12 }}>
            <TerminalPane title="ceph osd tree" chrome>
              <div className="mono" style={{ fontSize: 12.5, color: "#e5e5e5" }}>
            {roots.map((r) => {
              const rOpen = !collapsed.has(r.id);
              return (
                <div key={r.id}>
                  <button
                    type="button"
                    onClick={() => toggleNode(r.id)}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 8,
                      padding: "6px 0",
                      background: "none",
                      border: "none",
                      color: "#e5e5e5",
                      cursor: "pointer",
                      font: "inherit",
                    }}
                  >
                    {rOpen ? <ChevronDown size={14} color="#86868b" /> : <ChevronRight size={14} color="#86868b" />}
                    <span className="term-key">{r.type}</span> {r.name}
                  </button>
                  {rOpen &&
                    (r.children || []).map((hid: number) => {
                      const host = byId[hid];
                      if (!host) return null;
                      const hOpen = !collapsed.has(hid);
                      return (
                        <div key={hid} style={{ marginLeft: 20 }}>
                          <button
                            type="button"
                            onClick={() => toggleNode(hid)}
                            style={{
                              display: "flex",
                              alignItems: "center",
                              gap: 8,
                              padding: "6px 0",
                              background: "none",
                              border: "none",
                              color: "#e5e5e5",
                              cursor: "pointer",
                              font: "inherit",
                            }}
                          >
                            {hOpen ? (
                              <ChevronDown size={13} color="#86868b" />
                            ) : (
                              <ChevronRight size={13} color="#86868b" />
                            )}
                            <span className="term-num">host</span> {host.name}
                          </button>
                          {hOpen && (
                            <div style={{ marginLeft: 24 }}>
                              {(host.children || []).map((oid: number) => {
                                const o = byId[oid];
                                if (!o) return null;
                                const up = o.status === "up";
                                const u = util[oid];
                                const pct = u?.utilization || 0;
                                const d = depth(pct);
                                return (
                                  <div
                                    key={oid}
                                    style={{
                                      display: "flex",
                                      alignItems: "center",
                                      gap: 12,
                                      padding: "4px 0",
                                      flexWrap: "wrap",
                                    }}
                                  >
                                    <span style={{ width: 72, color: "#a1a1a6" }}>{o.name}</span>
                                    <Badge kind={up ? "success" : "danger"} dot>
                                      {o.status}
                                    </Badge>
                                    {u && up ? (
                                      <>
                                        <div className={`at-mini ${d.cls}`} style={{ width: 96 }}>
                                          <i style={{ width: depthWidth(pct) }} />
                                        </div>
                                        <span className="term-num" style={{ fontSize: 11 }}>
                                          {pct.toFixed(1)}%
                                        </span>
                                        <span style={{ fontSize: 11, color: "#86868b" }}>
                                          {u.pgs} PGs · {u.device_class}
                                        </span>
                                      </>
                                    ) : (
                                      <span style={{ fontSize: 11, color: "#86868b" }}>
                                        weight {(o.crush_weight ?? 0).toFixed?.(1) ?? o.crush_weight} · reweight {o.reweight}
                                      </span>
                                    )}
                                  </div>
                                );
                              })}
                            </div>
                          )}
                        </div>
                      );
                    })}
                </div>
              );
            })}
            {!nodes.length && <div style={{ color: "#86868b" }}>No OSD tree.</div>}
              </div>
            </TerminalPane>
          </div>
        </div>
      </DashboardModule>

      <DashboardModule title="Pool soundings" note="from inventory · click for detail">
        <div className="at-panel" style={{ marginBottom: 16 }}>
          {inventoryPools.slice(0, 8).map((p) => {
            const max = p.max_bytes || 0;
            const used = p.used_bytes || 0;
            const pct = max > 0 ? (used / max) * 100 : 0;
            const d = depth(pct);
            return (
              <button key={p.id} type="button" className="at-basin" onClick={() => nav(`/pools/${p.id}`)}>
                <div className="at-basin-id">
                  <div className="at-basin-name">
                    <span className="mono">{p.name}</span>
                    <span className="at-tag">{p.kind}</span>
                  </div>
                  <div className="at-trough">
                    <i className={`at-level ${d.cls}`} style={{ width: depthWidth(pct) }} />
                  </div>
                </div>
                <div className="at-basin-read">
                  {fmtBytes(used)} of {fmtBytes(max)}
                </div>
                <div className={`at-basin-pct ${d.cls} fg`}>{fmtPct(pct)}%</div>
              </button>
            );
          })}
          {!inventoryPools.length && (
            <div style={{ padding: 20, color: "var(--at-ink-4)", fontSize: 13 }}>No inventory pools yet.</div>
          )}
        </div>
      </DashboardModule>

      <Table
        soundings
        panelTitle="Pool usage · ceph df"
        rows={df?.pools}
        rowKey={(p: any) => String(p.id)}
        empty="No ceph df pools."
        cols={[
          { h: "Pool", f: (p: any) => p.name, mono: true },
          { h: "Objects", f: (p: any) => num(p.stats?.objects) },
          { h: "Stored", f: (p: any) => fmtBytes(p.stats?.stored) },
          { h: "% used", f: (p: any) => ((p.stats?.percent_used || 0) * 100).toFixed(2) + "%" },
          { h: "Max avail", f: (p: any) => fmtBytes(p.stats?.max_avail) },
        ]}
      />
    </DashboardHero>
  );
}
