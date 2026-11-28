#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
"""Tests of the agent profile's parsing, fixtures, runner logic and judging. No storage system.

Usage: python3 -I test_agent.py
The fixture tests need git. With AGENT_FLOOR_TEST=1 the floor is also run for real in TMPDIR (it
needs a filesystem with reflink, and syncs that filesystem).
"""

import contextlib
import io
import json
import math
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.dont_write_bytecode = True
import agent_fixtures  # noqa: E402
import agent_lib as lib  # noqa: E402
import agent_report  # noqa: E402
import agent_run  # noqa: E402

SMALL = {"seed": 1, "files": 40, "repo_files": 45, "commits": 3, "fork_sizes": [5, 25], "fork_file_bytes": 64}
MIB = 1024 * 1024


class Temp(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)


class ParserTest(unittest.TestCase):
    def test_loadavg_and_pressure(self):
        self.assertEqual(lib.parse_loadavg("1.50 2.00 3.25 4/500 12\n"), [1.5, 2.0, 3.25])
        p = lib.parse_pressure("some avg10=1.20 avg60=2.00 avg300=0.00 total=99\nfull avg10=0.50 avg60=0 avg300=0 total=1\n")
        self.assertEqual(p["some"]["avg10"], 1.2)
        self.assertEqual(p["full"]["total"], 1.0)
        with self.assertRaises(ValueError):
            lib.parse_pressure("")

    def test_diskstats(self):
        line = "   8      16 sdb 10 0 800 5 20 3 1600 9 0 7 14 0 0 0 0 33 2\n"
        got = lib.parse_diskstats("   8 0 sda 1 1 1 1 1 1 1 1 1 1 1\n" + line, "sdb")
        self.assertEqual(got, {"reads": 10, "sectors_read": 800, "writes": 20, "sectors_written": 1600, "flushes": 33})
        self.assertIsNone(lib.parse_diskstats(line, "sdc"))
        # An old kernel without flush counters: unknown, not zero.
        self.assertIsNone(lib.parse_diskstats("8 0 sdb 1 0 8 1 2 0 16 1 0 1 1", "sdb")["flushes"])

    def test_process_status(self):
        text = "Name:\tx\nVmHWM:\t  2048 kB\nVmRSS:\t  1024 kB\n"
        self.assertEqual(lib.parse_status_kb(text, "VmRSS"), MIB)
        self.assertEqual(lib.parse_status_kb(text, "VmHWM"), 2 * MIB)
        self.assertIsNone(lib.parse_status_kb(text, "VmPeak"))

    def test_metrics_and_sync_delta(self):
        def scrape(count, total):
            return lib.parse_metrics(
                "# HELP x\n"
                f'atlas_native_data_sync_seconds_count{{node="d1",device="0"}} {count}\n'
                f'atlas_native_data_sync_seconds_count{{node="d1",device="1"}} 1\n'
                f'atlas_native_data_sync_seconds_sum{{node="d1",device="0"}} {total}\n'
                'atlas_native_data_sync_seconds_bucket{le="+Inf"} 7\n'
                "garbage line\n"
            )

        a, b = scrape(10, 0.5), scrape(30, 1.5)
        self.assertEqual(lib.metric_sum(a, "atlas_native_data_sync_seconds_count"), 11)
        self.assertIsNone(lib.metric_sum(a, "absent"))
        self.assertEqual(lib.sync_delta(a, b, "atlas_native_data_sync_seconds"), (20, 1.0))
        self.assertIsNone(lib.sync_delta(b, a, "atlas_native_data_sync_seconds"), "a restart")
        self.assertIsNone(lib.sync_delta([], b, "atlas_native_data_sync_seconds"))

    def test_settings(self):
        text = 'extent_bytes=1048576 (verified through GET /v1/fs)\nmount_args=--fuse-threads 32 --retry-secs 60 \n{"replicas":3}\n'
        self.assertEqual(lib.parse_mount_args(text), ["--fuse-threads", "32", "--retry-secs", "60"])
        self.assertEqual(lib.setting_extent_bytes(text), 1048576)
        with self.assertRaises(ValueError):
            lib.parse_mount_args("{}")

    def test_leader(self):
        self.assertTrue(lib.is_leader({"metadata": {"role": "leader"}}))
        self.assertTrue(lib.is_leader({"metadata_groups": [{"role": "follower"}, {"role": "leader"}]}))
        self.assertFalse(lib.is_leader({"metadata": None, "metadata_groups": None}))

    def test_ceph(self):
        self.assertEqual(lib.clone_state({"status": {"state": "in-progress"}}), "in-progress")
        for doc in ({}, {"status": {"state": "done"}}):
            with self.assertRaises(ValueError):
                lib.clone_state(doc)
        doc = {"pools": [{"name": "cephfs_metadata", "stats": {"bytes_used": 5}},
                         {"name": "cephfs_data", "stats": {"bytes_used": 10}}]}
        self.assertEqual(lib.parse_ceph_df(doc), (15, 5))
        with self.assertRaises(KeyError):
            lib.parse_ceph_df({"pools": []})

    def fio(self, **over):
        job = {"error": 0, "write": {"total_ios": 64, "io_bytes": 64 * 4096, "iops": 4.5,
                                     "clat_ns": {"percentile": {"99.000000": 2e9}}}}
        job.update(over)
        return "warning\n" + json.dumps({"jobs": [job]})

    def test_smallfile(self):
        got = lib.smallfile_figures(lib.parse_fio_job(self.fio()), 64)
        self.assertEqual(got, {"files_per_s": 4.5, "p99_ms": 2000.0})
        with self.assertRaises(lib.VerifyError):
            lib.smallfile_figures(lib.parse_fio_job(self.fio()), 128)
        for text in (self.fio(error=5), "", "{}", '{"jobs": []}'):
            with self.assertRaises(lib.VerifyError):
                lib.parse_fio_job(text)
        with self.assertRaises(lib.VerifyError):
            lib.smallfile_figures({"write": {"total_ios": 1, "io_bytes": 4096, "iops": 1, "clat_ns": {}}}, 1)

    def test_events_skip_truncated_lines(self):
        text = "noise\n" + lib.format_event({"type": "item", "id": "A"}) + "\n" + lib.EVENT_PREFIX + '{"type": "it'
        self.assertEqual(lib.parse_events(text), [{"type": "item", "id": "A"}])


