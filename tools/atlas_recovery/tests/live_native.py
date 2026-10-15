# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Actual gateway + single Native metadata/data process, disposable loopback storage.

No Kubernetes/Ceph, privileged disks or production endpoints are used. Exits nonzero
on failed assertions. Build with cargo build -p atlas-gateway -p atlas-native --bins.
"""
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.request

from atlas_recovery.core import AtlasClient, Recovery, atomic_json


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def ready(url, processes):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if any(p.poll() is not None for p in processes):
            raise RuntimeError("Atlas process exited during startup")
        try:
            with urllib.request.urlopen(url, timeout=.5) as response:
                if response.status == 200:
                    return
        except OSError:
            pass
        time.sleep(.05)
    raise RuntimeError("Atlas startup timed out")


def main():
    repo = Path(__file__).resolve().parents[3]
    native, gateway = repo / "target/debug/atlas-native-node", repo / "target/debug/atlas-gateway"
    assert native.exists() and gateway.exists(), "build Native and gateway binaries first"
    with tempfile.TemporaryDirectory(prefix="atlas-recovery-live-") as directory:
        root = Path(directory)
        processes, logs = [], []
        data_port, data_http, meta_port, meta_http, rest_port, grpc_port = [free_port() for _ in range(6)]
        configs = [
            {"node_id": "d1", "data_dir": str(root / "d1"), "http_listen": f"127.0.0.1:{data_http}",
             "data_node": {"listen": f"127.0.0.1:{data_port}"}},
            {"node_id": "m1", "data_dir": str(root / "m1"), "http_listen": f"127.0.0.1:{meta_http}",
             "metadata": {"listen": f"127.0.0.1:{meta_port}", "peers": {},
                          "data_nodes": [{"id": "d1", "addr": f"127.0.0.1:{data_port}"}],
                          "replicas": 1, "extent_bytes": 4096}},
        ]
        try:
            for config in configs:
                file = root / (config["node_id"] + ".json")
                atomic_json(file, config)
                log = open(root / (config["node_id"] + ".log"), "w+")
                logs.append(log)
                processes.append(subprocess.Popen([str(native), "--config", str(file)], stdout=log, stderr=log))
            ready(f"http://127.0.0.1:{meta_http}/readyz", processes)
            env = {k: v for k, v in os.environ.items() if not k.startswith("ATLAS_")}
            env.update(ATLAS_BIND_ADDR=f"127.0.0.1:{rest_port}", ATLAS_GRPC_ADDR=f"127.0.0.1:{grpc_port}",
                       ATLAS_DATABASE_URL=f"sqlite://{root}/inventory.db?mode=rwc", ATLAS_AUTH_REQUIRED="false",
                       ATLAS_NATIVE_ENABLE="1", ATLAS_NATIVE_DRIVER_MODE="real",
                       ATLAS_NATIVE_ENDPOINTS=f"http://127.0.0.1:{meta_http}", ATLAS_CEPH_DRIVER_MODE="fake",
                       ATLAS_MONITOR_INTERVAL_SECS="0", ATLAS_SNAPSHOT_TICK_SECS="0",
                       ATLAS_DATABRIDGE_RECONCILE_SECS="0", ATLAS_JOB_POLL_SECS="0", RUST_LOG="warn")
            log = open(root / "gateway.log", "w+")
            logs.append(log)
            processes.append(subprocess.Popen([str(gateway)], cwd=root, env=env, stdout=log, stderr=log))
            ready(f"http://127.0.0.1:{rest_port}/healthz", processes)
            base = f"http://127.0.0.1:{rest_port}/api/atlas/v1"
            client = AtlasClient(base)
            payload = b"atlas-recovery-proof"
            created = client.request("POST", "/volumes", {"name": "recovery-source", "tenant_id": "acme",
                                     "size_bytes": 8192, "kubernetes": {"backend_id": "bkd_native"}})
            vid = created["volume_id"]
            request = urllib.request.Request(base + "/volumes/" + vid + "/data?offset=0",
                                             data=payload, method="PUT")
            with urllib.request.urlopen(request) as response:
                assert response.status == 204
            plan = {"version": 1, "name": "live-native", "tenant_id": "acme", "target_namespace": "atlas-drill-native",
                    "rpo_seconds": 60, "rto_seconds": 30, "components": [{"id": "db", "volumes": [
                        {"volume_id": vid, "checks": [{"offset": 0, "length": len(payload),
                         "sha256": hashlib.sha256(payload).hexdigest()}]}]}]}
            capture = Recovery(client, {}, root / "capture.json").capture(plan)
            assert capture["status"] == "passed", capture
            restored_plan = capture["captured_plan"]
            passed = Recovery(client, {}, root / "passed.json").drill(restored_plan)
            assert passed["status"] == "passed" and passed["cleanup_complete"], passed
            # Check source is intact after deletion of the restored clone.
            assert client.request("GET", f"/volumes/{vid}/data?offset=0&len={len(payload)}", binary=True) == payload
            # The same real snapshot with an incorrect expected digest must fail and clean up.
            restored_plan["components"][0]["volumes"][0]["checks"][0]["sha256"] = "0" * 64
            failed = Recovery(client, {}, root / "failed.json").drill(restored_plan)
            assert failed["status"] == "failed" and failed["cleanup_complete"], failed
            # Select newest ready snapshot on the real catalog without mutating the input plan.
            restored_plan["components"][0]["volumes"][0]["checks"][0]["sha256"] = hashlib.sha256(payload).hexdigest()
            restored_plan["components"][0]["volumes"][0]["snapshot_id"] = "latest"
            latest = Recovery(client, {}, root / "latest.json").drill(restored_plan)
            assert latest["status"] == "passed", latest
            assert restored_plan["components"][0]["volumes"][0]["snapshot_id"] == "latest"
            client.wait(client.request("DELETE", "/snapshots/" + capture["resources"][0]["id"]))
            client.wait(client.request("DELETE", "/volumes/" + vid + "?confirm=true"))
            print(json.dumps({"status": "passed", "scenarios": ["capture-real-native", "restore-and-checksum",
                              "source-unchanged", "checksum-failure-cleanup", "latest-snapshot-selection"],
                              "backend": "real Native engine on temporary files; one metadata and one data node",
                              "production_validation": False}, indent=2))
        except BaseException:
            for log in logs:
                log.flush(); log.seek(0)
                print(log.read()[-3000:])
            raise
        finally:
            for process in reversed(processes):
                process.terminate()
            for process in processes:
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill(); process.wait()
            for log in logs:
                log.close()


if __name__ == "__main__":
    main()
