# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Contract for the gateway's MCP data tools (crates/atlas-gateway/src/mcp/data.rs).

The Rust side sends exactly these actions with exactly these argument names and reads exactly these
reply fields. If this test fails, update data.rs and docs/MCP.md together with it. It also pins the two
facts the identity mapping depends on: the credential alone fixes tenant and subject (the envelope
cannot carry a subject), and another tenant's credential sees "not found".
"""
import inspect
import tempfile
import time
import unittest
from pathlib import Path

from atlas_data import Actor, DataError, Store
from atlas_data.api import Service
from atlas_data.core import sha

ALICE = Actor("acme", "alice", "operator")
EVE_OTHER_TENANT = Actor("rival", "eve", "operator")

# action -> argument names the Rust tools send (data.rs `rpc` callers).
SENT = {
    "artifacts.list": ["query"], "artifacts.get": ["key"], "jobs.status": ["job"],
    "workspaces.review": ["key"], "costs.report": ["job"], "costs.estimate": ["artifacts"],
    "workspaces.change": ["key", "generation", "name", "entry"],
    "workspaces.publish": ["key", "generation", "name", "revision", "selected"],
    "workspaces.discard": ["key"], "jobs.release": ["job"],
}


class McpContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.now = time.time()
        self.store = Store(Path(self.temp.name) / "s", clock=lambda: self.now)
        self.addCleanup(self.store.close)
        creds = [{"tenant": "acme", "subject": "alice", "role": "operator", "token_sha256": sha(b"a" * 48)}]
        self.service = Service(self.store, creds)

    def test_actions_exist_with_the_argument_names_the_tools_send(self):
        for action, names in SENT.items():
            self.assertIn(action, self.service.actions, action)
            params = list(inspect.signature(self.service.actions[action]).parameters)[1:]  # drop actor
            for n in names:
                self.assertIn(n, params, f"{action} has no argument {n}")

    def test_no_subject_can_be_supplied_in_the_envelope(self):
        with self.assertRaises(DataError):
            self.service.call(ALICE, "artifacts.list", {"a": "x"})
        with self.assertRaises(DataError):
            self.service.call(ALICE, "artifacts.list", {"subject": "root"})

    def test_reply_shapes_and_cross_tenant_not_found(self):
        chunk = self.store.put_chunk(ALICE, b"hello")["hash"]
        entry = {"chunks": [chunk], "size": 5, "sha256": sha(b"hello")}
        art = self.store.publish(ALICE, "dataset", "ds", "v1", {"a.txt": entry})
        listed = self.service.call(ALICE, "artifacts.list", {"query": "ds"})
        for field in ("id", "kind", "name", "revision", "created_at", "matches"):
            self.assertIn(field, listed[0])
        self.assertNotIn("files", listed[0])
        self.store.node(Actor("acme", "ops", "admin"), "n1", 1000, 100, {})
        self.service.call(ALICE, "jobs.reserve", {"job": "job1", "artifacts": [art["id"]]})
        job = self.service.call(ALICE, "jobs.status", {"job": "job1"})
        for field in ("id", "node", "artifacts", "data_ready", "missing", "expires_at"):
            self.assertIn(field, job)
        ws = self.service.call(ALICE, "workspaces.create", {"base": art["id"], "ttl": 600, "budget": 100})
        review = self.service.call(ALICE, "workspaces.review", {"key": ws["id"]})
        for field in ("id", "base", "generation", "state", "expires_at", "budget", "overlay", "changed_bytes"):
            self.assertIn(field, review)
        cost = self.service.call(ALICE, "costs.report", {"job": "job1"})
        for field in ("job", "meters", "micro_usd"):
            self.assertIn(field, cost)
        # Another tenant's credential sees the same "not found" for every lookup the tools make.
        for action, args in (("artifacts.get", {"key": art["id"]}), ("jobs.status", {"job": "job1"}),
                             ("workspaces.review", {"key": ws["id"]}), ("costs.report", {"job": "job1"})):
            with self.assertRaises(DataError) as c:
                self.service.call(EVE_OTHER_TENANT, action, args)
            self.assertEqual(c.exception.status, 404, action)
        self.assertEqual(self.service.call(EVE_OTHER_TENANT, "artifacts.list", {"query": ""}), [])
        # Errors the tools relay are DataError messages, never tracebacks.
        with self.assertRaises(DataError) as c:
            self.service.call(ALICE, "workspaces.change", {"key": ws["id"], "generation": 99, "name": "a.txt", "entry": None})
        self.assertEqual(c.exception.status, 409)


if __name__ == "__main__":
    unittest.main()
