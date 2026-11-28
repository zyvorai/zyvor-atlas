#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
"""Shared logic of the agent profile: the plan (what a run must produce), parsers, tree checks and
statistics. Nothing here talks to a storage system. Runs under Python 3.9 (the Ceph image) and newer.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
import re
import statistics
import sys
import tarfile
import time

SYSTEMS = ("ceph", "atlas", "floor")
ALL_PHASES = ("A", "B", "C", "D", "E", "F", "G", "S")
FORK_MODES = ("quiet", "busy")
MIN_REPEATS = 3

# Item states. Only OK counts towards a result.
OK = "ok"
FAILED = "failed"  # a check on data or on the result did not hold
ERROR = "error"  # a step could not be carried out
UNSUPPORTED = "unsupported"  # the comparison cannot be made to work on this system
INCOMPLETE = "incomplete"  # ran, but what was to be measured is not established
STATES = (OK, FAILED, ERROR, UNSUPPORTED, INCOMPLETE)

EVENT_PREFIX = "@@AGENT "


class VerifyError(Exception):
    """Data or a result differs from what it must be."""


class Unsupported(Exception):
    """This comparison cannot be made to work here."""


class Incomplete(Exception):
    """Ran, but the measurement is not established."""


# ---------------------------------------------------------------- the plan


def case_system(case):
    """The system of a case name: ceph, floor, atlas or atlas-<variant>."""
    if case in ("ceph", "atlas", "floor"):
        return case
    if re.fullmatch(r"atlas-[A-Za-z0-9_]+", case):
        return "atlas"
    raise ValueError(f"unknown case: {case}")


def methods(system, plan=None):
    """The ways of forking a tree that `system` is measured with, restricted to plan["methods"].

    clone            a writable clone made by the system itself, on its own mount
    overlay          a read-only snapshot on its own mount, plus a local overlayfs upper layer
    overlay-shared   the same with one lower layer for every view: on CephFS the snapshot directory
                     of the mount that is already there (its cheapest fork), on atlas-native one
                     snapshot mount per snapshot
    reflink          floor: cp -a --reflink=always on the host's XFS
    """
    every = ["reflink"] if system == "floor" else ["clone", "overlay", "overlay-shared"]
    wanted = (plan or {}).get("methods")
    return [m for m in every if system == "floor" or not wanted or m in wanted]


def expected_items(plan, system, sizes=None):
    """The item ids a run of `system` must report under `plan`, in the order they run. `sizes`
    restricts phase D to those tree sizes (one cluster per size)."""
    phases = plan["phases"]
    items = []
    if system == "floor":
        phases = [p for p in phases if p in ("D", "E", "F")]
    if "A" in phases:
        items.append("A")
        if "C" in phases:
            items.append("C/after-kill")
    if "B" in phases:
        items.append("B")
    if "C" in phases:
        items.append("C/final")
    if "E" in phases:
        items += [f"E/{k}/{m}" for m in methods(system, plan) for k in plan["fanout"]]
    if "F" in phases:
        items.append("F")
    if "G" in phases:
        items.append("G")
    if "D" in phases:
        modes = plan["fork_modes"] if system != "floor" else ["quiet"]
        for n in plan["fork_sizes"] if sizes is None else sizes:
            items += [f"D/{n}/{mode}/{m}" for mode in modes for m in methods(system, plan)]
    if "S" in phases:
        items += [f"S/w{w}" for w in plan["writers"]]
    return items


def segments(plan, system):
    """(segment name, phases, sizes) per cluster start: everything but D shares one cluster, and D
    gets a fresh one for each tree size so that memory and space figures start clean."""
    out = []
    others = [p for p in plan["phases"] if p != "D"]
    if system == "floor":
        others = [p for p in others if p in ("E", "F")]
    if others:
        out.append(("main", others, []))
    if "D" in plan["phases"]:
        out += [(f"d{n}", ["D"], [n]) for n in plan["fork_sizes"]]
    return out


def segment_items(plan, system, phases, sizes):
    return expected_items(dict(plan, phases=phases), system, sizes or None)


def check_plan(plan):
    """Problems that make a plan unusable."""
    problems = []
    unknown = [p for p in plan.get("phases", []) if p not in ALL_PHASES]
    if not plan.get("phases") or unknown:
        problems.append(f"phases must be some of {' '.join(ALL_PHASES)}, got {plan.get('phases')}")
        return problems
    phases = plan["phases"]
    if "B" in phases and "A" not in phases:
        problems.append("phase B works in phase A's workspace: add A")
    if "C" in phases and not {"A", "B"} <= set(phases):
        problems.append("phase C checks what A and B wrote: add A and B")
    if "F" in phases and "E" not in phases:
        problems.append("phase F discards phase E's clones: add E")
    for key, phase in (("fork_sizes", "D"), ("fanout", "E"), ("writers", "S")):
        values = plan.get(key)
        if phase in phases and (not values or any(not isinstance(v, int) or v < 1 for v in values)):
            problems.append(f"{key} must be positive integers")
        elif values and len(set(values)) != len(values):
            problems.append(f"{key} lists a value twice")
    if "D" in phases and any(m not in FORK_MODES for m in plan.get("fork_modes", [])):
        problems.append(f"fork_modes must be some of {' '.join(FORK_MODES)}")
    if "D" in phases and not plan.get("fork_modes"):
        problems.append("fork_modes is empty")
    wrong = [m for m in plan.get("methods") or [] if m not in ("clone", "overlay", "overlay-shared")]
    if wrong:
        problems.append(f"unknown fork methods: {wrong}")
    return problems


def order_items(items, round_):
    """Even rounds take the fork methods of a step, and the writer counts, in the reverse order, so
    that no variant always runs first on a fresh cluster."""
    if round_ % 2:
        return list(items)

    def step(item):
        parts = item.split("/")
        if parts[0] == "D":
            return ("D", parts[1], parts[2])
        if parts[0] in ("E", "S"):
            return (parts[0],)
        return (item,)

    out, group = [], []
    for item in items:
        if group and step(item) != step(group[0]):
            out += reorder(group)
            group = []
        group.append(item)
    return out + reorder(group)


def reorder(group):
    if group and group[0].startswith("E/"):
        # Methods in reverse, fan-out sizes still ascending.
        seen = list(dict.fromkeys(i.split("/")[2] for i in group))
        return [i for m in reversed(seen) for i in group if i.split("/")[2] == m]
    return group[::-1]


PROFILES = {
    # profile: (phases, cases, tmpfs GiB for the server data; 0 = the benchmark disk)
    "agent": ("A B C E F G", "ceph atlas floor", 0),
    "fork": ("D", "ceph atlas floor", 10),
    "smallfile": ("S", "ceph atlas", 0),
}
SETTINGS = {
    # setting: (environment variable, default)
    "small_files": ("AGENT_SMALL_FILES", 2000),
    "edits": ("AGENT_EDITS", 20),
    "large_files": ("AGENT_LARGE_FILES", 4),
    "large_mib": ("AGENT_LARGE_MIB", 256),
    "settle_s": ("AGENT_SETTLE_SECS", 10),
    "bg_base_s": ("AGENT_BG_BASE_SECS", 10),
    "bg_after_s": ("AGENT_BG_AFTER_SECS", 10),
    "stable_quiet_s": ("AGENT_STABLE_SECS", 90),
    "stable_timeout_s": ("AGENT_STABLE_TIMEOUT", 900),
    "clone_timeout_s": ("AGENT_CLONE_TIMEOUT", 3600),
    "command_timeout_s": ("AGENT_COMMAND_TIMEOUT", 7200),
    "build_threads": ("AGENT_BUILD_THREADS", 16),
    "ceph_max_clones": ("AGENT_CEPH_MAX_CLONES", 0),
}
VARIANT_KEY = re.compile(r"ATLAS_[A-Z0-9_]+|BENCH_ATLAS_SRC")


def whole(environ, name, default, least=0):
    text = environ.get(name) or str(default)
    if not re.fullmatch(r"[0-9]+", text) or int(text) < least:
        raise ValueError(f"{name} must be a whole number of at least {least}, got {text!r}")
    return int(text)


def words(environ, name, default):
    return (environ.get(name) or default).replace(",", " ").split()


def numbers(environ, name, default):
    values = words(environ, name, default)
    if any(not re.fullmatch(r"[1-9][0-9]*", v) for v in values):
        raise ValueError(f"{name} must be positive whole numbers, got {values}")
    return [int(v) for v in values]


def parse_variant(text):
    """The settings of an atlas variant: KEY=VALUE lines, ATLAS_* or BENCH_ATLAS_SRC only."""
    out = {}
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        key, sep, value = line.partition("=")
        if not sep or not VARIANT_KEY.fullmatch(key):
            raise ValueError(f"not an ATLAS_* or BENCH_ATLAS_SRC setting: {line!r}")
        if key in out:
            raise ValueError(f"{key} is set twice")
        out[key] = value
    return out


def build_plan(profile, tag, environ):
    if profile not in PROFILES:
        raise ValueError(f"profile must be one of {' '.join(PROFILES)}")
    if not re.fullmatch(r"[A-Za-z0-9_-]+", tag):
        raise ValueError("tag must be [A-Za-z0-9_-]+")
    phases, cases, tmpfs = PROFILES[profile]
    phases = words(environ, "AGENT_PHASES", phases)
    cases = words(environ, "CASES", cases)
    tmpfs = whole(environ, "AGENT_TMPFS_GB", tmpfs)
    if len(set(cases)) != len(cases):
        raise ValueError("a case is listed twice")
    plan = {
        "profile": profile,
        "tag": tag,
        "rounds": whole(environ, "ROUNDS", MIN_REPEATS, 1),
        "phases": [p for p in ALL_PHASES if p in phases] + [p for p in phases if p not in ALL_PHASES],
        "fork_sizes": numbers(environ, "AGENT_FORK_SIZES", "1000 10000 100000"),
        "fork_modes": words(environ, "AGENT_FORK_MODES", "quiet busy"),
        "fanout": numbers(environ, "AGENT_FANOUT", "1 4 16"),
        "writers": numbers(environ, "AGENT_WRITERS", "1 8 32"),
        "methods": words(environ, "AGENT_METHODS", "clone overlay overlay-shared"),
        "tmpfs_gb": tmpfs,
        "mem": environ.get("BENCH_MEM") or ("20g" if tmpfs else "12g"),
        "fixtures": {
            "seed": whole(environ, "AGENT_SEED", 1),
            "files": whole(environ, "AGENT_FILES", 2000, 1),
            "repo_files": whole(environ, "AGENT_REPO_FILES", 1000, 1),
            "commits": whole(environ, "AGENT_COMMITS", 20, 1),
            "fork_file_bytes": whole(environ, "AGENT_FORK_FILE_BYTES", 1024, 1),
        },
        "settings": {k: whole(environ, name, default) for k, (name, default) in SETTINGS.items()},
    }
    plan["fixtures"]["fork_sizes"] = plan["fork_sizes"]
    problems = check_plan(plan)
    if problems:
        raise ValueError("; ".join(problems))
    plan["case_env"], kept = {}, []
    for case in cases:
        system = case_system(case)
        if system == "floor" and not segments(plan, system):
            continue
        kept.append(case)
        env = {}
        if system == "atlas" and "D" in plan["phases"]:
            # A clone of a large tree may take longer than the 30 s a proposal normally waits.
            env["ATLAS_PROPOSAL_TIMEOUT_MS"] = environ.get("ATLAS_PROPOSAL_TIMEOUT_MS") or "120000"
        if case.startswith("atlas-"):
            directory = environ.get("AGENT_VARIANTS")
            if not directory:
                raise ValueError(f"case {case} needs AGENT_VARIANTS=<dir holding {case[6:]}.env>")
            with open(os.path.join(directory, case[6:] + ".env")) as f:
                env.update(parse_variant(f.read()))
        plan["case_env"][case] = env
    if not kept:
        raise ValueError("no case has anything to run")
    plan["cases"] = kept
    plan["runs"] = [
        {"round": r, "case": c, "system": case_system(c), "dir": f"{tag}-r{r}-{c}"}
        for r in range(1, plan["rounds"] + 1)
        for c in kept
    ]
    return plan


def fixtures_name(params):
    sizes = "-".join(str(n) for n in params["fork_sizes"])
    return (
        f"agent-s{params['seed']}-f{params['files']}-r{params['repo_files']}-c{params['commits']}"
        f"-b{params['fork_file_bytes']}-n{sizes}"
    )


def fixture_args(params):
    out = []
    for key, value in sorted(params.items()):
        text = ",".join(str(v) for v in value) if isinstance(value, list) else str(value)
        out += ["--" + key.replace("_", "-"), text]
    return out


def runner_config(plan, system, round_, phases, sizes, fixtures, disk, floor_root):
    return {
        "system": system,
        "plan": plan,
        "round": round_,
        "phases": phases,
        "sizes": sizes,
        "fixtures": fixtures,
        "disk": disk,
        "floor_root": floor_root,
        "settings": plan["settings"],
    }


# What an ok item must report. A missing or non-finite figure counts as missing.
def required_metrics(item, system):
    parts = item.split("/")
    phase = parts[0]
    if phase == "A":
        return ["tool_exit_s", "durable_s"]
    if phase == "B":
        return ["clone_s", "status_s", "edit_s", "commit_s", "fsck_s"]
    if phase == "C":
        return ["verify_s", "files"] + (["remount_s"] if parts[1] == "after-kill" else [])
    if phase == "D":
        mode, method = parts[2], parts[3]
        need = ["fork_s", "first_read_s", "total_s"]
        if method == "reflink":
            return need + ["sync_s"]
        need += ["snapshot_s", "mount_s", "rss_before_bytes", "rss_growth_bytes", "space_growth_bytes"]
        if method == "clone":
            need.append("clone_s")
        if system == "atlas":
            need.append("elections")
        if mode == "busy":
            need += ["bg_base_p99_ms", "bg_fork_p99_ms", "bg_fork_ops", "bg_base_ops"]
        return need
    if phase == "E":
        need = [
            "fork_wall_s", "fork_slowest_s", "cold_wall_s", "cold_slowest_s", "cold_p99_ms",
            "warm_wall_s", "warm_slowest_s", "warm_p99_ms", "edit_wall_s", "edit_p99_ms",
        ]
        return need + ["space_growth_bytes"]
    if phase == "F":
        return ["delete_s", "stable_s", "reclaimed_bytes"]
    if phase == "G":
        return ["write_mibs", "read_mibs"]
    if phase == "S":
        need = ["files_per_s", "p99_ms", "disk_flushes"]
        if system == "atlas":
            need += ["raft_syncs_per_file", "data_syncs_per_file"]
        return need
    raise ValueError(f"unknown item: {item}")


def finite(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def item_state(record, item, system):
    """(state, reason) of a reported item: an ok item that lacks a required figure is incomplete."""
    if record is None:
        return "missing", "not reported"
    status = record.get("status")
    if status not in STATES:
        return INCOMPLETE, f"unknown status {status!r}"
    if status != OK:
        return status, str(record.get("detail", ""))
    metrics = record.get("metrics") or {}
    lacking = [m for m in required_metrics(item, system) if not finite(metrics.get(m))]
    if lacking:
        return INCOMPLETE, "figures missing: " + " ".join(lacking)
    return OK, ""


# ---------------------------------------------------------------- statistics


def percentile(values, q):
    """Nearest-rank percentile: always one of the values. None for no values."""
    if not values:
        return None
    ordered = sorted(values)
    rank = max(1, math.ceil(q / 100.0 * len(ordered)))
    return ordered[rank - 1]


def median_range(values):
    return statistics.median(values), min(values), max(values)


def overlapping(ops, start, end):
    """Latencies of the (start, end) operations that were in progress at some point of [start, end]."""
    return [b - a for a, b in ops if b >= start and a <= end]


def wait_stable(sample, same, quiet_s, timeout_s, interval_s, min_s=0.0, clock=time.monotonic, sleep=time.sleep):
    """Samples until `same(a, b)` has held for `quiet_s` seconds and `min_s` has passed.

    Returns (last sample, seconds until it last changed, stable). Not stable after `timeout_s`.
    """
    start = clock()
    value = sample()
    changed_at = 0.0
    while True:
        elapsed = clock() - start
        if elapsed >= min_s and elapsed - changed_at >= quiet_s:
            return value, changed_at, True
        if elapsed >= timeout_s:
            return value, changed_at, False
        sleep(interval_s)
        new = sample()
        if not same(value, new):
            changed_at = clock() - start
            value = new


# ---------------------------------------------------------------- parsers


def parse_loadavg(text):
    fields = text.split()
    return [float(fields[0]), float(fields[1]), float(fields[2])]


def parse_pressure(text):
    """/proc/pressure/*: {"some": {"avg10": .., "avg60": .., "avg300": .., "total": ..}, "full": ..}."""
    out = {}
    for line in text.splitlines():
        fields = line.split()
        if not fields:
            continue
        out[fields[0]] = {k: float(v) for k, v in (f.split("=", 1) for f in fields[1:])}
    if "some" not in out:
        raise ValueError("no 'some' line in pressure data")
    return out


def parse_diskstats(text, device):
    """Counters of one block device: reads, writes, sectors and flush requests."""
    for line in text.splitlines():
        f = line.split()
        if len(f) >= 14 and f[2] == device:
            return {
                "reads": int(f[3]),
                "sectors_read": int(f[5]),
                "writes": int(f[7]),
                "sectors_written": int(f[9]),
                "flushes": int(f[18]) if len(f) > 18 else None,
            }
    return None


def parse_status_kb(text, key):
    """A kB figure such as VmRSS from /proc/<pid>/status, in bytes."""
    m = re.search(rf"^{key}:\s+(\d+)\s+kB", text, re.M)
    return int(m[1]) * 1024 if m else None


METRIC_LINE = re.compile(r"^([A-Za-z_:][A-Za-z0-9_:]*)(?:\{(.*)\})?\s+(\S+)$")


def parse_metrics(text):
    """[(name, labels string, value)] of a Prometheus exposition."""
    out = []
    for line in text.splitlines():
        if line.startswith("#"):
            continue
        m = METRIC_LINE.match(line.strip())
        if not m:
            continue
        try:
            out.append((m[1], m[2] or "", float(m[3])))
        except ValueError:
            continue
    return out


def metric_sum(parsed, name):
    """Sum over all label sets, or None if the metric is not there at all."""
    values = [v for n, _, v in parsed if n == name]
    return sum(values) if values else None


def sync_delta(before, after, name):
    """(syncs, seconds) of the histogram `name` between two scrapes of one node, or None if either
    lacks it or it went backwards (a restart)."""
    out = []
    for suffix in ("_count", "_sum"):
        a, b = metric_sum(before, name + suffix), metric_sum(after, name + suffix)
        if a is None or b is None or b < a:
            return None
        out.append(b - a)
    return tuple(out)


def parse_mount_args(settings_text):
    """The tuning arguments atlas-entry.sh mounted with, from settings.txt."""
    for line in settings_text.splitlines():
        if line.startswith("mount_args="):
            return line[len("mount_args=") :].split()
    raise ValueError("settings.txt has no mount_args line")


def setting_extent_bytes(settings_text):
    m = re.search(r"^extent_bytes=(\d+)", settings_text, re.M)
    if not m:
        raise ValueError("settings.txt has no extent_bytes line")
    return int(m[1])


def is_leader(status):
    """Whether a /v1/status document says this node leads a metadata group."""
    groups = status.get("metadata_groups") or [status.get("metadata") or {}]
    return any(g.get("role") == "leader" for g in groups)


def clone_state(doc):
    """The state in the output of `ceph fs clone status`."""
    state = doc.get("status", {}).get("state")
    if state not in ("pending", "in-progress", "complete", "failed", "canceled"):
        raise ValueError(f"unknown clone state: {json.dumps(doc)[:300]}")
    return state


def parse_ceph_df(doc):
    """(bytes used by all pools, replicas included; of which the metadata pool)."""
    pools = {p["name"]: p["stats"]["bytes_used"] for p in doc["pools"]}
    return sum(pools.values()), pools["cephfs_metadata"]


def parse_fio_job(text):
    """The single job of fio's JSON output (warnings may precede it); VerifyError unless it is clean."""
    try:
        jobs = json.loads(text[text.index("{") :])["jobs"]
        if len(jobs) != 1:
            raise VerifyError(f"{len(jobs)} fio job entries, expected 1")
        if jobs[0]["error"] != 0:
            raise VerifyError(f"fio error {jobs[0]['error']}")
        return jobs[0]
    except (ValueError, KeyError, IndexError, TypeError) as e:
        raise VerifyError(f"fio output cannot be read: {e!r}") from None


def smallfile_figures(job, files):
    """Rate and tail of the small-file job; VerifyError unless it wrote exactly `files` 4 KiB files."""
    try:
        w = job["write"]
        if w["total_ios"] != files or w["io_bytes"] != files * 4096:
            raise VerifyError(f"fio wrote {w['total_ios']} files, {w['io_bytes']} bytes; expected {files} of 4 KiB")
        if w["iops"] <= 0:
            raise VerifyError("fio reports no rate")
        return {"files_per_s": float(w["iops"]), "p99_ms": w["clat_ns"]["percentile"]["99.000000"] / 1e6}
    except (KeyError, TypeError) as e:
        raise VerifyError(f"fio field missing: {e!r}") from None


# ---------------------------------------------------------------- trees


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def sha256_file(path, chunk=1 << 20):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while True:
            block = f.read(chunk)
            if not block:
                return h.hexdigest()
            h.update(block)


def parse_manifest(text):
    """{path: sha256} from sha256sum lines; empty or malformed manifests are refused."""
    out = {}
    for line in text.splitlines():
        m = re.fullmatch(r"([0-9a-f]{64})  (.+)", line)
        if not m:
            raise ValueError(f"bad manifest line: {line[:100]!r}")
        if m[2] in out:
            raise ValueError(f"manifest lists {m[2]} twice")
        out[m[2]] = m[1]
    if not out:
        raise ValueError("empty manifest")
    return out


def format_manifest(files):
    return "".join(f"{files[p]}  {p}\n" for p in sorted(files))


def walk_files(root, skip=()):
    """Relative paths of everything that is not a directory under `root`, sorted. `skip` names
    top-level directories to leave out. Errors are raised, never swallowed."""
    out = []

    def fail(e):
        raise e

    for base, dirs, files in os.walk(root, onerror=fail):
        rel = os.path.relpath(base, root)
        if rel == ".":
            dirs[:] = [d for d in dirs if d not in skip]
        links = [d for d in dirs if os.path.islink(os.path.join(base, d))]
        for name in files + links:
            out.append(name if rel == "." else f"{rel}/{name}")
    return sorted(out)


def read_tree(root, expected, skip=()):
    """Reads every file of the tree and checks it against `expected` ({path: sha256}).

    Returns per-file read latencies in seconds; VerifyError on a missing, extra or different file.
    """
    found = walk_files(root, skip)
    missing = sorted(set(expected) - set(found))
    extra = sorted(set(found) - set(expected))
    if missing or extra:
        raise VerifyError(f"{root}: {len(missing)} files missing {missing[:3]}, {len(extra)} extra {extra[:3]}")
    latencies, wrong = [], []
    for path in found:
        t0 = time.monotonic()
        digest = sha256_file(os.path.join(root, path))
        latencies.append(time.monotonic() - t0)
        if digest != expected[path]:
            wrong.append(path)
    if wrong:
        raise VerifyError(f"{root}: {len(wrong)} files differ {wrong[:3]}")
    return latencies


def hash_tree(root, skip=()):
    return {p: sha256_file(os.path.join(root, p)) for p in walk_files(root, skip)}


def edit_targets(paths, count):
    """The files the fan-out edits: the first `count` in byte order, outside git's own directories."""
    usable = sorted((p for p in paths if ".git" not in p.split("/")), key=lambda p: p.encode())
    if len(usable) < count:
        raise ValueError(f"only {len(usable)} files to edit, {count} wanted")
    return usable[:count]


def edit_suffix(label):
    return f"edit {label}\n".encode()


def edited_manifest(source, originals, label):
    """The manifest a view labelled `label` must have after appending to each file in `originals`."""
    out = dict(source)
    for path, data in originals.items():
        out[path] = sha256_bytes(data + edit_suffix(label))
    return out


def tar_members(tar_path):
    """(directories, [(path, bytes)]) of a fixture tar."""
    dirs, files = [], []
    with tarfile.open(tar_path) as tar:
        for m in tar:
            if m.name.startswith("/") or ".." in m.name.split("/"):
                raise ValueError(f"unsafe path in {tar_path}: {m.name}")
            if m.isdir():
                dirs.append(m.name)
            elif m.isfile():
                files.append((m.name, tar.extractfile(m).read()))
            else:
                raise ValueError(f"unexpected member in {tar_path}: {m.name}")
    return dirs, files


# ---------------------------------------------------------------- events


def format_event(event):
    return EVENT_PREFIX + json.dumps(event, sort_keys=True)


def parse_events(text):
    """The events a runner printed. A truncated or unreadable line is skipped: its item is then
    missing, which is never a pass."""
    out = []
    for line in text.splitlines():
        if line.startswith(EVENT_PREFIX):
            try:
                event = json.loads(line[len(EVENT_PREFIX) :])
            except ValueError:
                continue
            if isinstance(event, dict):
                out.append(event)
    return out


def cli(argv):
    """What agent.sh asks about a plan.

    plan <profile> <tag> <out.json>     from the environment
    runs <plan.json>                    "round case system dir" per line, in run order
    segments <plan.json> <system>       "name phases sizes" per line ("-" for no sizes)
    env <plan.json> <case>              the case's own settings, KEY=VALUE per line
    get <plan.json> tmpfs_gb|mem        one value
    fixtures <plan.json>                directory name, then the generator's arguments
    config <plan.json> <system> <round> <phases> <sizes> <fixtures> <disk> <floor-root>
    """
    command, args = argv[0], argv[1:]
    if command == "plan":
        plan = build_plan(args[0], args[1], os.environ)
        with open(args[2], "w") as f:
            json.dump(plan, f, indent=1, sort_keys=True)
        return 0
    with open(args[0]) as f:
        plan = json.load(f)
    if command == "runs":
        for r in plan["runs"]:
            print(r["round"], r["case"], r["system"], r["dir"])
    elif command == "segments":
        for name, phases, sizes in segments(plan, args[1]):
            print(name, ",".join(phases), ",".join(str(n) for n in sizes) or "-")
    elif command == "env":
        for key, value in sorted(plan["case_env"][args[1]].items()):
            print(f"{key}={value}")
    elif command == "get":
        print(plan[args[1]])
    elif command == "fixtures":
        print(fixtures_name(plan["fixtures"]))
        for a in fixture_args(plan["fixtures"]):
            print(a)
    elif command == "config":
        system, round_, phases, sizes, fixtures, disk, floor_root = args[1:8]
        sizes = [] if sizes == "-" else [int(n) for n in sizes.split(",")]
        json.dump(
            runner_config(plan, system, int(round_), phases.split(","), sizes, fixtures, disk, floor_root),
            sys.stdout,
        )
    else:
        print(cli.__doc__, file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    try:
        sys.exit(cli(sys.argv[1:]))
    except (ValueError, OSError, KeyError, IndexError) as e:
        print(f"agent plan: {e}", file=sys.stderr)
        sys.exit(2)
