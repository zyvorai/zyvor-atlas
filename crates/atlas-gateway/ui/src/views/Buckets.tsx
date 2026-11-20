// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useCallback, useEffect, useRef, useState } from "react";
import { Download, Plus, Upload } from "lucide-react";
import { apiError, http, submitJob, toast } from "../api/client";
import { useBackends, useBuckets, useInvalidate } from "../api/hooks";
import type { StorageBucket } from "../api/types";
import { Badge, Button, FormModal, SlideOver, type FormField } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { del } from "../ui/confirm";
import { Table } from "../ui/Table";
import BucketSettings from "./BucketSettings";
import { fmtBytes, num } from "../lib/format";

export default function Buckets() {
  const { data } = useBuckets();
  const { data: backends } = useBackends();
  const inv = useInvalidate();
  const refetch = () => inv("buckets", "summary");
  const [create, setCreate] = useState(false);
  const [objBucket, setObjBucket] = useState<StorageBucket | null>(null);
  const [settingsBucket, setSettingsBucket] = useState<StorageBucket | null>(null);
  const n = data?.length || 0;

  // NB: /backends records are keyed `id` (only /backends/summary uses `backend_id`).
  // Object backends: Ceph RGW is the default. Retired `rustfs` rows stay visible as labels only.
  const objectBackends = (backends || []).filter((b) => b.backend_type === "ceph");
  const backendLabel = (t?: string) => (t === "ceph" ? "Ceph RGW" : t === "rustfs" ? "RustFS (retired)" : t || "?");
  // Legacy bucket rows (created before backend_id was recorded) are Ceph RGW.
  const bucketBackend = (b: StorageBucket) => {
    if (!b.backend_id) return "Ceph RGW";
    const rec = (backends || []).find((x) => x.id === b.backend_id);
    return backendLabel(rec?.backend_type) === "?" ? b.backend_id : backendLabel(rec?.backend_type);
  };

  return (
    <ListPage
      crumbs={navCrumbs("buckets")}
      eyebrow="DATA PROTECTION · INDEX"
      title="Buckets"
      state={
        n
          ? `${n} bucket${n === 1 ? "" : "s"} on Ceph RGW — stats, browse / upload / download.`
          : "No buckets yet. Create the first bucket to begin exports and backups."
      }
      actions={
        <>
          <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
            <Plus size={14} /> Bucket
          </button>
        </>
      }
    >
      <Table
        soundings
        panelTitle="Bucket index"
        rows={data}
        rowKey={(b) => b.id}
        empty="No buckets yet."
        emptyCta={
          <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
            <Plus size={14} /> Create bucket
          </button>
        }
        cols={[
          { h: "Name", f: (b) => b.bucket_name || b.name || b.id, mono: true },
          { h: "Backend", f: (b) => <Badge kind="info">{bucketBackend(b)}</Badge> },
          { h: "State", f: (b) => <Badge kind={b.state === "bound" ? "success" : "warning"} dot>{b.state}</Badge> },
          { h: "Namespace", f: (b) => b.namespace },
          { h: "Endpoint", f: (b) => <span className="mono" style={{ color: "var(--at-ink-4)" }}>{b.endpoint || "—"}</span> },
        ]}
        actions={(b) => (
          <>
            <Button size="sm" onClick={async () => {
              try {
                const s = await http.get(`/buckets/${b.id}/stats`, { timeout: 14000 });
                const d = s.data;
                if (d?.available === false) {
                  toast(d.reason ? `Stats unavailable: ${d.reason}` : "Stats unavailable right now", "info");
                  return;
                }
                toast(`${d.bucket}: ${num(d.num_objects)} objs, ${fmtBytes(d.size_bytes)}`, "ok");
              } catch (e) {
                const msg = apiError(e);
                const soft =
                  /timeout|timed out|network|ECONNABORTED|unavailable|no bucket_name/i.test(msg)
                  || (e as { code?: string })?.code === "ECONNABORTED"
                  || (e as { response?: { status?: number } })?.response?.status === 400
                  || (e as { response?: { status?: number } })?.response?.status === 504;
                toast(soft ? "Stats unavailable right now" : `stats: ${msg}`, soft ? "info" : "err");
              }
            }}>Stats</Button>
            <Button size="sm" onClick={() => setObjBucket(b)}>Objects</Button>
            <Button size="sm" variant="danger" onClick={() => del(`bucket ${b.bucket_name || b.name || b.id}`, () => submitJob("delete", `/buckets/${b.id}?force=true`, null, "delete bucket", refetch))}>Del</Button>
          </>
        )}
      />

      <FormModal open={create} onClose={() => setCreate(false)} title="Create bucket" submitLabel="Create"
        fields={(vals): FormField[] => {
          const sel = vals.backend_id || objectBackends[0]?.id;
          const selType = objectBackends.find((b) => b.id === sel)?.backend_type;
          const out: FormField[] = [
            {
              name: "backend_id", label: "Backend",
              options: objectBackends.map((b) => ({ value: b.id, label: `${backendLabel(b.backend_type)} (${b.id})` })),
              hint: "No object backend registered.",
            },
            {
              name: "name", label: "Name",
              pattern: /^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$/,
              hint: "3-63 chars: lowercase letters, digits, dots, hyphens (S3 bucket naming rules).",
            },
          ];
          // Ceph RGW buckets are provisioned via a Rook ObjectBucketClaim (namespace + optional
          // quotas).
          if (selType === "ceph") {
            out.push(
              { name: "namespace", label: "Namespace", value: "rook-ceph" },
              { name: "max_objects", label: "Max objects (optional)", type: "number", optional: true, min: 0 },
              { name: "max_size", label: "Max size (e.g. 2G, optional)", optional: true },
            );
          }
          return out;
        }}
        onSubmit={(v) => {
          const backendId = v.backend_id || objectBackends[0]?.id;
          const isCeph = objectBackends.find((b) => b.id === backendId)?.backend_type === "ceph";
          const body: Record<string, unknown> = { name: v.name, backend_id: backendId };
          if (isCeph) {
            body.namespace = v.namespace;
            if (v.max_objects) body.max_objects = +v.max_objects;
            if (v.max_size) body.max_size = v.max_size;
          }
          return submitJob("post", "/buckets", body, "bucket", refetch);
        }} />

      <ObjectBrowser bucket={objBucket} onClose={() => setObjBucket(null)} />
      <BucketSettings bucket={settingsBucket} onClose={() => setSettingsBucket(null)} />
    </ListPage>
  );
}