class StatisticsTest(unittest.TestCase):
    def test_percentile_is_a_member(self):
        self.assertIsNone(lib.percentile([], 99))
        self.assertEqual(lib.percentile([5], 99), 5)
        self.assertEqual(lib.percentile(list(range(1, 101)), 99), 99)
        self.assertEqual(lib.percentile([3, 1, 2], 50), 2)

    def test_writer_windows(self):
        # Steady 10 ms operations, then one held up from just before the fork to after it.
        ops = [(i * 0.1, i * 0.1 + 0.01) for i in range(100)] + [(9.95, 13.0), (13.0, 13.01)]
        got = agent_run.writer_figures(ops, start=0.0, fork_start=10.0, fork_end=12.0, stop=14.0)
        self.assertEqual(got["bg_fork_ops"], 1)
        self.assertAlmostEqual(got["bg_fork_p99_ms"], 3050.0, places=3)
        self.assertAlmostEqual(got["bg_base_p99_ms"], 10.0, places=3)
        self.assertEqual(got["bg_base_ops"], 90, "the first second and the held-up operation are not baseline")
        self.assertEqual(got["bg_after_ops"], 2)

    def test_writer_windows_without_operations_report_no_latency(self):
        got = agent_run.writer_figures([], 0.0, 10.0, 11.0, 12.0)
        self.assertEqual(got, {"bg_base_ops": 0, "bg_fork_ops": 0, "bg_after_ops": 0})

    def stable(self, values, **kw):
        now = [0.0]
        queue = list(values)

        def sample():
            return queue.pop(0) if len(queue) > 1 else queue[0]

        def sleep(s):
            now[0] += s

        return lib.wait_stable(sample, lambda a, b: a == b, clock=lambda: now[0], sleep=sleep, **kw)

    def test_wait_stable(self):
        self.assertEqual(self.stable([9, 7, 5, 5], quiet_s=15, timeout_s=100, interval_s=5), (5, 10.0, True))

    def test_wait_stable_respects_minimum(self):
        # A change that comes only after a while (garbage collection) is seen.
        got = self.stable([9, 9, 9, 9, 9, 9, 3], quiet_s=10, timeout_s=200, interval_s=5, min_s=60)
        self.assertEqual(got, (3, 30.0, True))

    def test_wait_stable_times_out(self):
        value, _, ok = self.stable(list(range(100)), quiet_s=15, timeout_s=50, interval_s=5)
        self.assertFalse(ok)


