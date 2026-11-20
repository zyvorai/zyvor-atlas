// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Ops Advisor — explainable AI posture and read-only runbooks. No action execution by design.
import { useState, type FormEvent } from "react";
import {
  AlertTriangle,
  ArrowRight,
  BrainCircuit,
  CheckCircle2,
  Eye,
  LockKeyhole,
  RefreshCw,
  ShieldCheck,
  Sparkles,
} from "lucide-react";
import { apiError, http } from "../api/client";
import type { AdvisorMode, AdvisorResponse, AnomaliesResponse, IncidentsResponse, WhatIfResponse } from "../api/types";
import { navCrumbs } from "../nav/routes";
import { Badge, Button, RadialGauge } from "../ui/kit";
import { DashboardHero } from "../ui/templates/DashboardHero";

const PROMPTS = [
  "What should the storage team handle first?",
  "Explain the current capacity risk.",
  "Are recovery signals affecting data safety?",
];

function riskKind(level: AdvisorResponse["risk_level"]) {
  if (level === "critical") return "danger" as const;
  if (level === "high" || level === "moderate") return "warning" as const;
  return "success" as const;
}

function modeLabel(mode: AdvisorResponse["mode"]) {
  if (mode === "llm") return "Model enhanced";
  if (mode === "local_fallback") return "Local fallback";
  return "Local analysis";
}

