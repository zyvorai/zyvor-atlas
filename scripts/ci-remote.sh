#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Run Atlas's CI gates on the lab host instead of locally (nothing is compiled on the developer
# machine): rsync the tree, then inside the Dockerfile's own builder image run clippy (-D warnings),
# the whole Rust test suite, and — in a Node container — the UI lint, tests and production build.
# Mirrors .github/workflows/ci.yml's build-test and ui jobs (not cargo-deny / Postgres / coverage).
#
# Usage: scripts/ci-remote.sh <host> <user> [rust|ui|ceph]   (default: rust and ui)
#   ceph  additionally builds Dockerfile.ceph end to end, as CI's docker job does.
set -euo pipefail
HOST="${1:?usage: ci-remote.sh <host> <user> [rust|ui|ceph]}"
USER_="${2:?}"
WHAT="${3:-all}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SSH="ssh ${USER_}@${HOST}"
DIR=.deployment/atlas-ci
# Cargo's target dir for this workspace grows past 100 GB across feature sets. Keep it (and the registry
# cache) in a plain directory on a data disk, not in a podman named volume under the home directory:
# on the lab it once filled the root filesystem (150 GB), tripped the kubelet's disk-pressure eviction
# and took Ceph mons and a Kueue webhook down with it. Override with ATLAS_CI_CACHE_DIR.
CACHE="${ATLAS_CI_CACHE_DIR:-/data/atlas-ci}"
log() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

log "rsync repo -> ${USER_}@${HOST}:~/${DIR}"
$SSH "mkdir -p ~/${DIR} '${CACHE}/target' '${CACHE}/cargo'" || { echo "cannot create ${CACHE} on ${HOST} (set ATLAS_CI_CACHE_DIR to a writable data-disk path)" >&2; exit 1; }
rsync -az --delete \
  --exclude target --exclude .git --exclude '*.db' --exclude '*.db-wal' --exclude '*.db-shm' \
  --exclude node_modules --exclude '**/node_modules' --exclude dist --exclude '**/ui/dist' \
  --exclude .docusaurus --exclude website/build \
  "${ROOT}/" "${USER_}@${HOST}:~/${DIR}/"

if [[ "$WHAT" == "all" || "$WHAT" == "rust" ]]; then
  log "Rust: clippy (-D warnings) + workspace tests in the Dockerfile builder image"
  $SSH "cd ~/${DIR} && podman build --target builder -t atlas-ci-builder -f Dockerfile . >/tmp/atlas-ci-builder.log 2>&1 || { tail -30 /tmp/atlas-ci-builder.log; exit 1; }
    podman run --rm -v ${CACHE}/target:/build/target -v ${CACHE}/cargo:/usr/local/cargo/registry atlas-ci-builder bash -c 'rustup component add clippy >/dev/null 2>&1; cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -60; echo CLIPPY_EXIT=\${PIPESTATUS[0]}; cargo test --workspace 2>&1 | grep -E \"^test result|FAILED|failed|panicked|error(\\[|:)\" | tail -60; echo TEST_EXIT=\${PIPESTATUS[0]}'"
fi

if [[ "$WHAT" == "all" || "$WHAT" == "ui" ]]; then
  log "UI: lint, tests, production build in a Node container"
  $SSH "cd ~/${DIR}/crates/atlas-gateway/ui && podman run --rm -v \$PWD:/ui -w /ui docker.io/library/node:22-bookworm-slim sh -c 'npm ci --no-audit --no-fund >/dev/null 2>&1; echo LINT; npm run lint 2>&1 | tail -30; echo TEST; npx vitest run 2>&1 | tail -40; echo BUILD; npm run build 2>&1 | tail -15'"
fi

if [[ "$WHAT" == "ceph" ]]; then
  log "Dockerfile.ceph full image build"
  $SSH "cd ~/${DIR} && podman build -f Dockerfile.ceph -t atlas-gateway-ceph:ci . 2>&1 | tail -15"
fi
