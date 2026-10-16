#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Install Envoy Gateway (the Gateway API implementation Atlas's chart targets) and the `eg`
# GatewayClass. Additive: it creates the `envoy-gateway-system` namespace, the Gateway API + Envoy
# Gateway CRDs and one controller Deployment. It does not touch existing Ingress objects or workloads.
#
# Usage: deploy/envoy-lab/up.sh [--dry-run]     (run where kubectl/helm point at the target cluster)
set -euo pipefail
VERSION="${ENVOY_GATEWAY_VERSION:-v1.9.2}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ -r "$HOME/.kube/config" ]]; then export KUBECONFIG="${KUBECONFIG:-$HOME/.kube/config}"; fi
DRY=()
[[ "${1:-}" == "--dry-run" ]] && DRY=(--dry-run)

echo "==> Envoy Gateway ${VERSION}"
helm upgrade --install eg oci://docker.io/envoyproxy/gateway-helm --version "${VERSION}" \
  -n envoy-gateway-system --create-namespace "${DRY[@]}"
if [[ ${#DRY[@]} -eq 0 ]]; then
  kubectl -n envoy-gateway-system rollout status deploy/envoy-gateway --timeout=180s
  kubectl apply -f "${HERE}/gatewayclass.yaml"
  kubectl wait --for=condition=Accepted gatewayclass/eg --timeout=60s
  echo "==> ready. Enable in the chart: --set envoy.enabled=true --set envoy.host=<hostname>"
fi
