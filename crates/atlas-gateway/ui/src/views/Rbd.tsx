// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useEffect, useState } from "react";
import { Plus, RefreshCw } from "lucide-react";
import { apiError, http, submit, submitJob } from "../api/client";
import { useInvalidate, usePools, useRbdImages, useVolumes } from "../api/hooks";
import { Button, FormModal, SlideOver } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { confirmThen, del } from "../ui/confirm";
import { Table } from "../ui/Table";
import { fmtBytes, gib } from "../lib/format";

export default function Rbd() {
  const [pool, setPool] = useState("rbd-nvme-prod");
  const { data, isError, error, refetch: refetchImages } = useRbdImages(pool);
  const { data: vols } = useVolumes();
  const { data: pools } = usePools();
  const rbdPools = (pools || []).filter((p) => p.kind?.toLowerCase() === "rbd");
  const inv = useInvalidate();
  const refetch = () => inv("rbd");
  const [create, setCreate] = useState(false);
  const [modal, setModal] = useState<{ img: string; kind: string } | null>(null);
  const [snapImg, setSnapImg] = useState<string | null>(null);
  // Raw RBD images aren't tracked with a size by /rbd-images; cross-reference the volume
  // inventory (CSI-provisioned images are), so Resize can show/guard against the current size.
  const sizeOf = (img: string) => vols?.find((v) => v.backend_native_id === `rbd:${pool}/${img}`)?.size_bytes;
  const [cloneSnaps, setCloneSnaps] = useState<string[] | null>(null);
  useEffect(() => {
    if (modal?.kind === "clone") {
      // Clear stale snapshots from a previous clone dialog before fetching the current one's.
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setCloneSnaps(null);
      http.get(`/rbd-images/${pool}/${modal.img}/snapshots`).then((r) => setCloneSnaps(r.data.snapshots || [])).catch(() => setCloneSnaps([]));
    }
  }, [modal, pool]);

  const n = data?.images?.length || 0;
  return (
    <ListPage
      crumbs={navCrumbs("rbd")}
      eyebrow="STORAGE · INDEX"
      title="RBD Images"
      state={
        n
          ? `${n} raw image${n === 1 ? "" : "s"} in ${pool} — machina/libvirt & bare VMs (bypassing CSI).`
          : `No images in ${pool}. Create one or switch pool.`
      }
      actions={
        <>
          <select className="field w-44" value={pool} onChange={(e) => setPool(e.target.value)}>
            {rbdPools.length ? (
              rbdPools.map((p) => (
                <option key={p.id} value={p.name}>
                  {p.name}
                </option>
              ))
            ) : (
              <option value={pool}>{pool}</option>
            )}
          </select>
          <button
            type="button"
            className="at-btn"
            onClick={() => submit("post", "/rbd-usage/refresh", null, "usage refresh", () => inv("volumes")).catch(() => {})}
          >
            <RefreshCw size={14} /> Refresh usage
          </button>
          <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
            <Plus size={14} /> Image
          </button>
        </>
      }
    >
      <Table
        soundings
        panelTitle={`Images · ${pool}`}
        rows={data?.images}
        error={isError}
        errorDetail={isError ? apiError(error) : undefined}
        onRetry={() => refetchImages()}
        rowKey={(i) => i}
        empty="No RBD images in this pool."
        emptyCta={
          <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
            <Plus size={14} /> Create image
          </button>
        }
        cols={[
          { h: "Image", f: (i) => i, mono: true },
          {
            h: "Size",
            f: (i) => {
              const sz = sizeOf(i);
              return sz != null ? fmtBytes(sz) : <span style={{ color: "var(--at-ink-4)" }}>—</span>;
            },
          },
        ]}
        actions={(img) => (
          <>
            <Button size="sm" onClick={() => setSnapImg(img)}>Snaps</Button>
            <Button size="sm" onClick={() => setModal({ img, kind: "clone" })}>Clone</Button>
            <Button size="sm" onClick={() => setModal({ img, kind: "resize" })}>Resize</Button>
            <Button size="sm" onClick={() => confirmThen({ title: `Flatten ${img}?`, message: "Detaches the clone from its parent (copies all data).", confirmLabel: "Flatten" }, () => submitJob("post", `/rbd-images/${pool}/${img}/flatten`, {}, `flatten ${img}`, refetch))}>Flatten</Button>
            <Button size="sm" variant="danger" onClick={() => del(`image ${img}`, () => submitJob("delete", `/rbd-images/${pool}/${img}`, null, `delete ${img}`, refetch))}>Del</Button>
          </>
        )}
      />

      <FormModal open={create} onClose={() => setCreate(false)} title="Create RBD image" submitLabel="Create"
        fields={[{ name: "name", label: "Name" }, { name: "size_gib", label: "Size (GiB)", type: "number", value: "1", min: 1 }]}
        onSubmit={(v) => submitJob("post", "/rbd-images", { name: v.name, size_bytes: gib(+v.size_gib), pool }, `create ${v.name}`, refetch)} />

      {modal?.kind === "clone" && (
        cloneSnaps && cloneSnaps.length === 0 ? (
          <FormModal open onClose={() => setModal(null)} title={`Clone ${modal.img}`} submitLabel="Clone"
            fields={[{ name: "name", label: "Clone name" }, { name: "snap", label: "Snapshot", value: "", hint: `${modal.img} has no snapshots yet — create one first (Snaps → snapshot name), then retry.` }]}
            onSubmit={(v) => submitJob("post", `/rbd-images/${pool}/${modal.img}/clone`, { name: v.name, snap: v.snap }, "clone", refetch)} />
        ) : (
          <FormModal open onClose={() => setModal(null)} title={`Clone ${modal.img}`} submitLabel="Clone"
            fields={[
              { name: "name", label: "Clone name" },
              { name: "snap", label: "Snapshot", options: (cloneSnaps || []).map((s) => ({ value: s, label: s })) },
            ]}
            onSubmit={(v) => submitJob("post", `/rbd-images/${pool}/${modal.img}/clone`, { name: v.name, snap: v.snap }, "clone", refetch)} />
        )
      )}
      {modal?.kind === "resize" && (() => {
        const cur = sizeOf(modal.img);
        const curGib = cur != null ? Math.ceil(cur / 1073741824) : undefined;
        return (
          <FormModal open onClose={() => setModal(null)} title={`Resize ${modal.img}`} submitLabel="Resize"
            fields={[{
              name: "size_gib", label: "New size (GiB)", type: "number", min: curGib != null ? curGib : 1,
              hint: cur != null
                ? `Current size: ${fmtBytes(cur)} — shrinking isn't supported from this dialog.`
                : "Current size unknown for this image (not tracked as a volume).",
            }]}
            onSubmit={(v) => submitJob("post", `/rbd-images/${pool}/${modal.img}/resize`, { size_bytes: gib(+v.size_gib) }, "resize", refetch)} />
        );
      })()}

      <RbdSnaps pool={pool} img={snapImg} onClose={() => setSnapImg(null)} />
    </ListPage>
  );
}

