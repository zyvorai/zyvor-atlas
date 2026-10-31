#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
#
# Smoke test for the I/O pattern an inference KV-cache offload tier uses (LMCache, Dynamo KVBM, vLLM
# offloading): many multi-MiB block files, written with direct I/O, published by rename, read back
# concurrently and byte-exactly. It checks that an Atlas interface behaves correctly for that pattern. It
# does NOT measure throughput, and it has not been run against any inference engine (docs/AI_INFERENCE.md).
#
# Usage: kvcache-smoke.sh <dir>            a directory on an atlas-native mount, NFS export or CSI volume
#        S3_ENDPOINT=http://host:port S3_BUCKET=name S3_ACCESS_KEY=... S3_SECRET_KEY=... \
#          kvcache-smoke.sh <dir>          additionally checks the S3 gateway (needs curl 7.75+)
# Env: BLOCKS (default 64), BLOCK_KIB (default 2048), READERS (default 8).
# Prints one PASS/FAIL/SKIP line per check and exits non-zero if any check failed.
set -uo pipefail

DIR=${1:?usage: kvcache-smoke.sh <dir>}
BLOCKS=${BLOCKS:-64}
BLOCK_KIB=${BLOCK_KIB:-2048}
READERS=${READERS:-8}
FAILS=0

pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; FAILS=$((FAILS + 1)); }
skip() { echo "SKIP  $*"; }

[ -d "$DIR" ] || { echo "not a directory: $DIR" >&2; exit 2; }
# WORK holds the blocks under test, on the path being tested. LOCAL holds this script's own output and
# scratch files, off that path, so a failing mount cannot also lose the results.
WORK=$(mktemp -d "$DIR/.kvcache-smoke.XXXXXX") || { echo "cannot write in $DIR" >&2; exit 2; }
LOCAL=$(mktemp -d)
trap 'rm -rf "$WORK" "$LOCAL"' EXIT

# The file checks run in Python: O_DIRECT needs a page-aligned buffer on most filesystems, which a shell
# cannot provide. Output is one "PASS|FAIL <name>" line per check.
python3 - "$WORK" "$BLOCKS" "$BLOCK_KIB" "$READERS" <<'PY' | tee "$LOCAL/files.out"
import hashlib, mmap, os, random, sys, threading

work, blocks, kib, readers = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
size = kib * 1024
rng = random.Random(7)
data = [rng.randbytes(size) for _ in range(blocks)]
digest = [hashlib.sha256(d).hexdigest() for d in data]
failed = False


def report(ok, name):
    global failed
    failed |= not ok
    print(("PASS" if ok else "FAIL"), " ", name, flush=True)


def aligned(buf):
    m = mmap.mmap(-1, len(buf))  # anonymous mappings are page aligned
    m.write(buf)
    return m


def write_direct(path, buf):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_DIRECT, 0o644)
    try:
        view = aligned(buf)
        done = 0
        while done < len(buf):
            done += os.pwritev(fd, [memoryview(view)[done:]], done)
    finally:
        os.close(fd)


def read_all(path, direct):
    flags = os.O_RDONLY | (os.O_DIRECT if direct else 0)
    fd = os.open(path, flags)
    try:
        view = mmap.mmap(-1, size)
        got = 0
        while got < size:
            n = os.preadv(fd, [memoryview(view)[got:]], got)
            if n == 0:
                break
            got += n
        return bytes(view[:got])
    finally:
        os.close(fd)


# 1. Direct-I/O write of each block to a temp name, then rename into place (how engines publish a block).
i = -1
try:
    for i, d in enumerate(data):
        tmp = os.path.join(work, f"blk{i}.tmp")
        write_direct(tmp, d)
        os.rename(tmp, os.path.join(work, f"blk{i}"))
    report(True, f"direct-I/O write + rename of {blocks} blocks of {kib} KiB")
except OSError as e:
    report(False, f"direct-I/O write + rename failed at block {i} of {blocks}: {e}")
    sys.exit(1)

leftover = [n for n in os.listdir(work) if n.endswith(".tmp")]
report(not leftover, "no temp files left after rename")

# 2. Byte-exact read-back, direct and buffered.
for direct in (True, False):
    label = "direct" if direct else "buffered"
    bad = [i for i in range(blocks) if hashlib.sha256(read_all(os.path.join(work, f"blk{i}"), direct)).hexdigest() != digest[i]]
    report(not bad, f"{label} read-back is byte-exact" + (f" (bad blocks {bad[:5]})" if bad else ""))

