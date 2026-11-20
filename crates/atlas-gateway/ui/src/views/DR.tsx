// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Day-2 cross-cluster DR: mirroring peers, mirrored images, and failover (promote/demote).
import { useState, type CSSProperties } from "react";
import { submit, submitJob } from "../api/client";
import { useDrMirrors, useDrPeers, useDrPreflight, useDrStatus, useInvalidate, useVolumes } from "../api/hooks";
import { Badge, Button, Field, Label, Select } from "../ui/kit";
import { DashboardHero } from "../ui/templates/DashboardHero";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";
import { confirmThen } from "../ui/confirm";

export default function DR() {
  const inv = useInvalidate();
  const { data: status } = useDrStatus();
  const { data: peers } = useDrPeers();
  const { data: mirrors } = useDrMirrors();
  const { data: preflight } = useDrPreflight();
  const { data: volumes } = useVolumes(undefined, undefined, undefined, "block");
  const [peerName, setPeerName] = useState("");
  const [peerFsid, setPeerFsid] = useState("");
  const [peerSecret, setPeerSecret] = useState("");
  const [peerDir, setPeerDir] = useState("rx-tx");
  const [volId, setVolId] = useState("");
  const [mirrorPeer, setMirrorPeer] = useState("");
  const [mirrorMode, setMirrorMode] = useState("snapshot");
  const refresh = () => inv("dr-status", "dr-peers", "dr-mirrors", "dr-preflight");

  const blockVols = (volumes || []).filter((v) => v.state === "bound" || v.state === "ready" || !v.state);

  return (
    <DashboardHero
      className="at-stack"
      crumbs={navCrumbs("dr")}
      eyebrow="INFRASTRUCTURE · OPS"
      title="Disaster Recovery"
      state={
        status
          ? `${status.peers ?? 0} peer${(status.peers ?? 0) === 1 ? "" : "s"} · ${status.mirrors ?? 0} mirror${(status.mirrors ?? 0) === 1 ? "" : "s"} — control plane ${status.control_plane_ready ? "ready" : "incomplete"}; dataplane ${status.dataplane_verified || status.verified ? "verified" : "unverified (needs 2nd Ceph site)"}.`
          : "Cross-cluster RBD mirroring & failover — control plane ready; live mirror needs a peer cluster."
      }
    >

      <div className="at-instrs" style={{ "--instr-cols": 6 } as CSSProperties}>
        <div className="at-instr">
          <div className="at-caption">Peers</div>
          <div className="at-val md">{status?.peers ?? "—"}</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Mirrors</div>
          <div className="at-val md">{status?.mirrors ?? "—"}</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Primary</div>
          <div className="at-val md">{status?.primary ?? "—"}</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Secondary</div>
          <div className="at-val md">{status?.secondary ?? "—"}</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Worst RPO</div>
          <div className="at-val md mono">
            {status?.worst_rpo_seconds != null ? `${status.worst_rpo_seconds}s` : "—"}
          </div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Dataplane</div>
          <div className="at-val md" style={{ fontSize: 14 }}>
            <Badge kind={status?.dataplane_verified || status?.verified ? "success" : "warning"}>
              {status?.dataplane_verified || status?.verified ? "verified" : "unverified"}
            </Badge>
          </div>
        </div>
      </div>

      <div className="at-panel">
        <div className="at-panel-bar">
          <span className="at-caption">Preflight</span>
          <span className="grow" />
          <Badge kind={preflight?.ready ? "success" : "warning"}>
            {preflight?.ready ? "ready" : "blocked"}
          </Badge>
        </div>
        <div className="at-list-row">
          <span className="at-sub" style={{ margin: 0 }}>
            {status?.note || "Run before failover drills. Live rbd mirror image status needs a second cluster — see docs/DR.md."}
          </span>
        </div>
        {(preflight?.checks ?? []).map((c: any) => (
          <div key={c.id} className="at-list-row" style={{ alignItems: "center" }}>
            <Badge kind={c.ok ? "success" : "danger"}>{c.ok ? "ok" : "fail"}</Badge>
            <span className="mono" style={{ fontSize: 12 }}>{c.id}</span>
            <span className="at-sub" style={{ margin: 0, flex: 1 }}>{c.detail}</span>
          </div>
        ))}
        {(preflight?.warnings ?? []).map((w: string, i: number) => (
          <div key={`w-${i}`} className="at-list-row" style={{ alignItems: "center" }}>
            <Badge kind="warning">warn</Badge>
            <span className="at-sub" style={{ margin: 0, flex: 1 }}>{w}</span>
          </div>
        ))}
        {(preflight?.checks ?? []).length === 0 && (
          <div className="at-list-row">
            <span className="at-sub" style={{ margin: 0 }}>No preflight checks yet.</span>
          </div>
        )}
      </div>

      <Table
        soundings
        panelTitle="Mirroring peers"
        panelExtra={
          <div className="flex flex-wrap items-end gap-2">
            <div>
              <Label>Name</Label>
              <Field className="w-28" value={peerName} onChange={(e) => setPeerName(e.target.value)} placeholder="dc2" />
            </div>
            <div>
              <Label>Cluster FSID</Label>
              <Field className="w-40" value={peerFsid} onChange={(e) => setPeerFsid(e.target.value)} placeholder="optional" />
            </div>
            <div>
              <Label>Secret ref</Label>
              <Field className="w-36" value={peerSecret} onChange={(e) => setPeerSecret(e.target.value)} placeholder="k8s secret" />
            </div>
            <div>
              <Label>Direction</Label>
              <Select value={peerDir} onChange={(e) => setPeerDir(e.target.value)}>
                <option value="rx-tx">rx-tx</option>
                <option value="rx">rx</option>
                <option value="tx">tx</option>
              </Select>
            </div>
            <button
              type="button"
              className="at-btn primary"
              style={{ height: 32 }}
              disabled={!peerName.trim()}
              onClick={() =>
                submit(
                  "post",
                  "/dr/peers",
                  {
                    name: peerName.trim(),
                    cluster_fsid: peerFsid.trim() || undefined,
                    secret_ref: peerSecret.trim() || undefined,
                    direction: peerDir,
                  },
                  "peer registered",
                  () => {
                    setPeerName("");
                    setPeerFsid("");
                    setPeerSecret("");
                    refresh();
                  },
                ).catch(() => {})
              }
            >
              Register
            </button>
          </div>
        }
        rows={peers}
        rowKey={(p) => p.id}
        cols={[
          { h: "Name", f: (p) => p.name },
          { h: "Cluster FSID", f: (p) => p.cluster_fsid || "—", mono: true },
          { h: "Secret", f: (p) => p.bootstrap_secret_ref || p.secret_ref || "—", mono: true },
          { h: "Direction", f: (p) => <Badge kind="info">{p.direction}</Badge> },
          { h: "State", f: (p) => p.state, mono: true },
        ]}
        actions={(p) => (
          <Button size="sm" variant="danger" onClick={() => confirmThen({ title: "Delete peer?", message: p.name, confirmLabel: "Delete", danger: true }, () => submit("delete", `/dr/peers/${p.id}`, null, "peer deleted", refresh))}>Delete</Button>
        )}
      />

      <div className="at-panel">
        <div className="at-panel-bar">
          <span className="at-caption">Enable mirror</span>
        </div>
        <div className="at-form-grid" style={{ padding: "12px var(--page-inset) 16px" }}>
          <div>
            <Label>Block volume</Label>
            <Select value={volId} onChange={(e) => setVolId(e.target.value)}>
              <option value="">Select volume…</option>
              {blockVols.map((v) => (
                <option key={v.id} value={v.id}>
                  {v.name} ({v.id})
                </option>
              ))}
            </Select>
          </div>
          <div>
            <Label>Peer</Label>
            <Select value={mirrorPeer} onChange={(e) => setMirrorPeer(e.target.value)}>
              <option value="">Select peer…</option>
              {(peers || []).map((p: any) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </Select>
          </div>
          <div>
            <Label>Mode</Label>
            <Select value={mirrorMode} onChange={(e) => setMirrorMode(e.target.value)}>
              <option value="snapshot">snapshot</option>
              <option value="journal">journal</option>
            </Select>
          </div>
          <div style={{ display: "flex", alignItems: "flex-end" }}>
            <Button
              variant="primary"
              disabled={!volId || !mirrorPeer}
              onClick={() =>
                confirmThen(
                  {
                    title: "Enable RBD mirroring?",
                    message: `POST /volumes/${volId}/mirror?mode=${mirrorMode}&peer=${mirrorPeer}. On a single-site lab the job may fail honestly — dataplane needs a second cluster.`,
                    confirmLabel: "Enable",
                  },
                  () =>
                    submitJob(
                      "post",
                      `/volumes/${volId}/mirror?mode=${mirrorMode}&peer=${mirrorPeer}`,
                      null,
                      "enable mirror",
                      refresh,
                    ),
                )
              }
            >
              Enable mirroring
            </Button>
          </div>
        </div>
      </div>

      <Table
        soundings
        panelTitle={
          <>
            Mirrored images <Badge kind="neutral">{mirrors?.length ?? 0}</Badge>
          </>
        }
        rows={mirrors}
        rowKey={(m) => m.id}
        cols={[
          { h: "Image", f: (m) => `${m.pool}/${m.image}`, mono: true },
          { h: "Volume", f: (m) => m.volume_id || "—", mono: true },
          { h: "Role", f: (m) => <Badge kind={m.role === "primary" ? "success" : "info"} dot>{m.role}</Badge> },
          { h: "State", f: (m) => m.state, mono: true },
          { h: "Mode", f: (m) => m.mode },
          { h: "RPO", f: (m) => (m.rpo_seconds != null ? `${m.rpo_seconds}s` : "—") },
          { h: "Last failover", f: (m) => m.last_failover_at || "—" },
        ]}
        actions={(m) => (
          <div className="flex flex-wrap gap-1">
            {m.role === "secondary" ? (
              <>
                <Button size="sm" onClick={() => confirmThen({ title: "Fail over (promote)?", message: `Promote ${m.pool}/${m.image} to primary on this cluster.`, confirmLabel: "Promote", danger: true }, () => submitJob("post", `/dr/mirrors/${m.id}/promote`, null, "promote", refresh))}>Promote</Button>
                <Button size="sm" variant="secondary" onClick={() => confirmThen({ title: "Force promote (split-brain)?", message: `${m.pool}/${m.image}`, confirmLabel: "Force", danger: true }, () => submitJob("post", `/dr/mirrors/${m.id}/promote?force=true`, null, "force-promote", refresh))}>Force</Button>
                <Button size="sm" variant="danger" onClick={() => confirmThen({ title: "Confirm-gated failover?", message: `POST /dr/failover for ${m.pool}/${m.image}. Requires confirm=true.`, confirmLabel: "Failover", danger: true }, () => submitJob("post", "/dr/failover", { mirror_id: m.id, confirm: true }, "failover", refresh))}>Failover</Button>
                <Button size="sm" variant="secondary" onClick={() => confirmThen({ title: "Resync from peer?", message: `Discard this cluster's copy of ${m.pool}/${m.image} and pull a full copy from the peer's primary.`, confirmLabel: "Resync", danger: true }, () => submitJob("post", `/dr/mirrors/${m.id}/resync`, null, "resync", refresh))}>Resync</Button>
              </>
            ) : (
              <Button size="sm" variant="secondary" onClick={() => confirmThen({ title: "Demote to secondary?", message: `${m.pool}/${m.image}`, confirmLabel: "Demote" }, () => submitJob("post", `/dr/mirrors/${m.id}/demote`, null, "demote", refresh))}>Demote</Button>
            )}
            <Button size="sm" variant="secondary" onClick={() => {
              const raw = window.prompt("Observed RPO seconds", m.rpo_seconds != null ? String(m.rpo_seconds) : "60");
              if (raw == null || raw.trim() === "") return;
              const rpo_seconds = Number(raw);
              if (!Number.isFinite(rpo_seconds) || rpo_seconds < 0) return;
              submit("post", `/dr/mirrors/${m.id}/rpo`, { rpo_seconds }, "rpo recorded", refresh).catch(() => {});
            }}>Set RPO</Button>
            {m.volume_id && (
              <Button
                size="sm"
                variant="secondary"
                onClick={() =>
                  confirmThen(
                    {
                      title: "Disable mirroring?",
                      message: `DELETE /volumes/${m.volume_id}/mirror — stops RBD mirror for this volume.`,
                      confirmLabel: "Disable",
                      danger: true,
                    },
                    () => submitJob("delete", `/volumes/${m.volume_id}/mirror`, null, "disable mirror", refresh),
                  )
                }
              >
                Disable
              </Button>
            )}
          </div>
        )}
      />
    </DashboardHero>
  );
}
