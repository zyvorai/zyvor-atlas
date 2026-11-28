#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
"""Markdown report and verdict of an agent.sh run.

Usage: agent_report.py <results/<tag>-agent/plan.json>

The plan says which runs and items must exist; the result sets lie next to the plan's directory.

  VERDICT PASS     every planned item of every run reported ok with its figures, at least 3 rounds
                   that alternated, one build and one set of host settings per case
  VERDICT FAIL     some data or result check failed, or a step could not be carried out
  VERDICT UNKNOWN  anything missing, unsupported, incomplete or non-standard; never a pass

The verdict says that the measurements are complete and verified. It does not say that atlas-native
met a performance target; the research bets are judged in their own sections. A figure is a median
and range over all rounds; if any round lacks it, the cell says so instead. Exit status: 0 PASS,
1 FAIL, 3 UNKNOWN, 2 usage.
"""

from __future__ import annotations

import datetime
import json
import statistics
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.dont_write_bytecode = True
import agent_lib as lib  # noqa: E402

PASS, FAIL, UNKNOWN = "PASS", "FAIL", "UNKNOWN"
EXIT = {PASS: 0, FAIL: 1, UNKNOWN: 3}
MIB = 1024 * 1024

# Bet 2 (docs/NATIVE_STORAGE_RESEARCH.md).
BET2_SMALL, BET2_LARGE = 1000, 100000
BET2_SECONDS = 1.0
BET2_FACTOR = 2.0
# Memory growth below this is allocator noise; the 2x rule is applied to at least this much.
BET2_MEMORY_FLOOR = 8 * MIB
# With fewer operations in a window its p99 is just its slowest one.
P99_OPS = 100
BET1_WRITERS = 8
BET1_FACTOR = 3.0

COLUMNS = {
    "A": [("tool_exit_s", "tool exit s"), ("durable_s", "durable s"), ("sync_s", "of which sync s")],
    "B": [("clone_s", "clone s"), ("status_s", "status s"), ("edit_s", "20 edits s"), ("commit_s", "commit s"),
          ("fsck_s", "fsck s")],
    "C": [("verify_s", "sha256sum -c s"), ("remount_s", "kill to remounted s"), ("files", "files")],
    "D": [("snapshot_s", "snapshot s"), ("clone_s", "clone s"), ("mount_s", "mount s"),
          ("first_read_s", "first read s"), ("fork_s", "fork s"), ("total_s", "total s"),
          ("rss_growth_bytes", "metadata RSS growth MiB"), ("space_growth_bytes", "space growth MiB"),
          ("metadata_growth_bytes", "metadata disk growth MiB"), ("elections", "elections"),
          ("bg_base_p99_ms", "bg p99 before ms"), ("bg_fork_p99_ms", "bg p99 during ms"),
          ("bg_after_p99_ms", "bg p99 after ms"), ("bg_fork_ops", "bg ops during")],
    "E": [("snapshot_s", "snapshot s"), ("fork_wall_s", "forks wall s"), ("fork_slowest_s", "slowest fork s"),
          ("cold_wall_s", "cold read wall s"), ("cold_slowest_s", "cold slowest s"), ("cold_p99_ms", "cold p99 ms"),
          ("warm_wall_s", "warm read wall s"), ("warm_p99_ms", "warm p99 ms"), ("edit_wall_s", "edits wall s"),
          ("edit_p99_ms", "edit p99 ms"), ("space_growth_bytes", "space growth MiB")],
    "F": [("delete_s", "delete calls s"), ("stable_s", "until space stable s"),
          ("reclaimed_bytes", "reclaimed MiB"), ("residual_bytes", "left over MiB")],
    "G": [("write_mibs", "write MiB/s"), ("read_mibs", "cold read MiB/s")],
    "S": [("files_per_s", "files/s"), ("p99_ms", "p99 ms"), ("disk_flushes", "disk flushes"),
          ("disk_mib_written", "disk MiB written"), ("raft_syncs_per_file", "raft log syncs/file"),
          ("raft_sync_mean_ms", "raft sync ms"), ("data_syncs_per_file", "data syncs/file"),
          ("data_sync_mean_ms", "data sync ms")],
}
TITLES = {
    "A": "A. Install: unpack the package fixture, then fsync every file and directory",
    "B": "B. Git: clone, status, 20 edits, commit, fsck",
    "C": "C. Verify: sha256sum -c, also after kill -9 of the client (not a power-loss test)",
    "D": "D. Fork one tree: snapshot, writable view, mount, first read",
    "E": "E. Fan-out: K writable views of one snapshot; read cold, warm, 20 edits each; isolation checked",
    "F": "F. Discard E's views and snapshots, until the space figures stop changing",
    "G": "G. Large files: write, rename, fsync; remount, drop server caches, read",
    "S": "Small files (run.sh's smallfile-create-4k, one fsync per close)",
}
GUARANTEES = [
    ("clone", "the system's own writable clone, replicated like any data. atlas-native: metadata only, "
     "sharing extents. CephFS: `fs subvolume snapshot clone`, an asynchronous bulk copy, timed until `complete`."),
    ("overlay", "read-only snapshot on a mount of its own, plus a local overlayfs upper layer. "
     "**The upper layer is unreplicated**: writes in it do not survive the loss of this host."),
    ("overlay-shared", "as overlay, with one lower layer for all views. CephFS: the `.snap` directory of a "
     "mount that already exists (nothing new is mounted). atlas-native: one snapshot mount per snapshot. "
     "Upper layer unreplicated."),
    ("reflink", "floor, not a competitor: `cp -a --reflink=always` on the host's XFS, one copy, no "
     "snapshot, no isolation from the host, no container limits. Durable only after the separate sync."),
]


