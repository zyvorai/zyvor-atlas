// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
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
import { EmptyState } from "../../graphics";
import { ENGINE_VERIFICATION } from "../../lib/engineVerification";
import "./Sources.css";

const STAGES = ["discover", "full-load", "validate", "cdc", "cutover"] as const;
const STAGE_LABEL: Record<(typeof STAGES)[number], string> = {
  discover: "Discover",
  "full-load": "Full load",
  validate: "Validate",
  cdc: "CDC",
  cutover: "Cutover",
};

const stateKind = (s: string) =>
  s === "discovered" ? "success" : s === "error" ? "danger" : s === "discovering" ? "warning" : "neutral";

export default function Sources() {
  const { data, isError } = useSources();
  const inv = useInvalidate();
  const refetch = () => inv("db-sources");
  const [create, setCreate] = useState(false);
  const [detail, setDetail] = useState<MigrationSource | null>(null);
  const n = data?.length || 0;

  const register = (
    <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
      <Plus size={14} /> Register source
    </button>
  );

  return (
    <ListPage
      className="at-stack"
      crumbs={navCrumbs("cloud-databases")}
      eyebrow="Migration sources"
      title="Cloud databases"
      state={
        !data
          ? isError
            ? "Could not load sources."
            : "Loading sources…"
          : n
            ? `${n} source database${n === 1 ? "" : "s"}. Discover a schema before you migrate.`
            : "No sources yet. Register a cloud database to begin."
      }
      actions={register}
    >
      {data && !data.length ? (
        <div className="at-panel">
          <h2 className="sr-only">Sources</h2>
          <EmptyState art="volume" title="No sources yet" description="Add AWS RDS, Aurora, Cloud SQL or any reachable server.">
            {register}
          </EmptyState>
        </div>
      ) : (
        <Table
          soundings
          panelTitle="Sources"
          rows={data}
          rowKey={(r) => r.id}
          cols={[
            { h: "Name", f: (r) => r.name, mono: true },
            { h: "Engine", f: (r) => <Badge kind="info">{r.kind}</Badge> },
            { h: "Cloud", f: (r) => r.cloud },
            { h: "Endpoint", f: (r) => <span className="mono text-muted-foreground">{r.endpoint || "—"}</span> },
            { h: "Tables", f: (r) => r.discovered?.tables?.length ?? "—" },
            { h: "State", f: (r) => <Badge kind={stateKind(r.state)} dot>{r.state}</Badge> },
          ]}
          actions={(r) => (
            <>
              <Button size="sm" icon={Search} onClick={() => submitJob("post", `/databridge/sources/${r.id}/discover`, null, "discover source", refetch).catch(() => {})}>Discover</Button>
              <Button size="sm" onClick={() => setDetail(r)}>Schema</Button>
              <Button size="sm" variant="danger" onClick={() => del(`source ${r.name}`, async () => { await submit("delete", `/databridge/sources/${r.id}`, null, "delete source"); refetch(); })}>Delete</Button>
            </>
          )}
        />
      )}

      <section className="at-panel" aria-labelledby="db-verify-title">
        <div className="at-panel-bar">
          <h2 id="db-verify-title" className="at-caption">Verified on live databases</h2>
          <span className="grow" />
          <span className="db-verify-hint">Demo mode covers every engine. See docs/DATABRIDGE.md.</span>
        </div>
        {/* Focusable so the keyboard can scroll the matrix sideways on narrow screens. */}
        <div className="overflow-x-auto" role="region" aria-label="Verification matrix" tabIndex={0}>
          <table className="at-tbl db-verify" aria-labelledby="db-verify-title">
            <thead>
              <tr>
                <th scope="col">Engine</th>
                {STAGES.map((s) => (
                  <th key={s} scope="col" className="db-verify-stage">{STAGE_LABEL[s]}</th>
                ))}
                <th scope="col">Notes</th>
              </tr>
            </thead>
            <tbody>
              {ENGINE_VERIFICATION.map((e) => (
                <tr key={e.engine}>
                  <th scope="row" className="mono">{e.engine}</th>
                  {STAGES.map((s) => {
                    const live = e.live.includes(s);
                    const label = live ? "Verified live" : "Not yet verified live";
                    return (
                      <td key={s} className="db-verify-stage" title={label}>
                        {live ? (
                          <Check size={15} className="db-verify-yes" aria-hidden />
                        ) : (
                          <span className="text-muted-foreground" aria-hidden>—</span>
                        )}
                        <span className="sr-only">{label}</span>
                      </td>
                    );
                  })}
                  <td className="db-verify-note">{e.note}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>

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
    <SlideOver open={!!source} onClose={onClose} title={<><span className="mono">{source.name}</span> schema</>} width={620}>
      {!d || !d.tables?.length ? (
        <>
          <h2 className="sr-only">Schema</h2>
          <EmptyState art="volume" title="No schema yet" description="Run Discover on this source first." />
        </>
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
