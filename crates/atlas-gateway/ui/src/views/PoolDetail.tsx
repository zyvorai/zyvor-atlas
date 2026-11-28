// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Pool detail: headline figures, how full the pool is, its open alerts, then its volumes (or the
// cluster's disks when none are linked). Fill levels come from lib/poolUsage, never used / max.
import { useEffect, useMemo } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useAlerts, useBackendsSummary, useCephDf, useClusters, useOsds, usePools, useVolumes } from "../api/hooks";
import type { Osd } from "../api/types";
import { CapacityRing, EmptyState, HealthIndicator, PoolUsageBars, StatusGlyph, healthMeta } from "../graphics";
import { formatPercent } from "../graphics/format";
import { fmtBytes, fmtBytesParts } from "../lib/format";
import { poolFill, poolUsage } from "../lib/poolUsage";
import { sendPrompt } from "../lib/prompts";
import { DetailPage, type DetailStat } from "../ui/templates/DetailPage";
import { DashboardModule } from "../ui/templates/DashboardHero";
import { UsageCell } from "./Volumes";
import "./PoolDetail.css";

/** Capacity turns amber and red at the same points as lib/depth.ts. */
const FILL = { warnAt: 75, failAt: 90 };
const VOLUME_ROWS = 48;

const CRUMBS = [
  { label: "Storage", to: "/" },
  { label: "Pools", to: "/ceph" },
];

const bytesStat = (label: string, bytes: number | null, delta: string): DetailStat => {
  const parts = fmtBytesParts(bytes);
  return { label, value: parts?.mag ?? "—", unit: parts?.unit, delta };
};

function diskState(o: Osd) {
  if (!o.up) return { tone: "fail", word: "down" } as const;
  if (!o.in_cluster) return { tone: "warn", word: "up, out" } as const;
  return { tone: "ok", word: "up, in" } as const;
}

