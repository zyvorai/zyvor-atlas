// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
// Provision a raw, unformatted disk into a new ZFS pool or a new Ceph OSD (via Rook). A rare,
// high-consequence admin action (wipes a disk) — deliberately its own destination rather than a
// button on Backends/Cluster, and deliberately kept thin: one form, one recent-jobs table.
import { useState } from "react";
import { submitJob } from "../api/client";
import { useCephNodeDevices, useInvalidate, useJobs, useNodes, usePools, useZfsDevices } from "../api/hooks";
import { Badge, FormModal, type FormField } from "../ui/kit";
import { Table } from "../ui/Table";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import MacDisks from "./MacDisks";
import { fmtBytes, stateKind, timeAgo } from "../lib/format";

const JOB_TYPES = new Set([
  "zfs.pool.create_from_device",
  "zfs.pool.destroy",
  "ceph.osd.add_device",
]);

const JOB_KIND: Record<string, string> = {
  "ceph.osd.add_device": "Ceph OSD",
  "zfs.pool.destroy": "ZFS pool destroy",
};

// Devices in these states are never a valid target (mirrors the server's own unconditional
// refusals) — left out of the picker entirely rather than shown disabled, since the plain
// options-based FormField has no per-option disabled state.
const ZFS_UNSELECTABLE = new Set(["root_or_boot", "mounted", "zpool_member", "read_only"]);

