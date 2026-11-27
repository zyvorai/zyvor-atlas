// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Compact read/light-write pages: Policies, Backends, Kubernetes, Cluster, Metrics.
import { useNavigate } from "react-router-dom";
import { submit } from "../api/client";
import {
  useBackends,
  useBackendsSummary,
  useCephDf,
  useCephMetrics,
  useCephOsdDf,
  useClusters,
  useHistory,
  useInvalidate,
  useNodes,
  useOsds,
  usePolicies,
  usePools,
  useStorageClasses,
} from "../api/hooks";
import {
  ClusterHero,
  EmptyState,
  HealthIndicator,
  PoolUsageBars,
  TrendChart,
  formatBytesPerSecond,
  iopsSeries,
  replicaSizesFromPools,
  throughputSeries,
  topologyFromOsds,
  type PoolUsage,
} from "../graphics";
import { Badge } from "../ui/kit";
import { DashboardModule } from "../ui/templates/DashboardHero";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";
import { poolFill, poolUsage } from "../lib/poolUsage";
import { fmtBytesOpt, healthKind } from "../lib/format";
import "./Simple.css";

/** Capacity turns amber and red at the same points as lib/depth.ts. */
const FILL = { warnAt: 75, failAt: 90 };

export function Policies() {
  const { data } = usePolicies();
  const n = data?.length || 0;
  return (
    <ListPage
      crumbs={navCrumbs("policies")}
      eyebrow="Governance"
      title="Policies"
      state={
        n
          ? `${n} intent${n === 1 ? "" : "s"} in the built-in placement catalog (atlas-policy).`
          : "Built-in intent → placement catalog (atlas-policy)."
      }
    >
      <Table
        soundings
        panelTitle="Intent catalog"
        rows={data}
        rowKey={(p) => p.intent}
        empty="No policies loaded."
        cols={[
          { h: "Intent", f: (p) => <Badge kind="info">{p.intent}</Badge> },
          { h: "StorageClass", f: (p) => p.storage_class, mono: true },
          { h: "Access", f: (p) => p.access_mode },
          { h: "Volume mode", f: (p) => p.volume_mode },
          { h: "Description", f: (p) => <span className="sm-muted">{p.description}</span> },
        ]}
      />
    </ListPage>
  );
}

const BACKEND_KIND: Record<string, "success" | "info" | "warning" | "neutral"> = {
  ceph: "info",
  nfs: "success",
  zfs: "warning",
  rgw: "neutral",
};

export function Backends() {
  const { data } = useBackends();
  const { data: summary } = useBackendsSummary();
  const inv = useInvalidate();
  const n = data?.length || 0;
  const totals = new Map((summary || []).map((b) => [b.backend_id, b]));
  // Backends report a total, so used / total is a real fill level. 0 of 0 means "not surveyed".
  const bars: PoolUsage[] = (summary || []).map((b) => {
    const total = b.raw_capacity_bytes || 0;
    const used = b.used_capacity_bytes;
    return {
      id: b.backend_id,
      name: `${b.backend_type} · ${b.backend_id}`,
      usedBytes: total > 0 || used > 0 ? used : null,
      capacityBytes: total > 0 ? total : null,
    };
  });
  return (
    <ListPage
      crumbs={navCrumbs("backends")}
      eyebrow="Infrastructure"
      title="Backends"
      state={
        n
          ? `${n} registered backend${n === 1 ? "" : "s"} — discovery and capacity summary.`
          : "No backends registered yet."
      }
      actions={
        <button
          type="button"
          className="at-btn"
          onClick={() =>
            submit("post", "/backends/bkd_ceph_lab/discover", null, "discovery triggered", () => inv("clusters")).catch(
              () => {},
            )
          }
        >
          Discover
        </button>
      }
    >
      <DashboardModule title="Capacity" note={summary ? `${summary.length} backends` : undefined}>
        <div className="at-panel sm-bars">
          {bars.length ? (
            <PoolUsageBars label="Backend capacity" pools={bars} {...FILL} />
          ) : (
            <EmptyState art="cluster" title="No capacity summary yet" compact />
          )}
        </div>
      </DashboardModule>

      <Table
        soundings
        panelTitle="Backend registry"
        rows={data}
        rowKey={(b) => b.id}
        empty="No backends registered."
        cols={[
          { h: "ID", f: (b) => b.id, mono: true },
          { h: "Name", f: (b) => b.name },
          { h: "Type", f: (b) => <Badge kind={BACKEND_KIND[b.backend_type] || "neutral"}>{b.backend_type}</Badge> },
          { h: "Mode", f: (b) => b.mode },
          { h: "Clusters", f: (b) => totals.get(b.id)?.clusters },
          { h: "Volumes", f: (b) => totals.get(b.id)?.volumes },
          { h: "Status", f: (b) => <Badge kind={b.status === "active" ? "success" : "neutral"}>{b.status}</Badge> },
        ]}
      />
    </ListPage>
  );
}

