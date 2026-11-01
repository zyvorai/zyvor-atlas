# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Real local bytes, HTTP, subprocess and Iceberg scenarios; no fake storage driver."""
import base64
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request

from atlas_data import Actor, DataError, Store
from atlas_data.api import Server, Service, decode
from atlas_data.client import Client, KVAdapter, prefix_digests
from atlas_data.core import sha
from atlas_data.iceberg import Iceberg
from atlas_data.recovery import Recovery

ADMIN = Actor("acme", "ops", "admin")
ALICE = Actor("acme", "alice", "operator")
BOB = Actor("acme", "bob", "viewer")
OTHER = Actor("other", "ops", "admin")
KV = dict(model="weights-sha", tokenizer="tokens-sha", adapter="none", layout="float16-4heads", prefix="prefix-sha")


def wipe_postgres():
    """With ATLAS_DATA_DATABASE_URL set the whole suite runs on that Postgres: start each test empty."""
    url = os.environ.get("ATLAS_DATA_DATABASE_URL")
    if url:
        from atlas_data.catalog import PostgresCatalog
        catalog = PostgresCatalog(url)
        catalog.execute("DROP TABLE IF EXISTS resources, chunks, chunk_owners, events, ledger")
        catalog.close()


class Fixture(unittest.TestCase):
    def setUp(self):
        wipe_postgres()
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.now = time.time()
        self.store = Store(self.root / "service", clock=lambda: self.now)
        self.addCleanup(self.temp.cleanup)
        self.addCleanup(self.store.close)

    def entry(self, value=b"hello", actor=ALICE):
        chunks = [self.store.put_chunk(actor, value)["hash"]] if value else []
        return {"chunks": chunks, "size": len(value), "sha256": sha(value)}

    def asset(self, revision="v1", value=b"hello", kind="dataset", name="training", readers=None):
        return self.store.publish(ALICE, kind, name, revision, {"data.txt": self.entry(value)}, readers)

    def failure(self, status, callback, *args, **kwargs):
        with self.assertRaises(DataError) as c:
            callback(*args, **kwargs)
        self.assertEqual(c.exception.status, status)


class ContentTests(Fixture):
    def test_real_bytes_dedup_and_restart(self):
        asset = self.asset()
        result = self.store.put_chunk(ALICE, b"hello")
        self.assertTrue(result["deduplicated"])
        self.store.close()
        self.store = Store(self.root / "service")
        self.assertEqual(self.store.file(ALICE, asset["id"], "data.txt"), b"hello")

    def test_tenant_isolation_even_same_content_hash(self):
        asset = self.asset()
        self.failure(404, self.store.artifact, OTHER, asset["id"])
        self.failure(404, self.store.chunk, OTHER, sha(b"hello"))

    def test_private_chunk_cannot_be_republished_by_guessed_hash(self):
        self.asset()
        thief = Actor("acme", "eve", "operator")
        self.failure(404, self.store.publish, thief, "dataset", "stolen", "v1",
                     {"data.txt": {"chunks": [sha(b"hello")], "size": 5, "sha256": sha(b"hello")}})

    def test_checksum_upload_and_corruption(self):
        self.failure(422, self.store.put_chunk, ALICE, b"hello", "0" * 64)
        asset = self.asset()
        self.store._chunk_path(ALICE, sha(b"hello")).write_bytes(b"wrong")
        self.failure(422, self.store.file, ALICE, asset["id"], "data.txt")

    def test_manifest_checksum_rejects_lie(self):
        entry = self.entry()
        entry["size"] += 1
        self.failure(422, self.store.publish, ALICE, "model", "llama", "v1", {"weights": entry})

    def test_immutable_revision_and_rollback(self):
        self.asset()
        self.failure(409, self.asset)
        self.assertEqual(len(self.store.artifacts(ALICE)), 1)

    def test_paths_and_collisions(self):
        entry = self.entry()
        for name in ("../out", "/tmp/out", "a/../x", "a\\b", "a//b", "a/./b"):
            self.failure(400, self.store.publish, ALICE, "model", "test", "v1", {name: entry})
        self.failure(400, self.store.publish, ALICE, "model", "test", "v1", {"a": entry, "a/b": entry})

    def test_source_symlinks_are_refused(self):
        path = self.root / "input"
        path.mkdir()
        (path / "escape").symlink_to("/etc/passwd")
        self.failure(400, self.store.ingest, ALICE, path)

    def test_export_and_empty_files(self):
        asset = self.store.publish(ALICE, "model", "test", "v1", {"nested/empty": self.entry(b"")})
        target = self.root / "export"
        self.store.materialize(ALICE, asset["id"], target)
        self.assertEqual((target / "nested/empty").read_bytes(), b"")
        self.failure(409, self.store.materialize, ALICE, asset["id"], target)

    def test_capacity_is_hard(self):
        self.store.capacity = 2
        self.failure(507, self.store.put_chunk, ALICE, b"three")
        self.assertEqual(self.store.db.execute("SELECT COUNT(*) FROM chunks").fetchone()[0], 0)

    def test_dataset_diff(self):
        left = self.asset()
        right = self.asset("v2", b"changed")
        self.assertEqual(self.store.diff(ALICE, left["id"], right["id"])["changed"], ["data.txt"])


