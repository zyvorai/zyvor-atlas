#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Checks conditional writes and object tagging of running atlas-native S3 gateways with aws-cli.
# Needs aws-cli 2 with `s3api put-object --if-match`, and credentials in the usual AWS_* variables.
# Creates objects under a fresh prefix and leaves one retention-locked for 2 minutes.
# Usage: deploy/native/s3-conditional-check.sh <bucket> <endpoint-url> [<second-gateway-url>]
set -uo pipefail

B=${1:?usage: s3-conditional-check.sh bucket endpoint [endpoint2]}
E1=${2:?usage: s3-conditional-check.sh bucket endpoint [endpoint2]}
E2=${3:-$E1}
P=cond-check-$(date +%s)-$$
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
PASS=0
FAIL=0

s3() { aws --endpoint-url "$E1" s3api "$@"; }
s3b() { aws --endpoint-url "$E2" s3api "$@"; }
report() { # name, exit status
  if [ "$2" -eq 0 ]; then
    PASS=$((PASS + 1))
    echo "ok   $1"
  else
    FAIL=$((FAIL + 1))
    echo "FAIL $1"
    sed 's/^/     /' "$W/out" | head -5
  fi
}
# ok <name> <command...>: the command succeeds.
ok() {
  local name=$1
  shift
  "$@" >"$W/out" 2>&1
  report "$name" $?
}
# refused <name> <error code> <command...>: the command fails with that code.
refused() {
  local name=$1 code=$2
  shift 2
  if "$@" >"$W/out" 2>&1; then
    report "$name" 1
  else
    grep -q "$code" "$W/out"
    report "$name" $?
  fi
}
# equals <name> <wanted> <command...>
equals() {
  local name=$1 want=$2
  shift 2
  "$@" >"$W/out" 2>&1 && [ "$(cat "$W/out")" = "$want" ]
  report "$name" $?
}
etag() { s3 head-object --bucket "$B" --key "$1" --query ETag --output text; }
body() { s3 get-object --bucket "$B" --key "$1" "$W/body" >/dev/null && cat "$W/body"; }
tags() { s3 get-object-tagging --bucket "$B" --key "$1" --query 'TagSet[].[Key,Value]' --output text | tr '\t' '=' | paste -sd, -; }
put() { # key, file, extra arguments
  local key=$1 file=$2
  shift 2
  s3 put-object --bucket "$B" --key "$key" --body "$file" "$@"
}

printf 'v1' >"$W/v1"
printf 'v2' >"$W/v2"

# The first conditional write waits until the cluster has heard every metadata node.
for _ in $(seq 1 30); do
  put "$P/warm" "$W/v1" --if-none-match '*' >"$W/out" 2>&1 && break
  grep -q ServiceUnavailable "$W/out" || break
  sleep 1
done

ok "create with If-None-Match" put "$P/a" "$W/v1" --if-none-match '*'
refused "second create is refused" PreconditionFailed put "$P/a" "$W/v2" --if-none-match '*'
equals "content unchanged" v1 body "$P/a"
FIRST=$(etag "$P/a")
refused "If-Match with a wrong ETag" PreconditionFailed put "$P/a" "$W/v2" --if-match '"0000"'
ok "If-Match with the current ETag" put "$P/a" "$W/v2" --if-match "$FIRST"
refused "If-Match with the old ETag" PreconditionFailed put "$P/a" "$W/v1" --if-match "$FIRST"
refused "If-Match on a missing key" NoSuchKey put "$P/missing" "$W/v1" --if-match "$FIRST"
equals "content replaced once" v2 body "$P/a"

for round in 1 2 3; do
  pids=()
  for i in $(seq 1 8); do
    url=$E1
    [ $((i % 2)) -eq 0 ] && url=$E2
    printf 'writer %s' "$i" >"$W/w$i"
    aws --endpoint-url "$url" s3api put-object --bucket "$B" --key "$P/lock$round" \
      --body "$W/w$i" --if-none-match '*' >"$W/race$i" 2>&1 &
    pids+=($!)
  done
  wins=0
  losses=0
  winner=
  for i in $(seq 1 8); do
    if wait "${pids[$((i - 1))]}"; then
      wins=$((wins + 1))
      winner=$i
    elif grep -q PreconditionFailed "$W/race$i"; then
      losses=$((losses + 1))
    fi
  done
  echo "$wins winners, $losses refused" >"$W/out"
  [ "$wins" -eq 1 ] && [ "$losses" -eq 7 ]
  report "round $round: one of 8 concurrent creates through both gateways wins" $?
  equals "round $round: the winner's content" "writer $winner" body "$P/lock$round"
done

