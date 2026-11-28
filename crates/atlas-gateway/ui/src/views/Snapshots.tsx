// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useInvalidate, useSnapshots } from "../api/hooks";
import type { StorageSnapshot } from "../api/types";
import { Badge, Button, FormModal } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { del } from "../ui/confirm";
import { Table } from "../ui/Table";
import { stateKind, timeAgo } from "../lib/format";
import { apiError, submitJob } from "../api/client";
import { EmptyState } from "../graphics";
import "./Snapshots.css";

export default function Snapshots() {
  const nav = useNavigate();
  const { data, isError, error, refetch: reload } = useSnapshots();
  const inv = useInvalidate();
  const refetch = () => inv("snapshots", "volumes");
  const [modal, setModal] = useState<{ s: StorageSnapshot; kind: string } | null>(null);
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
    del(`${picked.size} snapshot(s)`, () => {
      const ids = [...picked];
      setPicked(new Set());
      ids.forEach((id) => submitJob("delete", `/snapshots/${id}?force=true`, null, "delete snapshot", refetch).catch(() => {}));
    });
  const n = data?.length || 0;
  return (
    <ListPage
      crumbs={navCrumbs("snapshots")}
      eyebrow={null}
      title="Snapshots"
      state={
        !data
          ? isError
            ? "Couldn't load snapshots."
            : "Loading snapshots…"
          : n
            ? `${n} point-in-time ${n === 1 ? "copy" : "copies"}. Clone or restore one into a new volume.`
            : "No snapshots yet. Take one from the Volumes page."
      }
      actions={
        !n ? (
          <button type="button" className="at-btn" onClick={() => nav("/volumes")}>
            Volumes
          </button>
        ) : undefined
      }
    >
      {data && !data.length ? (
        <div className="at-panel">
          <EmptyState art="snapshot" title="No snapshots yet" description="A snapshot is a point-in-time copy of a volume.">
            <button type="button" className="at-btn primary" onClick={() => nav("/volumes")}>
              Open Volumes
            </button>
          </EmptyState>
        </div>
      ) : (
        <Table
          soundings
          panelTitle="All snapshots"
          panelExtra={
            picked.size > 0 ? (
              <>
                <span className="at-sub sn-bar-note">{picked.size} selected</span>
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
          error={isError}
          errorDetail={isError ? apiError(error) : undefined}
          onRetry={() => reload()}
          rowKey={(s) => s.id}
          selectable
          selected={picked}
          onToggle={toggle}
          onToggleAll={toggleAll}
          cols={[
            { h: "Name", f: (s) => s.name, mono: true, sortKey: (s) => s.name },
            { h: "Volume", f: (s) => s.volume_id, mono: true, sortKey: (s) => s.volume_id },
            { h: "State", f: (s) => <Badge kind={stateKind(s.state)} dot>{s.state}</Badge>, sortKey: (s) => s.state },
            { h: "Protected", f: (s) => (s.protected ? "yes" : "no") },
            {
              h: "Created",
              f: (s) => <span className="sn-dim">{timeAgo(s.created_at)}</span>,
              sortKey: (s) => s.created_at || "",
            },
          ]}
          actions={(s) => (
            <>
              <Button size="sm" onClick={() => setModal({ s, kind: "clone" })}>
                Clone
              </Button>
              <Button size="sm" onClick={() => setModal({ s, kind: "restore" })}>
                Restore
              </Button>
              <Button
                size="sm"
                variant="danger"
                onClick={() =>
                  del(`snapshot ${s.name}`, () =>
                    submitJob("delete", `/snapshots/${s.id}?force=true`, null, "delete snapshot", refetch),
                  )
                }
              >
                Delete
              </Button>
            </>
          )}
        />
      )}
      {modal && (
        <FormModal
          open
          onClose={() => setModal(null)}
          title={`${modal.kind === "clone" ? "Clone" : "Restore"} ${modal.s.name}`}
          submitLabel={modal.kind === "clone" ? "Clone" : "Restore"}
          fields={[
            { name: "name", label: "New volume name" },
            { name: "namespace", label: "Namespace", value: "rook-ceph" },
          ]}
          onSubmit={(v) =>
            submitJob(
              "post",
              `/snapshots/${modal.s.id}/${modal.kind}`,
              { name: v.name, namespace: v.namespace },
              modal.kind,
              refetch,
            )
          }
        />
      )}
    </ListPage>
  );
}
