// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useNavigate } from "react-router-dom";
import { useCdcStreams } from "../../api/hooks";
import { Badge } from "../../ui/kit";
import { ListPage } from "../../ui/templates/ListPage";
import { navCrumbs } from "../../nav/routes";
import { Table } from "../../ui/Table";
import { fmtBytes, fmtSi } from "../../lib/format";

const kind = (s: string) =>
  s === "streaming" ? "success" : s === "error" ? "danger" : s === "stopped" ? "neutral" : "warning";
const lagKind = (secs: number) => (secs <= 0 ? "success" : secs < 10 ? "warning" : "danger");

export default function Replication() {
  const nav = useNavigate();
  const { data } = useCdcStreams();
  const n = data?.length || 0;
  const streaming = (data || []).filter((r) => r.state === "streaming").length;
  return (
    <ListPage
      crumbs={navCrumbs("replication")}
      eyebrow="DATABRIDGE · INDEX"
      title="Replication"
      state={
        n
          ? `${n} CDC stream${n === 1 ? "" : "s"} · ${streaming} streaming — Debezium lag from cloud sources.`
          : "No CDC streams — start CDC from a migration plan."
      }
    >
      <Table
        soundings
        panelTitle="CDC stream index"
        rows={data}
        rowKey={(r) => r.id}
        empty="No CDC streams — start CDC from a migration plan."
        emptyCta={
          <button type="button" className="at-btn primary" onClick={() => nav("/databridge/plans")}>
            Open Migration Plans
          </button>
        }
        cols={[
          { h: "Connector", f: (r) => r.connector_name || r.id, mono: true },
          { h: "Engine", f: (r) => <Badge kind="info">{r.engine}</Badge> },
          { h: "State", f: (r) => <Badge kind={kind(r.state)} dot>{r.state}</Badge> },
          { h: "Lag (time)", f: (r) => <Badge kind={lagKind(r.lag_seconds)}>{r.lag_seconds}s</Badge> },
          { h: "Lag (bytes)", f: (r) => fmtBytes(r.lag_bytes) },
          { h: "Events", f: (r) => fmtSi(r.events_total) },
        ]}
      />
    </ListPage>
  );
}
