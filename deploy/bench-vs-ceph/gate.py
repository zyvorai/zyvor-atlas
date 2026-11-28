#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
"""Verdict on the delta-write target of docs/NATIVE_DELTA_REDESIGN.md.

Usage: gate.py --ceph <results/tag/ceph> [...] --atlas <results/tag/atlas> [...]

Inputs are run.sh result sets of long runs (STEADY_SECS=1200), as compare.sh with ROUNDS=3 leaves
them. The verdict is PASS only if all of this holds:

  session   at least 3 rounds of each system, as many of one as of the other, from one compare.sh
            session, alternating in time, with the same host settings; one atlas commit, node binary
            and settings
  iops      median atlas write IOPS of the long run >= median Ceph
  p99       median atlas p99 write latency <= median Ceph
  pending   in every atlas round, pending deltas not growing over the last 600 s of the load
  seqread   median atlas 1 MiB sequential read afterwards >= half of median Ceph
  verify    every atlas round passed fio's crc32c write and read-back

Anything missing, failed or non-standard makes the verdict FAIL or UNKNOWN, never PASS. Exit status:
0 PASS, 1 FAIL, 3 UNKNOWN, 2 usage.

Metrics (metrics-series.txt). atlas_native_delta_pending is replicated catalog state, reported by
each metadata node: it is the median of the nodes that answered, never their sum. *_total counters
count what one node did, so they are summed over nodes. "Growing" means that either the median of the
last three samples less that of the first three, or the least-squares trend over the window, exceeds
1% of the delta writes taken in the window. A backlog within 10% of its configured limit fails too:
there writes stop being deltas, so a flat count does not show that folding keeps up.
"""

import datetime
import json
import re
import statistics
import sys
from pathlib import Path

PASS, FAIL, UNKNOWN = "PASS", "FAIL", "UNKNOWN"
EXIT = {PASS: 0, FAIL: 1, UNKNOWN: 3}

MIN_ROUNDS = 3
STEADY_SECS = 1200
WINDOW = 600
EDGE = 90  # how far from either end of the window the outermost samples may be
MAX_GAP = 150
MIN_SAMPLES = 8
GROWTH_TOLERANCE = 0.01  # of the delta writes taken in the window
AT_LIMIT = 0.9
# NativeDeltaConfig defaults, used when settings.txt does not set them.
DEFAULT_LIMITS = {
    "max_pending_deltas": 65536,
    "max_pending_bytes": 2 << 30,
    "fold_headroom_bytes": 512 << 20,
}
META_PORTS = ("7401", "7402", "7403")
STEADY = "steady-randwrite-4k-qd32"
SEQREAD = "seqread-1m-after"
SAME_ENV = ("size_mb", "ramp", "runtime", "repeats", "files", "mem", "cpus", "disk", "kernel")
MIB = 1024 * 1024


class Unusable(Exception):
    """An input that cannot be used, with the reason."""


def read_env(path):
    env = {}
    try:
        lines = path.read_text().splitlines()
    except OSError:
        return env
    for line in lines:
        key, sep, value = line.partition("=")
        if not sep:
            continue
        if key in ("system", "mem"):  # several key=value pairs on one line
            env.update(t.split("=", 1) for t in line.split() if "=" in t)
        else:
            env[key] = value.strip()
    return env


def fio_job(directory, name, rep=1):
    """The single job of one fio run, or Unusable."""
    stem = f"{name}.{rep}"
    if (directory / f"{stem}.failed").exists():
        raise Unusable(f"{stem}: fio failed")
    try:
        text = (directory / f"{stem}.json").read_text()
        jobs = json.loads(text[text.index("{") :])["jobs"]
        if len(jobs) != 1:
            raise Unusable(f"{stem}: {len(jobs)} job entries, expected 1")
        job = jobs[0]
        if job["error"] != 0:
            raise Unusable(f"{stem}: fio error {job['error']}")
        return job
    except OSError:
        raise Unusable(f"{stem}.json is missing") from None
    except (ValueError, KeyError, IndexError, TypeError) as e:
        raise Unusable(f"{stem}.json cannot be read: {e!r}") from None


