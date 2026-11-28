#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
"""Runs the agent profile's items on one system and prints one event line per item.

Started by agent.sh: inside atlas-bench-<system> for ceph and atlas, on the host for floor.

Usage: agent_run.py <config.json>

Each item is timed, checked and reported by itself; a later item still runs after a failed one where
it can. Nothing is reported as ok unless its checks held.
"""

from __future__ import annotations

import concurrent.futures
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.dont_write_bytecode = True
import agent_lib as lib  # noqa: E402

MIB = 1024 * 1024
LOGS = "/tmp/agent-logs"
VOLUME = "agent"  # the tree of phases A to C, E and G
DEFAULTS = {
    "settle_s": 10,
    "bg_base_s": 10,
    "bg_after_s": 10,
    "stable_quiet_s": 90,
    "stable_timeout_s": 900,
    "edits": 20,
    "large_files": 4,
    "large_mib": 256,
    "small_files": 2000,
    "build_threads": 16,
    "clone_timeout_s": 3600,
    "command_timeout_s": 7200,
    "sample_check": 100,
}


def log(*args):
    print(time.strftime("%T"), *args, file=sys.stderr, flush=True)


def emit(event):
    print(lib.format_event(event), flush=True)


def run(cmd, timeout=7200, env=None, cwd=None, check=True):
    """A command's stdout; RuntimeError with the end of stderr if it fails."""
    p = subprocess.run(
        cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout, env=env, cwd=cwd
    )
    if check and p.returncode != 0:
        err = p.stderr.decode(errors="replace").strip()[-800:]
        out = p.stdout.decode(errors="replace").strip()[-400:]
        raise RuntimeError(f"{' '.join(map(str, cmd))[:300]} exited {p.returncode}: {err} {out}")
    return p


def timed(fn, *args, **kwargs):
    t0 = time.monotonic()
    value = fn(*args, **kwargs)
    return time.monotonic() - t0, value


def read_file(path):
    with open(path, "rb") as f:
        return f.read()


def conditions():
    out = {"time": time.time()}
    try:
        out["load"] = lib.parse_loadavg(Path("/proc/loadavg").read_text())
        out["io_pressure"] = lib.parse_pressure(Path("/proc/pressure/io").read_text())
    except (OSError, ValueError, IndexError) as e:
        out["error"] = repr(e)
    return out


def oom_kills():
    """OOM kills in this container's cgroup so far (0 where there is no such file)."""
    try:
        for line in Path("/sys/fs/cgroup/memory.events").read_text().splitlines():
            if line.startswith("oom_kill "):
                return int(line.split()[1])
    except OSError:
        pass
    return 0


def du_bytes(path):
    """Bytes allocated under `path` (each file once)."""
    seen, total = set(), 0
    for base, dirs, files in os.walk(path):
        for name in dirs + files:
            try:
                st = os.lstat(os.path.join(base, name))
            except OSError:
                continue  # removed meanwhile by the system under test
            if (st.st_dev, st.st_ino) not in seen:
                seen.add((st.st_dev, st.st_ino))
                total += st.st_blocks * 512
    return total


def drop_page_cache(root):
    """Writes back the filesystem holding `root`, then drops the page cache of its files only."""
    run(["sync", "-f", root])
    for base, _, files in os.walk(root):
        for name in files:
            try:
                fd = os.open(os.path.join(base, name), os.O_RDONLY | os.O_NOFOLLOW)
            except OSError:
                continue
            try:
                os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            finally:
                os.close(fd)


def processes(match):
    """pids whose command line `match` accepts."""
    out = []
    for entry in os.listdir("/proc"):
        if entry.isdigit():
            try:
                argv = read_file(f"/proc/{entry}/cmdline").decode(errors="replace").split("\0")
            except OSError:
                continue
            if argv and match(argv):
                out.append(int(entry))
    return out


def is_mounted(path):
    # Not realpath: it would stat the mount, which fails or blocks on a dead FUSE client.
    real = os.path.abspath(path)
    for line in Path("/proc/self/mounts").read_text().splitlines():
        fields = line.split()
        if len(fields) > 1 and fields[1].replace("\\040", " ") == real:
            return True
    return False


class Mount:
    """One FUSE mount run by this process, so that it can be killed."""

    def __init__(self, cmd, path, env=None, timeout=300):
        self.cmd, self.path, self.env, self.timeout = cmd, path, env, timeout
        self.proc = None

    def start(self):
        os.makedirs(self.path, exist_ok=True)
        os.makedirs(LOGS, exist_ok=True)
        logfile = os.path.join(LOGS, "mount-" + self.path.strip("/").replace("/", "_") + ".log")
        with open(logfile, "ab") as f:
            self.proc = subprocess.Popen(self.cmd, stdout=f, stderr=f, env=self.env)
        deadline = time.monotonic() + self.timeout
        while not is_mounted(self.path):
            rc = self.proc.poll()
            if rc is not None or time.monotonic() > deadline:
                tail = Path(logfile).read_text(errors="replace")[-800:]
                self.stop()
                why = f"exited {rc}" if rc is not None else f"did not mount in {self.timeout} s"
                raise RuntimeError(f"mount {self.path} {why}: {tail}")
            time.sleep(0.005)
        return self

    def stop(self):
        if is_mounted(self.path):
            if run(["umount", self.path], timeout=120, check=False).returncode != 0:
                run(["umount", "-l", self.path], timeout=60, check=False)
        if self.proc is not None:
            try:
                self.proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=60)
            self.proc = None

    def kill9(self):
        self.proc.kill()
        self.proc.wait(timeout=60)
        self.proc = None
        run(["umount", "-l", self.path], timeout=60)

    def remount(self):
        self.stop()
        return self.start()


# ---------------------------------------------------------------- systems