class TreeTest(Temp):
    def make(self):
        (self.root / "d").mkdir()
        (self.root / "d" / "a").write_bytes(b"a")
        (self.root / "b").write_bytes(b"b")
        return {"d/a": lib.sha256_bytes(b"a"), "b": lib.sha256_bytes(b"b")}

    def test_read_tree(self):
        expected = self.make()
        self.assertEqual(len(lib.read_tree(str(self.root), expected)), 2)
        self.assertEqual(lib.hash_tree(str(self.root)), expected)

    def test_read_tree_failures(self):
        expected = self.make()
        (self.root / "b").write_bytes(b"x")
        with self.assertRaisesRegex(lib.VerifyError, "differ"):
            lib.read_tree(str(self.root), expected)
        (self.root / "b").unlink()
        with self.assertRaisesRegex(lib.VerifyError, "1 files missing"):
            lib.read_tree(str(self.root), expected)
        (self.root / "b").write_bytes(b"b")
        (self.root / "extra").write_bytes(b"")
        with self.assertRaisesRegex(lib.VerifyError, "1 extra"):
            lib.read_tree(str(self.root), expected)

    def test_skip_and_missing_root(self):
        expected = self.make()
        (self.root / "bg").mkdir()
        (self.root / "bg" / "w0").write_bytes(b"")
        lib.read_tree(str(self.root), expected, skip=("bg",))
        with self.assertRaises(OSError):
            lib.walk_files(str(self.root / "absent"))

    def test_manifest(self):
        files = {"b": "0" * 64, "a/c d": "1" * 64}
        self.assertEqual(lib.parse_manifest(lib.format_manifest(files)), files)
        for bad in ("", "xyz  a\n", "0" * 64 + "  a\n" + "0" * 64 + "  a\n"):
            with self.assertRaises(ValueError):
                lib.parse_manifest(bad)

    def test_edits(self):
        paths = ["b", "src/.git/config", "a", "B", "src/x"]
        self.assertEqual(lib.edit_targets(paths, 3), ["B", "a", "b"])
        with self.assertRaises(ValueError):
            lib.edit_targets(paths, 5)
        out = lib.edited_manifest({"a": "0" * 64, "b": "1" * 64}, {"a": b"x"}, "c1")
        self.assertEqual(out, {"a": lib.sha256_bytes(b"xedit c1\n"), "b": "1" * 64})


@unittest.skipUnless(shutil.which("git"), "git is not installed")
class FixtureTest(Temp):
    @classmethod
    def setUpClass(cls):
        cls.shared = tempfile.TemporaryDirectory()
        cls.dir = Path(cls.shared.name) / "fixtures"
        cls.info = agent_fixtures.generate(cls.dir, SMALL)

    @classmethod
    def tearDownClass(cls):
        cls.shared.cleanup()

    def test_rerun_gives_the_same_hashes(self):
        again = agent_fixtures.generate(self.root / "again", SMALL)
        self.assertEqual(again["files"], self.info["files"])
        self.assertEqual(again["head"], self.info["head"])
        self.assertEqual(agent_fixtures.check(self.root / "again", SMALL), [])

    def test_other_seed_differs(self):
        other = agent_fixtures.generate(self.root / "other", dict(SMALL, seed=2))
        self.assertNotEqual(other["files"]["pkgs.tar"], self.info["files"]["pkgs.tar"])
        self.assertNotEqual(other["head"], self.info["head"])

    def test_check_finds_damage_and_other_parameters(self):
        copy = self.root / "copy"
        shutil.copytree(self.dir, copy)
        self.assertEqual(agent_fixtures.check(copy, SMALL), [])
        self.assertTrue(agent_fixtures.check(copy, dict(SMALL, files=41)))
        with open(copy / "pkgs.tar", "ab") as f:
            f.write(b"x")
        self.assertEqual(agent_fixtures.check(copy, SMALL), ["pkgs.tar: differs"])
        (copy / "manifest.sha256").unlink()
        self.assertIn("manifest.sha256: missing", agent_fixtures.check(copy, SMALL))
        self.assertTrue(agent_fixtures.check(self.root / "absent", SMALL))

    def test_generate_refuses_an_existing_directory(self):
        with self.assertRaises(SystemExit):
            agent_fixtures.generate(self.dir, SMALL)

    def test_shares_and_paths(self):
        sizes = agent_fixtures.pkg_sizes(1, 2000)
        counts = [sum(lo <= s <= hi for s in sizes.values()) for lo, hi in agent_fixtures.CLASSES]
        self.assertEqual(counts, [1400, 500, 100])
        self.assertEqual(list(sizes)[:1] + list(sizes)[20:21] + list(sizes)[-1:],
                         ["d000/f00000", "d001/f00020", "d099/f01999"])

    def test_tars_match_their_manifests(self):
        for tar, manifest, count in (("pkgs.tar", "manifest-pkgs.sha256", 40), ("base-5.tar", "manifest-base-5.sha256", 5),
                                     ("base-25.tar", "manifest-base-25.sha256", 25)):
            dirs, files = lib.tar_members(self.dir / tar)
            expected = lib.parse_manifest((self.dir / manifest).read_text())
            self.assertEqual({p: lib.sha256_bytes(d) for p, d in files}, expected)
            self.assertEqual(len(files), count)
            self.assertTrue(dirs)
        self.assertEqual({len(d) for _, d in lib.tar_members(self.dir / "base-25.tar")[1]}, {64})

    def runner(self):
        r = object.__new__(agent_run.Runner)
        r.fixtures, r.info, r.cfg = self.dir, self.info, dict(agent_run.DEFAULTS)
        return r

    def test_phase_b_produces_the_manifest(self):
        """The runner's tar, build_tree and git steps give exactly what manifest.sha256 says."""
        ws = self.root / "ws"
        ws.mkdir()
        agent_run.build_tree(self.dir / "pkgs.tar", str(ws), 4)
        record = {"metrics": {}}
        self.runner().git_steps(str(ws), record)
        self.assertEqual(sorted(record["metrics"]), ["clone_s", "commit_s", "edit_s", "fsck_s", "status_s"])
        manifest = lib.parse_manifest((self.dir / "manifest.sha256").read_text())
        for path, digest in manifest.items():
            self.assertEqual(lib.sha256_file(ws / path), digest, path)
        self.assertEqual(len(manifest), 40 + 45)

    def test_phase_b_detects_a_wrong_repository(self):
        runner = self.runner()
        runner.info = dict(self.info, tree_after_edits="0" * 40)
        with self.assertRaisesRegex(lib.VerifyError, "committed tree"):
            runner.git_steps(str(self.root), {"metrics": {}})