def expect_options(job, stem, wanted):
    options = job.get("job options", {})
    wrong = {k: options.get(k) for k, v in wanted.items() if str(options.get(k)) != v}
    if wrong:
        raise Unusable(f"{stem}: not the expected workload: {wrong}")


def steady(directory):
    job = fio_job(directory, STEADY)
    expect_options(job, STEADY, {"rw": "randwrite", "bs": "4k", "sync": "1", "numjobs": "32", "direct": "1"})
    try:
        w = job["write"]
        if w["total_ios"] <= 0 or w["iops"] <= 0:
            raise Unusable(f"{STEADY}: no writes")
        if w["runtime"] < STEADY_SECS * 1000 * 0.995:
            raise Unusable(f"{STEADY}: ran {w['runtime'] / 1000:.0f} s, the target is {STEADY_SECS} s")
        return {"iops": float(w["iops"]), "p99": w["clat_ns"]["percentile"]["99.000000"] / 1e6}
    except (KeyError, TypeError) as e:
        raise Unusable(f"{STEADY}: field missing: {e!r}") from None


def seqread(directory, env):
    try:
        repeats = int(env.get("repeats", "1"))
    except ValueError:
        raise Unusable("env.txt: repeats is not a number") from None
    rates = []
    for rep in range(1, max(repeats, 1) + 1):
        job = fio_job(directory, SEQREAD, rep)
        expect_options(job, SEQREAD, {"rw": "read", "bs": "1m"})
        try:
            if job["read"]["total_ios"] <= 0 or job["read"]["bw_bytes"] <= 0:
                raise Unusable(f"{SEQREAD}: nothing was read")
            rates.append(job["read"]["bw_bytes"] / MIB)
        except (KeyError, TypeError) as e:
            raise Unusable(f"{SEQREAD}: field missing: {e!r}") from None
    return statistics.median(rates)


def verify(directory):
    """(status, detail) of fio's write and read-back."""
    if (directory / "verify.1.failed").exists():
        return FAIL, "fio failed"
    try:
        text = (directory / "verify.1.json").read_text()
        job = json.loads(text[text.index("{") :])["jobs"][0]
        if job["error"] != 0:
            return FAIL, f"fio error {job['error']}"
        expect_options(job, "verify", {"verify": "crc32c", "do_verify": "1"})
        written, read = job["write"]["io_bytes"], job["read"]["io_bytes"]
    except OSError:
        return UNKNOWN, "verify.1.json is missing"
    except Unusable as e:
        return UNKNOWN, str(e)
    except (ValueError, KeyError, IndexError, TypeError) as e:
        return UNKNOWN, f"verify.1.json cannot be read: {e!r}"
    if written <= 0 or read < written:
        return UNKNOWN, f"read back {read} of {written} bytes written"
    return PASS, f"{written} bytes written and read back"


SAMPLE_LINE = re.compile(r"^## sample t=(\d+) done=(\d+) status=(\S+)")
END_LINE = re.compile(r"^## load-end t=(\d+)")
PORT_LINE = re.compile(r"^# port (\d+)(.*)$")
METRIC_LINE = re.compile(r"^([A-Za-z_:][A-Za-z0-9_:]*)(?:\{.*\})?\s+(\S+)")


def add_metric(ports, port, line):
    m = METRIC_LINE.match(line)
    if not m:
        return
    try:
        value = float(m[2])
    except ValueError:
        return
    values = ports.setdefault(port, {})
    values[m[1]] = values.get(m[1], 0.0) + value  # label sets (metadata groups) of one node


