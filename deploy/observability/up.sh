#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Stand up a self-contained Prometheus + Grafana that scrape/visualize the Atlas control plane.
# Requires kubectl pointed at the cluster running atlas-gateway-ceph (namespace rook-ceph).
set -euo pipefail
cd "$(dirname "$0")"
NS=rook-ceph

echo "==> Prometheus (scrapes atlas /metrics + ceph mgr)"
kubectl apply -f prometheus.yaml

echo "==> Grafana dashboard ConfigMap (from atlas-overview.json)"
kubectl -n "$NS" create configmap atlas-grafana-dashboard \
  --from-file=atlas-overview.json=atlas-overview.json \
  --dry-run=client -o yaml | kubectl apply -f -

echo "==> Grafana admin Secret (random password if missing)"
if ! kubectl -n "$NS" get secret atlas-grafana-admin >/dev/null 2>&1; then
  GW_PASS="$(openssl rand -base64 18 | tr -d '/+=' | head -c 24)"
  kubectl -n "$NS" create secret generic atlas-grafana-admin \
    --from-literal=admin-password="$GW_PASS"
  echo "    created atlas-grafana-admin (admin password printed once below)"
else
  GW_PASS="$(kubectl -n "$NS" get secret atlas-grafana-admin -o jsonpath='{.data.admin-password}' | base64 -d)"
  echo "    atlas-grafana-admin already exists — left alone"
fi

echo "==> Grafana (datasource + dashboard auto-provisioned)"
kubectl apply -f grafana.yaml

echo "==> Jaeger (OTLP trace collector + UI — see docs/TRACING.md)"
kubectl apply -f jaeger.yaml

echo "==> waiting for rollouts"
kubectl -n "$NS" rollout status deploy/atlas-prometheus --timeout=180s
kubectl -n "$NS" rollout status deploy/atlas-grafana --timeout=180s
kubectl -n "$NS" rollout status deploy/atlas-jaeger --timeout=180s

NODE_IP="$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')"
cat <<EOF

Observability is up:
  Prometheus  http://${NODE_IP}:30514   (targets: Status -> Targets; atlas-gateway should be UP)
  Grafana     http://${NODE_IP}:30515   (admin / <secret atlas-grafana-admin>)
  admin password: ${GW_PASS}
  Jaeger UI   http://${NODE_IP}:30517   (traces — only populated once atlas-gateway is run with
                                          ATLAS_OTEL_EXPORTER_ENDPOINT=http://${NODE_IP}:30516)
EOF
