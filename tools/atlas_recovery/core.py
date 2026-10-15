# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Recovery planning, bounded HTTP operations, hooks and durable drill evidence."""
from __future__ import annotations

import copy
import datetime as dt
import hashlib
import ipaddress
import json
import math
import os
from pathlib import Path
import re
import signal
import ssl
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


class ApiError(Exception):
    def __init__(self, method, status):
        self.status = status
        super().__init__(f"Atlas {method} failed: HTTP {status}")


class RecoveryError(Exception):
    """An operational error whose message is safe to put in a report."""


def require(condition, message):
    if not condition:
        raise RecoveryError(message)


def utcnow():
    return dt.datetime.now(dt.timezone.utc)


def timestamp(value):
    require(isinstance(value, str), "snapshot has no creation timestamp")
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
        # SQLite Atlas timestamps are UTC without an explicit offset.
        return parsed.replace(tzinfo=dt.timezone.utc) if parsed.tzinfo is None else parsed
    except ValueError as exc:
        raise RecoveryError("invalid snapshot creation timestamp") from exc


def positive(value, label, maximum=86400):
    require(type(value) in (int, float) and math.isfinite(value) and 0 < value <= maximum,
            f"{label} must be positive and <= {maximum}")
    return value


def identifier(value):
    require(isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.:-]{0,199}", value),
            "invalid resource identifier")
    return urllib.parse.quote(value, safe="")


def validate(plan, hooks, capture=False):
    require(isinstance(hooks, dict), "hooks registry must be an object")
    require(isinstance(plan, dict) and plan.get("version") == 1, "plan version must be 1")
    require(isinstance(plan.get("name"), str) and re.fullmatch(r"[a-z][a-z0-9-]{0,29}", plan["name"]),
            "plan name must be lowercase and <= 30 characters")
    identifier(plan.get("tenant_id"))
    ns = plan.get("target_namespace", "")
    require(isinstance(ns, str) and len(ns) <= 63 and re.fullmatch(r"atlas-drill-[a-z0-9]+(?:-[a-z0-9]+)*", ns),
            "target_namespace must start with atlas-drill- and be a DNS label <= 63 characters")
    positive(plan.get("rpo_seconds", 3600), "rpo_seconds", 31536000)
    positive(plan.get("rto_seconds", 600), "rto_seconds", 86400)
    positive(plan.get("capture_timeout_seconds", 120), "capture timeout", 600)
    components = plan.get("components")
    require(isinstance(components, list) and 0 < len(components) <= 64, "plan needs 1-64 components")
    ids, sources = set(), set()
    for component in components:
        require(isinstance(component, dict), "component must be an object")
        cid = component.get("id")
        require(isinstance(cid, str) and re.fullmatch(r"[a-z][a-z0-9-]{0,19}", cid), "invalid component id")
        require(cid not in ids, "duplicate component id")
        ids.add(cid)
        deps = component.get("depends_on", [])
        require(isinstance(deps, list) and all(isinstance(d, str) for d in deps) and len(set(deps)) == len(deps),
                "depends_on must contain unique component ids")
        require(bool(component.get("start_hook")) == bool(component.get("stop_hook")),
                "start_hook and stop_hook must be paired")
        require(bool(component.get("quiesce_hook")) == bool(component.get("resume_hook")),
                "quiesce_hook and resume_hook must be paired")
        for phase in ("start", "verify", "stop", "quiesce", "resume"):
            name = component.get(phase + "_hook")
            if name is not None:
                require(isinstance(name, str) and name in hooks, f"unknown {phase} hook")
                hook = hooks[name]
                require(isinstance(hook, dict) and phase in hook.get("phases", []), f"hook not allowed for {phase}")
                argv = hook.get("argv")
                require(isinstance(argv, list) and argv and all(isinstance(a, str) and a and "\0" not in a for a in argv)
                        and os.path.isabs(argv[0]), "hook argv needs an absolute executable path")
                positive(hook.get("timeout_seconds", 30), "hook timeout", 600)
        volumes = component.get("volumes")
        require(isinstance(volumes, list) and 0 < len(volumes) <= 16, "component needs 1-16 volumes")
        for volume in volumes:
            require(isinstance(volume, dict), "volume must be an object")
            source = volume.get("volume_id")
            identifier(source)
            require(source not in sources, "source volume occurs more than once")
            sources.add(source)
            if not capture:
                identifier(volume.get("snapshot_id"))
            checks = volume.get("checks", [])
            require(isinstance(checks, list) and len(checks) <= 64, "checks must be a list of at most 64 ranges")
            for check in checks:
                require(isinstance(check, dict), "checksum check must be an object")
                require(type(check.get("offset")) is int and check["offset"] >= 0, "invalid checksum offset")
                require(type(check.get("length")) is int and 0 < check["length"] <= 4 * 1024 * 1024,
                        "checksum length must be 1 byte to 4 MiB")
                require(isinstance(check.get("sha256"), str) and re.fullmatch(r"[0-9a-f]{64}", check["sha256"]),
                        "checksum must be a lowercase SHA-256 digest")
            require(checks or component.get("verify_hook"), "every volume needs checks or a component verify_hook")
    by_id = {c["id"]: c for c in components}
    ordered, visiting, done = [], set(), set()

    def visit(cid):
        require(cid in by_id, "unknown dependency")
        require(cid not in visiting, "dependency cycle")
        if cid in done:
            return
        visiting.add(cid)
        for dependency in by_id[cid].get("depends_on", []):
            visit(dependency)
        visiting.remove(cid)
        done.add(cid)
        ordered.append(by_id[cid])

    for cid in by_id:
        visit(cid)
    return ordered


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise RecoveryError("Atlas redirects are refused; configure the final gateway URL")