def parse_series(text):
    """(samples, load-end t or None); a sample is {t, done, ports: {port: {metric: value}}}."""
    samples, end, sample, port = [], None, None, None
    for line in text.splitlines():
        if m := SAMPLE_LINE.match(line):
            sample = {"t": int(m[1]), "done": int(m[2]), "ports": {}}
            samples.append(sample)
            port = None
        elif m := END_LINE.match(line):
            end, sample, port = int(m[1]), None, None
        elif m := PORT_LINE.match(line):
            port = None if "scrape-failed" in m[2] else m[1]
        elif sample is not None and port is not None and not line.startswith("#"):
            add_metric(sample["ports"], port, line)
    return samples, end


def pending_of(sample):
    values = [
        sample["ports"][p]["atlas_native_delta_pending"]
        for p in META_PORTS
        if "atlas_native_delta_pending" in sample["ports"].get(p, {})
    ]
    return statistics.median(values) if len(values) >= 2 else None


def total_of(sample, name):
    """A per-node counter summed over the metadata nodes; None unless all of them reported it."""
    if any(name not in sample["ports"].get(p, {}) for p in META_PORTS):
        return None
    return sum(sample["ports"][p][name] for p in META_PORTS)


def counter_increase(samples, name):
    """Increase over `samples` summed over nodes, each between its first and last report."""
    total, seen = 0.0, False
    for port in META_PORTS:
        values = [s["ports"][port][name] for s in samples if name in s["ports"].get(port, {})]
        if len(values) >= 2:
            total, seen = total + values[-1] - values[0], True
    return total if seen else None


def delta_settings(directory):
    """(delta writes enabled, limits) from the node settings the run recorded."""
    try:
        for line in (directory / "settings.txt").read_text().splitlines():
            if line.startswith("{"):
                delta = json.loads(line).get("delta_writes", {})
                limits = {k: delta.get(k, v) for k, v in DEFAULT_LIMITS.items()}
                return bool(delta.get("enabled", False)), limits
    except (OSError, ValueError, AttributeError) as e:
        raise Unusable(f"settings.txt cannot be read: {e!r}") from None
    raise Unusable("settings.txt has no metadata settings line")


def slope(points):
    xs, ys = [p[0] for p in points], [p[1] for p in points]
    mx, my = statistics.fmean(xs), statistics.fmean(ys)
    var = sum((x - mx) ** 2 for x in xs)
    return sum((x - mx) * (y - my) for x, y in points) / var if var else 0.0


