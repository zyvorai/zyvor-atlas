#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# One-shot "test all features" runner. Executes the tiers that need no external infrastructure and
# then, if a container runtime is present, the real-database connector verification.
#
#   Tier 0  static gate (clippy + workspace tests + build) + optional-feature compile checks
#   Tier 1  fake-driver gap tests (folded into the workspace test run)
#   Tier 2  real DB connectors via ephemeral containers (scripts/test-connectors.sh)
#   Tier 4  UI unit tests (vitest) + production build (when npm is on PATH)
#
# Tier 3 (real Ceph/k8s write path on a remote cluster) is printed as an opt-in follow-up — it
# mutates a remote, so it isn't automatic. Docker image builds are CI-only (see .github/workflows).
#
# Usage: scripts/test-all.sh [--no-containers] [--no-ui]
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$HERE"

NO_CONTAINERS=0
NO_UI=0
for arg in "$@"; do
  case "$arg" in
    --no-containers) NO_CONTAINERS=1 ;;
    --no-ui) NO_UI=1 ;;
    -h|--help)
      sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

log() { printf '\n\033[1;36m========== %s ==========\033[0m\n' "$*"; }

log "Tier 0: clippy (-D warnings)"
cargo clippy --workspace --all-targets -- -D warnings

log "Tier 0+1: workspace tests (unit + gateway integration incl. observability + databridge_engines)"
cargo test --workspace

log "Tier 0: optional-feature compile checks"
for f in sqlserver mongodb azure-blob oracle; do
  echo "--- --features $f ---"; cargo check -p atlas-databridge --features "$f"
done
if command -v cmake >/dev/null; then
  echo "--- --features kafka-lag ---"; cargo check -p atlas-databridge --features kafka-lag
else
  echo "!! skipping kafka-lag compile (cmake not installed)"
fi

# NOTE: `cargo fmt --all --check` is intentionally NOT a hard gate. The repo's committed style is
# denser than stable rustfmt (~280 files of drift); CI surfaces it as informational. Prefer
# formatting the files you touch (`rustfmt path/to/file.rs`) rather than a whole-tree rewrite.

if [[ "$NO_CONTAINERS" == "0" ]] && { command -v podman >/dev/null || command -v docker >/dev/null; }; then
  log "Tier 2: real DB connectors via containers"
  ./scripts/test-connectors.sh
else
  echo "!! skipping Tier 2 (no container runtime, or --no-containers)"
fi

if [[ "$NO_UI" == "0" ]] && command -v npm >/dev/null; then
  log "Tier 4: UI unit tests + production build"
  (cd crates/atlas-gateway/ui && npm ci && npm test && npm run build)
else
  echo "!! skipping Tier 4 (no npm, or --no-ui)"
fi

if [[ "${ATLAS_LIVE:-0}" == "1" ]]; then
  log "Tier 3: live remote suite (ATLAS_LIVE=1)"
  ./scripts/test-live.sh
else
  echo "!! skipping Tier 3 live suite (set ATLAS_LIVE=1 to run against a deployed gateway)"
fi

log "DONE. Opt-in follow-ups:"
cat <<'EOF'
  Tier 3 (live Ceph/k8s): ATLAS_LIVE=1 ./scripts/test-live.sh
          Default base http://212.8.248.187:30511 — see scripts/live/README.md.
          Mutates the remote (volumes/buckets) and cleans up.
  Docker: CI builds Dockerfile.ceph (full) + both Dockerfiles' UI stages; locally:
          podman build -t atlas-gateway:ceph -f Dockerfile.ceph .
EOF
