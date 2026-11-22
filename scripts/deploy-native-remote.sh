#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
# Build and deploy atlas-native-node (deploy/k8s/atlas-native.yaml) to a remote k3s host.
#
#   ./scripts/deploy-native-remote.sh <host> [user] [--verify-failover]
#
# 1. rsync the repo to <user>@<host>:~/.deployment/atlas
# 2. podman build Dockerfile.native on the remote
# 3. tag the image with its content id and import it into k3s containerd (skipped if present)
# 4. ensure the namespace and API token Secret, then apply the manifest with the image pinned to
#    that content tag and the pod template stamped with a hash of the manifest
# 5. wait for the StatefulSet, then write and read a block through the leader over the HTTP API
#
# Step 4 never restarts pods unconditionally: `kubectl apply` only rolls the StatefulSet when the
# image content or the manifest changed, so re-running this script against a healthy cluster is a
# no-op (see CLAUDE.md "Deploy script gotchas").
#
# --verify-failover additionally deletes the leader pod and checks a new leader serves the block.
set -euo pipefail

HOST="${1:-${DEPLOY_HOST:-}}"
USER="${2:-${DEPLOY_USER:-sus}}"
[[ "$USER" == --* ]] && USER="${DEPLOY_USER:-sus}"
VERIFY_FAILOVER=0
for a in "$@"; do [[ "$a" == "--verify-failover" ]] && VERIFY_FAILOVER=1; done
[[ -z "$HOST" ]] && { echo "usage: $0 <host> [user] [--verify-failover]" >&2; exit 2; }

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REMOTE_DIR=".deployment/atlas"
SSH="ssh -o StrictHostKeyChecking=accept-new ${USER}@${HOST}"
NS="atlas-native"
MANIFEST="deploy/k8s/atlas-native.yaml"
# shellcheck disable=SC2016  # expanded by the remote shell
REMOTE_KUBE='if [ -r "$HOME/.kube/config" ]; then export KUBECONFIG="$HOME/.kube/config"; fi'

log() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

log "1/5 rsync repo -> ${USER}@${HOST}:~/${REMOTE_DIR}"
$SSH "mkdir -p ~/${REMOTE_DIR}"
rsync -az --delete \
  --exclude target --exclude .git --exclude node_modules --exclude '**/node_modules' \
  --exclude dist --exclude '**/ui/dist' --exclude .docusaurus --exclude website/build \
  -e "ssh -o StrictHostKeyChecking=accept-new" \
  "${HERE}/" "${USER}@${HOST}:${REMOTE_DIR}/"

log "2/5 build image with podman on remote"
$SSH "cd ~/${REMOTE_DIR} && podman build --ulimit nofile=65536:65536 -t atlas-native-node:dev -f Dockerfile.native ."

log "3/5 tag by content id + import into k3s containerd"
TAG="$($SSH "podman image inspect --format '{{.Id}}' atlas-native-node:dev" | cut -c1-12)"
IMAGE="localhost/atlas-native-node:${TAG}"
$SSH "set -e
  podman tag atlas-native-node:dev ${IMAGE}
  if sudo k3s ctr images ls -q | grep -qx 'docker.io/${IMAGE}\|${IMAGE}'; then
    echo 'image ${IMAGE} already imported'
  else
    podman save --format oci-archive -o /tmp/atlas-native-node.tar ${IMAGE}
    sudo k3s ctr images import /tmp/atlas-native-node.tar
    rm -f /tmp/atlas-native-node.tar
  fi"

log "4/5 namespace + API token Secret + apply (image ${IMAGE})"
SHA="$( (cat "${HERE}/${MANIFEST}"; echo "${TAG}") | shasum -a 256 | cut -c1-16)"
$SSH "${REMOTE_KUBE}; set -e; cd ~/${REMOTE_DIR}
  kubectl create namespace ${NS} --dry-run=client -o yaml | kubectl apply -f -
  if ! kubectl -n ${NS} get secret atlas-native-api >/dev/null 2>&1; then
    kubectl -n ${NS} create secret generic atlas-native-api --from-literal=token=\$(openssl rand -hex 32)
  fi
  sed -e 's|image: localhost/atlas-native-node:dev|image: ${IMAGE}|' \
      -e 's|atlas.zyvor.ai/manifest-sha: \"unset\"|atlas.zyvor.ai/manifest-sha: \"${SHA}\"|' \
      ${MANIFEST} | kubectl apply -f -
  kubectl -n ${NS} rollout status statefulset/atlas-native --timeout=300s"

