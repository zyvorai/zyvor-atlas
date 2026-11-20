// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Day-2 maintenance: upgrade pre-flight, job-engine pause, backend cordon, orphan backups.
import { AlertTriangle, CheckCircle2, PauseCircle, PlayCircle } from "lucide-react";
import { submit } from "../api/client";
import { useBackends, useInvalidate, useMaintenance, useOrphans, usePreflight } from "../api/hooks";
import { Badge, Button } from "../ui/kit";
import { DashboardHero } from "../ui/templates/DashboardHero";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";
import { confirmThen } from "../ui/confirm";
import { timeAgo } from "../lib/format";

export default function Maintenance() {
  const inv = useInvalidate();
  const { data: pre } = usePreflight();
  const { data: maint } = useMaintenance();
  const { data: backends } = useBackends();
  const { data: orphans } = useOrphans();
  const paused = !!maint?.paused;

  return (
    <DashboardHero
      className="at-stack"
      crumbs={navCrumbs("maintenance")}
      eyebrow="INFRASTRUCTURE · OPS"
      title="Maintenance"
      state={
        pre
          ? pre.ready
            ? paused
              ? "Upgrade ready · job engine paused — resume when maintenance completes."
              : `Upgrade ready · job engine running · ${orphans?.count ?? 0} orphan backup${(orphans?.count ?? 0) === 1 ? "" : "s"}.`
            : `Upgrade blocked — ${pre.blockers?.length || 0} pre-flight issue${(pre.blockers?.length || 0) === 1 ? "" : "s"}.`
          : "Upgrade readiness, job-engine pause, backend cordon, and orphan cleanup."
      }
    >

      <div className="at-instrs">
        <div className="at-instr">
          <div className="at-caption">Pre-flight</div>
          <div className="at-val md" style={{ textTransform: "uppercase" }}>
            {pre ? (pre.ready ? "ready" : "blocked") : "—"}
          </div>
          <div className="at-delta mono">{pre?.checks?.length ?? 0} checks</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Job engine</div>
          <div className="at-val md" style={{ textTransform: "uppercase" }}>
            {maint ? (paused ? "paused" : "running") : "—"}
          </div>
          <div className="at-delta">{paused ? "new jobs queue" : "dispatching"}</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Backends</div>
          <div className="at-val md">{backends?.length ?? "—"}</div>
          <div className="at-delta mono">
            {(backends || []).filter((b) => b.cordoned).length} cordoned
          </div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Orphans</div>
          <div className="at-val md">{orphans?.count ?? "—"}</div>
          <div className="at-delta">backup GC</div>
        </div>
      </div>

      <div className="at-panel">
        <div className="at-panel-bar">
          <span className="at-caption">Upgrade pre-flight</span>
          <span className="grow" />
          {pre && (
            <Badge kind={pre.ready ? "success" : "warning"} dot>
              {pre.ready ? "ready" : "blocked"}
            </Badge>
          )}
        </div>
        {pre?.blockers?.length ? (
          pre.blockers.map((b, i) => (
            <div key={`b-${i}`} className="at-list-row" style={{ color: "var(--at-warn)" }}>
              <AlertTriangle size={16} style={{ flexShrink: 0, marginTop: 2 }} />
              <span style={{ flex: 1 }}>{b}</span>
            </div>
          ))
        ) : (
          <div className="at-list-row" style={{ color: "var(--at-ok)" }}>
            <CheckCircle2 size={16} style={{ flexShrink: 0 }} />
            <span>Safe to upgrade.</span>
          </div>
        )}
        {(pre?.checks ?? []).map((c) => (
          <div key={c.check} className="at-list-row" style={{ alignItems: "center" }}>
            <Badge kind={c.ok ? "success" : "warning"} dot>
              {c.ok ? "ok" : "blocked"}
            </Badge>
            <span className="mono" style={{ fontSize: 12.5, color: "var(--at-ink)" }}>{c.check}</span>
            <span className="at-sub" style={{ margin: 0, flex: 1 }}>{c.detail}</span>
          </div>
        ))}
        {!pre && (
          <div className="at-list-row">
            <span className="at-sub" style={{ margin: 0 }}>Loading pre-flight…</span>
          </div>
        )}
      </div>

      <div className="at-panel">
        <div className="at-panel-bar">
          <span className="at-caption">Job engine</span>
          {maint && (
            <Badge kind={paused ? "warning" : "success"} dot>
              {paused ? "paused" : "running"}
            </Badge>
          )}
          <span className="grow" />
          {paused ? (
            <button
              type="button"
              className="at-btn primary compact"
              onClick={() => submit("post", "/maintenance", { paused: false }, "resumed", () => inv("maintenance")).catch(() => {})}
            >
              <PlayCircle size={14} /> Resume
            </button>
          ) : (
            <button
              type="button"
              className="at-btn compact"
              onClick={() => confirmThen({ title: "Pause the job engine?", message: "New jobs will queue instead of running until you resume — quiesce the system only if you mean to.", confirmLabel: "Pause" }, () => submit("post", "/maintenance", { paused: true }, "paused", () => inv("maintenance")))}
            >
              <PauseCircle size={14} /> Pause
            </button>
          )}
        </div>
        <div className="at-list-row">
          <p className="at-sub" style={{ margin: 0 }}>
            While paused, new jobs stay <span className="mono">queued</span> and drain when resumed — quiesce the system before maintenance.
          </p>
        </div>
      </div>

      <Table
        soundings
        panelTitle="Backends"
        rows={backends}
        rowKey={(b) => b.id}
        cols={[
          { h: "Name", f: (b) => b.name },
          { h: "Type", f: (b) => <Badge kind="info">{b.backend_type}</Badge> },
          { h: "Status", f: (b) => b.status, mono: true },
          { h: "Provisioning", f: (b) => <Badge kind={b.cordoned ? "warning" : "success"} dot>{b.cordoned ? "cordoned" : "open"}</Badge> },
        ]}
        actions={(b) =>
          b.cordoned ? (
            <Button size="sm" onClick={() => submit("post", `/backends/${b.id}/uncordon`, null, "uncordoned", () => inv("backends")).catch(() => {})}>Uncordon</Button>
          ) : (
            <Button size="sm" variant="secondary" onClick={() => confirmThen({ title: "Cordon backend?", message: `${b.name} will reject new provisioning (existing volumes untouched).`, confirmLabel: "Cordon" }, () => submit("post", `/backends/${b.id}/cordon`, null, "cordoned", () => inv("backends")))}>Cordon</Button>
          )
        }
      />

      <Table
        soundings
        panelTitle={
          <>
            Orphan backups{" "}
            <Badge kind={orphans?.count ? "warning" : "neutral"}>{orphans?.count ?? 0}</Badge>
          </>
        }
        rows={orphans?.orphan_backups}
        rowKey={(b) => b.id}
        cols={[
          { h: "Backup", f: (b) => b.id, mono: true },
          { h: "Source volume (gone)", f: (b) => b.volume_id, mono: true },
          { h: "Object key", f: (b) => <span className="text-muted-foreground">{b.object_key}</span> },
          { h: "Since", f: (b) => <span className="text-muted-foreground">{timeAgo(b.created_at)}</span> },
        ]}
        actions={(b) => (
          <Button size="sm" variant="danger" onClick={() => confirmThen({ title: "Delete orphan backup?", message: b.id, confirmLabel: "Delete", danger: true }, () => submit("delete", `/backups/${b.id}`, null, "backup deleted", () => inv("orphans")))}>Delete</Button>
        )}
      />
    </DashboardHero>
  );
}