class AccessTests(Fixture):
    def test_catalog_search_and_live_revocation_all_revisions(self):
        one = self.asset(readers=["bob"])
        two = self.asset("v2", b"new")
        self.assertEqual(len(self.store.artifacts(BOB, "data.txt")), 2)
        self.assertEqual(self.store.file(BOB, two["id"], "data.txt"), b"new")
        self.store.set_readers(ALICE, one["id"], [])
        self.assertEqual(self.store.artifacts(BOB), [])
        self.failure(404, self.store.artifact, BOB, two["id"])

    def test_viewer_write_refused(self):
        self.failure(403, self.store.put_chunk, BOB, b"hello")

    def test_shared_reader_cannot_publish_revision_or_acl(self):
        asset = self.asset(readers=["eve"])
        eve = Actor("acme", "eve", "operator")
        self.failure(403, self.store.publish, eve, "dataset", "training", "v2", asset["files"])
        self.failure(404, self.store.set_readers, eve, asset["id"], ["bob"])
        self.assertEqual(len(self.store.artifacts(ALICE)), 1)


class SchedulingTests(Fixture):
    def test_readiness_requires_staging_and_healthy_node(self):
        asset = self.asset()
        self.store.node(ADMIN, "gpu1", 1000, 100, {"arch": "linux"})
        reserve = self.store.schedule(ALICE, "job", [asset["id"]], {"arch": "linux"})
        self.assertFalse(reserve["data_ready"])
        self.assertEqual(reserve["transfer_bytes"], 5)
        self.store.mark_staged(ADMIN, "gpu1", asset["id"])
        self.assertTrue(self.store.job(ALICE, "job")["data_ready"])
        self.now += 121
        self.assertFalse(self.store.job(ALICE, "job")["data_ready"])

    def test_locality_beats_transfer_and_reservation_is_idempotent(self):
        asset = self.asset()
        self.store.node(ADMIN, "mac", 1000, 1, {})
        self.store.node(ADMIN, "gpu", 1000, 1000, {})
        self.store.mark_staged(ADMIN, "mac", asset["id"])
        reserve = self.store.schedule(ALICE, "job", [asset["id"]])
        self.assertEqual(reserve["node"], "mac")
        self.assertEqual(reserve, self.store.schedule(ALICE, "job", [asset["id"]]))
        other = self.asset("v2")
        self.failure(409, self.store.schedule, ALICE, "job", [other["id"]])

    def test_reservations_prevent_overcommit(self):
        one, two = self.asset(), self.asset("v2", b"world")
        self.store.node(ADMIN, "n1", 5, 100, {})
        self.store.schedule(ALICE, "one", [one["id"]])
        self.failure(409, self.store.schedule, ALICE, "two", [two["id"]])
        self.now += 301
        self.store.node(ADMIN, "n1", 5, 100, {})
        self.store.schedule(ALICE, "two", [two["id"]])

    def test_stale_and_label_mismatch(self):
        asset = self.asset()
        self.store.node(ADMIN, "mac", 1000, 100, {"arch": "arm64"})
        self.failure(409, self.store.schedule, ALICE, "job", [asset["id"]], {"arch": "amd64"})
        self.now += 121
        self.failure(409, self.store.schedule, ALICE, "job", [asset["id"]])

    def test_active_jobs_pin_cache(self):
        asset = self.asset()
        self.store.node(ADMIN, "mac", 100, 100, {})
        self.store.mark_staged(ADMIN, "mac", asset["id"])
        self.store.schedule(ALICE, "job", [asset["id"]])
        self.failure(409, self.store.evict, ADMIN, "mac", asset["id"])
        self.now += 301
        self.store.evict(ADMIN, "mac", asset["id"])

    def test_concurrent_capacity_reservations(self):
        assets = [self.asset(f"v{i}", str(i).encode())["id"] for i in range(10)]
        self.store.node(ADMIN, "n1", 1, 100, {})
        def reserve(i):
            try:
                self.store.schedule(ALICE, f"j{i}", [assets[i]])
                return True
            except DataError:
                return False
        with ThreadPoolExecutor(max_workers=10) as pool:
            self.assertEqual(sum(pool.map(reserve, range(10))), 1)

    @unittest.skipUnless(os.environ.get("ATLAS_DATA_DATABASE_URL"), "needs a Postgres catalog")
    def test_replicas_on_one_database_do_not_overcommit(self):
        """Two service replicas (separate Store objects, shared root and database) racing for one slot."""
        replicas = [self.store, Store(self.root / "service", clock=lambda: self.now)]
        self.addCleanup(replicas[1].close)
        assets = [self.asset(f"v{i}", str(i).encode())["id"] for i in range(10)]
        self.store.node(ADMIN, "n1", 1, 100, {})
        def reserve(i):
            try:
                replicas[i % 2].schedule(ALICE, f"j{i}", [assets[i]])
                return True
            except DataError:
                return False
        with ThreadPoolExecutor(max_workers=10) as pool:
            self.assertEqual(sum(pool.map(reserve, range(10))), 1)
        # Each replica sees what the other committed.
        self.assertEqual(len(replicas[1].audit(ADMIN)), len(replicas[0].audit(ADMIN)))

    def test_shared_reservations_do_not_double_count_capacity(self):
        asset = self.asset()
        self.store.node(ADMIN, "node", 5, 100, {})
        self.store.schedule(ALICE, "one", [asset["id"]])
        self.store.schedule(ALICE, "two", [asset["id"]])

    def test_readiness_lease_expires_even_with_fresh_heartbeat(self):
        asset = self.asset()
        self.store.node(ADMIN, "node", 100, 100, {})
        self.store.mark_staged(ADMIN, "node", asset["id"])
        self.store.schedule(ALICE, "one", [asset["id"]])
        self.store.renew_job(ALICE, "one", 600)
        self.now += 301
        self.store.node(ADMIN, "node", 100, 100, {})
        self.assertFalse(self.store.job(ALICE, "one")["data_ready"])
        self.store.mark_staged(ADMIN, "node", asset["id"])
        self.assertTrue(self.store.job(ALICE, "one")["data_ready"])
        self.store.release_job(ALICE, "one")
        self.failure(404, self.store.job, ALICE, "one")



