#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Install the edge-side operators DataBridge needs to run a REAL cloud-to-edge DB migration:
#   - CloudNativePG           -> Postgres edge targets (data on Ceph RBD)
#   - Percona XtraDB Cluster  -> MySQL / MariaDB edge targets (data on Ceph RBD)
#   - Percona Server for MongoDB (PSMDB) -> MongoDB edge replica sets (oplog / change streams)
#   - Strimzi (Kafka)         -> runs Debezium (KafkaConnect) for CDC
#
# The DataBridge *control plane* (atlas-gateway) does NOT need these — its fake pipeline runs with
# none of them (see `make run-databridge`). Install these only to migrate against real cloud DBs.
#
# Requires: kubectl pointing at the target cluster, a Ceph RBD StorageClass (zyvor-rbd-prod from
# deploy/rook-ceph-lab). Idempotent: safe to re-run. Pin versions via env.
#
# Usage:
#   ./up.sh                 # CloudNativePG + Percona PXC + PSMDB + Strimzi
#   ./up.sh --pg-only       # just CloudNativePG (Postgres migrations only)
#   ./up.sh --no-streaming  # skip Strimzi/Kafka (full-load only, no CDC yet)
#   ./up.sh --no-mongo      # skip PSMDB operator
#   CNPG_VERSION=1.24.1 PXC_VERSION=1.14.0 PSMDB_VERSION=1.16.0 STRIMZI_CHANNEL=latest ./up.sh
set -euo pipefail

CNPG_VERSION="${CNPG_VERSION:-1.24.1}"
PXC_VERSION="${PXC_VERSION:-1.14.0}"
PSMDB_VERSION="${PSMDB_VERSION:-1.16.0}"
STRIMZI_CHANNEL="${STRIMZI_CHANNEL:-latest}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NS=zyvor-databridge

WITH_MYSQL=1
WITH_MONGO=1
WITH_STREAMING=1
for a in "$@"; do
  case "$a" in
    --pg-only) WITH_MYSQL=0; WITH_MONGO=0; WITH_STREAMING=0 ;;
    --no-streaming) WITH_STREAMING=0 ;;
    --no-mongo) WITH_MONGO=0 ;;
    *) echo "unknown flag: $a" >&2; exit 2 ;;
  esac
done

log() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }
require() { command -v "$1" >/dev/null 2>&1 || { echo "missing required tool: $1" >&2; exit 1; }; }
require kubectl

log "1/5 namespace"
kubectl apply -f "$HERE/00-namespace.yaml"

log "2/5 CloudNativePG operator ($CNPG_VERSION)"
cnpg_minor="${CNPG_VERSION%.*}"   # 1.24.1 -> 1.24
kubectl apply --server-side -f \
  "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/release-${cnpg_minor}/releases/cnpg-${CNPG_VERSION}.yaml"
kubectl -n cnpg-system rollout status deploy/cnpg-controller-manager --timeout=180s || true

if [[ "$WITH_MYSQL" == "1" ]]; then
  log "3/5 Percona XtraDB Cluster operator ($PXC_VERSION)"
  kubectl apply -f \
    "https://raw.githubusercontent.com/percona/percona-xtradb-cluster-operator/v${PXC_VERSION}/deploy/bundle.yaml" \
    -n "$NS"
else
  log "3/5 skipping MySQL operator (--pg-only)"
fi

if [[ "$WITH_MONGO" == "1" ]]; then
  log "4/5 Percona Server for MongoDB operator ($PSMDB_VERSION)"
  # Bundle installs CRDs + operator Deployment into the target namespace.
  # Server-side: the PSMDB CRD is larger than the 256 KiB last-applied annotation client-side
  # apply writes, so a plain apply fails on a fresh cluster and the operator crash-loops.
  kubectl apply --server-side -f \
    "https://raw.githubusercontent.com/percona/percona-server-mongodb-operator/v${PSMDB_VERSION}/deploy/bundle.yaml" \
    -n "$NS" || {
      echo "WARN: PSMDB bundle apply failed — check version pin / already-installed CRDs" >&2
    }
  kubectl -n "$NS" rollout status deploy/percona-server-mongodb-operator --timeout=180s || true
else
  log "4/5 skipping PSMDB operator (--no-mongo / --pg-only)"
fi

if [[ "$WITH_STREAMING" == "1" ]]; then
  log "5/5 Strimzi (Kafka for Debezium CDC)"
  kubectl create -f "https://strimzi.io/install/${STRIMZI_CHANNEL}?namespace=${NS}" -n "$NS" \
    2>/dev/null || kubectl apply -f "https://strimzi.io/install/${STRIMZI_CHANNEL}?namespace=${NS}" -n "$NS"
  kubectl -n "$NS" rollout status deploy/strimzi-cluster-operator --timeout=180s || true
  # let Kafka Connect resolve ${secrets:...} in connector configs
  kubectl apply -f "$HERE/connect-rbac.yaml"
  # Lab Kafka the gateway's start_cdc hard-codes (zyvor-kafka-kafka-bootstrap:9092).
  # Safe to re-apply; Strimzi will reconcile. Skip with --no-kafka if the cluster already has one.
  if [[ "${SKIP_KAFKA_CR:-0}" != "1" ]]; then
    log "applying Kafka CR zyvor-kafka (lab RF=1)"
    kubectl apply -f "$HERE/10-kafka.yaml"
  fi
else
  log "5/5 skipping Strimzi/Kafka (--no-streaming: full-load only, no CDC)"
fi

cat <<EOF

Done. Edge operators installed in namespace '$NS'. Verify with:
  kubectl -n cnpg-system get deploy cnpg-controller-manager
  kubectl -n $NS get deploy | grep -E 'percona|strimzi|mongodb'
  kubectl -n $NS get kafka zyvor-kafka
  kubectl get storageclass | grep zyvor-rbd-prod   # edge DB data lands here

Build the multi-engine Connect image (required for real CDC — includes MariaDB plugin):
  podman build -t databridge-connect:dev -f deploy/databridge/connect/Dockerfile deploy/databridge/connect
  # set ATLAS_DATABRIDGE_CONNECT_IMAGE on the gateway deployment

MySQL CDC note: prefer DATETIME columns; TIMESTAMP → Debezium ZonedTimestamp can break the JDBC sink.
Then point Atlas DataBridge at a real source (driver_mode=real, creds in a k8s Secret) and run
the pipeline from the Migration Plans page, or over REST at /api/atlas/v1/databridge/*.
EOF
