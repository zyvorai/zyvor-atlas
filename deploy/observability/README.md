<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Atlas observability bundle

A self-contained **Prometheus + Grafana + Jaeger** stack for the Atlas control plane. No
Prometheus Operator / CRDs required — plain Deployments + a NodePort each.

## What it scrapes
- **`atlas-gateway-ceph:5110/metrics`** — Atlas's own operational gauges (`atlas_volumes`,
  `atlas_capacity_{raw,used}_bytes`, `atlas_jobs{state}`, `atlas_alerts_open`, …), emitted by the
  gateway's `GET /metrics` endpoint.
- **`rook-ceph-mgr:9283/metrics`** — the Ceph mgr Prometheus module (`ceph_*`).

## Deploy
```bash
deploy/observability/up.sh     # kubectl must target the cluster running atlas-gateway-ceph
```
Then:
- **Prometheus** → `http://<node>:30514` — check *Status → Targets*; `atlas-gateway` should be `UP`.
- **Grafana** → `http://<node>:30515` (admin / `atlas`) → *Dashboards → Atlas → “Atlas Storage
  Control Plane”* — capacity gauge, resource counts, jobs-by-state, open alerts, Ceph client IOPS.
- **Jaeger** → `http://<node>:30517` — empty until atlas-gateway runs with
  `ATLAS_OTEL_EXPORTER_ENDPOINT=http://<node>:30516` (see `docs/TRACING.md`).

## Alerting rules (in `prometheus.yaml`)
- `AtlasCapacityHigh` — used/raw > 85% for 5m.
- `AtlasOpenAlerts` — `atlas_alerts_open > 0` for 5m.
- `AtlasControlPlaneDown` — `up{job="atlas-gateway"} == 0` for 2m.

These complement the in-app monitor rules (cluster/pool/OSD/capacity-forecast) that Atlas already
evaluates into `storage_alerts`; the `atlas_alerts_open` gauge re-exports that count to Prometheus.

## Files
- `prometheus.yaml` — config + rules ConfigMap, Deployment, NodePort 30514.
- `grafana.yaml` — datasource + dashboard provisioning, Deployment, NodePort 30515.
- `atlas-overview.json` — the Grafana dashboard (loaded into a ConfigMap by `up.sh`).
- `jaeger.yaml` — all-in-one Jaeger (OTLP/HTTP collector + query UI), NodePorts 30516/30517. See
  `docs/TRACING.md`.
- `up.sh` — apply everything + wait for rollout.
