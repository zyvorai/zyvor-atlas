#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# KV-cache I/O smoke test on the lab host: rsync the tree, build the binaries in the Dockerfile's builder
# image, and run deploy/native/lab-kvcache.sh there as plain host-network processes with /dev/fuse (a real
# throwaway native cluster, an atlas-native-mount FUSE mount and the S3 gateway). Same shape as
# lab-ransomware-remote.sh. It shares ~/.deployment/atlas-ci with ci-remote.sh, so never run both at once.
#
# Usage: scripts/lab-kvcache-remote.sh <host> <user>
set -euo pipefail
HOST="${1:?usage: lab-kvcache-remote.sh <host> <user>}"
USER_="${2:?}"
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

log "build binaries and run the KV-cache smoke test"
$SSH "cd ~/${DIR} && podman build --target builder -t atlas-ci-builder -f Dockerfile . >/tmp/atlas-ci-builder.log 2>&1 || { tail -30 /tmp/atlas-ci-builder.log; exit 1; }
  podman run --rm --network=host --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
    -e BLOCKS=${BLOCKS:-64} -e BLOCK_KIB=${BLOCK_KIB:-2048} -e READERS=${READERS:-8} \
    -v ${CACHE}/target:/build/target -v ${CACHE}/cargo:/usr/local/cargo/registry atlas-ci-builder bash -c '
    set -e
    command -v curl python3 fusermount3 >/dev/null || { apt-get update -qq && apt-get install -y -qq curl python3 fuse3 >/dev/null; }
    cargo build -p atlas-native -p atlas-native-s3 -p atlas-gateway -p atlasctl 2>&1 | tail -3
    cargo build -p atlas-native-fuse --features fuse --bin atlas-native-mount 2>&1 | tail -3
    set +e
    bash deploy/native/lab-kvcache.sh
  '"
