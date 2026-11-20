#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Deploy the **real-Ceph** Atlas gateway (`atlas-gateway-ceph` in `rook-ceph`) to a
# remote k3s host. The sibling `deploy-remote.sh` builds the fake/k8s image
# (`Dockerfile`) and deploys to `zyvor-system`; THIS one builds `Dockerfile.ceph`
# (bundles the Ceph Squid client) and rolls out the rook-ceph deployment, whose pod
# runs `localhost/atlas-gateway:ceph` with `imagePullPolicy: Never`.
#
# What it does:
#   1. rsync the repo to <user>@<host>:~/.deployment/atlas
#   2. ensure podman (+ docker.io short-name registries) and build Dockerfile.ceph
#   3. import the image into k3s containerd
#   4. ensure ClusterRole / atlas-tls / atlas-gateway-auth
#   5. kubectl apply deploy/k8s/atlas-gateway-ceph.yaml + rollout restart/status
#
# Prereq: CephCluster Ready + ceph-csi-drivers installed so zyvor-rbd-prod can Bind
# (see deploy/rook-ceph-lab/up.sh --single-node and docs/DEPLOYMENT.md).
#
# Usage:
#   ./scripts/deploy-ceph-gateway-remote.sh <host> [user]
#   ./scripts/deploy-ceph-gateway-remote.sh 212.8.248.187 sus
#
# Env:
#   DEPLOY_HOST / DEPLOY_USER   Defaults when host/user omitted
#   ATLAS_NS                    Gateway namespace       (default rook-ceph)
#   ATLAS_DEPLOY                Deployment name         (default atlas-gateway-ceph)
#   ATLAS_SKIP_APPLY=1          Skip `kubectl apply` of the manifest (rollout only)
#   ATLAS_SSH_KEY / SSH_KEY     SSH identity (falls back to ~/.ssh/id_ed25519_hyper2kvm)
#   ATLAS_SSH_TIMEOUT           ssh ConnectTimeout secs (default 90)
set -euo pipefail

HOST="${1:-${DEPLOY_HOST:-}}"
USER="${2:-${DEPLOY_USER:-sus}}"
[[ -z "${HOST}" ]] && { echo "usage: $0 <host> [user]" >&2; exit 2; }

NS="${ATLAS_NS:-rook-ceph}"
DEPLOY="${ATLAS_DEPLOY:-atlas-gateway-ceph}"
MANIFEST="deploy/k8s/atlas-gateway-ceph.yaml"
REMOTE_DIR=".deployment/atlas"
TIMEOUT="${ATLAS_SSH_TIMEOUT:-90}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Prefer explicit ATLAS_SSH_KEY / SSH_KEY; fall back to the hyper2kvm lab key, then agent default.
SSH_KEY_FILE="${ATLAS_SSH_KEY:-${SSH_KEY:-}}"
if [[ -z "${SSH_KEY_FILE}" && -f "${HOME}/.ssh/id_ed25519_hyper2kvm" ]]; then
  SSH_KEY_FILE="${HOME}/.ssh/id_ed25519_hyper2kvm"
fi
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ConnectTimeout="${TIMEOUT}")
if [[ -n "${SSH_KEY_FILE}" ]]; then
  SSH_OPTS+=(-o IdentitiesOnly=yes -i "${SSH_KEY_FILE}")
fi
SSH=(ssh "${SSH_OPTS[@]}" "${USER}@${HOST}")
RSYNC_SSH="ssh ${SSH_OPTS[*]}"

log() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

log "1/4 rsync repo -> ${USER}@${HOST}:~/${REMOTE_DIR}"
"${SSH[@]}" "mkdir -p ~/${REMOTE_DIR}"
rsync -az --delete \
  --exclude target --exclude .git --exclude '*.db' --exclude '*.db-wal' --exclude '*.db-shm' \
  --exclude node_modules \
  -e "${RSYNC_SSH}" \
  "${HERE}/" "${USER}@${HOST}:${REMOTE_DIR}/"

log "2/4 ensure podman, then build atlas-gateway:ceph (Dockerfile.ceph) on remote"
"${SSH[@]}" bash -s <<'REMOTE'
set -euo pipefail
if ! command -v podman >/dev/null 2>&1; then
  echo "podman missing — installing via apt"
  sudo apt-get update -qq
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq podman
fi
# Ubuntu's podman defaults to empty unqualified-search-registries; docker.io is required.
if [[ ! -f /etc/containers/registries.conf.d/999-atlas-dockerio.conf ]]; then
  sudo tee /etc/containers/registries.conf.d/999-atlas-dockerio.conf >/dev/null <<'EOF'
unqualified-search-registries = ["docker.io"]
EOF
fi
cd ~/.deployment/atlas
podman build --ulimit nofile=65536:65536 -t atlas-gateway:ceph -f Dockerfile.ceph .
REMOTE

log "3/6 import image into k3s containerd"
# oci-archive: docker-archive import fails with "doesn't support modifying existing images"
# when content-store blobs already exist from a prior import of the same layers.
"${SSH[@]}" "cd ~/${REMOTE_DIR} && podman save --format oci-archive -o /tmp/atlas-gateway-ceph.tar atlas-gateway:ceph \
  && sudo k3s ctr images import /tmp/atlas-gateway-ceph.tar && rm -f /tmp/atlas-gateway-ceph.tar"

