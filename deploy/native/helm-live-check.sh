#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
#
# Installs deploy/helm/atlas-native on the current kube context with Raft/data mutual TLS and
# HTTPS + client-certificate auth (throwaway openssl PKI), then checks: auth refusals, unaligned
# I/O through the leader, leader-pod failover, that an unchanged `helm upgrade` restarts no
# pods, and scaling 3 -> 5 -> 3 through Raft membership changes without losing data. Run from the repo root on a host with kubectl, helm, openssl and curl.
#
#   deploy/native/helm-live-check.sh <image-repository> <image-tag> [namespace]
set -euo pipefail

REPO=${1:?image repository}
TAG=${2:?image tag}
NS=${3:-atlas-native-helm}
REL=atlas-native
PKI=$(mktemp -d)
PF_PIDS=()
cleanup() {
  for p in "${PF_PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$PKI"
}
trap cleanup EXIT

leaf() { # name sans eku
  openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=$1" \
    -keyout "$PKI/$1.key" -out "$PKI/$1.csr" 2>/dev/null
  printf 'subjectAltName=%s\nextendedKeyUsage=%s\nbasicConstraints=CA:FALSE\n' "$2" "$3" >"$PKI/$1.ext"
  openssl x509 -req -in "$PKI/$1.csr" -CA "$PKI/ca.crt" -CAkey "$PKI/ca.key" -CAcreateserial \
    -days 2 -extfile "$PKI/$1.ext" -out "$PKI/$1.crt" 2>/dev/null
}
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 -subj "/CN=atlas-native-test-ca" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
  -keyout "$PKI/ca.key" -out "$PKI/ca.crt" 2>/dev/null
leaf raft "DNS:$REL-0,DNS:$REL-1,DNS:$REL-2,DNS:$REL-3,DNS:$REL-4" "serverAuth,clientAuth"
leaf http "DNS:localhost,DNS:$REL-api.$NS.svc" "serverAuth"
leaf client "DNS:ops" "clientAuth"

# Every run issues a new PKI, which pods of an earlier run would not trust: start clean.
kubectl delete namespace "$NS" --ignore-not-found --wait --timeout=300s >/dev/null
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
for s in raft http; do
  kubectl -n "$NS" create secret generic "$REL-$s-tls" --from-file=ca.crt="$PKI/ca.crt" \
    --from-file=tls.crt="$PKI/$s.crt" --from-file=tls.key="$PKI/$s.key" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
done

install() {
  helm upgrade --install "$REL" deploy/helm/atlas-native -n "$NS" --wait --timeout 5m \
    --set image.repository="$REPO",image.tag="$TAG",image.pullPolicy=Never \
    --set tls.enabled=true,tls.existingSecret="$REL-raft-tls" \
    --set httpTls.enabled=true,httpTls.existingSecret="$REL-http-tls",httpTls.requireClientCert=true \
    --set persistence.size=1Gi "$@" >/dev/null
}
install
kubectl -n "$NS" rollout status statefulset/$REL --timeout=300s
echo "helm install: ok"

TOKEN=$(kubectl -n "$NS" get secret "$REL-api" -o jsonpath='{.data.token}' | base64 -d)
forward() { # pod local-port
  kubectl -n "$NS" port-forward "pod/$1" "$2:7480" >/dev/null 2>&1 &
  PF_PIDS+=($!)
  for _ in $(seq 1 50); do
    curl -sf --cacert "$PKI/ca.crt" "https://localhost:$2/healthz" >/dev/null && return 0
    sleep 0.2
  done
  echo "port-forward to $1 failed" >&2
  return 1
}
url() { echo "https://localhost:$1$2"; }
call() { # port path [curl args...]
  local port=$1 path=$2
  shift 2
  curl -sS --cacert "$PKI/ca.crt" --cert "$PKI/client.crt" --key "$PKI/client.key" \
    -H "Authorization: Bearer $TOKEN" "$@" "$(url "$port" "$path")"
}
leader_of() { call "$1" /v1/status | sed -nE 's/.*"leader":"([^"]+)".*/\1/p'; }

forward "$REL-0" 18480
code=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$PKI/ca.crt" -H "Authorization: Bearer $TOKEN" "$(url 18480 /v1/status)")
[ "$code" = 401 ] || { echo "no client cert: want 401, got $code" >&2; exit 1; }
echo "no client certificate -> 401: ok"
code=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$PKI/ca.crt" "$(url 18480 /readyz)")
[ "$code" = 200 ] || { echo "readyz without cert: $code" >&2; exit 1; }
echo "readyz without client certificate -> 200: ok"

LEADER=""
for _ in $(seq 1 100); do LEADER=$(leader_of 18480); [ -n "$LEADER" ] && break; sleep 0.2; done
[ -n "$LEADER" ] || { echo "no leader" >&2; exit 1; }
echo "leader: $LEADER (raft over mTLS)"
forward "$LEADER" 18481
VOL=$(call 18481 /v1/volumes -X POST -d '{"name":"helm-check","size_bytes":16777216}' |
  sed -nE 's/.*"id":"([^"]+)".*/\1/p')
[ -n "$VOL" ] || { echo "volume create failed" >&2; exit 1; }
head -c 100000 /dev/urandom >"$PKI/block"
call 18481 "/v1/volumes/$VOL/data?offset=4194000" -X PUT --data-binary @"$PKI/block"
call 18481 "/v1/volumes/$VOL/data?offset=4194000&len=100000" -o "$PKI/back"
cmp "$PKI/block" "$PKI/back"
echo "unaligned write/read across an extent boundary via $LEADER: ok"

