// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Ops Advisor — explainable AI risk assessment and read-only runbooks. No action execution by design.
import { useState, type FormEvent } from "react";
import {
  AlertTriangle,
  ArrowRight,
  BrainCircuit,
  CheckCircle2,
  Eye,
  LockKeyhole,
  RefreshCw,
  Sparkles,
} from "lucide-react";
import { apiError, http } from "../api/client";
import type { AdvisorMode, AdvisorResponse, AnomaliesResponse, IncidentsResponse, WhatIfResponse } from "../api/types";
import { navCrumbs } from "../nav/routes";
import { EmptyState } from "../graphics";
import { Badge, Button } from "../ui/kit";
import { DashboardHero } from "../ui/templates/DashboardHero";
import "./OpsAdvisor.css";

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

/** The advisor's 0-100 score. Its colour follows the level the gateway reports, not the number. */
function RiskMeter({ score, level }: { score: number; level: AdvisorResponse["risk_level"] }) {
  const value = Math.max(0, Math.min(100, score));
  return (
    <div
      className={`adv-meter ${riskKind(level)}`}
      role="meter"
      aria-label="Risk score"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={value}
      aria-valuetext={`${score} of 100, ${level} risk`}
    >
      <div>
        <span className="adv-score">{score}</span>
        <span className="at-unit">/ 100 risk score</span>
      </div>
      <div className="adv-track">
        <i style={{ width: `${value}%` }} />
      </div>
    </div>
  );
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
      eyebrow="Observability"
      title="Ops Advisor"
      state={
        result
          ? `${result.risk_level[0].toUpperCase()}${result.risk_level.slice(1)} risk · ${result.actions.length} prioritized step${result.actions.length === 1 ? "" : "s"} · advisory only`
          : "Ask about capacity, recovery, alerts, forecasts and recent job failures."
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
          <h2 className="at-caption">Ask about your storage</h2>
          <span className="grow" />
          <Badge kind="info">No autonomous actions</Badge>
        </div>
        <div className="adv-form">
          <textarea
            className="field adv-question"
            rows={3}
            maxLength={512}
            value={question}
            aria-label="Advisor question"
            placeholder="What should the storage team handle first?"
            onChange={(e) => setQuestion(e.target.value)}
          />
          <div className="at-chips adv-chips">
            {PROMPTS.map((prompt) => (
              <button
                key={prompt}
                className={`at-chip${question === prompt ? " on" : ""}`}
                type="button"
                aria-pressed={question === prompt}
                onClick={() => setQuestion(prompt)}
              >
                {prompt}
              </button>
            ))}
          </div>
          <div className="adv-toolbar">
            <div className="at-chips adv-chips" role="group" aria-label="Analysis mode">
              {(["local", "auto", "llm"] as AdvisorMode[]).map((value) => (
                <button
                  key={value}
                  className={`at-chip${mode === value ? " on" : ""}`}
                  type="button"
                  aria-pressed={mode === value}
                  onClick={() => setMode(value)}
                >
                  {value === "llm" ? "LLM" : value[0].toUpperCase() + value.slice(1)}
                </button>
              ))}
            </div>
            <span className="adv-grow adv-note">
              {mode === "local"
                ? "Telemetry stays inside Atlas."
                : "Aggregate telemetry may be sent to the configured model endpoint."}
            </span>
            <Button type="submit" variant="primary" icon={Sparkles} loading={loading} disabled={!question.trim()}>
              Analyze
            </Button>
          </div>
        </div>
      </form>

      {error ? (
        <div className="at-panel" role="alert">
          <div className="at-list-row at-fail">
            <AlertTriangle size={17} aria-hidden />
            <span>{error}</span>
          </div>
        </div>
      ) : null}

      {!result && !error ? (
        <section className="at-panel">
          <h2 className="sr-only">About this advisor</h2>
          <EmptyState
            art="chart"
            title="Advice only"
            description="A model can summarize the evidence, but it cannot change the risk score, rewrite the runbook or run storage operations."
          />
        </section>
      ) : null}

      {result ? (
        <>
          <div className="at-panel">
            <div className="at-panel-bar">
              <h2 className="at-caption">Current assessment</h2>
              <Badge kind={riskKind(result.risk_level)} dot>{result.risk_level}</Badge>
              <Badge kind="neutral">{modeLabel(result.mode)}</Badge>
              <span className="grow" />
              <Badge kind="success"><LockKeyhole size={12} /> advisory only</Badge>
            </div>
            <div className="adv-assessment">
              <RiskMeter score={result.risk_score} level={result.risk_level} />
              <div className="adv-summary">
                <div className="at-val md adv-level">{result.risk_level} risk</div>
                <p className="adv-copy">{result.summary}</p>
                {result.warnings.map((warning) => (
                  <div key={warning} className="adv-warning">
                    <AlertTriangle size={14} aria-hidden />{warning}
                  </div>
                ))}
              </div>
            </div>
          </div>

          <div className="at-panel">
            <div className="at-panel-bar">
              <h2 className="at-caption">Correlated incidents</h2>
              <span className="grow" />
              <Badge kind={incidents?.count ? "warning" : "success"}>{incidents?.count ?? 0} active</Badge>
            </div>
            {incidents?.incidents.length ? incidents.incidents.map((incident) => (
              <div className="at-list-row" key={incident.id}>
                <Badge kind={incident.severity === "critical" ? "danger" : incident.severity === "warning" ? "warning" : "neutral"} dot>
                  {incident.severity}
                </Badge>
                <div className="adv-grow">
                  <div className="adv-line">
                    <strong>{incident.title}</strong>
                    <Badge kind="info">{Math.round(incident.confidence * 100)}% correlation</Badge>
                    <span className="mono adv-small">{incident.category.replaceAll("_", " ")}</span>
                  </div>
                  <p className="adv-copy">{incident.likely_cause}</p>
                  <div className="adv-line">
                    {incident.signals.map((signal) => (
                      <Badge key={`${signal.source}-${signal.resource_id}-${signal.title}`} kind="neutral">
                        {signal.source} · {signal.title}
                      </Badge>
                    ))}
                  </div>
                </div>
              </div>
            )) : (
              <div className="at-list-row at-ok">
                <CheckCircle2 size={17} aria-hidden /> No related active signals were found.
              </div>
            )}
          </div>

          <div className="at-panel">
            <div className="at-panel-bar">
              <h2 className="at-caption">Telemetry anomalies · 6-hour window</h2>
              <div className="at-chips adv-chips" role="group" aria-label="Anomaly sensitivity">
                {["3", "3.5", "5"].map((value) => (
                  <button
                    key={value}
                    type="button"
                    className={`at-chip${sensitivity === value ? " on" : ""}`}
                    aria-pressed={sensitivity === value}
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
              <div className="at-list-row at-warn" key={warning}>
                <AlertTriangle size={16} aria-hidden /> {warning}
              </div>
            ))}
            {anomalies?.anomalies.length ? anomalies.anomalies.map((anomaly) => (
              <div className="at-list-row" key={anomaly.id}>
                <Badge kind={anomaly.severity === "critical" ? "danger" : "warning"} dot>
                  {anomaly.severity}
                </Badge>
                <div className="adv-grow">
                  <div className="adv-line">
                    <strong>{anomaly.label}</strong>
                    <Badge kind="info">score {anomaly.score.toFixed(1)}</Badge>
                    <span className="mono adv-small">{anomaly.change_percent >= 0 ? "+" : ""}{anomaly.change_percent.toFixed(1)}%</span>
                  </div>
                  <p className="adv-copy">{anomaly.explanation}</p>
                  <code className="adv-code">{anomaly.inspect}</code>
                </div>
              </div>
            )) : anomalies?.warnings.length ? null : (
              <div className="at-list-row at-ok">
                <CheckCircle2 size={17} aria-hidden /> No statistically significant upward deviations detected.
              </div>
            )}
            {anomalies ? (
              <div className="at-list-row">
                <span className="adv-note">
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
              <h2 className="at-caption">Prioritized runbook</h2>
              <span className="grow" />
              <Badge kind="neutral">read-only inspection</Badge>
            </div>
            {result.actions.map((action, index) => (
              <div className="at-list-row" key={`${action.priority}-${action.title}`}>
                <Badge kind={action.priority === 1 ? "danger" : action.priority <= 3 ? "warning" : "neutral"}>
                  P{action.priority}
                </Badge>
                <div className="adv-grow">
                  <h3 className="adv-step">{index + 1}. {action.title}</h3>
                  <p className="adv-copy">{action.rationale}</p>
                  <code className="adv-code">{action.inspect}</code>
                </div>
                <Eye size={16} className="adv-eye" role="img" aria-label="Inspect only" />
              </div>
            ))}
          </div>

          <form className="at-panel" onSubmit={simulate}>
            <div className="at-panel-bar">
              <h2 className="at-caption">What-if capacity simulator</h2>
              <span className="grow" />
              <Badge kind="info">no inventory changes</Badge>
            </div>
            <div className="adv-form">
              <div className="adv-fields">
                <label>
                  <span className="at-caption">Capacity to add · TiB</span>
                  <input className="field" type="number" min="0" step="0.1" value={capacityTib} onChange={(e) => setCapacityTib(e.target.value)} />
                </label>
                <label>
                  <span className="at-caption">Daily growth · GiB</span>
                  <input className="field" type="number" min="0" step="0.1" value={growthGib} onChange={(e) => setGrowthGib(e.target.value)} />
                </label>
                <label>
                  <span className="at-caption">Horizon · days</span>
                  <input className="field" type="number" min="1" max="365" value={horizon} onChange={(e) => setHorizon(e.target.value)} />
                </label>
              </div>
              <div className="adv-toolbar">
                <label className="adv-check">
                  <input type="checkbox" checked={clearAlerts} onChange={(e) => setClearAlerts(e.target.checked)} /> Assume alerts resolved
                </label>
                <label className="adv-check">
                  <input type="checkbox" checked={clearRecovery} onChange={(e) => setClearRecovery(e.target.checked)} /> Assume recovery complete
                </label>
                <span className="grow" />
                <Button type="submit" icon={Sparkles} loading={simulating}>Simulate</Button>
              </div>
              {simulationError ? <div role="alert" className="at-fail">{simulationError}</div> : null}
              {simulation ? (
                <div className="adv-result">
                  <div className="adv-compare">
                    <div>
                      <div className="at-caption">Baseline</div>
                      <div className="at-val md">{simulation.baseline.risk_score}</div>
                      <Badge kind={riskKind(simulation.baseline.risk_level)}>{simulation.baseline.risk_level}</Badge>
                    </div>
                    <ArrowRight size={24} className={simulation.risk_delta < 0 ? "at-ok" : "at-warn"} aria-hidden />
                    <div>
                      <div className="at-caption">Projected · {simulation.horizon_days}d</div>
                      <div className="at-val md">{simulation.projected.risk_score}</div>
                      <Badge kind={riskKind(simulation.projected.risk_level)}>{simulation.projected.risk_level}</Badge>
                    </div>
                    <Badge kind={simulation.risk_delta < 0 ? "success" : simulation.risk_delta > 0 ? "warning" : "neutral"}>
                      {simulation.risk_delta > 0 ? "+" : ""}{simulation.risk_delta} risk
                    </Badge>
                  </div>
                  <div className="adv-note">
                    <span>
                      Projected utilization <strong>{simulation.projected.capacity_used_percent.toFixed(1)}%</strong>
                      {simulation.projected.days_to_full == null ? " · no meaningful fill date" : ` · ${simulation.projected.days_to_full.toFixed(1)} days to full`}
                    </span>
                  </div>
                </div>
              ) : null}
            </div>
          </form>

          <div className="at-panel">
            <div className="at-panel-bar"><h2 className="at-caption">Safety boundary</h2></div>
            <div className="at-list-row at-ok">
              <CheckCircle2 size={17} aria-hidden />
              <span>Advisor response confirms <span className="mono">can_execute: {String(result.can_execute)}</span>. Every proposed step points to a read-only inspection endpoint.</span>
            </div>
          </div>
        </>
      ) : null}
    </DashboardHero>
  );
}
