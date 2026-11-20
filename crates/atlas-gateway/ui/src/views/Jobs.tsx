// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useState } from "react";
import { useJobs } from "../api/hooks";
import { useUi } from "../store/ui";
import { Badge, Copyable, SlideOver, TerminalPane, colorizeJson } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";
import { stateKind, timeAgo } from "../lib/format";

function Bar({ pct, kind }: { pct: number; kind: string }) {
  const c = kind === "failed" ? "#E23B3B" : kind === "succeeded" ? "#30D69E" : "#38BDF8";
  return (
    <div className="w-28 h-1.5 rounded-full overflow-hidden" style={{ background: "var(--at-line)" }}>
      <div className="h-full rounded-full" style={{ width: `${Math.min(100, pct || 0)}%`, background: c }} />
    </div>
  );
}

export default function Jobs() {
  const { data } = useJobs();
  const live = Object.values(useUi((s) => s.jobs));
  const [selId, setSelId] = useState<string | null>(null);
  const sel = (data || []).find((j) => j.id === selId) || null;
  const n = data?.length || 0;
  const running = (data || []).filter((j) => j.state === "running" || j.state === "queued").length;
  return (
    <ListPage
      crumbs={navCrumbs("jobs")}
      eyebrow="OBSERVE · TIMELINE"
      title="Jobs"
      state={
        live.length
          ? `${live.length} live this session · ${n} recent — open a row for Terminal result.`
          : running
            ? `${running} in flight · ${n} recent on the job engine.`
            : n
              ? `${n} recent job${n === 1 ? "" : "s"} — click a row for Terminal detail.`
              : "No jobs yet. Provisioning and day-2 ops appear here as they run."
      }
    >
      {live.length > 0 && (
        <div style={{ marginBottom: 16 }}>
          <Table
            soundings
            panelTitle="Live · this session"
            rows={live}
            rowKey={(j) => j.id}
            cols={[
              { h: "Job", f: (j) => j.label },
              { h: "State", f: (j) => <Badge kind={stateKind(j.state)} dot>{j.state}</Badge> },
              { h: "Progress", f: (j) => <Bar pct={j.progress} kind={j.state} /> },
              { h: "ID", f: (j) => j.id, mono: true },
            ]}
          />
        </div>
      )}
      <Table
        soundings
        panelTitle="Recent jobs"
        rows={data}
        rowKey={(j) => j.id}
        onRow={(j) => setSelId(j.id)}
        empty="No jobs recorded yet."
        cols={[
          { h: "ID", f: (j) => <Copyable text={j.id} />, mono: true },
          { h: "Type", f: (j) => j.job_type, mono: true, sortKey: (j) => j.job_type },
          { h: "State", f: (j) => <Badge kind={stateKind(j.state)} dot>{j.state}</Badge>, sortKey: (j) => j.state },
          { h: "Progress", f: (j) => <Bar pct={j.progress_percent} kind={j.state} /> },
          { h: "Error", f: (j) => <span className="text-danger">{j.error || ""}</span> },
          { h: "When", f: (j) => <span className="text-muted-foreground">{timeAgo(j.updated_at || j.created_at)}</span>, sortKey: (j) => j.updated_at || j.created_at || "" },
        ]}
      />

      <SlideOver open={!!selId} onClose={() => setSelId(null)} title={<span className="mono">{sel?.id}</span>} width={520}>
        {sel && (
          <div className="at-stack text-sm">
            <div className="at-panel">
              <div className="at-panel-bar">
                <span className="at-caption">Detail</span>
              </div>
              <div className="grid grid-cols-2 gap-px" style={{ background: "var(--at-line)" }}>
                <Kv k="Type" v={sel.job_type} mono />
                <Kv k="State" v={sel.state} />
                <Kv k="By" v={sel.requested_by} mono />
                <Kv k="Progress" v={`${sel.progress_percent ?? 0}%`} />
                <Kv k="Created" v={timeAgo(sel.created_at)} />
                <Kv k="Updated" v={timeAgo(sel.updated_at)} />
              </div>
            </div>
            {sel.error && (
              <TerminalPane title="stderr" variant="error">
                {sel.error}
              </TerminalPane>
            )}
            <TerminalPane title="result.json">
              {sel.result ? colorizeJson(JSON.stringify(sel.result, null, 2)) : "—"}
            </TerminalPane>
          </div>
        )}
      </SlideOver>
    </ListPage>
  );
}

function Kv({ k, v, mono }: { k: string; v?: string; mono?: boolean }) {
  return (
    <div style={{ padding: "10px 14px", background: "var(--at-shelf)" }}>
      <div className="at-caption">{k}</div>
      <div className={mono ? "mono" : ""} style={{ marginTop: 4, color: "var(--at-ink)" }}>{v || "—"}</div>
    </div>
  );
}