class AtlasSystem:
    name = "atlas"
    server_dir = "/srv/bench"
    ENDPOINTS = [f"http://127.0.0.1:740{i}" for i in (1, 2, 3)]
    PORTS = ["7201", "7202", "7203", "7401", "7402", "7403"]
    TOKEN_FILE = "/run/atlas/token"
    root_mount = "/mnt/bench"
    root_volume = "bench"

    def __init__(self, cfg):
        self.cfg = cfg
        self.token = Path(self.TOKEN_FILE).read_text().strip()
        self.settings = Path(self.server_dir, "settings.txt").read_text()
        self.mount_args = lib.parse_mount_args(self.settings)
        self.leader = None
        self.retries = 0

    def versions(self):
        return {"atlas_mount": run(["atlas-native-mount", "--version"]).stdout.decode().strip()}

    def prepare(self):
        self.find_leader()

    # -- HTTP

    def fetch(self, url, method="GET", body=None, timeout=900):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(url, data=data, method=method)
        req.add_header("Authorization", f"Bearer {self.token}")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(req, timeout=timeout) as r:
            return r.read()

    def find_leader(self, timeout=120):
        deadline = time.monotonic() + timeout
        while True:
            for url in self.ENDPOINTS:
                try:
                    if lib.is_leader(json.loads(self.fetch(url + "/v1/status", timeout=10))):
                        self.leader = url
                        return url
                except (OSError, ValueError):
                    pass
            if time.monotonic() > deadline:
                raise RuntimeError("no metadata leader found")
            time.sleep(0.2)

    def api(self, method, path, body=None):
        """A request to the metadata leader, retried while there is none."""
        deadline = time.monotonic() + self.cfg["clone_timeout_s"]
        attempts = 0
        while True:
            attempts += 1
            try:
                raw = self.fetch((self.leader or self.find_leader()) + path, method, body)
                return json.loads(raw) if raw.strip() else None
            except urllib.error.HTTPError as e:
                text = e.read().decode(errors="replace")[:500]
                # A delete that was retried may have been carried out the first time.
                if e.code == 404 and method == "DELETE" and attempts > 1:
                    return None
                if e.code not in (421, 429, 503) or time.monotonic() > deadline:
                    raise RuntimeError(f"{method} {path}: HTTP {e.code} {text}") from None
            except OSError as e:
                if time.monotonic() > deadline:
                    raise RuntimeError(f"{method} {path}: {e!r}") from None
            self.retries += 1
            self.leader = None
            time.sleep(0.2)

    # -- volumes

    def create_volume(self, volume):
        extent = lib.setting_extent_bytes(self.settings)
        self.api("POST", "/v1/fs", {"id": volume, "name": volume, "extent_bytes": extent})

    def mount(self, volume, path, snapshot=None):
        cmd = ["atlas-native-mount", "--endpoint", ",".join(self.ENDPOINTS), "--token-file", self.TOKEN_FILE,
               "--fs", volume]
        if snapshot:
            cmd += ["--snapshot", snapshot, "--read-only"]
        return Mount(cmd + self.mount_args + [path], path).start()

    def snapshot(self, volume, snap):
        self.api("POST", f"/v1/fs/{volume}/snapshots", {"id": snap, "name": snap})

    def clone(self, volume, snap, new):
        self.api("POST", f"/v1/fs-snapshots/{snap}/clone", {"id": new, "name": new})

    def snapshot_mount(self, volume, snap, path, base=None):
        """(mount, directory holding the snapshot's tree)."""
        return self.mount(volume, path, snapshot=snap), path

    def delete_volume(self, volume):
        self.api("DELETE", f"/v1/fs/{volume}")

    def delete_snapshot(self, volume, snap):
        self.api("DELETE", f"/v1/fs-snapshots/{snap}")

    def volumes(self):
        return {f["id"] for f in self.api("GET", "/v1/fs")["filesystems"]}

    def snapshots(self, volume):
        return {s["id"] for s in self.api("GET", "/v1/fs-snapshots")["snapshots"]}

    # -- crash of a client

    def before_kill(self, mount):
        return None

    def after_kill(self, token):
        return []

    # -- measurements

    def evict_caches(self):
        drop_page_cache(self.server_dir)
        # The catalog lives in the metadata processes: nothing outside can drop it.
        return []

    def scrape(self):
        return {p: self.fetch(f"http://127.0.0.1:{p}/metrics", timeout=60).decode() for p in self.PORTS}

    def metadata_pids(self):
        pids = processes(
            lambda a: a[0].endswith("atlas-native-node") and any(x.endswith(("m1.json", "m2.json", "m3.json")) for x in a)
        )
        if len(pids) != 3:
            raise RuntimeError(f"expected 3 metadata processes, found {len(pids)}")
        return pids

    def elections(self):
        total = 0.0
        for port in self.PORTS[3:]:
            parsed = lib.parse_metrics(self.fetch(f"http://127.0.0.1:{port}/metrics", timeout=120).decode())
            value = lib.metric_sum(parsed, "atlas_native_raft_elections_total")
            if value is None:
                raise RuntimeError(f"no election counter on port {port}")
            total += value
        return total

    def space(self):
        texts = self.scrape()
        parsed = {p: lib.parse_metrics(t) for p, t in texts.items()}
        device = [lib.metric_sum(parsed[p], "atlas_native_data_device_bytes") for p in self.PORTS[:3]]
        leader = (self.leader or self.find_leader()).rsplit(":", 1)[1]
        free = lib.metric_sum(parsed[leader], "atlas_native_allocator_free_bytes")
        if None in device or free is None:
            raise RuntimeError("space metrics are missing")
        meta = sum(du_bytes(f"{self.server_dir}/m{i}") for i in (1, 2, 3))
        return {
            # Data in use on the three data nodes (the device files never shrink) plus the metadata
            # directories.
            "bytes": sum(device) - free + meta,
            "metadata_bytes": meta,
            "device_bytes": sum(device),
            "free_list_bytes": free,
            "dead_bytes": lib.metric_sum(parsed[leader], "atlas_native_dead_bytes"),
            "du_bytes": du_bytes(self.server_dir),
        }