def plan_for(profile="agent", **env):
    return lib.build_plan(profile, "t1", {k: str(v) for k, v in env.items()})


class PlanTest(Temp):
    def test_defaults(self):
        plan = plan_for()
        self.assertEqual(plan["phases"], ["A", "B", "C", "E", "F", "G"])
        self.assertEqual((plan["rounds"], plan["tmpfs_gb"], plan["mem"]), (3, 0, "12g"))
        self.assertEqual([r["case"] for r in plan["runs"][:4]], ["ceph", "atlas", "floor", "ceph"])
        fork = plan_for("fork")
        self.assertEqual((fork["phases"], fork["tmpfs_gb"], fork["mem"]), (["D"], 10, "20g"))
        self.assertEqual(fork["case_env"]["atlas"], {"ATLAS_PROPOSAL_TIMEOUT_MS": "120000"})
        small = plan_for("smallfile", CASES="ceph atlas floor")
        self.assertEqual(small["cases"], ["ceph", "atlas"], "the floor has nothing to run")

    def test_items(self):
        plan = plan_for()
        atlas = lib.expected_items(plan, "atlas")
        self.assertEqual(atlas[:4], ["A", "C/after-kill", "B", "C/final"])
        self.assertEqual(len([i for i in atlas if i.startswith("E/")]), 9)
        floor = lib.expected_items(plan, "floor")
        self.assertEqual(floor, ["E/1/reflink", "E/4/reflink", "E/16/reflink", "F"])
        fork = plan_for("fork")
        self.assertEqual([s[0] for s in lib.segments(fork, "atlas")], ["d1000", "d10000", "d100000"])
        self.assertEqual(len(lib.segment_items(fork, "ceph", ["D"], [1000])), 6)
        self.assertEqual(lib.expected_items(fork, "floor", [1000]), ["D/1000/quiet/reflink"])
        some = plan_for("fork", AGENT_METHODS="clone")
        self.assertEqual(lib.expected_items(some, "atlas", [1000]), ["D/1000/quiet/clone", "D/1000/busy/clone"])
        for item in atlas + floor + lib.expected_items(fork, "ceph"):
            lib.required_metrics(item, "atlas")

    def test_order_alternates(self):
        items = ["D/5/quiet/clone", "D/5/quiet/overlay", "D/5/busy/clone", "D/5/busy/overlay"]
        self.assertEqual(lib.order_items(items, 1), items)
        self.assertEqual(lib.order_items(items, 2),
                         ["D/5/quiet/overlay", "D/5/quiet/clone", "D/5/busy/overlay", "D/5/busy/clone"])
        items = ["A", "E/1/clone", "E/4/clone", "E/1/overlay", "E/4/overlay", "F"]
        self.assertEqual(lib.order_items(items, 2), ["A", "E/1/overlay", "E/4/overlay", "E/1/clone", "E/4/clone", "F"])
        self.assertEqual(lib.order_items(["S/w1", "S/w8", "S/w32"], 2), ["S/w32", "S/w8", "S/w1"])

    def test_refusals(self):
        for env in (
            {"AGENT_PHASES": "B"}, {"AGENT_PHASES": "A C"}, {"AGENT_PHASES": "F"}, {"AGENT_PHASES": "Z"},
            {"ROUNDS": "0"}, {"ROUNDS": "x"}, {"AGENT_FANOUT": "0"}, {"AGENT_FANOUT": "4 4"},
            {"AGENT_METHODS": "snapshot"}, {"CASES": "zfs"}, {"CASES": "ceph ceph"}, {"CASES": "atlas-x"},
        ):
            with self.assertRaises((ValueError, OSError), msg=env):
                plan_for(**env)
        with self.assertRaises(ValueError):
            lib.build_plan("agent", "../x", {})
        with self.assertRaises(ValueError):
            lib.build_plan("full", "t", {})

    def test_variants(self):
        (self.root / "x.env").write_text('# feature\nBENCH_ATLAS_SRC=/data/s\nATLAS_METADATA_EXTRA="a": 1, "b": true\n')
        plan = plan_for("smallfile", CASES="ceph atlas atlas-x", AGENT_VARIANTS=self.root)
        self.assertEqual(plan["case_env"]["atlas-x"],
                         {"BENCH_ATLAS_SRC": "/data/s", "ATLAS_METADATA_EXTRA": '"a": 1, "b": true'})
        self.assertEqual(plan["case_env"]["atlas"], {})
        for bad in ("PATH=/x", "ATLAS_A=1\nATLAS_A=2", "ATLAS_A", "BENCH_ROOT=/"):
            with self.assertRaises(ValueError):
                lib.parse_variant(bad)

    def test_command_line(self):
        plan = plan_for("fork")
        path = self.root / "plan.json"
        path.write_text(json.dumps(plan))
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            lib.cli(["segments", str(path), "ceph"])
            lib.cli(["env", str(path), "atlas"])
        self.assertEqual(out.getvalue().splitlines()[0], "d1000 D 1000")
        self.assertIn("ATLAS_PROPOSAL_TIMEOUT_MS=120000", out.getvalue())
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            lib.cli(["config", str(path), "ceph", "2", "D", "1000", "/fixtures", "sdb", "/unused"])
        config = json.loads(out.getvalue())
        self.assertEqual((config["round"], config["sizes"], config["phases"]), (2, [1000], ["D"]))

    def test_fixture_arguments_round_trip(self):
        plan = plan_for()
        _, _, params = agent_fixtures.parse_args(["ensure", "d"] + lib.fixture_args(plan["fixtures"]))
        self.assertEqual(params, plan["fixtures"])


