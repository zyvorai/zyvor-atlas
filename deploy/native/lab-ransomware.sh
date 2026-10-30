#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
#
# Live ransomware test with a real encrypting program. Meant to run INSIDE the Dockerfile builder
# image on the lab host (scripts/lab-ransomware-remote.sh) with /dev/fuse: the same plain-process
# native cluster as lab-verify.sh, but the workload goes through an `atlas-native-mount` FUSE mount
# as an ordinary POSIX program would, not through the S3 gateway.
#
#   1. seed a document tree, record a sha256 manifest
#   2. quiet baseline of ordinary edits (BASELINE_MINUTES, default 8)
#   3. control: a benign backup (tar+gzip and gzip copies of the tree, nothing deleted); it must not
#      be called likely_ransomware and must not trigger auto-protect
#   4. attack: an `openssl enc -aes-256-cbc` loop that encrypts every file to `<name>.locked`, deletes
#      the original and drops a ransom note; the verdict is polled while it runs
#   5. the verdict, auto-protect lock, deletion refusal, and what the locked snapshot still holds
#      (mounted read-only and compared with the manifest)
# Prints one PASS/FAIL line per check and the measured numbers; exits non-zero if a check failed.
set -uo pipefail

WORK=$(mktemp -d)
TOKEN=lab-ransom-token
PIDS=()
FAILS=0
MNT=$WORK/mnt
SNAPMNT=$WORK/snapmnt
cleanup() {
  fusermount3 -uz "$MNT" 2>/dev/null || true
  fusermount3 -uz "$SNAPMNT" 2>/dev/null || true
  sleep 1
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; FAILS=$((FAILS + 1)); }

BIN=${BIN:-target/debug}
NODE=$BIN/atlas-native-node
MOUNT=$BIN/atlas-native-mount
GW=$BIN/atlas-gateway
CTL=$BIN/atlasctl
FILES=${FILES:-400}

echo "$TOKEN" >"$WORK/token"
api() { curl -sS -H "Authorization: Bearer $TOKEN" "$@"; }

data_nodes='[{"id":"d1","addr":"127.0.0.1:27101"},{"id":"d2","addr":"127.0.0.1:27102"},{"id":"d3","addr":"127.0.0.1:27103"}]'
for i in 1 2 3; do
  cat >"$WORK/d$i.json" <<EOF
{"node_id":"d$i","data_dir":"$WORK/d$i","http_listen":"127.0.0.1:2720$i",
 "data_node":{"listen":"127.0.0.1:2710$i"}}
EOF
  peers=$(for j in 1 2 3; do [ "$j" = "$i" ] || printf '"m%s":"127.0.0.1:2730%s",' "$j" "$j"; done)
  cat >"$WORK/m$i.json" <<EOF
{"node_id":"m$i","data_dir":"$WORK/m$i","http_listen":"127.0.0.1:2740$i",
 "api_token_file":"$WORK/token",
 "metadata":{"listen":"127.0.0.1:2730$i","peers":{${peers%,}},"data_nodes":$data_nodes,
             "replicas":3,"extent_bytes":4096,"compression":true,"dedup":true,
             "repair_interval_secs":5,"gc_interval_secs":5}}
EOF
done
for n in d1 d2 d3 m1 m2 m3; do
  "$NODE" --config "$WORK/$n.json" 2>"$WORK/$n.log" &
  PIDS+=($!)
done
for i in 1 2 3; do
  for _ in $(seq 1 300); do curl -sf "http://127.0.0.1:2740$i/readyz" >/dev/null && break; sleep 0.1; done
done
L=""
for _ in $(seq 1 200); do
  for i in 1 2 3; do
    api "http://127.0.0.1:2740$i/v1/status" 2>/dev/null | grep -q '"role":"leader"' && L=$i
  done
  [ -n "$L" ] && break
  sleep 0.1
done
[ -n "$L" ] || { echo "no leader elected"; exit 2; }
NODEURL=http://127.0.0.1:2740$L
echo "cluster up; leader m$L; the gateway and the mount both use m$L only (activity windows are per serving node)"

