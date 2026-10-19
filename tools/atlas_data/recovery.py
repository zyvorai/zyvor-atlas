# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Recovery evidence with trusted operator profiles; never accepts executable commands over HTTP."""
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import uuid

from .core import DataError, ident, number, require


def run_hook(argv, workspace, timeout):
    require(isinstance(argv, list) and argv and Path(argv[0]).is_absolute()
            and all(isinstance(x, str) and "\x00" not in x for x in argv), "invalid trusted hook")
    args = [x.replace("{workspace}", str(workspace)) for x in argv]
    # Profiles run without server tokens and do not inherit credential-bearing environment variables.
    env = {"PATH": os.defpath, "LANG": "C.UTF-8", "ATLAS_RECOVERY_ROOT": str(workspace)}
    with tempfile.TemporaryFile() as output:
        process = subprocess.Popen(args, cwd=workspace, env=env, stdout=output,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            raise DataError("recovery hook timed out", 408) from None
        finally:
            # Hooks are bounded tasks. No child processes survive a completed phase.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        require(code == 0, "recovery hook failed", 422)


class Recovery:
    def __init__(self, store, profiles=None):
        self.store = store
        self.profiles = profiles or {}

    def drill(self, a, artifact, profile, rto_seconds=60):
        a.admin()
        ident(profile)
        number(rto_seconds, 1, 600)
        require(profile in self.profiles, "unknown recovery profile", 404)
        hooks = self.profiles[profile]
        require(isinstance(hooks, dict) and "verify" in hooks, "profile needs an application verification hook")
        source = self.store.artifact(a, artifact)
        started = time.monotonic()
        report = {"id": uuid.uuid4().hex, "artifact": artifact, "profile": profile,
                  "started_at": self.store.clock(), "owner": a.subject, "status": "failed",
                  "checksum_verified": False, "application_verified": False}
        try:
            with tempfile.TemporaryDirectory(prefix="atlas-drill-") as root:
                workspace = Path(root) / "restore"
                self.store.materialize(a, artifact, workspace)
                report["checksum_verified"] = True
                remaining = rto_seconds - (time.monotonic() - started)
                require(remaining > 0, "restore exceeded RTO", 408)
                run_hook(hooks["verify"], workspace, remaining)
                report["application_verified"] = True
                report["status"] = "passed"
        except (DataError, OSError) as exc:
            report["error"] = str(exc) if isinstance(exc, DataError) else "recovery process failed"
        report["duration_seconds"] = time.monotonic() - started
        report["rto_met"] = report["duration_seconds"] <= rto_seconds
        if not report["rto_met"]:
            report["status"] = "failed"
        report["source_age_seconds"] = max(0, self.store.clock() - source["created_at"])
        # Snapshot age is NOT an application RPO measurement.
        with self.store.transaction():
            self.store._put(a, "recovery", report["id"], report, create=True)
            self.store._audit(a, "recovery.drill", report["id"])
        return report

    def latest(self, a, name):
        a.admin()
        with self.store.lock:
            ids = {v["id"] for v in self.store._list(a, "artifact") if v["name"] == name}
            reports = [r for r in self.store._list(a, "recovery") if r["artifact"] in ids
                       and r["status"] == "passed" and r["application_verified"]]
            if not reports:
                return {"artifact": None, "verified": False}
            result = max(reports, key=lambda r: self.store._get(a, "artifact", r["artifact"])["created_at"])
            return {"artifact": result["artifact"], "report": result, "verified": True}
