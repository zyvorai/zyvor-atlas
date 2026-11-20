// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useState } from "react";
import { PlayCircle } from "lucide-react";
import { submit } from "../api/client";
import { useAlerts, useInvalidate } from "../api/hooks";
import { Badge, Button } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";
import { confirmThen } from "../ui/confirm";
import { stateKind, timeAgo } from "../lib/format";

export default function Alerts() {
  const [state, setState] = useState("");
  const { data } = useAlerts(state || undefined);
  const inv = useInvalidate();
  const open = (data || []).filter((a) => a.state === "open").length;
  const crit = (data || []).filter((a) => a.severity === "critical").length;
  return (
    <ListPage
      crumbs={navCrumbs("alerts")}
      eyebrow="OBSERVABILITY · LEDGER"
      title="Alerts"
      state={
        data
          ? crit
            ? `${crit} critical · ${open} open — acknowledge or silence before capacity work.`
            : open
              ? `${open} open alert${open === 1 ? "" : "s"} across health, capacity, and recovery.`
              : "No open alerts — estate quiet."
          : "Loading alert ledger…"
      }
      actions={
        <button
          type="button"
          className="at-btn"
          onClick={() => submit("post", "/alerts/evaluate", null, "evaluated", () => inv("alerts")).catch(() => {})}
        >
          <PlayCircle size={14} /> Evaluate
        </button>
      }
    >
      <div className="at-chips">
        {(["", "open", "resolved"] as const).map((s) => (
          <button key={s || "all"} type="button" className={`at-chip${state === s ? " on" : ""}`} onClick={() => setState(s)}>
            {s || "All"}
          </button>
        ))}
      </div>
      <Table
        soundings
        panelTitle="Alert ledger"
        rows={data}
        rowKey={(a) => a.id}
        empty="No alerts match this filter."
        cols={[
          { h: "Severity", f: (a) => <Badge kind={stateKind(a.severity)} dot>{a.severity}</Badge> },
          {
            h: "State", f: (a) => (
              <div className="flex flex-wrap gap-1">
                <Badge kind={a.state === "open" ? "warning" : "neutral"}>{a.state}</Badge>
                {a.acknowledged_at && <Badge kind="info">acked{a.acknowledged_by ? ` · ${a.acknowledged_by}` : ""}</Badge>}
                {a.silenced_until && new Date(a.silenced_until) > new Date() && <Badge kind="neutral">silenced {timeAgo(a.silenced_until)}</Badge>}
              </div>
            ),
          },
          { h: "Title", f: (a) => a.title },
          { h: "Detail", f: (a) => <span className="text-muted-foreground">{a.description}</span> },
          { h: "Resource", f: (a) => a.resource_id, mono: true },
          { h: "Since", f: (a) => <span className="text-muted-foreground">{timeAgo(a.created_at)}</span> },
        ]}
        actions={(a) => {
          const silenced = !!a.silenced_until && new Date(a.silenced_until) > new Date();
          return a.state === "open" ? (
            <>
              <Button size="sm" disabled={!!a.acknowledged_at} onClick={() => submit("post", `/alerts/${a.id}/ack`, null, "acknowledged", () => inv("alerts")).catch(() => {})}>{a.acknowledged_at ? "Acked" : "Ack"}</Button>
              <Button size="sm" disabled={silenced} onClick={() => confirmThen({ title: "Silence for 1 hour?", message: `${a.title} — suppresses webhook paging until it expires; the alert stays visible here.`, confirmLabel: "Silence" }, () => submit("post", `/alerts/${a.id}/silence?secs=3600`, null, "silenced 1h", () => inv("alerts")))}>{silenced ? "Silenced" : "Silence"}</Button>
              <Button size="sm" variant="danger" onClick={() => confirmThen({ title: "Resolve alert?", message: a.title, confirmLabel: "Resolve" }, () => submit("post", `/alerts/${a.id}/resolve`, null, "resolved", () => inv("alerts")))}>Resolve</Button>
            </>
          ) : null;
        }}
      />
    </ListPage>
  );
}