class CephSystem:
    name = "ceph"
    server_dir = "/srv/bench"
    root_mount = "/mnt/bench"
    root_volume = None
    FS = "bench"

    def __init__(self, cfg):
        import rados  # only the Ceph image has it

        self.cfg = cfg
        self.cluster = rados.Rados(conffile="/etc/ceph/ceph.conf", name="client.admin")
        self.cluster.connect(timeout=60)
        self.retries = 0
        self.env = dict(os.environ, CEPH_USE_RANDOM_NONCE="1")
        self.roots = {}

    def versions(self):
        return {
            "ceph": run(["ceph", "--version"]).stdout.decode().strip(),
            "ceph_fuse": run(["ceph-fuse", "--version"]).stdout.decode().strip(),
        }

    def command(self, kind, prefix, **args):
        """A mon or mgr command over librados, so that no timing includes a CLI start."""
        if kind == "mon":
            args["format"] = "json"
        cmd = json.dumps(dict(prefix=prefix, **args))
        send = self.cluster.mgr_command if kind == "mgr" else self.cluster.mon_command
        ret, out, err = send(cmd, b"", timeout=600)
        if ret != 0:
            raise RuntimeError(f"ceph {prefix}: error {ret} {err}")
        text = out.decode(errors="replace").strip()
        try:
            return json.loads(text) if text else None
        except ValueError:
            return text

    def fs(self, prefix, **args):
        return self.command("mgr", prefix, vol_name=self.FS, **args)

    def cli(self, *args, timeout=600):
        return run(["timeout", str(timeout), "ceph", *args], timeout=timeout + 30).stdout.decode()

    def prepare(self):
        self.cli("osd", "set", "noscrub")
        self.cli("osd", "set", "nodeep-scrub")
        # By default a clone request is refused while all cloner threads are busy; queue instead.
        self.command("mon", "config set", who="mgr", name="mgr/volumes/snapshot_clone_no_wait", value="false")
        if self.cfg.get("ceph_max_clones"):
            self.command("mon", "config set", who="mgr", name="mgr/volumes/max_concurrent_clones",
                         value=str(self.cfg["ceph_max_clones"]))

    # -- volumes: CephFS subvolumes

    def create_volume(self, volume):
        self.fs("fs subvolume create", sub_name=volume)

    def path(self, volume):
        """The data directory of a subvolume: /volumes/_nogroup/<name>/<uuid>."""
        if volume not in self.roots:
            cmd = json.dumps({"prefix": "fs subvolume getpath", "vol_name": self.FS, "sub_name": volume})
            ret, out, err = self.cluster.mgr_command(cmd, b"", timeout=120)
            if ret != 0:
                raise RuntimeError(f"fs subvolume getpath {volume}: error {ret} {err}")
            self.roots[volume] = out.decode().strip().strip('"')
        return self.roots[volume]

    def fuse(self, root, path):
        cmd = ["ceph-fuse", "-f", "-n", "client.admin", "--client_fs", self.FS, "-r", root,
               "-o", "max_idle_threads=64", "-o", "clone_fd", path]
        return Mount(cmd, path, env=self.env).start()

    def mount(self, volume, path, snapshot=None):
        return self.fuse(self.path(volume) if volume else "/", path)

    def snapshot(self, volume, snap):
        self.fs("fs subvolume snapshot create", sub_name=volume, snap_name=snap)

    def clone(self, volume, snap, new):
        """A writable copy of the snapshot; returns when Ceph reports it complete."""
        self.fs("fs subvolume snapshot clone", sub_name=volume, snap_name=snap, target_sub_name=new)
        deadline = time.monotonic() + self.cfg["clone_timeout_s"]
        while True:
            doc = self.fs("fs clone status", clone_name=new)
            state = lib.clone_state(doc)
            if state == "complete":
                return
            if state in ("failed", "canceled"):
                raise RuntimeError(f"clone {new} {state}: {doc}")
            if time.monotonic() > deadline:
                raise RuntimeError(f"clone {new} still {state} after {self.cfg['clone_timeout_s']} s")
            time.sleep(0.05)

    def in_snapshot(self, volume, snap, mount_of_parent):
        """The snapshot's tree seen through a mount of the subvolume's parent directory."""
        uuid = self.path(volume).rstrip("/").rsplit("/", 1)[1]
        return f"{mount_of_parent}/.snap/{snap}/{uuid}"

    def parent(self, volume):
        return self.path(volume).rstrip("/").rsplit("/", 1)[0]

    def snapshot_mount(self, volume, snap, path, base=None):
        """With base (the mount of the subvolume's parent that is already there) nothing new is
        mounted; otherwise a client of its own, as atlas-native needs one."""
        if base is not None:
            return None, self.in_snapshot(volume, snap, base.path)
        m = self.fuse(self.parent(volume), path)
        return m, self.in_snapshot(volume, snap, path)

    def delete_volume(self, volume):
        self.fs("fs subvolume rm", sub_name=volume)
        self.roots.pop(volume, None)

    def delete_snapshot(self, volume, snap):
        self.fs("fs subvolume snapshot rm", sub_name=volume, snap_name=snap)

    def volumes(self):
        return {v["name"] for v in self.fs("fs subvolume ls")}

    def snapshots(self, volume):
        return {v["name"] for v in self.fs("fs subvolume snapshot ls", sub_name=volume)}

    # -- crash of a client

    def before_kill(self, mount):
        sessions = json.loads(self.cli("tell", "mds.a", "client", "ls"))
        ids = [s["id"] for s in sessions if s.get("client_metadata", {}).get("mount_point") == mount.path]
        return ids

    def after_kill(self, ids):
        """The dead client holds capabilities until the MDS gives up on it (300 s by default); evict
        it, as an operator or a CSI driver would."""
        if not ids:
            return ["the killed client's MDS session was not found: reads may wait for its timeout"]
        for i in ids:
            self.cli("tell", "mds.a", "client", "evict", f"id={i}")
        return [f"evicted the killed client's MDS session ({len(ids)})"]

    # -- measurements

    def evict_caches(self):
        drop_page_cache(self.server_dir)
        problems = []
        for target in ("osd.*", "mds.a"):
            try:
                self.cli("tell", target, "cache", "drop", timeout=300)
            except (RuntimeError, subprocess.TimeoutExpired) as e:
                problems.append(f"cache drop on {target} failed: {str(e)[:200]}")
        return problems

    def scrape(self):
        return {}

    def metadata_pids(self):
        pids = processes(lambda a: a[0].endswith("ceph-mds"))
        if len(pids) != 1:
            raise RuntimeError(f"expected 1 MDS process, found {len(pids)}")
        return pids

    def elections(self):
        return None

    def space(self):
        total, meta = lib.parse_ceph_df(self.command("mon", "df"))
        return {"bytes": total, "metadata_bytes": meta}


class FloorSystem:
    """cp -a --reflink=always on the host's XFS: one copy, no replication, no storage system."""

    name = "floor"
    root_mount = None
    root_volume = None

    def __init__(self, cfg):
        self.cfg = cfg
        self.server_dir = cfg["floor_root"]
        self.retries = 0

    def versions(self):
        return {"cp": run(["cp", "--version"]).stdout.decode().splitlines()[0]}

    def prepare(self):
        os.makedirs(self.server_dir, exist_ok=True)
        probe = os.path.join(self.server_dir, ".reflink-probe")
        Path(probe).write_bytes(b"x" * 8192)
        p = run(["cp", "--reflink=always", probe, probe + ".copy"], check=False)
        for f in (probe, probe + ".copy"):
            if os.path.exists(f):
                os.unlink(f)
        self.unsupported = None
        if p.returncode != 0:
            self.unsupported = "reflink does not work here: " + p.stderr.decode(errors="replace").strip()

    def dir(self, volume):
        return os.path.join(self.server_dir, volume)

    def create_volume(self, volume):
        os.makedirs(self.dir(volume))

    def delete_volume(self, volume):
        shutil.rmtree(self.dir(volume))

    def volumes(self):
        return set(os.listdir(self.server_dir))

    def evict_caches(self):
        drop_page_cache(self.server_dir)
        return []

    def space(self):
        run(["sync", "-f", self.server_dir])
        st = os.statvfs(self.server_dir)
        # The whole shared filesystem: anything else written to it counts too.
        return {"bytes": (st.f_blocks - st.f_bfree) * st.f_frsize}

    def elections(self):
        return None

    def before_kill(self, mount):
        return None


# ---------------------------------------------------------------- volumes and views as a run sees them


class Folder:
    """The floor's "mount": the directory itself."""

    def __init__(self, path):
        self.path = path

    def stop(self):
        pass

    def remount(self):
        return self


def overlay_mount(lower, upper, work, target):
    """Mounts overlayfs; Unsupported with the kernel's refusals if it cannot be done."""
    os.makedirs(target, exist_ok=True)
    errors = []
    for extra in (",userxattr", ""):
        options = f"lowerdir={lower},upperdir={upper},workdir={work}{extra}"
        p = run(["mount", "-t", "overlay", "overlay", "-o", options, target], timeout=300, check=False)
        if p.returncode == 0:
            return "userxattr" if extra else "trusted xattrs"
        errors.append(p.stderr.decode(errors="replace").strip())
    raise lib.Unsupported("overlayfs mount refused: " + " | ".join(errors))


class OverlayView:
    def __init__(self, name, lower_mount, lower_dir, scratch, path, owns_lower):
        self.name, self.lower_mount, self.lower_dir = name, lower_mount, lower_dir
        self.scratch, self.path, self.owns_lower = scratch, path, owns_lower
        self.note = None

    def open(self):
        for d in ("upper", "work"):
            os.makedirs(os.path.join(self.scratch, d), exist_ok=True)
        self.note = overlay_mount(
            self.lower_dir, os.path.join(self.scratch, "upper"), os.path.join(self.scratch, "work"), self.path
        )

    def close(self):
        if is_mounted(self.path):
            run(["umount", self.path], timeout=300)
        if self.owns_lower and self.lower_mount is not None:
            self.lower_mount.stop()

    def remount(self):
        self.close()
        if self.owns_lower and self.lower_mount is not None:
            self.lower_mount.start()
        self.open()

    def discard(self):
        self.close()
        shutil.rmtree(self.scratch)


