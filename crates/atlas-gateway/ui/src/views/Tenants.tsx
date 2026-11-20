// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useState } from "react";
import { submit } from "../api/client";
import { useInvalidate, useTenantPolicies, useTenants } from "../api/hooks";
import type { TenantQuota } from "../api/types";
import { Button, FormModal, SlideOver } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { del } from "../ui/confirm";
import { Table } from "../ui/Table";
import { fmtBytes, num } from "../lib/format";

export default function Tenants() {
  const { data } = useTenants();
  const inv = useInvalidate();
  const [quota, setQuota] = useState<TenantQuota | null>(null);
  const [drill, setDrill] = useState<TenantQuota | null>(null);
  const n = data?.length || 0;
  return (
    <ListPage
      crumbs={navCrumbs("tenants")}
      eyebrow="GOVERNANCE · INDEX"
      title="Tenants"
      state={
        n
          ? `${n} tenant${n === 1 ? "" : "s"} — capacity quotas and intent→placement overrides.`
          : "No tenants yet. Quotas and policy overrides appear once tenants register usage."
      }
    >
      <Table
        soundings
        panelTitle="Tenant index"
        rows={data}
        rowKey={(t) => t.tenant_id}
        empty="No tenants yet."
        cols={[
          { h: "Tenant", f: (t) => t.tenant_id, mono: true },
          { h: "Used", f: (t) => fmtBytes(t.used_bytes) },
          { h: "Volumes", f: (t) => num(t.volume_count) },
          { h: "Max bytes", f: (t) => (t.max_bytes ? fmtBytes(t.max_bytes) : "∞") },
          { h: "Max vols", f: (t) => t.max_volumes || "∞" },
        ]}
        actions={(t) => (
          <>
            <Button size="sm" onClick={() => setQuota(t)}>Quota</Button>
            <Button size="sm" onClick={() => setDrill(t)}>Policies</Button>
          </>
        )}
      />

      {quota && (
        <FormModal open onClose={() => setQuota(null)} title={`Quota — ${quota.tenant_id}`} submitLabel="Save"
          fields={[
            { name: "max_bytes", label: "Max bytes (0=∞)", type: "number", value: String(quota.max_bytes), min: 0 },
            { name: "max_volumes", label: "Max volumes (0=∞)", type: "number", value: String(quota.max_volumes), min: 0 },
          ]}
          onSubmit={(v) => submit("put", `/tenants/${quota.tenant_id}/quota`, { max_bytes: +v.max_bytes, max_volumes: +v.max_volumes }, "quota set", () => inv("tenants"))} />
      )}
      <PolicyDrawer tenant={drill} onClose={() => setDrill(null)} />
    </ListPage>
  );
}

function PolicyDrawer({ tenant, onClose }: { tenant: TenantQuota | null; onClose: () => void }) {
  const id = tenant?.tenant_id || "";
  const { data, refetch } = useTenantPolicies(id);
  const [intent, setIntent] = useState("database");
  const [sc, setSc] = useState("zyvor-cephfs-shared");
  const [am, setAm] = useState("ReadWriteMany");
  if (!tenant) return null;
  return (
    <SlideOver open={!!tenant} onClose={onClose} title={`Policies — ${tenant.tenant_id}`} width={520}>
      <div className="at-stack">
        <Table
          soundings
          panelTitle="Overrides"
          rows={data}
          rowKey={(p) => p.intent}
          cols={[
            { h: "Intent", f: (p) => p.intent },
            { h: "StorageClass", f: (p) => p.storage_class, mono: true },
            { h: "Access", f: (p) => p.access_mode },
          ]}
          actions={(p) => (
            <Button size="sm" variant="danger" onClick={() => del(`override ${p.intent}`, async () => { await submit("delete", `/tenants/${id}/policies/${p.intent}`, null, "delete override"); refetch(); })}>Del</Button>
          )}
          empty="No overrides — falls back to the built-in catalog."
        />
        <div className="at-panel">
          <div className="at-panel-bar">
            <span className="at-caption">Add override</span>
          </div>
          <div className="at-form-grid">
            <div className="grid grid-cols-3 gap-2">
              <input className="field" placeholder="intent" value={intent} onChange={(e) => setIntent(e.target.value)} />
              <input className="field" placeholder="storage class" value={sc} onChange={(e) => setSc(e.target.value)} />
              <input className="field" placeholder="access mode" value={am} onChange={(e) => setAm(e.target.value)} />
            </div>
            <div>
              <button
                type="button"
                className="at-btn primary"
                disabled={!intent.trim() || !sc.trim() || !am.trim()}
                onClick={async () => {
                  try {
                    await submit("put", `/tenants/${id}/policies/${intent}`, { storage_class: sc, access_mode: am, volume_mode: "Filesystem" }, "override set");
                    setIntent("database"); setSc("zyvor-cephfs-shared"); setAm("ReadWriteMany");
                    refetch();
                  } catch { /* toasted */ }
                }}
              >
                Save override
              </button>
            </div>
          </div>
        </div>
      </div>
    </SlideOver>
  );
}
