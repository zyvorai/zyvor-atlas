// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { Plus } from "lucide-react";
import { submit } from "../../api/client";
import { usePlans, useSources, useInvalidate } from "../../api/hooks";
import { Badge, Button, FormModal } from "../../ui/kit";
import { ListPage } from "../../ui/templates/ListPage";
import { navCrumbs } from "../../nav/routes";
import { Table } from "../../ui/Table";
import { EmptyState } from "../../graphics";
import "./Plans.css";

export const planStateKind = (s: string) =>
  s.includes("failed") || s === "rolled_back" ? "danger"
  : s === "completed" || s === "cutover_complete" || s === "validated" ? "success"
  : s === "draft" ? "neutral" : "info";

// migration_plans.state is snake_case (e.g. "cdc_streaming", "cutover_complete") — CSS
// text-transform:capitalize only capitalizes whitespace-delimited words, so it leaves the
// underscore in place ("Cdc_streaming"). Replace it with a space so each word capitalizes.
export const planStateLabel = (s: string) => s.replace(/_/g, " ");

export default function Plans() {
  const { data, isError } = usePlans();
  const { data: sources } = useSources();
  const inv = useInvalidate();
  const nav = useNavigate();
  const [create, setCreate] = useState(false);
  const srcName = (id: string) => sources?.find((s) => s.id === id)?.name || id;
  const n = data?.length || 0;

  const newPlan = (
    <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
      <Plus size={14} /> New plan
    </button>
  );

  return (
    <ListPage
      crumbs={navCrumbs("migration-plans")}
      eyebrow="Database migration"
      title="Migration plans"
      state={
        !data
          ? isError
            ? "Could not load plans."
            : "Loading plans…"
          : n
            ? `${n} plan${n === 1 ? "" : "s"}. Open one to run its stages.`
            : "No plans yet. Register a source, then create a plan."
      }
      actions={newPlan}
    >
      {data && !data.length ? (
        <div className="at-panel">
          <h2 className="sr-only">Plans</h2>
          <EmptyState art="volume" title="No migration plans yet" description="A plan moves one source database to an edge cluster.">
            {newPlan}
          </EmptyState>
        </div>
      ) : (
        <Table
          soundings
          panelTitle="Plans"
          rows={data}
          rowKey={(r) => r.id}
          cols={[
            { h: "Name", f: (r) => r.name, mono: true },
            { h: "Source", f: (r) => srcName(r.source_id) },
            {
              h: "Readiness",
              f: (r) =>
                r.readiness_score ? (
                  <span className="db-score">
                    <span className="at-mini" aria-hidden>
                      <i style={{ width: `${Math.max(0, Math.min(100, r.readiness_score))}%` }} />
                    </span>
                    {r.readiness_score}%
                  </span>
                ) : (
                  "—"
                ),
            },
            { h: "State", f: (r) => <Badge kind={planStateKind(r.state)} dot>{planStateLabel(r.state)}</Badge> },
          ]}
          actions={(r) => <Button size="sm" onClick={() => nav(`/databridge/plans/${r.id}`)}>Open</Button>}
        />
      )}

      <FormModal open={create} onClose={() => setCreate(false)} title="New migration plan" submitLabel="Create"
        fields={[
          { name: "name", label: "Plan name" },
          {
            name: "source_id", label: "Source database",
            options: (sources || []).map((s) => ({ value: s.id, label: s.name })),
            hint: "No sources yet. Register one on the Cloud databases page.",
          },
        ]}
        onSubmit={(v) => submit("post", "/databridge/plans", { name: v.name, source_id: v.source_id }, "create plan", () => inv("db-plans"))} />
    </ListPage>
  );
}