class CloneView:
    def __init__(self, system, name, mount):
        self.system, self.name, self.mount = system, name, mount
        self.path = mount.path

    def close(self):
        self.mount.stop()

    def remount(self):
        self.mount.remount()

    def discard(self):
        self.close()
        self.system.delete_volume(self.name)


class DirView:
    def __init__(self, name, path):
        self.name, self.path = name, path

    def close(self):
        pass

    def remount(self):
        pass

    def discard(self):
        shutil.rmtree(self.path)


class Writer(threading.Thread):
    """One client writing in the background: create a 4 KiB file, fsync, close, remove, repeat."""

    def __init__(self, directory):
        super().__init__(daemon=True)
        self.directory = directory
        self.ops = []
        self.current = None
        self.error = None
        self.halt = threading.Event()

    def run(self):
        data = os.urandom(4096)
        i = 0
        try:
            os.makedirs(self.directory, exist_ok=True)
            while not self.halt.is_set():
                path = os.path.join(self.directory, f"w{i}")
                t0 = self.current = time.monotonic()
                fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
                try:
                    os.write(fd, data)
                    os.fsync(fd)
                finally:
                    os.close(fd)
                os.unlink(path)
                self.ops.append((t0, time.monotonic()))
                self.current = None
                i += 1
        except OSError as e:
            self.error = repr(e)

    def finish(self, timeout=300):
        self.halt.set()
        self.join(timeout)
        ops = list(self.ops)
        if self.is_alive():
            if self.current is not None:
                ops.append((self.current, time.monotonic()))
            self.error = self.error or f"still blocked in one operation {timeout} s after the stop request"
        return ops


def writer_figures(ops, start, fork_start, fork_end, stop, warmup=1.0):
    """Foreground latency before, during and after a fork, in ms."""
    windows = {
        # Operations wholly before the fork: one that the fork already held up is not baseline.
        "base": [b - a for a, b in ops if a >= start + warmup and b < fork_start],
        "fork": lib.overlapping(ops, fork_start, fork_end),
        "after": lib.overlapping(ops, fork_end, stop),
    }
    out = {}
    for name, values in windows.items():
        out[f"bg_{name}_ops"] = len(values)
        if values:
            out[f"bg_{name}_p99_ms"] = lib.percentile(values, 99) * 1000
            out[f"bg_{name}_max_ms"] = max(values) * 1000
    return out


def parallel(fn, items):
    """fn over items, all at once. The first failure is raised after all have ended."""
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, len(items))) as pool:
        futures = [pool.submit(fn, item) for item in items]
        concurrent.futures.wait(futures)
    return [f.result() for f in futures]


def build_tree(tar_path, dest, threads):
    """Unpacks a fixture tar with several writers (setup, not a measurement)."""
    dirs, files = lib.tar_members(tar_path)
    for d in dirs:
        os.makedirs(os.path.join(dest, d), exist_ok=True)

    def write(item):
        with open(os.path.join(dest, item[0]), "wb") as f:
            f.write(item[1])

    with concurrent.futures.ThreadPoolExecutor(max_workers=threads) as pool:
        for _ in pool.map(write, files):
            pass


# ---------------------------------------------------------------- the run


