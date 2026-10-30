#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Live ransomware test (real encrypting program over a FUSE mount) on the lab host; same shape as
# lab-verify-remote.sh but the builder container gets /dev/fuse.
#
# Usage: scripts/lab-ransomware-remote.sh <host> <user> [baseline-minutes]
set -euo pipefail
HOST="${1:?usage: lab-ransomware-remote.sh <host> <user> [baseline-minutes]}"
USER_="${2:?}"
BASELINE="${3:-8}"
case "$BASELINE" in ''|*[!0-9]*) echo "baseline-minutes must be an integer" >&2; exit 2 ;; esac
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

log "build binaries and run the ransomware test (baseline ${BASELINE} min)"
$SSH "cd ~/${DIR} && podman build --target builder -t atlas-ci-builder -f Dockerfile . >/tmp/atlas-ci-builder.log 2>&1 || { tail -30 /tmp/atlas-ci-builder.log; exit 1; }
  podman run --rm --network=host --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
    -e BASELINE_MINUTES=${BASELINE} -e SKIP_CONTROL=${SKIP_CONTROL:-0} -e CONTROL_ONLY=${CONTROL_ONLY:-0} -v ${CACHE}/target:/build/target -v ${CACHE}/cargo:/usr/local/cargo/registry atlas-ci-builder bash -c '
    set -e
    command -v curl python3 fusermount3 >/dev/null || { apt-get update -qq && apt-get install -y -qq curl python3 fuse3 >/dev/null; }
    cargo build -p atlas-native -p atlas-gateway -p atlasctl 2>&1 | tail -3
    cargo build -p atlas-native-fuse --features fuse --bin atlas-native-mount 2>&1 | tail -3
    set +e
    bash deploy/native/lab-ransomware.sh
  '"