class Run:
    def __init__(self, plan, entry, root):
        self.plan, self.case, self.system, self.round = plan, entry["case"], entry["system"], entry["round"]
        self.dir = root / entry["dir"] / "agent"
        self.problems, self.records, self.env, self.settings = [], {}, {}, {}
        self.expected = []
        self.load()

    def label(self):
        return f"round {self.round} {self.case}"

    def load(self):
        for name, phases, sizes in lib.segments(self.plan, self.system):
            self.expected += lib.segment_items(self.plan, self.system, phases, sizes)
        if not self.dir.is_dir():
            self.problems.append("no result directory")
            return
        try:
            for line in (self.dir / "env.txt").read_text().splitlines():
                key, sep, value = line.partition("=")
                if sep:
                    self.env[key] = value
        except OSError:
            self.problems.append("env.txt is missing")
        for key, wanted in (("case", self.case), ("system", self.system), ("session", self.plan["tag"]),
                            ("round", str(self.round))):
            if self.env and self.env.get(key) != wanted:
                self.problems.append(f"env.txt says {key}={self.env.get(key)}, expected {wanted}")
        if self.env and "end" not in self.env:
            self.problems.append("env.txt has no end: the run did not finish")
        for name, phases, sizes in lib.segments(self.plan, self.system):
            try:
                events = lib.parse_events((self.dir / f"{name}.jsonl").read_text(errors="replace"))
            except OSError:
                self.problems.append(f"segment {name}: no results ({self.env.get(f'segment_{name}', 'not run')})")
                continue
            if not any(e.get("type") == "end" and e.get("complete") is True for e in events):
                self.problems.append(
                    f"segment {name}: did not complete ({self.env.get(f'segment_{name}', 'no status')})"
                )
            wanted = set(lib.segment_items(self.plan, self.system, phases, sizes))
            for e in events:
                if e.get("type") == "item" and e.get("id") in wanted:
                    if e["id"] in self.records:
                        self.records[e["id"]] = {"status": lib.INCOMPLETE, "detail": "reported twice"}
                    else:
                        self.records[e["id"]] = e
            if self.system == "atlas":
                try:
                    self.settings[name] = (self.dir / f"settings-{name}.txt").read_text()
                except OSError:
                    self.problems.append(f"segment {name}: no node settings recorded")

    def state(self, item):
        return lib.item_state(self.records.get(item), item, self.system)


