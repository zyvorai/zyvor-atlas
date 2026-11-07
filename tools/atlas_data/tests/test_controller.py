# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Controller, node agent, metering and KV connector over a real local HTTP service and a fake Gryvia."""
from pathlib import Path
import tempfile
import threading
import time
import unittest

from atlas_data import DataError, Store
from atlas_data.api import Server, Service
from atlas_data.client import Client, KVAdapter
from atlas_data.controller import FakeGryvia, GryviaAdapter, JobController, NodeAgent
from atlas_data.core import CHUNK, sha
from atlas_data.kv_connector import KVConnector

TOKEN = "a" * 48


class Env(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.addCleanup(self.temp.cleanup)
        self.store = Store(self.root / "service", clock=time.time)
        self.addCleanup(self.store.close)
        creds = [{"tenant": "acme", "subject": "ops", "role": "admin", "token_sha256": sha(TOKEN.encode())}]
        self.server = Server(("127.0.0.1", 0), Service(self.store, creds))
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.client = Client(f"http://127.0.0.1:{self.server.server_port}", TOKEN)

    def publish(self, payload=b"model-weights", name="llama"):
        source = self.root / ("src-" + name)
        source.mkdir()
        (source / "w.bin").write_bytes(payload)
        files = self.client.upload(source)
        return self.client.call("artifacts.publish", kind="model", name=name, revision="v1", files=files)["id"]

    def agent(self, node="gpu1", **kw):
        agent = NodeAgent(self.client, node, self.root / ("cache-" + node), 10**9, 10**6, **kw)
        agent.heartbeat()
        return agent

    def rates(self):
        for meter in ("transfer_bytes", "cache_miss_bytes"):
            self.client.call("costs.rate", meter=meter, micros=1_000_000)

    def status(self, job):
        return self.client.call("jobs.status", job=job)


class ControllerTests(Env):
    def test_adapter_interface_is_abstract(self):
        for call in (lambda a: a.apply_node_selector("j", {}), lambda a: a.job_state("j"), lambda a: a.report("j", "m")):
            with self.assertRaises(NotImplementedError):
                call(GryviaAdapter())

    def test_full_flow_selector_renewals_release_and_meters(self):
        self.rates()
        artifact = self.publish()
        agent = self.agent()
        gryvia = FakeGryvia({"train": ["running", "running", "running", "succeeded"]})
        controller = JobController(self.client, gryvia, lambda node, job: agent.stage_job(job), lease_ttl=600)
        renewals = []
        original = controller.gate.renew
        controller.gate.renew = lambda job, ttl=300: renewals.append(ttl) or original(job, ttl)
        sleeps = []
        ready, final = controller.run("train", [artifact], sleep=sleeps.append, interval=7)
        self.assertEqual(final, "succeeded")
        self.assertEqual(ready["node_selector"], {"kubernetes.io/hostname": "gpu1"})
        self.assertEqual(gryvia.selectors["train"], ready["node_selector"])
        self.assertEqual(renewals, [600, 600, 600])
        self.assertEqual(sleeps, [7, 7, 7])
        with self.assertRaises(DataError) as c:  # released on completion
            self.status("train")
        self.assertEqual(c.exception.status, 404)
        meters = {m["meter"]: m["units"] for m in self.client.call("costs.report", job="train")["meters"]}
        self.assertEqual(meters, {"transfer_bytes": 13, "cache_miss_bytes": 13})

    def test_restage_is_a_cache_hit_and_meters_nothing_more(self):
        self.rates()
        artifact = self.publish()
        agent = self.agent()
        self.client.call("jobs.reserve", job="a", artifacts=[artifact])
        first = agent.stage_job("a")
        self.assertEqual((first["stage_stats"]["misses"], first["stage_stats"]["network_bytes"]), (1, 13))
        self.client.call("jobs.release", job="a")
        self.client.call("jobs.reserve", job="b", artifacts=[artifact])
        second = agent.stage_job("b")
        self.assertEqual((second["stage_stats"]["hits"], second["stage_stats"]["network_bytes"]), (1, 0))
        self.assertEqual(self.client.call("costs.report", job="b")["meters"], [])

    def test_missing_rate_never_fails_staging(self):
        artifact = self.publish()
        agent = self.agent()
        self.client.call("jobs.reserve", job="a", artifacts=[artifact])
        self.assertTrue(agent.stage_job("a")["data_ready"])
        self.assertEqual(len(agent.meter_errors), 2)

    def test_stage_failure_releases_and_reports(self):
        artifact = self.publish()
        self.agent()
        gryvia = FakeGryvia()

        def broken(node, job):
            raise DataError("node unreachable", 503)

        controller = JobController(self.client, gryvia, broken)
        with self.assertRaises(DataError):
            controller.admit("j", [artifact])
        self.assertIn("node unreachable", gryvia.reports[0][1])
        self.assertNotIn("j", gryvia.selectors)
        with self.assertRaises(DataError):
            self.status("j")

    def test_never_staged_times_out_without_selector(self):
        artifact = self.publish()
        self.agent()
        gryvia = FakeGryvia()
        controller = JobController(self.client, gryvia, lambda node, job: None, ready_timeout=3, ready_interval=1)
        now = [0.0]

        def sleep(seconds):
            now[0] += seconds

        with self.assertRaises(DataError) as c:
            controller.admit("j", [artifact], sleep=sleep, clock=lambda: now[0])
        self.assertEqual(c.exception.status, 504)
        self.assertEqual(gryvia.selectors, {})
        self.assertEqual(len(gryvia.reports), 1)
        with self.assertRaises(DataError):
            self.status("j")

    def test_lost_lease_is_reported_and_surfaces(self):
        artifact = self.publish()
        agent = self.agent()
        gryvia = FakeGryvia({"j": ["running"]})
        controller = JobController(self.client, gryvia, lambda node, job: agent.stage_job(job))
        controller.admit("j", [artifact])
        self.client.call("jobs.release", job="j")  # someone else released it
        with self.assertRaises(DataError):
            controller.supervise("j", sleep=lambda s: None)
        self.assertIn("lease lost", gryvia.reports[-1][1])

    def test_release_even_when_supervision_is_cut_short(self):
        artifact = self.publish()
        agent = self.agent()
        controller = JobController(self.client, FakeGryvia({"j": ["running"]}), lambda n, j: agent.stage_job(j))
        controller.admit("j", [artifact])
        with self.assertRaises(DataError) as c:
            controller.supervise("j", sleep=lambda s: None, max_checks=2)
        self.assertEqual(c.exception.status, 504)
        with self.assertRaises(DataError):
            self.status("j")

    def test_failed_job_state_also_releases(self):
        artifact = self.publish()
        agent = self.agent()
        controller = JobController(self.client, FakeGryvia({"j": ["running", "failed"]}), lambda n, j: agent.stage_job(j))
        _, final = controller.run("j", [artifact], sleep=lambda s: None)
        self.assertEqual(final, "failed")
        with self.assertRaises(DataError):
            self.status("j")


class AgentLoopTests(Env):
    def test_run_heartbeats_and_stages_pending_then_stops(self):
        self.rates()
        artifact = self.publish()
        agent = NodeAgent(self.client, "gpu2", self.root / "cache2", 10**9, 10**6, {"zone": "a"})
        agent.heartbeat()
        self.client.call("jobs.reserve", job="a", artifacts=[artifact], labels={"zone": "a"})
        stop = threading.Event()
        calls = []

        def pending():
            calls.append(1)
            if len(calls) == 2:
                stop.set()
            return ["a"] if len(calls) == 1 else []

        agent.run(stop, interval=0.01, pending=pending)
        self.assertEqual(agent.errors, [])
        self.assertTrue(self.status("a")["data_ready"])

    def test_run_survives_a_failing_tick(self):
        agent = NodeAgent(self.client, "gpu3", self.root / "cache3", 10**9, 10**6)
        stop = threading.Event()
        calls = []

        def pending():
            calls.append(1)
            if len(calls) == 2:
                stop.set()
            return ["no-such-job"] if len(calls) == 1 else []

        agent.run(stop, interval=0.01, pending=pending)
        self.assertEqual(len(agent.errors), 1)
        self.assertEqual(len(calls), 2)


class KVConnectorTests(Env):
    def connector(self, block_size=4, **kw):
        return KVConnector(KVAdapter(self.client, "m", "t", "none", "f16"), block_size, **kw)

    def test_roundtrip_and_longest_prefix(self):
        kv = self.connector()
        tokens = list(range(10))  # two full blocks
        kv.save(tokens, [b"b0", b"b1"])
        self.assertEqual(kv.load(tokens), (8, [b"b0", b"b1"]))
        self.assertEqual(kv.load(tokens[:4] + [99, 98, 97, 96]), (4, [b"b0"]))
        self.assertEqual(kv.load([7, 7, 7, 7]), (0, []))
        self.assertEqual(kv.stats, {"saved_blocks": 2, "hit_blocks": 3, "miss_blocks": 2})

    def test_block_count_must_match_digest_count(self):
        with self.assertRaises(DataError):
            self.connector().save(list(range(8)), [b"only-one"])

    def test_block_over_4mib_is_split_and_restored(self):
        kv = self.connector()
        block = bytes(range(256)) * (CHUNK // 256 + 4096)  # a little over 4 MiB, forces two parts
        self.assertGreater(len(block), CHUNK)
        kv.save([1, 2, 3, 4], [block])
        matched, blocks = kv.load([1, 2, 3, 4])
        self.assertEqual((matched, blocks[0] == block), (4, True))

    def test_block_exactly_at_cap_stays_single_part(self):
        kv = self.connector()
        block = b"x" * (CHUNK - 1)
        kv.save([1, 2, 3, 4], [block])
        self.assertEqual(kv.load([1, 2, 3, 4])[1][0], block)

    def test_oversize_beyond_configured_max_stores_nothing(self):
        kv = self.connector(max_block_bytes=10)
        with self.assertRaises(DataError) as c:
            kv.save(list(range(8)), [b"small", b"x" * 11])
        self.assertEqual(c.exception.status, 413)
        self.assertEqual(kv.load(list(range(8))), (0, []))

    def test_lost_part_is_a_miss_not_an_error(self):
        kv = self.connector()
        block = b"y" * (CHUNK + 10)
        digest = kv.save([1, 2, 3, 4], [block])[0]
        real = kv.adapter.get
        kv.adapter.get = lambda d: None if d == kv._part(digest, 1) else real(d)
        self.assertEqual(kv.load([1, 2, 3, 4]), (0, []))

    def test_other_model_does_not_see_blocks(self):
        self.connector().save([1, 2, 3, 4], [b"b"])
        other = KVConnector(KVAdapter(self.client, "other-model", "t", "none", "f16"), 4)
        self.assertEqual(other.load([1, 2, 3, 4]), (0, []))


if __name__ == "__main__":
    unittest.main()
