#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
"""Synthetic result sets for gate.py. Usage: python3 -I test_gate.py"""

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.dont_write_bytecode = True
import gate  # noqa: E402

RAMP, SECS = 15, 1200
END = RAMP + SECS + 5


def fio_json(options, read=None, write=None, error=0):
    zero = {"io_bytes": 0, "total_ios": 0, "iops": 0, "bw_bytes": 0, "runtime": 0, "clat_ns": {}}
    job = {"error": error, "job options": options, "read": {**zero, **(read or {})}, "write": {**zero, **(write or {})}}
    return json.dumps({"jobs": [job]})


def series(pending, writes_per_s=40, folds_per_s=10, stop=END, drop_ports=(), restart_at=None, step=60):
    """metrics-series.txt: pending(t) is the same on every metadata node; counters are split over two
    of them, as after a change of leader."""
    lines = []
    for t in range(0, stop + 1, step):
        lines.append(f"## sample t={t} done={t + 1} status=ok")
        for port in ("7201", "7202", "7203", "7401", "7402", "7403"):
            if port in drop_ports:
                lines.append(f"# port {port} scrape-failed")
                continue
            lines.append(f"# port {port}")
            if port.startswith("72"):
                lines.append('atlas_native_data_bytes_total{node="d"} 5')
                continue
            share = {"7401": 0.25, "7402": 0.0, "7403": 0.75}[port]
            writes = int(writes_per_s * t * share)
            if restart_at is not None and t >= restart_at:
                writes = 0
            lines += [
                "# HELP atlas_native_delta_pending Deltas not folded yet.",
                f"atlas_native_delta_pending {int(pending(t))}",
                f"atlas_native_delta_pending_bytes {int(pending(t)) * 12288}",
                f"atlas_native_delta_writes_total {writes}",
                f"atlas_native_delta_folds_total {int(folds_per_s * t * share)}",
                "atlas_native_delta_fallbacks_total 0",
                f'atlas_native_raft_elections_total{{node="m{port[-1]}"}} 1',
                'atlas_native_new_family_seconds_bucket{le="0.1"} 3',
            ]
    lines.append(f"## load-end t={END}")
    return "\n".join(lines) + "\n"


class GateTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def result_set(self, system, round_, iops=40.0, p99=1000.0, seq=100.0, session="s1", hour=None):
        d = self.root / f"s1-r{round_}-{system}-{len(list(self.root.iterdir()))}" / system
        d.mkdir(parents=True)
        hour = hour if hour is not None else 2 * round_ + (system == "atlas")
        (d / "env.txt").write_text(
            f"date=2026-11-28T{hour:02d}:00:00+01:00\nkernel=k\ndisk=sdb\n"
            f"system={system} size_mb=1024 ramp={RAMP} runtime=60 repeats=1 files=32\nmem=12g cpus=10\n"
            f"loadavg=1.0 1.0 1.0 1/1 1\ncommit=abc\nsession={session}\nround={round_}\nloadavg_end=2.0 2 2 1/1 1\n"
            + ("node_binary=f00\n" if system == "atlas" else "")
        )
        (d / "settings.txt").write_text('extent_bytes=1\n{"replicas":3,"delta_writes":{"enabled":true}}\n')
        (d / "steady-randwrite-4k-qd32.1.json").write_text(
            "warning before the JSON\n"
            + fio_json(
                {"rw": "randwrite", "bs": "4k", "sync": "1", "numjobs": "32", "direct": "1"},
                write={"total_ios": 1000, "iops": iops, "runtime": SECS * 1000,
                       "clat_ns": {"percentile": {"99.000000": p99 * 1e6}}},
            )
        )
        (d / "seqread-1m-after.1.json").write_text(
            fio_json({"rw": "read", "bs": "1m"}, read={"total_ios": 10, "bw_bytes": seq * 1048576})
        )
        (d / "verify.1.json").write_text(
            fio_json({"verify": "crc32c", "do_verify": "1"}, read={"io_bytes": 4096}, write={"io_bytes": 4096})
        )
        if system == "atlas":
            # A bounded backlog that wobbles around 2000.
            (d / "metrics-series.txt").write_text(series(lambda t: 2000 + (t // 60 % 3) * 40))
        return d

    def session(self, rounds=3, ceph=None, atlas=None):
        c = [self.result_set("ceph", r, **(ceph or {})) for r in range(1, rounds + 1)]
        a = [self.result_set("atlas", r, **{"iops": 45.0, "p99": 900.0, **(atlas or {})}) for r in range(1, rounds + 1)]
        return c, a

    def judge(self, c, a):
        verdict, lines = gate.judge([str(p) for p in c], [str(p) for p in a])
        return verdict, "\n".join(lines)

    def assert_verdict(self, expected, c, a, *fragments):
        verdict, text = self.judge(c, a)
        self.assertEqual(verdict, expected, text)
        for fragment in fragments:
            self.assertIn(fragment, text)
        return text

    def test_pass(self):
        c, a = self.session()
        text = self.assert_verdict("PASS", c, a, "raft elections during the long run (summed): 0")
        # Writes are summed over the nodes (40/s over the window), pending is not (about 2000).
        self.assertIn("delta writes 24000", text)
        self.assertRegex(text, r"pending 20[0-8]0 -> 20[0-8]0")

    def test_median_decides_not_a_single_round(self):
        c, a = self.session()
        (a[0] / "steady-randwrite-4k-qd32.1.json").write_text(
            (a[0] / "steady-randwrite-4k-qd32.1.json").read_text().replace('"iops": 45.0', '"iops": 1.0')
        )
        self.assert_verdict("PASS", c, a)

    def test_equal_medians_pass_the_comparisons(self):
        c, a = self.session(atlas={"iops": 40.0, "p99": 1000.0, "seq": 50.0})
        self.assert_verdict("PASS", c, a)

    def test_fewer_iops_fail(self):
        c, a = self.session(atlas={"iops": 39.9})
        self.assert_verdict("FAIL", c, a, "[FAIL] write IOPS")

    def test_worse_p99_fails(self):
        c, a = self.session(atlas={"p99": 1001.0})
        self.assert_verdict("FAIL", c, a, "[FAIL] p99")

    def test_slow_sequential_read_fails(self):
        c, a = self.session(atlas={"seq": 49.9})
        self.assert_verdict("FAIL", c, a, "[FAIL] sequential read")

    def test_growing_backlog_fails(self):
        c, a = self.session()
        (a[1] / "metrics-series.txt").write_text(series(lambda t: 30 * t))
        self.assert_verdict("FAIL", c, a, "growing", "2 of 3 atlas rounds pass")

    def test_small_steady_growth_above_tolerance_fails(self):
        c, a = self.session()
        # 1 delta/s over 600 s against 1% of 24000 writes.
        (a[0] / "metrics-series.txt").write_text(series(lambda t: 1000 + t))
        self.assert_verdict("FAIL", c, a, "growing")

    def test_shrinking_backlog_passes(self):
        c, a = self.session()
        (a[0] / "metrics-series.txt").write_text(series(lambda t: 40000 - 10 * t))
        self.assert_verdict("PASS", c, a)

    def test_flat_backlog_at_the_limit_fails(self):
        c, a = self.session()
        (a[0] / "metrics-series.txt").write_text(series(lambda t: 65000))
        self.assert_verdict("FAIL", c, a, "backlog at its limit")

    def test_no_delta_writes_in_the_window_is_unknown(self):
        c, a = self.session()
        (a[0] / "metrics-series.txt").write_text(series(lambda t: 0, writes_per_s=0))
        self.assert_verdict("UNKNOWN", c, a, "none was taken")

    def test_missing_series_is_unknown(self):
        c, a = self.session()
        (a[2] / "metrics-series.txt").unlink()
        self.assert_verdict("UNKNOWN", c, a, "no metrics-series.txt")

    def test_unfinished_series_is_unknown(self):
        c, a = self.session()
        path = a[0] / "metrics-series.txt"
        path.write_text(path.read_text().replace(f"## load-end t={END}\n", ""))
        self.assert_verdict("UNKNOWN", c, a, "no load-end mark")

    def test_samples_that_stop_early_are_unknown(self):
        c, a = self.session()
        (a[0] / "metrics-series.txt").write_text(series(lambda t: 2000, stop=900))
        self.assert_verdict("UNKNOWN", c, a)

    def test_sparse_samples_are_unknown(self):
        c, a = self.session()
        (a[0] / "metrics-series.txt").write_text(series(lambda t: 2000, step=200))
        self.assert_verdict("UNKNOWN", c, a)

    def test_samples_after_the_load_are_ignored(self):
        c, a = self.session()
        path = a[0] / "metrics-series.txt"
        grown = series(lambda t: 30 * t)
        after = (
            f"## sample t={END + 5} done={END + 6} status=ok\n"
            + "".join(f"# port {p}\natlas_native_delta_pending 0\n" for p in ("7401", "7402", "7403"))
        )
        path.write_text(grown + after)
        self.assert_verdict("FAIL", c, a, "growing")

    def test_two_metadata_nodes_missing_is_unknown(self):
        c, a = self.session()
        (a[0] / "metrics-series.txt").write_text(series(lambda t: 2000, drop_ports=("7401", "7402")))
        self.assert_verdict("UNKNOWN", c, a)

    def test_one_metadata_node_missing_cannot_sum_writes(self):
        c, a = self.session()
        (a[0] / "metrics-series.txt").write_text(series(lambda t: 2000, drop_ports=("7403",)))
        self.assert_verdict("UNKNOWN", c, a, "all three metadata nodes")

    def test_counter_reset_is_unknown(self):
        c, a = self.session()
        (a[0] / "metrics-series.txt").write_text(series(lambda t: 2000, restart_at=900))
        self.assert_verdict("UNKNOWN", c, a, "went backwards")

    def test_missing_steady_result_is_unknown(self):
        c, a = self.session()
        (c[1] / "steady-randwrite-4k-qd32.1.json").unlink()
        self.assert_verdict("UNKNOWN", c, a, "is missing")

    def test_failed_steady_run_is_unknown(self):
        c, a = self.session()
        (a[1] / "steady-randwrite-4k-qd32.1.failed").write_text("1\n")
        self.assert_verdict("UNKNOWN", c, a, "fio failed")

    def test_short_steady_run_is_unknown(self):
        c, a = self.session()
        path = a[1] / "steady-randwrite-4k-qd32.1.json"
        path.write_text(path.read_text().replace(f'"runtime": {SECS * 1000}', '"runtime": 180000'))
        self.assert_verdict("UNKNOWN", c, a, "ran 180 s")

    def test_truncated_json_is_unknown(self):
        c, a = self.session()
        (a[1] / "seqread-1m-after.1.json").write_text('{"jobs": [')
        self.assert_verdict("UNKNOWN", c, a, "cannot be read")

    def test_failed_verify_fails(self):
        c, a = self.session()
        (a[1] / "verify.1.failed").write_text("1\n")
        self.assert_verdict("FAIL", c, a, "[FAIL] fio crc32c")

    def test_missing_verify_is_unknown(self):
        c, a = self.session()
        (a[1] / "verify.1.json").unlink()
        self.assert_verdict("UNKNOWN", c, a, "[UNKNOWN] fio crc32c")

    def test_partial_read_back_is_unknown(self):
        c, a = self.session()
        (a[1] / "verify.1.json").write_text(
            fio_json({"verify": "crc32c", "do_verify": "1"}, read={"io_bytes": 0}, write={"io_bytes": 4096})
        )
        self.assert_verdict("UNKNOWN", c, a, "read back 0")

    def test_two_rounds_are_not_enough(self):
        c, a = self.session(rounds=2)
        self.assert_verdict("UNKNOWN", c, a, "the target needs 3", "[UNKNOWN] write IOPS")

    def test_two_losing_rounds_fail(self):
        c, a = self.session(rounds=2, atlas={"iops": 1.0})
        self.assert_verdict("FAIL", c, a)

    def test_rounds_must_alternate(self):
        c = [self.result_set("ceph", r, hour=r) for r in (1, 2, 3)]
        a = [self.result_set("atlas", r, hour=10 + r, iops=45.0, p99=900.0) for r in (1, 2, 3)]
        self.assert_verdict("UNKNOWN", c, a, "did not alternate")

    def test_rounds_must_share_a_session(self):
        c, a = self.session()
        path = a[0] / "env.txt"
        path.write_text(path.read_text().replace("session=s1", "session=other"))
        self.assert_verdict("UNKNOWN", c, a, "not one compare.sh session")

    def test_old_result_sets_without_a_session_are_unknown(self):
        c, a = self.session(ceph={"session": ""}, atlas={"session": ""})
        self.assert_verdict("UNKNOWN", c, a, "not one compare.sh session")

    def test_a_set_listed_twice_is_unknown(self):
        c, a = self.session()
        self.assert_verdict("UNKNOWN", c, [a[0], a[0], a[0]], "listed twice")

    def test_missing_directory_is_unknown(self):
        c, a = self.session()
        self.assert_verdict("UNKNOWN", c, a[:2] + [self.root / "nowhere" / "atlas"], "directory is missing")

    def test_different_commits_are_unknown(self):
        c, a = self.session()
        path = a[0] / "env.txt"
        path.write_text(path.read_text().replace("commit=abc", "commit=def"))
        self.assert_verdict("UNKNOWN", c, a, "atlas commit differs")

    def test_a_different_node_binary_is_unknown(self):
        c, a = self.session()
        path = a[2] / "env.txt"
        path.write_text(path.read_text().replace("node_binary=f00", "node_binary=bad"))
        self.assert_verdict("UNKNOWN", c, a, "atlas node binary differs")

    def test_exit_status(self):
        c, a = self.session()
        argv = ["--ceph", *map(str, c), "--atlas", *map(str, a)]
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(gate.main(argv), 0)
            (a[0] / "verify.1.json").unlink()
            self.assertEqual(gate.main(argv), 3)
            (a[1] / "verify.1.failed").write_text("1\n")
            self.assertEqual(gate.main(argv), 1)

    def test_usage(self):
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(gate.main([]), 2)
            self.assertEqual(gate.main(["--ceph", "x"]), 2)
            self.assertEqual(gate.main(["x", "--ceph", "x", "--atlas", "y"]), 2)


if __name__ == "__main__":
    unittest.main()