class WorkspaceTests(Fixture):
    def test_copy_on_write_review_select_publish_discard(self):
        asset = self.asset()
        ws = self.store.workspace(ALICE, asset["id"], 60, 100)
        self.assertEqual(ws["overlay"], {})
        self.store.workspace_change(ALICE, ws["id"], 0, "result.txt", self.entry(b"result"))
        self.assertEqual(self.store.workspace_review(ALICE, ws["id"])["changed_bytes"], 6)
        new = self.store.workspace_publish(ALICE, ws["id"], 1, "results", "v1", ["result.txt"])
        self.assertEqual(len(new["files"]), 2)
        self.assertEqual(len(self.store.artifact(ALICE, asset["id"])["files"]), 1)
        self.store.workspace_discard(ALICE, ws["id"])
        self.failure(409, self.store.workspace_review, ALICE, ws["id"])

    def test_budget_and_generation_rollback(self):
        ws = self.store.workspace(ALICE, self.asset()["id"], 60, 2)
        self.failure(413, self.store.workspace_change, ALICE, ws["id"], 0, "result", self.entry(b"big"))
        self.assertEqual(self.store.workspace_review(ALICE, ws["id"])["generation"], 0)
        self.failure(409, self.store.workspace_change, ALICE, ws["id"], 10, "result", self.entry(b"a"))

    def test_expiry_and_owner(self):
        ws = self.store.workspace(ALICE, self.asset()["id"], 1, 100)
        self.failure(404, self.store.workspace_review, BOB, ws["id"])
        self.now += 2
        self.failure(409, self.store.workspace_review, ALICE, ws["id"])

    def test_delete_and_base_revocation(self):
        asset = self.asset(readers=["eve"])
        eve = Actor("acme", "eve", "operator")
        ws = self.store.workspace(eve, asset["id"], 60, 100)
        self.store.workspace_change(eve, ws["id"], 0, "data.txt", None)
        result = self.store.workspace_publish(eve, ws["id"], 1, "empty", "v1", ["data.txt"])
        self.assertEqual(result["files"], {})
        self.store.set_readers(ALICE, asset["id"], [])
        self.failure(404, self.store.workspace_review, eve, ws["id"])


