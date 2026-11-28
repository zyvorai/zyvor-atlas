#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
"""Markdown comparison of run.sh result sets.

Usage: report.py <label>=<results/tag/system> [<label>=<dir> ...]

Each cell is the median over the repeats. A workload with any failed repeat is shown as FAILED, never
averaged over the survivors. Disk MiB figures come from /proc/diskstats for the whole device, so they
include anything else the shared host wrote in the same window.
"""

import json
import statistics
import sys
from pathlib import Path

SECTOR = 512
MIB = 1024 * 1024


def sides_of(job):
    """The fio direction(s) that carry this workload's I/O."""
    return [job[d] for d in ("read", "write") if job[d]["io_bytes"] > 0]


def parse(path):
    """One repeat's metrics, or None if fio reported a failure or did no I/O."""
    if path.with_suffix(".failed").exists():
        return None
    try:
        text = path.read_text()
        job = json.loads(text[text.index("{") :])["jobs"][0]
    except (OSError, ValueError, KeyError, IndexError):
        return None
    sides = sides_of(job)
    if job.get("error") or not sides:
        return None
    busiest = max(sides, key=lambda s: s["total_ios"])
    pct = busiest["clat_ns"].get("percentile", {})
    return {
        "iops": sum(s["iops"] for s in sides),
        "mibs": sum(s["bw_bytes"] for s in sides) / MIB,
        "p50": pct.get("50.000000", 0) / 1e6,
        "p99": pct.get("99.000000", 0) / 1e6,
    }


def disk(result_dir):
    """(workload, repeat) -> MiB written to the device while it ran, ramp included."""
    out = {}
    path = result_dir / "disk.txt"
    if not path.exists():
        return out
    for line in path.read_text().splitlines():
        f = line.split()
        if len(f) >= 7:
            out[(f[0], f[1])] = (int(f[6]) - int(f[4])) * SECTOR / MIB
    return out


def load(result_dir):
    workloads = {}
    for path in sorted(result_dir.glob("*.json")):
        name, rep, _ = path.name.rsplit(".", 2)
        workloads.setdefault(name, []).append((rep, parse(path)))
    written = disk(result_dir)
    summary = {}
    for name, reps in workloads.items():
        if any(m is None for _, m in reps):
            summary[name] = None
            continue
        m = {k: statistics.median(r[k] for _, r in reps) for k in ("iops", "mibs", "p50", "p99")}
        m["n"] = len(reps)
        m["spread"] = (
            (max(r["iops"] for _, r in reps) - min(r["iops"] for _, r in reps)) / m["iops"]
            if m["iops"]
            else 0
        )
        m["disk_mib"] = statistics.median(written.get((name, rep), 0) for rep, _ in reps)
        summary[name] = m
    return summary


def main(argv):
    if not argv or any("=" not in a for a in argv):
        print(__doc__, file=sys.stderr)
        return 2
    runs = [(label, Path(d)) for label, d in (a.split("=", 1) for a in argv)]
    for _, d in runs:
        if not d.is_dir():
            print(f"not a directory: {d}", file=sys.stderr)
            return 2
    data = [(label, load(d)) for label, d in runs]

    for label, d in runs:
        env = d / "env.txt"
        print(f"### {label}\n\n```\n{env.read_text().strip() if env.exists() else 'no env.txt'}\n```\n")

    names = list(dict.fromkeys(n for _, s in data for n in s))
    print("| workload | system | IOPS | MiB/s | p50 ms | p99 ms | disk MiB written | repeats | IOPS spread |")
    print("|---|---|---:|---:|---:|---:|---:|---:|---:|")
    for name in names:
        for label, summary in data:
            m = summary.get(name, "missing")
            if m == "missing":
                print(f"| {name} | {label} | not run | | | | | | |")
            elif m is None:
                print(f"| {name} | {label} | FAILED | | | | | | |")
            else:
                print(
                    f"| {name} | {label} | {m['iops']:.1f} | {m['mibs']:.2f} | {m['p50']:.2f} | "
                    f"{m['p99']:.2f} | {m['disk_mib']:.0f} | {m['n']} | {m['spread'] * 100:.0f}% |"
                )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
