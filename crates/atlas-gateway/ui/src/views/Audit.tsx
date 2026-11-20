// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useState } from "react";
import { useAudit } from "../api/hooks";
import { Badge } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";
import { stateKind, timeAgo } from "../lib/format";

export default function Audit() {
  const [actor, setActor] = useState("");
  const [action, setAction] = useState("");
  const qs = new URLSearchParams({ limit: "80" });
  if (actor) qs.set("actor", actor);
  if (action) qs.set("action", action);
  const { data } = useAudit(`?${qs.toString()}`);
  const n = data?.length || 0;
  return (
    <ListPage
      crumbs={navCrumbs("audit")}
      eyebrow="GOVERNANCE · INDEX"
      title="Audit"
      state={
        data
          ? n
            ? `${n} entr${n === 1 ? "y" : "ies"} — compliance trail of state-changing and sensitive actions.`
            : "No audit entries match these filters."
          : "Loading compliance trail…"
      }
    >
      <div className="at-chips">
        <input
          className="at-chip-field"
          placeholder="Filter actor…"
          value={actor}
          onChange={(e) => setActor(e.target.value)}
        />
        <input
          className="at-chip-field"
          placeholder="Filter action…"
          value={action}
          onChange={(e) => setAction(e.target.value)}
          style={{ minWidth: 180 }}
        />
      </div>
      <Table
        soundings
        panelTitle="Audit trail"
        rows={data}
        rowKey={(a) => String(a.id)}
        empty="No audit entries."
        cols={[
          { h: "When", f: (a) => <span style={{ color: "var(--at-ink-4)" }}>{timeAgo(a.created_at)}</span> },
          { h: "Actor", f: (a) => a.actor_id, mono: true },
          { h: "Action", f: (a) => a.action, mono: true },
          { h: "Resource", f: (a) => <span className="mono">{a.resource_type}/{a.resource_id}</span> },
          { h: "Status", f: (a) => <Badge kind={stateKind(a.status)}>{a.status}</Badge> },
        ]}
      />
    </ListPage>
  );
}
