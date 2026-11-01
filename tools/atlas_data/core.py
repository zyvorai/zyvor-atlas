# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Tenant-isolated content storage and transactional AI data workflows.

No background promises: readiness follows verified bytes, never a reported cache hint.
All metadata changes and audit events commit together in one catalog transaction (SQLite, or Postgres
with `ATLAS_DATA_DATABASE_URL=postgres://...` for several service replicas; see catalog.py).
"""
from __future__ import annotations

import contextlib
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import tempfile
import threading
import time
import uuid

from .catalog import open_catalog

CHUNK = 4 * 1024 * 1024
MAX_FILES = 10000


class DataError(Exception):
    def __init__(self, message, status=400):
        super().__init__(message)
        self.status = status


def require(ok, message, status=400):
    if not ok:
        raise DataError(message, status)


def ident(value):
    require(isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", value),
            "invalid identifier")
    return value


def digest(value):
    require(isinstance(value, str) and re.fullmatch(r"[a-f0-9]{64}", value), "invalid SHA-256")
    return value


def logical_path(value):
    require(isinstance(value, str) and 0 < len(value) <= 1024 and "\\" not in value
            and "\x00" not in value, "invalid logical path")
    p = PurePosixPath(value)
    require(not p.is_absolute() and all(x not in (".", "..", "") for x in value.split("/")),
            "path must be relative and normalized")
    return value


def number(value, low=0, high=2**53):
    require(type(value) is int and low <= value <= high, "integer outside allowed range")
    return value


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False)


def sha(data):
    return hashlib.sha256(data).hexdigest()


@dataclass(frozen=True)
class Actor:
    tenant: str
    subject: str
    role: str = "viewer"

    def write(self):
        require(self.role in ("operator", "admin"), "operator role required", 403)

    def admin(self):
        require(self.role == "admin", "admin role required", 403)


class Store:
    """Single service process with serialized writes and durable metadata + immutable chunks.

    The root must be private to the service account. With SQLite the catalog is a single-process file
    and the service is not HA. With a Postgres catalog, replicas share the database and must share the
    root (a ReadWriteMany volume) for the chunk bytes. Put that volume on an Atlas backend.
    """
    def __init__(self, root, clock=time.time, capacity_bytes=100 * 1024**3, database_url=None):
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.lock = threading.RLock()
        self.clock = clock
        self.capacity = number(capacity_bytes, 1)
        self.kv_capacity = min(self.capacity, 64 * 1024**2)
        self.db = open_catalog(self.root, database_url or os.environ.get("ATLAS_DATA_DATABASE_URL"))

    def close(self):
        self.db.close()

    @contextlib.contextmanager
    def transaction(self):
        with self.lock:
            self.db.begin()
            try:
                yield
                self.db.commit()
            except BaseException:
                self.db.rollback()
                raise

    def _get(self, a, kind, key):
        ident(key)
        row = self.db.execute("SELECT body FROM resources WHERE tenant=? AND kind=? AND id=?",
                              (a.tenant, kind, key)).fetchone()
        require(row is not None, "resource not found", 404)
        return json.loads(row[0])

    def _put(self, a, kind, key, body, create=False):
        if create:
            try:
                self.db.execute("INSERT INTO resources VALUES (?,?,?,?)",
                                (a.tenant, kind, key, canonical(body)))
            except self.db.IntegrityError as exc:
                raise DataError("resource already exists", 409) from exc
        else:
            self.db.execute("INSERT INTO resources VALUES (?,?,?,?) ON CONFLICT(tenant,kind,id) "
                            "DO UPDATE SET body=excluded.body", (a.tenant, kind, key, canonical(body)))

    def _list(self, a, kind):
        return [json.loads(r[0]) for r in self.db.execute(
            "SELECT body FROM resources WHERE tenant=? AND kind=? ORDER BY id", (a.tenant, kind))]

    def _audit(self, a, action, key):
        self.db.execute("INSERT INTO events(tenant,subject,action,resource,at) VALUES (?,?,?,?,?)",
                        (a.tenant, a.subject, action, key, self.clock()))

    def audit(self, a):
        a.admin()
        with self.lock:
            return [dict(r) for r in self.db.execute(
                "SELECT * FROM events WHERE tenant=? ORDER BY seq DESC LIMIT 1000", (a.tenant,))]

    def _chunk_path(self, a, h):
        # Hash tenant IDs too: no supplied string becomes an on-disk directory name.
        return self.root / "chunks" / sha(a.tenant.encode()) / digest(h)

    def put_chunk(self, a, data, expected=None):
        a.write()
        require(isinstance(data, bytes) and 0 < len(data) <= CHUNK, "chunk must be 1 byte to 4 MiB")
        h = sha(data)
        if expected is not None:
            require(h == digest(expected), "checksum mismatch", 422)
        with self.transaction():
            old = self.db.execute("SELECT size FROM chunks WHERE tenant=? AND hash=?", (a.tenant, h)).fetchone()
            if old:
                self.db.execute("INSERT OR IGNORE INTO chunk_owners VALUES (?,?,?)", (a.tenant, h, a.subject))
                require(self._chunk_path(a, h).read_bytes() == data, "stored chunk corruption", 422)
                os.utime(self._chunk_path(a, h), None)
                return {"hash": h, "size": len(data), "deduplicated": True}
            used = self.db.execute("SELECT COALESCE(SUM(size),0) FROM chunks").fetchone()[0]
            require(used + len(data) <= self.capacity, "service storage capacity exceeded", 507)
            target = self._chunk_path(a, h)
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            fd, temp = tempfile.mkstemp(dir=target.parent)
            try:
                with os.fdopen(fd, "wb") as f:
                    f.write(data)
                    f.flush()
                    os.fsync(f.fileno())
                os.replace(temp, target)
                dfd = os.open(target.parent, os.O_RDONLY)
                try:
                    os.fsync(dfd)
                finally:
                    os.close(dfd)
            finally:
                if os.path.exists(temp):
                    os.unlink(temp)
            self.db.execute("INSERT INTO chunks VALUES (?,?,?)", (a.tenant, h, len(data)))
            self.db.execute("INSERT OR IGNORE INTO chunk_owners VALUES (?,?,?)", (a.tenant, h, a.subject))
            self._audit(a, "chunk.put", h)
        return {"hash": h, "size": len(data), "deduplicated": False}

    def chunk(self, a, h):
        with self.lock:
            row = self.db.execute("SELECT size FROM chunks WHERE tenant=? AND hash=?",
                                  (a.tenant, digest(h))).fetchone()
            require(row is not None, "chunk not found", 404)
            try:
                data = self._chunk_path(a, h).read_bytes()
            except FileNotFoundError as exc:
                raise DataError("chunk missing on disk", 422) from exc
            require(len(data) == row[0] and sha(data) == h, "chunk checksum mismatch", 422)
            return data

    def _files(self, a, files, verify=True):
        require(isinstance(files, dict) and len(files) <= MAX_FILES, "invalid file manifest")
        total = 0
        allowed = None
        if verify and a.role != "admin":
            allowed = {r[0] for r in self.db.execute("SELECT hash FROM chunk_owners WHERE tenant=? AND subject=?", (a.tenant, a.subject))}
            for asset in self._list(a, "artifact"):
                try:
                    self._access(a, asset)
                    allowed.update(h for e in asset["files"].values() for h in e["chunks"])
                except DataError:
                    continue
        for name, entry in files.items():
            logical_path(name)
            require(isinstance(entry, dict) and set(entry) == {"chunks", "size", "sha256"}, "invalid file entry")
            number(entry["size"])
            digest(entry["sha256"])
            require(isinstance(entry["chunks"], list) and len(entry["chunks"]) <= 65536, "invalid chunk list")
            if verify:
                hasher, count = hashlib.sha256(), 0
                for h in entry["chunks"]:
                    require(allowed is None or h in allowed, "chunk not accessible", 404)
                    part = self.chunk(a, h)
                    hasher.update(part)
                    count += len(part)
                require(count == entry["size"] and hasher.hexdigest() == entry["sha256"],
                        "file manifest checksum mismatch", 422)
            else:
                for h in entry["chunks"]:
                    digest(h)
            total += entry["size"]
        names = set(files)
        require(not any(str(parent) in names for name in names for parent in PurePosixPath(name).parents
                        if str(parent) != "."), "file/directory path collision")
        return total

    def ingest(self, a, directory):
        """Trusted local CLI helper; remote API cannot read server filesystem paths."""
        a.write()
        root = Path(directory).resolve(strict=True)
        require(root.is_dir(), "source must be a directory")
        files = {}
        for path in sorted(root.rglob("*")):
            require(not path.is_symlink(), "symlinks are not accepted")
            if path.is_dir():
                continue
            require(path.is_file(), "source must contain regular files")
            require(len(files) < MAX_FILES, "too many files")
            chunks, hasher, size = [], hashlib.sha256(), 0
            with path.open("rb") as stream:
                while part := stream.read(CHUNK):
                    chunks.append(self.put_chunk(a, part)["hash"])
                    hasher.update(part)
                    size += len(part)
            files[path.relative_to(root).as_posix()] = {"chunks": chunks, "size": size, "sha256": hasher.hexdigest()}
        return files

    def publish(self, a, kind, name, revision, files, readers=None, provenance=None):
        a.write()
        require(kind in ("model", "dataset"), "kind must be model or dataset")
        ident(name)
        ident(revision)
        readers = sorted(set(readers if readers is not None else [a.subject]))
        require(readers and len(readers) <= 1000 and all(isinstance(x, str) and x for x in readers), "invalid readers")
        with self.transaction():
            size = self._files(a, files)
            key = sha(f"{kind}:{name}:{revision}".encode())
            artifact = {"id": key, "kind": kind, "name": name, "revision": revision, "files": files,
                        "size": size, "created_at": self.clock(), "owner": a.subject,
                        "provenance": provenance or {}}
            self._put(a, "artifact", key, artifact, create=True)
            # ACL is independent of immutable revisions; revocation applies to old revisions too.
            acl_key = sha(f"{kind}:{name}".encode())
            existing = self.db.execute("SELECT body FROM resources WHERE tenant=? AND kind='acl' AND id=?",
                                       (a.tenant, acl_key)).fetchone()
            if existing:
                acl = json.loads(existing[0])
                require(a.role == "admin" or a.subject == acl["owner"], "only artifact owner may publish", 403)
            else:
                self._put(a, "acl", acl_key, {"owner": a.subject, "readers": readers, "epoch": 1})
            self._audit(a, "artifact.publish", key)
            return artifact

    def _access(self, a, artifact, write=False):
        acl = self._get(a, "acl", sha(f'{artifact["kind"]}:{artifact["name"]}'.encode()))
        require(a.role == "admin" or a.subject == acl["owner"] or
                (not write and a.subject in acl["readers"]), "resource not found", 404)
        return acl

    def artifact(self, a, key):
        with self.lock:
            artifact = self._get(a, "artifact", key)
            self._access(a, artifact)
            return artifact

    def artifacts(self, a, query=""):
        require(isinstance(query, str) and len(query) <= 200, "invalid query")
        with self.lock:
            result = []
            for artifact in self._list(a, "artifact"):
                try:
                    self._access(a, artifact)
                except DataError:
                    continue
                names = [n for n in artifact["files"] if query.casefold() in n.casefold()]
                if names or query.casefold() in artifact["name"].casefold():
                    result.append({k: v for k, v in artifact.items() if k != "files"} | {"matches": names[:100]})
            return result

    def set_readers(self, a, key, readers):
        a.write()
        require(isinstance(readers, list) and len(readers) <= 1000 and
                all(isinstance(x, str) and 0 < len(x) <= 200 for x in readers), "invalid readers")
        with self.transaction():
            artifact = self._get(a, "artifact", key)
            acl = self._access(a, artifact, write=True)
            acl.update(readers=sorted(set(readers)), epoch=acl["epoch"] + 1)
            self._put(a, "acl", sha(f'{artifact["kind"]}:{artifact["name"]}'.encode()), acl)
            self._audit(a, "artifact.acl", key)
            return acl

    def file(self, a, key, name):
        artifact = self.artifact(a, key)
        require(logical_path(name) in artifact["files"], "file not found", 404)
        entry = artifact["files"][name]
        require(entry["size"] <= 64 * 1024**2, "use chunk retrieval for files over 64 MiB", 413)
        return b"".join(self.chunk(a, h) for h in entry["chunks"])

    def artifact_chunk(self, a, key, h):
        artifact = self.artifact(a, key)
        require(any(h in e["chunks"] for e in artifact["files"].values()), "chunk not in artifact", 404)
        return self.chunk(a, h)

    def materialize(self, a, key, directory):
        artifact = self.artifact(a, key)
        target = Path(directory)
        require(not target.exists(), "destination must not exist", 409)
        target.parent.mkdir(parents=True, exist_ok=True)
        staging = Path(tempfile.mkdtemp(prefix=".atlas-stage-", dir=target.parent))
        try:
            self._files(a, artifact["files"])
            for name, entry in artifact["files"].items():
                path = staging / name
                path.parent.mkdir(parents=True, exist_ok=True)
                with path.open("xb") as stream:
                    for h in entry["chunks"]:
                        stream.write(self.chunk(a, h))
            os.rename(staging, target)
        except BaseException:
            import shutil
            shutil.rmtree(staging)
            raise
        return {"path": str(target), "bytes": artifact["size"], "verified": True}

    def node(self, a, node, capacity, bandwidth, labels):
        a.admin()
        ident(node)
        number(capacity, 1)
        number(bandwidth, 1)
        require(isinstance(labels, dict) and len(labels) <= 32 and
                all(isinstance(k, str) and isinstance(v, str) for k, v in labels.items()), "invalid labels")
        with self.transaction():
            body = {"id": node, "capacity": capacity, "bytes_per_sec": bandwidth, "labels": labels,
                    "heartbeat": self.clock()}
            self._put(a, "node", node, body)
            self._audit(a, "node.heartbeat", node)
            return body

    def mark_staged(self, a, node, artifact_id):
        """Only a trusted node worker calls this AFTER downloading and verifying bytes."""
        a.admin()
        artifact = self.artifact(a, artifact_id)
        with self.transaction():
            self._get(a, "node", node)
            key = sha(f"{node}:{artifact_id}".encode())
            self._put(a, "placement", key, {"id": key, "node": node, "artifact": artifact_id,
                                           "size": artifact["size"], "verified_at": self.clock()})
            self._audit(a, "node.staged", key)
        return {"verified": True}

    def schedule(self, a, job, artifacts, labels=None, reserve=True):
        a.write()
        ident(job)
        require(isinstance(artifacts, list) and 0 < len(artifacts) <= 64 and len(set(artifacts)) == len(artifacts),
                "job needs 1-64 unique artifacts")
        labels = {} if labels is None else labels
        require(isinstance(labels, dict), "labels must be an object")
        require(type(reserve) is bool, "reserve must be boolean")
        with self.transaction():
            assets = [self.artifact(a, k) for k in artifacts]
            previous = [r for r in self._list(a, "reservation") if r["id"] == job]
            if previous and previous[0]["expires_at"] > self.clock():
                require(previous[0]["owner"] == a.subject and previous[0]["artifacts"] == artifacts
                        and previous[0]["labels"] == labels, "job reservation conflicts", 409)
                return self.job(a, job)
            placements = self._list(a, "placement")
            reservations = [r for r in self._list(a, "reservation") if r["expires_at"] > self.clock()]
            candidates = []
            for node in self._list(a, "node"):
                if self.clock() - node["heartbeat"] > 120 or any(node["labels"].get(k) != v for k, v in labels.items()):
                    continue
                occupied = {p["artifact"]: p["size"] for p in placements if p["node"] == node["id"]}
                cached = {p["artifact"]: p["size"] for p in placements if p["node"] == node["id"] and self.clock() - p["verified_at"] <= 300}
                held_ids = {x for r in reservations if r["node"] == node["id"] for x in r["artifacts"]}
                held = sum(self._get(a, "artifact", x)["size"] for x in held_ids - occupied.keys())
                missing = [x for x in assets if x["id"] not in cached]
                need = sum(x["size"] for x in missing)
                # Capacity is conservative logical artifact bytes; shared chunks may use less.
                additional = sum(x["size"] for x in missing if x["id"] not in held_ids and x["id"] not in occupied)
                if sum(occupied.values()) + held + additional > node["capacity"]:
                    continue
                candidates.append({"id": job, "node": node["id"], "artifacts": artifacts, "labels": labels,
                                   "missing": [x["id"] for x in missing], "transfer_bytes": need,
                                   "estimated_staging_seconds": need / node["bytes_per_sec"],
                                   "data_ready": not missing, "owner": a.subject,
                                   "expires_at": self.clock() + 300})
            require(candidates, "no healthy node has matching labels and capacity", 409)
            chosen = min(candidates, key=lambda x: (x["estimated_staging_seconds"], x["node"]))
            if reserve:
                self._put(a, "reservation", job, chosen)
                self._audit(a, "job.reserve", job)
            return chosen

    def job(self, a, job):
        with self.lock:
            result = self._get(a, "reservation", job)
            require(a.role == "admin" or result["owner"] == a.subject, "resource not found", 404)
            require(result["expires_at"] > self.clock(), "reservation expired", 409)
            for key in result["artifacts"]:
                self.artifact(a, key)
            cached = {p["artifact"] for p in self._list(a, "placement") if p["node"] == result["node"] and self.clock() - p["verified_at"] <= 300}
            node = self._get(a, "node", result["node"])
            result["missing"] = [k for k in result["artifacts"] if k not in cached]
            result["data_ready"] = not result["missing"] and self.clock() - node["heartbeat"] <= 120
            return result

    def renew_job(self, a, job, ttl=300):
        a.write()
        number(ttl, 1, 3600)
        with self.transaction():
            body = self.job(a, job)
            body["expires_at"] = self.clock() + ttl
            self._put(a, "reservation", job, body)
            self._audit(a, "job.renew", job)
            return body

    def release_job(self, a, job):
        a.write()
        with self.transaction():
            body = self._get(a, "reservation", job)
            require(a.role == "admin" or body["owner"] == a.subject, "resource not found", 404)
            self.db.execute("DELETE FROM resources WHERE tenant=? AND kind='reservation' AND id=?", (a.tenant, job))
            self._audit(a, "job.release", job)
            return {"released": True}

    def workspace(self, a, base, ttl, budget):
        a.write()
        number(ttl, 1, 86400)
        number(budget, 1, 1024**4)
        with self.transaction():
            self.artifact(a, base)
            key = uuid.uuid4().hex
            body = {"id": key, "base": base, "owner": a.subject, "expires_at": self.clock() + ttl,
                    "budget": budget, "overlay": {}, "generation": 0, "state": "open"}
            self._put(a, "workspace", key, body, create=True)
            self._audit(a, "workspace.create", key)
            return body

    def _workspace(self, a, key):
        body = self._get(a, "workspace", key)
        require(a.role == "admin" or body["owner"] == a.subject, "resource not found", 404)
        require(body["state"] == "open" and body["expires_at"] > self.clock(), "workspace closed or expired", 409)
        self.artifact(a, body["base"])
        return body

    def workspace_change(self, a, key, generation, name, entry):
        a.write()
        logical_path(name)
        number(generation)
        with self.transaction():
            body = self._workspace(a, key)
            require(body["generation"] == generation, "workspace generation conflict", 409)
            if entry is not None:
                self._files(a, {name: entry})
            body["overlay"][name] = entry
            require(sum(e["size"] for e in body["overlay"].values() if e) <= body["budget"],
                    "workspace change budget exceeded", 413)
            merged = self.artifact(a, body["base"])["files"].copy()
            for n, e in body["overlay"].items():
                if e is None:
                    merged.pop(n, None)
                else:
                    merged[n] = e
            self._files(a, merged, verify=False)
            body["generation"] += 1
            self._put(a, "workspace", key, body)
            self._audit(a, "workspace.change", key)
            return body

    def workspace_review(self, a, key):
        with self.lock:
            body = self._workspace(a, key)
            return body | {"changed_bytes": sum(e["size"] for e in body["overlay"].values() if e)}

    def workspace_publish(self, a, key, generation, name, revision, selected):
        a.write()
        number(generation)
        # Publish a new immutable dataset; never modify an existing base in place.
        with self.lock:
            body = self._workspace(a, key)
            require(body["generation"] == generation, "workspace generation conflict", 409)
            require(isinstance(selected, list) and selected and len(set(selected)) == len(selected)
                    and all(n in body["overlay"] for n in selected), "select changed paths to publish")
            files = self.artifact(a, body["base"])["files"].copy()
            for n in selected:
                e = body["overlay"][n]
                if e is None:
                    files.pop(n, None)
                else:
                    files[n] = e
            return self.publish(a, "dataset", name, revision, files,
                                provenance={"workspace": key, "base": body["base"], "generation": generation})

    def workspace_discard(self, a, key):
        a.write()
        with self.transaction():
            body = self._workspace(a, key)
            body.update(state="discarded", overlay={})
            self._put(a, "workspace", key, body)
            self._audit(a, "workspace.discard", key)
            return {"state": "discarded"}

    def _kv_key(self, a, compatibility):
        required = {"model", "tokenizer", "adapter", "layout", "prefix"}
        require(isinstance(compatibility, dict) and set(compatibility) == required
                and all(isinstance(v, str) and 0 < len(v) <= 256 for v in compatibility.values()),
                "KV key needs model/tokenizer/adapter/layout/prefix")
        # Deliberately subject-isolated, even within a tenant.
        return sha(canonical({"subject": a.subject, **compatibility}).encode())

    def kv_put(self, a, compatibility, data, ttl):
        a.write()
        number(ttl, 1, 86400)
        key = self._kv_key(a, compatibility)
        require(len(data) <= self.kv_capacity, "KV block exceeds cache budget", 413)
        chunk = self.put_chunk(a, data)
        with self.transaction():
            self._put(a, "kv", key, {"id": key, "chunk": chunk["hash"], "expires_at": self.clock() + ttl,
                                      "compatibility": compatibility, "owner": a.subject,
                                      "size": len(data), "last_access": self.clock()})
            live = sorted((v for v in self._list(a, "kv") if v["owner"] == a.subject),
                          key=lambda v: (v["last_access"], v["id"]))
            used = sum(v["size"] for v in live if v["expires_at"] > self.clock())
            for v in live:
                if v["id"] != key and (v["expires_at"] <= self.clock() or used > self.kv_capacity):
                    self.db.execute("DELETE FROM resources WHERE tenant=? AND kind='kv' AND id=?", (a.tenant, v["id"]))
                    if v["expires_at"] > self.clock():
                        used -= v["size"]
                    self._audit(a, "kv.evict", v["id"])
            self._audit(a, "kv.put", key)
        return {"id": key, "bytes": len(data), "expires_at": self.clock() + ttl}

    def kv_get(self, a, compatibility):
        with self.transaction():
            body = self._get(a, "kv", self._kv_key(a, compatibility))
            require(body["expires_at"] > self.clock(), "KV block expired", 404)
            body["last_access"] = self.clock()
            self._put(a, "kv", body["id"], body)
            return self.chunk(a, body["chunk"])

    def charge(self, a, event, job, meter, units):
        a.admin()  # Only trusted collectors can create billable measurements.
        ident(event)
        ident(job)
        number(units)
        require(meter in ("stored_byte_hours", "transfer_bytes", "cache_miss_bytes", "checkpoint_bytes"), "unknown meter")
        with self.transaction():
            old = self.db.execute("SELECT job,meter,units,micros FROM ledger WHERE tenant=? AND event_id=?",
                                  (a.tenant, event)).fetchone()
            if old:
                require(tuple(old)[:3] == (job, meter, units), "meter event conflicts", 409)
                return {"deduplicated": True, "micro_usd": old["micros"]}
            rate = self._get(a, "rate", meter)["micros_per_million_units"]
            micros = (units * rate + 999999) // 1000000
            require(micros <= 2**63 - 1, "metered amount exceeds ledger limit")
            self.db.execute("INSERT INTO ledger VALUES (?,?,?,?,?,?)", (a.tenant, event, job, meter, units, micros))
            self._audit(a, "cost.meter", event)
            return {"deduplicated": False, "micro_usd": micros}

    def rate(self, a, meter, micros):
        a.admin()
        require(meter in ("stored_byte_hours", "transfer_bytes", "cache_miss_bytes", "checkpoint_bytes"), "unknown meter")
        number(micros, 0, 10**12)
        with self.transaction():
            self._put(a, "rate", meter, {"micros_per_million_units": micros})
            self._audit(a, "cost.rate", meter)
        return {"micros_per_million_units": micros}

    def costs(self, a, job):
        # A job owner can inspect its own bill; unregistered external jobs are admin-only.
        with self.lock:
            if a.role != "admin":
                body = self._get(a, "reservation", job)
                require(body["owner"] == a.subject, "resource not found", 404)
            meters = {}
            for row in self.db.execute("SELECT meter,units,micros FROM ledger WHERE tenant=? AND job=?", (a.tenant, job)):
                entry = meters.setdefault(row["meter"], {"meter": row["meter"], "units": 0, "micro_usd": 0})
                entry["units"] += row["units"]
                entry["micro_usd"] += row["micros"]
            rows = list(meters.values())
            return {"job": job, "currency": "USD", "meters": rows,
                    "micro_usd": sum(r["micro_usd"] for r in rows), "estimated": True}

    def diff(self, a, left, right):
        l, r = self.artifact(a, left)["files"], self.artifact(a, right)["files"]
        return {"added": sorted(r.keys() - l.keys()), "removed": sorted(l.keys() - r.keys()),
                "changed": sorted(k for k in l.keys() & r.keys() if l[k] != r[k]),
                "unchanged": sorted(k for k in l.keys() & r.keys() if l[k] == r[k])}

    def evict(self, a, node, artifact_id):
        a.admin()
        with self.transaction():
            active = [r for r in self._list(a, "reservation") if r["node"] == node and
                      r["expires_at"] > self.clock() and artifact_id in r["artifacts"]]
            require(not active, "artifact pinned by an active job reservation", 409)
            key = sha(f"{node}:{artifact_id}".encode())
            self.db.execute("DELETE FROM resources WHERE tenant=? AND kind='placement' AND id=?", (a.tenant, key))
            self._audit(a, "node.evict", key)
            return {"placement_removed": True, "worker_must_remove_local_copy": True}

    def invalidate(self, a, node, artifact_id):
        """Trusted worker reports lost/corrupt bytes; active jobs immediately lose readiness."""
        a.admin()
        with self.transaction():
            key = sha(f"{node}:{artifact_id}".encode())
            self.db.execute("DELETE FROM resources WHERE tenant=? AND kind='placement' AND id=?", (a.tenant, key))
            self._audit(a, "node.invalidate", key)
            return {"data_ready": False}

    def estimate(self, a, artifacts):
        a.write()
        require(isinstance(artifacts, list) and 0 < len(artifacts) <= 64, "1-64 artifacts required")
        with self.lock:
            assets = [self.artifact(a, k) for k in set(artifacts)]
            rate_rows = self.db.execute("SELECT body FROM resources WHERE tenant=? AND kind='rate' AND id='transfer_bytes'", (a.tenant,)).fetchone()
            rate = json.loads(rate_rows[0])["micros_per_million_units"] if rate_rows else None
            placements = self._list(a, "placement")
            result = []
            for node in self._list(a, "node"):
                if self.clock() - node["heartbeat"] > 120:
                    continue
                cached = {p["artifact"] for p in placements if p["node"] == node["id"] and self.clock() - p["verified_at"] <= 300}
                needed = sum(x["size"] for x in assets if x["id"] not in cached)
                result.append({"node": node["id"], "transfer_bytes": needed,
                               "staging_seconds": needed / node["bytes_per_sec"],
                               "estimated_transfer_micro_usd": (needed * rate + 999999) // 1000000 if rate is not None else None})
            return {"placements": result, "compute_cost_included": False}

    def gc(self, a):
        """Reclaim unreferenced chunks after a one-hour publication grace period."""
        a.admin()
        with self.lock:
            with self.transaction():
                reachable = set()
                for asset in self._list(a, "artifact"):
                    reachable.update(h for e in asset["files"].values() for h in e["chunks"])
                for ws in self._list(a, "workspace"):
                    if ws["state"] == "open" and ws["expires_at"] > self.clock():
                        reachable.update(h for e in ws["overlay"].values() if e for h in e["chunks"])
                for kv in self._list(a, "kv"):
                    if kv["expires_at"] > self.clock():
                        reachable.add(kv["chunk"])
                removed, paths = 0, []
                rows = self.db.execute("SELECT hash,size FROM chunks WHERE tenant=?", (a.tenant,)).fetchall()
                for row in rows:
                    if row["hash"] not in reachable:
                        path = self._chunk_path(a, row["hash"])
                        if path.exists() and self.clock() - path.stat().st_mtime < 3600:
                            continue
                        self.db.execute("DELETE FROM chunks WHERE tenant=? AND hash=?", (a.tenant, row["hash"]))
                        self.db.execute("DELETE FROM chunk_owners WHERE tenant=? AND hash=?", (a.tenant, row["hash"]))
                        removed += row["size"]
                        paths.append(path)
                self._audit(a, "gc.sweep", str(removed))
            for path in paths:
                path.unlink(missing_ok=True)
            return {"bytes_reclaimed": removed}
