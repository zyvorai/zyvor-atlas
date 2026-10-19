// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
// External-disk lifecycle for Macs (docs/MACOS.md): erase, add a volume, resize a container, eject,
// on the Mac the gateway runs on or on a fleet Mac through its agent. The server refuses internal
// disks, the running system and system volumes whatever is sent here; this page only keeps the
// obviously-refused actions out of reach and makes the destructive ones type the target back.
import { useState } from "react";
import { submitJob } from "../api/client";
import { useBackends, useInvalidate, useMacDevices, useMacSnapshots, type MacDevice } from "../api/hooks";
import { Badge, FormModal, type FormField } from "../ui/kit";
import { Table } from "../ui/Table";

type Action =
  | { kind: "erase"; device: string }
  | { kind: "eject"; device: string }
  | { kind: "add"; container: string }
  | { kind: "resize"; container: string }
  | { kind: "snap-delete"; date: string };

const GIB = 2 ** 30;

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

const confirmField = (target: string): FormField => ({
  name: "confirm_device",
  label: `Type "${target}" to confirm`,
  placeholder: target,
  pattern: new RegExp(`^${escapeRegExp(target || "\u0000")}$`),
});

export default function MacDisks() {
  const { data: backends } = useBackends();
  const macs = (backends || []).filter((b) => String(b.id ?? "").startsWith("bkd_macos"));
  const [chosen, setChosen] = useState("");
  const backend = chosen || macs[0]?.id || "";
  const { data: devices } = useMacDevices(backend);
  const { data: snapshots } = useMacSnapshots(backend);
  const [action, setAction] = useState<Action | null>(null);
  const inv = useInvalidate();

  if (!macs.length) return null;
  const q = `?backend=${encodeURIComponent(backend)}`;
  const refresh = () => inv("macos-devices", "macos-snapshots", "pools", "volumes", "jobs");
  const close = () => setAction(null);

  const fieldsFor = (a: Action): FormField[] => {
    switch (a.kind) {
      case "erase":
        return [
          { name: "name", label: "New volume name", placeholder: "Backup", pattern: /^[A-Za-z0-9][A-Za-z0-9 _.-]{0,31}$/ },
          confirmField(a.device),
        ];
      case "add":
        return [
          { name: "name", label: "Volume name", placeholder: "Scratch", pattern: /^[A-Za-z0-9][A-Za-z0-9 _.-]{0,31}$/ },
          { name: "quota_gib", label: "Quota (GiB)", type: "number", min: 1, optional: true, hint: "Empty means no quota." },
        ];
      case "resize":
        return [
          { name: "size_gib", label: "New container size (GiB)", type: "number", min: 1, hint: "Cannot go below what the container already holds." },
          confirmField(a.container),
        ];
      case "snap-delete":
        return [confirmField(a.date)];
      case "eject":
        return [];
    }
  };

  const submit = async (a: Action, v: Record<string, string>) => {
    switch (a.kind) {
      case "erase":
        await submitJob(
          "post",
          `/macos/disks/erase${q}`,
          { device: a.device, confirm_device: v.confirm_device, name: v.name },
          `erase ${a.device} → APFS "${v.name}"`,
          refresh,
        );
        return;
      case "eject":
        await submitJob("post", `/macos/disks/${a.device}/eject${q}`, {}, `eject ${a.device}`, refresh);
        return;
      case "add":
        await submitJob(
          "post",
          `/macos/containers/${a.container}/volumes${q}`,
          { name: v.name, quota_bytes: v.quota_gib ? Math.round(Number(v.quota_gib) * GIB) : undefined },
          `add volume "${v.name}" to ${a.container}`,
          refresh,
        );
        return;
      case "snap-delete":
        await submitJob(
          "post",
          `/macos/snapshots/${a.date}/delete${q}`,
          { confirm_device: v.confirm_device },
          `delete local snapshot ${a.date}`,
          refresh,
        );
        return;
      case "resize":
        await submitJob(
          "post",
          `/macos/containers/${a.container}/resize${q}`,
          { size_bytes: Math.round(Number(v.size_gib) * GIB), confirm_device: v.confirm_device },
          `resize ${a.container} to ${v.size_gib} GiB`,
          refresh,
        );
    }
  };

  const title = (a: Action | null) =>
    !a ? "" : { erase: `Erase ${"device" in a ? a.device : ""}`, eject: `Eject ${"device" in a ? a.device : ""}`, add: `Add a volume to ${"container" in a ? a.container : ""}`, resize: `Resize ${"container" in a ? a.container : ""}`, "snap-delete": `Delete local snapshot ${"date" in a ? a.date : ""}` }[a.kind];

  return (
    <>
      <Table
        soundings
        panelTitle={`Mac disks · ${backend}`}
        rows={devices || []}
        rowKey={(d: MacDevice) => d.device}
        empty="No external disks reported."
        cols={[
          { h: "Disk", f: (d: MacDevice) => d.device, mono: true },
          { h: "Media", f: (d: MacDevice) => d.media_name ?? "—" },
          { h: "Bus", f: (d: MacDevice) => d.bus ?? "—" },
          {
            h: "Kind",
            f: (d: MacDevice) =>
              d.internal ? <Badge kind="warning">internal · read-only</Badge> : d.disk_image ? <Badge kind="neutral">disk image</Badge> : <Badge kind="success">external</Badge>,
          },
          { h: "SMART", f: (d: MacDevice) => d.smart ?? "—" },
          {
            h: "Actions",
            f: (d: MacDevice) =>
              d.internal ? (
                <span title={d.erase_refusal ?? ""}>refused by Atlas</span>
              ) : (
                <span style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
                  <button type="button" className="at-btn danger" disabled={!!d.erase_refusal} title={d.erase_refusal ?? ""} onClick={() => setAction({ kind: "erase", device: d.device })}>
                    Erase…
                  </button>
                  {d.containers.map((c) => (
                    <span key={c} style={{ display: "inline-flex", gap: 6 }}>
                      <button type="button" className="at-btn" onClick={() => setAction({ kind: "add", container: c })}>
                        Add volume ({c})…
                      </button>
                      <button type="button" className="at-btn" onClick={() => setAction({ kind: "resize", container: c })}>
                        Resize ({c})…
                      </button>
                    </span>
                  ))}
                  <button type="button" className="at-btn" onClick={() => setAction({ kind: "eject", device: d.device })}>
                    Eject
                  </button>
                </span>
              ),
          },
        ]}
      />
      <Table
        soundings
        panelTitle="Local snapshots (the whole Mac, purgeable)"
        rows={(snapshots || []).map((date) => ({ date }))}
        rowKey={(r: { date: string }) => r.date}
        empty="No local snapshots."
        cols={[
          { h: "Date", f: (r: { date: string }) => r.date, mono: true },
          {
            h: "",
            f: (r: { date: string }) => (
              <button type="button" className="at-btn danger" onClick={() => setAction({ kind: "snap-delete", date: r.date })}>
                Delete…
              </button>
            ),
          },
        ]}
      />
      <button
        type="button"
        className="at-btn"
        title="macOS cannot snapshot one volume; this snapshots the Mac (tmutil localsnapshot)"
        onClick={() => void submitJob("post", `/macos/snapshots${q}`, {}, "create local snapshot", refresh)}
      >
        Create local snapshot
      </button>
      {macs.length > 1 && (
        <label style={{ display: "block", margin: "8px 0" }}>
          Mac{" "}
          <select value={backend} onChange={(e) => setChosen(e.target.value)}>
            {macs.map((m) => (
              <option key={m.id} value={m.id}>
                {m.name ?? m.id}
              </option>
            ))}
          </select>
        </label>
      )}
      <FormModal
        open={action !== null && action.kind !== "eject"}
        onClose={close}
        title={title(action)}
        fields={action && action.kind !== "eject" ? fieldsFor(action) : []}
        submitLabel={action?.kind === "add" ? "Add volume" : action?.kind === "resize" ? "Resize" : action?.kind === "snap-delete" ? "Delete snapshot" : "Erase (destructive)"}
        danger={action?.kind === "erase" || action?.kind === "snap-delete"}
        onSubmit={async (v) => {
          if (action) await submit(action, v);
        }}
      />
      <FormModal
        open={action?.kind === "eject"}
        onClose={close}
        title={title(action)}
        fields={[]}
        submitLabel="Eject"
        onSubmit={async (v) => {
          if (action) await submit(action, v);
        }}
      />
    </>
  );
}
