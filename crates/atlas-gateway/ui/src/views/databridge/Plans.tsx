// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { Plus } from "lucide-react";
import { submit } from "../../api/client";
import { usePlans, useSources, useInvalidate } from "../../api/hooks";
import { Badge, Button, FormModal } from "../../ui/kit";
import { ListPage } from "../../ui/templates/ListPage";
import { navCrumbs } from "../../nav/routes";
import { Table } from "../../ui/Table";

export const planStateKind = (s: string) =>
  s.includes("failed") || s === "rolled_back" ? "danger"
  : s === "completed" || s === "cutover_complete" || s === "validated" ? "success"
  : s === "draft" ? "neutral" : "info";

// migration_plans.state is snake_case (e.g. "cdc_streaming", "cutover_complete") — CSS
// text-transform:capitalize only capitalizes whitespace-delimited words, so it leaves the
// underscore in place ("Cdc_streaming"). Replace it with a space so each word capitalizes.
export const planStateLabel = (s: string) => s.replace(/_/g, " ");

export default function Plans() {
  const { data } = usePlans();
  const { data: sources } = useSources();
  const inv = useInvalidate();
  const nav = useNavigate();
  const [create, setCreate] = useState(false);
  const srcName = (id: string) => sources?.find((s) => s.id === id)?.name || id;
  const n = data?.length || 0;

  return (
    <ListPage
      crumbs={navCrumbs("migration-plans")}
      eyebrow="DATABRIDGE · INDEX"
      title="Migration Plans"
      state={
        n
          ? `${n} plan${n === 1 ? "" : "s"} — assess, provision, full-load, CDC, validate, cutover.`
          : "No migration plans yet. Create one after registering a cloud source."
      }
      actions={
        <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
          <Plus size={14} /> New plan
        </button>
      }
    >
      <Table
        soundings
        panelTitle="Plan index"
        rows={data}
        rowKey={(r) => r.id}
        empty="No migration plans yet."
        emptyCta={
          <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
            <Plus size={14} /> New plan
          </button>
        }
        cols={[
          { h: "Name", f: (r) => r.name, mono: true },
          { h: "Source", f: (r) => srcName(r.source_id) },
          { h: "Readiness", f: (r) => r.readiness_score ? `${r.readiness_score}%` : "—" },
          { h: "State", f: (r) => <Badge kind={planStateKind(r.state)} dot>{planStateLabel(r.state)}</Badge> },
        ]}
        actions={(r) => <Button size="sm" onClick={() => nav(`/databridge/plans/${r.id}`)}>Open</Button>}
      />

      <FormModal open={create} onClose={() => setCreate(false)} title="New migration plan" submitLabel="Create"
        fields={[
          { name: "name", label: "Plan name" },
          {
            name: "source_id", label: "Source (from Cloud Databases)",
            options: (sources || []).map((s) => ({ value: s.id, label: s.name })),
            hint: "No sources registered yet — register one on the Cloud Databases page first.",
          },
        ]}
        onSubmit={(v) => submit("post", "/databridge/plans", { name: v.name, source_id: v.source_id }, "create plan", () => inv("db-plans"))} />
    </ListPage>
  );
}
