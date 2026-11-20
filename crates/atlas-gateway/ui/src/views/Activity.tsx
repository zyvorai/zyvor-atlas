// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useState } from "react";
import { Activity as ActivityIcon, Camera, ClipboardList, Clock } from "lucide-react";
import { useEvents } from "../api/hooks";
import type { ActivityEvent } from "../api/types";
import { Badge } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { stateKind, timeAgo } from "../lib/format";

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
      eyebrow="OBSERVABILITY · INDEX"
      title="Activity"
      state={
        data
          ? rows.length
            ? `${rows.length} event${rows.length === 1 ? "" : "s"} in the unified timeline.`
            : "No events match this filter."
          : "Loading unified timeline of jobs, audit, and alerts…"
      }
    >
      <div className="at-chips">
        {filters.map((f) => (
          <button
            key={f.k || "all"}
            type="button"
            className={`at-chip${kind === f.k ? " on" : ""}`}
            onClick={() => setKind(f.k)}
          >
            {f.label}
          </button>
        ))}
      </div>
      <div className="at-panel">
        <div className="at-panel-bar">
          <span className="at-caption">Timeline</span>
          <span className="grow" />
          <span className="at-sub" style={{ margin: 0 }}>
            {data ? `${rows.length} shown` : "…"}
          </span>
        </div>
        {!data ? (
          <div className="at-loading">Loading timeline…</div>
        ) : rows.length === 0 ? (
          <div className="at-empty-box" style={{ margin: 16, boxShadow: "none" }}>
            <div className="at-empty-title">No activity yet</div>
            <p className="at-empty-copy">Jobs, audit events, and alerts will appear here as they happen.</p>
          </div>
        ) : (
          <ul style={{ listStyle: "none", margin: 0, padding: 0 }}>
            {rows.map((e) => {
              const Icon = KIND_ICON[e.kind] || ActivityIcon;
              return (
                <li
                  key={`${e.kind}-${e.id}-${e.ts}`}
                  style={{
                    display: "flex",
                    alignItems: "flex-start",
                    gap: 12,
                    padding: "12px 16px",
                    borderBottom: "1px solid var(--at-line)",
                  }}
                >
                  <Icon size={16} style={{ marginTop: 3, color: "var(--at-ink-4)", flexShrink: 0 }} />
                  <div style={{ minWidth: 0, flex: 1 }}>
                    <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
                      <Badge kind={stateKind(e.severity)} dot>
                        {e.severity}
                      </Badge>
                      <span style={{ fontWeight: 500, color: "var(--at-ink)" }}>{e.title}</span>
                      <span className="mono" style={{ fontSize: 11, color: "var(--at-ink-4)" }}>
                        {e.resource_type}/{e.resource_id}
                      </span>
                    </div>
                    <div style={{ fontSize: 12, color: "var(--at-ink-3)", marginTop: 4 }} className="truncate">
                      {e.detail}
                    </div>
                  </div>
                  <div style={{ textAlign: "right", flexShrink: 0 }}>
                    <div className="mono" style={{ fontSize: 11, color: "var(--at-ink-4)" }}>
                      {timeAgo(e.ts)}
                    </div>
                    <div className="mono" style={{ fontSize: 10, color: "var(--at-ink-4)", opacity: 0.7 }}>
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
