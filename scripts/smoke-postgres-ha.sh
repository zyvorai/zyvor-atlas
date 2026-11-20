#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# HA smoke: verify the workspace builds and the whole query layer (not just connect+migrate)
# actually runs against a real Postgres via the shared `sqlx::Any` driver. Postgres support is
# unconditional now (no more `--features postgres` — see docs/HA.md) — `atlas_inventory::connect`
# dispatches on the URL scheme, so every crate builds against either backend by default.
#
# Usage:
#   ./scripts/smoke-postgres-ha.sh              # cargo check + build (no live DB)
#   ./scripts/smoke-postgres-ha.sh --migrate    # also run the live query-layer tests (needs Postgres)
#
# Lab Postgres (optional — not required for the check-only path):
#   deploy/postgres-lab/up.sh   # or: docker compose -f deploy/postgres/docker-compose.yml up -d
#   export DATABASE_URL=postgres://atlas:atlas@127.0.0.1:5432/atlas
#
# If Docker/a cluster is unavailable, skip --migrate; cargo check/build still validate the
# workspace compiles against the Any driver.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

RUN_MIGRATE=0
for arg in "$@"; do
  case "$arg" in
    --migrate) RUN_MIGRATE=1 ;;
    -h|--help)
      sed -n '2,20p' "$0"
      exit 0
      ;;
  esac
done

echo "==> cargo check --workspace"
cargo check --workspace

echo "==> cargo test -p atlas-inventory --test postgres_connect (non-ignored: scheme detection only)"
cargo test -p atlas-inventory --test postgres_connect

if [[ "$RUN_MIGRATE" -eq 1 ]]; then
  URL="${DATABASE_URL:-${ATLAS_DATABASE_URL:-}}"
  if [[ -z "$URL" ]]; then
    echo "ERROR: --migrate needs DATABASE_URL or ATLAS_DATABASE_URL (postgres://...)" >&2
    echo "Start a lab DB with: deploy/postgres-lab/up.sh" >&2
    echo "  or: docker compose -f deploy/postgres/docker-compose.yml up -d" >&2
    exit 1
  fi
  echo "==> live query-layer verification against $URL"
  # postgres_live.rs exercises: connect/migrate + migrations parity, the jobs.rs job-claim/
  # reclaim/retry state machine (where the SQLite-only scalar MAX(a,b) portability bug was
  # found and fixed), and users.rs's case-insensitive username lookup (COLLATE NOCASE rewrite).
  DATABASE_URL="$URL" cargo test -p atlas-inventory --test postgres_live -- --ignored --nocapture
else
  echo "==> skip live query-layer tests (pass --migrate + DATABASE_URL when Postgres is up)"
fi

echo "OK: Postgres HA smoke passed"