export default function OpsAdvisor() {
  const [question, setQuestion] = useState(PROMPTS[0]);
  const [mode, setMode] = useState<AdvisorMode>("local");
  const [result, setResult] = useState<AdvisorResponse>();
  const [incidents, setIncidents] = useState<IncidentsResponse>();
  const [anomalies, setAnomalies] = useState<AnomaliesResponse>();
  const [sensitivity, setSensitivity] = useState("3.5");
  const [refreshingAnomalies, setRefreshingAnomalies] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [capacityTib, setCapacityTib] = useState("1");
  const [growthGib, setGrowthGib] = useState("10");
  const [horizon, setHorizon] = useState("90");
  const [clearAlerts, setClearAlerts] = useState(false);
  const [clearRecovery, setClearRecovery] = useState(false);
  const [simulation, setSimulation] = useState<WhatIfResponse>();
  const [simulating, setSimulating] = useState(false);
  const [simulationError, setSimulationError] = useState("");

  async function analyze(e?: FormEvent) {
    e?.preventDefault();
    setLoading(true);
    setError("");
    try {
      const [advisor, correlated, detected] = await Promise.all([
        http.post<AdvisorResponse>("/ai/advisor", { question, mode }),
        http.get<IncidentsResponse>("/ai/incidents"),
        http.get<AnomaliesResponse>(`/ai/anomalies?minutes=360&sensitivity=${sensitivity}`),
      ]);
      setResult(advisor.data);
      setIncidents(correlated.data);
      setAnomalies(detected.data);
    } catch (err) {
      setError(apiError(err));
    } finally {
      setLoading(false);
    }
  }

  // The sensitivity chips only affect /ai/anomalies — re-fetch just that endpoint instead of the
  // full analyze() (which would also needlessly redo the advisor + incidents calls). Only called
  // once `result` exists (the panel that holds these chips is gated on it), so anomalies is
  // already populated and there's a prior response to fall back to if this refresh fails.
  async function changeSensitivity(value: string) {
    setSensitivity(value);
    setRefreshingAnomalies(true);
    try {
      const { data } = await http.get<AnomaliesResponse>(`/ai/anomalies?minutes=360&sensitivity=${value}`);
      setAnomalies(data);
    } catch (err) {
      setError(apiError(err));
    } finally {
      setRefreshingAnomalies(false);
    }
  }

  async function simulate(e: FormEvent) {
    e.preventDefault();
    setSimulating(true);
    setSimulationError("");
    try {
      const { data } = await http.post<WhatIfResponse>("/ai/what-if", {
        add_capacity_bytes: Math.round(Number(capacityTib || 0) * 2 ** 40),
        horizon_days: Number(horizon),
        projected_growth_bytes_per_day: Math.round(Number(growthGib || 0) * 2 ** 30),
        assume_alerts_resolved: clearAlerts,
        assume_recovery_complete: clearRecovery,
      });
      setSimulation(data);
    } catch (err) {
      setSimulationError(apiError(err));
    } finally {
      setSimulating(false);
    }
  }

  return (
    <DashboardHero
      className="at-stack"
      crumbs={navCrumbs("ops-advisor")}
      eyebrow="OBSERVABILITY · EXPLAINABLE AI"
      title="Ops Advisor"
      state={
        result
          ? `${result.risk_level} risk · ${result.actions.length} prioritized step${result.actions.length === 1 ? "" : "s"} · advisory only`
          : "Ask Atlas to correlate capacity, recovery, alerts, forecasts, and recent job failures."
      }
      actions={
        result ? (
          <Button icon={RefreshCw} loading={loading} onClick={() => analyze()}>
            Refresh
          </Button>
        ) : undefined
      }
    >
      <form className="at-panel" onSubmit={analyze}>
        <div className="at-panel-bar">
          <BrainCircuit size={16} aria-hidden />
          <span className="at-caption">Ask about the live storage posture</span>
          <span className="grow" />
          <Badge kind="info">No autonomous actions</Badge>
        </div>
        <div style={{ padding: 18, display: "grid", gap: 14 }}>
          <textarea
            className="field"
            rows={3}
            maxLength={512}
            value={question}
            aria-label="Advisor question"
            placeholder="What should the storage team handle first?"
            onChange={(e) => setQuestion(e.target.value)}
            style={{ width: "100%", resize: "vertical" }}
          />
          <div className="at-chips" style={{ padding: 0, margin: 0 }}>
            {PROMPTS.map((prompt) => (
              <button
                key={prompt}
                className={`at-chip${question === prompt ? " on" : ""}`}
                type="button"
                onClick={() => setQuestion(prompt)}
              >
                {prompt}
              </button>
            ))}
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap" }}>
            <div className="at-chips" style={{ padding: 0, margin: 0 }} aria-label="Analysis mode">
              {(["local", "auto", "llm"] as AdvisorMode[]).map((value) => (
                <button
                  key={value}
                  className={`at-chip${mode === value ? " on" : ""}`}
                  type="button"
                  onClick={() => setMode(value)}
                >
                  {value === "llm" ? "LLM" : value[0].toUpperCase() + value.slice(1)}
                </button>
              ))}
            </div>
            <span className="at-sub" style={{ margin: 0, flex: 1 }}>
              {mode === "local"
                ? "Telemetry stays inside Atlas."
                : "Aggregate telemetry may be sent to the configured model endpoint."}
            </span>
            <Button type="submit" variant="primary" icon={Sparkles} loading={loading} disabled={!question.trim()}>
              Analyze posture
            </Button>
          </div>
        </div>
      </form>

      {error ? (
        <div className="at-panel" role="alert">
          <div className="at-list-row" style={{ color: "var(--at-fail)" }}>
            <AlertTriangle size={17} aria-hidden />
            <span>{error}</span>
          </div>
        </div>
      ) : null}

      {!result && !error ? (
        <div className="at-panel">
          <div className="at-list-row">
            <ShieldCheck size={18} style={{ color: "var(--at-ok)" }} aria-hidden />
            <div>
              <div className="at-val" style={{ fontSize: 15 }}>Atlas remains the authority</div>
              <p className="at-sub" style={{ margin: "4px 0 0" }}>
                The model can summarize evidence, but it cannot change the risk score, rewrite the runbook, or execute storage operations.
              </p>
            </div>
          </div>
        </div>
      ) : null}

      {result ? (
        <>
          <div className="at-panel">
            <div className="at-panel-bar">
              <span className="at-caption">Current assessment</span>
              <Badge kind={riskKind(result.risk_level)} dot>{result.risk_level}</Badge>
              <Badge kind="neutral">{modeLabel(result.mode)}</Badge>
              <span className="grow" />
              <Badge kind="success"><LockKeyhole size={12} /> advisory only</Badge>
            </div>
            <div style={{ padding: 20, display: "flex", alignItems: "center", gap: 24, flexWrap: "wrap" }}>
              <RadialGauge pct={result.risk_score} size={132} label="RISK SCORE" />
              <div style={{ flex: "1 1 360px" }}>
                <div className="at-val md" style={{ textTransform: "capitalize" }}>{result.risk_level} risk</div>
                <p className="at-sub" style={{ margin: "8px 0 0", maxWidth: 760 }}>{result.summary}</p>
                {result.warnings.map((warning) => (
                  <div key={warning} style={{ color: "var(--at-warn)", marginTop: 10, fontSize: 13 }}>
                    <AlertTriangle size={14} style={{ display: "inline", marginRight: 6 }} />{warning}
                  </div>
                ))}
              </div>
            </div>
          </div>

          <div className="at-panel">
            <div className="at-panel-bar">
              <span className="at-caption">Correlated incidents</span>
              <span className="grow" />
              <Badge kind={incidents?.count ? "warning" : "success"}>{incidents?.count ?? 0} active</Badge>
            </div>
            {incidents?.incidents.length ? incidents.incidents.map((incident) => (
              <div className="at-list-row" key={incident.id} style={{ alignItems: "flex-start" }}>
                <Badge kind={incident.severity === "critical" ? "danger" : incident.severity === "warning" ? "warning" : "neutral"} dot>
                  {incident.severity}
                </Badge>
                <div style={{ flex: 1 }}>
                  <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
                    <strong>{incident.title}</strong>
                    <Badge kind="info">{Math.round(incident.confidence * 100)}% correlation</Badge>
                    <span className="mono" style={{ fontSize: 11 }}>{incident.category.replaceAll("_", " ")}</span>
                  </div>
                  <p className="at-sub" style={{ margin: "5px 0 8px" }}>{incident.likely_cause}</p>
                  <div style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
                    {incident.signals.map((signal) => (
                      <Badge key={`${signal.source}-${signal.resource_id}-${signal.title}`} kind="neutral">
                        {signal.source} · {signal.title}
                      </Badge>
                    ))}
                  </div>
                </div>
              </div>
            )) : (
              <div className="at-list-row" style={{ color: "var(--at-ok)" }}>
                <CheckCircle2 size={17} aria-hidden /> No related active signals were found.
              </div>
            )}
          </div>

          <div className="at-panel">
            <div className="at-panel-bar">
              <span className="at-caption">Telemetry anomalies · 6-hour window</span>
              <div className="at-chips" style={{ padding: 0, margin: 0 }} aria-label="Anomaly sensitivity">
                {["3", "3.5", "5"].map((value) => (
                  <button
                    key={value}
                    type="button"
                    className={`at-chip${sensitivity === value ? " on" : ""}`}
                    onClick={() => changeSensitivity(value)}
                    disabled={refreshingAnomalies}
                    title="Lower values detect smaller deviations"
                  >
                    {value === "3" ? "Sensitive" : value === "3.5" ? "Balanced" : "Strict"}
                  </button>
                ))}
              </div>
              <span className="grow" />
              <Badge kind={anomalies && anomalies.telemetry_status !== "fresh" ? "warning" : anomalies?.anomalies.length ? "warning" : "success"}>
                {anomalies && anomalies.telemetry_status !== "fresh" ? "Detection paused" : `${anomalies?.anomalies.length ?? 0} detected`}
              </Badge>
            </div>
            {anomalies?.warnings.map((warning) => (
              <div className="at-list-row" key={warning} style={{ color: "var(--at-warn)" }}>
                <AlertTriangle size={16} aria-hidden /> {warning}
              </div>
            ))}
            {anomalies?.anomalies.length ? anomalies.anomalies.map((anomaly) => (
              <div className="at-list-row" key={anomaly.id} style={{ alignItems: "flex-start" }}>
                <Badge kind={anomaly.severity === "critical" ? "danger" : "warning"} dot>
                  {anomaly.severity}
                </Badge>
                <div style={{ flex: 1 }}>
                  <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
                    <strong>{anomaly.label}</strong>
                    <Badge kind="info">score {anomaly.score.toFixed(1)}</Badge>
                    <span className="mono" style={{ fontSize: 11 }}>{anomaly.change_percent >= 0 ? "+" : ""}{anomaly.change_percent.toFixed(1)}%</span>
                  </div>
                  <p className="at-sub" style={{ margin: "5px 0 8px" }}>{anomaly.explanation}</p>
                  <code className="mono" style={{ fontSize: 11.5 }}>{anomaly.inspect}</code>
                </div>
              </div>
            )) : anomalies?.warnings.length ? null : (
              <div className="at-list-row" style={{ color: "var(--at-ok)" }}>
                <CheckCircle2 size={17} aria-hidden /> No statistically significant upward deviations detected.
              </div>
            )}
            {anomalies ? (
              <div className="at-list-row">
                <span className="at-sub" style={{ margin: 0 }}>
                  {anomalies.sample_count} samples · {anomalies.telemetry_status} telemetry
                  {anomalies.latest_sample_age_minutes != null ? ` · latest ${anomalies.latest_sample_age_minutes} min ago` : ""}
                  {` · median/MAD baseline · sensitivity ${anomalies.sensitivity}`}
                </span>
              </div>
            ) : null}
          </div>

          <div className="at-instrs">
            <div className="at-instr">
              <div className="at-caption">Capacity used</div>
              <div className="at-val md">{result.evidence.capacity_used_percent.toFixed(1)}%</div>
              <div className="at-delta">live inventory</div>
            </div>
            <div className="at-instr">
              <div className="at-caption">Days to full</div>
              <div className="at-val md">{result.evidence.days_to_full == null ? "—" : result.evidence.days_to_full.toFixed(1)}</div>
              <div className="at-delta">14-day regression</div>
            </div>
            <div className="at-instr">
              <div className="at-caption">Open alerts</div>
              <div className="at-val md">{result.evidence.open_alerts}</div>
              <div className="at-delta mono">{result.evidence.critical_alerts} critical · {result.evidence.warning_alerts} warning</div>
            </div>
            <div className="at-instr">
              <div className="at-caption">Recent failures</div>
              <div className="at-val md">{result.evidence.failed_jobs_15m}</div>
              <div className="at-delta">last 15 minutes</div>
            </div>
          </div>

          <div className="at-panel">
            <div className="at-panel-bar">
              <span className="at-caption">Prioritized runbook</span>
              <span className="grow" />
              <Badge kind="neutral">read-only inspection</Badge>
            </div>
            {result.actions.map((action, index) => (
              <div className="at-list-row" key={`${action.priority}-${action.title}`} style={{ alignItems: "flex-start" }}>
                <Badge kind={action.priority === 1 ? "danger" : action.priority <= 3 ? "warning" : "neutral"}>
                  P{action.priority}
                </Badge>
                <div style={{ flex: 1 }}>
                  <div style={{ fontWeight: 650 }}>{index + 1}. {action.title}</div>
                  <p className="at-sub" style={{ margin: "4px 0 8px" }}>{action.rationale}</p>
                  <code className="mono" style={{ fontSize: 11.5 }}>{action.inspect}</code>
                </div>
                <Eye size={16} style={{ color: "var(--at-ink-4)", marginTop: 2 }} aria-label="Inspect only" />
              </div>
            ))}
          </div>

          <form className="at-panel" onSubmit={simulate}>
            <div className="at-panel-bar">
              <span className="at-caption">What-if capacity simulator</span>
              <span className="grow" />
              <Badge kind="info">no inventory changes</Badge>
            </div>
            <div style={{ padding: 18, display: "grid", gap: 16 }}>
              <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(160px, 1fr))", gap: 12 }}>
                <label>
                  <span className="at-caption">Capacity to add · TiB</span>
                  <input className="field" type="number" min="0" step="0.1" value={capacityTib} onChange={(e) => setCapacityTib(e.target.value)} style={{ width: "100%", marginTop: 6 }} />
                </label>
                <label>
                  <span className="at-caption">Daily growth · GiB</span>
                  <input className="field" type="number" min="0" step="0.1" value={growthGib} onChange={(e) => setGrowthGib(e.target.value)} style={{ width: "100%", marginTop: 6 }} />
                </label>
                <label>
                  <span className="at-caption">Horizon · days</span>
                  <input className="field" type="number" min="1" max="365" value={horizon} onChange={(e) => setHorizon(e.target.value)} style={{ width: "100%", marginTop: 6 }} />
                </label>
              </div>
              <div style={{ display: "flex", alignItems: "center", gap: 16, flexWrap: "wrap" }}>
                <label style={{ display: "flex", alignItems: "center", gap: 7, fontSize: 13 }}>
                  <input type="checkbox" checked={clearAlerts} onChange={(e) => setClearAlerts(e.target.checked)} /> Assume alerts resolved
                </label>
                <label style={{ display: "flex", alignItems: "center", gap: 7, fontSize: 13 }}>
                  <input type="checkbox" checked={clearRecovery} onChange={(e) => setClearRecovery(e.target.checked)} /> Assume recovery complete
                </label>
                <span className="grow" />
                <Button type="submit" icon={Sparkles} loading={simulating}>Simulate</Button>
              </div>
              {simulationError ? <div role="alert" style={{ color: "var(--at-fail)" }}>{simulationError}</div> : null}
              {simulation ? (
                <div style={{ display: "grid", gap: 14 }}>
                  <div style={{ display: "flex", alignItems: "center", justifyContent: "center", gap: 22, flexWrap: "wrap" }}>
                    <div style={{ textAlign: "center" }}>
                      <div className="at-caption">Baseline</div>
                      <div className="at-val md">{simulation.baseline.risk_score}</div>
                      <Badge kind={riskKind(simulation.baseline.risk_level)}>{simulation.baseline.risk_level}</Badge>
                    </div>
                    <ArrowRight size={24} style={{ color: simulation.risk_delta < 0 ? "var(--at-ok)" : "var(--at-warn)" }} aria-hidden />
                    <div style={{ textAlign: "center" }}>
                      <div className="at-caption">Projected · {simulation.horizon_days}d</div>
                      <div className="at-val md">{simulation.projected.risk_score}</div>
                      <Badge kind={riskKind(simulation.projected.risk_level)}>{simulation.projected.risk_level}</Badge>
                    </div>
                    <Badge kind={simulation.risk_delta < 0 ? "success" : simulation.risk_delta > 0 ? "warning" : "neutral"}>
                      {simulation.risk_delta > 0 ? "+" : ""}{simulation.risk_delta} risk
                    </Badge>
                  </div>
                  <div className="at-list-row">
                    <span className="at-sub" style={{ margin: 0 }}>
                      Projected utilization <strong>{simulation.projected.capacity_used_percent.toFixed(1)}%</strong>
                      {simulation.projected.days_to_full == null ? " · no meaningful fill date" : ` · ${simulation.projected.days_to_full.toFixed(1)} days to full`}
                    </span>
                  </div>
                </div>
              ) : null}
            </div>
          </form>

          <div className="at-panel">
            <div className="at-panel-bar"><span className="at-caption">Safety boundary</span></div>
            <div className="at-list-row" style={{ color: "var(--at-ok)" }}>
              <CheckCircle2 size={17} aria-hidden />
              <span>Advisor response confirms <span className="mono">can_execute: {String(result.can_execute)}</span>. Every proposed step points to a read-only inspection endpoint.</span>
            </div>
          </div>
        </>
      ) : null}
    </DashboardHero>
  );
}
