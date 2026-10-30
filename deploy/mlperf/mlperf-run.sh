#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Entry point of the atlas-mlperf image (Dockerfile.mlperf, docs/MLPERF.md): mounts an atlas-native
# filesystem, runs `mlpstorage` with the arguments in MLPERF_ARGS, and unmounts.
#
# Environment:
#   ATLAS_NATIVE_ENDPOINTS    node URLs, comma-separated (required; read by atlas-native-mount)
#   ATLAS_NATIVE_TOKEN_FILE   API bearer token file (read by atlas-native-mount)
#   MLPERF_FS                 filesystem id (required); its root must be writable by root
#   MLPERF_ARGS               arguments after `mlpstorage`, space-separated (required), e.g.
#                             "closed training unet3d datagen file --num-processes 4"
#   MLPERF_APPEND_DIRS        1 (default) appends --data-dir and --results-dir; set 0 for
#                             subcommands that do not take them (e.g. datasize)
#   MLPERF_CA_FILE            CA bundle for https endpoints
#   MLPERF_MOUNT_ARGS         extra atlas-native-mount arguments, space-separated
#   MLPERF_RESULTS            result directory (default /results)
#   MLPERF_HOLD_SECS          keep the container running this long after the run, e.g. to copy
#                             MLPERF_RESULTS out (default 0)
set -euo pipefail

: "${ATLAS_NATIVE_ENDPOINTS:?ATLAS_NATIVE_ENDPOINTS is required}"
: "${MLPERF_FS:?MLPERF_FS is required}"
: "${MLPERF_ARGS:?MLPERF_ARGS is required (the arguments after mlpstorage)}"
RESULTS="${MLPERF_RESULTS:-/results}"
MNT=/mnt/mlperf

log() { printf '==> %s\n' "$*"; }

mkdir -p "$MNT" "$RESULTS"
rm -f "$RESULTS/.done"

mount_args=(--fs "$MLPERF_FS")
[[ -n "${MLPERF_CA_FILE:-}" ]] && mount_args+=(--ca-file "$MLPERF_CA_FILE")
# shellcheck disable=SC2206  # word splitting is the documented format
[[ -n "${MLPERF_MOUNT_ARGS:-}" ]] && mount_args+=($MLPERF_MOUNT_ARGS)

log "mounting filesystem ${MLPERF_FS} at ${MNT}"
atlas-native-mount "${mount_args[@]}" "$MNT" &
mount_pid=$!

# shellcheck disable=SC2329  # invoked through the EXIT trap
unmount() {
    if mountpoint -q "$MNT"; then
        log "unmounting ${MNT}"
        fusermount3 -u "$MNT" || true
    fi
    wait "$mount_pid" 2>/dev/null || true
}
trap unmount EXIT
trap 'exit 143' TERM INT

for _ in $(seq 1 120); do
    mountpoint -q "$MNT" && break
    kill -0 "$mount_pid" 2>/dev/null || { echo "atlas-native-mount exited" >&2; exit 1; }
    sleep 0.5
done
mountpoint -q "$MNT" || { echo "mount did not appear within 60s" >&2; exit 1; }

# shellcheck disable=SC2206  # word splitting is the documented format
cmd=(mlpstorage $MLPERF_ARGS)
if [[ "${MLPERF_APPEND_DIRS:-1}" == 1 ]]; then
    mkdir -p "$MNT/mlperf"
    cmd+=(--data-dir "$MNT/mlperf" --results-dir "$RESULTS")
fi
# MLPerf Storage needs --allow-run-as-root on MPI-launching commands when run as root.
if [[ "$(id -u)" == 0 && " ${cmd[*]} " != *" --allow-run-as-root "* ]]; then
    cmd+=(--allow-run-as-root)
fi

log "running: ${cmd[*]}"
status=0
"${cmd[@]}" || status=$?
log "mlpstorage exited with status ${status}"

echo "$status" >"$RESULTS/.done"
if [[ "${MLPERF_HOLD_SECS:-0}" -gt 0 ]]; then
    log "holding for ${MLPERF_HOLD_SECS}s"
    sleep "$MLPERF_HOLD_SECS" &
    wait $! || true
fi
exit "$status"
