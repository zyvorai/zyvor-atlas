# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""HTTP contract scenarios: these fixtures are not real Ceph/native infrastructure."""
import copy
import datetime as dt
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from urllib.parse import parse_qs, urlsplit
from unittest.mock import patch

from atlas_recovery.core import (ApiError, AtlasClient, HookRunner, Recovery, RecoveryError,
                                 atomic_json, timestamp, utcnow, validate)

PAYLOAD = b"atlas-recovery-proof"
DIGEST = hashlib.sha256(PAYLOAD).hexdigest()


def plan():
    return {"version": 1, "name": "orders", "tenant_id": "acme", "target_namespace": "atlas-drill-orders",
            "rpo_seconds": 3600, "rto_seconds": 60, "components": [
                {"id": "api", "depends_on": ["db"], "volumes": [{"volume_id": "vol_native_api",
                    "snapshot_id": "snap_api", "checks": [{"offset": 0, "length": len(PAYLOAD), "sha256": DIGEST}]}]},
                {"id": "db", "volumes": [{"volume_id": "vol_native_db", "snapshot_id": "snap_db",
                    "checks": [{"offset": 0, "length": len(PAYLOAD), "sha256": DIGEST}]}]}]}


class Fixture:
    def __init__(self):
        self.calls, self.clones, self.jobs = [], {}, {}
        self.counter = 0
        self.asynchronous = False
        self.corrupt = False
        self.short = False
        self.fail_clone_number = None
        self.fail_delete = False
        self.active_job = False
        self.stale = False
        self.foreign = False
        self.unready = False
        self.future = False
        self.return_source = False
        self.ambiguous = False
        self.backend_type = "native"
        self.clone_requests = 0
        self.journal = None
        self.intent_seen = False
        self.sources = {"vol_native_db", "vol_native_api"}

    def volume(self, vid, name=None):
        return {"id": vid, "name": name or vid, "kind": "block", "size_bytes": 1024,
                "cluster_id": "cluster", "backend_native_id": vid[11:], "kubernetes_namespace": None}

    def snapshots(self):
        age = 7200 if self.stale else (-60 if self.future else 10)
        return [{"id": "snap_" + x, "volume_id": "vol_native_" + x,
                 "tenant_id": "other" if self.foreign else "acme",
                 "state": "creating" if self.unready else "ready",
                 "created_at": (utcnow() - dt.timedelta(seconds=age)).isoformat()} for x in ("db", "api")]

    def handle(self, method, target, body):
        parsed = urlsplit(target)
        path = parsed.path.removeprefix("/api/atlas/v1")
        self.calls.append((method, path, body))
        if path == "/backends":
            return 200, [{"id": "backend", "backend_type": self.backend_type,
                          "capabilities": {"snapshots": True, "clone": True}}]
        if path == "/clusters":
            return 200, [{"id": "cluster", "backend_id": "backend"}]
        if path == "/volumes" and method == "GET":
            return 200, [self.volume(v) for v in self.sources] if parse_qs(parsed.query).get("tenant") == ["acme"] else []
        if path == "/snapshots":
            return 200, self.snapshots()
        if path.startswith("/jobs/"):
            return 200, {"state": "running" if self.active_job else "succeeded"}
        if path.endswith("/clone"):
            self.clone_requests += 1
            if self.journal:
                self.intent_seen = bool(json.loads(self.journal.read_text())["uncertain_operations"])
            if self.fail_clone_number == self.clone_requests:
                return 503, {"error": "fixture clone failure"}
            if self.ambiguous:
                return 201, b"not-json"
            sid = path.split("/")[2]
            vid = "vol_native_" + sid.removeprefix("snap_") if self.return_source else "clone_" + str(self.clone_requests)
            self.clones[vid] = self.volume(vid, body["name"])
            response = {"volume_id": vid}
            if self.asynchronous:
                return 202, {"job_id": "job_" + str(self.clone_requests), "resource": response}
            return 201, response
        if path.endswith("/snapshots") and method == "POST":
            self.counter += 1
            return 201, {"snapshot_id": "captured_" + str(self.counter)}
        if path.endswith("/data"):
            return 200, b"x" * len(PAYLOAD) if self.corrupt else (PAYLOAD[:-1] if self.short else PAYLOAD)
        if path.startswith("/volumes/"):
            vid = path.split("/")[2]
            if method == "GET":
                return (200, self.clones[vid]) if vid in self.clones else (404, {})
            if method == "DELETE":
                if vid in self.sources:
                    raise AssertionError("source deletion attempted")
                if self.fail_delete:
                    return 503, {}
                self.clones.pop(vid, None)
                return (202, {"job_id": "delete_job"}) if self.asynchronous else (200, {})
        return 404, {}