# Tagging.
ok "put with x-amz-tagging" put "$P/t" "$W/v1" --tagging 'team=ml&stage=raw'
equals "tags read back" 'team=ml,stage=raw' tags "$P/t"
BEFORE=$(etag "$P/t")
ok "put-object-tagging" s3 put-object-tagging --bucket "$B" --key "$P/t" \
  --tagging 'TagSet=[{Key=owner,Value=a b}]'
equals "tags replaced" 'owner=a b' tags "$P/t"
equals "ETag unchanged by tagging" "$BEFORE" etag "$P/t"
ELEVEN=$(for i in $(seq 1 11); do printf '{Key=k%s,Value=v},' "$i"; done)
refused "11 tags" InvalidTag s3 put-object-tagging --bucket "$B" --key "$P/t" \
  --tagging "TagSet=[${ELEVEN%,}]"
ok "copy keeps tags" s3 copy-object --bucket "$B" --key "$P/t2" --copy-source "$B/$P/t"
equals "copied tags" 'owner=a b' tags "$P/t2"
ok "copy replaces tags" s3 copy-object --bucket "$B" --key "$P/t3" --copy-source "$B/$P/t" \
  --tagging-directive REPLACE --tagging 'new=1'
equals "replaced tags" 'new=1' tags "$P/t3"
ok "delete-object-tagging" s3 delete-object-tagging --bucket "$B" --key "$P/t"
equals "no tags left" '' tags "$P/t"
ok "copy onto an existing key with If-None-Match is judged" \
  bash -c "! aws --endpoint-url '$E1' s3api copy-object --bucket '$B' --key '$P/a' \
    --copy-source '$B/$P/t' --if-none-match '*'"

# A multipart copy by the high-level client reads the source's tags and writes them.
head -c $((20 << 20)) /dev/urandom >"$W/big"
ok "multipart upload" put "$P/big" "$W/big" --tagging 'kind=big'
ok "multipart copy with aws s3 cp" aws --endpoint-url "$E1" s3 cp --only-show-errors \
  "s3://$B/$P/big" "s3://$B/$P/big-copy"
equals "multipart copy kept tags" 'kind=big' tags "$P/big-copy"
ok "multipart copy downloads" aws --endpoint-url "$E2" s3 cp --only-show-errors \
  "s3://$B/$P/big-copy" "$W/back"
equals "multipart copy content" "$(sha256sum <"$W/big")" bash -c "sha256sum <'$W/back'"

# Conditional multipart completion, from the second gateway.
upload() { # key: prints the upload id and the part's ETag
  local id part
  id=$(s3 create-multipart-upload --bucket "$B" --key "$1" --tagging 'mp=1' \
    --query UploadId --output text) || return 1
  part=$(s3 upload-part --bucket "$B" --key "$1" --upload-id "$id" --part-number 1 \
    --body "$W/v1" --query ETag --output text) || return 1
  echo "$id ${part//\"/}"
}
finish() { # key, id, part etag, extra arguments
  local key=$1 id=$2 tag=$3
  shift 3
  s3b complete-multipart-upload --bucket "$B" --key "$key" --upload-id "$id" \
    --multipart-upload "{\"Parts\":[{\"PartNumber\":1,\"ETag\":\"$tag\"}]}" "$@"
}
read -r ID TAG <<<"$(upload "$P/mp")"
ok "conditional completion of a new key" finish "$P/mp" "$ID" "$TAG" --if-none-match '*'
equals "multipart tags" 'mp=1' tags "$P/mp"
read -r ID TAG <<<"$(upload "$P/mp")"
refused "conditional completion over it" PreconditionFailed \
  finish "$P/mp" "$ID" "$TAG" --if-none-match '*'
ok "completion with If-Match" finish "$P/mp" "$ID" "$TAG" --if-match "$(etag "$P/mp")"

refused "conditional delete is not silently unconditional" NotImplemented \
  s3 delete-object --bucket "$B" --key "$P/a" --if-match '"0000"'
equals "object still there" v2 body "$P/a"

# Retention still holds.
UNTIL=$(date -u -d '+2 minutes' +%Y-%m-%dT%H:%M:%SZ 2>/dev/null ||
  date -u -v+2M +%Y-%m-%dT%H:%M:%SZ)
ok "put-object-retention" s3 put-object-retention --bucket "$B" --key "$P/a" \
  --retention "Mode=COMPLIANCE,RetainUntilDate=$UNTIL"
refused "locked: If-Match is refused" AccessDenied \
  put "$P/a" "$W/v1" --if-match "$(etag "$P/a")"
refused "locked: plain put is refused" AccessDenied put "$P/a" "$W/v1"
ok "locked: tagging is allowed" s3 put-object-tagging --bucket "$B" --key "$P/a" \
  --tagging 'TagSet=[{Key=held,Value=yes}]'
equals "locked content unchanged" v2 body "$P/a"

echo "passed=$PASS failed=$FAIL"
[ "$FAIL" -eq 0 ]