export function Kubernetes() {
  const { data: scs, isError } = useStorageClasses();
  const n = scs?.length || 0;
  return (
    <ListPage
      crumbs={navCrumbs("kubernetes")}
      eyebrow="Infrastructure"
      title="Kubernetes"
      state={
        n
          ? `${n} StorageClass${n === 1 ? "" : "es"} discovered from the cluster.`
          : "No StorageClasses visible — check KUBECONFIG / RBAC."
      }
    >
      <Table
        soundings
        panelTitle="StorageClasses"
        error={isError}
        rows={scs}
        rowKey={(s) => s.name}
        empty="No StorageClasses."
        cols={[
          { h: "Name", f: (s) => s.name, mono: true },
          { h: "Provisioner", f: (s) => s.provisioner, mono: true },
          { h: "Reclaim", f: (s) => s.reclaim_policy || "—" },
          { h: "Binding", f: (s) => s.volume_binding_mode || "—" },
          {
            h: "Default",
            f: (s) => (s.is_default ? <Badge kind="success">yes</Badge> : <span className="sm-muted">—</span>),
          },
        ]}
      />
    </ListPage>
  );
}

export function Cluster() {
  const nav = useNavigate();
  const { data: clusters } = useClusters();
  const { data: nodes } = useNodes();
  const { data: pools } = usePools();
  const { data: osds, isLoading: osdsLoading } = useOsds();
  const { data: backends } = useBackendsSummary();
  const { data: cephDf } = useCephDf();
  const primary = clusters?.[0];
  // What `max_bytes` means differs per driver, so the table shows poolUsage's reading of it.
  const usage = new Map((poolUsage(pools, { clusters, backends, cephDf }) ?? []).map((u) => [u.id, u]));
  const counts = [
    pools ? `${pools.length} pool${pools.length === 1 ? "" : "s"}` : null,
    osds ? `${osds.length} OSD${osds.length === 1 ? "" : "s"}` : null,
  ].filter(Boolean);
  return (
    <ListPage
      crumbs={navCrumbs("cluster")}
      eyebrow="Infrastructure"
      title="Cluster"
      state={
        primary
          ? `${[primary.name, `health ${primary.health}`, ...counts].join(" · ")}.`
          : "Waiting on cluster inventory…"
      }
      actions={
        <button type="button" className="at-btn" onClick={() => nav("/ceph")}>
          Ceph detail
        </button>
      }
    >
      <DashboardModule title="Disks">
        <ClusterHero
          className="sm-hero"
          clusterName={clusters?.length === 1 ? clusters[0].name : (clusters?.length ?? 0) > 1 ? "All clusters" : undefined}
          nodes={topologyFromOsds(osds)}
          loading={osdsLoading}
          replicaSizes={replicaSizesFromPools(pools)}
        />
      </DashboardModule>

      <div className="at-instrs">
        <div className="at-instr">
          <div className="at-caption">Health</div>
          <div className="at-val md">
            {primary ? <HealthIndicator health={primary.health} size="lg" /> : "—"}
          </div>
          <div className="at-delta mono">{primary?.name || "—"}</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Raw</div>
          <div className="at-val md">{fmtBytesOpt(primary?.raw_capacity_bytes)}</div>
          <div className="at-delta">used {fmtBytesOpt(primary?.used_capacity_bytes)}</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Pools</div>
          <div className="at-val md">{pools?.length ?? "—"}</div>
          <div className="at-delta">inventory</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">OSDs</div>
          <div className="at-val md">{osds?.length ?? "—"}</div>
          <div className="at-delta">{nodes ? `${nodes.length} node${nodes.length === 1 ? "" : "s"}` : "—"}</div>
        </div>
      </div>

      <div className="at-stack">
        <Table
          soundings
          panelTitle="Clusters"
          rows={clusters}
          rowKey={(c) => c.id}
          empty="No clusters."
          cols={[
            { h: "Name", f: (c) => c.name, mono: true },
            { h: "Health", f: (c) => <Badge kind={healthKind(c.health)} dot>{c.health}</Badge> },
            { h: "Raw", f: (c) => fmtBytesOpt(c.raw_capacity_bytes) },
            { h: "Used", f: (c) => fmtBytesOpt(c.used_capacity_bytes) },
          ]}
        />
        <Table
          soundings
          panelTitle="Nodes"
          rows={nodes}
          rowKey={(n) => n.host}
          empty="No nodes."
          cols={[{ h: "Host", f: (n) => n.host, mono: true }]}
        />
        <Table
          soundings
          panelTitle="Pools"
          rows={pools}
          rowKey={(p) => p.id}
          onRow={(p) => nav(`/pools/${p.id}`)}
          empty="No pools."
          cols={[
            { h: "Name", f: (p) => p.name, mono: true },
            { h: "Kind", f: (p) => p.kind },
            { h: "Used", f: (p) => fmtBytesOpt(usage.get(p.id)?.usedBytes) },
            { h: "Total", f: (p) => fmtBytesOpt(usage.get(p.id)?.capacityBytes) },
            { h: "Available", f: (p) => fmtBytesOpt(usage.get(p.id)?.availableBytes) },
            {
              h: "Fill",
              f: (p) => {
                const u = usage.get(p.id);
                const pct = u ? poolFill(u) : null;
                return pct == null ? "—" : `${Math.round(pct)}%`;
              },
            },
            { h: "Health", f: (p) => <Badge kind={healthKind(p.health)} dot>{p.health}</Badge> },
          ]}
        />
        <Table
          soundings
          panelTitle="OSDs"
          rows={osds}
          rowKey={(o) => String(o.id)}
          empty="No OSDs."
          cols={[
            { h: "OSD", f: (o) => `osd.${o.osd_num ?? o.id}`, mono: true },
            { h: "Host", f: (o) => o.host || "—" },
            { h: "Up", f: (o) => (o.up ? <Badge kind="success">up</Badge> : <Badge kind="danger">down</Badge>) },
            { h: "In", f: (o) => (o.in_cluster ? "in" : "out") },
            { h: "Class", f: (o) => o.device_class || "—" },
          ]}
        />
      </div>
    </ListPage>
  );
}

