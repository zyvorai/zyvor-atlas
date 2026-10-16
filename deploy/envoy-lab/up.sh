#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Install Envoy Gateway (the Gateway API implementation Atlas's chart targets) and the `eg`
# GatewayClass. Additive: it creates the `envoy-gateway-system` namespace, the Gateway API + Envoy
# Gateway CRDs and one controller Deployment. It does not touch existing Ingress objects or workloads.
#
# Usage: deploy/envoy-lab/up.sh [--dry-run] [--if-missing]   (run where kubectl/helm point at the target cluster)
#   --if-missing  do nothing when the cluster already has an Envoy Gateway controller (a GatewayClass whose
#                 controller is gateway.envoyproxy.io); used by deploy-remote.sh so re-runs never touch it.
set -euo pipefail
VERSION="${ENVOY_GATEWAY_VERSION:-v1.9.2}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ -r "$HOME/.kube/config" ]]; then export KUBECONFIG="${KUBECONFIG:-$HOME/.kube/config}"; fi
DRY=()
IF_MISSING=0
for a in "$@"; do
  [[ "$a" == "--dry-run" ]] && DRY=(--dry-run)
  [[ "$a" == "--if-missing" ]] && IF_MISSING=1
done

if [[ "$IF_MISSING" == "1" ]] && \
   kubectl get gatewayclass -o jsonpath='{.items[*].spec.controllerName}' 2>/dev/null | grep -q 'gateway.envoyproxy.io'; then
  echo "==> Envoy Gateway already installed in this cluster, leaving it as is"
  exit 0
fi

echo "==> Envoy Gateway ${VERSION}"
helm upgrade --install eg oci://docker.io/envoyproxy/gateway-helm --version "${VERSION}" \
  -n envoy-gateway-system --create-namespace "${DRY[@]}"
if [[ ${#DRY[@]} -eq 0 ]]; then
  kubectl -n envoy-gateway-system rollout status deploy/envoy-gateway --timeout=180s
  kubectl apply -f "${HERE}/gatewayclass.yaml"
  kubectl wait --for=condition=Accepted gatewayclass/eg --timeout=60s
  echo "==> ready. Enable in the chart: --set envoy.enabled=true --set envoy.host=<hostname>"
fi