class AtlasClient:
    def __init__(self, base_url, token=None, ca_file=None, timeout=15, job_timeout=300, poll_seconds=1):
        parsed = urllib.parse.urlsplit(base_url)
        require(parsed.scheme in ("https", "http") and parsed.hostname and not parsed.username
                and not parsed.password and not parsed.query and not parsed.fragment, "invalid Atlas URL")
        if parsed.scheme == "http":
            host = parsed.hostname
            try:
                local = ipaddress.ip_address(host).is_loopback
            except ValueError:
                local = host == "localhost"
            require(local, "remote Atlas URLs require HTTPS")
        positive(timeout, "HTTP timeout", 600)
        positive(job_timeout, "job timeout", 86400)
        positive(poll_seconds, "poll interval", 60)
        self.base = base_url.rstrip("/")
        self.timeout, self.job_timeout, self.poll_seconds = timeout, job_timeout, poll_seconds
        self.token = token
        self.deadline = None
        context = ssl.create_default_context(cafile=ca_file)
        self.opener = urllib.request.build_opener(NoRedirect(), urllib.request.HTTPSHandler(context=context))

    def request(self, method, path, body=None, binary=False):
        require(path.startswith("/") and not path.startswith("//"), "invalid API path")
        headers = {"Accept": "application/octet-stream" if binary else "application/json"}
        if self.token:
            headers["Authorization"] = "Bearer " + self.token
        payload = None if body is None else json.dumps(body).encode()
        if payload is not None:
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(self.base + path, data=payload, headers=headers, method=method)
        timeout = self.timeout
        if self.deadline is not None:
            remaining = self.deadline - time.monotonic()
            require(remaining > 0, "capture deadline exceeded")
            timeout = min(timeout, remaining)
        try:
            with self.opener.open(req, timeout=timeout) as response:
                limit = 4 * 1024 * 1024 if binary else 16 * 1024 * 1024
                raw = response.read(limit + 1)
                require(len(raw) <= limit, "Atlas response exceeds size limit")
                return raw if binary else (json.loads(raw) if raw else {})
        except urllib.error.HTTPError as exc:
            raise ApiError(method, exc.code) from None
        except (urllib.error.URLError, TimeoutError, OSError) as exc:
            # Do not include URL credentials, request bodies, tokens or server response bodies.
            raise RecoveryError(f"Atlas {method} transport failure; mutation outcome may be unknown") from exc
        except (ValueError, UnicodeError) as exc:
            raise RecoveryError("Atlas returned invalid JSON; mutation outcome may be unknown") from exc

    def wait(self, response):
        job = response.get("job_id")
        if not job:
            return response
        deadline = time.monotonic() + self.job_timeout
        if self.deadline is not None:
            deadline = min(deadline, self.deadline)
        while True:
            result = self.request("GET", "/jobs/" + identifier(job))
            state = result.get("state")
            if state == "succeeded":
                return response
            require(state not in ("failed", "cancelled", "canceled"), f"Atlas job {job} {state}")
            require(time.monotonic() < deadline, f"Atlas job {job} timed out; it may still be running")
            time.sleep(min(self.poll_seconds, max(0, deadline - time.monotonic())))

    def resource(self, response, key):
        value = response.get(key) or response.get("resource", {}).get(key)
        identifier(value)
        return value


