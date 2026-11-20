#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Verify migrations/ (SQLite) and migrations-postgres/ (Postgres) stay in lockstep: every
# migration number present in one must be present in the other. migrations-postgres/ has
# silently drifted behind migrations/ twice before this check existed (see docs/ROADMAP.md) —
# this is the automated gate that replaces manually noticing the gap.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SQLITE_DIR="migrations"
PG_DIR="migrations-postgres"

numbers() {
  # shellcheck disable=SC2012
  ls "$1"/*.sql 2>/dev/null | xargs -n1 basename | grep -oE '^[0-9]+' | sort -u
}

sqlite_nums="$(numbers "$SQLITE_DIR")"
pg_nums="$(numbers "$PG_DIR")"

missing_in_pg="$(comm -23 <(echo "$sqlite_nums") <(echo "$pg_nums"))"
missing_in_sqlite="$(comm -13 <(echo "$sqlite_nums") <(echo "$pg_nums"))"

ec=0
if [[ -n "$missing_in_pg" ]]; then
  echo "ERROR: migration(s) present in $SQLITE_DIR/ but missing from $PG_DIR/:" >&2
  echo "$missing_in_pg" | sed 's/^/  /' >&2
  ec=1
fi
if [[ -n "$missing_in_sqlite" ]]; then
  echo "ERROR: migration(s) present in $PG_DIR/ but missing from $SQLITE_DIR/:" >&2
  echo "$missing_in_sqlite" | sed 's/^/  /' >&2
  ec=1
fi

if [[ "$ec" -eq 0 ]]; then
  count="$(echo "$sqlite_nums" | wc -l | tr -d ' ')"
  echo "migrations parity ok ($count migration(s) in both $SQLITE_DIR/ and $PG_DIR/)"
fi
exit "$ec"
