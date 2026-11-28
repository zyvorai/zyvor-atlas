// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Day-2 cross-cluster DR: mirroring peers, mirrored images, and failover (promote/demote).
import { useState, type CSSProperties } from "react";
import { submit, submitJob } from "../api/client";
import { useDrMirrors, useDrPeers, useDrPreflight, useDrStatus, useInvalidate, useVolumes } from "../api/hooks";
import { Badge, Button, Field, Label, Select } from "../ui/kit";
import { DashboardHero } from "../ui/templates/DashboardHero";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";
import { confirmThen } from "../ui/confirm";
import "./DR.css";

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
  const verified = !!(status?.dataplane_verified || status?.verified);

  return (
    <DashboardHero
      className="at-stack"
      crumbs={navCrumbs("dr")}
      eyebrow="Infrastructure"
      title="Disaster Recovery"
      state={
        status
          ? `${status.peers ?? 0} peer${(status.peers ?? 0) === 1 ? "" : "s"} · ${status.mirrors ?? 0} mirror${(status.mirrors ?? 0) === 1 ? "" : "s"}. Control plane ${status.control_plane_ready ? "ready" : "incomplete"}, data plane ${verified ? "verified" : "unverified (needs a second Ceph site)"}.`
          : "Cross-cluster RBD mirroring and failover. A live mirror needs a peer cluster."
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
          <div className="at-val md">
            {status?.worst_rpo_seconds != null ? `${status.worst_rpo_seconds}s` : "—"}
          </div>
        </div>
        <div className="at-instr dr-plane">
          <div className="at-caption">Data plane</div>
          <div className="at-val md">
            {status ? (
              <Badge kind={verified ? "success" : "warning"}>{verified ? "verified" : "unverified"}</Badge>
            ) : (
              "—"
            )}
          </div>
        </div>
      </div>

      <div className="at-panel">
        <div className="at-panel-bar">
          <h2 className="at-caption">Preflight</h2>
          <span className="grow" />
          {preflight && (
            <Badge kind={preflight.ready ? "success" : "warning"}>{preflight.ready ? "ready" : "blocked"}</Badge>
          )}
        </div>
        <div className="at-list-row">
          <span className="dr-note">
            {status?.note || "Run before failover drills. Live rbd mirror image status needs a second cluster (docs/DR.md)."}
          </span>
        </div>
        {(preflight?.checks ?? []).map((c: any) => (
          <div key={c.id} className="at-list-row dr-check">
            <Badge kind={c.ok ? "success" : "danger"}>{c.ok ? "ok" : "fail"}</Badge>
            <span className="mono dr-id">{c.id}</span>
            <span className="dr-note dr-grow">{c.detail}</span>
          </div>
        ))}
        {(preflight?.warnings ?? []).map((w: string, i: number) => (
          <div key={`w-${i}`} className="at-list-row dr-check">
            <Badge kind="warning">warn</Badge>
            <span className="dr-note dr-grow">{w}</span>
          </div>
        ))}
        {(preflight?.checks ?? []).length === 0 && (
          <div className="at-list-row">
            <span className="dr-note">No preflight checks yet.</span>
          </div>
        )}
      </div>

      <Table
        soundings
        panelTitle="Mirroring peers"
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
          <h2 className="at-caption">Register peer</h2>
        </div>
        <div className="dr-form">
          <div>
            <Label htmlFor="dr-peer-name">Name</Label>
            <Field id="dr-peer-name" value={peerName} onChange={(e) => setPeerName(e.target.value)} placeholder="dc2" />
          </div>
          <div>
            <Label htmlFor="dr-peer-fsid">Cluster FSID</Label>
            <Field id="dr-peer-fsid" value={peerFsid} onChange={(e) => setPeerFsid(e.target.value)} placeholder="optional" />
          </div>
          <div>
            <Label htmlFor="dr-peer-secret">Secret ref</Label>
            <Field id="dr-peer-secret" value={peerSecret} onChange={(e) => setPeerSecret(e.target.value)} placeholder="k8s secret" />
          </div>
          <div>
            <Label htmlFor="dr-peer-direction">Direction</Label>
            <Select id="dr-peer-direction" value={peerDir} onChange={(e) => setPeerDir(e.target.value)}>
              <option value="rx-tx">rx-tx</option>
              <option value="rx">rx</option>
              <option value="tx">tx</option>
            </Select>
          </div>
          <button
            type="button"
            className="at-btn primary dr-submit"
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
      </div>

      <div className="at-panel">
        <div className="at-panel-bar">
          <h2 className="at-caption">Enable mirror</h2>
        </div>
        <div className="dr-form">
          <div>
            <Label htmlFor="dr-mirror-volume">Block volume</Label>
            <Select id="dr-mirror-volume" value={volId} onChange={(e) => setVolId(e.target.value)}>
              <option value="">Select volume…</option>
              {blockVols.map((v) => (
                <option key={v.id} value={v.id}>
                  {v.name} ({v.id})
                </option>
              ))}
            </Select>
          </div>
          <div>
            <Label htmlFor="dr-mirror-peer">Peer</Label>
            <Select id="dr-mirror-peer" value={mirrorPeer} onChange={(e) => setMirrorPeer(e.target.value)}>
              <option value="">Select peer…</option>
              {(peers || []).map((p: any) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </Select>
          </div>
          <div>
            <Label htmlFor="dr-mirror-mode">Mode</Label>
            <Select id="dr-mirror-mode" value={mirrorMode} onChange={(e) => setMirrorMode(e.target.value)}>
              <option value="snapshot">snapshot</option>
              <option value="journal">journal</option>
            </Select>
          </div>
          <div className="dr-submit">
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
            Mirrored images {mirrors && <Badge kind="neutral">{mirrors.length}</Badge>}
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
          <div className="dr-actions">
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
