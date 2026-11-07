# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""The agent run id: stored on every audit event a call causes, filterable, and validated."""
import json
import sqlite3
import tempfile
import threading
import unittest
from pathlib import Path

from atlas_data import Actor, DataError, Store
from atlas_data.api import Server, Service
from atlas_data.client import Client
from atlas_data.core import sha


class RunIdTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.store = Store(self.root / "service")
        self.addCleanup(self.store.close)
        self.token = "a" * 48
        credentials = [{"tenant": "acme", "subject": "alice", "role": "admin", "token_sha256": sha(self.token.encode())}]
        self.server = Server(("127.0.0.1", 0), Service(self.store, credentials))
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.client = Client(f"http://127.0.0.1:{self.server.server_port}", self.token)

    def events(self, **kw):
        return self.client.call("audit.list", **kw)

    def test_actor_run_id_is_stored_on_the_audit_event(self):
        a = Actor("acme", "alice", "admin", run_id="run-1")
        self.store.put_chunk(a, b"hello")
        self.store.put_chunk(Actor("acme", "alice", "admin"), b"other")
        rows = {r["action"] + r["resource"]: r["run_id"] for r in self.store.audit(a)}
        self.assertEqual(sorted(rows.values(), key=str), [None, "run-1"])
        self.assertEqual([r["run_id"] for r in self.store.audit(a, run_id="run-1")], ["run-1"])
        self.assertEqual(self.store.audit(a, run_id="nope"), [])

    def test_envelope_field_and_header_both_tag_the_event(self):
        self.client.request("POST", "/v1/chunks", b"via-header", headers={"X-Atlas-Run-Id": "run-h"})
        self.client.call("nodes.heartbeat", node="n1", capacity=10, bandwidth=1, labels={}, run_id="run-e")
        self.assertEqual({r["run_id"] for r in self.events(run_id="run-h")}, {"run-h"})
        self.assertEqual(len(self.events(run_id="run-h")), 1)
        self.client.call("nodes.heartbeat", node="n1", capacity=10, bandwidth=1, labels={})
        # Every untagged event keeps a null run id.
        self.assertTrue(all("run_id" in r for r in self.events()))

    def test_invalid_run_ids_are_refused(self):
        for bad in ("", " x", "a/b", "x" * 65, "-x", 5, None):
            with self.assertRaises(DataError) as c:
                self.client.call("nodes.heartbeat", node="n1", capacity=10, bandwidth=1, labels={}, run_id=bad)
            self.assertEqual(c.exception.status, 400, bad)
        with self.assertRaises(DataError):
            self.client.request("POST", "/v1/chunks", b"x", headers={"X-Atlas-Run-Id": "a b"})

    def test_header_and_field_must_agree(self):
        with self.assertRaises(DataError):
            self.client.request("POST", "/v1/rpc/nodes.heartbeat",
                                json.dumps({"node": "n", "capacity": 1, "bandwidth": 1, "labels": {}, "run_id": "r1"}).encode(),
                                headers={"Content-Type": "application/json", "X-Atlas-Run-Id": "r2"})

    def test_a_catalog_from_before_the_run_id_gains_the_column(self):
        path = self.root / "old" / "data.sqlite"
        path.parent.mkdir()
        db = sqlite3.connect(path)
        db.executescript("CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, tenant TEXT NOT NULL,"
                         " subject TEXT NOT NULL, action TEXT NOT NULL, resource TEXT NOT NULL, at REAL NOT NULL);"
                         "INSERT INTO events(tenant,subject,action,resource,at) VALUES ('acme','a','x','y',1);")
        db.commit()
        db.close()
        store = Store(path.parent)
        self.addCleanup(store.close)
        rows = store.audit(Actor("acme", "a", "admin"))
        self.assertEqual([r["run_id"] for r in rows], [None])


if __name__ == "__main__":
    unittest.main()
