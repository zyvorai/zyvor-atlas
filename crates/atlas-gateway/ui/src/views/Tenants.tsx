// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
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
import { depth, depthWidth } from "../lib/depth";
import { EmptyState } from "../graphics";
import { formatPercent } from "../graphics/format";
import "./Tenants.css";

/** How much of its storage limit a tenant uses. Without a limit there is no level to draw. */
function QuotaUse({ used, max }: { used: number; max: number }) {
  if (!max) return <span className="text-muted-foreground">No limit</span>;
  const pct = (used / max) * 100;
  const level = depth(pct).cls;
  const word = level === "dp5" ? "Nearly full" : level === "dp4" ? "Filling up" : null;
  return (
    <span className="tn-use">
      <span className={`at-mini ${level}`} aria-hidden>
        <i style={{ width: depthWidth(pct) }} />
      </span>
      <span>{formatPercent(pct)}</span>
      {word && <span className={`tn-flag ${level}`}>{word}</span>}
    </span>
  );
}

export default function Tenants() {
  const { data, isError } = useTenants();
  const inv = useInvalidate();
  const [quota, setQuota] = useState<TenantQuota | null>(null);
  const [drill, setDrill] = useState<TenantQuota | null>(null);
  const n = data?.length || 0;
  return (
    <ListPage
      crumbs={navCrumbs("tenants")}
      eyebrow="Quotas and policy"
      title="Tenants"
      state={
        !data
          ? isError
            ? "Could not load tenants."
            : "Loading tenants…"
          : n
            ? `${n} tenant${n === 1 ? "" : "s"}. Set capacity quotas and placement overrides.`
            : "No tenants yet. They appear once usage is registered."
      }
    >
      {data && !data.length ? (
        <div className="at-panel">
          <h2 className="sr-only">Quotas</h2>
          <EmptyState art="pool" title="No tenants yet" description="Quotas and policy overrides appear once tenants register usage." />
        </div>
      ) : (
        <Table
          soundings
          panelTitle="Quotas"
          rows={data}
          rowKey={(t) => t.tenant_id}
          cols={[
            { h: "Tenant", f: (t) => t.tenant_id, mono: true },
            { h: "Used", f: (t) => fmtBytes(t.used_bytes) },
            { h: "Storage limit", f: (t) => (t.max_bytes ? fmtBytes(t.max_bytes) : "No limit") },
            { h: "Quota used", f: (t) => <QuotaUse used={t.used_bytes} max={t.max_bytes} /> },
            { h: "Volumes", f: (t) => num(t.volume_count) },
            { h: "Volume limit", f: (t) => t.max_volumes || "No limit" },
          ]}
          actions={(t) => (
            <>
              <Button size="sm" onClick={() => setQuota(t)}>Quota</Button>
              <Button size="sm" onClick={() => setDrill(t)}>Policies</Button>
            </>
          )}
        />
      )}

      {quota && (
        <FormModal open onClose={() => setQuota(null)} title={`Quota for ${quota.tenant_id}`} submitLabel="Save"
          fields={[
            { name: "max_bytes", label: "Max bytes (0 for no limit)", type: "number", value: String(quota.max_bytes), min: 0 },
            { name: "max_volumes", label: "Max volumes (0 for no limit)", type: "number", value: String(quota.max_volumes), min: 0 },
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
    <SlideOver open={!!tenant} onClose={onClose} title={`Policies for ${tenant.tenant_id}`} width={520}>
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
            <Button size="sm" variant="danger" onClick={() => del(`override ${p.intent}`, async () => { await submit("delete", `/tenants/${id}/policies/${p.intent}`, null, "delete override"); refetch(); })}>Delete</Button>
          )}
          empty="No overrides. The built-in catalog applies."
        />
        <div className="at-panel">
          <div className="at-panel-bar">
            <h2 className="at-caption">Add override</h2>
          </div>
          <div className="at-form-grid">
            <div className="tn-fields">
              <label>
                <span className="section-label">Intent</span>
                <input className="field" placeholder="intent" value={intent} onChange={(e) => setIntent(e.target.value)} />
              </label>
              <label>
                <span className="section-label">Storage class</span>
                <input className="field" placeholder="storage class" value={sc} onChange={(e) => setSc(e.target.value)} />
              </label>
              <label>
                <span className="section-label">Access mode</span>
                <input className="field" placeholder="access mode" value={am} onChange={(e) => setAm(e.target.value)} />
              </label>
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
