# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Streaming model/dataset transfer and serving-engine KV adapter."""
import base64
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import shutil
import ssl
import tempfile
import urllib.error
import urllib.parse
import urllib.request

from .core import CHUNK, DataError, canonical, logical_path, require, sha


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class Client:
    def __init__(self, url, token, ca=None):
        p = urllib.parse.urlsplit(url)
        require(p.scheme in ("http", "https") and p.hostname and not p.username and not p.password
                and not p.query and not p.fragment, "invalid service URL")
        try:
            loopback = ipaddress.ip_address(p.hostname).is_loopback
        except ValueError:
            loopback = p.hostname == "localhost"
        require(p.scheme == "https" or loopback, "HTTPS required for remote service")
        require(isinstance(token, str) and 32 <= len(token) <= 512 and "\n" not in token and "\r" not in token,
                "a bearer token of at least 32 characters is required")
        self.url, self.token = url.rstrip("/"), token
        self.opener = urllib.request.build_opener(NoRedirect(), urllib.request.HTTPSHandler(
            context=ssl.create_default_context(cafile=ca)))

    def request(self, method, path, data=None, binary=False, headers=None):
        req = urllib.request.Request(self.url + path, data=data, method=method,
                                     headers={"Authorization": "Bearer " + self.token, **(headers or {})})
        try:
            with self.opener.open(req, timeout=600) as response:
                content = response.read(16 * 1024**2 + 1)
                require(len(content) <= 16 * 1024**2, "response too large", 413)
                return content if binary else json.loads(content)
        except urllib.error.HTTPError as exc:
            raise DataError(f"Atlas data HTTP {exc.code}", exc.code) from None

    def call(self, action, **arguments):
        return self.request("POST", "/v1/rpc/" + action, canonical(arguments).encode(),
                            headers={"Content-Type": "application/json"})

    def upload(self, directory):
        root = Path(directory).resolve(strict=True)
        require(root.is_dir(), "source must be a directory")
        files = {}
        for path in sorted(root.rglob("*")):
            require(not path.is_symlink(), "symlinks not accepted")
            if path.is_dir():
                continue
            require(path.is_file(), "source must contain regular files")
            hasher, chunks, size = hashlib.sha256(), [], 0
            with path.open("rb") as stream:
                while data := stream.read(CHUNK):
                    h = sha(data)
                    self.request("POST", "/v1/chunks", data, headers={"X-Content-SHA256": h})
                    hasher.update(data)
                    chunks.append(h)
                    size += len(data)
            files[path.relative_to(root).as_posix()] = {"chunks": chunks, "sha256": hasher.hexdigest(), "size": size}
        return files

    def download(self, artifact, destination, chunk_cache=None):
        asset = self.call("artifacts.get", key=artifact)
        target = Path(destination)
        require(not target.exists(), "destination must not exist", 409)
        target.parent.mkdir(parents=True, exist_ok=True)
        network_bytes = 0
        staging = Path(tempfile.mkdtemp(prefix=".atlas-download-", dir=target.parent))
        try:
            for name, entry in asset["files"].items():
                path = staging / logical_path(name)
                path.parent.mkdir(parents=True, exist_ok=True)
                hasher, size = hashlib.sha256(), 0
                with path.open("xb") as stream:
                    for h in entry["chunks"]:
                        cached = Path(chunk_cache) / h if chunk_cache else None
                        if cached and cached.is_file() and not cached.is_symlink():
                            data = cached.read_bytes()
                            require(sha(data) == h, "local chunk cache corruption", 422)
                        else:
                            data = self.request("GET", f"/v1/chunks/{h}?artifact={artifact}", binary=True)
                            require(sha(data) == h, "download chunk checksum mismatch", 422)
                            network_bytes += len(data)
                            if cached:
                                cached.parent.mkdir(parents=True, exist_ok=True)
                                fd, temp = tempfile.mkstemp(dir=cached.parent)
                                with os.fdopen(fd, "wb") as cache_file:
                                    cache_file.write(data)
                                os.replace(temp, cached)
                        stream.write(data)
                        hasher.update(data)
                        size += len(data)
                require(size == entry["size"] and hasher.hexdigest() == entry["sha256"],
                        "download file checksum mismatch", 422)
            os.rename(staging, target)
        except BaseException:
            shutil.rmtree(staging)
            raise
        return {"verified": True, "bytes": asset["size"], "network_bytes": network_bytes, "path": str(target)}

    def stage(self, job, node, cache):
        """Trusted node worker: download/check bytes before acknowledging placement.

        Cache entries are reverified every stage, including after a worker restart.
        """
        decision = self.call("jobs.status", job=job)
        require(decision["node"] == node, "job reserved on another node", 409)
        cache = Path(cache)
        cache.mkdir(parents=True, exist_ok=True)
        for key in decision["artifacts"]:
            # Separate tenant/node caches by operator configuration; do not export this directory.
            target = cache / key
            try:
                if target.exists():
                    asset = self.call("artifacts.get", key=key)
                    for name, entry in asset["files"].items():
                        p = target / logical_path(name)
                        require(not p.is_symlink() and p.is_file(), "cached file missing", 422)
                        hasher, size = hashlib.sha256(), 0
                        with p.open("rb") as stream:
                            while part := stream.read(CHUNK):
                                hasher.update(part)
                                size += len(part)
                        require(size == entry["size"] and hasher.hexdigest() == entry["sha256"],
                                "cached file checksum mismatch", 422)
                else:
                    self.download(key, target, cache / ".chunks")
            except (DataError, OSError):
                self.call("nodes.invalidate", node=node, artifact_id=key)
                raise
            self.call("nodes.staged", node=node, artifact_id=key)
        return self.call("jobs.status", job=job)


