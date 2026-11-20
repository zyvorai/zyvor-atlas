<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Distributed tracing (OpenTelemetry)

Atlas can export spans over OTLP/HTTP to any OpenTelemetry-compatible backend (Jaeger, Tempo,
Grafana Cloud, Honeycomb, ...). Off by default — set `ATLAS_OTEL_EXPORTER_ENDPOINT` to turn it on.
This is always-compiled-in Rust code (`crates/atlas-common/src/lib.rs::init_tracing()`), not a
Cargo feature — no rebuild needed to enable it.

## Enabling it

```bash
ATLAS_OTEL_EXPORTER_ENDPOINT=http://localhost:4318 make run
```

Give it the collector's base URL (Atlas appends `/v1/traces` itself — don't include the path).
Optionally set `ATLAS_OTEL_SAMPLE_RATIO` (default `1.0` = export every span; lower it, e.g. `0.1`,
for high-traffic deployments where exporting every request would be wasteful).

If the endpoint is unreachable at startup, Atlas logs a warning and keeps running with tracing
export disabled — this can never take the gateway down.

## Try it in the lab

`deploy/observability/up.sh` now also stands up an all-in-one Jaeger (OTLP receiver + UI, NodePorts
30516/30517 — see `deploy/observability/jaeger.yaml`). Point a gateway at it:

```bash
ATLAS_OTEL_EXPORTER_ENDPOINT=http://<node>:30516 <run the gateway>
```

Then open `http://<node>:30517` and search for the `atlas-gateway` service — every HTTP request
gets a span (via `tower_http::trace::TraceLayer`), and a handful of the more interesting internal
operations get their own nested spans:

- `run_discovery` (`crates/atlas-discovery`) — every discovery pass, tagged with `backend_id`.
- `dispatch` (`crates/atlas-jobs/src/dispatch/mod.rs`) — every async job (volume create/expand,
  snapshots, RBD ops, DataBridge stage transitions), tagged with `tenant_id`.
- `compute_advisor` (`crates/atlas-gateway/src/routes/ai.rs`) — the Ops Advisor's risk assessment,
  tagged with the calling `actor`. Also the `ops_advisor` MCP tool (`crates/atlas-gateway/src/mcp.rs`
  — see `docs/HERMES_AGENT.md`), since both call the same function.

This is a first slice, not blanket instrumentation — see "What's not covered" below.

## What's not covered

Only a handful of functions carry manual `#[tracing::instrument]` spans; most of the codebase's
business logic (driver calls, most inventory queries) has no spans of its own and shows up only as
flat log events inside whichever HTTP-request span was active. Add `#[tracing::instrument]` to a
function when you need to see it as its own span in the trace waterfall — it's a one-line
attribute, no exporter-side changes needed.

Metrics and logs are not exported via OTLP — Atlas already has a separate Prometheus `/metrics`
endpoint (`deploy/observability/prometheus.yaml`) and structured `tracing` log lines (`RUST_LOG`);
adding an OTLP metrics/logs pipeline on top would be a separate effort.

## Helm chart

`deploy/helm/atlas/values.yaml`'s `tracing.*` block wires `ATLAS_OTEL_EXPORTER_ENDPOINT`/
`ATLAS_OTEL_SAMPLE_RATIO` for a chart-managed deployment.