log "5/5 verify through the leader"
$SSH "${REMOTE_KUBE}; VERIFY_FAILOVER=${VERIFY_FAILOVER} NS=${NS} bash -s" <<'REMOTE'
set -euo pipefail
TOKEN=$(kubectl -n "$NS" get secret atlas-native-api -o jsonpath='{.data.token}' | base64 -d)
PF_PIDS=()
trap 'for p in "${PF_PIDS[@]}"; do kill "$p" 2>/dev/null || true; done' EXIT

forward() { # pod local-port
  kubectl -n "$NS" port-forward "pod/$1" "$2:7480" >/dev/null 2>&1 &
  PF_PIDS+=($!)
  for _ in $(seq 1 50); do curl -sf "http://127.0.0.1:$2/healthz" >/dev/null && return 0; sleep 0.2; done
  echo "port-forward to $1 failed" >&2; return 1
}
api() { curl -sS -H "Authorization: Bearer $TOKEN" "$@"; }
leader_of() { # local-port -> leader pod name
  api "http://127.0.0.1:$1/v1/status" | sed -nE 's/.*"leader":"([^"]+)".*/\1/p'
}

forward atlas-native-0 17480
LEADER=""
for _ in $(seq 1 100); do LEADER=$(leader_of 17480); [ -n "$LEADER" ] && break; sleep 0.2; done
[ -n "$LEADER" ] || { echo "no leader" >&2; exit 1; }
echo "leader: $LEADER"
forward "$LEADER" 17481

VOL=$(api -X POST "http://127.0.0.1:17481/v1/volumes" -d '{"name":"deploy-check","size_bytes":4194304}' |
  sed -nE 's/.*"id":"([^"]+)".*/\1/p')
[ -n "$VOL" ] || { echo "volume create failed" >&2; exit 1; }
head -c 65536 /dev/urandom >/tmp/atlas-native-block
api -X PUT --data-binary @/tmp/atlas-native-block "http://127.0.0.1:17481/v1/volumes/$VOL/data?offset=0"
api -o /tmp/atlas-native-back "http://127.0.0.1:17481/v1/volumes/$VOL/data?offset=0&len=65536"
cmp /tmp/atlas-native-block /tmp/atlas-native-back
echo "write/read through $LEADER: ok (volume $VOL)"
api "http://127.0.0.1:17481/v1/status"; echo
LEADER_PORT=17481

if [ "$VERIFY_FAILOVER" = 1 ]; then
  kubectl -n "$NS" delete pod "$LEADER" --wait=false
  OTHER=$(for p in atlas-native-0 atlas-native-1 atlas-native-2; do [ "$p" = "$LEADER" ] || { echo "$p"; break; }; done)
  forward "$OTHER" 17482
  NEW=""
  for _ in $(seq 1 150); do
    NEW=$(leader_of 17482)
    [ -n "$NEW" ] && [ "$NEW" != "$LEADER" ] && break
    sleep 0.2
  done
  [ -n "$NEW" ] && [ "$NEW" != "$LEADER" ] || { echo "no new leader after deleting $LEADER" >&2; exit 1; }
  echo "new leader: $NEW"
  forward "$NEW" 17483
  api -o /tmp/atlas-native-back2 "http://127.0.0.1:17483/v1/volumes/$VOL/data?offset=0&len=65536"
  cmp /tmp/atlas-native-block /tmp/atlas-native-back2
  echo "read after failover via $NEW: ok"
  LEADER_PORT=17483
  kubectl -n "$NS" rollout status statefulset/atlas-native --timeout=300s
fi
curl -sf "http://127.0.0.1:$LEADER_PORT/metrics" | grep -E '^atlas_native_(raft_role\{.*"leader"\}|volumes |node_up)' || true
rm -f /tmp/atlas-native-block /tmp/atlas-native-back /tmp/atlas-native-back2
REMOTE

log "done. atlas-native in namespace ${NS} (in-cluster API: atlas-native-api.${NS}.svc:7480)"