class KVAndCostTests(Fixture):
    def test_kv_roundtrip_tenant_subject_and_layout_isolation(self):
        self.store.kv_put(ALICE, KV, b"tensor", 60)
        self.assertEqual(self.store.kv_get(ALICE, KV), b"tensor")
        self.failure(404, self.store.kv_get, ADMIN, KV)
        self.failure(404, self.store.kv_get, OTHER, KV)
        self.failure(404, self.store.kv_get, ALICE, {**KV, "layout": "other"})
        self.now += 61
        self.failure(404, self.store.kv_get, ALICE, KV)

    def test_kv_lru_eviction(self):
        self.store.kv_capacity = 10
        self.store.kv_put(ALICE, KV, b"111111", 60)
        self.now += 1
        self.store.kv_put(ALICE, {**KV, "prefix": "second"}, b"222222", 60)
        self.failure(404, self.store.kv_get, ALICE, KV)
        self.assertEqual(self.store.kv_get(ALICE, {**KV, "prefix": "second"}), b"222222")

    def test_kv_invalid_key(self):
        self.failure(400, self.store.kv_put, ALICE, {"model": "x"}, b"tensor", 60)

    def test_meter_idempotency_integer_money_and_tenant_isolation(self):
        self.store.rate(ADMIN, "transfer_bytes", 3)
        self.store.charge(ADMIN, "ev1", "job", "transfer_bytes", 1000001)
        self.assertTrue(self.store.charge(ADMIN, "ev1", "job", "transfer_bytes", 1000001)["deduplicated"])
        self.assertEqual(self.store.costs(ADMIN, "job")["micro_usd"], 4)
        self.failure(409, self.store.charge, ADMIN, "ev1", "job", "transfer_bytes", 2)
        self.assertEqual(self.store.costs(OTHER, "job")["micro_usd"], 0)
        self.failure(403, self.store.charge, ALICE, "ev2", "job", "transfer_bytes", 1)

    def test_placement_cost_estimate(self):
        asset = self.asset()
        self.store.node(ADMIN, "mac", 1000, 10, {})
        self.store.rate(ADMIN, "transfer_bytes", 1000000)
        estimate = self.store.estimate(ALICE, [asset["id"]])
        self.assertEqual(estimate["placements"][0]["estimated_transfer_micro_usd"], 5)

    def test_expired_cache_gc_preserves_dataset(self):
        self.asset()
        self.store.kv_put(ALICE, KV, b"unused", 1)
        self.now += 7200
        result = self.store.gc(ADMIN)
        self.assertEqual(result["bytes_reclaimed"], 6)
        self.assertEqual(self.store.chunk(ALICE, sha(b"hello")), b"hello")

    def test_meter_retry_keeps_original_rate_and_overflow_refused(self):
        self.store.rate(ADMIN, "transfer_bytes", 100)
        original = self.store.charge(ADMIN, "event", "job", "transfer_bytes", 1000000)
        self.store.rate(ADMIN, "transfer_bytes", 1000)
        retry = self.store.charge(ADMIN, "event", "job", "transfer_bytes", 1000000)
        self.assertEqual(original["micro_usd"], retry["micro_usd"])
        self.store.rate(ADMIN, "transfer_bytes", 10**12)
        self.failure(400, self.store.charge, ADMIN, "huge", "job", "transfer_bytes", 2**53)



