// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Compact read/light-write Centers: Policies, Backends, Kubernetes, Cluster, Metrics.
import { useNavigate } from "react-router-dom";
import { submit } from "../api/client";
import {
  useBackends,
  useBackendsSummary,
  useCephMetrics,
  useCephOsdDf,
  useClusters,
  useInvalidate,
  useNodes,
  useOsds,
  usePolicies,
  usePools,
  useStorageClasses,
} from "../api/hooks";
import { Badge } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";
import { depth, depthWidth } from "../lib/depth";
import { fmtBytes, fmtBytesOpt, fmtPct, healthKind } from "../lib/format";

export function Policies() {
  const { data } = usePolicies();
  const n = data?.length || 0;
  return (
    <ListPage
      crumbs={navCrumbs("policies")}
      eyebrow="GOVERNANCE · INDEX"
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
          { h: "Description", f: (p) => <span style={{ color: "var(--at-ink-3)" }}>{p.description}</span> },
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
  return (
    <ListPage
      crumbs={navCrumbs("backends")}
      eyebrow="INFRASTRUCTURE · OPS"
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

      <div className="at-mod">
        <div className="at-modhead">
          <span className="at-modtitle">Capacity soundings</span>
          <span className="at-modnote">{(summary || []).length} backends</span>
        </div>
        <div className="at-panel">
          {(summary || []).map((b) => {
            const pct = b.raw_capacity_bytes > 0 ? (b.used_capacity_bytes / b.raw_capacity_bytes) * 100 : 0;
            const d = depth(pct);
            return (
              <button key={b.backend_id} type="button" className="at-basin" style={{ cursor: "default" }}>
                <div className="at-basin-id">
                  <div className="at-basin-name">
                    <span className="mono">{b.backend_type?.toUpperCase?.() || b.backend_type}</span>
                    <span className="at-tag">{d.name.toUpperCase()}</span>
                    <span className="at-tag" style={{ opacity: 0.7 }}>
                      {b.mode}
                    </span>
                  </div>
                  <div className="at-trough">
                    <i className={`at-level ${d.cls}`} style={{ width: depthWidth(pct) }} />
                  </div>
                </div>
                <div className="at-basin-read">
                  {fmtBytes(b.used_capacity_bytes)} of {fmtBytes(b.raw_capacity_bytes)}
                  <div style={{ fontSize: 11, color: "var(--at-ink-4)", marginTop: 4 }}>
                    {b.clusters} cluster(s) · {b.volumes} volume(s)
                  </div>
                </div>
                <div className={`at-basin-pct ${d.cls} fg`}>{fmtPct(pct)}%</div>
              </button>
            );
          })}
          {!summary?.length && (
            <div style={{ padding: 24, color: "var(--at-ink-4)", fontSize: 13 }}>No capacity summary yet.</div>
          )}
        </div>
      </div>

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
      eyebrow="INFRASTRUCTURE · OPS"
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
            f: (s) => (s.is_default ? <Badge kind="success">yes</Badge> : <span style={{ color: "var(--at-ink-4)" }}>—</span>),
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
  const { data: osds } = useOsds();
  const primary = clusters?.[0];
  return (
    <ListPage
      crumbs={navCrumbs("cluster")}
      eyebrow="INFRASTRUCTURE · OPS"
      title="Cluster"
      state={
        primary
          ? `${primary.name} · health ${primary.health} · ${pools?.length || 0} pools · ${osds?.length || 0} OSDs.`
          : "Waiting on cluster inventory…"
      }
      actions={
        <button type="button" className="at-btn" onClick={() => nav("/ceph")}>
          Ceph detail
        </button>
      }
    >
      <div className="at-instrs">
        <div className="at-instr">
          <div className="at-caption">Health</div>
          <div className="at-val md" style={{ textTransform: "uppercase" }}>
            {primary?.health || "—"}
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
          <div className="at-delta">{nodes?.length || 0} node(s)</div>
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
            { h: "Used", f: (p) => fmtBytesOpt(p.used_bytes) },
            { h: "Max", f: (p) => fmtBytesOpt(p.max_bytes) },
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
  const n = data?.length || 0;
  const avg = osdDf?.summary?.average_utilization;
  return (
    <ListPage
      crumbs={navCrumbs("metrics")}
      eyebrow="OBSERVABILITY · INDEX"
      title="Metrics"
      state={
        n
          ? `${n} Ceph metric sample${n === 1 ? "" : "s"}${avg != null ? ` · OSD avg ${avg.toFixed(1)}%` : ""}.`
          : "Ceph-native metrics stream — waiting on samples."
      }
    >
      <Table
        soundings
        panelTitle="Metric samples"
        rows={data}
        rowKey={(_, i) => String(i)}
        empty="No metrics yet."
        cols={[
          { h: "Name", f: (m) => m.name, mono: true },
          { h: "Value", f: (m) => String(m.value), mono: true },
          { h: "Labels", f: (m) => <span className="mono" style={{ color: "var(--at-ink-4)" }}>{JSON.stringify(m.labels || {})}</span> },
        ]}
      />
    </ListPage>
  );
}
