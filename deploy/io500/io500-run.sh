#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
# Entry point of the atlas-io500 image (Dockerfile.io500, docs/IO500.md): mounts an atlas-native
# filesystem, runs the IO500 suite on it with IO500_NP MPI ranks in this container, and unmounts.
#
# Environment:
#   ATLAS_NATIVE_ENDPOINTS    node URLs, comma-separated (required; read by atlas-native-mount)
#   ATLAS_NATIVE_TOKEN_FILE   API bearer token file (read by atlas-native-mount)
#   IO500_FS                  filesystem id (required); its root must be writable by root
#   IO500_CA_FILE             CA bundle for https endpoints
#   IO500_MOUNT_ARGS          extra atlas-native-mount arguments, space-separated
#   IO500_NP                  MPI ranks (default: CPUs available)
#   IO500_STONEWALL           write-phase stonewall in seconds (default 300, the rules' value;
#                             anything else makes IO500 mark the run INVALID)
#   IO500_IOR_EASY_BLOCK      ior-easy per-rank block size, e.g. 4g (default: IO500's)
#   IO500_IOR_HARD_SEGMENTS   ior-hard segment count (default: IO500's)
#   IO500_MDTEST_EASY_N       mdtest-easy files per rank (default: IO500's)
#   IO500_MDTEST_HARD_N       mdtest-hard files per rank (default: IO500's)
#   IO500_DROP_CACHES         1 drops the host's page cache before each phase (as the rules expect
#                             for single-node runs). Affects every tenant of the host: dedicated
#                             benchmark hosts only.
#   IO500_RESULTS             result directory (default /results)
#   IO500_HOLD_SECS           keep the container running this long after the run, e.g. to copy
#                             IO500_RESULTS out (default 0)
set -euo pipefail

: "${ATLAS_NATIVE_ENDPOINTS:?ATLAS_NATIVE_ENDPOINTS is required}"
: "${IO500_FS:?IO500_FS is required}"
NP="${IO500_NP:-$(nproc)}"
STONEWALL="${IO500_STONEWALL:-300}"
RESULTS="${IO500_RESULTS:-/results}"
MNT=/mnt/io500

log() { printf '==> %s\n' "$*"; }

mkdir -p "$MNT" "$RESULTS"
rm -f "$RESULTS/.done"

mount_args=(--fs "$IO500_FS")
[[ -n "${IO500_CA_FILE:-}" ]] && mount_args+=(--ca-file "$IO500_CA_FILE")
# shellcheck disable=SC2206  # word splitting is the documented format
[[ -n "${IO500_MOUNT_ARGS:-}" ]] && mount_args+=($IO500_MOUNT_ARGS)

log "mounting filesystem ${IO500_FS} at ${MNT}"
atlas-native-mount "${mount_args[@]}" "$MNT" &
mount_pid=$!

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

drop=FALSE
[[ "${IO500_DROP_CACHES:-0}" == 1 ]] && drop=TRUE

ini="$RESULTS/config.ini"
{
    echo "[global]"
    echo "datadir = ${MNT}/io500"
    echo "timestamp-datadir = TRUE"
    echo "resultdir = ${RESULTS}"
    echo "timestamp-resultdir = TRUE"
    echo "api = POSIX"
    echo "drop-caches = ${drop}"
    # IO500's ini parser ends a value at ';'.
    [[ "$drop" == TRUE ]] && echo 'drop-caches-cmd = sh -c "sync && echo 3 > /proc/sys/vm/drop_caches"'
    echo "verbosity = 1"
    echo
    echo "[debug]"
    echo "stonewall-time = ${STONEWALL}"
    echo
    echo "[ior-easy]"
    [[ -n "${IO500_IOR_EASY_BLOCK:-}" ]] && echo "blockSize = ${IO500_IOR_EASY_BLOCK}"
    echo
    echo "[mdtest-easy]"
    [[ -n "${IO500_MDTEST_EASY_N:-}" ]] && echo "n = ${IO500_MDTEST_EASY_N}"
    echo
    echo "[ior-hard]"
    [[ -n "${IO500_IOR_HARD_SEGMENTS:-}" ]] && echo "segmentCount = ${IO500_IOR_HARD_SEGMENTS}"
    echo
    echo "[mdtest-hard]"
    [[ -n "${IO500_MDTEST_HARD_N:-}" ]] && echo "n = ${IO500_MDTEST_HARD_N}"
    true
} >"$ini"

log "running IO500 with ${NP} ranks, stonewall ${STONEWALL}s"
cat "$ini"
status=0
(cd "$RESULTS" && OMPI_ALLOW_RUN_AS_ROOT=1 OMPI_ALLOW_RUN_AS_ROOT_CONFIRM=1 \
    mpirun --allow-run-as-root --oversubscribe --bind-to none -np "$NP" \
    /opt/io500/io500 "$ini") || status=$?
log "IO500 exited with status ${status}"

rm -rf "${MNT}/io500" || true
echo "$status" >"$RESULTS/.done"
if [[ "${IO500_HOLD_SECS:-0}" -gt 0 ]]; then
    log "holding for ${IO500_HOLD_SECS}s"
    sleep "$IO500_HOLD_SECS" &
    wait $! || true
fi
exit "$status"