# 3. Concurrent readers over random blocks (many requests in flight, as a serving fleet does).
errors = []


def reader(seed):
    r = random.Random(seed)
    for _ in range(blocks):
        i = r.randrange(blocks)
        try:
            if hashlib.sha256(read_all(os.path.join(work, f"blk{i}"), r.random() < 0.5)).hexdigest() != digest[i]:
                errors.append(f"block {i} differs")
        except OSError as e:
            errors.append(f"block {i}: {e}")


threads = [threading.Thread(target=reader, args=(s,)) for s in range(readers)]
[t.start() for t in threads]
[t.join() for t in threads]
report(not errors, f"{readers} concurrent readers read every block byte-exactly" + (f" ({errors[:3]})" if errors else ""))

# 4. A published block is replaced atomically: a reader sees the old or the new bytes, never a mix.
path = os.path.join(work, "blk0")
new = rng.randbytes(size)
stop, torn = threading.Event(), []


def watch():
    while not stop.is_set():
        try:
            got = read_all(path, False)
        except OSError:
            continue
        if got not in (data[0], new):
            torn.append(len(got))


w = threading.Thread(target=watch)
w.start()
for _ in range(5):
    write_direct(path + ".tmp", new)
    os.rename(path + ".tmp", path)
    write_direct(path + ".tmp", data[0])
    os.rename(path + ".tmp", path)
stop.set()
w.join()
report(not torn, "rename-over replacement never exposes a partial block")

# 5. Deletion frees the name.
os.unlink(os.path.join(work, "blk1"))
report(not os.path.exists(os.path.join(work, "blk1")), "unlink removes a block")
sys.exit(1 if failed else 0)
PY
grep -q '^FAIL' "$LOCAL/files.out" && FAILS=$((FAILS + $(grep -c '^FAIL' "$LOCAL/files.out")))

if [ -n "${S3_ENDPOINT:-}" ] && [ -n "${S3_BUCKET:-}" ] && [ -n "${S3_ACCESS_KEY:-}" ] && [ -n "${S3_SECRET_KEY:-}" ]; then
  KEY="kvcache-smoke/$$-blk"
  s3() { # s3 METHOD /path [curl args...]; prints the HTTP status
    local m=$1 p=$2
    shift 2
    curl -sS -o "$LOCAL/s3.body" -w '%{http_code}' --aws-sigv4 "aws:amz:us-east-1:s3" \
      -H "x-amz-content-sha256: UNSIGNED-PAYLOAD" --user "$S3_ACCESS_KEY:$S3_SECRET_KEY" -X "$m" "$S3_ENDPOINT$p" "$@"
  }
  head -c $((BLOCK_KIB * 1024)) /dev/urandom >"$LOCAL/s3.in"
  if [ "$(s3 PUT "/$S3_BUCKET/$KEY" --data-binary @"$LOCAL/s3.in")" = 200 ] \
     && [ "$(s3 GET "/$S3_BUCKET/$KEY")" = 200 ] && cmp -s "$LOCAL/s3.in" "$LOCAL/s3.body"; then
    pass "S3 put + get is byte-exact"
  else
    fail "S3 put + get is byte-exact"
  fi
  HALF=$((BLOCK_KIB * 512))
  if [ "$(s3 GET "/$S3_BUCKET/$KEY" -H "Range: bytes=$HALF-$((HALF + 4095))")" = 206 ] \
     && [ "$(tail -c +$((HALF + 1)) "$LOCAL/s3.in" | head -c 4096 | sha256sum | cut -d' ' -f1)" = "$(sha256sum <"$LOCAL/s3.body" | cut -d' ' -f1)" ]; then
    pass "S3 ranged get returns the requested bytes"
  else
    fail "S3 ranged get returns the requested bytes"
  fi
  s3 DELETE "/$S3_BUCKET/$KEY" >/dev/null || true
else
  skip "S3 checks (set S3_ENDPOINT, S3_BUCKET, S3_ACCESS_KEY and S3_SECRET_KEY)"
fi

[ "$FAILS" -eq 0 ] && echo "ALL CHECKS PASSED" || echo "$FAILS CHECK(S) FAILED"
exit $((FAILS > 0))
