#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Smoke test of the atlas-data image and its data-ready gate with podman, no Kubernetes needed:
#   1. start the service (loopback http, host network) and publish a tiny dataset
#   2. reserve a job on a registered node; `gate-wait` must FAIL (timeout) while nothing is staged
#   3. mark the node staged; `gate-wait` must now succeed and write selection.json
# Usage: deploy/data/gate-smoke.sh   (run from the repository root; needs podman)
set -uo pipefail
IMG=${IMG:-atlas-data:smoke}
PORT=${PORT:-$((20000 + RANDOM % 20000))}
NAME=atlas-data-smoke-$$
W=$(mktemp -d); chmod 777 "$W"   # the image runs as UID 10001, not the invoking user
FAILS=0
cleanup() { podman rm -f "$NAME" >/dev/null 2>&1; rm -rf "$W"; }
trap cleanup EXIT
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; FAILS=$((FAILS + 1)); }

podman build -q -f Dockerfile.data -t "$IMG" . >/dev/null || { echo "image build failed"; exit 2; }
TOKEN=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')   # 48 hex chars, over the 32 minimum
HASH=$(podman run --rm -e ATLAS_DATA_TOKEN="$TOKEN" "$IMG" token-hash | python3 -c 'import sys,json;print(json.load(sys.stdin)["token_sha256"])')
cat >"$W/config.json" <<EOF
{"credentials":[{"tenant":"acme","subject":"ops","role":"admin","token_sha256":"$HASH"}],"recovery_profiles":{}}
EOF
chmod 644 "$W/config.json"; mkdir -m 777 "$W/data"
podman run -d --rm --name "$NAME" --network=host -v "$W/data:/data" -v "$W/config.json:/etc/atlas-data/config.json:ro" \
  "$IMG" serve --root /data --config /etc/atlas-data/config.json --host 127.0.0.1 --port "$PORT" >/dev/null
for _ in $(seq 1 50); do (echo >/dev/tcp/127.0.0.1/$PORT) 2>/dev/null && break; sleep 0.2; done

cli() { podman run --rm --network=host -e ATLAS_DATA_TOKEN="$TOKEN" -v "$W:/w" "$IMG" --url "http://127.0.0.1:$PORT" "$@"; }
mkdir -m 755 "$W/ds" && echo "training data" >"$W/ds/data.txt" && chmod 644 "$W/ds/data.txt"
cli ingest dataset training v1 /w/ds >"$W/ingest.out" 2>"$W/ingest.err"
ART=$(python3 -c 'import sys,json;print(json.load(open(sys.argv[1]))["id"])' "$W/ingest.out" 2>/dev/null)
[ -n "$ART" ] && pass "dataset published ($ART)" || { fail "publish: $(head -c 400 "$W/ingest.out" "$W/ingest.err")"; exit 1; }
echo "{\"node\":\"gpu-node-1\",\"capacity\":100,\"bandwidth\":100,\"labels\":{}}" >"$W/node.json"
echo "{\"job\":\"job-1\",\"artifacts\":[\"$ART\"]}" >"$W/job.json"
echo "{\"node\":\"gpu-node-1\",\"artifact_id\":\"$ART\"}" >"$W/staged.json"
cli call nodes.heartbeat --json /w/node.json >/dev/null 2>&1 && pass "node registered" || fail "node register"
cli call jobs.reserve --json /w/job.json >/dev/null 2>&1 && pass "job reserved" || fail "job reserve"

# Not staged yet: the gate must fail (non-zero), never let the pod start.
if cli gate-wait job-1 --timeout 3 --interval 1 >"$W/g1.out" 2>"$W/g1.err"; then
  fail "gate-wait succeeded before the data was staged"
else
  pass "gate-wait fails while the data is not staged ($(tr -d '\n' <"$W/g1.err" | cut -c1-90))"
fi

# A trusted worker stages and verifies, then the gate opens and writes the selection.
cli call nodes.staged --json /w/staged.json >/dev/null 2>&1 && pass "node marked staged" || fail "mark staged"
if cli gate-wait job-1 --timeout 10 --interval 1 --out /w/selection.json >"$W/g2.out" 2>"$W/g2.err"; then
  pass "gate-wait succeeds once the data is staged"
  grep -q gpu-node-1 "$W/selection.json" && pass "selection.json names gpu-node-1" || fail "selection.json: $(cat "$W/selection.json" 2>&1 | head -c 200)"
else
  fail "gate-wait after staging: $(cat "$W/g2.err" | head -c 300)"
fi

[ "$FAILS" -eq 0 ] && echo "ALL CHECKS PASSED" || echo "$FAILS CHECK(S) FAILED"
exit $((FAILS > 0))