ATLAS_DATABASE_URL="sqlite://$WORK/atlas.db?mode=rwc" ATLAS_BIND_ADDR=127.0.0.1:25110 ATLAS_GRPC_ADDR=127.0.0.1:25111 \
  ATLAS_CEPH_DRIVER_MODE=fake ATLAS_AUTH_REQUIRED=0 ATLAS_JWT_SECRET=lab-verify-secret-at-least-32-bytes!! \
  ATLAS_NATIVE_ENABLE=1 ATLAS_NATIVE_DRIVER_MODE=real ATLAS_NATIVE_ENDPOINTS=$NODEURL \
  ATLAS_NATIVE_TOKEN_FILE=$WORK/token ATLAS_RANSOMWARE_AUTOPROTECT=1 ATLAS_RANSOMWARE_INTERVAL_SECS=10 \
  "$GW" >"$WORK/gw.log" 2>&1 &
PIDS+=($!)
GWURL=http://127.0.0.1:25110/api/atlas/v1
for _ in $(seq 1 300); do curl -sf http://127.0.0.1:25110/health >/dev/null && break; sleep 0.2; done
curl -sf http://127.0.0.1:25110/health >/dev/null || { echo "gateway did not start"; tail -20 "$WORK/gw.log"; exit 2; }

"$CTL" --base-url http://127.0.0.1:25110 create-volume ransomfs --kind filesystem --backend bkd_native >"$WORK/create.out" 2>&1
VOL=$(curl -sS "$GWURL/volumes" | python3 -c 'import sys,json;v=[x for x in json.load(sys.stdin) if x["name"]=="ransomfs"];print(v[0]["id"] if v else "")')
[ -n "$VOL" ] || { echo "filesystem volume not created"; cat "$WORK/create.out"; exit 2; }
FSID=${VOL#vol_native_fs_}
echo "filesystem volume $VOL"

mkdir -p "$MNT" "$SNAPMNT"
"$MOUNT" --endpoint "$NODEURL" --token-file "$WORK/token" --fs "$FSID" "$MNT" >"$WORK/mount.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 100); do mountpoint -q "$MNT" && break; sleep 0.1; done
mountpoint -q "$MNT" && pass "native filesystem mounted over FUSE" || { fail "mount did not come up"; cat "$WORK/mount.log"; exit 2; }

verdict() { curl -sS "$GWURL/volumes/$VOL/ransomware" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d["verdict"],d.get("risk_score",d.get("score")))'; }
verdict_full() { curl -sS "$GWURL/volumes/$VOL/ransomware"; echo; }
activity() { api "$NODEURL/v1/fs/$FSID/activity" | python3 -c 'import sys,json;[print("  ",x) for x in json.load(sys.stdin)["samples"][-8:]]'; }
snap_count() { curl -sS "$GWURL/snapshots" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))'; }

# ---------------------------------------------------------------- 1. seed
echo "--- seeding $FILES documents"
python3 - "$MNT/docs" "$FILES" <<'PY'
import os, random, sys
root, n = sys.argv[1], int(sys.argv[2])
random.seed(7)
words = "quarterly revenue forecast invoice customer contract schedule project budget review summary".split()
for i in range(n):
    d = f"{root}/dept{i % 8}"
    os.makedirs(d, exist_ok=True)
    body = "\n".join(" ".join(random.choice(words) for _ in range(12)) + f" #{i}-{j}" for j in range(random.randint(40, 600)))
    open(f"{d}/report{i}.txt", "w").write(body)
PY
( cd "$MNT/docs" && find . -type f | sort | xargs sha256sum ) >"$WORK/manifest.sha256"
echo "seeded $(wc -l <"$WORK/manifest.sha256") files, $(du -sk "$MNT/docs" | cut -f1) KiB"

# ---------------------------------------------------------------- 2. baseline
echo "--- quiet baseline for ${BASELINE_MINUTES:-8} minutes (ordinary edits)"
END=$(( $(date +%s) + ${BASELINE_MINUTES:-8} * 60 ))
n=0
while [ "$(date +%s)" -lt "$END" ]; do
  f="$MNT/docs/dept$((n % 8))/report$(( (n * 8) % FILES + n % 8 )).txt"
  [ -f "$f" ] && echo "edit $n" >>"$f"
  n=$((n + 1))
  sleep 15
