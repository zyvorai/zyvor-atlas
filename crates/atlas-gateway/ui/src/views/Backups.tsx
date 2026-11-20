// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useState } from "react";
import { Plus } from "lucide-react";
import { http, submitJob, toast } from "../api/client";
import { useBackups, useBuckets, useInvalidate, useVolumes } from "../api/hooks";
import type { BackupRecord } from "../api/types";
import { Badge, Button, FormModal } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { del } from "../ui/confirm";
import { Table } from "../ui/Table";
import { stateKind, timeAgo } from "../lib/format";

export default function Backups() {
  const { data } = useBackups();
  const { data: vols } = useVolumes();
  const { data: buckets } = useBuckets();
  const inv = useInvalidate();
  const refetch = () => inv("backups", "summary");
  const [create, setCreate] = useState(false);
  const [restore, setRestore] = useState<BackupRecord | null>(null);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const toggle = (k: string) =>
    setPicked((s) => {
      const n = new Set(s);
      if (n.has(k)) n.delete(k);
      else n.add(k);
      return n;
    });
  const toggleAll = (keys: string[]) => setPicked((s) => (keys.every((k) => s.has(k)) ? new Set() : new Set(keys)));
  const bulkDelete = () =>
    del(`${picked.size} backup(s)`, () => {
      const ids = [...picked];
      setPicked(new Set());
      ids.forEach((id) => submitJob("delete", `/backups/${id}`, null, "delete backup", refetch).catch(() => {}));
    });
  const n = data?.length || 0;

  return (
    <ListPage
      crumbs={navCrumbs("backups")}
      eyebrow="DATA PROTECTION · INDEX"
      title="Backups"
      state={
        n
          ? `${n} backup${n === 1 ? "" : "s"} to RGW — restore or presign a download.`
          : "No backups yet. Export a PVC-backed volume to a bound bucket."
      }
      actions={
        <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
          <Plus size={14} /> Backup
        </button>
      }
    >
      <Table
        soundings
        panelTitle="Backup index"
        panelExtra={
          picked.size > 0 ? (
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
          ) : undefined
        }
        rows={data}
        rowKey={(b) => b.id}
        selectable
        selected={picked}
        onToggle={toggle}
        onToggleAll={toggleAll}
        empty="No backups yet."
        emptyCta={
          <button type="button" className="at-btn primary" onClick={() => setCreate(true)}>
            <Plus size={14} /> Back up a volume
          </button>
        }
        cols={[
          { h: "ID", f: (b) => b.id, mono: true },
          { h: "Volume", f: (b) => b.volume_id, mono: true, sortKey: (b) => b.volume_id },
          { h: "Format", f: (b) => b.format, sortKey: (b) => b.format },
          { h: "State", f: (b) => <Badge kind={stateKind(b.state)} dot>{b.state}</Badge>, sortKey: (b) => b.state },
          {
            h: "Checksum",
            f: (b) => <span className="mono" style={{ color: "var(--at-ink-4)" }}>{(b.checksum || "").slice(0, 12) || "—"}</span>,
          },
          {
            h: "Created",
            f: (b) => <span style={{ color: "var(--at-ink-4)" }}>{timeAgo(b.created_at)}</span>,
            sortKey: (b) => b.created_at || "",
          },
        ]}
        actions={(b) => (
          <>
            <Button size="sm" onClick={() => setRestore(b)}>
              Restore
            </Button>
            <Button
              size="sm"
              onClick={async () => {
                try {
                  const r = await http.get(`/backups/${b.id}/download?what=data`);
                  window.open(r.data.url, "_blank");
                } catch {
                  const r = await http.get(`/backups/${b.id}/download?what=manifest`).catch(() => null);
                  if (r) window.open(r.data.url, "_blank");
                  else toast("no download", "err");
                }
              }}
            >
              Download
            </Button>
            <Button
              size="sm"
              variant="danger"
              onClick={() => del(`backup ${b.id}`, () => submitJob("delete", `/backups/${b.id}`, null, "delete backup", refetch))}
            >
              Del
            </Button>
          </>
        )}
      />

      <FormModal
        open={create}
        onClose={() => setCreate(false)}
        title="Back up a volume"
        submitLabel="Back up"
        fields={[
          {
            name: "volume_id",
            label: "Volume",
            options: (vols || []).filter((v) => v.pvc_name).map((v) => ({ value: v.id, label: v.name })),
            hint: "No PVC-backed volumes to back up yet.",
          },
          {
            name: "bucket_id",
            label: "Bucket",
            options: (buckets || []).filter((b) => b.state === "bound").map((b) => ({ value: b.id, label: b.bucket_name || b.id })),
            hint: "No bound buckets yet — create one on the Buckets page first.",
          },
          {
            name: "mode",
            label: "Mode",
            options: [
              { value: "manifest", label: "manifest" },
              { value: "data", label: "data (rbd export-diff)" },
            ],
          },
          { name: "keep", label: "Keep (0=all)", type: "number", value: "0", min: 0 },
          { name: "max_age_secs", label: "Max age secs (0=off)", type: "number", value: "0", min: 0 },
        ]}
        onSubmit={(v) =>
          submitJob(
            "post",
            "/backup-jobs",
            {
              volume_id: v.volume_id,
              bucket_id: v.bucket_id,
              mode: v.mode,
              keep: +v.keep,
              max_age_secs: +v.max_age_secs,
            },
            "backup",
            refetch,
          )
        }
      />

      {restore && (
        <FormModal
          open
          onClose={() => setRestore(null)}
          title={`Restore ${restore.id}`}
          submitLabel="Restore"
          fields={[
            { name: "name", label: "New volume name (optional)", optional: true },
            {
              name: "mode",
              label: "Mode",
              options: [
                { value: "snapshot", label: "snapshot" },
                { value: "data", label: "data" },
              ],
            },
          ]}
          onSubmit={(v) =>
            submitJob(
              "post",
              "/restore-jobs",
              { backup_id: restore.id, name: v.name || undefined, mode: v.mode },
              "restore",
              () => inv("volumes"),
            )
          }
        />
      )}
    </ListPage>
  );
}
