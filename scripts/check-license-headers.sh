#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Verify Atlas source files carry copyright + SPDX license headers.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

EXPECTED_SPDX='LicenseRef-Zyvor-Production-1.0'
COPYRIGHT_NEEDLE='Copyright (c) 2026 ZyvorAI Labs Private Limited'

# File types that must carry headers (first ~20 lines / 2 KiB).
EXTENSIONS=(
  rs ts tsx js jsx mjs cjs css sql proto py sh bash
  toml yml yaml md html htm
)

SKIP_GLOBS=(
  './target/*'
  './.git/*'
  '*/node_modules/*'
  '*/dist/*'
  './.docs-tools/*'
  './LICENSES/*'
  './LICENSE'
  './NOTICE'
  './Cargo.lock'
  '*/package-lock.json'
  './deny.toml' # checked separately — hash header
)

missing=()
bad_spdx=()
scanned=0

is_skipped() {
  local f="$1"
  case "$f" in
    ./target/*|./.git/*|*/node_modules/*|*/dist/*|./.docs-tools/*|./LICENSES/*) return 0 ;;
    ./LICENSE|./NOTICE|./Cargo.lock|*/package-lock.json) return 0 ;;
  esac
  return 1
}

check_file() {
  local f="$1"
  scanned=$((scanned + 1))
  # Read a small head for speed.
  local head
  head="$(head -c 2048 "$f" 2>/dev/null || true)"
  if ! grep -qF "$COPYRIGHT_NEEDLE" <<<"$head"; then
    missing+=("$f")
    return
  fi
  # SQL migrations are checksummed by sqlx: editing even a header comment of an already-applied
  # migration makes every existing database refuse to start ("migration N was previously applied but
  # has been modified"). They keep their original header text and are licensed like everything else
  # (see LICENSE); only the presence of a copyright header is required of them.
  case "$f" in
    ./migrations/*|./migrations-postgres/*) return ;;
  esac
  if ! grep -qF "SPDX-License-Identifier: $EXPECTED_SPDX" <<<"$head"; then
    bad_spdx+=("$f")
  fi
}

# Named hash-style files without a typical extension.
for named in Makefile Dockerfile Dockerfile.ceph deny.toml .gitignore rustfmt.toml; do
  if [[ -f "$named" ]]; then
    check_file "./$named"
  fi
done

# Dockerfiles under deploy/
while IFS= read -r -d '' f; do
  check_file "$f"
done < <(find . \( -path ./target -o -path ./.git -o -path '*/node_modules/*' -o -path '*/dist/*' \) -prune -o \
  -type f \( -name 'Dockerfile' -o -name 'Dockerfile.*' \) -print0)

for ext in "${EXTENSIONS[@]}"; do
  while IFS= read -r -d '' f; do
    if is_skipped "$f"; then
      continue
    fi
    check_file "$f"
  done < <(find . \( -path ./target -o -path ./.git -o -path '*/node_modules/*' -o -path '*/dist/*' -o -path ./.docs-tools -o -path ./LICENSES \) -prune -o \
    -type f -name "*.${ext}" -print0)
done

ec=0
if ((${#missing[@]})); then
  echo "ERROR: missing Zyvor copyright header (${#missing[@]} file(s)):" >&2
  printf '  %s\n' "${missing[@]}" >&2
  ec=1
fi
if ((${#bad_spdx[@]})); then
  echo "ERROR: missing or wrong SPDX-License-Identifier (want: $EXPECTED_SPDX) (${#bad_spdx[@]} file(s)):" >&2
  printf '  %s\n' "${bad_spdx[@]}" >&2
  ec=1
fi

if [[ "$ec" -eq 0 ]]; then
  echo "license headers ok ($scanned files checked; SPDX=$EXPECTED_SPDX)"
fi
exit "$ec"
