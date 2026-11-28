// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useValidations } from "../../api/hooks";
import type { ValidationRun } from "../../api/types";
import { Badge, Button, SlideOver } from "../../ui/kit";
import { ListPage } from "../../ui/templates/ListPage";
import { navCrumbs } from "../../nav/routes";
import { Table } from "../../ui/Table";
import { EmptyState } from "../../graphics";

export default function Validation() {
  const nav = useNavigate();
  const { data, isError } = useValidations();
  const [detail, setDetail] = useState<ValidationRun | null>(null);
  const n = data?.length || 0;
  const failed = (data || []).filter((r) => r.state === "failed").length;
  return (
    <ListPage
      crumbs={navCrumbs("validation")}
      eyebrow="Pre-cutover checks"
      title="Validation"
      state={
        !data ? (
          isError ? "Could not load validation runs." : "Loading validation runs…"
        ) : !n ? (
          "No runs yet. Validate a plan before cutover."
        ) : failed ? (
          <>
            {n} run{n === 1 ? "" : "s"}, <b className="at-state-crit">{failed} failed</b>.
          </>
        ) : (
          `${n} run${n === 1 ? "" : "s"}, none failed.`
        )
      }
    >
      {data && !data.length ? (
        <div className="at-panel">
          <h2 className="sr-only">Runs</h2>
          <EmptyState art="chart" title="No validation runs yet" description="A run compares the source with the edge database, table by table.">
            <button type="button" className="at-btn primary" onClick={() => nav("/databridge/plans")}>
              Open migration plans
            </button>
          </EmptyState>
        </div>
      ) : (
        <Table
          soundings
          panelTitle="Runs"
          rows={data}
          rowKey={(r) => r.id}
          cols={[
            { h: "Kind", f: (r) => r.kind },
            { h: "Tables", f: (r) => (r.tables_total ? `${r.tables_total - r.tables_mismatched}/${r.tables_total} match` : "—") },
            { h: "State", f: (r) => <Badge kind={r.state === "passed" ? "success" : r.state === "failed" ? "danger" : "warning"} dot>{r.state}</Badge> },
            { h: "When", f: (r) => r.completed_at || r.created_at || "—", mono: true },
          ]}
          actions={(r) => <Button size="sm" onClick={() => setDetail(r)}>Details</Button>}
        />
      )}

      <SlideOver open={!!detail} onClose={() => setDetail(null)} title={<><span className="mono">{detail?.kind}</span> validation</>} width={560}>
        {detail && (
          <Table rows={(detail.summary?.tables || []) as any[]} rowKey={(t: any) => t.table}
            cols={[
              { h: "Table", f: (t: any) => t.table, mono: true },
              { h: "Source rows", f: (t: any) => t.source_rows?.toLocaleString?.() ?? t.source_rows },
              { h: "Edge rows", f: (t: any) => t.edge_rows?.toLocaleString?.() ?? t.edge_rows },
              { h: "Checksum", f: (t: any) => t.checksum_match ? <Badge kind="success">match</Badge> : <Badge kind="danger">mismatch</Badge> },
            ]} empty="No per-table results." />
        )}
      </SlideOver>
    </ListPage>
  );
}