export default function PoolDetail() {
  const { id = "" } = useParams();
  const nav = useNavigate();
  const { data: pools } = usePools();
  const { data: clusters } = useClusters();
  const { data: backends } = useBackendsSummary();
  const { data: cephDf } = useCephDf();
  const { data: vols } = useVolumes();
  const { data: osds } = useOsds();
  const { data: alerts } = useAlerts("open");

  const pool = useMemo(() => (pools || []).find((p) => p.id === id), [pools, id]);
  const cluster = useMemo(
    () => (clusters || []).find((c) => c.id === pool?.cluster_id),
    [clusters, pool],
  );
  const poolVols = useMemo(
    () => (vols || []).filter((v) => v.pool_id === id || (!v.pool_id && pool && v.name.includes(pool.name))),
    [vols, id, pool],
  );
  const clusterOsds = useMemo(
    () =>
      (osds || [])
        .filter((o) => o.cluster_id === pool?.cluster_id)
        .sort((a, b) => (a.osd_num ?? a.id) - (b.osd_num ?? b.id)),
    [osds, pool],
  );
  const usage = useMemo(
    () => (pool ? poolUsage([pool], { clusters, backends, cephDf })![0] : null),
    [pool, clusters, backends, cephDf],
  );

  useEffect(() => {
    if (pool) document.title = `Atlas · ${pool.name}`;
  }, [pool]);

  const poolAlerts = useMemo(() => {
    if (!pool) return [];
    return (alerts || [])
      .slice(0, 5)
      .filter(
        (a) =>
          a.resource_id === id ||
          a.resource_id === pool.name ||
          a.title.toLowerCase().includes(pool.name.toLowerCase()),
      );
  }, [alerts, id, pool]);

  if (!pool && pools) {
    return (
      <DetailPage
        crumbs={[...CRUMBS, { label: "Not found" }]}
        eyebrow={null}
        title="Pool not found"
        state="This pool is not in the inventory. Discovery may have removed it."
        actions={
          <button type="button" className="at-btn" onClick={() => nav("/")}>
            Overview
          </button>
        }
      >
        <div className="at-panel">
          <EmptyState art="pool" title="Nothing to show" description="Open the Overview to pick another pool." />
        </div>
      </DetailPage>
    );
  }

  const pct = usage ? poolFill(usage) : null;
  const used = usage?.usedBytes ?? null;
  const capacity = usage?.capacityBytes ?? null;
  const available = usage?.availableBytes ?? null;
  const fill = pct === null ? "fill level not reported" : `${formatPercent(pct)} full`;
  const pressure = pct === null || pct < FILL.warnAt ? "" : pct >= FILL.failAt ? " Nearly full." : " Filling up.";

  const stateLine = !pool
    ? "Loading pool…"
    : pool.health === "ok"
      ? `Healthy ${pool.kind} pool, ${fill}.${pressure}`
      : `Health: ${healthMeta(pool.health).word.toLowerCase()}. ${pool.kind} pool, ${fill}.${pressure}`;

  // Ceph reports space still available; other drivers report a total, so free is the remainder.
  const free = capacity !== null && used !== null ? Math.max(0, capacity - used) : null;
  const stats: DetailStat[] = [
    bytesStat("Used", used, pool ? fill : "—"),
    available !== null
      ? bytesStat("Available", available, "Space still free")
      : bytesStat("Free", free, capacity !== null && capacity > 0 ? `of ${fmtBytes(capacity)}` : "Capacity not reported"),
    {
      label: "Health",
      value: <HealthIndicator health={pool?.health} size="lg" />,
      delta: `Cluster: ${healthMeta(cluster?.health).word.toLowerCase()}`,
    },
    {
      label: "Replicas",
      value: pool?.replica_size ?? "—",
      delta: <span className="mono">{pool?.device_class || pool?.kind || "—"}</span>,
    },
  ];

  const shownVols = poolVols.slice(0, VOLUME_ROWS);

  return (
    <DetailPage
      crumbs={[...CRUMBS, { label: pool?.name || id }]}
      eyebrow={pool ? `${cluster?.name || pool.cluster_id} · ${pool.kind}` : null}
      title={pool?.name || "Pool"}
      state={stateLine}
      actions={
        <>
          <button type="button" className="at-btn" onClick={() => nav("/")}>
            Overview
          </button>
          <Link to="/volumes" className="at-btn">
            Volumes
          </Link>
          <button
            type="button"
            className="at-btn primary"
            onClick={() =>
              sendPrompt(
                `Analyse pool ${pool?.name || id}: ${fill}, health ${pool?.health}, and recommend capacity actions.`,
              )
            }
          >
            Ask Atlas
          </button>
        </>
      }
      stats={stats}
    >
      <div className="at-2col">
        <DashboardModule title="Capacity">
          <div className="at-panel pd-fill">
            {usage && capacity !== null ? (
              <CapacityRing label={usage.name} usedBytes={used} totalBytes={capacity} {...FILL} />
            ) : (
              <>
                <PoolUsageBars pools={usage ? [usage] : null} loading={!pool} {...FILL} />
                {typeof usage?.percentUsed === "number" ? (
                  <p className="pd-note">Fill level and available space as Ceph reports them.</p>
                ) : null}
              </>
            )}
          </div>
        </DashboardModule>
        <DashboardModule title="Alerts" note="Open, for this pool">
          <div className="at-panel at-ledger">
            {!alerts ? (
              <div className="at-loading">Loading alerts…</div>
            ) : !poolAlerts.length ? (
              <EmptyState art="alert" title="No open alerts for this pool" compact />
            ) : (
              poolAlerts.map((a) => (
                <div key={a.id} className="at-entry">
                  <time>
                    {a.created_at
                      ? new Date(a.created_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
                      : "—"}
                  </time>
                  <span
                    className={`mk ${a.severity === "critical" ? "dp5" : a.severity === "warning" ? "dp4" : "dp3"}`}
                    aria-hidden
                  />
                  <span className="txt">
                    <span className="sr-only">{a.severity}: </span>
                    {a.title}
                  </span>
                </div>
              ))
            )}
          </div>
        </DashboardModule>
      </div>

      {poolVols.length > 0 ? (
        <DashboardModule
          title="Volumes on this pool"
          note={
            poolVols.length > shownVols.length
              ? `${shownVols.length} of ${poolVols.length} shown`
              : `${poolVols.length} volume${poolVols.length === 1 ? "" : "s"}`
          }
        >
          <div className="at-panel">
            <div className="overflow-x-auto">
              <table className="at-tbl">
                <thead>
                  <tr>
                    <th>Name</th>
                    <th>Kind</th>
                    <th>Size</th>
                    <th>Used</th>
                    <th>State</th>
                  </tr>
                </thead>
                <tbody>
                  {shownVols.map((v) => (
                    <tr key={v.id} className="cursor-pointer" onClick={() => nav(`/volumes?focus=${v.id}`)}>
                      <td className="mono">
                        {/* Keyboard route to the volume: the click bubbles to the row. */}
                        <button type="button" className="a-link">
                          {v.name}
                        </button>
                      </td>
                      <td>{v.kind}</td>
                      <td className="mono">{fmtBytes(v.size_bytes)}</td>
                      <td>
                        <UsageCell used={v.used_bytes} size={v.size_bytes} />
                      </td>
                      <td>{v.state}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        </DashboardModule>
      ) : pool && vols ? (
        clusterOsds.length ? (
          <DashboardModule
            title="Disks in this cluster"
            note={`${clusterOsds.length} OSD${clusterOsds.length === 1 ? "" : "s"}`}
          >
            <div className="at-panel">
              <p className="pd-note pd-osd-note">
                No volumes are linked to this pool. These disks belong to the whole cluster, not only this pool.
              </p>
              <ul className="pd-osds" aria-label="OSDs in this cluster">
                {clusterOsds.map((o) => {
                  const s = diskState(o);
                  return (
                    <li key={o.id}>
                      <StatusGlyph tone={s.tone} size={14} />
                      <span className="mono">osd.{o.osd_num ?? o.id}</span>
                      <span className="pd-osd-state">{s.word}</span>
                    </li>
                  );
                })}
              </ul>
            </div>
          </DashboardModule>
        ) : (
          <div className="at-panel">
            <EmptyState art="volume" title="No volumes on this pool" compact />
          </div>
        )
      ) : null}
    </DetailPage>
  );
}
