// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Ceph — live cluster introspection: health checks, quorum/daemons, PG states, client I/O,
// the CRUSH OSD tree, and per-pool df. Backed by /ceph/status, /ceph/osd-tree, /ceph/df.
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { ChevronDown, ChevronRight } from "lucide-react";
import {
  useBackendsSummary,
  useCephDf,
  useCephHealthRollup,
  useCephOsdDf,
  useCephOsdTree,
  useCephStatus,
  useClusters,
  useOsds,
  usePools,
} from "../api/hooks";
import { ClusterHero, PoolUsageBars, replicaSizesFromPools, topologyFromOsds } from "../graphics";
import { Badge } from "../ui/kit";
import { DashboardHero, DashboardModule } from "../ui/templates/DashboardHero";
import { navCrumbs } from "../nav/routes";
import { SwipeRail } from "../ui/SwipeRail";
import { Table } from "../ui/Table";
import { depth, depthWidth } from "../lib/depth";
import { poolUsage } from "../lib/poolUsage";
import { fmtBytes, fmtBytesOpt, fmtSi, num } from "../lib/format";
import "./Ceph.css";

/** Capacity turns amber and red at the same points as lib/depth.ts. */
const FILL = { warnAt: 75, failAt: 90 };

const healthKind = (s?: string) =>
  !s ? "neutral" : s === "HEALTH_OK" ? "success" : s === "HEALTH_ERR" ? "danger" : "warning";
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
const PG_TONE = (state: string) =>
  state.includes("clean")
    ? "ok"
    : state.includes("degraded") || state.includes("undersized")
      ? "warn"
      : state.includes("down") || state.includes("stale") || state.includes("incomplete")
        ? "fail"
        : "warn";

/** A count Ceph may not have reported yet. */
const count = (n?: number | null) => (typeof n === "number" ? num(n) : "—");