class Runner:
    def __init__(self, cfg):
        self.cfg = dict(DEFAULTS, **cfg.get("settings", {}))
        self.cfg.update({k: v for k, v in cfg.items() if k != "settings"})
        self.plan = cfg["plan"]
        self.round = cfg["round"]
        self.fixtures = Path(cfg["fixtures"])
        self.info = json.loads((self.fixtures / "fixtures.json").read_text())
        system = {"atlas": AtlasSystem, "ceph": CephSystem, "floor": FloorSystem}[cfg["system"]]
        self.system = system(self.cfg)
        self.floor = cfg["system"] == "floor"
        self.mnt = os.path.join(self.system.server_dir, "mnt") if self.floor else "/mnt"
        # Short ids unique to this run, so that a second run on a live cluster does not collide.
        self.token = "%x" % (int(time.time()) % 0xFFFFFF)
        self.serial = 0
        self.serial_lock = threading.Lock()
        self.open = []  # views and mounts to take down at the end
        self.workspace = None  # the mount holding phase A's tree
        self.source = None  # {path: sha256} of the tree phase E forks
        self.originals = None
        self.fanout = []  # (view or None, snapshot) that phase F discards
        self.fanout_snapshots = []
        self.fanout_baseline = None
        self.bases = {}  # size -> (mount, manifest) or the error its setup ended in
        self.done = {}

    def name(self, kind):
        with self.serial_lock:
            self.serial += 1
            return f"{kind}{self.token}x{self.serial}"

    def manifest(self, name):
        return lib.parse_manifest((self.fixtures / name).read_text())

    def mount_volume(self, volume, path):
        if self.floor:
            return Folder(self.system.dir(volume))
        m = self.system.mount(volume, path)
        self.open.append(m)
        return m

    def mount_parent(self, volume):
        """Ceph's snapshots appear in the .snap directory of the subvolume's parent."""
        m = self.system.fuse(self.system.parent(volume), f"{self.mnt}/{self.name('parent')}")
        self.open.append(m)
        return m

    def stable_space(self, quiet, timeout, interval, minimum=0.0):
        if self.floor:
            # The whole shared disk never holds still: one reading after a sync.
            took, value = timed(self.system.space)
            return value, took, True

        def same(a, b):
            return abs(a["bytes"] - b["bytes"]) <= MIB and abs(a.get("metadata_bytes", 0) - b.get("metadata_bytes", 0)) <= MIB

        return lib.wait_stable(self.system.space, same, quiet, timeout, interval, minimum)

    def settled_space(self):
        value, _, stable = self.stable_space(15, 180, 5)
        return value, stable

    def drop_server_caches(self):
        """Why a following read would not be cold, as far as it is known; empty if it is."""
        problems = self.system.evict_caches()
        if self.plan.get("tmpfs_gb") and not self.floor:
            problems.append("server data is on tmpfs, whose pages cannot be dropped")
        return problems

    def settle(self):
        # The floor too: a reflink copy would otherwise first write back the source's dirty pages.
        run(["sync", "-f", self.system.server_dir])
        time.sleep(self.cfg["settle_s"])

    # -- items

    def run_item(self, item):
        record = {"type": "item", "id": item, "status": lib.OK, "metrics": {}, "notes": [], "detail": ""}
        record["start"] = conditions()
        ooms = oom_kills()
        retries = self.system.retries
        log("item", item)
        try:
            if self.floor and self.system.unsupported:
                raise lib.Unsupported(self.system.unsupported)
            self.dispatch(item, record)
        except lib.Unsupported as e:
            record.update(status=lib.UNSUPPORTED, detail=str(e)[:1500])
        except lib.VerifyError as e:
            record.update(status=lib.FAILED, detail=str(e)[:1500])
        except lib.Incomplete as e:
            record.update(status=lib.INCOMPLETE, detail=str(e)[:1500])
        except Exception as e:  # one broken step must not hide the others
            log(traceback.format_exc())
            record.update(status=lib.ERROR, detail=f"{type(e).__name__}: {e}"[:1500])
        killed = oom_kills() - ooms
        if killed:
            record.update(status=lib.FAILED, detail=f"{killed} processes OOM-killed during this item. {record['detail']}")
        if self.system.retries > retries:
            record["metrics"]["leader_retries"] = self.system.retries - retries
        if self.floor and any("space" in k or "reclaimed" in k for k in record["metrics"]):
            record["notes"].append("floor space figures are of the whole shared filesystem, one reading each")
        record["end"] = conditions()
        self.done[item] = record["status"]
        log("item", item, record["status"], record["detail"][:300])
        emit(record)

    def dispatch(self, item, record):
        parts = item.split("/")
        if parts[0] == "A":
            self.phase_a(record)
        elif item == "C/after-kill":
            self.phase_c_kill(record)
        elif parts[0] == "B":
            self.phase_b(record)
        elif item == "C/final":
            self.phase_c_final(record)
        elif parts[0] == "E":
            self.phase_e(record, int(parts[1]), parts[2])
        elif parts[0] == "F":
            self.phase_f(record)
        elif parts[0] == "G":
            self.phase_g(record)
        elif parts[0] == "D":
            self.phase_d(record, int(parts[1]), parts[2], parts[3])
        elif parts[0] == "S":
            self.phase_s(record, int(parts[1][1:]))
        else:
            raise ValueError(f"unknown item {item}")

    def need(self, item):
        if self.done.get(item) != lib.OK:
            raise lib.Incomplete(f"needs {item}, which did not succeed")

    # -- A, B, C

    def workspace_mount(self):
        if self.workspace is None:
            self.system.create_volume(VOLUME)
            self.workspace = self.mount_volume(VOLUME, f"{self.mnt}/{VOLUME}")
            os.makedirs(os.path.join(self.workspace.path, "ws"))
        return self.workspace

    def ws(self):
        return os.path.join(self.workspace_mount().path, "ws")

    def phase_a(self, record):
        ws = self.ws()
        if os.listdir(ws):
            raise lib.Incomplete("the workspace is not empty")
        self.settle()
        m = record["metrics"]
        t0 = time.monotonic()
        run(["tar", "-xf", str(self.fixtures / "pkgs.tar"), "-C", ws], self.cfg["command_timeout_s"])
        m["tool_exit_s"] = time.monotonic() - t0
        run(
            ["bash", "-c", 'set -euo pipefail; find "$1" \\( -type f -o -type d \\) -print0 | xargs -0 -n1 -P8 sync',
             "durable", ws],
            self.cfg["command_timeout_s"],
        )
        m["durable_s"] = time.monotonic() - t0
        m["sync_s"] = m["durable_s"] - m["tool_exit_s"]
        expected = self.manifest("manifest-pkgs.sha256")
        found = lib.walk_files(ws)
        m["files"] = len(found)
        if found != sorted(expected):
            raise lib.VerifyError(f"{len(found)} files after tar, expected {len(expected)} of other names or number")

    def sha256sum(self, name, record):
        """The spec's check: sha256sum -c in the workspace."""
        count = len(self.manifest(name))
        t, p = timed(
            run, ["sha256sum", "--quiet", "--strict", "-c", str(self.fixtures / name)],
            self.cfg["command_timeout_s"], None, self.ws(), False,
        )
        record["metrics"]["verify_s"] = t
        record["metrics"]["files"] = count
        if p.returncode != 0:
            raise lib.VerifyError(
                f"sha256sum -c {name} failed: {p.stdout.decode(errors='replace')[:600]}"
                f" {p.stderr.decode(errors='replace')[-600:]}"
            )

    def phase_c_kill(self, record):
        self.need("A")
        mount = self.workspace_mount()
        token = self.system.before_kill(mount)
        t0 = time.monotonic()
        mount.kill9()
        notes = self.system.after_kill(token)
        mount.start()
        record["metrics"]["remount_s"] = time.monotonic() - t0
        record["notes"] += notes + ["kill -9 of the client mount; not a power-loss test"]
        self.sha256sum("manifest-pkgs.sha256", record)

    def git_env(self):
        home = os.path.join(tempfile.gettempdir(), "agent-home")
        os.makedirs(home, exist_ok=True)
        Path(home, ".gitconfig").write_text("[safe]\n\tdirectory = *\n[gc]\n\tauto = 0\n[maintenance]\n\tauto = false\n")
        return {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": home, "GIT_CONFIG_NOSYSTEM": "1", "LC_ALL": "C"}

    def git_steps(self, ws, record):
        """Phase B in `ws`: clone, status, 20 edits, commit, fsck; each timed and checked."""
        env = self.git_env()
        src = os.path.join(ws, "src")
        m = record["metrics"] if record is not None else {}
        limit = self.cfg["command_timeout_s"]

        def git(*args):
            return run(["git", *args], limit, env).stdout

        m["clone_s"], _ = timed(git, "clone", "-q", "--no-hardlinks", str(self.fixtures / "repo.git"), src)
        head = git("-C", src, "rev-parse", "HEAD").decode().strip()
        if head != self.info["head"]:
            raise lib.VerifyError(f"cloned HEAD is {head}, expected {self.info['head']}")
        m["status_s"], out = timed(git, "-C", src, "status", "--porcelain")
        if out.strip():
            raise lib.VerifyError(f"a fresh clone is not clean: {out[:300]!r}")
        t0 = time.monotonic()
        listed = git("-C", src, "ls-files", "-z").split(b"\0")
        targets = sorted(p for p in listed if p)[: len(self.info["edited"])]
        for path in targets:
            with open(os.path.join(os.fsencode(src), path), "ab") as f:
                f.write(b"bench\n")
        m["edit_s"] = time.monotonic() - t0
        if [t.decode() for t in targets] != self.info["edited"]:
            raise lib.VerifyError("the files to edit are not the expected ones")
        m["commit_s"], _ = timed(
            git, "-C", src, "-c", "user.name=bench", "-c", "user.email=bench@invalid", "commit", "-qam", "bench"
        )
        tree = git("-C", src, "rev-parse", "HEAD^{tree}").decode().strip()
        if tree != self.info["tree_after_edits"]:
            raise lib.VerifyError(f"committed tree is {tree}, expected {self.info['tree_after_edits']}")
        if git("-C", src, "status", "--porcelain").strip():
            raise lib.VerifyError("not clean after the commit")
        m["fsck_s"], _ = timed(git, "-C", src, "fsck", "--full")

    def phase_b(self, record):
        self.need("A")
        self.git_steps(self.ws(), record)

    def phase_c_final(self, record):
        self.need("A")
        self.need("B")
        self.sha256sum("manifest.sha256", record)

    # -- the tree phase E forks

    def source_tree(self):
        """Phase A to C's workspace, or the same tree built untimed if they are not in this run.
        Returns (mount, {path: sha256} of everything in it)."""
        if self.source is not None:
            return self.workspace, self.source
        with_git = "B" in self.plan["phases"]
        if "A" in self.done:
            self.need("A")
            if with_git:
                self.need("B")
            mount = self.workspace
        else:
            if self.floor:
                self.system.create_volume(VOLUME)
                self.workspace = mount = Folder(self.system.dir(VOLUME))
                os.makedirs(os.path.join(mount.path, "ws"))
            else:
                mount = self.workspace_mount()
            log("building the fan-out source tree (setup)")
            ws = os.path.join(mount.path, "ws")
            build_tree(self.fixtures / "pkgs.tar", ws, self.cfg["build_threads"])
            if with_git:
                self.git_steps(ws, None)
        mount.remount()
        # Everything is read back and must match the fixtures; the rest (git's own files) is
        # recorded as found, and every fork must show the same.
        tree = lib.hash_tree(mount.path)
        expected = self.manifest("manifest.sha256" if with_git else "manifest-pkgs.sha256")
        wrong = [p for p, d in expected.items() if tree.get(f"ws/{p}") != d]
        if wrong:
            raise lib.VerifyError(f"source tree differs from the fixtures: {len(wrong)} files {wrong[:3]}")
        self.source = tree
        targets = lib.edit_targets([f"ws/{p}" for p in expected], self.cfg["edits"])
        self.originals = {p: read_file(os.path.join(mount.path, p)) for p in targets}
        return mount, tree

    # -- views

    def make_snapshot(self, volume):
        snap = self.name("s")
        if not self.floor:
            self.system.snapshot(volume, snap)
        return snap

    def shared_lower(self, volume, snap, base):
        """The one lower layer of overlay-shared views."""
        mount, lower = self.system.snapshot_mount(
            volume, snap, f"{self.mnt}/lower-{snap}", base=base if self.system.name == "ceph" else None
        )
        if mount is not None:
            self.open.append(mount)
        return mount, lower

    def make_view(self, volume, snap, method, shared=None):
        """(view, {step: seconds}) of one writable fork of snapshot `snap`."""
        name = self.name("c")
        path = f"{self.mnt}/views/{name}"
        times = {}
        if method == "reflink":
            target = self.system.dir(name)
            times["clone_s"], _ = timed(
                run, ["cp", "-a", "--reflink=always", self.system.dir(volume), target], self.cfg["command_timeout_s"]
            )
            return DirView(name, target), times
        if method == "clone":
            times["clone_s"], _ = timed(self.system.clone, volume, snap, name)
            times["mount_s"], mount = timed(self.system.mount, name, path)
            view = CloneView(self.system, name, mount)
            self.open.append(view)
            return view, times
        scratch = os.path.join(self.system.server_dir, "agent-upper", name)
        t0 = time.monotonic()
        if method == "overlay":
            lower_mount, lower = self.system.snapshot_mount(volume, snap, f"{path}.lower")
            view = OverlayView(name, lower_mount, lower, scratch, path, owns_lower=True)
        else:
            view = OverlayView(name, shared[0], shared[1], scratch, path, owns_lower=False)
        self.open.append(view)
        view.open()
        times["mount_s"] = time.monotonic() - t0
        return view, times

    # -- E and F

    def read_pass(self, views, expected, prefix, record):
        def one(view):
            return timed(lib.read_tree, view.path, expected)

        t0 = time.monotonic()
        try:
            results = parallel(one, views)
        except OSError as e:
            if any(isinstance(v, OverlayView) for v in views):
                raise lib.Unsupported(f"reading through overlayfs failed: {e}") from None
            raise
        m = record["metrics"]
        m[f"{prefix}_wall_s"] = time.monotonic() - t0
        m[f"{prefix}_slowest_s"] = max(t for t, _ in results)
        m[f"{prefix}_p99_ms"] = lib.percentile([x for _, lat in results for x in lat], 99) * 1000
        m[f"{prefix}_files"] = sum(len(lat) for _, lat in results)

    def edit_pass(self, views, record):
        def one(view):
            latencies = []
            for path in self.originals:
                t0 = time.monotonic()
                with open(os.path.join(view.path, path), "ab", buffering=0) as f:
                    f.write(lib.edit_suffix(view.name))
                    os.fsync(f.fileno())
                latencies.append(time.monotonic() - t0)
            return latencies

        t0 = time.monotonic()
        try:
            results = parallel(one, views)
        except OSError as e:
            if any(isinstance(v, OverlayView) for v in views):
                raise lib.Unsupported(f"writing through overlayfs failed: {e}") from None
            raise
        record["metrics"]["edit_wall_s"] = time.monotonic() - t0
        record["metrics"]["edit_p99_ms"] = lib.percentile([x for lat in results for x in lat], 99) * 1000

    def phase_e(self, record, k, method):
        mount, source = self.source_tree()
        base = None
        if method == "overlay-shared" and self.system.name == "ceph":
            base = self.mount_parent(VOLUME)
        self.settle()
        if self.fanout_baseline is None:
            self.fanout_baseline, _ = self.settled_space()
        before, stable_before = self.settled_space()
        m = record["metrics"]
        views, shared = [], None
        try:
            t0 = time.monotonic()
            snap = None
            m["snapshot_s"] = 0.0
            if not self.floor:
                m["snapshot_s"], snap = timed(self.make_snapshot, VOLUME)
                self.fanout_snapshots.append((VOLUME, snap))
            if method == "overlay-shared":
                shared = self.shared_lower(VOLUME, snap, base)

            def fork(_):
                t, (view, _times) = timed(self.make_view, VOLUME, snap, method, shared)
                views.append(view)
                self.fanout.append(view)  # phase F discards it even if a sibling failed
                return t

            took = parallel(fork, range(k))
            m["fork_wall_s"] = time.monotonic() - t0
            m["fork_slowest_s"] = max(took)
            if self.floor:
                m["sync_s"], _ = timed(run, ["sync", "-f", self.system.server_dir])
            lower = base or (shared[0] if shared else None)
            if lower is not None:
                self.remount_lower(views, lower)  # the lower layer's client must be cold too
            problems = self.drop_server_caches()
            self.read_pass(views, source, "cold", record)
            self.read_pass(views, source, "warm", record)
            self.edit_pass(views, record)

            # Every view shows its own edits and nothing else, as seen by fresh clients.
            if lower is not None:
                self.remount_lower(views, lower)
            else:
                for v in views:
                    v.remount()
            parallel(
                lambda v: lib.read_tree(v.path, lib.edited_manifest(source, self.originals, v.name)), views
            )
            mount.remount()
            lib.read_tree(mount.path, source)
            after, stable_after = self.settled_space()
            m["space_growth_bytes"] = after["bytes"] - before["bytes"]
            for key, value in after.items():
                if key != "bytes" and value is not None and before.get(key) is not None:
                    m[f"space_growth_{key}"] = value - before[key]
            if not (stable_before and stable_after):
                record["notes"].append("space figures had not stopped changing")
            if method.startswith("overlay"):
                record["notes"].append(f"upper layer on the server disk, unreplicated; mounted with {views[0].note}")
            if problems:
                raise lib.Incomplete("cold read not established: " + "; ".join(problems))
        finally:
            for v in views:
                v.close()
            for lower in (base, shared[0] if shared else None):
                if lower is not None:
                    lower.stop()

    def remount_lower(self, views, lower):
        """A fresh client under the overlays: take them down, remount it, put them back."""
        for v in views:
            v.close()
        lower.remount()
        for v in views:
            v.open()

    def phase_f(self, record):
        if not self.fanout and not self.fanout_snapshots:
            raise lib.Incomplete("phase E made nothing to discard")
        mount, source = self.workspace, self.source
        before = self.system.space()
        m = record["metrics"]
        views = [v.name for v in self.fanout]
        clones = [v.name for v in self.fanout if isinstance(v, (CloneView, DirView))]
        snapshots = list(self.fanout_snapshots)
        t0 = time.monotonic()
        for v in self.fanout:
            v.discard()
        if not self.floor:
            for volume, snap in snapshots:
                self.system.delete_snapshot(volume, snap)
        m["delete_s"] = time.monotonic() - t0
        after, changed, stable = self.stable_space(self.cfg["stable_quiet_s"], self.cfg["stable_timeout_s"], 5)
        m["stable_s"] = m["delete_s"] + changed
        m["space_before_bytes"] = before["bytes"]
        m["space_after_bytes"] = after["bytes"]
        m["reclaimed_bytes"] = before["bytes"] - after["bytes"]
        if self.fanout_baseline:
            m["residual_bytes"] = after["bytes"] - self.fanout_baseline["bytes"]
        m["discarded"] = len(views)
        for key, value in after.items():
            if value is not None and before.get(key) is not None:
                m[f"reclaimed_{key}"] = before[key] - value
        left = set(clones) & self.system.volumes()
        if left:
            raise lib.VerifyError(f"deleted clones are still listed: {sorted(left)[:5]}")
        if not self.floor:
            left = {s for _, s in snapshots} & self.system.snapshots(VOLUME)
            if left:
                raise lib.VerifyError(f"deleted snapshots are still listed: {sorted(left)[:5]}")
        self.fanout, self.fanout_snapshots = [], []
        if source is not None:
            mount.remount()
            lib.read_tree(mount.path, source)
        if not stable:
            raise lib.Incomplete(f"space still changing after {self.cfg['stable_timeout_s']} s")

    # -- G

    def phase_g(self, record):
        mount = self.workspace_mount()
        directory = os.path.join(mount.path, "large")
        os.makedirs(directory)
        count, mib = self.cfg["large_files"], self.cfg["large_mib"]
        bases = [hashlib.shake_128(f"large:{i}".encode()).digest(MIB) for i in range(count)]

        def blocks(i):
            for n in range(mib):
                yield n.to_bytes(8, "big") + bases[i][8:]

        digests = []
        for i in range(count):
            h = hashlib.sha256()
            for block in blocks(i):
                h.update(block)
            digests.append(h.hexdigest())
        self.settle()
        t0 = time.monotonic()
        for i in range(count):
            final = os.path.join(directory, f"large{i}")
            with open(final + ".tmp", "wb", buffering=0) as f:
                for block in blocks(i):
                    view = memoryview(block)
                    while view:
                        view = view[f.write(view) :]
                os.rename(final + ".tmp", final)
                os.fsync(f.fileno())
            fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        m = record["metrics"]
        m["write_s"] = time.monotonic() - t0
        m["write_mibs"] = count * mib / m["write_s"]
        mount.remount()
        problems = self.drop_server_caches()
        t0 = time.monotonic()
        found = [lib.sha256_file(os.path.join(directory, f"large{i}")) for i in range(count)]
        m["read_s"] = time.monotonic() - t0
        m["read_mibs"] = count * mib / m["read_s"]
        if found != digests:
            raise lib.VerifyError("large files read back differently: " + str([i for i in range(count) if found[i] != digests[i]]))
        if sorted(os.listdir(directory)) != sorted(f"large{i}" for i in range(count)):
            raise lib.VerifyError(f"unexpected entries after the renames: {os.listdir(directory)}")
        record["notes"].append(f"{count} files of {mib} MiB, one after another; client remounted and server caches dropped")
        if problems:
            raise lib.Incomplete("cold read not established: " + "; ".join(problems))

    # -- D

    def base_tree(self, n):
        if n not in self.bases:
            try:
                volume = f"base{n}"
                self.system.create_volume(volume)
                mount = self.mount_volume(volume, f"{self.mnt}/{volume}")
                manifest = self.manifest(f"manifest-base-{n}.sha256")
                log(f"building the tree of {n} files (setup)")
                t, _ = timed(build_tree, self.fixtures / f"base-{n}.tar", mount.path, self.cfg["build_threads"])
                emit({"type": "setup", "what": volume, "build_s": t})
                self.settle()
                mount.remount()
                log(f"checking the tree of {n} files (setup)")
                lib.read_tree(mount.path, manifest)
                self.bases[n] = (mount, manifest)
            except Exception as e:
                self.bases[n] = e
                raise
        if isinstance(self.bases[n], Exception):
            raise RuntimeError(f"setup of the {n} file tree failed: {self.bases[n]}")
        return self.bases[n]

    def memory(self):
        total = {"VmRSS": 0, "VmHWM": 0}
        for pid in self.system.metadata_pids():
            text = Path(f"/proc/{pid}/status").read_text()
            for key in total:
                value = lib.parse_status_kb(text, key)
                if value is None:
                    raise RuntimeError(f"no {key} for process {pid}")
                total[key] += value
        return total

    def reset_peak_memory(self):
        try:
            for pid in self.system.metadata_pids():
                Path(f"/proc/{pid}/clear_refs").write_text("5")
            return True
        except OSError:
            return False

    def phase_d(self, record, n, mode, method):
        volume = f"base{n}"
        mount, manifest = self.base_tree(n)
        base = None
        if method == "overlay-shared" and self.system.name == "ceph":
            base = self.mount_parent(volume)
        m = record["metrics"]
        measured = not self.floor
        view = writer = shared = None
        try:
            self.settle()
            before = stable_before = None
            if measured:
                before, stable_before = self.settled_space()
            if mode == "busy":
                writer = Writer(os.path.join(mount.path, "bg"))
                writer_start = time.monotonic()
                writer.start()
                time.sleep(self.cfg["bg_base_s"])
            mem_before = elections = None
            if measured:
                peak_reset = self.reset_peak_memory()
                mem_before = self.memory()
                elections = self.system.elections()
            fixed = sorted(manifest)[0]
            t0 = time.monotonic()
            snap = None
            if not self.floor:
                m["snapshot_s"], snap = timed(self.make_snapshot, volume)
            lower_s = 0.0
            if method == "overlay-shared":
                lower_s, shared = timed(self.shared_lower, volume, snap, base)
            view, times = self.make_view(volume, snap, method, shared)
            m.update(times)
            if method == "overlay-shared":
                m["mount_s"] += lower_s
            try:
                m["first_read_s"], digest = timed(lib.sha256_file, os.path.join(view.path, fixed))
            except OSError as e:
                if isinstance(view, OverlayView):
                    raise lib.Unsupported(f"reading through overlayfs failed: {e}") from None
                raise
            t1 = time.monotonic()
            m["total_s"] = t1 - t0
            if method == "clone":
                m["fork_s"] = m["snapshot_s"] + m["clone_s"]
            elif method == "reflink":
                m["fork_s"] = m["clone_s"]
                m["sync_s"], _ = timed(run, ["sync", "-f", self.system.server_dir])
            else:
                m["fork_s"] = m["snapshot_s"] + m["mount_s"]
            if digest != manifest[fixed]:
                raise lib.VerifyError(f"the fork's {fixed} differs")
            if measured:
                time.sleep(self.cfg["bg_after_s"])
            stop = time.monotonic()
            if writer is not None:
                ops = writer.finish()
                m.update(writer_figures(ops, writer_start, t0, t1, stop))
                if writer.error:
                    raise lib.VerifyError(f"the background writer failed: {writer.error}")
                writer = None
            if measured:
                mem_after = self.memory()
                m["rss_before_bytes"] = mem_before["VmRSS"]
                m["rss_after_bytes"] = mem_after["VmRSS"]
                peak = mem_after["VmHWM"] if peak_reset else 0
                if peak_reset:
                    m["rss_peak_bytes"] = peak
                else:
                    record["notes"].append("peak memory could not be reset: growth is after less before")
                m["rss_growth_bytes"] = max(mem_after["VmRSS"], peak) - mem_before["VmRSS"]
                if elections is not None:
                    m["elections"] = self.system.elections() - elections
                after, stable_after = self.settled_space()
                m["space_growth_bytes"] = after["bytes"] - before["bytes"]
                m["metadata_growth_bytes"] = after["metadata_bytes"] - before["metadata_bytes"]
                m["metadata_bytes"] = after["metadata_bytes"]
                if not (stable_before and stable_after):
                    record["notes"].append("space figures had not stopped changing")
            # The whole fork, untimed: the same names, and a sample of the contents.
            found = lib.walk_files(view.path, skip=("bg",))
            if found != sorted(manifest):
                raise lib.VerifyError(f"the fork lists {len(found)} files, expected {len(manifest)} or other names")
            names = sorted(manifest)
            for path in names[:: max(1, len(names) // self.cfg["sample_check"])]:
                if lib.sha256_file(os.path.join(view.path, path)) != manifest[path]:
                    raise lib.VerifyError(f"the fork's {path} differs")
            m["files"] = len(found)
            if isinstance(view, OverlayView):
                record["notes"].append(f"upper layer on the server disk, unreplicated; mounted with {view.note}")
            if method == "overlay-shared" and base is not None:
                record["notes"].append("lower layer: the .snap directory of a CephFS mount that was already there")
            if mode == "busy" and not m.get("bg_fork_ops"):
                raise lib.Incomplete("no background operation overlapped the fork")
        finally:
            if writer is not None:
                writer.finish(60)
            if view is not None:
                view.close()
            for lower in (base, shared[0] if shared else None):
                if lower is not None:
                    lower.stop()

    # -- S: bet 1

    def phase_s(self, record, writers):
        per_writer = -(-self.cfg["small_files"] // writers)
        files = per_writer * writers
        sub = f"agent-small/{self.token}-w{writers}"
        directory = os.path.join(self.system.root_mount, sub)
        os.makedirs(directory)
        self.settle()
        before = self.system.scrape()
        disk = self.cfg["disk"]
        disk0 = lib.parse_diskstats(Path("/proc/diskstats").read_text(), disk)
        # The smallfile-create-4k job of run.sh, with the number of writers varied.
        cmd = [
            "fio", "--name=smallfile-create-4k", "--ioengine=psync", "--direct=1", "--group_reporting",
            "--randrepeat=1", "--output-format=json", f"--directory={directory}", f"--numjobs={writers}",
            f"--nrfiles={per_writer}", "--filesize=4k", "--rw=write", "--bs=4k", "--create_on_open=1",
            "--openfiles=1", "--file_service_type=sequential", "--fsync_on_close=1",
        ]
        p = run(cmd, self.cfg["command_timeout_s"], check=False)
        disk1 = lib.parse_diskstats(Path("/proc/diskstats").read_text(), disk)
        after = self.system.scrape()
        text = p.stdout.decode(errors="replace")
        record["artifacts"] = {"fio.json": text}
        for port in before:
            record["artifacts"][f"metrics-{port}-before.txt"] = before[port]
            record["artifacts"][f"metrics-{port}-after.txt"] = after[port]
        if p.returncode != 0:
            raise lib.VerifyError(f"fio exit {p.returncode}: {p.stderr.decode(errors='replace')[-600:]}")
        m = record["metrics"]
        m.update(lib.smallfile_figures(lib.parse_fio_job(text), files))
        m["files"], m["writers"] = files, writers
        if disk0 and disk1:
            if disk1["flushes"] is not None:
                m["disk_flushes"] = disk1["flushes"] - disk0["flushes"]
            m["disk_writes"] = disk1["writes"] - disk0["writes"]
            m["disk_mib_written"] = (disk1["sectors_written"] - disk0["sectors_written"]) / 2048
        if self.system.name == "atlas":
            self.sync_figures(m, before, after, files)
        # As a second client sees them: every file there, 4 KiB long.
        check = self.mount_volume(self.system.root_volume, f"{self.mnt}/check-w{writers}")
        try:
            seen = os.path.join(check.path, sub)
            sizes = [e.stat().st_size for e in os.scandir(seen)]
        finally:
            check.stop()
        if len(sizes) != files or any(s != 4096 for s in sizes):
            raise lib.VerifyError(f"a second client sees {len(sizes)} files, {sum(s == 4096 for s in sizes)} of 4 KiB")
        record["notes"].append("checked: file count and sizes from a second mount; contents are not checked")

    def sync_figures(self, m, before, after, files):
        for kind, ports, name in (
            ("raft", AtlasSystem.PORTS[3:], "atlas_native_raft_wal_sync_seconds"),
            ("data", AtlasSystem.PORTS[:3], "atlas_native_data_sync_seconds"),
        ):
            deltas = [lib.sync_delta(lib.parse_metrics(before[p]), lib.parse_metrics(after[p]), name) for p in ports]
            if None in deltas:
                continue  # the figure is then missing, and the item incomplete
            count = sum(d[0] for d in deltas)
            seconds = sum(d[1] for d in deltas)
            m[f"{kind}_syncs"] = count
            m[f"{kind}_syncs_per_file"] = count / files
            m[f"{kind}_sync_seconds"] = seconds
            if count:
                m[f"{kind}_sync_mean_ms"] = seconds / count * 1000

    # -- all of it

    def close(self):
        for obj in reversed(self.open):
            try:
                obj.close() if hasattr(obj, "close") else obj.stop()
            except Exception as e:
                log("cleanup:", e)

    def main(self):
        segment_plan = dict(self.plan, phases=self.cfg["phases"])
        items = lib.expected_items(segment_plan, self.system.name, self.cfg.get("sizes") or None)
        items = lib.order_items(items, self.round)
        start = {
            "type": "start", "system": self.system.name, "round": self.round, "items": items,
            "python": platform.python_version(), "kernel": platform.release(), "conditions": conditions(),
        }
        for tool in ("git", "tar", "fio"):
            p = run([tool, "--version"], check=False) if shutil.which(tool) else None
            start[tool] = p.stdout.decode().splitlines()[0] if p and p.stdout else None
        start.update(self.system.versions())
        emit(start)
        ok = False
        try:
            self.system.prepare()
            for item in items:
                self.run_item(item)
            ok = True
        finally:
            self.close()
            emit({"type": "end", "complete": ok, "conditions": conditions()})


def main(argv):
    if len(argv) != 1:
        print(__doc__, file=sys.stderr)
        return 2
    cfg = json.loads(Path(argv[0]).read_text())
    runner = Runner(cfg)
    runner.main()
    bad = [i for i, s in runner.done.items() if s != lib.OK]
    log("not ok:", " ".join(bad) or "none")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
