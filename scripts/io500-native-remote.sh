#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Run IO500 (docs/IO500.md) against an atlas-native Helm release on a remote k3s host.
#
#   ./scripts/io500-native-remote.sh <host> [user] --namespace <ns> [--release <name>]
#       [--np N] [--stonewall SECS] [--ior-easy-block SIZE] [--ior-hard-segments N]
#       [--mdtest-easy-n N] [--mdtest-hard-n N] [--mount-args "ARGS"] [--node <k8s node>]
#
# 1. rsync the repo to <user>@<host>:~/.deployment/atlas and build Dockerfile.io500 with podman
# 2. import the image into k3s containerd (skipped if that content is already there)
# 3. create a scratch filesystem through the release's API Service and make its root writable
# 4. run one privileged pod (atlas-native-mount + IO500 under Open MPI) and stream its log
# 5. copy the result directory to ./io500-results/<timestamp>/, delete the pod and the filesystem
#
# Defaults are IO500's (stonewall 300 s, full-size phases). Smaller values give a quick functional
# run that IO500 itself marks INVALID; see docs/IO500.md before publishing any number.
set -euo pipefail

HOST="${1:-${DEPLOY_HOST:-}}"
[[ -z "$HOST" || "$HOST" == --* ]] && { echo "usage: $0 <host> [user] --namespace <ns> [...]" >&2; exit 2; }
shift
USER="${DEPLOY_USER:-sus}"
if [[ $# -gt 0 && "$1" != --* ]]; then USER="$1"; shift; fi

NS="" RELEASE="" NP="" STONEWALL="" IOR_EASY_BLOCK="" IOR_HARD_SEGMENTS="" MDTEST_EASY_N="" MDTEST_HARD_N=""
MOUNT_ARGS="" NODE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --namespace) NS="$2"; shift 2 ;;
    --release) RELEASE="$2"; shift 2 ;;
    --np) NP="$2"; shift 2 ;;
    --stonewall) STONEWALL="$2"; shift 2 ;;
    --ior-easy-block) IOR_EASY_BLOCK="$2"; shift 2 ;;
    --ior-hard-segments) IOR_HARD_SEGMENTS="$2"; shift 2 ;;
    --mdtest-easy-n) MDTEST_EASY_N="$2"; shift 2 ;;
    --mdtest-hard-n) MDTEST_HARD_N="$2"; shift 2 ;;
    --mount-args) MOUNT_ARGS="$2"; shift 2 ;;
    --node) NODE="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ -z "$NS" ]] && { echo "--namespace is required" >&2; exit 2; }
RELEASE="${RELEASE:-$NS}"
if [[ "$RELEASE" == *atlas-native* ]]; then FULL="$RELEASE"; else FULL="${RELEASE}-atlas-native"; fi

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REMOTE_DIR=".deployment/atlas"
SSH="ssh -o StrictHostKeyChecking=accept-new ${USER}@${HOST}"
# shellcheck disable=SC2016  # expanded by the remote shell
REMOTE_KUBE='if [ -r "$HOME/.kube/config" ]; then export KUBECONFIG="$HOME/.kube/config"; fi'
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
FS="io500-$(echo "$STAMP" | tr '[:upper:]' '[:lower:]')"
POD="$FS"

log() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

log "1/5 rsync repo -> ${USER}@${HOST}:~/${REMOTE_DIR} and build Dockerfile.io500"
$SSH "mkdir -p ~/${REMOTE_DIR}"
rsync -az --delete \
  --exclude target --exclude .git --exclude node_modules --exclude '**/node_modules' \
  --exclude dist --exclude '**/ui/dist' --exclude .docusaurus --exclude website/build \
  --exclude io500-results \
  -e "ssh -o StrictHostKeyChecking=accept-new" \
  "${HERE}/" "${USER}@${HOST}:${REMOTE_DIR}/"
$SSH "cd ~/${REMOTE_DIR} && podman build -t atlas-io500:dev -f Dockerfile.io500 ."

log "2/5 import image into k3s containerd"
TAG="$($SSH "podman image inspect --format '{{.Id}}' atlas-io500:dev" | cut -c1-12)"
IMAGE="localhost/atlas-io500:${TAG}"
$SSH "set -e
  podman tag atlas-io500:dev ${IMAGE}
  if sudo k3s ctr -n k8s.io images ls -q | grep -qx 'docker.io/${IMAGE}\|${IMAGE}'; then
    echo 'image ${IMAGE} already imported'
  else
    podman save --format oci-archive -o /tmp/atlas-io500.tar ${IMAGE}
    sudo k3s ctr -n k8s.io images import /tmp/atlas-io500.tar >/dev/null
    rm -f /tmp/atlas-io500.tar
  fi"

log "3-5/5 filesystem ${FS}, pod ${POD} in ${NS}"
mkdir -p "${HERE}/io500-results"
STATUS=0
$SSH "${REMOTE_KUBE}; NS='${NS}' FULL='${FULL}' FS='${FS}' POD='${POD}' IMAGE='${IMAGE}' NP='${NP}' \
  STONEWALL='${STONEWALL}' IOR_EASY_BLOCK='${IOR_EASY_BLOCK}' IOR_HARD_SEGMENTS='${IOR_HARD_SEGMENTS}' \
  MDTEST_EASY_N='${MDTEST_EASY_N}' MDTEST_HARD_N='${MDTEST_HARD_N}' MOUNT_ARGS='${MOUNT_ARGS}' \
  NODE='${NODE}' STAMP='${STAMP}' bash -s" <<'REMOTE' || STATUS=$?
