// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useState } from "react";
import { Plus } from "lucide-react";
import { submit } from "../api/client";
import { useBuckets, useInvalidate, useSchedules, useVolumes } from "../api/hooks";
import { Badge, Button, FormModal } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { del } from "../ui/confirm";
import { Table } from "../ui/Table";
import { timeAgo } from "../lib/format";

export default function Schedules() {
  const { data } = useSchedules();
  const { data: vols } = useVolumes();
  const { data: buckets } = useBuckets();
  const inv = useInvalidate();
  const [create, setCreate] = useState(false);
  const n = data?.length || 0;
  return (
    <ListPage
      crumbs={navCrumbs("schedules")}
      eyebrow="DATA PROTECTION · INDEX"
      title="Schedules"
      state={
        n
          ? `${n} protection schedule${n === 1 ? "" : "s"} — periodic snapshots & backups for a volume.`
          : "No schedules yet. Create one to automate snapshots or backups."
      }
      actions={
        <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
          <Plus size={14} /> Schedule
        </button>
      }
    >
      <Table
        soundings
        panelTitle="Schedule index"
        rows={data}
        rowKey={(s) => s.id}
        empty="No schedules yet."
        emptyCta={
          <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
            <Plus size={14} /> Create schedule
          </button>
        }
        cols={[
          { h: "ID", f: (s) => s.id, mono: true },
          { h: "Kind", f: (s) => <Badge kind={s.kind === "backup" ? "info" : "neutral"}>{s.kind}</Badge> },
          { h: "Volume", f: (s) => s.volume_id, mono: true },
          { h: "Every", f: (s) => `${s.interval_secs}s` },
          { h: "Keep", f: (s) => s.keep },
          { h: "Bucket", f: (s) => s.bucket_id || "—", mono: true },
          { h: "Next run", f: (s) => <span className="text-muted-foreground">{timeAgo(s.next_run_at)}</span> },
        ]}
        actions={(s) => (
          <Button size="sm" variant="danger" onClick={() => del("schedule", () => submit("delete", `/schedules/${s.id}`, null, "delete schedule", () => inv("schedules")))}>Del</Button>
        )}
      />

      <FormModal open={create} onClose={() => setCreate(false)} title="Create schedule" submitLabel="Create schedule"
        fields={(vals) => [
          {
            name: "volume_id", label: "Volume",
            // Both snapshot and backup schedules dispatch a CSI VolumeSnapshot job, which needs a
            // PVC-backed (k8s) volume — filter out raw NFS/ZFS volumes so a schedule can't be
            // created against one that would silently never fire (backend also enforces this).
            options: (vols || []).filter((v) => v.pvc_name).map((v) => ({ value: v.id, label: v.name })),
            hint: "No PVC-backed volumes yet — create one on the Volumes page first.",
          },
          { name: "kind", label: "Kind", options: [{ value: "snapshot", label: "snapshot" }, { value: "backup", label: "backup" }] },
          { name: "interval_secs", label: "Interval (seconds)", type: "number", value: "3600", min: 60 },
          { name: "keep", label: "Keep", type: "number", value: "24", min: 0 },
          // Bucket/Mode only matter for backup schedules — hide them for snapshot schedules.
          ...(vals.kind === "backup" ? [
            { name: "bucket_id", label: "Bucket (backup only)", options: [{ value: "", label: "—" }, ...(buckets || []).filter((b) => b.state === "bound").map((b) => ({ value: b.id, label: b.bucket_name || b.id }))] },
            { name: "mode", label: "Mode (backup)", options: [{ value: "manifest", label: "manifest" }, { value: "data", label: "data" }] },
          ] : []),
        ]}
        onSubmit={(v) => submit("post", `/volumes/${v.volume_id}/schedule`, {
          kind: v.kind, interval_secs: +v.interval_secs, keep: +v.keep,
          bucket_id: v.bucket_id || undefined, mode: v.mode,
        }, "schedule", () => inv("schedules"))} />
    </ListPage>
  );
}
