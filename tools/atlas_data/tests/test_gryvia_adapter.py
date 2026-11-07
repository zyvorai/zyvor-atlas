# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Contract tests for HttpGryvia against a local fake of Gryvia's API gateway.

The fake copies routes, auth and payload shapes from the Gryvia repo (zyvorai/zyvor-gryvia,
services/api-gateway/); file:line references below are into that tree. Nothing here talks to a real
Gryvia: this proves the adapter matches what the sources say, not that a live gateway behaves so.
"""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
import time
import unittest

from atlas_data.controller import GryviaAdapter
from atlas_data.gryvia_adapter import GryviaAuthError, GryviaError, GryviaUnsupported, HttpGryvia

API_KEY = "k" * 32
ALLOWED_TOP = {"apiVersion", "kind", "metadata", "spec"}  # submission.py:70
ALLOWED_META = {"name", "namespace", "labels", "annotations"}  # submission.py:85


class FakeGateway:
    """State plus a scripted queue of failures: [(status, headers)] consumed before normal handling."""

    def __init__(self):
        self.jobs, self.log, self.failures, self.delay = {}, [], [], 0.0
        self.lock = threading.Lock()


def make_handler(gw):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def _send(self, code, body, headers=()):
            raw = body if isinstance(body, bytes) else json.dumps(body).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            for k, v in headers:
                self.send_header(k, v)
            self.end_headers()
            self.wfile.write(raw)

        def _auth(self):
            # main.py:393-430: no header -> 401; wrong key -> 403 "Invalid credentials".
            header = self.headers.get("Authorization")
            if not header:
                self._send(401, {"detail": "Authorization header required"})
            elif header.removeprefix("Bearer ").strip() != API_KEY:
                self._send(403, {"detail": "Invalid credentials"})
            else:
                return True
            return False

        def _handle(self, method):
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else b""
            with gw.lock:
                gw.log.append((method, self.path, self.headers.get("Authorization"), body))
                scripted = gw.failures.pop(0) if gw.failures else None
            if gw.delay:
                time.sleep(gw.delay)
            if not self._auth():
                return
            if scripted:
                code, headers = scripted
                return self._send(code, b"<html>gateway</html>" if code == 200 else {"detail": "scripted"}, headers)
            if method == "GET" and self.path.startswith("/api/jobs/"):  # main.py:967-1003 get_job
                name = self.path.rsplit("/", 1)[1]
                job = gw.jobs.get(name)
                return self._send(200, job) if job else self._send(404, {"detail": f"Job '{name}' not found"})
            if method == "POST" and self.path == "/api/jobs":  # main.py:1017-1042 create_job
                return self._create(body)
            self._send(404, {"detail": "Not Found"})

        def _create(self, raw):
            try:
                job = json.loads(raw)
            except ValueError:
                return self._send(400, {"detail": "Request body must be valid JSON"})
            md, spec = job.get("metadata"), job.get("spec")
            if set(job) - ALLOWED_TOP or job.get("apiVersion") != "gryvia.io/v1alpha1" or job.get("kind") != "GryviaAIJob":
                return self._send(400, {"detail": "Expected gryvia.io/v1alpha1 GryviaAIJob"})  # submission.py:70-78
            if set(md) - ALLOWED_META or not isinstance(spec.get("image"), str) or not isinstance(spec.get("gpus"), int):
                return self._send(400, {"detail": "invalid job"})  # submission.py:85-103
            if md["name"] in gw.jobs:  # k8s AlreadyExists surfaces as exc.status (main.py:1036-1038)
                return self._send(409, {"detail": "Failed to create job"})
            job["metadata"]["namespace"] = "default"  # submission.py:107
            gw.jobs[md["name"]] = job  # status absent until the operator reconciles
            self._send(200, job)

        def do_GET(self):
            self._handle("GET")

        def do_POST(self):
            self._handle("POST")

    return Handler


def manifest(name="train-1", **spec):
    return {"apiVersion": "gryvia.io/v1alpha1", "kind": "GryviaAIJob", "metadata": {"name": name},
            "spec": {"image": "img:1", "gpus": 2, **spec}}


class Base(unittest.TestCase):
    def setUp(self):
        self.gw = FakeGateway()
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(self.gw))
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.sleeps = []
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"

    def adapter(self, token=API_KEY, **kw):
        kw.setdefault("sleep", self.sleeps.append)
        return HttpGryvia(self.url, token, **kw)

    def requests(self, method=None):
        return [r for r in self.gw.log if method is None or r[0] == method]


class JobStateTests(Base):
    def test_is_an_adapter(self):
        self.assertIsInstance(self.adapter(), GryviaAdapter)

    def test_phases_map_to_controller_states(self):
        # Phases from crds/gryvia.io_gryviaaijobs.yaml:3683 and docs/aijob-lifecycle.md:68-89.
        want = {"Pending": "pending", "Queued": "pending", "Scheduling": "pending", "Running": "running",
                "Succeeded": "succeeded", "Failed": "failed", "Rejected": "failed", "Cancelled": "cancelled",
                "Preempted": "cancelled"}
        client = self.adapter()
        for phase, state in want.items():
            self.gw.jobs["j"] = {"metadata": {"name": "j"}, "spec": {}, "status": {"phase": phase}}
            self.assertEqual(client.job_state("j"), state, phase)

    def test_job_without_status_is_pending_and_unknown_phase_is_an_error(self):
        client = self.adapter()
        self.gw.jobs["j"] = {"metadata": {"name": "j"}, "spec": {}}
        self.assertEqual(client.job_state("j"), "pending")
        self.gw.jobs["j"]["status"] = {"phase": "Unknown"}  # lifecycle doc: "unused"
        with self.assertRaisesRegex(GryviaError, "unrecognised phase"):
            client.job_state("j")

    def test_bearer_token_is_sent(self):
        self.gw.jobs["j"] = {"status": {"phase": "Running"}}
        self.adapter().job_state("j")
        self.assertEqual(self.requests()[0][2], f"Bearer {API_KEY}")

    def test_missing_job_is_a_404_error(self):
        with self.assertRaises(GryviaError) as ctx:
            self.adapter().job_state("ghost")
        self.assertEqual(ctx.exception.status, 404)
        self.assertIn("not found", str(ctx.exception))

    def test_registered_but_not_yet_created_job_is_pending(self):
        client = self.adapter()
        client.register_job("train-1", manifest())
        self.assertEqual(client.job_state("train-1"), "pending")

    def test_non_dns_job_id_is_refused_without_a_request(self):
        with self.assertRaisesRegex(GryviaError, "DNS label"):
            self.adapter().job_state("Job_1/../x")
        self.assertEqual(self.gw.log, [])


class AuthTests(Base):
    def test_wrong_token_is_403_and_never_retried(self):
        with self.assertRaises(GryviaAuthError) as ctx:
            self.adapter(token="wrong-token").job_state("j")
        self.assertEqual(ctx.exception.status, 403)
        self.assertEqual(len(self.gw.log), 1)
        self.assertNotIn("wrong-token", str(ctx.exception))

    def test_empty_token_and_bad_url_rejected_at_construction(self):
        with self.assertRaises(ValueError):
            HttpGryvia(self.url, " ")
        with self.assertRaises(ValueError):
            HttpGryvia("gryvia:8080", API_KEY)

    def test_401_status_from_server_is_an_auth_error(self):
        self.gw.failures.append((401, ()))  # scripted after auth passes, still maps by status
        # The scripted failure is only reached with a valid token, which is fine: status decides the class.
        with self.assertRaises(GryviaAuthError):
            self.adapter().job_state("j")


class ApplySelectorTests(Base):
    def test_creates_the_job_with_the_selector_merged(self):
        client = self.adapter()
        client.register_job("train-1", manifest(nodeSelector={"gpu": "a100"}))
        client.apply_node_selector("train-1", {"atlas.zyvor.ai/cache-node": "n1"})
        (_, path, _, body), = self.requests("POST")
        self.assertEqual(path, "/api/jobs")
        sent = json.loads(body)
        self.assertEqual(sent["spec"]["nodeSelector"], {"gpu": "a100", "atlas.zyvor.ai/cache-node": "n1"})
        self.assertEqual(set(sent), ALLOWED_TOP)
        self.assertEqual(self.gw.jobs["train-1"]["spec"]["image"], "img:1")

    def test_unregistered_job_is_unsupported_not_faked(self):
        with self.assertRaises(GryviaUnsupported):
            self.adapter().apply_node_selector("train-1", {"a": "b"})
        self.assertEqual(self.gw.log, [])

    def test_selector_conflicting_with_manifest_is_refused(self):
        client = self.adapter()
        client.register_job("train-1", manifest(nodeSelector={"zone": "x"}))
        with self.assertRaisesRegex(GryviaError, "zone"):
            client.apply_node_selector("train-1", {"zone": "y"})
        self.assertEqual(self.gw.log, [])

    def test_manifest_rules_are_checked_at_registration(self):
        client = self.adapter()
        bad = [manifest(gpus=0), manifest(image=" "), {**manifest(), "status": {}}, manifest(name="other"),
               {**manifest(), "kind": "Job"}]
        for item in bad:
            with self.assertRaises(GryviaError, msg=item):
                client.register_job("train-1", item)

    def test_server_side_400_is_surfaced_with_detail(self):
        client = self.adapter()
        client.register_job("train-1", manifest())
        client.manifests["train-1"]["metadata"]["finalizers"] = ["x"]  # bypass local check
        with self.assertRaisesRegex(GryviaError, "HTTP 400"):
            client.apply_node_selector("train-1", {"a": "b"})
        self.assertEqual(len(self.requests("POST")), 1)  # 4xx is not retried

    def test_409_with_identical_placement_is_success(self):
        client = self.adapter()
        client.register_job("train-1", manifest())
        client.apply_node_selector("train-1", {"a": "b"})
        client2 = self.adapter()
        client2.register_job("train-1", manifest())
        client2.apply_node_selector("train-1", {"a": "b"})  # create -> 409 -> GET -> same selector
        self.assertEqual(client2.job_state("train-1"), "pending")

    def test_409_with_different_placement_is_an_error(self):
        self.adapter().register_job("train-1", manifest())
        first = self.adapter()
        first.register_job("train-1", manifest())
        first.apply_node_selector("train-1", {"a": "b"})
        second = self.adapter()
        second.register_job("train-1", manifest())
        with self.assertRaisesRegex(GryviaError, "different nodeSelector"):
            second.apply_node_selector("train-1", {"a": "other"})


class RetryTests(Base):
    def test_503_then_success_backs_off_exponentially(self):
        self.gw.jobs["j"] = {"status": {"phase": "Running"}}
        self.gw.failures += [(503, ()), (503, ())]
        self.assertEqual(self.adapter(backoff=0.5).job_state("j"), "running")
        self.assertEqual(self.sleeps, [0.5, 1.0])
        self.assertEqual(len(self.gw.log), 3)

    def test_retries_are_bounded(self):
        self.gw.failures += [(503, ())] * 10
        with self.assertRaises(GryviaError) as ctx:
            self.adapter(retries=2).job_state("j")
        self.assertEqual(ctx.exception.status, 503)
        self.assertEqual(len(self.gw.log), 3)

    def test_429_honours_retry_after_capped(self):
        # slowapi rate limits (main.py:968 "30/minute") answer 429.
        self.gw.jobs["j"] = {"status": {"phase": "Running"}}
        self.gw.failures += [(429, [("Retry-After", "2")]), (429, [("Retry-After", "999")])]
        self.adapter(max_backoff=5).job_state("j")
        self.assertEqual(self.sleeps, [2.0, 5.0])

    def test_500_is_retried_for_reads_but_not_for_create(self):
        self.gw.jobs["j"] = {"status": {"phase": "Running"}}
        self.gw.failures.append((500, ()))
        self.assertEqual(self.adapter().job_state("j"), "running")  # main.py:1000,1003 return 500 on read errors
        client = self.adapter()
        client.register_job("train-1", manifest())
        self.gw.failures.append((500, ()))
        with self.assertRaises(GryviaError) as ctx:
            client.apply_node_selector("train-1", {"a": "b"})
        self.assertEqual(ctx.exception.status, 500)
        self.assertEqual(len(self.requests("POST")), 1)

    def test_create_is_retried_after_503(self):
        client = self.adapter()
        client.register_job("train-1", manifest())
        self.gw.failures.append((503, ()))
        client.apply_node_selector("train-1", {"a": "b"})
        self.assertEqual(len(self.requests("POST")), 2)

    def test_timeout_is_retried_then_reported_as_504(self):
        self.gw.delay = 0.4
        client = self.adapter(timeout=0.1, retries=1)
        with self.assertRaises(GryviaError) as ctx:
            client.job_state("j")
        self.assertEqual(ctx.exception.status, 504)
        self.assertIn("timed out", str(ctx.exception))
        self.assertEqual(len(self.sleeps), 1)

    def test_unreachable_gateway_is_502_after_retries(self):
        self.server.shutdown()
        self.server.server_close()
        with self.assertRaises(GryviaError) as ctx:
            self.adapter(retries=2).job_state("j")
        self.assertEqual(ctx.exception.status, 502)
        self.assertEqual(len(self.sleeps), 2)

    def test_non_json_success_body_is_an_error(self):
        self.gw.failures.append((200, ()))
        with self.assertRaisesRegex(GryviaError, "not JSON"):
            self.adapter().job_state("j")


class ReportTests(Base):
    def test_report_is_local_only_because_gryvia_has_no_route(self):
        client = self.adapter()
        with self.assertLogs("atlas_data.gryvia", "WARNING"):
            client.report("j", "data lease lost")
        self.assertEqual(client.reports, [("j", "data lease lost")])
        self.assertEqual(self.gw.log, [])


if __name__ == "__main__":
    unittest.main()