class TestRecovery(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.fixture = Fixture()
        fixture = self.fixture

        class Handler(BaseHTTPRequestHandler):
            def handle_request(self):
                raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                status, data = fixture.handle(self.command, self.path, json.loads(raw) if raw else None)
                binary = isinstance(data, bytes)
                raw = data if binary else json.dumps(data).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/octet-stream" if binary else "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            do_GET = do_POST = do_DELETE = handle_request

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": .01}, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}/api/atlas/v1"
        self.client = AtlasClient(self.url, job_timeout=.03, poll_seconds=.005)
        self.report = self.root / "report.json"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.temp.cleanup()

    def recovery(self, hooks=None):
        return Recovery(self.client, hooks or {}, self.report)

    def registry(self, p, phases, fail=None):
        log = str(self.root / "hooks.log")
        hooks = {}
        for component in p["components"]:
            for phase in phases:
                name = component["id"] + "-" + phase
                component[phase + "_hook"] = name
                code = ("import json,sys; c=json.load(sys.stdin); "
                        f"open({log!r},'a').write(c['component_id']+':'+c['phase']+'\\n'); "
                        f"sys.exit({1 if fail == name else 0})")
                hooks[name] = {"argv": [sys.executable, "-c", code], "phases": [phase], "timeout_seconds": 2}
        return hooks

    def test_dependency_order_clone_checksum_cleanup_and_journal(self):
        self.fixture.journal = self.report
        result = self.recovery().drill(plan())
        self.assertEqual(result["status"], "passed")
        self.assertTrue(result["cleanup_complete"])
        self.assertTrue(result["rpo_met"] and result["rto_met"])
        self.assertEqual([r["component_id"] for r in result["resources"]], ["db", "api"])
        self.assertEqual(self.fixture.clones, {})
        self.assertTrue(self.fixture.intent_seen)
        self.assertEqual(json.loads(self.report.read_text()), result)
        self.assertEqual(os.stat(self.report).st_mode & 0o777, 0o600)
        self.assertEqual(len([e for e in result["events"] if e["phase"] == "checksum"]), 2)

    def test_asynchronous_contract_and_cleanup_jobs(self):
        self.fixture.asynchronous = True
        result = self.recovery().drill(plan())
        self.assertEqual(result["status"], "passed")
        self.assertTrue(any(p.startswith("/jobs/") for _, p, _ in self.fixture.calls))
        self.assertFalse(self.fixture.clones)

    def test_corruption_fails_and_cleans(self):
        self.fixture.corrupt = True
        result = self.recovery().drill(plan())
        self.assertEqual(result["status"], "failed")
        self.assertIn("checksum mismatch", result["error"])
        self.assertTrue(result["cleanup_complete"])

    def test_short_read_is_not_a_success(self):
        self.fixture.short = True
        result = self.recovery().drill(plan())
        self.assertIn("short read", result["error"])

    def test_partial_clone_failure_cleans_prior_clone(self):
        self.fixture.fail_clone_number = 2
        result = self.recovery().drill(plan())
        self.assertEqual(result["status"], "failed")
        self.assertFalse(self.fixture.clones)
        self.assertFalse(result["cleanup_complete"])
        self.assertEqual(len(result["uncertain_operations"]), 1)

    def test_ambiguous_response_is_journaled_and_never_retried(self):
        self.fixture.ambiguous = True
        result = self.recovery().drill(plan())
        self.assertEqual(self.fixture.clone_requests, 1)
        self.assertEqual(result["status"], "failed")
        self.assertTrue(result["uncertain_operations"])

    def test_job_timeout_retains_pending_resource(self):
        self.fixture.asynchronous = self.fixture.active_job = True
        result = self.recovery().drill(plan())
        self.assertEqual(result["status"], "failed")
        self.assertIn("timed out", result["error"])
        self.assertFalse(result["cleanup_complete"])
        self.assertEqual(len(self.fixture.clones), 1)
        self.assertFalse(any(m == "DELETE" for m, _, _ in self.fixture.calls))

    def test_delete_failure_changes_success_to_failure(self):
        self.fixture.fail_delete = True
        result = self.recovery().drill(plan())
        self.assertEqual(result["status"], "failed")
        self.assertEqual(len(result["cleanup_errors"]), 2)

    def test_server_returning_source_id_never_deletes_source(self):
        self.fixture.return_source = True
        result = self.recovery().drill(plan())
        self.assertEqual(result["status"], "failed")
        self.assertFalse(any(m == "DELETE" for m, _, _ in self.fixture.calls))

    def test_stale_snapshot_fails_without_mutations(self):
        self.fixture.stale = True
        result = self.recovery().drill(plan())
        self.assertFalse(result["rpo_met"])
        self.assertEqual(result["status"], "failed")
        self.assertFalse(any(m != "GET" for m, _, _ in self.fixture.calls))

    def test_preflight_refuses_foreign_unready_future_and_unsupported(self):
        for flag in ("foreign", "unready", "future"):
            with self.subTest(flag=flag):
                setattr(self.fixture, flag, True)
                with self.assertRaises(RecoveryError):
                    self.recovery().preflight(plan())
                setattr(self.fixture, flag, False)
        self.fixture.backend_type = "longhorn"
        with self.assertRaisesRegex(RecoveryError, "backend"):
            self.recovery().preflight(plan())
        self.assertFalse(any(m != "GET" for m, _, _ in self.fixture.calls))

    def test_application_hooks_start_verify_and_reverse_stop(self):
        p = plan()
        hooks = self.registry(p, ("start", "verify", "stop"))
        self.assertEqual(self.recovery(hooks).drill(p)["status"], "passed")
        self.assertEqual((self.root / "hooks.log").read_text().splitlines(),
                         ["db:start", "db:verify", "api:start", "api:verify", "api:stop", "db:stop"])

    def test_failed_start_is_stopped_and_previous_dependencies_cleaned(self):
        p = plan()
        hooks = self.registry(p, ("start", "verify", "stop"), fail="api-start")
        result = self.recovery(hooks).drill(p)
        self.assertEqual(result["status"], "failed")
        self.assertTrue(result["cleanup_complete"])
        self.assertEqual((self.root / "hooks.log").read_text().splitlines()[-2:], ["api:stop", "db:stop"])

    def test_failed_stop_preserves_volumes(self):
        p = plan()
        hooks = self.registry(p, ("start", "verify", "stop"), fail="api-stop")
        result = self.recovery(hooks).drill(p)
        self.assertEqual(result["status"], "failed")
        self.assertFalse(result["cleanup_complete"])
        self.assertEqual(len(self.fixture.clones), 2)

    def test_rto_violation_still_cleans(self):
        p = plan()
        p["rto_seconds"] = .000001
        result = self.recovery().drill(p)
        self.assertEqual(result["status"], "failed")
        self.assertFalse(result["rto_met"])
        self.assertTrue(result["cleanup_complete"])

    def test_capture_quiesces_reverse_and_always_resumes_forward(self):
        p = plan()
        hooks = self.registry(p, ("quiesce", "resume"))
        result = self.recovery(hooks).capture(p)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["consistency"], "hook_coordinated")
        self.assertEqual((self.root / "hooks.log").read_text().splitlines(),
                         ["api:quiesce", "db:quiesce", "db:resume", "api:resume"])
        self.assertTrue(all(v["snapshot_id"].startswith("captured_") for c in result["captured_plan"]["components"] for v in c["volumes"]))
        self.assertEqual(p["components"][0]["volumes"][0]["snapshot_id"], "snap_api")

    def test_capture_quiesce_failure_resumes_including_failed_component(self):
        p = plan()
        hooks = self.registry(p, ("quiesce", "resume"), fail="db-quiesce")
        result = self.recovery(hooks).capture(p)
        self.assertEqual(result["status"], "failed")
        self.assertEqual((self.root / "hooks.log").read_text().splitlines(),
                         ["api:quiesce", "db:quiesce", "db:resume", "api:resume"])
        self.assertFalse(any(m == "POST" for m, _, _ in self.fixture.calls))

    def test_capture_resume_failure_never_reports_success(self):
        p = plan()
        hooks = self.registry(p, ("quiesce", "resume"), fail="db-resume")
        result = self.recovery(hooks).capture(p)
        self.assertEqual(result["status"], "failed")
        self.assertTrue(result["cleanup_errors"])
        self.assertIn("api:resume", (self.root / "hooks.log").read_text())

    def test_capture_without_hooks_is_explicitly_crash_consistent(self):
        result = self.recovery().capture(plan())
        self.assertEqual(result["consistency"], "crash")
        self.assertFalse(any(m == "DELETE" for m, _, _ in self.fixture.calls))

    def test_latest_snapshot_is_pinned_without_changing_input(self):
        p = plan()
        for c in p["components"]:
            c["volumes"][0]["snapshot_id"] = "latest"
        result = self.recovery().drill(p)
        self.assertEqual(result["status"], "passed")
        self.assertEqual([r["snapshot_id"] for r in result["resources"]], ["snap_db", "snap_api"])
        self.assertEqual(p["components"][0]["volumes"][0]["snapshot_id"], "latest")

    def test_capture_deadline_resumes_sources(self):
        p = plan()
        p["capture_timeout_seconds"] = .000001
        result = self.recovery().capture(p)
        self.assertEqual(result["status"], "failed")
        self.assertIn("capture deadline", result["error"])
        self.assertFalse(any(m == "POST" for m, _, _ in self.fixture.calls))

    def test_report_disk_failure_does_not_block_source_resume(self):
        p = plan()
        hooks = self.registry(p, ("quiesce", "resume"))
        recovery = self.recovery(hooks)
        recovery.begin(p, "capture")
        with patch.object(recovery, "save", side_effect=OSError("disk full")):
            recovery.hook(p, p["components"][0], "resume")
        self.assertIn("api:resume", (self.root / "hooks.log").read_text())
        self.assertTrue(recovery.report["cleanup_errors"])

    def test_preflight_is_read_only(self):
        self.recovery().preflight(plan())
        self.assertTrue(all(m == "GET" for m, _, _ in self.fixture.calls))
        self.assertFalse(self.report.exists())

    def test_existing_report_refused_before_mutation(self):
        self.report.write_text("preserve")
        with self.assertRaises(RecoveryError):
            self.recovery().drill(plan())
        self.assertEqual(self.report.read_text(), "preserve")
        self.assertTrue(all(m == "GET" for m, _, _ in self.fixture.calls))

    def test_cli_success_failure_and_token_not_in_report(self):
        path = self.root / "plan.json"
        atomic_json(path, plan())
        args = [sys.executable, "-m", "atlas_recovery", "drill", "--plan", str(path),
                "--report", str(self.report), "--base-url", self.url]
        env = dict(os.environ, ATLAS_TOKEN="SECRET_TOKEN_SENTINEL")
        result = subprocess.run(args, capture_output=True, text=True, env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("SECRET_TOKEN_SENTINEL", self.report.read_text() + result.stdout + result.stderr)
        self.assertEqual(subprocess.run(args, capture_output=True, env=env).returncode, 2)


class TestValidation(unittest.TestCase):
    def test_cycle_duplicate_unknown_namespace_and_checks_refused(self):
        cases = []
        p = plan(); p["components"][1]["depends_on"] = ["api"]; cases.append(p)
        p = plan(); p["components"][1]["id"] = "api"; cases.append(p)
        p = plan(); p["components"][0]["depends_on"] = ["missing"]; cases.append(p)
        p = plan(); p["target_namespace"] = "production"; cases.append(p)
        p = plan(); p["components"][0]["volumes"][0]["checks"] = []; cases.append(p)
        p = plan(); p["components"][0]["volumes"][0]["checks"][0]["length"] = 4194305; cases.append(p)
        p = plan(); p["components"][0]["volumes"][0]["checks"][0]["offset"] = True; cases.append(p)
        p = plan(); p["rto_seconds"] = float("nan"); cases.append(p)
        p = plan(); p["components"][0]["volumes"][0]["volume_id"] = "../source"; cases.append(p)
        p = plan(); p["components"][0]["volumes"][0]["volume_id"] = "vol_native_db"; cases.append(p)
        for index, p in enumerate(cases):
            with self.subTest(case=index), self.assertRaises(RecoveryError):
                validate(p, {})

    def test_hook_registry_cannot_be_overridden_by_plan(self):
        p = plan(); p["components"][0]["verify_hook"] = "shell"
        with self.assertRaises(RecoveryError):
            validate(p, {})
        with self.assertRaises(RecoveryError):
            validate(p, {"shell": {"argv": ["sh", "-c", "echo unsafe"], "phases": ["verify"]}})

    def test_remote_http_and_embedded_credentials_refused(self):
        for url in ("http://example.com", "https://user:secret@example.com", "https://example.com/?q=secret"):
            with self.subTest(url=url), self.assertRaises(RecoveryError):
                AtlasClient(url)

    def test_timestamp_sqlite_utc_and_invalid(self):
        self.assertEqual(timestamp("2026-10-15 10:00:00").utcoffset(), dt.timedelta(0))
        with self.assertRaises(RecoveryError):
            timestamp(None)
        with self.assertRaises(RecoveryError):
            timestamp("garbage")

    def test_atomic_exclusive_write_preserves_existing_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "plan.json"
            atomic_json(path, {"a": 1}, exclusive=True)
            with self.assertRaises(FileExistsError):
                atomic_json(path, {"a": 2}, exclusive=True)
            self.assertEqual(json.loads(path.read_text()), {"a": 1})

    def test_hook_timeout_and_no_inherited_token(self):
        spec = {"slow": {"argv": [sys.executable, "-c", "import time; time.sleep(30)"],
                         "phases": ["verify"], "timeout_seconds": .03}}
        started = time.monotonic()
        with self.assertRaises(RecoveryError):
            HookRunner(spec).run("slow", "verify", {})
        self.assertLess(time.monotonic() - started, 2)
        spec["safe"] = {"argv": [sys.executable, "-c", "import os; assert 'ATLAS_TOKEN' not in os.environ"],
                        "phases": ["verify"]}
        with patch.dict(os.environ, {"ATLAS_TOKEN": "secret"}):
            HookRunner(spec).run("safe", "verify", {})


if __name__ == "__main__":
    unittest.main()
