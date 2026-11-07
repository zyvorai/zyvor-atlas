# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""`HttpGryvia`: a `GryviaAdapter` over Gryvia's REST API gateway (services/api-gateway).

Route and payload shapes are taken from the Gryvia sources (repo zyvorai/zyvor-gryvia), not from a
live instance; this module has only been run against a local fake that copies those shapes.

Mapping:
  job_state            GET  /api/jobs/{name}  -> status.phase (CRD GryviaAIJob phases)
  apply_node_selector  POST /api/jobs         -> creates the job with spec.nodeSelector set
  report               no Gryvia route exists: logged locally and kept in `reports`

Gryvia has no route that changes a submitted job (no PATCH/PUT on /api/jobs/{name}), so placement
can only be applied at creation. The caller therefore registers the job manifest with
`register_job` and Gryvia first sees the job when the data is ready and the selector is applied.
"""
import json
import logging
import socket
import time
import urllib.error
import urllib.parse
import urllib.request

from .controller import GryviaAdapter
from .core import DataError

LOG = logging.getLogger("atlas_data.gryvia")

API_VERSION, KIND = "gryvia.io/v1alpha1", "GryviaAIJob"
# CRD status.phase (crds/gryvia.io_gryviaaijobs.yaml:3683, docs/aijob-lifecycle.md:68-89) -> adapter state.
PHASES = {
    "": "pending", "Pending": "pending", "Queued": "pending", "Scheduling": "pending",
    "Running": "running", "Succeeded": "succeeded", "Failed": "failed", "Rejected": "failed",
    "Cancelled": "cancelled",
    "Preempted": "cancelled",  # parked and not resumed automatically: the run is over, release the lease
}
RETRY_ALL = frozenset({429, 502, 503, 504})  # safe for POST too: a repeated create answers 409, reconciled below
RETRY_READ = RETRY_ALL | {500}  # the gateway answers 500 for transient Kubernetes errors on reads
MAX_REPORTS = 100


class GryviaError(DataError):
    """A Gryvia call failed. `status` is the HTTP status (502 unreachable, 504 timeout)."""

    # Retry metadata, set by `_once` on errors that came from a request. Errors raised by
    # validation carry none, so these defaults mean "not retryable, no Retry-After".
    transport: bool = False
    http: bool = False
    retry_after: "float | None" = None  # quoted: not evaluated at runtime (Python 3.9 compatible)


class GryviaAuthError(GryviaError):
    """401/403 from the gateway: missing, wrong or insufficient token. Never retried."""


class GryviaUnsupported(GryviaError):
    """The requested capability does not exist in Gryvia's API."""


def _dns_label(name):
    # submission.py:13 NAME and :83 (1-63 chars).
    return (isinstance(name, str) and 0 < len(name) <= 63 and name[0].isalnum() and name[-1].isalnum()
            and all(c.isalnum() or c == "-" for c in name) and name == name.lower())