class ItemStateTest(unittest.TestCase):
    def test_ok_needs_every_figure(self):
        good = {"status": "ok", "metrics": {"write_mibs": 1.0, "read_mibs": 2.0}}
        self.assertEqual(lib.item_state(good, "G", "atlas")[0], "ok")
        for metrics in ({"write_mibs": 1.0}, {"write_mibs": 1.0, "read_mibs": math.nan},
                        {"write_mibs": 1.0, "read_mibs": None}, {"write_mibs": 1.0, "read_mibs": True}, {}):
            self.assertEqual(lib.item_state({"status": "ok", "metrics": metrics}, "G", "atlas")[0], "incomplete")
        self.assertEqual(lib.item_state(None, "G", "atlas")[0], "missing")
        self.assertEqual(lib.item_state({"status": "great"}, "G", "atlas")[0], "incomplete")
        self.assertEqual(lib.item_state({"status": "failed"}, "G", "atlas")[0], "failed")

    def test_reads_from_tmpfs_are_never_called_cold(self):
        class System:
            def evict_caches(self):
                return []

        r = object.__new__(agent_run.Runner)
        r.system, r.floor, r.plan = System(), False, {"tmpfs_gb": 0}
        self.assertEqual(r.drop_server_caches(), [])
        r.plan = {"tmpfs_gb": 4}
        self.assertIn("tmpfs", r.drop_server_caches()[0])
        r.floor = True  # the floor stays on the host's disk
        self.assertEqual(r.drop_server_caches(), [])

    def test_runner_classifies_what_a_step_ends_in(self):
        class System:
            retries = 0

        cases = [(None, "ok"), (lib.VerifyError("x"), "failed"), (lib.Unsupported("x"), "unsupported"),
                 (lib.Incomplete("x"), "incomplete"), (RuntimeError("x"), "error"), (OSError("x"), "error")]
        for raised, status in cases:
            r = object.__new__(agent_run.Runner)
            r.system, r.floor, r.done = System(), False, {}

            def dispatch(item, record, raised=raised):
                if raised:
                    raise raised

            r.dispatch = dispatch
            out = io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
                r.run_item("G")
            event = lib.parse_events(out.getvalue())[0]
            self.assertEqual(event["status"], status, raised)
            self.assertEqual(r.done, {"G": status})
            if status != "ok":
                with self.assertRaises(lib.Incomplete):
                    r.need("G")


# ---------------------------------------------------------------- the report


def good_metrics(item, system):
    m = {name: 1.0 for name in lib.required_metrics(item, system)}
    if item.startswith("D/"):
        m.update(fork_s=0.2, rss_growth_bytes=MIB, elections=0, bg_base_p99_ms=100.0, bg_base_max_ms=150.0,
                 bg_base_ops=500, bg_fork_p99_ms=110.0, bg_fork_max_ms=110.0, bg_fork_ops=5,
                 bg_after_p99_ms=120.0, bg_after_max_ms=130.0, bg_after_ops=200)
    return m


