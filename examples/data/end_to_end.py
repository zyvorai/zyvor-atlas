# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Exercise all nine workflows against actual HTTP/file/Parquet operations.

Run from repository root: PYTHONPATH=tools python3 examples/data/end_to_end.py
"""
import json
from pathlib import Path
import secrets
import sqlite3
import sys
import tempfile
import threading

from atlas_data import Store
from atlas_data.api import Server, Service
from atlas_data.client import Client, KVAdapter
from atlas_data.core import sha
from atlas_data.integrations import CostCollector, DataReadyGate


def main():
    with tempfile.TemporaryDirectory(prefix="atlas-nine-") as directory:
        root = Path(directory)
        token = secrets.token_urlsafe(48)
        profile = {"sqlite": {"verify": [sys.executable, "-c",
                   "import sqlite3; c=sqlite3.connect('orders.db'); "
                   "assert c.execute('PRAGMA integrity_check').fetchone()[0]=='ok'; "
                   "assert c.execute('SELECT COUNT(*) FROM orders').fetchone()[0]==2"]}}
        store = Store(root / "service")
        service = Service(store, [{"token_sha256": sha(token.encode()), "tenant": "demo",
                                  "subject": "operator", "role": "admin"}], profile)
        server = Server(("127.0.0.1", 0), service)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            client = Client(f"http://127.0.0.1:{server.server_port}", token)
            model_dir = root / "model"
            model_dir.mkdir()
            (model_dir / "weights.bin").write_bytes(b"demonstration model bytes, not an inference model")
            model = client.call("artifacts.publish", kind="model", name="demo-model", revision="v1",
                                files=client.upload(model_dir))
            data_dir = root / "data"
            data_dir.mkdir()
            with sqlite3.connect(data_dir / "orders.db") as db:
                db.execute("CREATE TABLE orders(id INTEGER PRIMARY KEY, total INTEGER)")
                db.executemany("INSERT INTO orders VALUES (?,?)", [(1, 10), (2, 20)])
            dataset = client.call("artifacts.publish", kind="dataset", name="orders", revision="v1",
                                  files=client.upload(data_dir), provenance={"fixture": "closed sqlite DB"})
            listed = client.call("artifacts.list", query="orders")
            assert len(listed) == 1
            client.call("nodes.heartbeat", node="node1", capacity=1024**3,
                        bandwidth=100 * 1024**2, labels={"arch": "arm64"})
            gate = DataReadyGate(client)
            gate.reserve("train", [model["id"], dataset["id"]], {"arch": "arm64"})
            assert not client.call("jobs.status", job="train")["data_ready"]
            client.stage("train", "node1", root / "worker-cache")
            assert gate.ready("train")["node_selector"]["kubernetes.io/hostname"] == "node1"
            gate.renew("train")
            ws = client.call("workspaces.create", base=dataset["id"], ttl=60, budget=1024)
            changes = root / "changes"
            changes.mkdir()
            (changes / "summary.txt").write_text("Orders total: 30")
            entry = client.upload(changes)["summary.txt"]
            client.call("workspaces.change", key=ws["id"], generation=0, name="summary.txt", entry=entry)
            review = client.call("workspaces.review", key=ws["id"])
            branch = client.call("workspaces.publish", key=ws["id"], generation=review["generation"],
                                 name="orders-analysis", revision="v1", selected=["summary.txt"])
            assert client.call("artifacts.diff", left=dataset["id"], right=branch["id"])["added"] == ["summary.txt"]
            client.call("workspaces.discard", key=ws["id"])
            kv = KVAdapter(client, model["id"], "tokenizer-v1", "none", "demo-byte-layout")
            kv.put(sha(b"token-prefix"), b"serialized context block")
            assert kv.get(sha(b"token-prefix")) == b"serialized context block"
            recovery = client.call("recovery.drill", artifact=dataset["id"], profile="sqlite")
            assert recovery["status"] == "passed" and recovery["application_verified"]
            assert client.call("recovery.latest", name="orders")["verified"]
            client.call("costs.rate", meter="transfer_bytes", micros=100)
            download = client.download(branch["id"], root / "export")
            CostCollector(client).transfer("train", "export-1", download["network_bytes"])
            bill = client.call("costs.report", job="train")
            assert bill["micro_usd"] > 0
            client.call("iceberg.create", name="metrics", fields={"job": "string", "tokens": "long"})
            first = client.call("iceberg.append", name="metrics", rows=[{"job": "one", "tokens": 10}], expected_snapshot=None)
            second = client.call("iceberg.append", name="metrics", rows=[{"job": "two", "tokens": 20}],
                                 expected_snapshot=first["snapshot_id"])
            compact = client.call("iceberg.compact", name="metrics", expected_snapshot=second["snapshot_id"])
            assert compact["after"]["data_files"] == 1
            assert sum(r["tokens"] for r in client.call("iceberg.rows", name="metrics")) == 30
            gate.release("train")
            print(json.dumps({"status": "passed", "workflows": 9, "http": "real loopback",
                              "application": "restored SQLite integrity and query",
                              "iceberg_files_before": compact["before"]["data_files"],
                              "iceberg_files_after": compact["after"]["data_files"],
                              "gpu_or_remote_cluster_tested": False}, indent=2))
        finally:
            server.shutdown()
            thread.join(timeout=10)
            server.server_close()
            store.close()


if __name__ == "__main__":
    main()