def prefix_digests(token_ids, block_size):
    """Chained digests of the full token blocks of a prompt, as prefix caches key them.

    Digest i covers every token up to the end of block i (it folds in digest i-1), so two prompts
    share digest i only if they share all of the first i+1 blocks. A trailing partial block gets
    no digest: it cannot be reused until it fills. Token ids must be the exact ids the engine
    prefilled; text similarity is not a compatibility test.
    """
    require(isinstance(block_size, int) and not isinstance(block_size, bool) and 0 < block_size <= 4096,
            "block_size must be 1..4096")
    require(isinstance(token_ids, (list, tuple)) and all(isinstance(t, int) and not isinstance(t, bool)
                                                       and 0 <= t < 2**32 for t in token_ids),
            "token ids must be unsigned 32-bit integers")
    digests, previous = [], b""
    for start in range(0, len(token_ids) - block_size + 1, block_size):
        block = token_ids[start:start + block_size]
        previous = hashlib.sha256(previous + b"".join(t.to_bytes(4, "big") for t in block)).digest()
        digests.append(previous.hex())
    return digests


class KVAdapter:
    """Portable exact-key byte-block adapter; serving engines must serialize their own tensors.

    No inference acceleration is claimed without engine-specific integration and benchmarks.
    """
    def __init__(self, client, model, tokenizer, adapter, layout):
        self.client = client
        self.compatibility = dict(model=model, tokenizer=tokenizer, adapter=adapter, layout=layout)

    def put(self, prefix_digest, block, ttl=3600):
        return self.client.call("kv.put", compatibility={**self.compatibility, "prefix": prefix_digest},
                                data=base64.b64encode(block).decode(), ttl=ttl)

    def get(self, prefix_digest):
        try:
            result = self.client.call("kv.get", compatibility={**self.compatibility, "prefix": prefix_digest})
            return base64.b64decode(result["data"], validate=True)
        except DataError as exc:
            if exc.status == 404:
                return None
            raise

    def put_blocks(self, digests, blocks, ttl=3600):
        """Stores one serialized block per prefix digest (same length, same order)."""
        require(len(digests) == len(blocks), "one block per digest")
        for digest, block in zip(digests, blocks):
            self.put(digest, block, ttl)

    def longest_prefix(self, digests):
        """Cached blocks for the longest leading run of `digests`; stops at the first miss.

        Blocks after a miss are not looked up: a prefix cache is only valid as an unbroken
        prefix. The caller prefills the rest. Returns the list of blocks (possibly empty).
        """
        found = []
        for digest in digests:
            block = self.get(digest)
            if block is None:
                break
            found.append(block)
        return found