set -euo pipefail
SECRET=$(kubectl -n "$NS" get statefulset "$FULL" -o jsonpath='{.spec.template.spec.volumes[?(@.name=="api-token")].secret.secretName}' 2>/dev/null || true)
SECRET="${SECRET:-${FULL}-api}"
TOKEN=$(kubectl -n "$NS" get secret "$SECRET" -o jsonpath='{.data.token}' | base64 -d)
IP=$(kubectl -n "$NS" get svc "${FULL}-api" -o jsonpath='{.spec.clusterIP}')
REPLICAS=$(kubectl -n "$NS" get statefulset "$FULL" -o jsonpath='{.spec.replicas}')
DOMAIN=$(kubectl -n "$NS" get statefulset "$FULL" -o jsonpath='{.spec.serviceName}')
TLS=$(kubectl -n "$NS" get statefulset "$FULL" -o jsonpath='{.spec.template.spec.volumes[?(@.name=="http-tls")].name}')
[ -z "$TLS" ] || { echo "httpTls releases are not supported by this script yet" >&2; exit 1; }
ENDPOINTS=""
for i in $(seq 0 $((REPLICAS - 1))); do
  ENDPOINTS="${ENDPOINTS:+$ENDPOINTS,}http://${FULL}-${i}.${DOMAIN}.${NS}.svc:7480"
done

api() { # method path [body] -> http code
  local code
  for _ in $(seq 1 30); do
    code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" -X "$1" \
      "http://${IP}:7480$2" ${3:+-d "$3"}) || code=000
    case "$code" in 2*|409) echo "$code"; return 0 ;; esac
    sleep 2
  done
  echo "$1 $2 failed: HTTP $code" >&2
  return 1
}
cleanup() {
  kubectl -n "$NS" delete pod "$POD" --ignore-not-found --wait=true --timeout=120s >/dev/null || true
  api DELETE "/v1/fs/${FS}" >/dev/null || true
}
trap cleanup EXIT

api POST /v1/fs "{\"name\":\"${FS}\",\"id\":\"${FS}\"}" >/dev/null
api POST "/v1/fs/${FS}/inodes/1/attr" '{"mode":511}' >/dev/null

env_item() { [ -n "$2" ] && printf '        - { name: %s, value: "%s" }\n' "$1" "$2" || true; }
{
  cat <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: ${POD}
  namespace: ${NS}
  labels: { app.kubernetes.io/name: atlas-io500 }
spec:
  restartPolicy: Never
  terminationGracePeriodSeconds: 60
EOF
  [ -n "$NODE" ] && printf '  nodeName: %s\n' "$NODE"
  cat <<EOF
  containers:
    - name: io500
      image: ${IMAGE}
      imagePullPolicy: Never
      securityContext: { privileged: true }
      env:
        - { name: ATLAS_NATIVE_ENDPOINTS, value: "${ENDPOINTS}" }
        - { name: ATLAS_NATIVE_TOKEN_FILE, value: /etc/atlas-io500/api/token }
        - { name: IO500_FS, value: "${FS}" }
        - { name: IO500_HOLD_SECS, value: "900" }
EOF
  env_item IO500_NP "$NP"
  env_item IO500_STONEWALL "$STONEWALL"
  env_item IO500_IOR_EASY_BLOCK "$IOR_EASY_BLOCK"
  env_item IO500_IOR_HARD_SEGMENTS "$IOR_HARD_SEGMENTS"
  env_item IO500_MDTEST_EASY_N "$MDTEST_EASY_N"
  env_item IO500_MDTEST_HARD_N "$MDTEST_HARD_N"
  env_item IO500_MOUNT_ARGS "$MOUNT_ARGS"
  cat <<EOF
      volumeMounts:
        - { name: fuse, mountPath: /dev/fuse }
        - { name: api-token, mountPath: /etc/atlas-io500/api, readOnly: true }
        - { name: results, mountPath: /results }
  volumes:
    - name: fuse
      hostPath: { path: /dev/fuse, type: CharDevice }
    - name: api-token
      secret: { secretName: ${SECRET} }
    - name: results
      emptyDir: {}
EOF
} | kubectl apply -f - >/dev/null

kubectl -n "$NS" wait --for=condition=Ready "pod/${POD}" --timeout=300s >/dev/null
kubectl -n "$NS" logs -f "pod/${POD}" &
LOGS=$!
until kubectl -n "$NS" exec "$POD" -- test -e /results/.done 2>/dev/null; do
  phase=$(kubectl -n "$NS" get pod "$POD" -o jsonpath='{.status.phase}')
  [ "$phase" = Running ] || { echo "pod ${POD} is ${phase}" >&2; break; }
  sleep 10
done
mkdir -p "$HOME/io500-results/${STAMP}"
kubectl -n "$NS" exec "$POD" -- tar -C /results -cf - . | tar -C "$HOME/io500-results/${STAMP}" -xf - || true
kill "$LOGS" 2>/dev/null || true
STATUS=$(cat "$HOME/io500-results/${STAMP}/.done" 2>/dev/null || echo 1)
exit "$STATUS"
REMOTE

rsync -az -e "ssh -o StrictHostKeyChecking=accept-new" \
  "${USER}@${HOST}:io500-results/${STAMP}/" "${HERE}/io500-results/${STAMP}/"
$SSH "rm -rf ~/io500-results/${STAMP}"
log "results in io500-results/${STAMP}/"
grep -h '^\[SCORE' "${HERE}/io500-results/${STAMP}"/*/result.txt 2>/dev/null || true
exit "$STATUS"
