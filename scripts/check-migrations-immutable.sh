#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
# Fail if a change modifies, renames or deletes a migration file that already existed at BASE.
# sqlx records a checksum of every applied migration, so editing one — even a comment or a license
# header — makes every existing database refuse to start ("migration N was previously applied but has
# been modified"). New migrations (added files) are fine; fix mistakes with a NEW migration.
#
# Usage: scripts/check-migrations-immutable.sh <base-commit>
#   CI passes the pull request's base sha, or the push's previous head. Skips (exit 0) when the base
#   is empty/unknown (first push of a branch), so it can never block for lack of history.
set -euo pipefail
BASE="${1:-}"
if [[ -z "$BASE" || "$BASE" =~ ^0+$ ]] || ! git cat-file -e "${BASE}^{commit}" 2>/dev/null; then
  echo "migrations immutability: no usable base commit (${BASE:-none}); skipping"
  exit 0
fi
changed="$(git diff --name-status --diff-filter=MDR "$BASE" HEAD -- migrations migrations-postgres || true)"
if [[ -n "$changed" ]]; then
  echo "ERROR: already-committed migration files were modified/renamed/deleted since $BASE:" >&2
  echo "$changed" | sed 's/^/  /' >&2
  echo "Existing databases record a checksum of each applied migration; add a NEW migration instead." >&2
  exit 1
fi
echo "migrations immutability ok (no existing migration changed since ${BASE:0:7})"
