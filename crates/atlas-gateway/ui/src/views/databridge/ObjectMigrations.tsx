// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Object-storage migration (the DataBridge object leg): copy one S3 bucket into another.
// Either side can be an Atlas Ceph RGW backend or an external S3 endpoint.
import { useState } from "react";
import { Play, Plus } from "lucide-react";
import { apiError, http, submit, submitJob, toast } from "../../api/client";
import { useBackends, useInvalidate, useObjectMigrations } from "../../api/hooks";
import { Badge, Button, FormModal, type FormField } from "../../ui/kit";
import { ListPage } from "../../ui/templates/ListPage";
import { navCrumbs } from "../../nav/routes";
import { del } from "../../ui/confirm";
import { Table } from "../../ui/Table";
import { fmtBytes, num } from "../../lib/format";
import { EmptyState } from "../../graphics";
import "./ObjectMigrations.css";

const stateKind = (s: string) =>
  s === "completed" ? "success" : s === "failed" ? "danger" : s === "running" ? "info" : "neutral";

const EXTERNAL = "external";

export default function ObjectMigrations() {
  const { data, isError } = useObjectMigrations();
  const { data: backends } = useBackends();
  const inv = useInvalidate();
  const refetch = () => inv("db-object");
  const [create, setCreate] = useState(false);
  const n = data?.length || 0;

  const ceph = (backends || []).find((b) => b.backend_type === "ceph");
  const sideOptions = [
    ...(ceph ? [{ value: ceph.id, label: `Ceph RGW (${ceph.id})` }] : []),
    { value: EXTERNAL, label: "External S3 endpoint" },
  ];

  const fields = (vals: Record<string, string>): FormField[] => {
    const out: FormField[] = [
      { name: "name", label: "Name", pattern: /^[A-Za-z0-9][A-Za-z0-9 ._-]{1,62}$/ },
      { name: "source_side", label: "Source", options: sideOptions },
      { name: "source_bucket", label: "Source bucket" },
      { name: "source_prefix", label: "Source prefix (optional)", optional: true },
    ];
    if ((vals.source_side || sideOptions[0]?.value) === EXTERNAL) {
      out.push(
        { name: "source_endpoint", label: "Source endpoint", placeholder: "https://s3.us-east-1.amazonaws.com" },
        { name: "source_secret_ref", label: "Source credentials Secret", hint: "Kubernetes Secret name. Keys are never entered here." },
      );
    }
    out.push(
      { name: "dest_side", label: "Destination", options: sideOptions },
      { name: "dest_bucket", label: "Destination bucket", hint: "Created on the destination if it does not exist." },
    );
    if ((vals.dest_side || sideOptions[0]?.value) === EXTERNAL) {
      out.push(
        { name: "dest_endpoint", label: "Destination endpoint" },
        { name: "dest_secret_ref", label: "Destination credentials Secret" },
      );
    }
    if ((vals.source_side || sideOptions[0]?.value) === EXTERNAL || (vals.dest_side || sideOptions[0]?.value) === EXTERNAL) {
      out.push({
        name: "secret_namespace",
        label: "Credentials Secret namespace",
        value: "zyvor-system",
        hint: "Namespace holding the Secret(s) named above (keys AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY).",
      });
    }
    out.push({
      name: "mode",
      label: "Mode",
      options: [
        { value: "incremental", label: "Incremental (skip unchanged objects)" },
        { value: "full", label: "Full (copy everything)" },
      ],
    });
    return out;
  };

  const newMigration = (
    <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
      <Plus size={14} /> New migration
    </button>
  );

  return (
    <ListPage
      crumbs={navCrumbs("object-migrations")}
      eyebrow="Object storage"
      title="Object migrations"
      state={
        !data
          ? isError
            ? "Could not load object migrations."
            : "Loading object migrations…"
          : n
            ? `${n} bucket-to-bucket migration${n === 1 ? "" : "s"}.`
            : "No migrations yet. Copy one bucket into another."
      }
      actions={newMigration}
    >
      {data && !data.length ? (
        <div className="at-panel">
          <h2 className="sr-only">Migrations</h2>
          <EmptyState art="bucket" title="No object migrations yet" description="Source and destination can be Ceph RGW or any S3-compatible endpoint.">
            {newMigration}
          </EmptyState>
        </div>
      ) : (
        <Table
          soundings
          panelTitle="Migrations"
          rows={data}
          rowKey={(m) => m.id}
          cols={[
            { h: "Name", f: (m) => m.name },
            { h: "Source", f: (m) => `${m.source_bucket}${m.source_prefix ? `/${m.source_prefix}` : ""}`, mono: true },
            { h: "Destination", f: (m) => m.dest_bucket, mono: true },
            { h: "Mode", f: (m) => m.mode },
            { h: "State", f: (m) => <Badge kind={stateKind(m.state)} dot>{m.state}</Badge> },
            {
              h: "Progress",
              f: (m) => (
                <div className="om-progress">
                  {m.objects_total > 0 && (
                    <span className="at-mini" aria-hidden>
                      <i style={{ width: `${Math.min(100, (m.objects_done / m.objects_total) * 100)}%` }} />
                    </span>
                  )}
                  <span>{`${num(m.objects_done)}/${num(m.objects_total)} objects · ${fmtBytes(m.bytes_done)}${m.verified ? " · verified" : ""}`}</span>
                </div>
              ),
            },
            { h: "Error", f: (m) => (m.last_error ? <span className="om-error">{m.last_error}</span> : "") },
          ]}
          actions={(m) => (
            <>
              <Button
                size="sm"
                icon={Play}
                onClick={() =>
                  submitJob("post", `/databridge/object/${m.id}/start`, null, `migrate ${m.name}`, refetch).catch(() => {})
                }
              >
                Start
              </Button>
              <Button
                size="sm"
                variant="danger"
                onClick={() =>
                  del(`migration ${m.name}`, async () => {
                    await submit("delete", `/databridge/object/${m.id}`, null, "delete migration");
                    refetch();
                  })
                }
              >
                Delete
              </Button>
            </>
          )}
        />
      )}

      <FormModal
        open={create}
        onClose={() => setCreate(false)}
        title="New object migration"
        submitLabel="Create"
        fields={fields}
        onSubmit={async (v) => {
          const body: Record<string, unknown> = {
            name: v.name,
            source_bucket: v.source_bucket,
            source_prefix: v.source_prefix || undefined,
            dest_bucket: v.dest_bucket,
            mode: v.mode || "incremental",
          };
          if (v.source_side === EXTERNAL) {
            body.source_endpoint = v.source_endpoint;
            body.source_secret_ref = v.source_secret_ref;
          } else {
            body.source_backend_id = v.source_side || sideOptions[0]?.value;
          }
          if (v.dest_side === EXTERNAL) {
            body.dest_endpoint = v.dest_endpoint;
            body.dest_secret_ref = v.dest_secret_ref;
          } else {
            body.dest_backend_id = v.dest_side || sideOptions[0]?.value;
          }
          if (v.secret_namespace) body.secret_namespace = v.secret_namespace;
          try {
            await http.post("/databridge/object", body);
            toast(`migration ${v.name} created`, "ok");
            refetch();
          } catch (e) {
            toast(`create migration: ${apiError(e)}`, "err");
            throw e;
          }
        }}
      />
    </ListPage>
  );
}