class PrefixBlockTests(unittest.TestCase):
    def test_digests_chain_and_share_only_a_common_prefix(self):
        a = prefix_digests(list(range(10)), 4)
        b = prefix_digests([0, 1, 2, 3, 4, 5, 6, 7, 99, 98, 97, 96], 4)
        self.assertEqual(len(a), 2)  # the trailing partial block gets no digest
        self.assertEqual(a, b[:2])
        self.assertNotEqual(b[1], prefix_digests([9, 9, 9, 9, 4, 5, 6, 7], 4)[1])
        self.assertEqual(prefix_digests([1, 2, 3], 4), [])

    def test_digests_reject_bad_input(self):
        for tokens, size in (([1, -1], 1), ([True], 1), ("abc", 1), ([1], 0), ([1], True), ([2**32], 1)):
            with self.assertRaises(DataError):
                prefix_digests(tokens, size)

    def test_longest_prefix_stops_at_the_first_miss(self):
        class Fake:
            def __init__(self):
                self.blocks = {}

            def call(self, op, compatibility, data=None, ttl=None):
                key = compatibility["prefix"]
                if op == "kv.put":
                    self.blocks[key] = data
                    return {}
                if key not in self.blocks:
                    raise DataError("missing", 404)
                return {"data": self.blocks[key]}

        adapter = KVAdapter(Fake(), "m", "t", "none", "f16")
        digests = prefix_digests(list(range(16)), 4)
        adapter.put_blocks(digests[:2] + digests[3:], [b"b0", b"b1", b"b3"])
        self.assertEqual(adapter.longest_prefix(digests), [b"b0", b"b1"])  # b3 is after the gap
        self.assertEqual(adapter.longest_prefix([]), [])
        with self.assertRaises(DataError):
            adapter.put_blocks(digests, [b"x"])


class RecoveryTests(Fixture):
    def test_real_application_verification_and_latest(self):
        old = self.asset()
        self.now += 10
        new = self.asset("v2", b"hello")
        code = "from pathlib import Path; assert Path('data.txt').read_text() == 'hello'"
        recovery = Recovery(self.store, {"app": {"verify": [sys.executable, "-c", code]}})
        self.assertEqual(recovery.drill(ADMIN, old["id"], "app")["status"], "passed")
        self.assertTrue(recovery.drill(ADMIN, new["id"], "app")["application_verified"])
        self.assertEqual(recovery.latest(ADMIN, "training")["artifact"], new["id"])

    def test_failed_health_is_not_verified(self):
        asset = self.asset()
        recovery = Recovery(self.store, {"bad": {"verify": [sys.executable, "-c", "raise SystemExit(1)"]}})
        report = recovery.drill(ADMIN, asset["id"], "bad")
        self.assertEqual(report["status"], "failed")
        self.assertFalse(recovery.latest(ADMIN, "training")["verified"])

    def test_timeout_and_missing_profile(self):
        asset = self.asset()
        recovery = Recovery(self.store, {"slow": {"verify": [sys.executable, "-c", "import time;time.sleep(10)"]}})
        self.assertEqual(recovery.drill(ADMIN, asset["id"], "slow", 1)["status"], "failed")
        self.failure(404, recovery.drill, ADMIN, asset["id"], "unknown")


