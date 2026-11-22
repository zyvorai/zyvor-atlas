#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
# Build docs/ux/anim/hero.gif from the console screenshots in docs/ux. Needs ffmpeg.
#   ./docs/ux/build-hero-gif.sh
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/hero.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
i=0
for f in night-00-overview day-01-volumes day-02-observatory day-03-ceph day-04-databridge day-05-alerts day-06-jobs; do
  cp "$HERE/$f.png" "$TMP/f$i.png"; i=$((i+1))
done
ffmpeg -y -loglevel error -framerate 0.45 -i "$TMP/f%d.png" \
  -vf "scale=960:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128[p];[b][p]paletteuse=dither=bayer:bayer_scale=3" \
  -loop 0 "$HERE/anim/hero.gif"
ls -l "$HERE/anim/hero.gif"
