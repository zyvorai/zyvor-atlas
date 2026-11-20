// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useEffect } from "react";
import { useParams } from "react-router-dom";
import { Check, Circle, Loader2 } from "lucide-react";
import { submitJob } from "../../api/client";
import { usePlan, useSource, useInvalidate } from "../../api/hooks";
import { Badge, Button } from "../../ui/kit";
import { DetailPage } from "../../ui/templates/DetailPage";
import { SwipeRail } from "../../ui/SwipeRail";
import { confirmThen } from "../../ui/confirm";
import { planStateKind, planStateLabel } from "./Plans";
import { stageBadge, verificationFor } from "../../lib/engineVerification";

// Pipeline stages in order. `state` values from migration_plans map to how far we've progressed.
const STAGES = ["discover", "assess", "provision", "full-load", "cdc", "validate", "cutover"] as const;
const STAGE_LABEL: Record<string, string> = {
  discover: "Discover", assess: "Assess", provision: "Provision edge",
  "full-load": "Full-load", cdc: "CDC replication", validate: "Validate", cutover: "Cutover",
};
// migration_plans.state -> index of the last COMPLETED stage.
const COMPLETED_THROUGH: Record<string, number> = {
  draft: -1, discovered: 0, assessed: 1, provisioning: 1, provisioned: 2,
  full_loading: 2, loaded: 3, cdc_streaming: 4, validating: 4, validated: 5,
  cutover_pending: 5, cutover_in_progress: 5, cutover_complete: 6, completed: 6,
  rolled_back: 6, failed: -1,
};
// All pipeline stages are wired (fake pipeline runs end-to-end).
const IMPLEMENTED = new Set(STAGES as readonly string[]);

