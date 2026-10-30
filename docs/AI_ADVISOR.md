<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Atlas Ops Advisor

Atlas Ops Advisor turns current Atlas telemetry into an explainable risk score and a prioritized,
read-only runbook. The endpoint never executes an action and does not require a model.

Operators can use **Storage Center → Observability → Ops Advisor** to ask a focused question,
choose local/automatic/LLM analysis, inspect evidence, and follow the prioritized runbook.

```bash
curl -sS -X POST http://127.0.0.1:5110/api/atlas/v1/ai/advisor \
  -H 'Content-Type: application/json' \
  -d '{"question":"What should the storage team handle first?","mode":"local"}'
```

The response contains:

- a bounded `risk_score` and `risk_level`;
- evidence from capacity, the 14-day forecast, recovery counters, open alerts, recent failures, and NVMe wear (drives at 80% or more of rated endurance, media errors, spare at its threshold or a critical-warning flag, from Ceph's device-health scrapes, see `GET /ceph/nvme`; skipped when Ceph has no scrapes), and hosts with NVMe drives that fall short of the CPU/RAM rule of thumb (`GET /ceph/sizing`, a heuristic that adds a smaller score than a failing drive);
- prioritized actions pointing only to read-only Atlas endpoints;
- `can_execute: false`, an explicit automation safety boundary.

## Modes

| Mode | Behavior |
| --- | --- |
| `local` | Deterministic analysis; no operational context leaves Atlas. |
| `auto` | Uses an OpenAI-compatible provider when configured, otherwise local; safely falls back on provider failure. |
| `llm` | Requires a configured provider and returns `503` if it fails. |

Configure an optional provider with `ATLAS_AI_BASE_URL`, `ATLAS_AI_MODEL`, and optionally
`ATLAS_AI_API_KEY` and `ATLAS_AI_TIMEOUT_SECS`. The provider may rewrite only the executive summary;
Atlas computes and retains authority over the score, evidence, and actions. Remote endpoints must
use HTTPS. Plain HTTP is permitted only for a loopback model endpoint.

### Narratives for anomalies and what-if

`GET /ai/anomalies` and `POST /ai/what-if` accept the same `?mode=` (default `local`). With `auto` or `llm`,
a configured provider adds a short plain-English `narrative` (and `narrative_mode`: `llm` or `local_fallback`)
to the response; with `local` both fields are `null` and nothing leaves Atlas. The anomaly narrative is only written
when at least one anomaly was detected; the model gets only the figures Atlas computed with
an instruction to treat every string as untrusted data, and the scores, anomalies and runbook are never taken
from the model. The MCP tools (`detect_anomalies`, `what_if_capacity`, `list_incidents`, `ops_advisor`) always
run in `local` mode: an agent cannot make Atlas call an external provider.

## Storage I/O risk from `atlas-io-agent`

Set `ATLAS_IO_AGENT_URL` (for example `http://atlas-io-agent.zyvor-system:5111`; optional
`ATLAS_IO_AGENT_TIMEOUT_SECS`, default 3, max 10) and `/ai/advisor` adds an `io_risk` block read
from the agent's `GET /io/risk` ([IO_RISK.md](IO_RISK.md)):

```json
"io_risk": {"status":"ok","level":"critical",
            "flagged":[{"device":"rbd0","verdict":"critical_latency"}],"detail":null}
```

- Informational only: it never changes `risk_score` or the runbook, and `can_execute` stays false.
- Fails open to *unavailable*: an unreachable, slow, oversized or malformed agent response gives
  `status: "unavailable"` plus a warning, never `healthy`. Unset URL means no `io_risk` field.
- The agent's output is untrusted: the level and verdicts are whitelisted, device names are
  sanitized and capped at 8, redirects are not followed, and none of it is sent to the LLM provider.
- If the agent has `ATLAS_IO_TOKEN` set, give the gateway the same value as
  `ATLAS_IO_AGENT_TOKEN` (never followed across redirects). Keep the agent on a cluster-internal
  Service either way. The gateway fetches it live per request, so the data is as fresh as the
  agent's last poll.

When authentication is enabled, the endpoint requires an operator-or-higher token and records an
`ai.advisor` audit event. Questions are capped at 512 characters, model output at 2,000 characters,
and only aggregate telemetry plus at most 20 alert titles is sent to the provider.

## Incident correlation

`GET /api/atlas/v1/ai/incidents` groups related open alerts and recent job failures into incident
families: data safety, capacity, Ceph recovery, replication, job engine, availability, or
unclassified. Each incident includes the contributing signals, highest severity, a bounded
correlation-confidence score, a likely-cause explanation, and read-only inspection endpoints.

This is deterministic correlation rather than a claim of statistical causation. It reduces alert
fatigue while keeping every contributing signal visible to the operator.

## What-if capacity planning

`POST /api/atlas/v1/ai/what-if` projects risk without mutating inventory:

```json
{
  "add_capacity_bytes": 1099511627776,
  "horizon_days": 90,
  "projected_growth_bytes_per_day": 10737418240,
  "assume_alerts_resolved": false,
  "assume_recovery_complete": false
}
```

The response compares baseline and projected risk, capacity utilization, days-to-full, remaining
actions, and explicit assumptions. Horizons are limited to 1–365 days, added capacity to 1 EiB,
and growth must be finite and non-negative. The endpoint records an `ai.what_if` audit event and
always returns `can_execute: false`.

## Explainable anomaly detection

`GET /api/atlas/v1/ai/anomalies?minutes=360&sensitivity=3.5` analyzes persisted metric history
without an external model. It detects upward deviations in:

- capacity growth;
- read/write byte deltas;
- read/write operation deltas;
- concurrent jobs;
- open alerts.

The detector uses the median and median absolute deviation (MAD), which are resistant to older
spikes contaminating the baseline. Cumulative counters are converted to per-sample deltas before
analysis. Each result exposes its current value, baseline, MAD, percentage change, anomaly score,
severity, explanation, and a read-only inspection endpoint.

The query accepts a 15-minute to 14-day window and sensitivity from 2–10. Lower sensitivity finds
smaller deviations. Atlas also reports `telemetry_status` (`fresh`, `stale`, or `unavailable`) and
`latest_sample_age_minutes`. Detection is paused with a warning when the latest sample is over
15 minutes old, its timestamp is invalid, or telemetry is unavailable; old spikes are never
presented as current incidents. At least five history samples are required; Atlas returns a warning rather
than inventing a result when the baseline is insufficient.