class HttpGryvia(GryviaAdapter):
    def __init__(self, base_url, token, timeout=10.0, retries=3, backoff=0.5, max_backoff=8.0,
                 sleep=time.sleep, ssl_context=None):
        if not base_url.startswith(("http://", "https://")):
            raise ValueError("base_url must be http(s)://...")
        if not token or not token.strip():
            raise ValueError("a Gryvia token is required")
        if timeout <= 0 or retries < 0 or backoff < 0:
            raise ValueError("timeout must be positive; retries and backoff must not be negative")
        self.base = base_url.rstrip("/")
        self._token = token.strip()
        self.timeout, self.retries, self.backoff, self.max_backoff = timeout, retries, backoff, max_backoff
        self._sleep, self._ssl = sleep, ssl_context
        self.manifests, self.submitted, self.reports = {}, set(), []

    # -- adapter interface -------------------------------------------------------------------

    def register_job(self, job, manifest):
        """Remember the GryviaAIJob manifest to create once `apply_node_selector` is called."""
        body = self._check_manifest(job, manifest)
        self.manifests[job] = body

    def apply_node_selector(self, job, node_selector):
        manifest = self.manifests.get(job)
        if manifest is None:
            raise GryviaUnsupported(
                f"job {job!r} has no registered manifest and Gryvia cannot change the placement of an "
                "existing job (no update route on /api/jobs/{name}); call register_job() first", 409)
        spec = dict(manifest["spec"])
        merged = dict(spec.get("nodeSelector") or {})
        for key, value in node_selector.items():
            if key in merged and merged[key] != value:
                raise GryviaError(f"nodeSelector {key!r} is {merged[key]!r} in the manifest, data needs {value!r}", 409)
            merged[key] = value
        body = {**manifest, "spec": {**spec, "nodeSelector": merged}}
        try:
            self._call("POST", "/api/jobs", body, RETRY_ALL)  # main.py:1017-1042 create_job
        except GryviaError as exc:
            if exc.status != 409:
                raise
            # Already exists (retried create, or an earlier attempt): fine only if placed identically.
            existing = self._call("GET", self._job_path(job), None, RETRY_READ)
            if ((existing.get("spec") or {}).get("nodeSelector") or {}) != merged:
                raise GryviaError(f"job {job!r} already exists in Gryvia with a different nodeSelector", 409) from None
        self.submitted.add(job)

    def job_state(self, job):
        try:
            obj = self._call("GET", self._job_path(job), None, RETRY_READ)  # main.py:967-1003 get_job
        except GryviaError as exc:
            if exc.status == 404 and job in self.manifests and job not in self.submitted:
                return "pending"  # not created yet: waiting for apply_node_selector
            raise
        phase = (obj.get("status") or {}).get("phase") or ""
        if phase not in PHASES:
            raise GryviaError(f"job {job!r} has unrecognised phase {phase!r}", 502)
        return PHASES[phase]

    def report(self, job, message):
        """Gryvia has no route to attach a message to a job; keep it locally and log it."""
        self.reports.append((job, message))
        del self.reports[:-MAX_REPORTS]
        LOG.warning("atlas data path, job %s: %s", job, message)

    # -- internals ---------------------------------------------------------------------------

    @staticmethod
    def _job_path(job):
        if not _dns_label(job):
            raise GryviaError(f"job {job!r} is not a DNS label, so it cannot be a Gryvia job name", 400)
        return "/api/jobs/" + urllib.parse.quote(job, safe="")

    def _check_manifest(self, job, manifest):
        """The admission rules of submission.normalize_job (submission.py:67-108), checked up front."""
        self._job_path(job)
        if not isinstance(manifest, dict) or set(manifest) - {"apiVersion", "kind", "metadata", "spec"}:
            raise GryviaError("manifest must be an object with only apiVersion, kind, metadata, spec", 400)
        md, spec = manifest.get("metadata"), manifest.get("spec")
        if manifest.get("apiVersion") != API_VERSION or manifest.get("kind") != KIND:
            raise GryviaError(f"manifest must be {API_VERSION} {KIND}", 400)
        if not isinstance(md, dict) or not isinstance(spec, dict) or md.get("name") != job:
            raise GryviaError("metadata.name must equal the job id and spec must be an object", 400)
        if set(md) - {"name", "namespace", "labels", "annotations"}:
            raise GryviaError("metadata may only hold name, namespace, labels, annotations", 400)
        gpus = spec.get("gpus")
        if not isinstance(spec.get("image"), str) or not spec["image"].strip() or isinstance(gpus, bool) \
                or not isinstance(gpus, int) or not 1 <= gpus <= 1048576:
            raise GryviaError("spec.image and spec.gpus (1..1048576) are required", 400)
        return json.loads(json.dumps(manifest))

    def _call(self, method, path, body, retry_on):
        data = None if body is None else json.dumps(body).encode()
        attempt = 0
        while True:
            try:
                return self._once(method, path, data)
            except GryviaError as exc:
                retryable = exc.transport or (exc.http and exc.status in retry_on)
                if not retryable or attempt >= self.retries:
                    raise
                delay = min(exc.retry_after if exc.retry_after is not None else self.backoff * 2 ** attempt,
                            self.max_backoff)
                attempt += 1
                self._sleep(delay)

    def _once(self, method, path, data):
        req = urllib.request.Request(self.base + path, data=data, method=method, headers={
            "Authorization": f"Bearer {self._token}", "Accept": "application/json",
            **({"Content-Type": "application/json"} if data is not None else {})})
        try:
            with urllib.request.urlopen(req, timeout=self.timeout, context=self._ssl) as resp:
                raw = resp.read()
        except urllib.error.HTTPError as exc:
            raise self._http_error(method, path, exc) from None
        except (socket.timeout, TimeoutError):
            raise self._transport(f"{method} {path} timed out after {self.timeout}s", 504) from None
        except urllib.error.URLError as exc:
            timed_out = isinstance(exc.reason, (socket.timeout, TimeoutError))
            raise self._transport(f"{method} {path}: {'timed out' if timed_out else exc.reason}",
                                  504 if timed_out else 502) from None
        except OSError as exc:  # reset mid-response
            raise self._transport(f"{method} {path}: {exc}", 502) from None
        try:
            obj = json.loads(raw)
        except ValueError:
            raise GryviaError(f"{method} {path}: response is not JSON", 502) from None
        if not isinstance(obj, dict):
            raise GryviaError(f"{method} {path}: response is not a JSON object", 502)
        return obj

    @staticmethod
    def _transport(message, status):
        err = GryviaError(message, status)
        err.transport, err.http, err.retry_after = True, False, None
        return err

    @staticmethod
    def _http_error(method, path, exc):
        detail = ""
        try:
            detail = str(json.loads(exc.read() or b"{}").get("detail", ""))[:300]  # FastAPI error body
        except (ValueError, AttributeError, OSError):
            pass
        cls = GryviaAuthError if exc.code in (401, 403) else GryviaError
        err = cls(f"{method} {path}: HTTP {exc.code}" + (f": {detail}" if detail else ""), exc.code)
        err.transport, err.http, err.retry_after = False, True, None
        if exc.code == 429:
            try:
                err.retry_after = max(0.0, float(exc.headers.get("Retry-After")))
            except (TypeError, ValueError):
                pass
        return err