def session_problems(plan, runs):
    problems = [f"{r.label()}: {p}" for r in runs for p in r.problems]
    if plan["rounds"] < lib.MIN_REPEATS:
        problems.append(f"{plan['rounds']} rounds; a result needs {lib.MIN_REPEATS}")
    planned = {(r, c) for r in range(1, plan["rounds"] + 1) for c in plan["cases"]}
    listed = [(r.round, r.case) for r in runs]
    if len(listed) != len(set(listed)) or set(listed) != planned:
        problems.append("the plan's runs are not one per case and round")
    if len(plan["cases"]) > 1:
        try:
            order = sorted(runs, key=lambda r: datetime.datetime.fromisoformat(r.env["date"]))
            if any(a.case == b.case for a, b in zip(order, order[1:])):
                problems.append("the rounds did not alternate: " + ", ".join(r.case for r in order))
        except (KeyError, ValueError, TypeError):
            problems.append("a run has no usable date")

    def same(key, group, what):
        values = {r.env.get(key) for r in group}
        if len(values) != 1 or values & {None, "", "unknown"}:
            problems.append(f"{what} differs or is unknown: {sorted(str(v) for v in values)}")

    if runs:
        for key in ("kernel", "disk", "harness_sha256", "fixtures_head"):
            same(key, runs, key)
        containers = [r for r in runs if r.system != "floor"]
        if containers:
            for key in ("mem", "cpus", "tmpfs_gb"):
                same(key, containers, key)
    for case in plan["cases"]:
        group = [r for r in runs if r.case == case]
        system = lib.case_system(case)
        if system == "atlas":
            same("commit", group, f"{case}: commit")
            same("node_binary", group, f"{case}: node binary")
            for name, *_ in lib.segments(plan, system):
                values = {r.settings.get(name) for r in group}
                if len(values) != 1 or None in values:
                    problems.append(f"{case}: node settings of segment {name} differ between rounds or are missing")
        elif system == "ceph":
            for name, *_ in lib.segments(plan, system):
                same(f"image_{name}", group, f"{case}: image of segment {name}")
    return problems


class Results:
    def __init__(self, plan, root):
        self.plan = plan
        self.runs = [Run(plan, entry, root) for entry in plan["runs"]]
        self.problems = session_problems(plan, self.runs)

    def of(self, case):
        return [r for r in self.runs if r.case == case]

    def items(self):
        out = []
        for case in self.plan["cases"]:
            for r in self.of(case)[:1]:
                out += [i for i in r.expected if i not in out]
        return out

    def values(self, case, item, metric):
        """(one value per round, "") only if every round reports it ok; otherwise (None, why)."""
        runs = self.of(case)
        states = [r.state(item) for r in runs]
        for bad in (lib.FAILED, lib.ERROR, lib.UNSUPPORTED, lib.INCOMPLETE, "missing"):
            count = sum(s == bad for s, _ in states)
            if count:
                label = {lib.FAILED: "FAILED", lib.ERROR: "ERROR"}.get(bad, bad)
                return None, f"{label} ({count}/{len(runs)})"
        if len(runs) < self.plan["rounds"] or not runs:
            return None, "missing"
        values = [(r.records[item].get("metrics") or {}).get(metric) for r in runs]
        if not all(lib.finite(v) for v in values):
            return None, "-" if all(v is None for v in values) else "figure missing"
        return values, ""

    def verdict(self):
        states = [r.state(i)[0] for r in self.runs for i in r.expected]
        if any(s in (lib.FAILED, lib.ERROR) for s in states):
            return FAIL
        if self.problems or not states or any(s != lib.OK for s in states):
            return UNKNOWN
        return PASS