class ReportBase(Temp):
    def write(self, plan, change=None, env_change=None, order=None):
        """Result sets for every run of `plan`. change(run, item, record) may edit or drop (return
        None) a record; env_change(run, env) may edit env.txt."""
        results = self.root / "results"
        plandir = results / f"{plan['tag']}-agent"
        plandir.mkdir(parents=True, exist_ok=True)
        (plandir / "plan.json").write_text(json.dumps(plan))
        runs = list(plan["runs"])
        if order:
            runs = order(runs)
        for minute, run in enumerate(runs):
            d = results / run["dir"] / "agent"
            d.mkdir(parents=True, exist_ok=True)
            env = {
                "date": f"2026-11-28T{minute // 60:02d}:{minute % 60:02d}:00+01:00", "kernel": "k", "disk": "sdb",
                "system": run["system"], "case": run["case"], "session": plan["tag"], "round": run["round"],
                "commit": "abc", "harness_sha256": "h", "fixtures_head": "f", "mem": "12g", "cpus": "10",
                "tmpfs_gb": "0", "node_binary": "n", "loadavg": "1.0 1 1 1/1 1",
                "io_pressure": "some avg10=0.00 avg60=0.00 avg300=0.00 total=0;full avg10=0.00;",
                "loadavg_end": "2.0 1 1 1/1 1", "io_pressure_end": "some avg10=1.00;", "end": "x",
            }
            for name, *_ in lib.segments(plan, run["system"]):
                env[f"image_{name}"] = "img"
                env[f"segment_{name}"] = "ok"
            if env_change:
                env_change(run, env)
            (d / "env.txt").write_text("".join(f"{k}={v}\n" for k, v in env.items()))
            for name, phases, sizes in lib.segments(plan, run["system"]):
                lines = [lib.format_event({"type": "start"})]
                for item in lib.segment_items(plan, run["system"], phases, sizes):
                    record = {"type": "item", "id": item, "status": "ok", "notes": ["a note"],
                              "metrics": good_metrics(item, run["system"])}
                    if change:
                        record = change(run, item, record)
                    if record is not None:
                        lines.append(lib.format_event(record))
                lines.append(lib.format_event({"type": "end", "complete": True}))
                (d / f"{name}.jsonl").write_text("\n".join(lines) + "\n")
                (d / f"settings-{name}.txt").write_text("settings\n")
        return plandir / "plan.json"

    def judge(self, path):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = agent_report.main([str(path)])
        return code, out.getvalue()

    def assert_verdict(self, expected, path, *fragments):
        code, text = self.judge(path)
        self.assertIn(f"VERDICT: {expected}", text, text[:3000])
        self.assertEqual(code, agent_report.EXIT[expected])
        for fragment in fragments:
            self.assertIn(fragment, text)
        return text


