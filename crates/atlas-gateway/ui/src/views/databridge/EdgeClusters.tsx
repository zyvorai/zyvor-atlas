// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { Link, useNavigate } from "react-router-dom";
import { useEdgeClusters } from "../../api/hooks";
import { Badge } from "../../ui/kit";
import { ListPage } from "../../ui/templates/ListPage";
import { navCrumbs } from "../../nav/routes";
import { Table } from "../../ui/Table";
import { fmtBytes, num } from "../../lib/format";

const kind = (s: string) => (s === "ready" ? "success" : s === "degraded" ? "danger" : "warning");

export default function EdgeClusters() {
  const nav = useNavigate();
  const { data } = useEdgeClusters();
  const n = data?.length || 0;
  return (
    <ListPage
      crumbs={navCrumbs("edge-clusters")}
      eyebrow="DATABRIDGE · INDEX"
      title="Edge DB Clusters"
      state={
        n
          ? `${n} edge cluster${n === 1 ? "" : "s"} on Ceph RBD (CloudNativePG / MySQL operator).`
          : "No edge clusters yet — provision one from a migration plan."
      }
    >
      <Table
        soundings
        panelTitle="Edge cluster index"
        rows={data}
        rowKey={(r) => r.id}
        empty="No edge clusters yet — provision one from a migration plan."
        emptyCta={
          <button type="button" className="at-btn primary" onClick={() => nav("/databridge/plans")}>
            Open Migration Plans
          </button>
        }
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
                  className="mono"
                  style={{ color: "var(--at-cyan)" }}
                  onClick={(e) => e.stopPropagation()}
                >
                  {r.plan_id}
                </Link>
              ) : (
                <span style={{ color: "var(--at-ink-4)" }}>—</span>
              ),
          },
          { h: "Instances", f: (r) => num(r.instances) },
          { h: "Size", f: (r) => fmtBytes(r.size_bytes) },
          { h: "Storage class", f: (r) => <span className="mono" style={{ color: "var(--at-ink-4)" }}>{r.storage_class}</span> },
          { h: "Endpoint", f: (r) => <span className="mono" style={{ color: "var(--at-ink-4)" }}>{r.service_endpoint || "—"}</span> },
          { h: "State", f: (r) => <Badge kind={kind(r.state)} dot>{r.state}</Badge> },
        ]}
      />
    </ListPage>
  );
}
