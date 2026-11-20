#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Stand up a minimal HTTP log receiver in the lab k3s cluster, standing in for a SIEM's ingestion
# endpoint, to verify ATLAS_AUDIT_EXPORT_URL (crates/atlas-monitor/src/audit_export.rs) against a
# real network path — not just the unit test's in-process mock server. See receiver.py.
#
# Usage (run ON the lab host, or with kubectl already pointed at it):
#   ./up.sh
#
# What you get:
#   - Namespace zyvor-system (same namespace as the fake/k8s atlas-gateway; created if missing)
#   - ConfigMap siem-lab-receiver holding receiver.py
#   - Deployment/Service siem-lab-receiver on NodePort 30557
#
# This does NOT touch the live atlas-gateway's ATLAS_AUDIT_EXPORT_URL/ATLAS_AUDIT_RETENTION_DAYS —
# turning those on is a real, standing retention-policy change (rows older than the retention
# window get deleted every 6h), not something to flip on silently as a side effect of standing up
# a receiver to test against. Point a throwaway export_and_prune() call at this receiver's NodePort
# to verify the real HTTP path, then decide separately whether/when to enable it on a live gateway.
set -euo pipefail
cd "$(dirname "$0")"
NS=zyvor-system

log() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

log "namespace"
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -

log "ConfigMap (receiver.py)"
kubectl -n "$NS" create configmap siem-lab-receiver --from-file=receiver.py=./receiver.py \
  --dry-run=client -o yaml | kubectl apply -f -

log "Deployment + Service"
kubectl apply -f deployment.yaml
# ConfigMap volume mount — force a restart so an updated script is picked up (no-op if unchanged).
kubectl -n "$NS" rollout restart deploy/siem-lab-receiver
kubectl -n "$NS" rollout status deploy/siem-lab-receiver --timeout=120s

NODE_IP="$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')"
cat <<EOF

siem-lab-receiver is up:
  External:    http://${NODE_IP}:30557/
  In-cluster:  http://siem-lab-receiver.${NS}.svc:8080/

Verify it's reachable and logging:
  curl -s -X POST http://${NODE_IP}:30557/ -d '{"audit_logs":[]}' -H 'Content-Type: application/json'
  kubectl -n ${NS} logs deploy/siem-lab-receiver --tail=20

Point ATLAS_AUDIT_EXPORT_URL at the in-cluster address above to test against a real atlas-gateway
Deployment, or call atlas_monitor::audit_export::export_and_prune() directly against the external
address for a lighter-weight check (see docs/DAY2.md).
EOF