class ReportTest(ReportBase):
    def test_pass(self):
        text = self.assert_verdict("PASS", self.write(plan_for()), "## E. Fan-out", "**overlay-shared**",
                                   "- no problem found", "## Host load and I/O pressure")
        self.assertNotIn("Research bet", text)

    def one(self, run_case, run_round, item, edit):
        def change(run, i, record):
            if run["case"] == run_case and run["round"] == run_round and i == item:
                return edit(record)
            return record

        return change

    def test_failed_verification_fails_and_is_not_averaged(self):
        change = self.one("atlas", 2, "G", lambda r: dict(r, status="failed", detail="large files differ"))
        text = self.assert_verdict("FAIL", self.write(plan_for(), change), "large files differ")
        self.assertIn("| G | atlas | 2/3 | FAILED (1/3) | FAILED (1/3) |", text)

    def test_error_fails(self):
        change = self.one("ceph", 1, "A", lambda r: dict(r, status="error"))
        self.assert_verdict("FAIL", self.write(plan_for(), change))

    def test_unsupported_is_unknown(self):
        change = self.one("ceph", 1, "E/4/overlay", lambda r: dict(r, status="unsupported", detail="refused"))
        self.assert_verdict("UNKNOWN", self.write(plan_for(), change), "could not be made to work", "unsupported (1/3)")

    def test_missing_item_is_unknown(self):
        change = self.one("floor", 3, "F", lambda r: None)
        self.assert_verdict("UNKNOWN", self.write(plan_for(), change), "Never reported", "missing (1/3)")

    def test_missing_figure_is_unknown(self):
        def drop(r):
            del r["metrics"]["durable_s"]
            return r

        self.assert_verdict("UNKNOWN", self.write(plan_for(), self.one("atlas", 1, "A", drop)), "durable_s")

    def test_missing_run_is_unknown(self):
        plan = plan_for()
        path = self.write(plan)
        shutil.rmtree(self.root / "results" / plan["runs"][4]["dir"])
        self.assert_verdict("UNKNOWN", path, "no result directory")

    def test_unfinished_segment_is_unknown(self):
        plan = plan_for()
        path = self.write(plan)
        f = self.root / "results" / plan["runs"][0]["dir"] / "agent" / "main.jsonl"
        f.write_text("\n".join(f.read_text().splitlines()[:-1]) + "\n")
        self.assert_verdict("UNKNOWN", path, "did not complete")

    def test_item_reported_twice_is_unknown(self):
        plan = plan_for()
        path = self.write(plan)
        f = self.root / "results" / plan["runs"][0]["dir"] / "agent" / "main.jsonl"
        lines = f.read_text().splitlines()
        f.write_text("\n".join(lines + [lines[1]]) + "\n")
        self.assert_verdict("UNKNOWN", path, "reported twice")

    def test_too_few_rounds_is_unknown(self):
        self.assert_verdict("UNKNOWN", self.write(plan_for(ROUNDS=2)), "a result needs 3")

    def test_not_alternating_is_unknown(self):
        path = self.write(plan_for(), order=lambda runs: sorted(runs, key=lambda r: r["case"]))
        self.assert_verdict("UNKNOWN", path, "did not alternate")

    def test_differing_builds_or_settings_are_unknown(self):
        for key, case in (("node_binary", "atlas"), ("commit", "atlas"), ("harness_sha256", "ceph"),
                          ("mem", "ceph"), ("image_main", "ceph"), ("fixtures_head", "floor")):
            def env_change(run, env, key=key, case=case):
                if run["case"] == case and run["round"] == 2:
                    env[key] = "other"

            shutil.rmtree(self.root / "results", ignore_errors=True)
            self.assert_verdict("UNKNOWN", self.write(plan_for(), env_change=env_change), "differs")

    def test_unreadable_plan_is_unknown(self):
        path = self.root / "plan.json"
        path.write_text("{}")
        code, text = self.judge(path)
        self.assertEqual(code, 3)
        self.assertIn("could not judge", text)


class Bet2Test(ReportBase):
    def bet(self, edit=None, **env):
        def change(run, item, record):
            if edit and run["case"] == "atlas" and item.endswith("/clone"):
                _, n, mode, _ = item.split("/")
                edit(int(n), mode, run["round"], record)
            return record

        plan = plan_for("fork", **env)
        path = self.write(plan, change, env_change=lambda r, e: e.update(tmpfs_gb="10"))
        code, text = self.judge(path)
        section = text.split("## Research bet 2")[1].split("\n## ")[0]
        return code, section

    def test_met(self):
        code, section = self.bet()
        self.assertEqual(code, 0, section)
        self.assertIn("### atlas: yes", section)
        self.assertIn("tmpfs", section)

    def check(self, expected, edit, fragment):
        _, section = self.bet(edit)
        self.assertIn(f"### atlas: {expected}", section, section)
        self.assertIn(fragment, section)

    def test_too_slow_at_the_large_size(self):
        def edit(n, mode, round_, r):
            if n == 100000 and round_ == 2:
                r["metrics"]["fork_s"] = 1.0

        self.check("no", edit, "[FAIL] quiet: snapshot + clone at 100000")

    def test_time_grows(self):
        def edit(n, mode, round_, r):
            if n == 1000:
                r["metrics"]["fork_s"] = 0.05

        self.check("no", edit, "[FAIL] quiet: median time within 2x")

    def test_memory_noise_floor(self):
        def edit(n, mode, round_, r):
            r["metrics"]["rss_growth_bytes"] = 0 if n == 1000 else 15 * MIB

        self.check("yes", edit, "[PASS] quiet: metadata memory growth")

        def edit(n, mode, round_, r):
            r["metrics"]["rss_growth_bytes"] = 0 if n == 1000 else 17 * MIB

        shutil.rmtree(self.root / "results")
        self.check("no", edit, "[FAIL] quiet: metadata memory growth")

    def test_one_election_fails(self):
        def edit(n, mode, round_, r):
            if n == 10000 and round_ == 3:
                r["metrics"]["elections"] = 1

        self.check("no", edit, "[FAIL] no election")

    def busy(self, **figures):
        def edit(n, mode, round_, r):
            if mode == "busy" and n == 1000 and round_ == 1:
                r["metrics"].update(figures)

        shutil.rmtree(self.root / "results", ignore_errors=True)
        return edit

    def test_doubled_p99_with_enough_operations_fails(self):
        for window in ("fork", "after"):
            edit = self.busy(**{f"bg_{window}_p99_ms": 201.0, f"bg_{window}_ops": 100})
            self.check("no", edit, "[FAIL] foreground p99 at 1000")

    def test_a_stall_beyond_anything_before_fails_even_with_few_operations(self):
        edit = self.busy(bg_fork_p99_ms=301.0, bg_fork_max_ms=301.0, bg_fork_ops=1)
        self.check("no", edit, "[FAIL] foreground p99 at 1000")

    def test_few_operations_over_2x_but_seen_before_is_unknown(self):
        edit = self.busy(bg_fork_p99_ms=250.0, bg_fork_max_ms=250.0, bg_fork_ops=9)
        self.check("unknown", edit, "[UNKNOWN] foreground p99 at 1000")

    def test_memory_with_a_writer_is_not_judged(self):
        def edit(n, mode, round_, r):
            if mode == "busy" and n == 100000:
                r["metrics"]["rss_growth_bytes"] = 900 * MIB

        self.check("yes", edit, "[PASS] quiet: metadata memory growth")

    def test_failed_item_is_unknown_not_met(self):
        def edit(n, mode, round_, r):
            if n == 100000 and round_ == 1:
                r["status"] = "error"

        self.check("unknown", edit, "ERROR (1/3)")

    def test_no_after_figure_cannot_pass(self):
        def edit(n, mode, round_, r):
            r["metrics"].pop("bg_after_p99_ms", None)

        self.check("unknown", edit, "no figure for the time after")

    def test_sizes_missing(self):
        _, section = self.bet(AGENT_FORK_SIZES="1000 10000")
        self.assertIn("### atlas: unknown", section)

    def test_quiet_only_is_unknown(self):
        _, section = self.bet(AGENT_FORK_MODES="quiet")
        self.assertIn("### atlas: unknown", section)

    def test_short_session_cannot_say_yes(self):
        _, section = self.bet(ROUNDS=1)
        self.assertIn("### atlas: unknown", section)
        self.assertNotIn("[PASS]", section)


