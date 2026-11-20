#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Install/upgrade the Helm chart on the lab host next to the raw-manifest gateway, using the image
# scripts/deploy-remote.sh already imported into k3s (localhost/atlas-gateway:dev). See
# deploy/helm/atlas/values-lab.yaml. Never prints Secret contents.
#
# Usage: scripts/helm-lab-remote.sh <host> <user> [extra helm args, e.g. --set authRequired=true]
#        scripts/helm-lab-remote.sh <host> <user> --uninstall
set -euo pipefail
HOST="${1:?usage: helm-lab-remote.sh <host> <user> [helm args]}"
USER_="${2:?usage: helm-lab-remote.sh <host> <user> [helm args]}"
shift 2
REL=atlas-helm
NS=atlas-helm
SSH="ssh ${USER_}@${HOST}"

if [[ "${1:-}" == "--uninstall" ]]; then
  $SSH "sudo helm --kubeconfig /etc/rancher/k3s/k3s.yaml uninstall ${REL} -n ${NS}"
  exit 0
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REMOTE_DIR="/tmp/atlas-helm-chart"
$SSH "rm -rf ${REMOTE_DIR} && mkdir -p ${REMOTE_DIR}"
scp -qr "${ROOT}/deploy/helm/atlas" "${USER_}@${HOST}:${REMOTE_DIR}/"
$SSH "sudo helm --kubeconfig /etc/rancher/k3s/k3s.yaml upgrade --install ${REL} ${REMOTE_DIR}/atlas \
  -n ${NS} --create-namespace -f ${REMOTE_DIR}/atlas/values-lab.yaml $* --wait --timeout 300s"
$SSH "sudo k3s kubectl -n ${NS} get deploy,svc,pvc"