def number(value):
    if abs(value) >= 100 or value == int(value):
        return f"{value:.0f}"
    if abs(value) >= 1:
        return f"{value:.2f}"
    return f"{value:.3f}"


def cell(values, why, metric):
    if values is None:
        return why
    if metric.endswith("_bytes"):
        values = [v / MIB for v in values]
    mid, low, high = lib.median_range(values)
    if low == high:
        return number(mid)
    return f"{number(mid)} ({number(low)}..{number(high)})"


def phase_table(results, phase):
    items = [i for i in results.items() if i.split("/")[0] == phase]
    if not items:
        return []
    rows = []
    for item in items:
        for case in results.plan["cases"]:
            runs = results.of(case)
            if not runs or item not in runs[0].expected:
                continue
            states = [r.state(item)[0] for r in runs]
            cells = []
            for metric, _ in COLUMNS[phase]:
                cells.append(cell(*results.values(case, item, metric), metric))
            ok = f"{states.count(lib.OK)}/{results.plan['rounds']}"
            rows.append((item, case, ok, cells))
    # Columns no row has anything in are left out.
    used = [n for n in range(len(COLUMNS[phase])) if any(row[3][n] != "-" for row in rows)]
    head = ["item", "case", "rounds ok"] + [COLUMNS[phase][n][1] for n in used]
    lines = ["", f"## {TITLES[phase]}", "", "| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
    for item, case, ok, cells in rows:
        lines.append("| " + " | ".join([item, case, ok] + [cells[n] for n in used]) + " |")
    return lines


def combine(statuses):
    if FAIL in statuses:
        return FAIL
    return UNKNOWN if UNKNOWN in statuses or not statuses else PASS


def bet2(results, case):
    """[(status, text)] of the claim of bet 2 for one atlas case."""
    plan = results.plan
    if "D" not in plan["phases"]:
        return []
    sizes = plan["fork_sizes"]
    if BET2_SMALL not in sizes or BET2_LARGE not in sizes:
        return [(UNKNOWN, f"needs trees of {BET2_SMALL} and {BET2_LARGE} files; this run has {sizes}")]
    if "clone" not in plan["methods"]:
        return [(UNKNOWN, "the clone method was not measured")]
    out = []
    for mode in plan["fork_modes"]:
        large, why_l = results.values(case, f"D/{BET2_LARGE}/{mode}/clone", "fork_s")
        small, why_s = results.values(case, f"D/{BET2_SMALL}/{mode}/clone", "fork_s")
        if large is None:
            out.append((UNKNOWN, f"{mode}: snapshot + clone at {BET2_LARGE}: {why_l}"))
        else:
            out.append((PASS if max(large) < BET2_SECONDS else FAIL,
                        f"{mode}: snapshot + clone at {BET2_LARGE} files under {BET2_SECONDS:g} s in every round: "
                        f"{', '.join(number(v) for v in large)} s"))
        if large is None or small is None:
            out.append((UNKNOWN, f"{mode}: time ratio: {why_l or why_s}"))
        else:
            a, b = statistics.median(large), statistics.median(small)
            out.append((PASS if a <= BET2_FACTOR * b else FAIL,
                        f"{mode}: median time within {BET2_FACTOR:g}x from {BET2_SMALL} to {BET2_LARGE} files: "
                        f"{number(b)} s -> {number(a)} s"))
    # Memory is judged on the quiet forks only: with a writer, its own growth is in the figure.
    large, why_l = results.values(case, f"D/{BET2_LARGE}/quiet/clone", "rss_growth_bytes")
    small, why_s = results.values(case, f"D/{BET2_SMALL}/quiet/clone", "rss_growth_bytes")
    if "quiet" not in plan["fork_modes"]:
        out.append((UNKNOWN, "metadata memory growth: no quiet fork in this run"))
    elif large is None or small is None:
        out.append((UNKNOWN, f"quiet: metadata memory growth: {why_l or why_s}"))
    else:
        a, b = statistics.median(large), statistics.median(small)
        floor = max(b, BET2_MEMORY_FLOOR)
        out.append((PASS if a <= BET2_FACTOR * floor else FAIL,
                    f"quiet: metadata memory growth within {BET2_FACTOR:g}x: {b / MIB:.1f} -> {a / MIB:.1f} MiB"
                    f" (compared with at least {BET2_MEMORY_FLOOR // MIB} MiB)"))
    elections, missing = [], []
    for mode in plan["fork_modes"]:
        for n in sizes:
            values, why = results.values(case, f"D/{n}/{mode}/clone", "elections")
            if values is None:
                missing.append(f"D/{n}/{mode}: {why}")
            else:
                elections += values
    if any(v > 0 for v in elections):
        out.append((FAIL, f"no election meanwhile: {sum(elections):.0f} elections during clone forks"))
    elif missing:
        out.append((UNKNOWN, "no election meanwhile: " + "; ".join(missing)))
    else:
        out.append((PASS, "no election during any clone fork"))
    if "busy" not in plan["fork_modes"]:
        out.append((UNKNOWN, "foreground p99: no run with a background writer"))
        return out
    return out + [foreground(results, case, n) for n in sizes]


def window_verdict(base_p99, base_max, p99, worst, ops):
    """Whether the foreground p99 stayed within 2x in one window of one round. A short window has no
    p99 of its own: it fails only if an operation took longer than twice anything seen before."""
    if p99 <= BET2_FACTOR * base_p99:
        return PASS
    return FAIL if ops >= P99_OPS or worst > BET2_FACTOR * base_max else UNKNOWN


def foreground(results, case, n):
    """(status, text) for the background writer of the clone fork of the tree of n files."""
    item = f"D/{n}/busy/clone"
    got, why = {}, ""
    for key in ("base_p99_ms", "base_max_ms", "fork_p99_ms", "fork_max_ms", "fork_ops",
                "after_p99_ms", "after_max_ms", "after_ops"):
        got[key], reason = results.values(case, item, "bg_" + key)
        if not key.startswith("after"):
            why = why or reason
    if why:
        return UNKNOWN, f"foreground p99 at {n}: {why}"
    statuses, parts = [], []
    for window, label in (("fork", "during"), ("after", "after it")):
        columns = [got[f"{window}_p99_ms"], got[f"{window}_max_ms"], got[f"{window}_ops"]]
        if None in columns:
            statuses.append(UNKNOWN)
            parts.append("no figure for the time after the fork")
            continue
        rounds = list(zip(got["base_p99_ms"], got["base_max_ms"], *columns))
        statuses += [window_verdict(*r) for r in rounds]
        parts.append(f"{label} " + ", ".join(f"x{r[2] / r[0]:.2f} ({r[4]:.0f} ops)" for r in rounds))
    return combine(statuses), (f"foreground p99 at {n} files at most doubled in every round: " + "; ".join(parts)
                               + f" [under {P99_OPS} ops, over 2x counts only beyond twice the slowest before]")


def bet2_section(results):
    if "D" not in results.plan["phases"]:
        return []
    lines = ["", "## Research bet 2: fork cost independent of file count",
             "", "Does the build under test **already** meet the claim? If it does, the bet says: do nothing."]
    if results.plan["tmpfs_gb"]:
        lines.append(f"Server data on **tmpfs** ({results.plan['tmpfs_gb']} GiB): times carry no disk cost.")
    for case in results.plan["cases"]:
        if lib.case_system(case) != "atlas":
            continue
        criteria = bet2(results, case)
        if results.problems:
            criteria = [(UNKNOWN, t + " [met, but the session and inputs fall short]") if s == PASS else (s, t)
                        for s, t in criteria]
        answer = {PASS: "yes: the claim is met (bet not needed)", FAIL: "no: the claim is not met",
                  UNKNOWN: "unknown"}[combine([s for s, _ in criteria])]
        lines += ["", f"### {case}: {answer}", ""] + [f"- [{s}] {t}" for s, t in criteria]
    return lines


def bet1(results, case, base="atlas"):
    """(status, text): at least 3x the base's rate with 8 writers."""
    if case == base or BET1_WRITERS not in results.plan["writers"] or base not in results.plan["cases"]:
        return None
    item = f"S/w{BET1_WRITERS}"
    mine, why = results.values(case, item, "files_per_s")
    theirs, why_b = results.values(base, item, "files_per_s")
    if mine is None or theirs is None:
        return UNKNOWN, f"{case} against {base} at {BET1_WRITERS} writers: {why or why_b}"
    ratio = statistics.median(mine) / statistics.median(theirs)
    status = PASS if ratio >= BET1_FACTOR else FAIL
    if status == PASS and results.problems:
        status = UNKNOWN
    return status, (f"{case}: {ratio:.2f}x the rate of {base} at {BET1_WRITERS} writers "
                    f"(claim: at least {BET1_FACTOR:g}x)")


def bet1_section(results):
    if "S" not in results.plan["phases"]:
        return []
    writers = results.plan["writers"]
    head = ["case"] + [f"files/s at {w}" for w in writers] + [f"{w} / {writers[0]}" for w in writers[1:]]
    lines = ["", "## Research bet 1: concurrent small-write commits", "", "| " + " | ".join(head) + " |",
             "|" + "---|" * len(head)]
    for case in results.plan["cases"]:
        got = [results.values(case, f"S/w{w}", "files_per_s") for w in writers]
        medians = [None if v is None else statistics.median(v) for v, _ in got]
        cells = [cell(v, why, "files_per_s") for v, why in got]
        cells += ["-" if m is None or not medians[0] else f"x{m / medians[0]:.2f}" for m in medians[1:]]
        lines.append("| " + " | ".join([case] + cells) + " |")
    lines += [
        "",
        "Disproof, as written: the bet is dead if the rate already scales with writers, or if one writer "
        "already keeps the disk at its flush ceiling with few syncs to remove. The page gives no number for "
        "either, so none is judged here: compare the ratios above, the syncs per file in the table, and "
        "disk-probe.sh's flush ceiling. A durability claim is not tested by this job.",
    ]
    verdicts = [v for case in results.plan["cases"] if lib.case_system(case) == "atlas" if (v := bet1(results, case))]
    if verdicts:
        lines += [""] + [f"- [{s}] {t}" for s, t in verdicts]
    return lines


def conditions_text(c):
    try:
        pressure = c["io_pressure"]["some"]["avg10"]
        return f"{c['load'][0]:.1f} / {pressure:.0f}"
    except (KeyError, TypeError, IndexError):
        return "?"


def report(plan, root):
    """(verdict, markdown)."""
    results = Results(plan, root)
    verdict = results.verdict()
    lines = [f"# Agent profile `{plan['profile']}`, tag `{plan['tag']}`", "",
             f"**VERDICT: {verdict}**" + (" (not a pass)" if verdict == UNKNOWN else ""), "",
             "The verdict covers completeness and verification, not speed. Cells are median (min..max) "
             f"over {plan['rounds']} rounds."]
    data = (f"**tmpfs** of {plan['tmpfs_gb']} GiB inside the container's memory limit (not the benchmark disk)"
            if plan["tmpfs_gb"] else "the benchmark disk")
    lines += ["", f"Server data of ceph and atlas on {data}; container limit {plan['mem']}; floor on the host's "
              "XFS disk. Fixtures: " + json.dumps(plan["fixtures"], sort_keys=True) + ". Settings: "
              + json.dumps(plan["settings"], sort_keys=True) + "."]
    for case, env in sorted(plan["case_env"].items()):
        if env:
            lines.append(f"Settings of {case}: `{json.dumps(env, sort_keys=True)}`")

    lines += ["", "## Session and inputs", ""]
    lines += [f"- {p}" for p in results.problems] or ["- no problem found"]
    bad = [(r, i, *r.state(i)) for r in results.runs for i in r.expected if r.state(i)[0] != lib.OK]
    total = sum(len(r.expected) for r in results.runs)
    lines += ["", "## Verification", "", f"{total - len(bad)} of {total} planned items are ok.", ""]
    for state, title in ((lib.FAILED, "Checks that failed"), (lib.ERROR, "Steps that could not be carried out"),
                         (lib.UNSUPPORTED, "Comparisons that could not be made to work"),
                         (lib.INCOMPLETE, "Incomplete measurements"), ("missing", "Never reported")):
        found = [b for b in bad if b[2] == state]
        if found:
            lines += [f"### {title}", ""]
            lines += [f"- {r.label()} {item}: {detail[:400]}".replace("\n", " ") for r, item, _, detail in found]
            lines.append("")

    if any(p in plan["phases"] for p in ("D", "E")):
        lines += ["", "## What each fork method guarantees", ""]
        shown = set(plan["methods"]) | ({"reflink"} if "floor" in plan["cases"] else set())
        lines += [f"- **{name}**: {text}" for name, text in GUARANTEES if name in shown]
        if "D" in plan["phases"] and "busy" in plan["fork_modes"]:
            lines.append("- In D's busy rows the memory and space figures include what the background writer "
                         "itself caused.")
        lines.append("- First reads follow a fresh client mount (overlay-shared on CephFS: remounted before the "
                     "fork). Cold reads in E drop the server files' page cache and, for Ceph, OSD and MDS caches; "
                     "atlas-native's catalog stays in memory.")
    for phase in "ABCDEFGS":
        lines += phase_table(results, phase)
    lines += bet2_section(results)
    lines += bet1_section(results)

    notes = {}
    for r in results.runs:
        for item in r.expected:
            for note in (r.records.get(item) or {}).get("notes", []):
                notes.setdefault((r.case, note), []).append(item)
    if notes:
        lines += ["", "## Notes from the runs", ""]
        lines += [f"- {case}: {note} ({', '.join(sorted(set(items))[:6])}{' ...' if len(set(items)) > 6 else ''})"
                  for (case, note), items in sorted(notes.items())]

    lines += ["", "## Host load and I/O pressure", "",
              "| run | commit | start load | end load | io pressure at start | io pressure at end |",
              "|---|---|---|---|---|---|"]
    for r in results.runs:
        e = r.env
        lines.append("| " + " | ".join(
            [r.label(), e.get("commit", "?")[:14]]
            + [e.get(k, "?").split(" ")[0] for k in ("loadavg", "loadavg_end")]
            + [e.get(k, "?").split(";")[0].replace("some ", "") for k in ("io_pressure", "io_pressure_end")]
        ) + " |")
    lines += ["", "Per item: 1 minute load / io pressure (some, avg10), at its start and at its end.", "",
              "| run | item | state | start | end |", "|---|---|---|---|---|"]
    for r in results.runs:
        for item in r.expected:
            record = r.records.get(item) or {}
            lines.append(f"| {r.label()} | {item} | {r.state(item)[0]} | {conditions_text(record.get('start'))} | "
                         f"{conditions_text(record.get('end'))} |")
    return verdict, "\n".join(lines) + "\n"


def main(argv):
    if len(argv) != 1:
        print(__doc__, file=sys.stderr)
        return 2
    path = Path(argv[0]).resolve()
    try:
        plan = json.loads(path.read_text())
        problems = lib.check_plan(plan)
        if problems:
            raise ValueError("; ".join(problems))
        verdict, text = report(plan, path.parent.parent)
    except Exception as e:  # an input nobody foresaw must not look like a pass (or a plain failure)
        print(f"VERDICT: {UNKNOWN} (not a pass): agent_report.py could not judge these inputs: {e!r}")
        return EXIT[UNKNOWN]
    print(text, end="")
    return EXIT[verdict]


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
