#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Live verification of retention locks, ransomware assessment and data reduction on the lab host:
# rsync the tree, build the binaries in the Dockerfile's builder image, and run
# deploy/native/lab-verify.sh there as plain host-network processes (no Kubernetes pod slot, nothing
# outside its own temp directory). Cargo caches live on the data disk, as in ci-remote.sh.
#
# Usage: scripts/lab-verify-remote.sh <host> <user> [baseline-minutes]
set -euo pipefail
HOST="${1:?usage: lab-verify-remote.sh <host> <user> [baseline-minutes]}"
USER_="${2:?}"
BASELINE="${3:-8}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SSH="ssh ${USER_}@${HOST}"
DIR=.deployment/atlas-ci
CACHE="${ATLAS_CI_CACHE_DIR:-/data/atlas-ci}"
log() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

log "rsync repo -> ${USER_}@${HOST}:~/${DIR}"
$SSH "mkdir -p ~/${DIR} '${CACHE}/target' '${CACHE}/cargo'"
rsync -az --delete \
  --exclude target --exclude .git --exclude '*.db' --exclude '*.db-wal' --exclude '*.db-shm' \
  --exclude node_modules --exclude '**/node_modules' --exclude dist --exclude '**/ui/dist' \
  --exclude .docusaurus --exclude website/build \
  "${ROOT}/" "${USER_}@${HOST}:~/${DIR}/"

log "build binaries and run the live checks (baseline ${BASELINE} min)"
$SSH "cd ~/${DIR} && podman build --target builder -t atlas-ci-builder -f Dockerfile . >/tmp/atlas-ci-builder.log 2>&1 || { tail -30 /tmp/atlas-ci-builder.log; exit 1; }
  podman run --rm --network=host -e BASELINE_MINUTES=${BASELINE} -v ${CACHE}/target:/build/target -v ${CACHE}/cargo:/usr/local/cargo/registry atlas-ci-builder bash -c '
    set -e
    command -v curl python3 >/dev/null || { apt-get update -qq && apt-get install -y -qq curl python3 >/dev/null; }
    curl --version | head -1
    cargo build -p atlas-native -p atlas-native-s3 -p atlas-gateway -p atlasctl 2>&1 | tail -3
    set +e
    bash deploy/native/lab-verify.sh
  '"
