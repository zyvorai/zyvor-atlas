// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useState } from "react";
import { Check, Plus, Search } from "lucide-react";
import { submitJob, submit } from "../../api/client";
import { useSources, useInvalidate } from "../../api/hooks";
import type { MigrationSource } from "../../api/types";
import { Badge, Button, FormModal, SlideOver } from "../../ui/kit";
import { ListPage } from "../../ui/templates/ListPage";
import { navCrumbs } from "../../nav/routes";
import { del } from "../../ui/confirm";
import { Table } from "../../ui/Table";
import { fmtBytes } from "../../lib/format";
import { ENGINE_VERIFICATION } from "../../lib/engineVerification";

const STAGES = ["discover", "full-load", "validate", "cdc", "cutover"] as const;
const STAGE_LABEL: Record<(typeof STAGES)[number], string> = {
  discover: "Discover",
  "full-load": "Full-load",
  validate: "Validate",
  cdc: "CDC",
  cutover: "Cutover",
};

const stateKind = (s: string) =>
  s === "discovered" ? "success" : s === "error" ? "danger" : s === "discovering" ? "warning" : "neutral";

export default function Sources() {
  const { data } = useSources();
  const inv = useInvalidate();
  const refetch = () => inv("db-sources");
  const [create, setCreate] = useState(false);
  const [detail, setDetail] = useState<MigrationSource | null>(null);
  const n = data?.length || 0;

  return (
    <ListPage
      crumbs={navCrumbs("cloud-databases")}
      eyebrow="DATABRIDGE · INDEX"
      title="Cloud Databases"
      state={
        n
          ? `${n} source database${n === 1 ? "" : "s"} — discover schema before migrating.`
          : "No sources yet. Register AWS RDS/Aurora or GCP Cloud SQL to begin."
      }
      actions={
        <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
          <Plus size={14} /> Register source
        </button>
      }
    >
      <Table
        soundings
        panelTitle="Source index"
        rows={data}
        rowKey={(r) => r.id}
        empty="No sources yet."
        emptyCta={
          <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
            <Plus size={14} /> Register source
          </button>
        }
        cols={[
          { h: "Name", f: (r) => r.name, mono: true },
          { h: "Engine", f: (r) => <Badge kind="info">{r.kind}</Badge> },
          { h: "Cloud", f: (r) => r.cloud },
          { h: "Endpoint", f: (r) => <span className="mono" style={{ color: "var(--at-ink-4)" }}>{r.endpoint || "—"}</span> },
          { h: "Tables", f: (r) => r.discovered?.tables?.length ?? "—" },
          { h: "State", f: (r) => <Badge kind={stateKind(r.state)} dot>{r.state}</Badge> },
        ]}
        actions={(r) => (
          <>
            <Button size="sm" icon={Search} onClick={() => submitJob("post", `/databridge/sources/${r.id}/discover`, null, "discover source", refetch).catch(() => {})}>Discover</Button>
            <Button size="sm" onClick={() => setDetail(r)}>Schema</Button>
            <Button size="sm" variant="danger" onClick={() => del(`source ${r.name}`, async () => { await submit("delete", `/databridge/sources/${r.id}`, null, "delete source"); refetch(); })}>Del</Button>
          </>
        )}
      />

      <div className="at-panel" style={{ marginTop: 0 }}>
        <div className="at-panel-bar">
          <span className="at-caption">Live verification matrix</span>
          <span className="grow" />
          <span className="at-sub" style={{ margin: 0 }}>fake path covers all engines end-to-end · docs/DATABRIDGE.md</span>
        </div>
        <div className="at-verify-table">
          <div className="at-verify-row at-verify-head">
            <span>Engine</span>
            {STAGES.map((s) => (
              <span key={s}>{STAGE_LABEL[s]}</span>
            ))}
            <span>Notes</span>
          </div>
          {ENGINE_VERIFICATION.map((e) => (
            <div key={e.engine} className="at-verify-row">
              <span className="mono">{e.engine}</span>
              {STAGES.map((s) => (
                <span key={s} className="at-verify-cell" title={e.live.includes(s) ? "Verified live" : "Not yet verified live"}>
                  {e.live.includes(s) ? (
                    <Check size={13} style={{ color: "var(--at-ok)" }} />
                  ) : (
                    <span style={{ color: "var(--at-ink-4)" }}>—</span>
                  )}
                </span>
              ))}
              <span className="at-sub" style={{ margin: 0 }}>{e.note}</span>
            </div>
          ))}
        </div>
      </div>

      <FormModal open={create} onClose={() => setCreate(false)} title="Register source database" submitLabel="Register"
        fields={[
          { name: "name", label: "Name" },
          {
            name: "kind", label: "Engine",
            options: ["postgres", "mysql", "mariadb", "oracle", "sqlserver", "mongodb"].map((v) => ({ value: v, label: v })),
          },
          {
            name: "cloud", label: "Cloud",
            options: ["rds", "aurora", "cloudsql", "generic"].map((v) => ({ value: v, label: v })),
          },
          { name: "endpoint", label: "Endpoint host", optional: true },
          { name: "port", label: "Port", type: "number", optional: true, min: 1 },
          { name: "database", label: "Database", optional: true },
          { name: "driver_mode", label: "Driver mode", options: [{ value: "fake", label: "fake" }, { value: "real", label: "real" }] },
        ]}
        onSubmit={(v) => {
          const body: Record<string, unknown> = { name: v.name, kind: v.kind, cloud: v.cloud, driver_mode: v.driver_mode };
          if (v.endpoint) body.endpoint = v.endpoint;
          if (v.port) body.port = +v.port;
          if (v.database) body.database = v.database;
          return submit("post", "/databridge/sources", body, "register source", refetch);
        }} />

      <SchemaBrowser source={detail} onClose={() => setDetail(null)} />
    </ListPage>
  );
}

function SchemaBrowser({ source, onClose }: { source: MigrationSource | null; onClose: () => void }) {
  if (!source) return null;
  const d = source.discovered;
  return (
    <SlideOver open={!!source} onClose={onClose} title={<span className="mono">{source.name} · schema</span>} width={620}>
      {!d || !d.tables?.length ? (
        <div className="text-muted-foreground text-sm">
          No schema discovered yet. Run <b>Discover</b> on this source first.
        </div>
      ) : (
        <>
          <div className="flex gap-4 mb-3 text-sm">
            <span>Engine <b>{d.engine} {d.version}</b></span>
            <span>Size <b>{fmtBytes(d.total_size_bytes || 0)}</b></span>
            <span>CDC {d.cdc_capable ? <Badge kind="success">capable</Badge> : <Badge kind="danger">off</Badge>}</span>
          </div>
          <Table rows={d.tables} rowKey={(t) => `${t.schema}.${t.name}`}
            cols={[
              { h: "Table", f: (t) => `${t.schema}.${t.name}`, mono: true },
              { h: "Rows", f: (t) => t.est_rows.toLocaleString() },
              { h: "Size", f: (t) => fmtBytes(t.size_bytes) },
              { h: "PK", f: (t) => t.has_primary_key ? <Badge kind="success">yes</Badge> : <Badge kind="warning">none</Badge> },
            ]} empty="No tables." />
        </>
      )}
    </SlideOver>
  );
}