def pending_trend(directory):
    """(status, detail) for one atlas round."""
    try:
        text = (directory / "metrics-series.txt").read_text()
    except OSError:
        return UNKNOWN, "no metrics-series.txt"
    try:
        enabled, limits = delta_settings(directory)
    except Unusable as e:
        return UNKNOWN, str(e)
    samples, end = parse_series(text)
    if end is None:
        return UNKNOWN, "metrics-series.txt has no load-end mark (the run did not finish)"
    if end < STEADY_SECS:
        return UNKNOWN, f"the load ran {end} s, the target is {STEADY_SECS} s"
    for port in META_PORTS:
        for name in ("atlas_native_delta_writes_total", "atlas_native_delta_folds_total"):
            values = [s["ports"][port][name] for s in samples if name in s["ports"].get(port, {})]
            if any(b < a for a, b in zip(values, values[1:])):
                return UNKNOWN, f"{name} went backwards on port {port} (a node restarted?)"
    window = [
        s
        for s in samples
        if s["t"] >= end - WINDOW - EDGE / 3 and s["done"] <= end and pending_of(s) is not None
    ]
    if len(window) < MIN_SAMPLES:
        return UNKNOWN, f"only {len(window)} usable samples in the last {WINDOW} s of load"
    times = [s["t"] for s in window]
    if times[0] > end - WINDOW + EDGE or times[-1] < end - EDGE:
        return UNKNOWN, f"samples span t={times[0]}..{times[-1]}, not the last {WINDOW} s before t={end}"
    gap = max(b - a for a, b in zip(times, times[1:]))
    if gap > MAX_GAP:
        return UNKNOWN, f"a gap of {gap} s between samples"
    counted = [s for s in window if total_of(s, "atlas_native_delta_writes_total") is not None]
    if len(counted) < 2 or counted[0]["t"] > times[0] + MAX_GAP or counted[-1]["t"] < times[-1] - MAX_GAP:
        return UNKNOWN, "delta write counters of all three metadata nodes are not there across the window"
    writes = total_of(counted[-1], "atlas_native_delta_writes_total") - total_of(
        counted[0], "atlas_native_delta_writes_total"
    )
    pending = [pending_of(s) for s in window]
    ends = statistics.median(pending[-3:]) - statistics.median(pending[:3])
    trend = slope(list(zip(times, pending))) * (times[-1] - times[0])
    tolerance = GROWTH_TOLERANCE * writes
    folds = counter_increase(counted, "atlas_native_delta_folds_total")
    fallbacks = counter_increase(counted, "atlas_native_delta_fallbacks_total")
    detail = (
        f"pending {pending[0]:.0f} -> {pending[-1]:.0f} (max {max(pending):.0f}) over t={times[0]}..{times[-1]}; "
        f"end medians {ends:+.0f}, trend {trend:+.0f}, tolerated {tolerance:.0f}; delta writes {writes:.0f}, "
        f"folds {'?' if folds is None else f'{folds:.0f}'}, "
        f"fallbacks {'?' if fallbacks is None else f'{fallbacks:.0f}'}"
    )
    bytes_room = limits["max_pending_bytes"] - limits["fold_headroom_bytes"]
    bytes_max = max(
        (
            statistics.median(v)
            for s in window
            if (
                v := [
                    s["ports"][p]["atlas_native_delta_pending_bytes"]
                    for p in META_PORTS
                    if "atlas_native_delta_pending_bytes" in s["ports"].get(p, {})
                ]
            )
        ),
        default=0,
    )
    if max(pending) >= AT_LIMIT * limits["max_pending_deltas"]:
        return FAIL, f"backlog at its limit of {limits['max_pending_deltas']} deltas; {detail}"
    if enabled and bytes_room > 0 and bytes_max >= AT_LIMIT * bytes_room:
        return FAIL, f"backlog at its byte limit ({bytes_max:.0f} of {bytes_room}); {detail}"
    if ends > tolerance or trend > tolerance:
        return FAIL, f"growing; {detail}"
    if enabled and writes <= 0:
        return UNKNOWN, f"delta writes are on but none was taken in the window; {detail}"
    if not enabled:
        detail = f"delta writes off; {detail}"
    return PASS, f"not growing; {detail}"


def snapshot_note(directory):
    """Counters at the end of the long run, from the single snapshot older result sets have too."""
    ports, port = {}, None
    try:
        text = (directory / "metrics-after-steady.txt").read_text()
    except OSError:
        return None
    for line in text.splitlines():
        if m := PORT_LINE.match(line):
            port = None if "scrape-failed" in m[2] else m[1]
        elif port and not line.startswith("#"):
            add_metric(ports, port, line)
    sample = {"ports": ports}
    pending = pending_of(sample)
    writes = total_of(sample, "atlas_native_delta_writes_total")
    folds = total_of(sample, "atlas_native_delta_folds_total")
    if pending is None or writes is None or folds is None:
        return None
    return f"after the long run: {writes:.0f} delta writes and {folds:.0f} folds (summed), {pending:.0f} pending"


def run_notes(directory, env):
    notes = []
    loads = []
    try:
        loads = [float(v) for v in re.findall(r"load1=(\S+)", (directory / "host-series.txt").read_text())]
    except (OSError, ValueError):
        pass
    start = env.get("loadavg", "?").split(" ")[0]
    stop = env.get("loadavg_end", "?").split(" ")[0]
    note = f"host load (1 min) at start {start}, at end {stop}"
    if loads:
        note += f", during the long run median {statistics.median(loads):.1f} max {max(loads):.1f}"
    notes.append(note)
    if env.get("system") == "atlas":
        try:
            samples, _ = parse_series((directory / "metrics-series.txt").read_text())
            elections = counter_increase(samples, "atlas_native_raft_elections_total")
            if elections is not None:
                notes.append(f"raft elections during the long run (summed): {elections:.0f}")
        except OSError:
            pass
        if note := snapshot_note(directory):
            notes.append(note)
    return notes