export default function Ceph() {
  const nav = useNavigate();
  const { data: st } = useCephStatus();
  const { data: rollup } = useCephHealthRollup();
  const { data: tree } = useCephOsdTree();
  const { data: df } = useCephDf();
  const { data: osdDf } = useCephOsdDf();
  const { data: pools } = usePools();
  const { data: clusters } = useClusters();
  const { data: backends } = useBackendsSummary();
  const { data: osds, isLoading: osdsLoading } = useOsds();

  const util: Record<number, any> = {};
  (osdDf?.nodes || []).forEach((o: any) => (util[o.id] = o));
  const fullest = (osdDf?.nodes || [])
    .filter((o: any) => o.status === "up")
    .sort((a: any, b: any) => (b.utilization || 0) - (a.utilization || 0))[0];
  const fullestDepth = fullest ? depth(fullest.utilization) : null;

  // Inventory OSDs, with per-disk usage from `ceph osd df` where it reports it.
  const heroOsds = osds?.map((o) => {
    const live = util[o.osd_num ?? o.id];
    if (!live?.kb) return o;
    return {
      ...o,
      used_bytes: o.used_bytes ?? (live.kb_used || 0) * 1024,
      capacity_bytes: o.capacity_bytes ?? live.kb * 1024,
    };
  });

  const health = st?.health?.status;
  const checks: any[] = st?.health?.checks ? Object.entries(st.health.checks).map(([k, v]: any) => ({ k, ...v })) : [];
  const healthWhy =
    checks[0]?.summary?.message ||
    checks[0]?.k ||
    (health === "HEALTH_WARN" ? "Often too-few OSDs or undersized PGs on lab clusters" : undefined);
  const osd = st?.osdmap || {};
  const pgs: any[] = st?.pgmap?.pgs_by_state || [];
  const pgCount: number | null = st?.pgmap?.num_pgs || pgs.reduce((a, p) => a + p.count, 0) || null;
  const io = st?.pgmap;
  // Ceph leaves the rate fields out when the cluster is idle, so a reported pgmap without them is 0.
  const bps = (n?: number) => (io ? `${fmtBytes(n || 0)}/s` : "—");

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

  const poolRows = poolUsage(pools, { clusters, backends, cephDf: df });

  const branch = (n: any) => {
    const open = !collapsed.has(n.id);
    return (
      <button type="button" className="ceph-branch" aria-expanded={open} onClick={() => toggleNode(n.id)}>
        {open ? <ChevronDown size={14} aria-hidden /> : <ChevronRight size={14} aria-hidden />}
        <span className="ceph-type">{n.type}</span> {n.name}
      </button>
    );
  };

  return (
    <DashboardHero
      crumbs={navCrumbs("ceph")}
      eyebrow="Infrastructure"
      title="Ceph"
      state={
        health
          ? `${rollup ? `${ROLLUP_LABEL[rollup.state] || rollup.state} — ${rollup.summary}. ` : ""}${health}${healthWhy ? ` — ${healthWhy}` : ""}${fullest ? ` · fullest OSD ${Math.round(fullest.utilization || 0)}%` : ""}.`
          : "Waiting on Ceph status…"
      }
      actions={
        <button type="button" className="at-btn" onClick={() => nav("/cluster")}>
          Cluster inventory
        </button>
      }
    >
      <DashboardModule title="Cluster">
        <ClusterHero
          className="ceph-hero"
          clusterName="Ceph disks"
          nodes={topologyFromOsds(heroOsds)}
          loading={osdsLoading}
          health={rollup?.state}
          replicaSizes={replicaSizesFromPools(pools)}
          rebuilding={!!rollup?.recovering}
        />
        <SwipeRail label="Ceph summary">
          <div className="at-instr ceph-state">
            <div className="at-caption">Atlas status</div>
            <div className="at-val md">
              <Badge kind={rollupKind(rollup?.state)} dot title={rollup?.reasons?.join("; ")}>
                {rollup ? ROLLUP_LABEL[rollup.state] || rollup.state : "…"}
              </Badge>
            </div>
            <div className="at-delta">{rollup?.summary || "—"}</div>
          </div>
          <div className="at-instr ceph-state">
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
              {count(osd.num_up_osds)}
              <span className="at-unit">/ {count(osd.num_in_osds)}</span>
            </div>
            <div className="at-delta">{count(osd.num_osds)} total</div>
          </div>
          <div className="at-instr">
            <div className="at-caption">{fullest ? "Fullest OSD" : "Placement groups"}</div>
            <div className={`at-val md ceph-level ${fullestDepth?.cls ?? ""}`}>
              {fullest ? `${(fullest.utilization || 0).toFixed(1)}%` : count(pgCount)}
            </div>
            <div className="at-delta">
              {fullest
                ? `${fullest.name} · avg ${(osdDf?.summary?.average_utilization ?? 0).toFixed(1)}%${
                    fullestDepth?.cls === "dp5" ? " · nearly full" : fullestDepth?.cls === "dp4" ? " · filling up" : ""
                  }`
                : st
                  ? `${pgs.length} state(s)`
                  : "—"}
            </div>
          </div>
          <div className="at-instr">
            <div className="at-caption">Client I/O</div>
            <div className="at-val md">
              {bps(io?.read_bytes_sec)}
              <span className="at-unit">read</span>
            </div>
            <div className="at-delta">
              {io
                ? `${bps(io.write_bytes_sec)} write · ${fmtSi(io.read_op_per_sec)}/${fmtSi(io.write_op_per_sec)} ops`
                : "—"}
            </div>
          </div>
        </SwipeRail>
      </DashboardModule>

      {checks.length > 0 && (
        <DashboardModule title="Health checks" note={`${checks.length} reported`}>
          <div className="at-panel">
            {checks.map((c) => (
              <div key={c.k} className="at-list-row">
                <Badge kind={c.severity === "HEALTH_ERR" ? "danger" : "warning"}>{c.k}</Badge>
                <span className="ceph-check">{c.summary?.message || ""}</span>
              </div>
            ))}
          </div>
        </DashboardModule>
      )}

      <DashboardModule title="Placement groups" note={pgCount ? `${num(pgCount)} PGs` : undefined}>
        <div className="at-panel ceph-pgs">
          {pgs.length ? (
            <>
              <div className="at-pg-bar" aria-hidden="true">
                {pgs.map((p) => (
                  <div
                    key={p.state_name}
                    className={`ceph-pg ${PG_TONE(p.state_name)}`}
                    style={{ width: `${(p.count / (pgCount || 1)) * 100}%` }}
                    title={`${p.state_name} · ${p.count}`}
                  />
                ))}
              </div>
              <div className="ceph-pg-chips">
                {pgs.map((p) => (
                  <span key={p.state_name} className="at-pg-chip">
                    <span className={`ceph-pg ceph-pg-key ${PG_TONE(p.state_name)}`} aria-hidden="true" />
                    {p.state_name}
                    <b>{p.count}</b>
                  </span>
                ))}
              </div>
            </>
          ) : (
            <span className="at-empty">No PG state data.</span>
          )}
        </div>
      </DashboardModule>

      <DashboardModule
        title="OSD tree"
        note={tree ? `${nodes.filter((n) => n.type === "osd").length} OSDs · CRUSH map` : "CRUSH map"}
      >
        <div className="at-panel ceph-tree mono">
          {roots.map((r) => (
            <div key={r.id}>
              {branch(r)}
              {!collapsed.has(r.id) &&
                (r.children || []).map((hid: number) => {
                  const host = byId[hid];
                  if (!host) return null;
                  return (
                    <div key={hid} className="ceph-children">
                      {branch(host)}
                      {!collapsed.has(hid) && (
                        <div className="ceph-children">
                          {(host.children || []).map((oid: number) => {
                            const o = byId[oid];
                            if (!o) return null;
                            const up = o.status === "up";
                            const u = util[oid];
                            const pct = u?.utilization || 0;
                            const d = depth(pct);
                            return (
                              <div key={oid} className="ceph-osd">
                                <span className="ceph-osd-name">{o.name}</span>
                                <Badge kind={up ? "success" : "danger"} dot>
                                  {o.status}
                                </Badge>
                                {u && up ? (
                                  <>
                                    <div className={`at-mini ${d.cls}`} aria-hidden="true">
                                      <i style={{ width: depthWidth(pct) }} />
                                    </div>
                                    <span>{pct.toFixed(1)}%</span>
                                    <span className="ceph-meta">
                                      {u.pgs} PGs · {u.device_class}
                                    </span>
                                  </>
                                ) : (
                                  <span className="ceph-meta">
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
          ))}
          {!nodes.length && <div className="ceph-meta">No OSD tree.</div>}
        </div>
      </DashboardModule>

      <DashboardModule title="Pool fill" note="Select a pool for detail">
        <div className="at-panel ceph-pools">
          <PoolUsageBars
            pools={poolRows}
            loading={!pools}
            limit={8}
            onSelect={(p) => nav(`/pools/${p.id}`)}
            {...FILL}
          />
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
          { h: "Objects", f: (p: any) => count(p.stats?.objects) },
          { h: "Stored", f: (p: any) => <span className="ceph-nowrap">{fmtBytesOpt(p.stats?.stored)}</span> },
          {
            h: "% used",
            f: (p: any) =>
              typeof p.stats?.percent_used === "number" ? `${(p.stats.percent_used * 100).toFixed(2)}%` : "—",
          },
          { h: "Available", f: (p: any) => <span className="ceph-nowrap">{fmtBytesOpt(p.stats?.max_avail)}</span> },
        ]}
      />
    </DashboardHero>
  );
}
