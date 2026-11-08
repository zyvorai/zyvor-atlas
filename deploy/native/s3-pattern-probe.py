#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""CPU-only probes for the storage patterns described in docs/AI_STORAGE_RECIPES.md.

No third-party packages, no GPU, no LMCache, no vLLM. Two subcommands:

  s3         Replays, with plain HTTP and a hand-written SigV4 signer, the request shapes that LMCache's
             S3 L2 adapter issues (docs/AI_STORAGE_RECIPES.md, "What the S3 backend sends"): PUT with a
             Content-Type, HEAD, GET, part-sized ranged GETs, ListObjectsV2 paging, DELETE, optional
             multipart upload, against path-style or virtual-hosted-style addressing. The object keys have
             the shape LMCache builds. It then reports (INFO, never a failure) how many of several
             concurrent `If-None-Match: *` PUTs of one new key succeeded, which is the primitive Lance
             needs for concurrent writers on S3.
  link-race  Races several os.link() calls onto one destination name in a directory (an atlas-native
             mount, for example) and checks that exactly one wins and the rest get EEXIST. This is the
             create-if-absent primitive the Rust object_store crate's local backend uses.

It does not use the AWS CRT client LMCache uses, so a pass shows that the gateway accepts these request
shapes, not that LMCache works. It measures no throughput.