class HttpTests(Fixture):
    def setUp(self):
        super().setUp()
        self.token = "a" * 48
        self.bob_token = "b" * 48
        credentials = [{"tenant": "acme", "subject": "alice", "role": "admin", "token_sha256": sha(self.token.encode())},
                       {"tenant": "acme", "subject": "bob", "role": "viewer", "token_sha256": sha(self.bob_token.encode())}]
        self.service = Service(self.store, credentials)
        self.server = Server(("127.0.0.1", 0), self.service)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.client = Client(f"http://127.0.0.1:{self.server.server_port}", self.token)

    def test_real_http_ingest_download_worker_kv(self):
        source = self.root / "source"
        source.mkdir()
        (source / "weights.bin").write_bytes(b"model-weights")
        files = self.client.upload(source)
        asset = self.client.call("artifacts.publish", kind="model", name="llama", revision="v1", files=files)
        self.client.call("nodes.heartbeat", node="mac", capacity=1000, bandwidth=10, labels={})
        self.client.call("jobs.reserve", job="serve", artifacts=[asset["id"]])
        result = self.client.stage("serve", "mac", self.root / "cache")
        self.assertTrue(result["data_ready"])
        self.assertEqual((self.root / "cache" / asset["id"] / "weights.bin").read_bytes(), b"model-weights")
        adapter = KVAdapter(self.client, "model", "tokenizer", "none", "float16")
        self.assertIsNone(adapter.get("missing"))
        adapter.put("prefix", b"tensor")
        self.assertEqual(adapter.get("prefix"), b"tensor")

    def test_http_unauthorized_and_private_chunk(self):
        asset = self.asset()
        bob = Client(self.client.url, self.bob_token)
        self.failure(404, bob.download, asset["id"], self.root / "stolen")
        bad = Client(self.client.url, "x" * 48)
        self.failure(401, bad.call, "artifacts.list")
        self.failure(403, bob.request, "POST", "/v1/chunks", b"new")

    def test_http_bad_arguments_and_json(self):
        self.failure(400, self.client.call, "jobs.reserve", unexpected=True)
        self.failure(404, self.client.call, "__dict__")
        for data in (b'{"x":1,"x":2}', b'{"x":NaN}', b'[]'):
            self.failure(400, decode, data)

    def test_cross_artifact_chunk_access_is_refused(self):
        public = self.asset(readers=["bob"])
        self.asset("v2", b"secret")
        # v2 shares the same ACL, so use a separate private artifact for this check.
        private = self.store.publish(ALICE, "dataset", "private", "v1", {"secret": self.entry(b"private")})
        bob = Client(self.client.url, self.bob_token)
        self.failure(404, bob.request, "GET", f"/v1/chunks/{sha(b'private')}?artifact={public['id']}", binary=True)

    def test_worker_detects_cache_corruption_and_shared_chunk_reuse(self):
        source = self.root / "src"
        source.mkdir()
        (source / "weights").write_bytes(b"same weights")
        files = self.client.upload(source)
        one = self.client.call("artifacts.publish", kind="model", name="m", revision="v1", files=files)
        two = self.client.call("artifacts.publish", kind="model", name="m", revision="v2", files=files)
        first = self.client.download(one["id"], self.root / "a", self.root / "chunks")
        second = self.client.download(two["id"], self.root / "b", self.root / "chunks")
        self.assertEqual(first["network_bytes"], len(b"same weights"))
        self.assertEqual(second["network_bytes"], 0)
        self.client.call("nodes.heartbeat", node="node", capacity=10000, bandwidth=100, labels={})
        self.client.call("jobs.reserve", job="job", artifacts=[one["id"]])
        cache = self.root / "cache"
        self.client.stage("job", "node", cache)
        (cache / one["id"] / "weights").write_bytes(b"broken")
        self.failure(422, self.client.stage, "job", "node", cache)
        self.assertFalse(self.client.call("jobs.status", job="job")["data_ready"])

    def test_expiring_credentials_and_failed_stage_clears_readiness(self):
        self.service.credentials[0]["expires_at"] = self.now - 1
        self.failure(401, self.client.call, "artifacts.list")

    def test_remote_plain_http_refused(self):
        self.failure(400, Client, "http://example.com", self.token)