done
# The edits above changed a few files, so the manifest is retaken as the "known good" state.
( cd "$MNT/docs" && find . -type f | sort | xargs sha256sum ) >"$WORK/manifest.sha256"
B=$(verdict)
echo "baseline verdict: $B"
case "$B" in "normal "*) pass "quiet baseline is assessed normal" ;; *) fail "baseline verdict: $B" ;; esac

# ---------------------------------------------------------------- 3. control: benign backup
if [ "${SKIP_CONTROL:-0}" != 1 ]; then
echo "--- control: tar+gzip backup and gzip copies (high entropy, nothing deleted)"
SNAPS0=$(snap_count)
mkdir -p "$MNT/backup"
tar czf "$MNT/backup/docs.tar.gz" -C "$MNT" docs
( cd "$MNT/docs" && find . -type f | head -150 | while read -r f; do mkdir -p "$MNT/backup/$(dirname "$f")"; gzip -c "$f" >"$MNT/backup/$f.gz"; done )
sleep 20
C=$(verdict)
echo "control verdict: $C"
case "$C" in "likely_ransomware "*) fail "benign backup was called likely_ransomware: $C" ;; *) pass "benign backup is not called likely_ransomware ($C)" ;; esac
[ "$(snap_count)" = "$SNAPS0" ] && pass "benign backup triggered no auto-protect snapshot" || { fail "benign backup triggered auto-protect"; grep -i "ransomware\|locked snapshot" "$WORK/gw.log" | tail -5; }
echo "control signals:"; verdict_full; activity
if [ "${CONTROL_ONLY:-0}" = 1 ]; then
  echo; if [ "$FAILS" = 0 ]; then echo "ALL CHECKS PASSED"; else echo "$FAILS CHECK(S) FAILED"; fi
  exit "$FAILS"
fi
echo "waiting 70 s so the attack lands in its own one-minute windows"
sleep 70
fi

# ---------------------------------------------------------------- 4. attack
echo "--- attack: openssl aes-256-cbc encrypt, write .locked, delete original"
cat >"$WORK/ransom.sh" <<'EOS'
#!/usr/bin/env bash
# A small but real file-encrypting program: reads each file, encrypts it with AES-256-CBC (openssl,
# PBKDF2 key from a passphrase), writes <name>.locked, unlinks the original, drops a note per directory.
root=$1
find "$root" -type f ! -name '*.locked' ! -name 'README_DECRYPT.txt' | while read -r f; do
  openssl enc -aes-256-cbc -pbkdf2 -salt -pass pass:attackerkey -in "$f" -out "$f.locked" && rm -f "$f"
  d=$(dirname "$f")
  [ -f "$d/README_DECRYPT.txt" ] || echo "Your files are encrypted. Pay to recover." >"$d/README_DECRYPT.txt"
done
EOS
chmod +x "$WORK/ransom.sh"
SNAPS1=$(snap_count)
SNAP_IDS_BEFORE=$(curl -sS "$GWURL/snapshots" | python3 -c 'import sys,json;print(" ".join(x["id"] for x in json.load(sys.stdin)))')
(
  first=""
  while [ ! -f "$WORK/attack.done" ]; do
    v=$(verdict 2>/dev/null)
    echo "$(date +%s.%N) $v" >>"$WORK/verdicts.log"
    sleep 2
  done
) &
PIDS+=($!)
T0=$(date +%s.%N)
"$WORK/ransom.sh" "$MNT/docs"
T1=$(date +%s.%N)
touch "$WORK/attack.done"
python3 -c "print('attack ran %.1f s' % (float('$T1') - float('$T0')))"
LEFT=$(find "$MNT/docs" -type f ! -name '*.locked' ! -name 'README_DECRYPT.txt' | wc -l)
LOCKED=$(find "$MNT/docs" -type f -name '*.locked' | wc -l)
echo "after attack: $LOCKED .locked files, $LEFT originals left"
[ "$LOCKED" -ge "$FILES" ] && [ "$LEFT" = 0 ] && pass "the program really encrypted and removed every document ($LOCKED files)" || fail "attack incomplete ($LOCKED locked, $LEFT left)"