def load_run(system, directory):
    run = {"system": system, "dir": directory, "env": read_env(directory / "env.txt"), "problems": []}
    if not directory.is_dir():
        run["problems"].append("directory is missing")
    elif not run["env"]:
        run["problems"].append("env.txt is missing or empty")
    elif run["env"].get("system") != system:
        run["problems"].append(f"env.txt says system={run['env'].get('system')}")
    for key, load in (("steady", lambda: steady(directory)), ("seqread", lambda: seqread(directory, run["env"]))):
        try:
            run[key] = load()
        except Unusable as e:
            run[key] = None
            run[f"{key}_why"] = str(e)
    run["verify"] = verify(directory)
    if system == "atlas":
        run["pending"] = pending_trend(directory)
    return run


def session_problems(runs):
    problems = []
    ceph = [r for r in runs if r["system"] == "ceph"]
    atlas = [r for r in runs if r["system"] == "atlas"]
    for r in runs:
        problems += [f"{r['dir']}: {p}" for p in r["problems"]]
    if len({r["dir"].resolve() for r in runs}) != len(runs):
        problems.append("a result set is listed twice")
    if len(ceph) < MIN_ROUNDS or len(atlas) < MIN_ROUNDS:
        problems.append(f"{len(ceph)} Ceph and {len(atlas)} atlas rounds; the target needs {MIN_ROUNDS} of each")
    if len(ceph) != len(atlas):
        problems.append("unequal numbers of rounds")
    sessions = {r["env"].get("session") or None for r in runs}
    if None in sessions or len(sessions) != 1:
        found = sorted(str(s) for s in sessions)
        problems.append(f"not one compare.sh session (session in env.txt: {', '.join(found)})")
    try:
        order = sorted(runs, key=lambda r: datetime.datetime.fromisoformat(r["env"]["date"]))
        if any(a["system"] == b["system"] for a, b in zip(order, order[1:])):
            problems.append("the rounds did not alternate: " + ", ".join(r["system"] for r in order))
    except (KeyError, ValueError, TypeError):
        problems.append("a result set has no usable date")
    for key in SAME_ENV:
        values = {r["env"].get(key) for r in runs}
        if None in values or len(values) != 1:
            problems.append(f"{key} differs or is missing: {sorted(str(v) for v in values)}")
    # The commit is what the source copy claims; the checksum is what ran (builds on a shared host).
    for key, what in (("commit", "atlas commit"), ("node_binary", "atlas node binary")):
        values = {r["env"].get(key) for r in atlas}
        if len(values) != 1 or values & {None, "", "unknown"}:
            problems.append(f"{what} differs or is unknown: {sorted(str(v) for v in values)}")
    settings = set()
    for r in atlas:
        try:
            settings.add((r["dir"] / "settings.txt").read_text())
        except OSError:
            settings.add(None)
    if None in settings or len(settings) != 1:
        problems.append("atlas settings differ between rounds or are missing")
    return problems


def compare(runs, key, field, label, unit, ok, target):
    """Median of `field` per system, judged by ok(atlas, ceph)."""
    values, missing = {}, []
    for system in ("ceph", "atlas"):
        values[system] = []
        for r in (r for r in runs if r["system"] == system):
            if r[key] is None:
                missing.append(f"{r['dir']}: {r[key + '_why']}")
            else:
                values[system].append(r[key] if field is None else r[key][field])
    rounds = "; ".join(f"{s} {' '.join(f'{v:.1f}' for v in values[s]) or '-'}" for s in values)
    if missing or not values["ceph"] or not values["atlas"]:
        return UNKNOWN, f"{label}: rounds: {rounds}", missing
    c, a = statistics.median(values["ceph"]), statistics.median(values["atlas"])
    status = PASS if ok(a, c) else FAIL
    return status, f"{label}: atlas {a:.1f} vs Ceph {c:.1f} {unit} ({target}); rounds: {rounds}", []


