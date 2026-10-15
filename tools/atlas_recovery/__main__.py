# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Run with PYTHONPATH=tools python3 -m atlas_recovery --help."""
import argparse
import json
import os
import signal
from pathlib import Path
import sys
import uuid
from datetime import datetime, timezone

from .core import ApiError, AtlasClient, Recovery, RecoveryError, atomic_json, validate


def main(argv=None):
    parser = argparse.ArgumentParser(description="Atlas application recovery capture and restore drills")
    parser.add_argument("command", choices=("validate", "preflight", "capture", "drill"))
    parser.add_argument("--plan", required=True, type=Path)
    parser.add_argument("--hooks", type=Path, help="trusted operator hook registry JSON")
    parser.add_argument("--base-url", default="https://localhost:5110/api/atlas/v1")
    parser.add_argument("--ca-file")
    reports = parser.add_mutually_exclusive_group()
    reports.add_argument("--report", type=Path)
    reports.add_argument("--report-dir", type=Path, help="generate a unique report filename for scheduled drills")
    parser.add_argument("--captured-plan", type=Path, help="new output plan for capture")
    parser.add_argument("--job-timeout", type=float, default=300)
    args = parser.parse_args(argv)
    if hasattr(signal, "SIGTERM"):
        def interrupted(signum, frame):
            raise KeyboardInterrupt
        signal.signal(signal.SIGTERM, interrupted)
    try:
        plan = json.loads(args.plan.read_text())
        hooks = json.loads(args.hooks.read_text()) if args.hooks else {}
        order = validate(plan, hooks, capture=args.command == "capture")
        if args.command == "validate":
            print(json.dumps({"valid": True, "recovery_order": [c["id"] for c in order]}))
            return 0
        client = AtlasClient(args.base_url, os.environ.get("ATLAS_TOKEN"), args.ca_file,
                             job_timeout=args.job_timeout)
        if args.command == "preflight":
            _, age = Recovery(client, hooks, "unused").preflight(plan)
            print(json.dumps({"ready": age <= plan.get("rpo_seconds", 3600), "recovery_point_age_seconds": age}))
            return 0 if age <= plan.get("rpo_seconds", 3600) else 1
        if args.report_dir:
            stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
            args.report = args.report_dir / f"{plan['name']}-{stamp}-{uuid.uuid4().hex[:12]}.json"
        if not args.report:
            parser.error("--report is required for capture and drill")
        if args.command == "capture":
            if not args.captured_plan:
                parser.error("--captured-plan is required for capture")
            if args.captured_plan.exists() or args.captured_plan.resolve() in (args.plan.resolve(), args.report.resolve()):
                raise RecoveryError("captured plan must be a new, distinct output path")
        recovery = Recovery(client, hooks, args.report)
        report = recovery.capture(plan) if args.command == "capture" else recovery.drill(plan)
        if args.command == "capture" and report["status"] == "passed":
            atomic_json(args.captured_plan, report["captured_plan"], exclusive=True)
        print(json.dumps({"status": report["status"], "report": str(args.report)}))
        return 0 if report["status"] == "passed" else 1
    except (ApiError, RecoveryError) as exc:
        print(f"Atlas recovery failed: {exc}", file=sys.stderr)
        return 2
    except (OSError, ValueError, TypeError, KeyError):
        # Reports carry operational details; never dump input files or credentials on errors.
        print("Atlas recovery failed: invalid input, unavailable API, or failed preflight; inspect the report if created.", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
