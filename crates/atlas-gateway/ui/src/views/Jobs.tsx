// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { useState } from "react";
import { useJobs } from "../api/hooks";
import { useUi } from "../store/ui";
import { Badge, Copyable, SlideOver, TerminalPane, colorizeJson } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";
import { stateKind, timeAgo } from "../lib/format";
import "./Jobs.css";

function Bar({ pct, state, label }: { pct: number; state: string; label: string }) {
  const value = Math.max(0, Math.min(100, pct || 0));
  return (
    <div
      className="jobs-bar"
      data-state={state}
      role="progressbar"
      aria-label={`${label} progress`}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={value}
      title={`${value}%`}
    >
      <i style={{ width: `${value}%` }} />
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
      eyebrow="Observability"
      title="Jobs"
      state={
        live.length
          ? `${live.length} live this session · ${n} recent. Select a job to see its result.`
          : running
            ? `${running} in progress · ${n} recent.`
            : n
              ? `${n} recent job${n === 1 ? "" : "s"}. Select one to see its result.`
              : data
                ? "No jobs yet. Provisioning and day-2 operations appear here as they run."
                : "Loading jobs…"
      }
    >
      {live.length > 0 && (
        <div className="jobs-live">
          <Table
            soundings
            panelTitle="Live · this session"
            rows={live}
            rowKey={(j) => j.id}
            cols={[
              { h: "Job", f: (j) => j.label },
              { h: "State", f: (j) => <Badge kind={stateKind(j.state)} dot>{j.state}</Badge> },
              { h: "Progress", f: (j) => <Bar pct={j.progress} state={j.state} label={j.label} /> },
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
          { h: "Progress", f: (j) => <Bar pct={j.progress_percent} state={j.state} label={j.id} /> },
          { h: "Error", f: (j) => <span className="text-danger">{j.error || ""}</span> },
          { h: "When", f: (j) => <span className="text-muted-foreground">{timeAgo(j.updated_at || j.created_at)}</span>, sortKey: (j) => j.updated_at || j.created_at || "" },
        ]}
      />

      <SlideOver open={!!selId} onClose={() => setSelId(null)} title={<span className="mono">{sel?.id}</span>} width={520}>
        {sel && (
          <div className="at-stack text-sm">
            <div className="at-panel">
              <div className="at-panel-bar">
                <h2 className="at-caption">Detail</h2>
              </div>
              <dl className="jobs-detail">
                <Kv k="Type" v={sel.job_type} mono />
                <Kv k="State" v={sel.state} />
                <Kv k="By" v={sel.requested_by} mono />
                <Kv k="Progress" v={`${sel.progress_percent ?? 0}%`} />
                <Kv k="Created" v={timeAgo(sel.created_at)} />
                <Kv k="Updated" v={timeAgo(sel.updated_at)} />
              </dl>
            </div>
            {sel.error && (
              <TerminalPane title="Error" variant="error">
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
    <div>
      <dt>{k}</dt>
      <dd className={mono ? "mono" : undefined}>{v || "—"}</dd>
    </div>
  );
}