# ${MANIFEST}'s own ClusterRoleBinding (atlas-gateway-readonly-cephns) already
# targets the right ServiceAccount (rook-ceph:atlas-gateway) — it just
# references a ClusterRole named atlas-gateway-readonly that this script never
# created, because that ClusterRole is defined in the *sibling* deploy-remote.sh's
# manifest (deploy/k8s/atlas-gateway.yaml), which this script doesn't apply. On a
# cluster that only ever ran the real-Ceph path, the pod started with the
# ClusterRole missing and every k8s API call it made came back "clusterrole ...
# not found: Forbidden" — non-fatal (only the live k8s driver's PV/PVC reads
# degrade) but silent, and easy to miss on a fresh install.
log "4/6 ensure the shared atlas-gateway-readonly ClusterRole exists"
"${SSH[@]}" bash -s <<REMOTE
set -euo pipefail
cd ~/${REMOTE_DIR}
rm -rf /tmp/atlas-rbac-split
mkdir -p /tmp/atlas-rbac-split
csplit -z -s -f /tmp/atlas-rbac-split/doc- deploy/k8s/atlas-gateway.yaml '/^---\$/' '{*}'
for f in /tmp/atlas-rbac-split/doc-*; do
  if grep -q '^kind: ClusterRole\$' "\$f"; then
    sudo kubectl apply -f "\$f"
  fi
done
rm -rf /tmp/atlas-rbac-split
REMOTE

# ${MANIFEST}'s Deployment mounts a `tls` volume from a Secret named atlas-tls
# (deploy/k8s/atlas-gateway-ceph.yaml) that nothing creates — on a fresh
# install the pod crash-loops on "load TLS cert/key: ... No such file or
# directory" until someone notices and creates it by hand. Self-signed and
# idempotent: left alone once it exists, so re-running this script never
# rotates a cert a browser has already been told to trust.
log "5/7 ensure the atlas-tls secret exists (self-signed, HTTPS listener)"
"${SSH[@]}" bash -s <<REMOTE
set -euo pipefail
if ! sudo kubectl -n ${NS} get secret atlas-tls >/dev/null 2>&1; then
  tmpdir="\$(mktemp -d)"
  trap 'rm -rf "\$tmpdir"' EXIT
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout "\$tmpdir/tls.key" -out "\$tmpdir/tls.crt" \
    -days 365 -subj "/CN=atlas-gateway" >/dev/null 2>&1
  sudo kubectl -n ${NS} create secret tls atlas-tls \
    --cert="\$tmpdir/tls.crt" --key="\$tmpdir/tls.key"
else
  echo "atlas-tls already exists — left alone"
fi
REMOTE

# Auth Secret: strong JWT signing key + optional bootstrap admin bearer. Required because the
# Ceph gateway Deployment sets ATLAS_AUTH_REQUIRED=1 and refuses to start on the weak default.
log "6/7 ensure atlas-gateway-auth Secret (jwt-secret + bootstrap-admin-token)"
"${SSH[@]}" "cd ~/${REMOTE_DIR} && NAMESPACE=${NS} KUBECTL='sudo kubectl' bash scripts/ensure-atlas-auth-secret.sh"

log "7/7 apply manifest + roll out ${DEPLOY} in ${NS}"
apply_changed=1
if [[ "${ATLAS_SKIP_APPLY:-0}" != "1" ]]; then
  apply_out="$("${SSH[@]}" "cd ~/${REMOTE_DIR} && sudo kubectl apply -f ${MANIFEST}")"
  echo "$apply_out"
  # `kubectl apply` on an already-applied, unchanged manifest reports every
  # object "unchanged" and is itself a no-op — an unconditional rollout
  # restart after it, on every single re-run of this script (including the
  # idempotent re-runs hypercluster's own `storage apply` does), needlessly
  # bounces a healthy pod every time. That repeated churn is what interrupts
  # this deployment's in-flight RBD volume mount (see ../zeus-os
  # docs/deploy-notes-*.md for the Ceph data-loss cascade one such interrupted
  # mount caused) — restart only when the deployment's own manifest line
  # actually changed something.
  if echo "$apply_out" | grep -qE "^deployment\.apps/${DEPLOY} unchanged$"; then
    apply_changed=0
  fi
fi
if [[ "$apply_changed" == "1" ]]; then
  "${SSH[@]}" "sudo kubectl -n ${NS} rollout restart deploy/${DEPLOY} \
    && sudo kubectl -n ${NS} rollout status deploy/${DEPLOY} --timeout=180s"
else
  log "manifest unchanged — skipping rollout restart (pod left running)"
  "${SSH[@]}" "sudo kubectl -n ${NS} rollout status deploy/${DEPLOY} --timeout=180s"
fi

BOOT="$("${SSH[@]}" "sudo kubectl -n ${NS} get secret atlas-gateway-auth -o jsonpath='{.data.bootstrap-admin-token}' 2>/dev/null | base64 -d || true")"
log "done — verify: curl -s -H \"Authorization: Bearer <bootstrap-or-jwt>\" http://<node>:30511/api/atlas/v1/volumes?kind=block"
if [[ -n "$BOOT" ]]; then
  echo "  bootstrap token still present in secret/${NS}/atlas-gateway-auth (mint JWTs, then remove it)"
fi
