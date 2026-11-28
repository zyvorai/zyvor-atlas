// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { useEffect } from "react";
import { useParams } from "react-router-dom";
import { AlertTriangle, Check, Circle, CircleDot, Loader2, OctagonAlert } from "lucide-react";
import { submitJob } from "../../api/client";
import { usePlan, useSource, useInvalidate } from "../../api/hooks";
import { Badge, Button } from "../../ui/kit";
import { DetailPage } from "../../ui/templates/DetailPage";
import { confirmThen } from "../../ui/confirm";
import { planStateKind, planStateLabel } from "./Plans";
import { stageBadge, verificationFor } from "../../lib/engineVerification";
import "./PlanDetail.css";

// Pipeline stages in order. `state` values from migration_plans map to how far we've progressed.
const STAGES = ["discover", "assess", "provision", "full-load", "cdc", "validate", "cutover"] as const;
const STAGE_LABEL: Record<string, string> = {
  discover: "Discover", assess: "Assess", provision: "Provision edge",
  "full-load": "Full load", cdc: "CDC replication", validate: "Validate", cutover: "Cutover",
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
// States in which the gateway is working on a stage right now; only these spin.
const WORKING = new Set(["provisioning", "full_loading", "validating", "cutover_in_progress"]);

export default function PlanDetail() {
  const { id = "" } = useParams();
  const { data: plan } = usePlan(id);
  const { data: source } = useSource(plan?.source_id || "");
  const inv = useInvalidate();
  const refresh = () => inv("db-plan", "db-plans", "db-source", "db-sources");

  useEffect(() => {
    if (plan) document.title = `Atlas · ${plan.name}`;
  }, [plan]);

  if (!plan) return <div className="at-loading">Loading plan…</div>;
  const done = COMPLETED_THROUGH[plan.state] ?? -1;
  const current = done + 1; // the next actionable stage index
  const completed = Math.max(0, done + 1);
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
      eyebrow="Migration plan"
      title={plan.name}
      state={`Plan ${plan.id} · source ${source?.name || plan.source_id} · ${planStateLabel(plan.state)}${engVer ? ` · ${engVer.engine}` : ""}`}
      actions={<div className="flex flex-wrap gap-2 items-center">
        {engVer && (
          <Badge kind={cdcLive && cutoverLive ? "success" : "warning"} title={engVer.note} className="normal-case">
            {cdcLive && cutoverLive ? "CDC and cutover verified live" : "CDC and cutover not verified live"}
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
            {rollbackDeadline && <span className="pd-deadline">Rollback available until {rollbackDeadline.toLocaleString()}</span>}
            <button type="button" className="at-btn danger" onClick={() => confirmThen({
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

      <section className="at-panel" aria-labelledby="pd-pipeline-title">
        <div className="at-panel-bar">
          <h2 id="pd-pipeline-title" className="at-caption">Pipeline</h2>
          <span className="grow" />
          <span className="pd-count">
            {completed} of {STAGES.length} stages done
          </span>
        </div>
        <div className="pd-pipeline">
          <div
            className="at-trough pd-trough"
            role="progressbar"
            aria-label="Stages done"
            aria-valuemin={0}
            aria-valuemax={STAGES.length}
            aria-valuenow={completed}
          >
            <span className="at-level" style={{ width: `${(completed / STAGES.length) * 100}%` }} />
          </div>
          <ol className="pd-stages" aria-label="Migration stages">
            {STAGES.map((stage, i) => {
              const status = i <= done ? "done" : i === current ? "current" : "upcoming";
              const working = status === "current" && WORKING.has(plan.state);
              const Icon = status === "done" ? Check : working ? Loader2 : status === "current" ? CircleDot : Circle;
              const actionable = status === "current" && IMPLEMENTED.has(stage);
              return (
                <li key={stage} className={`pd-stage ${status}`} aria-current={status === "current" ? "step" : undefined}>
                  <div className="pd-stage-top">
                    <Icon size={18} className={working ? "animate-spin" : undefined} aria-hidden />
                    <span className="mono">{String(i + 1).padStart(2, "0")}</span>
                  </div>
                  <h3 className="pd-stage-name">{STAGE_LABEL[stage]}</h3>
                  <div className="pd-stage-badges">
                    {status === "done" && <Badge kind="success">done</Badge>}
                    {status === "current" && <Badge kind="info">current</Badge>}
                    {status === "upcoming" && <span className="sr-only">upcoming</span>}
                    {(stage === "cdc" || stage === "cutover") && engVer && (
                      <Badge kind={stageBadge(engVer, stage) === "live" ? "success" : "warning"} className="normal-case">
                        {stageBadge(engVer, stage) === "live" ? "Verified live" : "Needs Kafka"}
                      </Badge>
                    )}
                  </div>
                  {actionable && stage === "cutover" && (
                    <div className="pd-stage-run">
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
                    <div className="pd-stage-run">
                      <Button size="sm" variant="primary" onClick={() => act(stage)?.catch(() => {})}>Run</Button>
                    </div>
                  )}
                </li>
              );
            })}
          </ol>
        </div>
      </section>

      {a && typeof a === "object" && "score" in a && (
        <section className="at-panel" aria-labelledby="pd-assessment-title">
          <div className="at-panel-bar">
            <h2 id="pd-assessment-title" className="at-caption">Assessment</h2>
            <span className="grow" />
            <Badge kind={a.risk === "low" ? "success" : a.risk === "medium" ? "warning" : "danger"}>
              {a.risk} risk
            </Badge>
          </div>
          <dl className="pd-figures">
            <div>
              <dt>Readiness</dt>
              <dd>
                {a.score}
                <span className="at-unit">/100</span>
              </dd>
              {typeof a.score === "number" && (
                <dd className="at-trough" aria-hidden>
                  <span className="at-level" style={{ width: `${Math.max(0, Math.min(100, a.score))}%` }} />
                </dd>
              )}
            </div>
            <div>
              <dt>Estimated downtime</dt>
              <dd>{a.downtime_estimate || "—"}</dd>
            </div>
            <div>
              <dt>Tables</dt>
              <dd>{a.table_count ?? "—"}</dd>
            </div>
          </dl>
          {!!a.blockers?.length && (
            <div className="pd-issues fail">
              <h3>Blockers</h3>
              <ul>
                {a.blockers.map((b: string, k: number) => (
                  <li key={k}>
                    <OctagonAlert size={15} aria-hidden />
                    <span>{b}</span>
                  </li>
                ))}
              </ul>
            </div>
          )}
          {!!a.warnings?.length && (
            <div className="pd-issues warn">
              <h3>Warnings</h3>
              <ul>
                {a.warnings.map((w: string, k: number) => (
                  <li key={k}>
                    <AlertTriangle size={15} aria-hidden />
                    <span>{w}</span>
                  </li>
                ))}
              </ul>
            </div>
          )}
          {!a.blockers?.length && !a.warnings?.length && (
            <p className="pd-clear">
              <Check size={15} aria-hidden />
              No blockers or warnings. Ready to provision.
            </p>
          )}
        </section>
      )}
    </DetailPage>
  );
}