export function Metrics() {
  const { data } = useCephMetrics();
  const { data: osdDf } = useCephOsdDf();
  const { data: history, isLoading: historyLoading } = useHistory();
  const n = data?.length || 0;
  const avg = osdDf?.summary?.average_utilization;
  return (
    <ListPage
      crumbs={navCrumbs("metrics")}
      eyebrow="Observability"
      title="Metrics"
      state={
        n
          ? `${n} Ceph metric sample${n === 1 ? "" : "s"}${avg != null ? ` · OSD avg ${avg.toFixed(1)}%` : ""}.`
          : "Ceph-native metrics stream — waiting on samples."
      }
    >
      <DashboardModule title="Client I/O" note="Last hour">
        <div className="at-2col">
          <div className="at-panel sm-chart">
            <TrendChart
              title="Client throughput"
              series={throughputSeries(history)}
              formatValue={formatBytesPerSecond}
              loading={historyLoading}
            />
          </div>
          <div className="at-panel sm-chart">
            <TrendChart title="Client IOPS" series={iopsSeries(history)} loading={historyLoading} />
          </div>
        </div>
      </DashboardModule>

      <Table
        soundings
        panelTitle="Metric samples"
        rows={data}
        rowKey={(_, i) => String(i)}
        empty="No metrics yet."
        cols={[
          { h: "Name", f: (m) => m.name, mono: true },
          { h: "Value", f: (m) => String(m.value), mono: true },
          { h: "Labels", f: (m) => <span className="mono sm-muted">{JSON.stringify(m.labels || {})}</span> },
        ]}
      />
    </ListPage>
  );
}