kubectl -n "$NS" delete pod "$LEADER" --wait=false >/dev/null
OTHER=$(for i in 0 1 2; do [ "$REL-$i" = "$LEADER" ] || { echo "$REL-$i"; break; }; done)
forward "$OTHER" 18482
NEW=""
for _ in $(seq 1 150); do
  NEW=$(leader_of 18482 || true)
  [ -n "$NEW" ] && [ "$NEW" != "$LEADER" ] && break
  sleep 0.2
done
[ -n "$NEW" ] && [ "$NEW" != "$LEADER" ] || { echo "no new leader" >&2; exit 1; }
forward "$NEW" 18483
call 18483 "/v1/volumes/$VOL/data?offset=4194000&len=100000" -o "$PKI/back2"
cmp "$PKI/block" "$PKI/back2"
echo "failover $LEADER -> $NEW, read back: ok"
kubectl -n "$NS" rollout status statefulset/$REL --timeout=300s >/dev/null

before=$(kubectl -n "$NS" get pods -l app.kubernetes.io/instance=$REL -o jsonpath='{range .items[*]}{.metadata.uid} {end}')
install
after=$(kubectl -n "$NS" get pods -l app.kubernetes.io/instance=$REL -o jsonpath='{range .items[*]}{.metadata.uid} {end}')
[ "$before" = "$after" ] || { echo "unchanged upgrade restarted pods" >&2; exit 1; }
echo "unchanged helm upgrade kept every pod: ok"

# Leader seen from pod $1 through local port $2, waiting out elections.
wait_leader() {
  local l=""
  for _ in $(seq 1 150); do
    l=$(leader_of "$2" || true)
    [ -n "$l" ] && { echo "$l"; return 0; }
    sleep 0.2
  done
  echo "no leader seen from $1" >&2
  return 1
}
voters() { # count
  local i sep="" out='{"voters":{'
  for i in $(seq 0 $(($1 - 1))); do
    out+="$sep\"$REL-$i\":\"$REL-$i.$REL.$NS.svc.cluster.local:7482\""
    sep=,
  done
  echo "$out}}"
}
change_members() { # count port
  local code
  for _ in $(seq 1 20); do
    L=$(wait_leader "$REL-0" "$2")
    forward "$L" $((PORT += 1))
    code=$(call "$PORT" /v1/members -X POST -d "$(voters "$1")" -o "$PKI/members" -w '%{http_code}')
    [ "$code" = 200 ] && return 0
    sleep 1
  done
  echo "membership change to $1 voters failed ($code): $(cat "$PKI/members")" >&2
  return 1
}
PORT=18500

install --set replicas=5
kubectl -n "$NS" rollout status statefulset/$REL --timeout=300s >/dev/null
forward "$REL-4" $((PORT += 1))
P4=$PORT
call "$P4" /v1/status | grep -q '"voter":false' || { echo "$REL-4 should start as a non-voter" >&2; exit 1; }
forward "$REL-0" $((PORT += 1))
P0=$PORT
change_members 5 "$P0"
for _ in $(seq 1 100); do call "$P4" /v1/status | grep -q '"voter":true' && break; sleep 0.2; done
call "$P4" /v1/status | grep -q '"voter":true' || { echo "$REL-4 was not promoted" >&2; exit 1; }
call "$PORT" /v1/members | grep -q "\"$REL-4\"" || { echo "members lack $REL-4" >&2; exit 1; }
L=$(wait_leader "$REL-0" "$P0")
forward "$L" $((PORT += 1))
call "$PORT" "/v1/volumes/$VOL/data?offset=0" -X PUT --data-binary @"$PKI/block"
call "$PORT" "/v1/volumes/$VOL/data?offset=0&len=100000" -o "$PKI/back4"
cmp "$PKI/block" "$PKI/back4"
echo "scaled to 5 pods, promoted $REL-3/$REL-4 to voters, wrote via $L: ok"

change_members 3 "$P0"
install --set replicas=3
kubectl -n "$NS" rollout status statefulset/$REL --timeout=300s >/dev/null
kubectl -n "$NS" delete pvc "state-$REL-3" "state-$REL-4" --wait=false >/dev/null 2>&1 || true
forward "$REL-0" $((PORT += 1))
P0=$PORT
L=$(wait_leader "$REL-0" "$P0")
forward "$L" $((PORT += 1))
want="\"voters\":[\"$REL-0\",\"$REL-1\",\"$REL-2\"]"
call "$PORT" /v1/members | grep -qF "$want" || { echo "voters not back to 0..2: $(call "$PORT" /v1/members)" >&2; exit 1; }
for _ in $(seq 1 20); do
  call "$PORT" /v1/repair -X POST -o "$PKI/repair" -w '%{http_code}' | grep -q 200 && break
  sleep 1
done
grep -q '"unrecoverable":0' "$PKI/repair" || { echo "repair: $(cat "$PKI/repair")" >&2; exit 1; }
for off in 0 4194000; do
  call "$PORT" "/v1/volumes/$VOL/data?offset=$off&len=100000" -o "$PKI/back5"
  cmp "$PKI/block" "$PKI/back5"
done
echo "scaled back to 3 voters/pods, repair re-replicated, data intact via $L: ok"
echo "helm live check: ok"