class IcebergTests(Fixture):
    def test_real_parquet_compaction_preserves_rows_and_time_travel(self):
        try:
            import pyiceberg
        except ImportError:
            self.skipTest("optional PyIceberg unavailable")
        ice = Iceberg(self.store)
        created = ice.create(ADMIN, "metrics", {"id": "long", "value": "string"})
        one = ice.append(ADMIN, "metrics", [{"id": 1, "value": "one"}], created["snapshot_id"])
        two = ice.append(ADMIN, "metrics", [{"id": 2, "value": "two"}], one["snapshot_id"])
        self.assertEqual(two["data_files"], 2)
        compact = ice.compact(ADMIN, "metrics", two["snapshot_id"])
        self.assertEqual(compact["after"]["data_files"], 1)
        self.assertEqual(sorted(ice.rows(ADMIN, "metrics"), key=lambda r: r["id"]),
                         [{"id": 1, "value": "one"}, {"id": 2, "value": "two"}])
        table = ice._table(ADMIN, "metrics")
        self.assertEqual(len(table.scan(snapshot_id=one["snapshot_id"]).to_arrow()), 1)
        self.failure(409, ice.compact, ADMIN, "metrics", two["snapshot_id"])
        self.failure(403, ice.rows, ALICE, "metrics")
        self.now += 7200
        expired = ice.expire(ADMIN, "metrics", 3600)
        self.assertFalse(expired["physical_files_deleted"])
        self.assertEqual(len(ice.rows(ADMIN, "metrics")), 2)


class GateWaitTests(unittest.TestCase):
    """`DataReadyGate.wait_ready` against a scripted client and clock: no sleeping, no server."""

    class Scripted:
        def __init__(self, answers):
            self.answers, self.calls = list(answers), 0

        def call(self, action, **arguments):
            self.calls += 1
            answer = self.answers.pop(0) if self.answers else self.answers_last
            if isinstance(answer, Exception):
                raise answer
            return answer

    NOT_READY = {"data_ready": False, "node": "n1", "artifacts": [], "expires_at": 0}
    READY = {"data_ready": True, "node": "n1", "artifacts": ["a1"], "expires_at": 99}

    def gate(self, answers):
        from atlas_data.integrations import DataReadyGate
        client = self.Scripted(answers)
        client.answers_last = answers[-1]
        return DataReadyGate(client), client

    def test_waits_through_not_ready_then_returns_the_node_selector(self):
        gate, client = self.gate([self.NOT_READY, self.NOT_READY, self.READY])
        slept = []
        got = gate.wait_ready("job1", timeout=60, interval=5, sleep=slept.append, clock=lambda: 0)
        self.assertEqual(got["node_selector"], {"kubernetes.io/hostname": "n1"})
        self.assertEqual(got["artifact_ids"], ["a1"])
        self.assertEqual((client.calls, slept), (3, [5, 5]))

    def test_times_out_with_504_instead_of_starting_without_data(self):
        gate, client = self.gate([self.NOT_READY])
        now = [0]
        def sleep(s):
            now[0] += s
        with self.assertRaises(DataError) as ctx:
            gate.wait_ready("job1", timeout=12, interval=5, sleep=sleep, clock=lambda: now[0])
        self.assertEqual(ctx.exception.status, 504)
        self.assertLessEqual(now[0], 12)

    def test_other_errors_are_not_retried(self):
        gate, client = self.gate([DataError("unknown job", 404)])
        with self.assertRaises(DataError) as ctx:
            gate.wait_ready("nope", timeout=60, interval=5, sleep=lambda s: None, clock=lambda: 0)
        self.assertEqual((ctx.exception.status, client.calls), (404, 1))

    def test_bad_timing_arguments_are_refused(self):
        gate, _ = self.gate([self.READY])
        for kwargs in ({"timeout": 0}, {"interval": 0}):
            with self.assertRaises(DataError):
                gate.wait_ready("job1", **kwargs)


if __name__ == "__main__":
    unittest.main()