function RbdSnaps({ pool, img, onClose }: { pool: string; img: string | null; onClose: () => void }) {
  const [snaps, setSnaps] = useState<string[] | null>(null);
  const [name, setName] = useState("");
  const load = () => img && http.get(`/rbd-images/${pool}/${img}/snapshots`).then((r) => setSnaps(r.data.snapshots || [])).catch(() => setSnaps([]));
  if (img && snaps === null) load();
  const close = () => { setSnaps(null); onClose(); };
  if (!img) return null;
  return (
    <SlideOver open={!!img} onClose={close} title={<span className="mono">{img} · snapshots</span>}>
      <div className="flex gap-2 mb-4">
        <input className="field" placeholder="snapshot name" value={name} onChange={(e) => setName(e.target.value)} />
        <Button variant="primary" disabled={!name || !/^[a-zA-Z0-9_.-]+$/.test(name)} onClick={async () => { await submitJob("post", `/rbd-images/${pool}/${img}/snapshots`, { name }, "snapshot"); setName(""); setTimeout(load, 1200); }}>Create</Button>
      </div>
      {name && !/^[a-zA-Z0-9_.-]+$/.test(name) && <div className="text-xs text-danger -mt-2 mb-3">Letters, digits, dot, dash, underscore only (no spaces or slashes).</div>}
      <Table
        rows={snaps || []}
        rowKey={(s) => s}
        cols={[{ h: "Snapshot", f: (s) => s, mono: true }]}
        actions={(s) => (
          <>
            <Button size="sm" variant="danger" onClick={() => confirmThen({ title: `Roll back to ${s}?`, message: "Destructive: reverts the image to this snapshot's contents.", confirmLabel: "Roll back", danger: true }, () => submitJob("post", `/rbd-images/${pool}/${img}/rollback`, { name: s }, `rollback to ${s}`))}>Rollback</Button>
            <Button size="sm" variant="danger" onClick={() => del(`snapshot ${s}`, () => submitJob("delete", `/rbd-images/${pool}/${img}/snapshots/${s}`, null, `delete snapshot ${s}`, load))}>Del</Button>
          </>
        )}
      />
    </SlideOver>
  );
}