class Bet1Test(ReportBase):
    def bet(self, rate, drop=False):
        (self.root / "x.env").write_text('ATLAS_METADATA_EXTRA="new": true\n')
        plan = plan_for("smallfile", CASES="ceph atlas atlas-x", AGENT_VARIANTS=self.root)

        def change(run, item, record):
            base = {"S/w1": 3.0, "S/w8": 4.0, "S/w32": 5.0}[item]
            if run["case"] == "atlas-x" and item == "S/w8":
                base = rate
                if drop and run["round"] == 1:
                    return None
            record["metrics"]["files_per_s"] = base
            return record

        return self.judge(self.write(plan, change))

    def test_three_times_meets_the_claim(self):
        code, text = self.bet(12.0)
        self.assertEqual(code, 0, text)
        self.assertIn("[PASS] atlas-x: 3.00x the rate of atlas", text)
        self.assertIn("| atlas | 3 | 4 | 5 | x1.33 | x1.67 |", text)

    def test_less_does_not(self):
        code, text = self.bet(11.9)
        self.assertEqual(code, 0, "the measurements are complete")
        self.assertIn("[FAIL] atlas-x: 2.98x", text)

    def test_missing_round_is_unknown(self):
        code, text = self.bet(20.0, drop=True)
        self.assertEqual(code, 3)
        self.assertIn("[UNKNOWN] atlas-x against atlas", text)


@unittest.skipUnless(os.environ.get("AGENT_FLOOR_TEST") == "1", "set AGENT_FLOOR_TEST=1 on a reflink filesystem")
@unittest.skipUnless(shutil.which("git"), "git is not installed")
class FloorRunTest(Temp):
    def test_floor_end_to_end(self):
        env = {"AGENT_PHASES": "A B C D E F", "CASES": "floor", "AGENT_FORK_SIZES": "25", "AGENT_FANOUT": "1 3",
               "AGENT_FILES": "40", "AGENT_REPO_FILES": "45", "AGENT_COMMITS": "3", "AGENT_FORK_FILE_BYTES": "64",
               "AGENT_SETTLE_SECS": "0", "AGENT_BG_AFTER_SECS": "0", "AGENT_STABLE_SECS": "1"}
        plan = plan_for(**env)
        agent_fixtures.generate(self.root / "fixtures", plan["fixtures"])
        for name, phases, sizes in lib.segments(plan, "floor"):
            config = lib.runner_config(plan, "floor", 1, phases, sizes, str(self.root / "fixtures"), "none",
                                       str(self.root / f"floor-{name}"))
            out = io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
                agent_run.Runner(config).main()
            events = lib.parse_events(out.getvalue())
            items = {e["id"]: e for e in events if e["type"] == "item"}
            self.assertEqual(set(items), set(lib.segment_items(plan, "floor", phases, sizes)))
            for item, record in items.items():
                self.assertEqual(lib.item_state(record, item, "floor")[0], "ok", record)


if __name__ == "__main__":
    unittest.main()
