#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
# Render docs/social/atlas-social-card.html (1600x900) to a JPEG for LinkedIn and X.
# Needs Google Chrome and macOS `sips`; nothing is installed.
#   ./docs/social/build-social-card.sh [output.jpg]
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:-$HERE/atlas-social-card.jpg}"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
[[ -x "$CHROME" ]] || { echo "Google Chrome not found (set CHROME=...)" >&2; exit 1; }
PNG="$(mktemp "${TMPDIR:-/tmp}/atlas-card.XXXXXX.png")"
trap 'rm -f "$PNG"' EXIT
"$CHROME" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
  --window-size=1600,900 --screenshot="$PNG" "file://$HERE/atlas-social-card.html" >/dev/null 2>&1
sips -s format jpeg -s formatOptions 92 "$PNG" --out "$OUT" >/dev/null
echo "wrote $OUT"
