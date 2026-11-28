#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Builds the release atlas-native binaries and starts atlas-bench-atlas (see common.sh for the contract
# and atlas-entry.sh for what runs inside). Safe to re-run: a healthy container with the same image and
# settings is left alone. Server data in $BENCH_ROOT/data/atlas is kept; changing ATLAS_EXTENT_BYTES
# needs `down.sh atlas` first.
#
# Usage: atlas-up.sh
#   ATLAS_EXTENT_BYTES=1048576  extent size of the filesystem, fixed when it is created
#   ATLAS_SKIP_BUILD=1          use the existing image
#   ATLAS_START_TIMEOUT=600     seconds to wait for the cluster and mount on a busy disk
#   BENCH_ATLAS_SRC=<dir>       source tree to build the binaries from (default: this one); the harness,
#                               entry script and image recipe are always this directory's
#   BENCH_ATLAS_IMAGE=<name>    image to build or use (default localhost/<prefix>-atlas:latest)
#   BENCH_ATLAS_BUILD_ONLY=1    build the image and stop
#   BENCH_CACHE, BENCH_TARGET   cargo caches and target directory (default under $BENCH_ROOT/cache)
#   CARGO_BUILD_JOBS            passed to the build
# The build holds $BENCH_ROOT/bench.lock shared, so it waits for a running benchmark.
#   Other ATLAS_* settings and their defaults are listed at the top of atlas-entry.sh.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$HERE/common.sh"

ATLAS_EXTENT_BYTES="${ATLAS_EXTENT_BYTES:-1048576}"
[[ "$ATLAS_EXTENT_BYTES" =~ ^[1-9][0-9]*$ ]] || { echo "ATLAS_EXTENT_BYTES must be a number of bytes" >&2; exit 2; }
export ATLAS_EXTENT_BYTES
START_TIMEOUT="${ATLAS_START_TIMEOUT:-600}"

SRC="$(cd "${BENCH_ATLAS_SRC:-$HERE/../..}" && pwd)"
CACHE="${BENCH_CACHE:-$BENCH_ROOT/cache}"
TARGET="${BENCH_TARGET:-$CACHE/target}"
CT="$(container_name atlas)"
IMAGE="${BENCH_ATLAS_IMAGE:-localhost/$BENCH_NAME_PREFIX-atlas:latest}"
for dir in "$CACHE" "$TARGET"; do
  case "$dir" in
    *..*) echo "cache paths must not contain '..'" >&2; exit 2 ;;
    /data/atlas-bench/*) ;;
    *) echo "cache paths must be under /data/atlas-bench, got: $dir" >&2; exit 2 ;;
  esac
done

build() {
  local rust_image channel ctx="$BENCH_ROOT/cache/atlas-image"
  local jobs=()
  [ -z "${CARGO_BUILD_JOBS:-}" ] || jobs=(-e "CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS")
  rust_image="$(awk '$1 == "FROM" && / AS builder$/ {print $2}' "$SRC/Dockerfile.native")"
  channel="$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$SRC/rust-toolchain.toml")"
  [ -n "$rust_image" ] && [ -n "$channel" ] || { echo "cannot read the builder image or toolchain" >&2; exit 1; }
  mkdir -p "$CACHE/cargo-registry" "$CACHE/cargo-git" "$CACHE/rustup" "$TARGET" "$ctx"
  echo "building release binaries in $rust_image (toolchain $channel)"
  # Every source copy mounts at /build and shares one target directory, and cargo judges freshness by
  # file times: without this, a tree older than another copy's build would be served that build.
  find "$SRC/crates" "$SRC/Cargo.toml" "$SRC/Cargo.lock" -type f -exec touch {} +
  bpodman run --rm --replace --name "$CT-build" --http-proxy=false "${jobs[@]}" \
    --cpus "$BENCH_CPUS" --memory "$BENCH_MEM" --pids-limit -1 \
    -v "$SRC:/build:ro" -w /build \
    -v "$CACHE/cargo-registry:/usr/local/cargo/registry" -v "$CACHE/cargo-git:/usr/local/cargo/git" \
    -v "$CACHE/rustup:/cache/rustup" -v "$TARGET:/cache/target" \
    -e RUSTUP_HOME=/cache/rustup -e CARGO_TARGET_DIR=/cache/target -e "CHANNEL=$channel" \
    "$rust_image" bash -euc '
      rustup toolchain install "$CHANNEL" --profile minimal --no-self-update
      cargo build --release --locked -p atlas-native --features s3 --bin atlas-native-node
      cargo build --release --locked -p atlas-native-fuse --features fuse --bin atlas-native-mount' 9>&-
  cp "$TARGET/release/atlas-native-node" "$TARGET/release/atlas-native-mount" \
    "$HERE/atlas-entry.sh" "$ctx/"
  bpodman build --http-proxy=false -t "$IMAGE" -f "$HERE/atlas-Containerfile" "$ctx" 9>&-
}

writable() {
  bpodman exec "$CT" timeout 60 sh -c 'test -f /run/atlas/ready && mountpoint -q /mnt/bench &&
    p=/mnt/bench/.atlas-up-probe && dd if=/dev/zero of="$p" bs=4096 count=1 oflag=direct,sync status=none &&
    rm "$p"'
}

running() { [ "$(bpodman inspect -f '{{.State.Running}}' "$CT" 2>/dev/null)" = true ]; }

if [ "${ATLAS_SKIP_BUILD:-0}" != 1 ]; then
  (
    flock -s 9
    build
  ) 9>"$BENCH_ROOT/bench.lock"
fi
if [ "${BENCH_ATLAS_BUILD_ONLY:-0}" = 1 ]; then
  echo "built $IMAGE from $SRC"
  exit 0
fi
image_id="$(bpodman image inspect -f '{{.Id}}' "$IMAGE")"

settings=()
for v in $(compgen -e | grep '^ATLAS_' | grep -v '^ATLAS_SKIP_BUILD$' | sort); do
  settings+=(-e "$v=${!v}")
done
bench_mounts atlas
config="$(printf '%s\n' "$image_id" "${settings[@]}" "${BENCH_RUN_ARGS[@]}" "${BENCH_MOUNT_ARGS[@]}" |
  sha256sum | cut -c1-16)"

if running && [ "$(bpodman inspect -f '{{index .Config.Labels "atlas.bench.config"}}' "$CT")" = "$config" ] &&
  writable; then
  echo "$CT already running with this image and these settings; mount is writable"
  exit 0
fi

bpodman rm -f --ignore -t 30 "$CT" >/dev/null
# No network: every daemon talks over the container's own loopback.
bpodman run -d --name "$CT" "${BENCH_RUN_ARGS[@]}" --network=none --http-proxy=false \
  --label "atlas.bench.config=$config" "${BENCH_MOUNT_ARGS[@]}" "${settings[@]}" "$IMAGE" >/dev/null

deadline=$((SECONDS + START_TIMEOUT + 60))
while [ "$SECONDS" -lt "$deadline" ]; do
  if ! running; then
    echo "$CT exited during startup:" >&2
    bpodman logs --tail 120 "$CT" >&2
    exit 1
  fi
  if bpodman exec "$CT" test -f /run/atlas/ready 2>/dev/null; then
    writable || break
    bpodman logs "$CT" 2>&1
    echo "$CT up: /mnt/bench writable, extent_bytes=$ATLAS_EXTENT_BYTES"
    exit 0
  fi
  sleep 1
done
echo "$CT: mount did not become writable:" >&2
bpodman logs --tail 120 "$CT" >&2
exit 1
