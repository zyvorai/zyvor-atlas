// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { useNavigate } from "react-router-dom";
import { useCdcStreams } from "../../api/hooks";
import { Badge } from "../../ui/kit";
import { ListPage } from "../../ui/templates/ListPage";
import { navCrumbs } from "../../nav/routes";
import { Table } from "../../ui/Table";
import { fmtBytes, fmtSi } from "../../lib/format";
import { EmptyState } from "../../graphics";

const kind = (s: string) =>
  s === "streaming" ? "success" : s === "error" ? "danger" : s === "stopped" ? "neutral" : "warning";
const lagKind = (secs: number) => (secs <= 0 ? "success" : secs < 10 ? "warning" : "danger");

export default function Replication() {
  const nav = useNavigate();
  const { data, isError } = useCdcStreams();
  const n = data?.length || 0;
  const streaming = (data || []).filter((r) => r.state === "streaming").length;
  return (
    <ListPage
      crumbs={navCrumbs("replication")}
      eyebrow="Change data capture"
      title="Replication"
      state={
        !data
          ? isError
            ? "Could not load streams."
            : "Loading streams…"
          : n
            ? `${n} CDC stream${n === 1 ? "" : "s"}, ${streaming} streaming.`
            : "No streams yet. Start CDC from a migration plan."
      }
    >
      {data && !data.length ? (
        <div className="at-panel">
          <h2 className="sr-only">Streams</h2>
          <EmptyState art="chart" title="No CDC streams yet" description="Streams show how far each edge database trails its source.">
            <button type="button" className="at-btn primary" onClick={() => nav("/databridge/plans")}>
              Open migration plans
            </button>
          </EmptyState>
        </div>
      ) : (
        <Table
          soundings
          panelTitle="Streams"
          rows={data}
          rowKey={(r) => r.id}
          cols={[
            { h: "Connector", f: (r) => r.connector_name || r.id, mono: true },
            { h: "Engine", f: (r) => <Badge kind="info">{r.engine}</Badge> },
            { h: "State", f: (r) => <Badge kind={kind(r.state)} dot>{r.state}</Badge> },
            // Lag is only a measurement once the gateway has timestamped one.
            { h: "Lag (time)", f: (r) => (r.lag_updated_at ? <Badge kind={lagKind(r.lag_seconds)} className="normal-case">{r.lag_seconds}s</Badge> : "—") },
            { h: "Lag (bytes)", f: (r) => (r.lag_updated_at ? fmtBytes(r.lag_bytes) : "—") },
            { h: "Events", f: (r) => fmtSi(r.events_total) },
          ]}
        />
      )}
    </ListPage>
  );
}