def every_atlas(runs, key, label):
    results = [(r["dir"], *r[key]) for r in runs if r["system"] == "atlas"]
    states = [s for _, s, _ in results]
    status = FAIL if FAIL in states else UNKNOWN if UNKNOWN in states or not states else PASS
    details = [f"{d}: {s}: {text}" for d, s, text in results]
    return status, f"{label}: {states.count(PASS)} of {len(states)} atlas rounds pass", details


def judge(ceph_dirs, atlas_dirs):
    """(verdict, report lines)."""
    runs = [load_run("ceph", Path(d)) for d in ceph_dirs] + [load_run("atlas", Path(d)) for d in atlas_dirs]
    lines = ["Gate: docs/NATIVE_DELTA_REDESIGN.md, target", "", "Result sets"]
    for r in runs:
        lines.append(f"  {r['system']:<5} {r['dir']} (commit {r['env'].get('commit', '?')}, {r['env'].get('date', '?')})")
        lines += [f"        {n}" for n in run_notes(r["dir"], r["env"])]

    problems = session_problems(runs)
    criteria = [
        compare(runs, "steady", "iops", "write IOPS", "IOPS", lambda a, c: a >= c, "needs atlas >= Ceph"),
        compare(runs, "steady", "p99", "p99 write latency", "ms", lambda a, c: a <= c, "needs atlas <= Ceph"),
        every_atlas(runs, "pending", f"pending deltas not growing over the last {WINDOW} s"),
        compare(runs, "seqread", None, "sequential read afterwards", "MiB/s", lambda a, c: a >= c / 2,
                "needs atlas >= half of Ceph"),
        every_atlas(runs, "verify", "fio crc32c verification"),
    ]
    lines += ["", f"[{UNKNOWN if problems else PASS}] session and inputs"]
    lines += [f"        {p}" for p in problems]
    states = [UNKNOWN] if problems else []
    for status, text, details in criteria:
        # A comparison that is met on inputs short of the target proves nothing.
        if problems and status == PASS:
            status, text = UNKNOWN, text + " [met, but the session and inputs fall short]"
        states.append(status)
        lines.append(f"[{status}] {text}")
        lines += [f"        {d}" for d in details]
    ceph_failed = [f"{r['dir']}: {r['verify'][1]}" for r in runs if r["system"] == "ceph" and r["verify"][0] != PASS]
    if ceph_failed:
        lines.append("note: Ceph verification did not pass (not part of the target): " + "; ".join(ceph_failed))
    verdict = FAIL if FAIL in states else UNKNOWN if UNKNOWN in states else PASS
    lines += ["", f"VERDICT: {verdict}" + (" (not a pass)" if verdict == UNKNOWN else "")]
    return verdict, lines


def main(argv):
    groups, current = {"--ceph": [], "--atlas": []}, None
    for arg in argv:
        if arg in groups:
            current = arg
        elif current is None or arg.startswith("-"):
            current = None
            break
        else:
            groups[current].append(arg)
    if current is None or not all(groups.values()):
        print(__doc__, file=sys.stderr)
        return 2
    verdict, lines = judge(groups["--ceph"], groups["--atlas"])
    print("\n".join(lines))
    return EXIT[verdict]


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except Exception as e:  # an input nobody foresaw must not look like a pass (or a plain failure)
        print(f"VERDICT: {UNKNOWN} (not a pass): gate.py could not judge these inputs: {e!r}")
        sys.exit(EXIT[UNKNOWN])
