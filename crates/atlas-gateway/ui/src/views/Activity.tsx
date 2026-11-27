// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { useState } from "react";
import { Activity as ActivityIcon, Camera, ClipboardList, Clock } from "lucide-react";
import { useEvents } from "../api/hooks";
import type { ActivityEvent } from "../api/types";
import { EmptyState } from "../graphics";
import { Badge } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { stateKind, timeAgo } from "../lib/format";
import "./Activity.css";

const KIND_ICON = { job: Clock, audit: ClipboardList, alert: Camera } as const;

export default function Activity() {
  const [kind, setKind] = useState<"" | ActivityEvent["kind"]>("");
  const { data } = useEvents(150);
  const rows = (data || []).filter((e) => !kind || e.kind === kind);
  const filters: Array<{ k: "" | ActivityEvent["kind"]; label: string }> = [
    { k: "", label: "All" },
    { k: "job", label: "Jobs" },
    { k: "audit", label: "Audit" },
    { k: "alert", label: "Alerts" },
  ];

  return (
    <ListPage
      crumbs={navCrumbs("activity")}
      eyebrow="Observability"
      title="Activity"
      state={
        data
          ? rows.length
            ? `${rows.length} event${rows.length === 1 ? "" : "s"} from jobs, audit and alerts.`
            : "No events match this filter."
          : "Loading jobs, audit and alerts…"
      }
    >
      <div className="at-chips">
        {filters.map((f) => (
          <button
            key={f.k || "all"}
            type="button"
            className={`at-chip${kind === f.k ? " on" : ""}`}
            aria-pressed={kind === f.k}
            onClick={() => setKind(f.k)}
          >
            {f.label}
          </button>
        ))}
      </div>
      <div className="at-panel">
        <div className="at-panel-bar">
          <h2 className="at-caption">Timeline</h2>
          <span className="grow" />
          <span className="act-count">{data ? `${rows.length} shown` : "…"}</span>
        </div>
        {!data ? (
          <div className="at-loading">Loading timeline…</div>
        ) : rows.length === 0 ? (
          <EmptyState
            art="chart"
            title="No activity yet"
            description="Jobs, audit events and alerts appear here as they happen."
          />
        ) : (
          <ul className="act-list">
            {rows.map((e) => {
              const Icon = KIND_ICON[e.kind] || ActivityIcon;
              return (
                <li key={`${e.kind}-${e.id}-${e.ts}`} className="act-row">
                  <Icon size={16} className="act-icon" aria-hidden />
                  <div className="act-body">
                    <div className="act-head">
                      <Badge kind={stateKind(e.severity)} dot>
                        {e.severity}
                      </Badge>
                      <span className="act-title">{e.title}</span>
                      <span className="mono act-meta">
                        {e.resource_type}/{e.resource_id}
                      </span>
                    </div>
                    <div className="act-detail truncate" title={e.detail || undefined}>
                      {e.detail}
                    </div>
                  </div>
                  <div className="act-when mono act-meta">
                    <div>{timeAgo(e.ts)}</div>
                    <div>
                      {e.kind} · {e.actor}
                    </div>
                  </div>
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </ListPage>
  );
}