function ObjectBrowser({ bucket, onClose }: { bucket: StorageBucket | null; onClose: () => void }) {
  const [objs, setObjs] = useState<{ key: string; size_bytes: number }[] | null>(null);
  const [listError, setListError] = useState(false);
  const [prefix, setPrefix] = useState("");
  const [busy, setBusy] = useState(false);
  const [keep, setKeep] = useState(0); // 0 = overwrite in place; >0 = keep N timestamped versions
  const fileRef = useRef<HTMLInputElement>(null);
  const load = useCallback(
    (p = "") => bucket && http.get(`/buckets/${bucket.id}/objects${p ? "?prefix=" + encodeURIComponent(p) : ""}`)
      .then((r) => { setObjs(r.data.objects || []); setListError(false); })
      .catch((e) => { setObjs([]); setListError(true); toast(`list objects: ${apiError(e)}`, "err"); }),
    [bucket],
  );
  useEffect(() => { if (bucket) load(); }, [bucket, load]);
  const close = () => { setObjs(null); setListError(false); setPrefix(""); onClose(); };
  if (!bucket) return null;

  // Upload straight to the object store (S3/RGW): gateway mints a presigned PUT, the browser PUTs the file to it —
  // the object bytes never pass through atlas-gateway, so large db files scale fine. With keep>0
  // each upload is stored as a timestamped version and old ones are pruned to keep N (db backups).
  const upload = async (files: FileList | null) => {
    if (!files || !files.length) return;
    // Snapshot first: `files` is a live FileList that the browser empties when the input's value
    // is reset, which made the success toast below report "uploaded 0 file(s)".
    const list = Array.from(files);
    setBusy(true);
    try {
      for (const file of list) {
        const key = (prefix ? prefix.replace(/\/+$/, "") + "/" : "") + file.name;
        const versioned = keep > 0;
        const r = await http.post(`/buckets/${bucket.id}/objects/upload-url`, { key, versioned });
        const put = await fetch(r.data.url, { method: "PUT", body: file });
        if (!put.ok) throw new Error(`upload ${file.name}: HTTP ${put.status}`);
        if (versioned) await http.post(`/buckets/${bucket.id}/objects/prune`, { prefix: r.data.base_key + ".", keep });
      }
      toast(`uploaded ${list.length} file(s)${keep > 0 ? `, keeping ${keep} versions` : ""}`, "ok");
      await load(prefix);
    } catch (e) { toast(String(e), "err"); }
    finally { setBusy(false); }
  };

  const download = async (key: string) => {
    try {
      const r = await http.get(`/buckets/${bucket.id}/objects/download-url?key=${encodeURIComponent(key)}`);
      window.open(r.data.url, "_blank");
    } catch (e) { toast(String(e), "err"); }
  };

  const remove = (key: string) => del(`object ${key}`, async () => {
    try { await http.delete(`/buckets/${bucket.id}/objects?key=${encodeURIComponent(key)}`); toast("deleted", "ok"); await load(prefix); }
    catch (e) { toast(String(e), "err"); }
  });

  return (
    <SlideOver open={!!bucket} onClose={close} title={<span className="mono">{bucket.bucket_name || bucket.name || bucket.id} · objects</span>} width={560}>
      <div className="flex gap-2 mb-3 items-center">
        <input className="field" placeholder="prefix / folder…" value={prefix} onChange={(e) => setPrefix(e.target.value)} />
        <Button onClick={() => load(prefix)}>List</Button>
        <input className="field" style={{ width: 84 }} type="number" min={0} title="Keep N timestamped versions per file (0 = overwrite in place)" placeholder="keep" value={keep} onChange={(e) => setKeep(Math.max(0, +e.target.value || 0))} />
        <Button variant="primary" icon={Upload} loading={busy} onClick={() => fileRef.current?.click()}>Upload</Button>
        <input ref={fileRef} type="file" multiple hidden onChange={(e) => { upload(e.target.files); e.currentTarget.value = ""; }} />
      </div>
      <Table rows={objs ?? undefined} rowKey={(o) => o.key}
        cols={[{ h: "Key", f: (o) => o.key, mono: true }, { h: "Size", f: (o) => fmtBytes(o.size_bytes) }]}
        actions={(o) => (
          <>
            <Button size="sm" icon={Download} onClick={() => download(o.key)}>Get</Button>
            <Button size="sm" variant="danger" onClick={() => remove(o.key)}>Del</Button>
          </>
        )}
        empty={listError ? "Couldn't list objects (see toast for the error)." : "No objects — upload one above."} />
    </SlideOver>
  );
}