const ZFS_STATUS_LABEL: Record<string, string> = {
  empty: "empty",
  has_data: "has data — wipeable",
};

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export default function Disks({ embedded = false }: { embedded?: boolean }) {
  const [open, setOpen] = useState(false);
  const [destroyPool, setDestroyPool] = useState<string | null>(null);
  const [selectedNode, setSelectedNode] = useState("");
  const { data: nodes } = useNodes();
  const { data: jobs } = useJobs();
  const { data: zfsDevices } = useZfsDevices();
  const { data: pools } = usePools();
  const zpools = (pools || []).filter((p) => p.kind === "zpool");
  const { data: cephDevices } = useCephNodeDevices(selectedNode);
  const inv = useInvalidate();

  const nodeOptions = (nodes || []).map((n) => ({ value: n.host, label: n.host }));
  const recentJobs = (jobs || []).filter((j) => JOB_TYPES.has(j.job_type));

  const zfsDeviceOptions = (zfsDevices || [])
    .filter((d) => !ZFS_UNSELECTABLE.has(d.status))
    .map((d) => ({
      value: d.path,
      label: `${d.path} · ${fmtBytes(d.size_bytes)} · ${ZFS_STATUS_LABEL[d.status] ?? d.status}`,
    }));
  const cephDeviceOptions = (cephDevices || [])
    .filter((d) => d.empty && !d.filesystem)
    .map((d) => ({
      value: `/dev/${d.name}`,
      label: `/dev/${d.name}${d.class ? ` · ${d.class.toUpperCase()}` : ""}${d.size ? ` · ${fmtBytes(d.size)}` : ""} · empty`,
    }));

  const fields = (vals: Record<string, string>): FormField[] => {
    const backend = vals.backend || "zfs";
    const out: FormField[] = [
      {
        name: "backend",
        label: "Provision as",
        options: [
          { value: "zfs", label: "ZFS pool" },
          { value: "ceph", label: "Ceph OSD (Rook)" },
        ],
      },
    ];
    if (backend === "ceph") {
      out.push({
        name: "node_name",
        label: "Node",
        options: nodeOptions,
        hint: nodeOptions.length ? undefined : "No nodes discovered yet.",
      });
    } else if (backend === "zfs") {
      out.push({
        name: "pool_name",
        label: "Pool name",
        placeholder: "tank2",
        pattern: /^[a-z][a-z0-9_-]*$/,
        hint: "zpool name — lowercase, no spaces.",
      });
    }
    if (backend === "zfs") {
      out.push({
        name: "device_path",
        label: "Device (detected on this host)",
        options: zfsDeviceOptions,
        hint: zfsDeviceOptions.length
          ? "Root/boot, mounted, read-only, and active-zpool-member devices are never shown — see docs/DISKS.md."
          : "No usable whole disks detected on this host.",
      });
    } else {
      out.push({
        name: "device_path",
        label: "Device (detected on the selected node)",
        options: cephDeviceOptions,
        hint: !vals.node_name
          ? "Pick a node first."
          : cephDeviceOptions.length
            ? "Only devices Rook's own discovery reports empty are shown."
            : "No empty devices discovered on this node yet (needs ROOK_ENABLE_DISCOVERY_DAEMON).",
      });
    }
    if (backend === "zfs") {
      out.push({
        name: "wipe_existing",
        label: "If the device already has data",
        options: [
          { value: "refuse", label: "Refuse (default, safest)" },
          { value: "wipe", label: "Wipe residual signatures first (destructive)" },
        ],
        hint:
          "Clears a stale partition table or filesystem signature (e.g. a decommissioned Ceph " +
          "OSD or an old ZFS pool) before formatting. Never overrides the root/boot-disk, " +
          "mounted-device or active-pool-member refusal.",
      });
    }
    // Cross-field "type it again to confirm" gate: FormModal's own per-field `pattern` check is
    // reused here rather than a bespoke modal — the pattern is just built from the *other* field's
    // current value. "\u0000" as the fallback can never be typed into a text input, so an empty
    // device_path makes this field impossible to satisfy (it's also still gated by the plain
    // required-field check below that point).
    out.push({
      name: "confirm_path",
      label: `Type "${vals.device_path || "the device path above"}" to confirm`,
      placeholder: vals.device_path || "/dev/sdX",
      pattern: new RegExp(`^${escapeRegExp(vals.device_path || "\u0000")}$`),
    });
    return out;
  };

  const content = (
    <>
      <MacDisks />
      <Table
        soundings
        panelTitle="ZFS pools on this host"
        rows={zpools}
        rowKey={(p) => p.id}
        empty="No ZFS pools."
        cols={[
          { h: "Pool", f: (p) => p.name, mono: true },
          { h: "Health", f: (p) => <Badge kind={stateKind(p.health)} dot>{p.health}</Badge> },
          {
            h: "",
            f: (p) => (
              <button type="button" className="at-btn danger" onClick={() => setDestroyPool(p.name)}>
                Destroy…
              </button>
            ),
          },
        ]}
      />

      <Table
        soundings
        panelTitle="Recent provisioning jobs"
        rows={recentJobs}
        rowKey={(j) => j.id}
        empty="No disk-provisioning jobs yet."
        cols={[
          { h: "Job", f: (j) => j.id, mono: true },
          {
            h: "Kind",
            f: (j) => JOB_KIND[j.job_type] ?? "ZFS pool",
          },
          { h: "State", f: (j) => <Badge kind={stateKind(j.state)} dot>{j.state}</Badge> },
          { h: "Requested", f: (j) => timeAgo(j.created_at) },
        ]}
      />

      <FormModal
        open={open}
        onClose={() => setOpen(false)}
        title="Provision a raw device"
        fields={fields}
        submitLabel="Provision (destructive)"
        onValuesChange={(v) => setSelectedNode(v.node_name || "")}
        danger
        onSubmit={async (vals) => {
          if (vals.backend === "ceph") {
            await submitJob(
              "post",
              "/ceph/devices",
              { node_name: vals.node_name, device_path: vals.device_path, confirm: true },
              `provision ${vals.device_path} → Ceph OSD (${vals.node_name})`,
              () => inv("osds", "pools", "clusters", "jobs"),
            );
          } else {
            await submitJob(
              "post",
              "/zfs/pools/from-device",
              {
                pool_name: vals.pool_name,
                device_path: vals.device_path,
                confirm: true,
                wipe_existing: vals.wipe_existing === "wipe",
              },
              `provision ${vals.device_path} → ZFS pool ${vals.pool_name}`,
              () => inv("pools", "nodes", "jobs"),
            );
          }
        }}
      />
      <FormModal
        open={destroyPool !== null}
        onClose={() => setDestroyPool(null)}
        title={`Destroy ZFS pool ${destroyPool ?? ""}`}
        submitLabel="Destroy pool (irreversible)"
        danger
        fields={[
          {
            name: "confirm_pool_name",
            label: `Type "${destroyPool ?? ""}" to confirm`,
            placeholder: destroyPool ?? "",
            pattern: new RegExp(`^${escapeRegExp(destroyPool || "\u0000")}$`),
            hint:
              "Refused while the pool still holds any dataset or volume. The disk keeps its ZFS " +
              "labels and then shows up in the device picker as wipeable.",
          },
        ]}
        onSubmit={async (vals) => {
          if (!destroyPool) return;
          await submitJob(
            "post",
            `/zfs/pools/${encodeURIComponent(destroyPool)}/destroy`,
            { confirm_pool_name: vals.confirm_pool_name },
            `destroy ZFS pool ${destroyPool}`,
            () => inv("pools", "nodes", "jobs"),
          );
        }}
      />
    </>
  );

  if (embedded) {
    return (
      <div>
        <div style={{ display: "flex", justifyContent: "flex-end", marginBottom: 12 }}>
          <button type="button" className="at-btn danger" onClick={() => setOpen(true)}>
          Provision device…
        </button>
        </div>
        {content}
      </div>
    );
  }

  return (
    <ListPage
      crumbs={navCrumbs("disks")}
      eyebrow="INFRASTRUCTURE · OPS"
      title="Disks"
      state="Format a raw disk as a ZFS pool or a Ceph OSD. Irreversible — wipes the target device."
      actions={
        <button type="button" className="at-btn danger" onClick={() => setOpen(true)}>
          Provision device…
        </button>
      }
    >
      {content}
    </ListPage>
  );
}
