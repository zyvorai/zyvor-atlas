// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { Link, useNavigate } from "react-router-dom";
import { useEdgeClusters } from "../../api/hooks";
import { Badge } from "../../ui/kit";
import { ListPage } from "../../ui/templates/ListPage";
import { navCrumbs } from "../../nav/routes";
import { Table } from "../../ui/Table";
import { fmtBytes, num } from "../../lib/format";
import { EmptyState } from "../../graphics";

const kind = (s: string) => (s === "ready" ? "success" : s === "degraded" ? "danger" : "warning");

export default function EdgeClusters() {
  const nav = useNavigate();
  const { data, isError } = useEdgeClusters();
  const n = data?.length || 0;
  const openPlans = (
    <button type="button" className="at-btn primary" onClick={() => nav("/databridge/plans")}>
      Open migration plans
    </button>
  );
  return (
    <ListPage
      crumbs={navCrumbs("edge-clusters")}
      eyebrow="Migration targets"
      title="Edge database clusters"
      state={
        !data
          ? isError
            ? "Could not load edge clusters."
            : "Loading edge clusters…"
          : n
            ? `${n} edge cluster${n === 1 ? "" : "s"} on Ceph RBD.`
            : "No edge clusters yet. Provision one from a migration plan."
      }
    >
      {data && !data.length ? (
        <div className="at-panel">
          <h2 className="sr-only">Clusters</h2>
          <EmptyState art="cluster" title="No edge clusters yet" description="Provisioning a migration plan creates one.">
            {openPlans}
          </EmptyState>
        </div>
      ) : (
        <Table
          soundings
          panelTitle="Clusters"
          rows={data}
          rowKey={(r) => r.id}
          cols={[
            { h: "Name", f: (r) => r.cr_name || r.id, mono: true },
            { h: "Engine", f: (r) => <Badge kind="info">{r.engine}</Badge> },
            { h: "Operator", f: (r) => r.operator },
            {
              h: "Plan",
              f: (r) =>
                r.plan_id ? (
                  <Link
                    to={`/databridge/plans/${r.plan_id}`}
                    className="a-link mono"
                    onClick={(e) => e.stopPropagation()}
                  >
                    {r.plan_id}
                  </Link>
                ) : (
                  <span className="text-muted-foreground">—</span>
                ),
            },
            { h: "Instances", f: (r) => num(r.instances) },
            { h: "Size", f: (r) => fmtBytes(r.size_bytes) },
            { h: "Storage class", f: (r) => <span className="mono text-muted-foreground">{r.storage_class}</span> },
            { h: "Endpoint", f: (r) => <span className="mono text-muted-foreground">{r.service_endpoint || "—"}</span> },
            { h: "State", f: (r) => <Badge kind={kind(r.state)} dot>{r.state}</Badge> },
          ]}
        />
      )}
    </ListPage>
  );
}
