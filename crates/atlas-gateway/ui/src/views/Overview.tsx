// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Overview: a health headline, the cluster itself, a grid of feature tiles, then pools, activity
// and clusters. Every number and shape comes from the gateway; anything unknown says so.
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useNavigate } from "react-router-dom";
import { ChevronRight } from "lucide-react";
import {
  useAlerts,
  useBackendsSummary,
  useCephDf,
  useCephOsdDf,
  useClusters,
  useForecast,
  useHistory,
  useOsds,
  usePools,
  useSummary,
} from "../api/hooks";
import type { Health } from "../api/types";
import {
  CapacityRing,
  ClusterHero,
  EmptyState,
  HealthIndicator,
  PoolUsageBars,
  TrendChart,
  capacityFromSummary,
  formatBytesPerSecond,
  replicaSizesFromPools,
  throughputSeries,
  topologyFromOsds,
} from "../graphics";
import { history, iopsMaxGapMs, iopsTrend, onHistory, recordSummary, seed } from "../store/history";
import { depth } from "../lib/depth";
import { poolFill, poolUsage } from "../lib/poolUsage";
import { sendPrompt } from "../lib/prompts";
import { fmtBytes, fmtBytesOpt, fmtForecastFill, fmtSi, num } from "../lib/format";
import { DashboardHero, DashboardModule } from "../ui/templates/DashboardHero";
import "./Overview.css";

/** Capacity turns amber and red at the same points as lib/depth.ts. */
const FILL = { warnAt: 75, failAt: 90 };

const HEADLINE: Record<Health, string> = {
  ok: "Storage is healthy.",
  warn: "Storage needs attention.",
  critical: "Storage is critical.",
  unknown: "Your storage, at a glance.",
};

/** The worst health any cluster reports; "ok" only when every cluster says so. */
function worstHealth(values: (Health | undefined)[]): Health {
  if (values.includes("critical")) return "critical";
  if (values.includes("warn")) return "warn";
  return values.length && values.every((h) => h === "ok") ? "ok" : "unknown";
}

const dash = (n: number | null | undefined) => (typeof n === "number" ? num(n) : "—");

function Stat({
  label,
  value,
  unit,
  onOpen,
  children,
}: {
  label: string;
  value: ReactNode;
  unit?: string;
  onOpen: () => void;
  children?: ReactNode;
}) {
  return (
    <article className="ov-tile ov-stat a-reveal">
      <h2 className="ov-tile-title">
        <button type="button" className="ov-open" onClick={onOpen}>
          {label}
          <ChevronRight size={16} aria-hidden />
        </button>
      </h2>
      <div className="ov-number">
        {value}
        {unit ? <span className="at-unit">{unit}</span> : null}
      </div>
      <div className="ov-note">{children}</div>
    </article>
  );
}

