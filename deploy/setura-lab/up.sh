#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
# Install Setura (the Gateway API controller + built-in proxy Atlas's chart targets) and the `setura`
# GatewayClass. Additive: it creates the `setura-system` namespace and one controller Deployment, plus the
# standard Gateway API CRDs when the cluster has none. It does not touch existing Ingress objects or workloads.
# Needs the `setura` CLI on PATH (https://github.com/zyvorai/zyvor-setura-gateway/releases).
#
# Usage: deploy/setura-lab/up.sh [--dry-run] [--if-missing]   (run where kubectl points at the target cluster)
#   --dry-run     print the manifests `setura gateway install` generates and apply nothing.
#   --if-missing  do nothing when the cluster already has a Setura controller (a GatewayClass whose
#                 controller is zyvor.dev/setura-gateway); used by deploy-remote.sh so re-runs never touch it.
set -euo pipefail
IMAGE="${SETURA_IMAGE:-ghcr.io/zyvorai/zyvor-setura-gateway:0.7.0}"
GATEWAY_API_VERSION="${GATEWAY_API_VERSION:-v1.4.0}"
if [[ -r "$HOME/.kube/config" ]]; then export KUBECONFIG="${KUBECONFIG:-$HOME/.kube/config}"; fi
DRY=0
IF_MISSING=0
for a in "$@"; do
  [[ "$a" == "--dry-run" ]] && DRY=1
  [[ "$a" == "--if-missing" ]] && IF_MISSING=1
done

command -v setura >/dev/null 2>&1 || { echo "setura is required but not installed" >&2; exit 1; }

if [[ "$DRY" == "1" ]]; then
  echo "==> Setura ${IMAGE} (dry run)"
  setura gateway install --image "${IMAGE}" --namespace setura-system
  exit 0
fi

if [[ "$IF_MISSING" == "1" ]] && \
   kubectl get gatewayclass -o jsonpath='{.items[*].spec.controllerName}' 2>/dev/null | grep -q 'zyvor.dev/setura-gateway'; then
  echo "==> Setura already installed in this cluster, leaving it as is"
  exit 0
fi

if ! kubectl get crd gateways.gateway.networking.k8s.io >/dev/null 2>&1; then
  echo "==> Gateway API ${GATEWAY_API_VERSION} standard CRDs"
  kubectl apply -f "https://github.com/kubernetes-sigs/gateway-api/releases/download/${GATEWAY_API_VERSION}/standard-install.yaml"
fi

echo "==> Setura ${IMAGE}"
setura gateway install --image "${IMAGE}" --namespace setura-system | kubectl apply -f -
kubectl -n setura-system rollout status deploy/setura-gateway --timeout=180s
kubectl wait --for=condition=Accepted gatewayclass/setura --timeout=60s
echo "==> ready. Enable in the chart: --set gateway.enabled=true --set gateway.host=<hostname>"