def atomic_json(path, value, exclusive=False):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, temporary = tempfile.mkstemp(prefix=".atlas-recovery-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream, indent=2, sort_keys=True, allow_nan=False)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        if exclusive:
            os.link(temporary, path)
        else:
            os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


class HookRunner:
    """Trusted operator hook config; plans name hooks but cannot supply shell commands."""
    def __init__(self, hooks):
        self.hooks = hooks

    def run(self, name, phase, context, timeout=None):
        spec = self.hooks[name]
        require(phase in spec["phases"], "hook phase is not allowed")
        # No inherited Atlas token or cloud credentials. stdout/stderr never enter reports.
        env = {k: os.environ[k] for k in ("PATH", "LANG", "LC_ALL", "SYSTEMROOT") if k in os.environ}
        env["ATLAS_RECOVERY_PHASE"] = phase
        with tempfile.TemporaryDirectory(prefix="atlas-hook-") as cwd:
            with tempfile.TemporaryFile() as input_file:
                input_file.write(json.dumps(context).encode())
                input_file.seek(0)
                process = subprocess.Popen(spec["argv"], stdin=input_file, stdout=subprocess.DEVNULL,
                                           stderr=subprocess.DEVNULL, cwd=cwd, env=env,
                                           start_new_session=(os.name == "posix"))
                try:
                    code = process.wait(timeout=min(spec.get("timeout_seconds", 30), timeout) if timeout is not None else spec.get("timeout_seconds", 30))
                except BaseException:
                    if os.name == "posix":
                        try:
                            os.killpg(process.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    else:
                        process.kill()
                    process.wait()
                    raise RecoveryError(f"{phase} hook interrupted or timed out") from None
                require(code == 0, f"{phase} hook exited with status {code}")


class Recovery:
    def __init__(self, client, hooks, report_path):
        self.client, self.hooks = client, hooks
        self.runner = HookRunner(hooks)
        self.path = Path(report_path)
        self.report = {}

    def save(self):
        atomic_json(self.path, self.report)

    def context(self, plan, component, phase):
        return {"version": 1, "phase": phase, "run_id": self.report["run_id"],
                "tenant_id": plan["tenant_id"], "namespace": plan["target_namespace"],
                "component_id": component["id"],
                "volumes": [r for r in self.report["resources"] if r["component_id"] == component["id"]],
                "sources": [v["volume_id"] for v in component["volumes"]]}

    def hook(self, plan, component, phase, timeout=None):
        name = component.get(phase + "_hook")
        if name:
            def record():
                try:
                    self.save()
                except OSError:
                    # A full report disk must not prevent a source resume or a workload stop.
                    if phase not in ("resume", "stop"):
                        raise
                    self.report["cleanup_errors"].append(f"journal write failed during {phase}")

            event = {"component_id": component["id"], "phase": phase, "status": "running"}
            self.report["events"].append(event)
            record()
            try:
                self.runner.run(name, phase, self.context(plan, component, phase), timeout=timeout)
            except BaseException:
                event["status"] = "failed"
                record()
                raise
            event["status"] = "succeeded"
            record()

    def preflight(self, plan, capture=False):
        plan = copy.deepcopy(plan)
        ordered = validate(plan, self.hooks, capture)
        visible = self.client.request("GET", "/volumes?" + urllib.parse.urlencode({"tenant": plan["tenant_id"]}))
        require(isinstance(visible, list), "Atlas volume inventory must be a list")
        volumes = {v["id"]: v for v in visible}
        snapshots = {} if capture else {s["id"]: s for s in self.client.request("GET", "/snapshots")}
        backends = {b["id"]: b for b in self.client.request("GET", "/backends")}
        clusters = {c["id"]: c for c in self.client.request("GET", "/clusters")}
        now, ages = utcnow(), []
        for c in ordered:
            for v in c["volumes"]:
                require(v["volume_id"] in volumes, "source volume is not in the requested tenant")
                source = volumes[v["volume_id"]]
                backend = backends.get(clusters.get(source.get("cluster_id"), {}).get("backend_id"), {})
                require(backend.get("backend_type") in ("native", "ceph") and
                        backend.get("capabilities", {}).get("snapshots") and
                        backend.get("capabilities", {}).get("clone"), "backend does not support this recovery path")
                require(source.get("kubernetes_namespace") != plan["target_namespace"], "drill namespace equals source namespace")
                for check in v.get("checks", []):
                    require(source.get("kind") == "block" and backend.get("backend_type") == "native",
                            "range checks require an Atlas Native block volume; use verify_hook for other backends")
                    require(check["offset"] + check["length"] <= source["size_bytes"], "checksum range exceeds source size")
                if not capture:
                    if v["snapshot_id"] == "latest":
                        candidates = [s for s in snapshots.values() if s.get("volume_id") == v["volume_id"]
                                      and s.get("tenant_id") == plan["tenant_id"] and s.get("state") == "ready"]
                        require(candidates, "no ready snapshot available for source volume")
                        v["snapshot_id"] = max(candidates, key=lambda s: timestamp(s.get("created_at")))["id"]
                    snap = snapshots.get(v["snapshot_id"], {})
                    require(snap.get("volume_id") == v["volume_id"] and snap.get("tenant_id") == plan["tenant_id"],
                            "snapshot does not belong to the source volume and tenant")
                    require(snap.get("state") == "ready", "snapshot is not ready")
                    age = (now - timestamp(snap.get("created_at"))).total_seconds()
                    require(age >= -5, "snapshot timestamp is in the future")
                    ages.append(max(0, age))
        return ordered, max(ages, default=0)

    def begin(self, plan, mode):
        require(not self.path.exists(), "report already exists; choose a new report path")
        # Reserve before making API mutations, so simultaneous invocations cannot share a journal.
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        try:
            fd = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
            os.close(fd)
        except FileExistsError:
            raise RecoveryError("report already exists; choose a new report path") from None
        self.report = {"version": 1, "run_id": uuid.uuid4().hex, "plan": plan["name"], "mode": mode,
                       "started_at": utcnow().isoformat(), "status": "running", "resources": [],
                       "events": [], "cleanup_errors": [], "uncertain_operations": [],
                       "source_volume_ids": [v["volume_id"] for c in plan["components"] for v in c["volumes"]]}
        self.save()

    def mutation(self, method, path, body, resource):
        # Persist intent before sending; never blindly retry mutations after connection loss.
        intent = {"method": method, "path": path, "name": body.get("name") if body else None}
        self.report["uncertain_operations"].append(intent)
        self.save()
        response = self.client.request(method, path, body)
        key = "snapshot_id" if resource["kind"] == "snapshot" else "volume_id"
        resource["id"] = self.client.resource(response, key)
        resource["job_id"] = response.get("job_id")
        self.report["resources"].append(resource)
        self.report["uncertain_operations"].remove(intent)
        self.save()
        self.client.wait(response)
        resource["ready"] = True
        self.save()
        return resource["id"]

    def drill(self, plan):
        ordered, age = self.preflight(plan)
        self.begin(plan, "drill")
        self.report["recovery_point_age_seconds"] = age
        self.report["rpo_met"] = age <= plan.get("rpo_seconds", 3600)
        started, active = time.monotonic(), []
        try:
            require(self.report["rpo_met"], "snapshot age exceeds the RPO objective")
            for component in ordered:
                for index, volume in enumerate(component["volumes"]):
                    name = f"drill-{self.report['run_id'][:16]}-{component['id']}-{index}"
                    rid = self.mutation("POST", "/snapshots/" + identifier(volume["snapshot_id"]) + "/clone",
                                        {"name": name, "namespace": plan["target_namespace"]},
                                        {"kind": "volume", "name": name, "component_id": component["id"],
                                         "source_id": volume["volume_id"], "snapshot_id": volume["snapshot_id"]})
                    require(rid not in {v["volume_id"] for c in ordered for v in c["volumes"]},
                            "Atlas returned a source id for a clone")
                    for check in volume.get("checks", []):
                        data = self.client.request("GET", "/volumes/" + identifier(rid) + "/data?" +
                                                   urllib.parse.urlencode({"offset": check["offset"], "len": check["length"]}),
                                                   binary=True)
                        require(len(data) == check["length"], "short read during checksum verification")
                        require(hashlib.sha256(data).hexdigest() == check["sha256"], "restored data checksum mismatch")
                        self.report["events"].append({"component_id": component["id"], "phase": "checksum",
                                                      "offset": check["offset"], "length": len(data), "status": "succeeded"})
                # A failed start may have partially created workloads: always attempt its stop hook.
                active.append(component)
                self.hook(plan, component, "start")
                self.hook(plan, component, "verify")
            elapsed = time.monotonic() - started
            self.report["recovery_seconds"] = elapsed
            self.report["rto_met"] = elapsed <= plan.get("rto_seconds", 600)
            require(self.report["rto_met"], "application verification exceeded the RTO objective")
            self.report["status"] = "passed"
        except (Exception, KeyboardInterrupt) as exc:
            self.report["status"] = "failed"
            self.report["error"] = str(exc) if isinstance(exc, (ApiError, RecoveryError)) else "drill interrupted or failed"
        finally:
            self.report.setdefault("recovery_seconds", time.monotonic() - started)
            self.report.setdefault("rto_met", False)
            stopped = True
            for component in reversed(active):
                try:
                    self.hook(plan, component, "stop")
                except (Exception, KeyboardInterrupt):
                    stopped = False
                    self.report["cleanup_errors"].append(f"stop hook failed for {component['id']}; clones retained")
            if stopped:
                self.cleanup_clones()
            self.report["cleanup_complete"] = stopped and not self.report["cleanup_errors"] and not self.report["uncertain_operations"]
            if not self.report["cleanup_complete"]:
                self.report["status"] = "failed"
            if self.report["cleanup_errors"]:
                self.report["status"] = "failed"
            self.report["finished_at"] = utcnow().isoformat()
            self.save()
        return self.report

    def cleanup_clones(self):
        for resource in reversed(self.report["resources"]):
            if resource["kind"] != "volume":
                continue
            try:
                # A still-running clone job may create a volume after deletion: retain and report it.
                if resource.get("job_id"):
                    job = self.client.request("GET", "/jobs/" + identifier(resource["job_id"]))
                    require(job.get("state") in ("succeeded", "failed", "cancelled", "canceled"),
                            "clone job remains active")
                require(resource["id"] not in self.report["source_volume_ids"], "refusing to delete a source volume")
                try:
                    actual = self.client.request("GET", "/volumes/" + identifier(resource["id"]))
                except ApiError as exc:
                    if exc.status == 404:
                        resource["deleted"] = True
                        continue
                    raise
                require(actual.get("name") == resource["name"], "clone ownership check failed")
                response = self.client.request("DELETE", "/volumes/" + identifier(resource["id"]) + "?confirm=true")
                self.client.wait(response)
                resource["deleted"] = True
            except (Exception, KeyboardInterrupt):
                self.report["cleanup_errors"].append(f"clone cleanup incomplete for {resource['id']}")
            self.save()

    def capture(self, plan):
        ordered, _ = self.preflight(plan, capture=True)
        self.begin(plan, "capture")
        captured, frozen = copy.deepcopy(plan), []
        captured_by_id = {c["id"]: c for c in captured["components"]}
        deadline = time.monotonic() + plan.get("capture_timeout_seconds", 120)
        original_timeout, original_job_timeout = self.client.timeout, self.client.job_timeout
        original_deadline, self.client.deadline = self.client.deadline, deadline

        def budget():
            remaining = deadline - time.monotonic()
            require(remaining > 0, "capture deadline exceeded; resuming source applications")
            self.client.timeout = min(original_timeout, remaining)
            self.client.job_timeout = min(original_job_timeout, remaining)

        try:
            for component in reversed(ordered):
                frozen.append(component)
                budget()
                self.hook(plan, component, "quiesce", timeout=max(.001, deadline - time.monotonic()))
            for component in ordered:
                for index, volume in enumerate(component["volumes"]):
                    name = f"drill-{self.report['run_id'][:16]}-{component['id']}-{index}"
                    budget()
                    sid = self.mutation("POST", "/volumes/" + identifier(volume["volume_id"]) + "/snapshots",
                                        {"name": name}, {"kind": "snapshot", "component_id": component["id"],
                                                         "name": name, "source_id": volume["volume_id"]})
                    captured_by_id[component["id"]]["volumes"][index]["snapshot_id"] = sid
            self.report["status"] = "passed"
            self.report["captured_plan"] = captured
            self.report["consistency"] = "hook_coordinated" if all(c.get("quiesce_hook") for c in ordered) else "crash"
        except (Exception, KeyboardInterrupt) as exc:
            self.report["status"] = "failed"
            self.report["error"] = str(exc) if isinstance(exc, (ApiError, RecoveryError)) else "capture interrupted or failed"
        finally:
            self.client.timeout, self.client.job_timeout = original_timeout, original_job_timeout
            self.client.deadline = original_deadline
            for component in reversed(frozen):
                try:
                    self.hook(plan, component, "resume")
                except (Exception, KeyboardInterrupt):
                    self.report["status"] = "failed"
                    self.report["cleanup_errors"].append(f"resume hook failed for {component['id']}")
            if self.report["cleanup_errors"]:
                self.report["status"] = "failed"
            self.report["finished_at"] = utcnow().isoformat()
            self.save()
        return self.report