export default function Overview() {
  const nav = useNavigate();
  const { data: s } = useSummary();
  const { data: clusters } = useClusters();
  const { data: pools } = usePools();
  const { data: osds, isLoading: osdsLoading } = useOsds();
  const { data: osdDf } = useCephOsdDf();
  const { data: alerts } = useAlerts("open");
  const { data: forecast } = useForecast();
  const { data: cephDf } = useCephDf();
  const { data: backends } = useBackendsSummary();
  const io = s?.client_io;
  const rc = s?.recovery;
  const recovering = rc ? (rc.pg_recovering || 0) + (rc.pg_backfilling || 0) : null;
  const [, setTick] = useState(0);
  const { data: serverHist } = useHistory();
  useEffect(() => onHistory(() => setTick((t) => t + 1)), []);
  useEffect(() => {
    if (serverHist) seed(serverHist);
  }, [serverHist]);
  useEffect(() => {
    if (s) recordSummary(s);
  }, [s]);
  const samples = history();
  const iops = iopsTrend(samples);
  const lastRead = iops[0].points.at(-1)?.v;
  const lastWrite = iops[1].points.at(-1)?.v;

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

  const hasSurvey = (s?.raw_capacity_bytes ?? 0) > 0;
  const health = worstHealth((clusters || []).map((c) => c.health));
  const hostCount = new Set(osdRows.map((o) => o.host).filter(Boolean)).size;

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

  const forecastLine = fmtForecastFill(forecast?.days_to_full, forecast?.growth_bytes_per_day);

  const summaryLine: ReactNode[] = [];
  if (!hasSurvey) {
    summaryLine.push("Capacity not surveyed yet.");
  } else {
    summaryLine.push(
      <span key="cap">
        <b>{fmtBytesOpt(s?.used_capacity_bytes)}</b> used of {fmtBytesOpt(s?.raw_capacity_bytes)}.
        {typeof s?.volumes === "number" ? ` ${s.volumes} volume${s.volumes === 1 ? "" : "s"}.` : ""}
      </span>,
    );
  }
  if (health === "warn" || health === "critical") {
    const tip = alerts?.[0]?.title;
    summaryLine.push(
      <span key="health">
        {" "}
        Cluster reports <b className={health === "critical" ? "at-state-crit" : "at-state-warn"}>{health}</b>
        {tip ? ` — ${tip}` : ""}.
      </span>,
    );
  } else if (fullest && fullest.pct >= 60) {
    summaryLine.push(
      <span key="pool">
        {" "}
        Pool <b>{fullest.name}</b> is {Math.round(fullest.pct)}% full.
      </span>,
    );
  } else if (recovering) {
    summaryLine.push(
      <span key="rec">
        {" "}
        <b>{recovering}</b> PG{recovering === 1 ? "" : "s"} rebuilding.
      </span>,
    );
  }

  const ledger: { key: string; t: string; cls: string; body: ReactNode }[] = [];
  for (const a of (alerts || []).slice(0, 4)) {
    const t = a.created_at ? new Date(a.created_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : "—";
    ledger.push({
      key: a.id,
      t,
      cls: a.severity === "critical" ? "dp5" : a.severity === "warning" ? "dp4" : "dp3",
      body: (
        <>
          <span className="sr-only">{a.severity}: </span>
          {a.title}
          {a.resource_id ? (
            <>
              {" "}
              <code>{a.resource_id}</code>
            </>
          ) : null}
        </>
      ),
    });
  }
  if (fullest && fullest.pct >= 40) {
    ledger.push({
      key: "pool",
      t: "now",
      cls: depth(fullest.pct).cls,
      body: (
        <>
          Pool <code>{fullest.name}</code> at {Math.round(fullest.pct)}% of capacity
        </>
      ),
    });
  }
  if (forecastLine) ledger.push({ key: "forecast", t: "next", cls: "dp2", body: forecastLine });

  const noBuckets = s?.buckets === 0 && s?.backups === 0;

  return (
    <DashboardHero
      hero
      eyebrow={
        <>
          Overview
          {clusters?.length === 1 ? ` · ${clusters[0].name}` : ""}
          {(clusters?.length ?? 0) > 1 ? ` · ${clusters!.length} clusters` : ""}
          {hostCount ? ` · ${hostCount} host${hostCount === 1 ? "" : "s"}` : ""}
        </>
      }
      title={HEADLINE[health]}
      state={summaryLine}
      actions={
        <>
          <button type="button" className="at-btn primary large" onClick={() => nav("/volumes")}>
            New volume
          </button>
          <button type="button" className="a-link ov-more" onClick={() => nav("/observatory")}>
            Open Observatory
            <ChevronRight size={16} aria-hidden />
          </button>
        </>
      }
    >
      <ClusterHero
        className="ov-cluster"
        clusterName={
          clusters?.length === 1 ? clusters[0].name : (clusters?.length ?? 0) > 1 ? "All clusters" : undefined
        }
        nodes={topologyFromOsds(osdRows.length ? osdRows : osds)}
        loading={osdsLoading}
        health={clusters?.length ? health : undefined}
        replicaSizes={replicaSizesFromPools(pools)}
        rebuilding={!!recovering}
      />

      <div className="ov-grid">
        <section className="ov-tile ov-capacity" aria-labelledby="ov-capacity-title">
          <h2 id="ov-capacity-title" className="ov-headline">
            Capacity
          </h2>
          <p className="ov-lede">
            {forecastLine || (hasSurvey ? "No near-term fill projected." : "Waiting on cluster capacity discovery.")}
          </p>
          <CapacityRing {...capacityFromSummary(s)} {...FILL} label="Raw capacity" size={280} loading={!s} />
        </section>

        <section className="ov-tile ov-io" aria-labelledby="ov-io-title">
          <h2 id="ov-io-title" className="ov-headline">
            Performance
          </h2>
          <TrendChart
            title="Client throughput"
            series={throughputSeries(serverHist)}
            formatValue={formatBytesPerSecond}
          />
          <div className="ov-iops">
            <div className="at-sub" aria-hidden>
              IOPS, recent
            </div>
            <TrendChart title="Client IOPS" variant="sparkline" series={iops} maxGapMs={iopsMaxGapMs(samples)} />
          </div>
          <dl className="ov-figures">
            <div>
              <dt>Read now</dt>
              <dd>
                {typeof lastRead === "number" ? num(Math.round(lastRead)) : "—"} <small>IOPS</small>
              </dd>
              <dd className="sub">{io ? `${fmtSi(io.read_ops_total)} ops · ${fmtBytes(io.read_bytes_total)}` : "—"}</dd>
            </div>
            <div>
              <dt>Write now</dt>
              <dd>
                {typeof lastWrite === "number" ? num(Math.round(lastWrite)) : "—"} <small>IOPS</small>
              </dd>
              <dd className="sub">{io ? `${fmtSi(io.write_ops_total)} ops · ${fmtBytes(io.write_bytes_total)}` : "—"}</dd>
            </div>
          </dl>
        </section>

        <Stat label="Volumes" value={dash(s?.volumes)} onOpen={() => nav("/volumes")}>
          {s?.volumes === 0 ? (
            <>
              No volumes yet.{" "}
              <button type="button" className="a-link" onClick={() => nav("/volumes")}>
                Create a volume →
              </button>
            </>
          ) : (
            "Block and filesystem"
          )}
        </Stat>
        <Stat label="Snapshots" value={dash(s?.snapshots)} onOpen={() => nav("/snapshots")}>
          {s?.snapshots === 0 ? (
            <>
              No point-in-time copies yet.{" "}
              <button
                type="button"
                className="a-link"
                onClick={() =>
                  sendPrompt("Create a nightly snapshot schedule with 14-day retention for the busiest pools.")
                }
              >
                Schedule nightly snapshots →
              </button>
            </>
          ) : (
            "Protection points"
          )}
        </Stat>
        <Stat
          label="Buckets and backups"
          value={
            <>
              {dash(s?.buckets)}
              <span className="ov-slash">/</span>
              {dash(s?.backups)}
            </>
          }
          onOpen={() => nav("/buckets")}
        >
          {noBuckets ? (
            <>
              Object storage unused.{" "}
              <button type="button" className="a-link" onClick={() => nav("/buckets")}>
                Create first bucket →
              </button>
            </>
          ) : (
            "Object storage"
          )}
        </Stat>
        <Stat label="Recovery" value={dash(recovering)} unit="PG rebuilding" onOpen={() => nav("/ceph")}>
          {rc ? `${num(rc.objects_degraded)} degraded · ${num(rc.objects_unfound)} unfound` : "Not reported"}
        </Stat>
      </div>

      <DashboardModule
        title="Pools"
        note={poolRows ? `${poolRows.length} pool${poolRows.length === 1 ? "" : "s"}, fullest first` : undefined}
        className="a-reveal"
      >
        <div className="at-panel ov-pools">
          <PoolUsageBars pools={poolRows} loading={!pools} onSelect={(p) => nav(`/pools/${p.id}`)} {...FILL} />
        </div>
      </DashboardModule>

      <div className="at-2col">
        <DashboardModule title="Activity" note="Alerts and forecasts" className="a-reveal">
          <div className="at-panel at-ledger">
            {ledger.length === 0 && <EmptyState art="alert" title="No recent exceptions" compact />}
            {ledger.map((e) => (
              <div key={e.key} className="at-entry">
                <time>{e.t}</time>
                <span className={`mk ${e.cls}`} aria-hidden />
                <span className="txt">{e.body}</span>
              </div>
            ))}
          </div>
        </DashboardModule>

        <DashboardModule title="Clusters" note={`${clusters?.length || 0} registered`} className="a-reveal">
          <div className="at-panel">
            {clusters?.length ? (
              <div className="overflow-x-auto">
                <table className="at-tbl">
                  <thead>
                    <tr>
                      <th>Name</th>
                      <th>Health</th>
                      <th>Raw</th>
                      <th>Used</th>
                    </tr>
                  </thead>
                  <tbody>
                    {clusters.map((c) => (
                      <tr key={c.id} className="cursor-pointer" onClick={() => nav("/cluster")}>
                        <td>
                          <button type="button" className="a-link">
                            {c.name}
                          </button>
                        </td>
                        <td>
                          <HealthIndicator health={c.health} />
                        </td>
                        <td className="mono">{fmtBytesOpt(c.raw_capacity_bytes)}</td>
                        <td className="mono">{fmtBytesOpt(c.used_capacity_bytes)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            ) : (
              <EmptyState art="cluster" title="No clusters" compact />
            )}
          </div>
        </DashboardModule>
      </div>
    </DashboardHero>
  );
}
