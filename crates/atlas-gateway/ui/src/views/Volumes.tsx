// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { Download, Plus, RefreshCw, SlidersHorizontal } from "lucide-react";
import { apiError, http, isUnauthorized, submit, submitJob, toast } from "../api/client";
import { useBackends, useBuckets, useInvalidate, useVolumes } from "../api/hooks";
import type { StorageVolume } from "../api/types";
import { depth, depthWidth } from "../lib/depth";
import { fmtBytes, gib, stateKind } from "../lib/format";
import { del } from "../ui/confirm";
import { Badge, Button, FormModal, SlideOver } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";

const POLICIES = ["database", "production", "development", "shared", "ai"];
const STATES = ["bound", "available", "creating", "deleting", "failed"] as const;
const KINDS = ["block", "filesystem", "object"] as const;

export default function Volumes() {
  const [state, setState] = useState("");
  const [tenant, setTenant] = useState("");
  const [backend, setBackend] = useState("");
  const [kind, setKind] = useState("");
  const [filtersOpen, setFiltersOpen] = useState(false);
  const secondaryCount = [kind, backend, tenant].filter(Boolean).length;
  const { data: vols } = useVolumes(state || undefined, tenant || undefined, backend || undefined, kind || undefined);
  const { data: backends } = useBackends();
  const inv = useInvalidate();
  const refetch = () => inv("volumes", "summary");
  const exportCsv = async () => {
    const qs = new URLSearchParams();
    if (state) qs.set("state", state);
    if (tenant) qs.set("tenant", tenant);
    if (backend) qs.set("backend", backend);
    if (kind) qs.set("kind", kind);
    try {
      const r = await http.get(`/volumes.csv${qs.toString() ? "?" + qs : ""}`, { responseType: "blob" });
      const url = URL.createObjectURL(r.data as Blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = "atlas-volumes.csv";
      a.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      if (!isUnauthorized(e)) toast(`export CSV: ${apiError(e)}`, "err");
    }
  };
  const [createOpen, setCreateOpen] = useState(false);
  const [sel, setSel] = useState<StorageVolume | null>(null);
  const [modal, setModal] = useState<{ v: StorageVolume; kind: string } | null>(null);
  const { data: buckets } = useBuckets();
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [sort, setSort] = useState<{ key: string; dir: 1 | -1 }>({ key: "name", dir: 1 });

  const toggle = (k: string) =>
    setPicked((s) => {
      const n = new Set(s);
      if (n.has(k)) n.delete(k);
      else n.add(k);
      return n;
    });
  const toggleAll = (keys: string[]) => setPicked((s) => (keys.every((k) => s.has(k)) ? new Set() : new Set(keys)));
  const bulkDelete = () =>
    del(`${picked.size} volume(s)`, () => {
      const ids = [...picked];
      setPicked(new Set());
      ids.forEach((id) => {
        const v = vols?.find((x) => x.id === id);
        submitJob("delete", `/volumes/${id}?force=true`, null, `delete ${v?.name || id}`, refetch);
      });
    });

  const [params, setParams] = useSearchParams();
  useEffect(() => {
    const f = params.get("focus");
    if (f && vols) {
      const v = vols.find((x) => x.id === f);
      if (v) {
        // Deep-link support: open the volume named by ?focus=<id>, then strip it from the URL.
        // eslint-disable-next-line react-hooks/set-state-in-effect
        setSel(v);
        setParams({}, { replace: true });
      }
    }
  }, [params, vols, setParams]);

  const counts = useMemo(() => {
    const all = vols || [];
    return {
      total: all.length,
      bound: all.filter((v) => v.state === "bound").length,
      failed: all.filter((v) => v.state === "failed").length,
      block: all.filter((v) => v.kind === "block").length,
      fs: all.filter((v) => v.kind === "filesystem").length,
    };
  }, [vols]);

  const sorted = useMemo(() => {
    if (!vols) return undefined;
    const dir = sort.dir;
    return [...vols].sort((a, b) => {
      let ka: string | number;
      let kb: string | number;
      switch (sort.key) {
        case "size":
          ka = a.size_bytes;
          kb = b.size_bytes;
          break;
        case "used":
          ka = a.used_bytes ?? -1;
          kb = b.used_bytes ?? -1;
          break;
        case "state":
          ka = a.state;
          kb = b.state;
          break;
        case "kind":
          ka = a.kind;
          kb = b.kind;
          break;
        default:
          ka = a.name;
          kb = b.name;
      }
      return (ka < kb ? -1 : ka > kb ? 1 : 0) * dir;
    });
  }, [vols, sort]);

  const keys = (sorted || []).map((v) => v.id);
  const allSelected = keys.length > 0 && keys.every((k) => picked.has(k));

  const stateLine = (() => {
    if (!vols) return "Surveying volumes…";
    if (!vols.length) {
      return state || tenant || backend || kind
        ? "No volumes match the current filter set."
        : "No volumes yet — provision the first block or filesystem volume.";
    }
    if (counts.failed) return `${counts.failed} volume${counts.failed === 1 ? "" : "s"} in failed state — inspect before expanding.`;
    return `${counts.total} volume${counts.total === 1 ? "" : "s"} · ${counts.bound} bound · ${counts.block} block · ${counts.fs} filesystem.`;
  })();

  const clickSort = (key: string) =>
    setSort((s) => (s.key === key ? { key, dir: s.dir === 1 ? -1 : 1 } : { key, dir: 1 }));

  return (
    <ListPage
      crumbs={navCrumbs("volumes")}
      eyebrow="STORAGE · INDEX"
      title="Volumes"
      state={stateLine}
      actions={
        <>
          <button type="button" className="at-btn" onClick={refetch}>
            <RefreshCw size={14} /> Refresh
          </button>
          <button type="button" className="at-btn" onClick={exportCsv}>
            <Download size={14} /> Export CSV
          </button>
          <button type="button" className="at-btn primary" onClick={() => setCreateOpen(true)}>
            <Plus size={14} /> Volume
          </button>
        </>
      }
    >
      <div className="at-chips">
        <button type="button" className={`at-chip${!state ? " on" : ""}`} onClick={() => setState("")}>
          All <span className="n">{counts.total || "—"}</span>
        </button>
        {STATES.map((s) => (
          <button
            key={s}
            type="button"
            className={`at-chip${state === s ? " on" : ""}`}
            onClick={() => setState(state === s ? "" : s)}
          >
            {s}
          </button>
        ))}
        <span className="grow" />
        <div className="relative">
          <button
            type="button"
            className="at-btn compact"
            aria-expanded={filtersOpen}
            aria-haspopup="menu"
            onClick={() => setFiltersOpen((v) => !v)}
          >
            <SlidersHorizontal size={13} />
            Filters
            {secondaryCount > 0 && (
              <span
                style={{
                  fontFamily: "var(--at-mono)",
                  fontSize: 11,
                  color: "var(--at-cyan)",
                  opacity: 0.9,
                }}
              >
                {secondaryCount}
              </span>
            )}
          </button>
          {filtersOpen && (
            <>
              <div className="fixed inset-0 z-40" onClick={() => setFiltersOpen(false)} />
              <div className="at-theme-menu" style={{ minWidth: 260, padding: "6px 6px 10px" }}>
                <div className="at-theme-menu-label">Filters</div>
                <div style={{ padding: "2px 12px 10px" }}>
                  <div style={{ fontSize: 11, color: "var(--at-ink-4)", marginBottom: 6 }}>Kind</div>
                  <div style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
                    {KINDS.map((k) => (
                      <button
                        key={k}
                        type="button"
                        className={`at-chip${kind === k ? " on" : ""}`}
                        onClick={() => setKind(kind === k ? "" : k)}
                      >
                        {k}
                      </button>
                    ))}
                  </div>
                </div>
                <div style={{ padding: "2px 12px 10px" }}>
                  <div style={{ fontSize: 11, color: "var(--at-ink-4)", marginBottom: 6 }}>Backend</div>
                  <select
                    className="at-chip-field"
                    style={{ width: "100%" }}
                    value={backend}
                    onChange={(e) => setBackend(e.target.value)}
                    aria-label="Backend filter"
                  >
                    <option value="">All backends</option>
                    {(backends || []).map((b) => (
                      <option key={b.id} value={b.id}>
                        {b.name} ({b.backend_type})
                      </option>
                    ))}
                  </select>
                </div>
                <div style={{ padding: "2px 12px 10px" }}>
                  <div style={{ fontSize: 11, color: "var(--at-ink-4)", marginBottom: 6 }}>Tenant</div>
                  <input
                    className="at-chip-field"
                    style={{ width: "100%" }}
                    placeholder="Filter tenant…"
                    value={tenant}
                    onChange={(e) => setTenant(e.target.value)}
                  />
                </div>
                {secondaryCount > 0 && (
                  <button
                    type="button"
                    className="at-theme-item"
                    onClick={() => {
                      setKind("");
                      setBackend("");
                      setTenant("");
                    }}
                  >
                    Clear filters
                  </button>
                )}
              </div>
            </>
          )}
        </div>
      </div>

      <div className="at-panel">
        <div className="at-panel-bar">
          <span className="at-caption">Volume index</span>
          <span className="grow" />
          {picked.size > 0 ? (
            <>
              <span className="at-sub" style={{ margin: 0 }}>
                {picked.size} selected
              </span>
              <button type="button" className="at-btn compact" onClick={bulkDelete}>
                Delete selected
              </button>
              <button type="button" className="at-btn compact" onClick={() => setPicked(new Set())}>
                Clear
              </button>
            </>
          ) : (
            <span className="at-sub" style={{ margin: 0 }}>
              {sorted ? `${sorted.length} shown` : "…"}
            </span>
          )}
        </div>

        {!sorted ? (
          <div style={{ padding: 32, color: "var(--at-ink-4)", fontSize: 13 }}>Loading volumes…</div>
        ) : !sorted.length ? (
          <div className="at-empty-box" style={{ margin: 16, boxShadow: "none" }}>
            <div className="at-empty-title">
              {state || tenant || backend || kind ? "No volumes match your filters" : "No volumes yet"}
            </div>
            {!(state || tenant || backend || kind) && (
              <button type="button" className="at-btn primary" onClick={() => setCreateOpen(true)}>
                <Plus size={14} /> Create volume
              </button>
            )}
          </div>
        ) : (
          <div className="overflow-x-auto">
            <table className="at-tbl">
              <thead>
                <tr>
                  <th style={{ width: 36 }}>
                    <input type="checkbox" checked={allSelected} onChange={() => toggleAll(keys)} aria-label="Select all" />
                  </th>
                  <th className="sortable" onClick={() => clickSort("name")}>
                    Name
                  </th>
                  <th className="sortable" onClick={() => clickSort("kind")}>
                    Kind
                  </th>
                  <th className="sortable" onClick={() => clickSort("size")}>
                    Size
                  </th>
                  <th>Depth</th>
                  <th className="sortable" onClick={() => clickSort("state")}>
                    State
                  </th>
                  <th>Class</th>
                  <th>PVC / RBD</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {sorted.map((v) => {
                  const pct =
                    v.size_bytes > 0 && v.used_bytes != null ? (v.used_bytes / v.size_bytes) * 100 : 0;
                  const d = depth(pct);
                  return (
                    <tr
                      key={v.id}
                      className={picked.has(v.id) ? "selected" : undefined}
                      onClick={() => setSel(v)}
                      style={{ cursor: "pointer" }}
                    >
                      <td onClick={(e) => e.stopPropagation()}>
                        <input
                          type="checkbox"
                          checked={picked.has(v.id)}
                          onChange={() => toggle(v.id)}
                          aria-label={`Select ${v.name}`}
                        />
                      </td>
                      <td className="mono">{v.name}</td>
                      <td>{v.kind}</td>
                      <td className="mono">{fmtBytes(v.size_bytes)}</td>
                      <td>
                        <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
                          <div className={`at-mini ${d.cls}`} title={`${Math.round(pct)}% · ${d.name}`}>
                            <i style={{ width: depthWidth(pct) }} />
                          </div>
                          <span className="mono" style={{ fontSize: 11, color: "var(--at-ink-4)" }}>
                            {v.used_bytes != null ? fmtBytes(v.used_bytes) : "—"}
                          </span>
                        </div>
                      </td>
                      <td>
                        <Badge kind={stateKind(v.state)} dot>
                          {v.state}
                        </Badge>
                      </td>
                      <td className="mono">{v.storage_class_name || "—"}</td>
                      <td className="mono" style={{ color: "var(--at-ink-4)" }}>
                        {v.pvc_name || v.backend_native_id || "—"}
                      </td>
                      <td onClick={(e) => e.stopPropagation()}>
                        <div className="at-row-actions">
                          <button type="button" className="at-act" onClick={() => setModal({ v, kind: "snapshot" })}>
                            Snap
                          </button>
                          <button type="button" className="at-act" onClick={() => setModal({ v, kind: "expand" })}>
                            Expand
                          </button>
                          <button type="button" className="at-act" onClick={() => setModal({ v, kind: "schedule" })}>
                            Schedule
                          </button>
                          <button
                            type="button"
                            className="at-act danger"
                            onClick={() =>
                              del(`volume ${v.name}`, () =>
                                submitJob("delete", `/volumes/${v.id}?force=true`, null, `delete ${v.name}`, refetch),
                              )
                            }
                          >
                            Del
                          </button>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>

      <FormModal
        open={createOpen}
        onClose={() => setCreateOpen(false)}
        title="Create volume"
        submitLabel="Create"
        fields={[
          { name: "name", label: "Name", placeholder: "my-volume" },
          { name: "tenant_id", label: "Tenant", value: "default" },
          { name: "size_gib", label: "Size (GiB)", type: "number", value: "1", min: 1 },
          { name: "policy", label: "Policy (intent)", options: POLICIES.map((p) => ({ value: p, label: p })) },
          { name: "namespace", label: "Namespace", value: "rook-ceph" },
        ]}
        onSubmit={(v) =>
          submitJob(
            "post",
            "/volumes",
            {
              tenant_id: v.tenant_id,
              name: v.name,
              size_bytes: gib(+v.size_gib),
              policy: v.policy,
              namespaces: { namespace: v.namespace },
            },
            `create ${v.name}`,
            refetch,
          )
        }
      />

      {modal?.kind === "snapshot" && (
        <FormModal
          open
          onClose={() => setModal(null)}
          title={`Snapshot ${modal.v.name}`}
          submitLabel="Snapshot"
          fields={[
            {
              name: "name",
              label: "Snapshot name (optional)",
              optional: true,
              pattern: /^[a-zA-Z0-9_.-]+$/,
              hint: "Letters, digits, dot, dash, underscore only — leave blank to auto-generate.",
            },
          ]}
          onSubmit={(x) =>
            submitJob("post", `/volumes/${modal.v.id}/snapshots`, { name: x.name || undefined }, "snapshot", refetch)
          }
        />
      )}
      {modal?.kind === "expand" && (
        <FormModal
          open
          onClose={() => setModal(null)}
          title={`Expand ${modal.v.name}`}
          submitLabel="Expand"
          fields={[
            {
              name: "size_gib",
              label: "New size (GiB)",
              type: "number",
              value: String(Math.ceil(modal.v.size_bytes / 1073741824) + 1),
              min: Math.ceil(modal.v.size_bytes / 1073741824) + 1,
              hint: `Current size: ${fmtBytes(modal.v.size_bytes)} — expand only grows.`,
            },
          ]}
          onSubmit={(x) =>
            submitJob("post", `/volumes/${modal.v.id}/expand`, { new_size_bytes: gib(+x.size_gib) }, "expand", refetch)
          }
        />
      )}
      {modal?.kind === "schedule" && (
        <FormModal
          open
          onClose={() => setModal(null)}
          title={`Schedule for ${modal.v.name}`}
          submitLabel="Create schedule"
          fields={(vals) => [
            { name: "kind", label: "Kind", options: [{ value: "snapshot", label: "snapshot" }, { value: "backup", label: "backup" }] },
            { name: "interval_secs", label: "Interval (seconds)", type: "number", value: "3600", min: 60 },
            { name: "keep", label: "Keep", type: "number", value: "24", min: 0 },
            ...(vals.kind === "backup"
              ? [
                  {
                    name: "bucket_id",
                    label: "Bucket (backup only)",
                    options: [
                      { value: "", label: "—" },
                      ...(buckets || [])
                        .filter((b) => b.state === "bound")
                        .map((b) => ({ value: b.id, label: b.bucket_name || b.id })),
                    ],
                  },
                  {
                    name: "mode",
                    label: "Mode (backup)",
                    options: [
                      { value: "manifest", label: "manifest" },
                      { value: "data", label: "data" },
                    ],
                  },
                ]
              : []),
          ]}
          onSubmit={(x) =>
            submit(
              "post",
              `/volumes/${modal.v.id}/schedule`,
              {
                kind: x.kind,
                interval_secs: +x.interval_secs,
                keep: +x.keep,
                bucket_id: x.bucket_id || undefined,
                mode: x.mode,
              },
              "schedule",
              () => inv("schedules"),
            )
          }
        />
      )}

      <VolumeDrawer vol={sel} onClose={() => setSel(null)} refetch={refetch} />
    </ListPage>
  );
}

function VolumeDrawer({ vol, onClose, refetch }: { vol: StorageVolume | null; onClose: () => void; refetch: () => void }) {
  const inv = useInvalidate();
  const [bindings, setBindings] = useState<any[] | null>(null);
  const [labels, setLabels] = useState<Record<string, string> | null>(null);
  const [lk, setLk] = useState("");
  const [lv, setLv] = useState("");
  if (vol && bindings === null) {
    http.get(`/volumes/${vol.id}/bindings`).then((r) => setBindings(r.data)).catch(() => setBindings([]));
    http.get(`/volumes/${vol.id}/labels`).then((r) => setLabels(r.data || {})).catch(() => setLabels({}));
  }
  const close = () => {
    setBindings(null);
    setLabels(null);
    onClose();
  };
  if (!vol) return null;
  return (
    <SlideOver open={!!vol} onClose={close} title={<span className="mono">{vol.name}</span>} width={480}>
      <div className="space-y-4 text-sm">
        <div className="grid grid-cols-2 gap-2">
          <Kv k="ID" v={vol.id} mono />
          <Kv k="Kind" v={vol.kind} />
          <Kv k="Size" v={fmtBytes(vol.size_bytes)} />
          <Kv k="Used" v={vol.used_bytes != null ? fmtBytes(vol.used_bytes) : "—"} />
          <Kv k="State" v={vol.state} />
          <Kv k="Class" v={vol.storage_class_name || "—"} mono />
          <Kv k="Namespace" v={vol.kubernetes_namespace || "—"} />
          <Kv k="PVC" v={vol.pvc_name || "—"} mono />
          <Kv k="Native" v={vol.backend_native_id || "—"} mono />
        </div>
        <div>
          <div className="section-label mb-1">Labels</div>
          <div className="flex flex-wrap gap-1.5 mb-2">
            {labels && Object.entries(labels).length
              ? Object.entries(labels).map(([k, v]) => (
                  <Badge key={k} kind="info" className="normal-case">
                    {k}={String(v)}
                  </Badge>
                ))
              : <span className="text-muted-foreground">none</span>}
          </div>
          <div className="flex gap-2">
            <input className="field" placeholder="key" value={lk} onChange={(e) => setLk(e.target.value)} />
            <input className="field" placeholder="value" value={lv} onChange={(e) => setLv(e.target.value)} />
            <Button
              variant="primary"
              size="sm"
              disabled={!lk}
              onClick={async () => {
                try {
                  const r = await http.put(`/volumes/${vol.id}/labels`, { [lk]: lv });
                  setLabels(r.data);
                  setLk("");
                  setLv("");
                  inv("volumes");
                } catch (e) {
                  if (!isUnauthorized(e)) toast(`set label: ${apiError(e)}`, "err");
                }
              }}
            >
              Set
            </Button>
          </div>
        </div>
        <div>
          <div className="section-label mb-1">Product bindings</div>
          {bindings && bindings.length ? (
            <div className="space-y-1">
              {bindings.map((b, i) => (
                <div key={i} className="mono text-xs">
                  {b.product} · {b.resource_type}/{b.resource_id} · {b.role}
                </div>
              ))}
            </div>
          ) : (
            <span className="text-muted-foreground">none</span>
          )}
        </div>
        <div className="flex gap-2 pt-2">
          <Button
            variant="danger"
            onClick={() =>
              del(`volume ${vol.name}`, () => {
                submitJob("delete", `/volumes/${vol.id}?force=true`, null, `delete ${vol.name}`, refetch);
                close();
              })
            }
          >
            Delete volume
          </Button>
        </div>
      </div>
    </SlideOver>
  );
}

function Kv({ k, v, mono }: { k: string; v: string; mono?: boolean }) {
  return (
    <div>
      <div className="section-label">{k}</div>
      <div className={mono ? "mono" : ""}>{v}</div>
    </div>
  );
}