Credentials come from S3_ACCESS_KEY and S3_SECRET_KEY, never from arguments.
Exit status 0 when no check failed, 1 otherwise, 2 for a usage or connection error.
"""
import argparse
import concurrent.futures
import datetime
import hashlib
import hmac
import http.client
import os
import random
import shutil
import ssl
import sys
import tempfile
import urllib.parse
import xml.etree.ElementTree as ET

FAILS = []


def report(status, name):
    if status == "FAIL":
        FAILS.append(name)
    print("%-5s %s" % (status, name), flush=True)


# --------------------------------------------------------------------------- SigV4


def _hmac(key, msg):
    return hmac.new(key, msg.encode(), hashlib.sha256).digest()


def quote(text, safe=""):
    return urllib.parse.quote(text, safe=safe + "-_.~")


def sign_headers(method, host, path, query, headers, payload_hash, access_key, secret_key, region, now=None):
    """Return the request headers (including Authorization) for an S3 SigV4 request.

    `path` is the request path exactly as it will be sent, already percent-encoded once; `query` is a
    list of (name, value) pairs. Only the headers in `headers` plus host, x-amz-date and
    x-amz-content-sha256 are signed.
    """
    now = now or datetime.datetime.now(datetime.timezone.utc)
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")
    day = amz_date[:8]
    all_headers = {k.lower(): str(v).strip() for k, v in headers.items()}
    all_headers["host"] = host
    all_headers["x-amz-date"] = amz_date
    all_headers["x-amz-content-sha256"] = payload_hash
    names = sorted(all_headers)
    canonical_query = "&".join("%s=%s" % (quote(k), quote(v)) for k, v in sorted(query))
    canonical = "\n".join(
        [method, path, canonical_query, "".join("%s:%s\n" % (n, all_headers[n]) for n in names), ";".join(names), payload_hash]
    )
    scope = "%s/%s/s3/aws4_request" % (day, region)
    to_sign = "\n".join(["AWS4-HMAC-SHA256", amz_date, scope, hashlib.sha256(canonical.encode()).hexdigest()])
    key = _hmac(_hmac(_hmac(_hmac(("AWS4" + secret_key).encode(), day), region), "s3"), "aws4_request")
    signature = hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
    all_headers["authorization"] = "AWS4-HMAC-SHA256 Credential=%s/%s, SignedHeaders=%s, Signature=%s" % (
        access_key,
        scope,
        ";".join(names),
        signature,
    )
    return all_headers


# --------------------------------------------------------------------------- LMCache key shape


def lmcache_key(model, rank, group, chunk_hash_hex):
    """The object name LMCache gives an L2 object with no cache salt and no identity tags.

    From encode_storage_key in lmcache/v1/distributed/key_codec.py: "{model}@{rank}@{group}@{hash}" with
    rank as 8 hex digits, group as hex, and the model's slashes kept.
    """
    return "%s@%08x@%x@%s" % (model, rank, group, chunk_hash_hex)


# --------------------------------------------------------------------------- S3 client


class S3:
    def __init__(self, endpoint, bucket, access_key, secret_key, region, addressing, domain, cafile=None):
        url = urllib.parse.urlsplit(endpoint)
        if url.scheme not in ("http", "https") or not url.hostname:
            raise ValueError("endpoint must be http(s)://host[:port]")
        self.scheme, self.connect_host = url.scheme, url.hostname
        self.port = url.port or (443 if url.scheme == "https" else 80)
        self.bucket, self.access_key, self.secret_key, self.region = bucket, access_key, secret_key, region
        self.addressing = addressing
        self.ctx = ssl.create_default_context(cafile=cafile) if url.scheme == "https" else None
        default_port = 443 if url.scheme == "https" else 80
        port_part = "" if self.port == default_port else ":%d" % self.port
        if addressing == "virtual":
            if not domain:
                raise ValueError("--domain is required with --addressing virtual")
            # What a virtual-hosted client sends: the bucket is in the Host header, the connection still
            # goes to --endpoint, so no wildcard DNS is needed for this probe.
            self.host_header = "%s.%s%s" % (bucket, domain, port_part)
        else:
            self.host_header = "%s%s" % (self.connect_host, port_part)

    def path(self, key=""):
        quoted = quote(key, "/") if key else ""
        if self.addressing == "virtual":
            return "/" + quoted
        return "/" + self.bucket + ("/" + quoted if key else "")

    def request(self, method, key="", query=(), body=b"", headers=None, signed=("content-type", "range", "if-none-match")):
        headers = dict(headers or {})
        path = self.path(key)
        payload_hash = hashlib.sha256(body).hexdigest()
        to_sign = {k: v for k, v in headers.items() if k.lower() in signed}
        full = sign_headers(method, self.host_header, path, list(query), to_sign, payload_hash, self.access_key, self.secret_key, self.region)
        send = {k: v for k, v in headers.items() if k.lower() not in signed}
        send.update({k: v for k, v in full.items() if k != "host"})
        target = path + ("?" + "&".join("%s=%s" % (quote(k), quote(v)) for k, v in query) if query else "")
        if self.scheme == "https":
            conn = http.client.HTTPSConnection(self.connect_host, self.port, context=self.ctx, timeout=60)
        else:
            conn = http.client.HTTPConnection(self.connect_host, self.port, timeout=60)
        try:
            conn.putrequest(method, target, skip_host=True, skip_accept_encoding=True)
            conn.putheader("Host", self.host_header)
            for k, v in send.items():
                conn.putheader(k, v)
            if method in ("PUT", "POST"):
                conn.putheader("Content-Length", str(len(body)))
            conn.endheaders(body if body else None)
            resp = conn.getresponse()
            data = resp.read()
            return resp.status, {k.lower(): v for k, v in resp.getheaders()}, data
        finally:
            conn.close()


def parse_list(xml_bytes):
    root = ET.fromstring(xml_bytes)
    strip = lambda tag: tag.split("}", 1)[-1]  # noqa: E731  (the response may or may not carry a namespace)
    keys, token, truncated = [], None, False
    for el in root.iter():
        name = strip(el.tag)
        if name == "Key":
            keys.append(el.text or "")
        elif name == "NextContinuationToken":
            token = el.text
        elif name == "IsTruncated":
            truncated = (el.text or "").lower() == "true"
    return keys, token, truncated


# --------------------------------------------------------------------------- the s3 checks


def run_s3(args):
    access, secret = os.environ.get("S3_ACCESS_KEY", ""), os.environ.get("S3_SECRET_KEY", "")
    if not access or not secret:
        print("set S3_ACCESS_KEY and S3_SECRET_KEY", file=sys.stderr)
        return 2
    s3 = S3(args.endpoint, args.bucket, access, secret, args.region, args.addressing, args.domain, args.cafile)
    rng = random.Random(11)
    size = args.object_mib * 1024 * 1024
    run = "%08x" % rng.getrandbits(32)
    model = "probe-org/probe-model-%s" % run  # LMCache keeps the slash in the model name, so this is a directory
    made = []

    def key(i):
        k = lmcache_key(model, 0, 0, hashlib.sha256(b"%d-%s" % (i, run.encode())).hexdigest())
        made.append(k)
        return k

    mode = args.addressing + (" (Host: %s)" % s3.host_header if args.addressing == "virtual" else "")
    print("# addressing: %s; object %d MiB; key shape %s" % (mode, args.object_mib, lmcache_key(model, 0, 0, "<64 hex>")))
    try:
        st, _, _ = s3.request("HEAD", "does-not-exist-%s" % run)
    except OSError as err:
        print("cannot reach %s: %s" % (args.endpoint, err), file=sys.stderr)
        return 2
    if st in (401, 403):
        report("FAIL", "credentials or Host header refused (HTTP %d) on a HEAD of a missing key" % st)
        return 1
    report("PASS" if st == 404 else "FAIL", "HEAD of a missing key is 404 (got %d)" % st)

    k0 = key(0)
    blob = rng.randbytes(size) if hasattr(rng, "randbytes") else bytes(rng.getrandbits(8) for _ in range(size))
    st, _, body = s3.request("PUT", k0, body=blob, headers={"Content-Type": "application/octet-stream"})
    report("PASS" if st in (200, 201) else "FAIL", "PUT with Content-Type application/octet-stream (got %d %s)" % (st, body[:120].decode("latin1")))

    st, hdr, _ = s3.request("HEAD", k0)
    report("PASS" if st == 200 and hdr.get("content-length") == str(size) else "FAIL", "HEAD returns Content-Length %d (got %s %s)" % (size, st, hdr.get("content-length")))

    st, _, got = s3.request("GET", k0)
    report("PASS" if st == 200 and got == blob else "FAIL", "GET returns the exact bytes (got %d, %d bytes)" % (st, len(got)))

    part = max(1, args.part_kib) * 1024
    ranges = [(o, min(o + part, size) - 1) for o in range(0, size, part)]

    def ranged(r):
        st_, _, data = s3.request("GET", k0, headers={"Range": "bytes=%d-%d" % r})
        return r, st_, data

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        results = list(pool.map(ranged, ranges))
    ok = all(st_ == 206 and data == blob[r[0] : r[1] + 1] for r, st_, data in results)
    report("PASS" if ok else "FAIL", "%d part-sized ranged GETs (%d KiB, 8 in flight) answer 206 with the right bytes" % (len(ranges), args.part_kib))

    new = rng.randbytes(size) if hasattr(rng, "randbytes") else blob[::-1]
    st, _, _ = s3.request("PUT", k0, body=new, headers={"Content-Type": "application/octet-stream"})
    st2, _, got = s3.request("GET", k0)
    report("PASS" if st in (200, 201) and st2 == 200 and got == new else "FAIL", "overwriting a key with a new PUT replaces the object")

    # ListObjectsV2 paging over a prefix (LMCache lists with list-type=2, max-keys, prefix, continuation-token).
    for i in range(1, 5):
        s3.request("PUT", key(i), body=b"x" * 4096, headers={"Content-Type": "application/octet-stream"})
    seen, token, pages = [], None, 0
    while True:
        query = [("list-type", "2"), ("max-keys", "2"), ("prefix", model + "@")]
        if token:
            query.append(("continuation-token", token))
        st, _, xml = s3.request("GET", "", query=query)
        if st != 200:
            break
        keys, token, truncated = parse_list(xml)
        seen += keys
        pages += 1
        if not truncated or not token or pages > 20:
            break
    report("PASS" if st == 200 and sorted(seen) == sorted(made) and pages >= 3 else "FAIL", "ListObjectsV2 with prefix, max-keys=2 and continuation tokens returns all %d keys in %d pages" % (len(made), pages))

    if not args.no_multipart:
        mk = key(99)
        sizes = [5 * 1024 * 1024, 1024 * 1024]
        parts = [rng.randbytes(s) if hasattr(rng, "randbytes") else b"p" * s for s in sizes]
        st, _, xml = s3.request("POST", mk, query=[("uploads", "")])
        upload = None
        if st == 200:
            for el in ET.fromstring(xml).iter():
                if el.tag.split("}", 1)[-1] == "UploadId":
                    upload = el.text
        etags = []
        for n, p in enumerate(parts, 1):
            st, hdr, _ = s3.request("PUT", mk, query=[("partNumber", str(n)), ("uploadId", upload or "")], body=p)
            etags.append(hdr.get("etag", ""))
        doc = "<CompleteMultipartUpload>%s</CompleteMultipartUpload>" % "".join("<Part><PartNumber>%d</PartNumber><ETag>%s</ETag></Part>" % (n, e) for n, e in enumerate(etags, 1))
        st, _, resp = s3.request("POST", mk, query=[("uploadId", upload or "")], body=doc.encode())
        st2, _, got = s3.request("GET", mk)
        report("PASS" if upload and st == 200 and st2 == 200 and got == b"".join(parts) else "FAIL", "multipart upload (5 MiB + 1 MiB parts) completes and reads back exactly (complete=%s get=%s)" % (st, st2))

    deleted = [s3.request("DELETE", k)[0] for k in made]
    gone = [s3.request("HEAD", k)[0] for k in made]
    report("PASS" if all(d in (200, 204) for d in deleted) and all(g == 404 for g in gone) else "FAIL", "DELETE removes every probe key (statuses %s, then HEAD %s)" % (sorted(set(deleted)), sorted(set(gone))))

    # Informational: the create-only PUT Lance relies on for concurrent writers on S3.
    race_key = "probe-org/race-%s" % run
    def create_only(i):
        return s3.request("PUT", race_key, body=b"writer-%d" % i, headers={"If-None-Match": "*"}, signed=("if-none-match",))[0]
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.race_threads) as pool:
        codes = list(pool.map(create_only, range(args.race_threads)))
    s3.request("DELETE", race_key)
    won = sum(1 for c in codes if c in (200, 201))
    print("INFO  %d of %d concurrent PUTs of one new key with If-None-Match: * succeeded (statuses %s); a store that enforces the condition atomically lets exactly 1 win" % (won, len(codes), sorted(codes)))
    return 1 if FAILS else 0


# --------------------------------------------------------------------------- link-race


def _read(path):
    with open(path, "rb") as f:
        return f.read()


def run_link_race(args):
    if not os.path.isdir(args.dir):
        print("not a directory: %s" % args.dir, file=sys.stderr)
        return 2
    work = tempfile.mkdtemp(prefix=".link-race.", dir=args.dir)
    try:
        bad = []
        for rnd in range(args.rounds):
            dest = os.path.join(work, "dest-%d" % rnd)
            staged = []
            for t in range(args.threads):
                path = os.path.join(work, "staged-%d-%d" % (rnd, t))
                with open(path, "wb") as f:
                    f.write(b"writer-%d-%d" % (rnd, t))
                    f.flush()
                    os.fsync(f.fileno())
                staged.append(path)

            def attempt(path):
                try:
                    os.link(path, dest)
                    return "won"
                except FileExistsError:
                    return "exists"
                except OSError as err:
                    return "error:%s" % err.errno

            with concurrent.futures.ThreadPoolExecutor(max_workers=args.threads) as pool:
                outcomes = list(pool.map(attempt, staged))
            winners = [p for p, o in zip(staged, outcomes) if o == "won"]
            content = _read(dest) if os.path.exists(dest) else None
            ok = len(winners) == 1 and all(o in ("won", "exists") for o in outcomes) and content == _read(winners[0])
            if not ok:
                bad.append((rnd, sorted(set(outcomes)), len(winners)))
        report("PASS" if not bad else "FAIL", "%d rounds of %d concurrent os.link() onto one name: exactly one wins, the others get EEXIST%s" % (args.rounds, args.threads, "" if not bad else " (bad rounds %s)" % bad[:3]))
    finally:
        shutil.rmtree(work, ignore_errors=True)
    return 1 if FAILS else 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("s3", help="replay LMCache S3 adapter request shapes against an S3 endpoint")
    s.add_argument("--endpoint", required=True, help="http(s)://host:port the gateway listens on")
    s.add_argument("--bucket", required=True)
    s.add_argument("--region", default="us-east-1")
    s.add_argument("--addressing", choices=("path", "virtual"), default="path")
    s.add_argument("--domain", help="virtual-hosted domain: the Host header becomes <bucket>.<domain>[:port]")
    s.add_argument("--cafile", help="CA bundle for an https endpoint")
    s.add_argument("--object-mib", type=int, default=8)
    s.add_argument("--part-kib", type=int, default=1024, help="size of each ranged GET")
    s.add_argument("--race-threads", type=int, default=8)
    s.add_argument("--no-multipart", action="store_true")
    s.set_defaults(fn=run_s3)
    l = sub.add_parser("link-race", help="race os.link() onto one name in a directory")
    l.add_argument("dir")
    l.add_argument("--rounds", type=int, default=20)
    l.add_argument("--threads", type=int, default=8)
    l.set_defaults(fn=run_link_race)
    args = ap.parse_args(argv)
    try:
        rc = args.fn(args)
    except ValueError as err:
        print(err, file=sys.stderr)
        return 2
    print("ALL CHECKS PASSED" if rc == 0 else ("%d CHECK(S) FAILED" % len(FAILS) if rc == 1 else "ERROR"))
    return rc


if __name__ == "__main__":
    sys.exit(main())