export default function PlanDetail() {
  const { id = "" } = useParams();
  const { data: plan } = usePlan(id);
  const { data: source } = useSource(plan?.source_id || "");
  const inv = useInvalidate();
  const refresh = () => inv("db-plan", "db-plans", "db-source", "db-sources");

  useEffect(() => {
    if (plan) document.title = `Atlas · ${plan.name}`;
  }, [plan]);

  if (!plan) return <div className="p-6 text-muted-foreground">Loading plan…</div>;
  const done = COMPLETED_THROUGH[plan.state] ?? -1;
  const current = done + 1; // the next actionable stage index
  const a = plan.assessment as any;
  const rollbackDeadline = plan.cutover_at ? new Date(Date.parse(plan.cutover_at) + plan.rollback_window_secs * 1000) : null;
  const engineKind = source?.kind || "";
  const engVer = verificationFor(String(engineKind));
  const cdcLive = stageBadge(engVer, "cdc") === "live";
  const cutoverLive = stageBadge(engVer, "cutover") === "live";

  const act = (stage: string) => {
    const P = `/databridge/plans/${plan.id}`;
    if (stage === "discover" && source) return submitJob("post", `/databridge/sources/${source.id}/discover`, null, "discover source", refresh);
    if (stage === "assess") return submitJob("post", `${P}/assess`, null, "assess plan", refresh);
    if (stage === "provision") return submitJob("post", `${P}/provision`, null, "provision edge", refresh);
    if (stage === "full-load") return submitJob("post", `${P}/full-load`, null, "full-load", refresh);
    if (stage === "cdc") return submitJob("post", `${P}/cdc/start`, null, "start CDC", refresh);
    if (stage === "validate") return submitJob("post", `${P}/validate`, null, "validate", refresh);
    if (stage === "cutover") return submitJob("post", `${P}/cutover`, null, "cutover", refresh);
  };

  return (
    <DetailPage
      className="at-stack"
      crumbs={[
        { label: "DataBridge", to: "/databridge/sources" },
        { label: "Migration Plans", to: "/databridge/plans" },
        { label: plan.name },
      ]}
      eyebrow="DATABRIDGE · DETAIL"
      title={plan.name}
      state={`Plan ${plan.id} · source ${source?.name || plan.source_id} · ${planStateLabel(plan.state)}${engVer ? ` · ${engVer.engine}` : ""}`}
      actions={<div className="flex gap-2 items-center">
        {engVer && (
          <Badge kind={cdcLive && cutoverLive ? "success" : "warning"} title={engVer.note}>
            CDC/cutover {cdcLive && cutoverLive ? "live" : "pending infra"}
          </Badge>
        )}
        {plan.state === "cdc_streaming" && (
          <>
            <button type="button" className="at-btn" onClick={() => submitJob("post", `/databridge/plans/${plan.id}/cdc/stop`, null, "stop CDC", refresh).catch(() => {})}>Stop CDC</button>
            <button type="button" className="at-btn" onClick={() => submitJob("post", `/databridge/plans/${plan.id}/cdc/restart`, null, "restart CDC", refresh).catch(() => {})}>Restart CDC</button>
          </>
        )}
        {plan.state === "cutover_complete" && (
          <>
            {rollbackDeadline && <span className="at-sub" style={{ margin: 0 }}>Rollback available until {rollbackDeadline.toLocaleString()}</span>}
            <button type="button" className="at-btn" style={{ color: "var(--at-fail)", borderColor: "rgba(255, 90, 110, 0.35)" }} onClick={() => confirmThen({
              title: "Roll back this migration?",
              message: rollbackDeadline
                ? `Reverts cutover and switches traffic back to the source database. Only available until ${rollbackDeadline.toLocaleString()}. This cannot be undone.`
                : "Reverts cutover and switches traffic back to the source database. This cannot be undone.",
              confirmLabel: "Roll back",
              danger: true,
            }, () => submitJob("post", `/databridge/plans/${plan.id}/rollback`, null, "rollback", refresh))}>Rollback</button>
          </>
        )}
        <Badge kind={planStateKind(plan.state)} dot>{planStateLabel(plan.state)}</Badge>
      </div>}
    >

      <div className="at-panel">
        <div className="at-panel-bar">
          <span className="at-caption">Pipeline</span>
          <span className="grow" />
          <span className="at-sub" style={{ margin: 0 }}>
            {Math.max(0, done + 1)} / {STAGES.length} stages · swipe
          </span>
        </div>
        <div style={{ padding: "12px 16px 16px" }}>
          <SwipeRail label="Migration stages">
            {STAGES.map((stage, i) => {
              const status = i <= done ? "done" : i === current ? "current" : "upcoming";
              const Icon = status === "done" ? Check : status === "current" ? Loader2 : Circle;
              const actionable = status === "current" && IMPLEMENTED.has(stage);
              const tickColor =
                status === "done" ? "var(--at-cyan)" : status === "current" ? "var(--at-cyan-2)" : "var(--at-ink-4)";
              return (
                <div key={stage} className="at-instr" style={{ minHeight: 140 }}>
                  <div className="at-caption" style={{ display: "flex", alignItems: "center", gap: 8 }}>
                    <span style={{ color: tickColor, display: "flex" }}>
                      <Icon size={16} className={status === "current" ? "animate-spin" : undefined} />
                    </span>
                    <span className="mono">{String(i + 1).padStart(2, "0")}</span>
                  </div>
                  <div
                    className="at-val md"
                    style={{
                      fontSize: 18,
                      color: status === "upcoming" ? "var(--at-ink-4)" : "var(--at-ink)",
                    }}
                  >
                    {STAGE_LABEL[stage]}
                  </div>
                  <div className="at-delta" style={{ display: "flex", flexWrap: "wrap", gap: 8, marginTop: 8 }}>
                    {status === "done" && <Badge kind="success">done</Badge>}
                    {status === "current" && <Badge kind="info">current</Badge>}
                    {(stage === "cdc" || stage === "cutover") && engVer && (
                      <Badge kind={stageBadge(engVer, stage) === "live" ? "success" : "warning"}>
                        {stageBadge(engVer, stage) === "live" ? "live verified" : "needs Kafka"}
                      </Badge>
                    )}
                  </div>
                  {actionable && stage === "cutover" && (
                    <div style={{ marginTop: 12 }}>
                      <Button size="sm" variant="danger" onClick={() => confirmThen({
                        title: "Run cutover?",
                        message: (a?.blockers?.length
                          ? `Assessment still lists ${a.blockers.length} blocker(s) (see below) — cutover will proceed anyway. `
                          : "") + "Switches live traffic to the edge database. This is the point of no return short of a rollback within the window.",
                        confirmLabel: "Run cutover",
                        danger: true,
                      }, () => act(stage)?.catch(() => {}))}>Run</Button>
                    </div>
                  )}
                  {actionable && stage !== "cutover" && (
                    <div style={{ marginTop: 12 }}>
                      <Button size="sm" variant="primary" onClick={() => act(stage)?.catch(() => {})}>Run</Button>
                    </div>
                  )}
                </div>
              );
            })}
          </SwipeRail>
        </div>
      </div>

      {a && typeof a === "object" && "score" in a && (
        <div className="at-panel">
          <div className="at-panel-bar">
            <span className="at-caption">Assessment</span>
            <span className="grow" />
            <Badge kind={a.risk === "low" ? "success" : a.risk === "medium" ? "warning" : "danger"}>
              {a.risk} risk
            </Badge>
          </div>
          <div className="at-list-row" style={{ alignItems: "center", gap: 24 }}>
            <div>
              <div className="at-val lg mono">
                {a.score}
                <span style={{ fontSize: 14, color: "var(--at-ink-3)" }}>/100</span>
              </div>
            </div>
            <div className="at-sub" style={{ margin: 0 }}>
              Est. downtime: <span style={{ color: "var(--at-ink)", fontWeight: 500 }}>{a.downtime_estimate}</span>
              {" · "}
              {a.table_count} tables
            </div>
          </div>
          {!!a.blockers?.length && (
            <>
              <div className="at-list-row" style={{ paddingBottom: 4 }}>
                <span className="at-caption" style={{ color: "var(--at-fail)" }}>Blockers</span>
              </div>
              {a.blockers.map((b: string, k: number) => (
                <div key={k} className="at-list-row" style={{ color: "var(--at-fail)", paddingTop: 8, paddingBottom: 8 }}>
                  <span style={{ flex: 1 }}>{b}</span>
                </div>
              ))}
            </>
          )}
          {!!a.warnings?.length && (
            <>
              <div className="at-list-row" style={{ paddingBottom: 4 }}>
                <span className="at-caption" style={{ color: "var(--at-warn)" }}>Warnings</span>
              </div>
              {a.warnings.map((w: string, k: number) => (
                <div key={k} className="at-list-row" style={{ color: "var(--at-warn)", paddingTop: 8, paddingBottom: 8 }}>
                  <span style={{ flex: 1 }}>{w}</span>
                </div>
              ))}
            </>
          )}
          {!a.blockers?.length && !a.warnings?.length && (
            <div className="at-list-row" style={{ color: "var(--at-ok)" }}>
              No blockers or warnings — ready to provision.
            </div>
          )}
        </div>
      )}
    </DetailPage>
  );
}