# decrypting a sample with the key proves the .locked files are genuine ciphertext of the originals
f=$(cd "$MNT/docs" && head -1 "$WORK/manifest.sha256" | awk '{print $2}')
want=$(head -1 "$WORK/manifest.sha256" | awk '{print $1}')
got=$(openssl enc -d -aes-256-cbc -pbkdf2 -pass pass:attackerkey -in "$MNT/docs/$f.locked" | sha256sum | awk '{print $1}')
[ "$want" = "$got" ] && pass "a .locked file decrypts back to the original (it is real AES ciphertext)" || fail "sample did not decrypt to the original"

# ---------------------------------------------------------------- 5. detection and protection
echo "--- detection"
echo "verdict timeline during the attack:"; sed "s/^/  /" "$WORK/verdicts.log"
echo "activity windows:"; activity
echo "full verdict:"; verdict_full
LASTV=""
for _ in $(seq 1 30); do
  LASTV=$(verdict)
  case "$LASTV" in "likely_ransomware "*) break ;; esac
  sleep 3
done
echo "verdict after attack: $LASTV"; verdict_full; activity; grep -i "ransomware\|locked snapshot" "$WORK/gw.log" | tail -5
case "$LASTV" in "likely_ransomware "*) pass "real encrypting workload is assessed likely_ransomware" ;; *) fail "verdict after real attack: $LASTV" ;; esac
FIRST=$(grep -m1 likely_ransomware "$WORK/verdicts.log" | awk '{print $1}')
if [ -n "$FIRST" ]; then
  python3 -c "print('first likely_ransomware verdict %.1f s after the attack started (attack lasted %.1f s)' % (float('$FIRST') - float('$T0'), float('$T1') - float('$T0')))"
else
  echo "no likely_ransomware verdict was seen while the attack was running"
fi

AUTO=$SNAPS1
for _ in $(seq 1 24); do
  AUTO=$(snap_count)
  [ "$AUTO" -gt "$SNAPS1" ] && break
  sleep 5
done
[ "$AUTO" -gt "$SNAPS1" ] && pass "auto-protect took a snapshot ($SNAPS1 -> $AUTO)" || fail "auto-protect took no snapshot"
AUTOSNAP=$(curl -sS "$GWURL/snapshots" | python3 -c 'import sys,json;old=set(sys.argv[1].split());print(next((x["id"] for x in json.load(sys.stdin) if x["id"] not in old),""))' "$SNAP_IDS_BEFORE")
if [ -n "$AUTOSNAP" ]; then
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE "$GWURL/snapshots/$AUTOSNAP")
  [ "$code" = 409 ] && pass "the auto-protect snapshot refuses deletion (409)" || fail "delete locked snapshot -> $code"
  NATIVESNAP=${AUTOSNAP#snap_native_fs_}
  "$MOUNT" --endpoint "$NODEURL" --token-file "$WORK/token" --fs "$FSID" --snapshot "$NATIVESNAP" "$SNAPMNT" >"$WORK/snapmount.log" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 1 100); do mountpoint -q "$SNAPMNT" && break; sleep 0.1; done
  if mountpoint -q "$SNAPMNT"; then
    SP=$(find "$SNAPMNT/docs" -type f ! -name '*.locked' ! -name 'README_DECRYPT.txt' | wc -l)
    SL=$(find "$SNAPMNT/docs" -type f -name '*.locked' | wc -l)
    OK=$(cd "$SNAPMNT/docs" 2>/dev/null && sha256sum -c "$WORK/manifest.sha256" 2>/dev/null | grep -c ': OK$')
    echo "locked snapshot holds: $SP plaintext files ($OK byte-identical to the pre-attack manifest), $SL .locked files, of $FILES"
    [ "$OK" = "$FILES" ] && pass "the whole tree is recoverable from the locked snapshot" \
      || echo "NOTE  the snapshot was taken after $((FILES - OK)) of $FILES files were already encrypted"
  else
    fail "could not mount the locked snapshot"; cat "$WORK/snapmount.log"
  fi
fi
code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$GWURL/volumes/$VOL/ransomware/protect")
[ "$code" = 201 ] && pass "manual protect locked a snapshot (201)" || fail "manual protect -> $code"

echo
if [ "$FAILS" = 0 ]; then echo "ALL CHECKS PASSED"; else echo "$FAILS CHECK(S) FAILED"; tail -15 "$WORK/gw.log"; fi
exit "$FAILS"
