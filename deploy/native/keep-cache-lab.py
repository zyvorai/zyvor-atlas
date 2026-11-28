#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
"""`atlas-native-mount --keep-cache` on a real kernel, with several mounts of one filesystem.

Starts 3 metadata + 3 data atlas-native-node processes on localhost, mounts one filesystem
through FUSE several times, and checks after every kind of change that an open on a keeping mount
returns the current bytes, and that the kernel kept or dropped its page cache as expected
(asked with mincore(2) right after each open). Needs Linux, /dev/fuse and the right to mount (a
container with --device /dev/fuse --cap-add SYS_ADMIN, or fusermount3).

    keep-cache-lab.py --node .../atlas-native-node --mount .../atlas-native-mount [--work DIR]
    keep-cache-lab.py ... --bench      # repeated-read timings with the flag off and on

--bench measures; run it alone on the machine, with release builds.
"""

import argparse
import ctypes
import errno
import json
import mmap
import os
import random
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

PAGE = mmap.PAGESIZE
MIB = 1 << 20
TOKEN = "keep-cache-lab-token"
XATTR = "user.atlas.keep_cache"
LIBC = ctypes.CDLL(None, use_errno=True)
LIBC.mmap.restype = ctypes.c_void_p
LIBC.mmap.argtypes = [
    ctypes.c_void_p,
    ctypes.c_size_t,
    ctypes.c_int,
    ctypes.c_int,
    ctypes.c_int,
    ctypes.c_long,
]
LIBC.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
LIBC.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]


class Failed(Exception):
    pass


def check(ok, what):
    if not ok:
        raise Failed(what)


def resident(fd):
    """Pages of the open file in the page cache, without reading any (mincore(2))."""
    size = os.fstat(fd).st_size
    if size == 0:
        return 0
    addr = LIBC.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
    if addr == ctypes.c_void_p(-1).value:
        e = ctypes.get_errno()
        raise OSError(e, "mmap: " + os.strerror(e))
    try:
        pages = (size + PAGE - 1) // PAGE
        vec = (ctypes.c_ubyte * pages)()
        if LIBC.mincore(addr, size, vec) != 0:
            e = ctypes.get_errno()
            raise OSError(e, "mincore: " + os.strerror(e))
        return sum(v & 1 for v in vec)
    finally:
        LIBC.munmap(addr, size)


def read_fd(fd, offset=0, length=None):
    out = bytearray()
    while length is None or len(out) < length:
        want = MIB if length is None else min(MIB, length - len(out))
        b = os.pread(fd, want, offset + len(out))
        if not b:
            break
        out += b
    return bytes(out)


def open_read(path):
    """(pages cached when the open returned, the file's bytes)."""
    fd = os.open(path, os.O_RDONLY)
    try:
        cached = resident(fd)
        return cached, read_fd(fd)
    finally:
        os.close(fd)


def write(path, data, offset=0, truncate=False):
    flags = os.O_WRONLY | os.O_CREAT | (os.O_TRUNC if truncate else 0)
    fd = os.open(path, flags, 0o644)
    try:
        done = 0
        while done < len(data):
            done += os.pwrite(fd, data[done : done + MIB], offset + done)
    finally:
        os.close(fd)


def patch(data, offset, new):
    return data[:offset] + new + data[offset + len(new) :]


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Cluster:
    def __init__(self, node_bin, work):
        self.work = work
        self.procs = {}
        self.node_bin = node_bin
        token = os.path.join(work, "token")
        with open(token, "w") as f:
            f.write(TOKEN)
        self.token_file = token
        data = [(f"d{i}", free_port()) for i in (1, 2, 3)]
        meta = [(f"m{i}", free_port(), free_port()) for i in (1, 2, 3)]
        for name, port in data:
            self.start(
                name,
                {
                    "node_id": name,
                    "data_dir": os.path.join(work, name),
                    "http_listen": f"127.0.0.1:{free_port()}",
                    "data_node": {"listen": f"127.0.0.1:{port}"},
                },
            )
        self.urls = {}
        specs = [{"id": n, "addr": f"127.0.0.1:{p}"} for n, p in data]
        for name, http, raft in meta:
            self.urls[name] = f"http://127.0.0.1:{http}"
            self.start(
                name,
                {
                    "node_id": name,
                    "data_dir": os.path.join(work, name),
                    "http_listen": f"127.0.0.1:{http}",
                    "api_token_file": token,
                    "metadata": {
                        "listen": f"127.0.0.1:{raft}",
                        "peers": {n: f"127.0.0.1:{r}" for n, _, r in meta if n != name},
                        "data_nodes": specs,
                        "replicas": 3,
                        "repair_interval_secs": 5,
                        "gc_interval_secs": 5,
                    },
                },
            )
        self.leader()

    def start(self, name, config=None):
        path = os.path.join(self.work, name + ".json")
        if config is not None:
            with open(path, "w") as f:
                json.dump(config, f)
        log = open(os.path.join(self.work, name + ".log"), "ab")
        self.procs[name] = subprocess.Popen(
            [self.node_bin, "--config", path], stdout=log, stderr=log
        )
        log.close()

    def status(self, name):
        req = urllib.request.Request(
            self.urls[name] + "/v1/status",
            headers={"Authorization": "Bearer " + TOKEN},
        )
        with urllib.request.urlopen(req, timeout=2) as r:
            return json.load(r)

    def leader(self, but_not=()):
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            for name in self.urls:
                if name in but_not:
                    continue
                try:
                    if self.status(name)["metadata"]["role"] == "leader":
                        return name
                except (OSError, KeyError, ValueError):
                    pass
            time.sleep(0.1)
        raise Failed("no metadata leader")

    def api(self, method, path, body=None, skip=()):
        deadline = time.monotonic() + 30
        last = None
        while time.monotonic() < deadline:
            for name, url in self.urls.items():
                if name in skip:
                    continue
                req = urllib.request.Request(
                    url + path,
                    method=method,
                    data=None if body is None else json.dumps(body).encode(),
                    headers={"Authorization": "Bearer " + TOKEN},
                )
                try:
                    with urllib.request.urlopen(req, timeout=5) as r:
                        text = r.read()
                        return json.loads(text) if text else None
                except urllib.error.HTTPError as e:
                    last = f"{e.code} {e.read()[:200]!r}"
                    if e.code not in (421, 503):
                        raise Failed(f"{method} {path}: {last}")
                except OSError as e:
                    last = str(e)
            time.sleep(0.1)
        raise Failed(f"{method} {path}: {last}")

    def stop(self):
        for p in self.procs.values():
            try:
                p.send_signal(signal.SIGCONT)
                p.kill()
                p.wait(10)
            except (OSError, subprocess.TimeoutExpired):
                pass


class Mounts:
    def __init__(self, mount_bin, cluster):
        self.bin = mount_bin
        self.cluster = cluster
        self.live = []

    def mount(self, name, fs, keep, ttl_ms=None, nodes=None, extra=()):
        path = os.path.join(self.cluster.work, "mnt-" + name)
        os.makedirs(path, exist_ok=True)
        urls = ",".join(
            u for n, u in self.cluster.urls.items() if nodes is None or n in nodes
        )
        args = [self.bin, "--endpoint", urls, "--fs", fs]
        args += ["--token-file", self.cluster.token_file]
        if keep:
            args.append("--keep-cache")
        if ttl_ms is not None:
            args += ["--ttl-ms", str(ttl_ms)]
        args += list(extra) + [path]
        log = open(os.path.join(self.cluster.work, name + ".mount.log"), "ab")
        p = subprocess.Popen(args, stdout=log, stderr=log)
        log.close()
        deadline = time.monotonic() + 30
        while not os.path.ismount(path):
            check(p.poll() is None, f"mount {name} exited {p.returncode}; see its log")
            check(time.monotonic() < deadline, f"mount {name} did not appear")
            time.sleep(0.05)
        self.live.append((path, p))
        return path

    def unmount(self, path):
        for i, (m, p) in enumerate(self.live):
            if m != path:
                continue
            if LIBC.umount2(m.encode(), 0) != 0:
                subprocess.run(["fusermount3", "-u", m], check=False)
            try:
                p.wait(20)
            except subprocess.TimeoutExpired:
                p.kill()
                LIBC.umount2(m.encode(), 2)  # MNT_DETACH
            del self.live[i]
            return

    def stop(self):
        for path, _ in list(self.live):
            self.unmount(path)


def counters(mnt):
    return json.loads(os.getxattr(mnt, XATTR))


class Suite:
    def __init__(self, cluster, mounts):
        self.c = cluster
        self.m = mounts
        self.passed = 0
        self.rng = random.Random(11)

    def ok(self, what):
        self.passed += 1
        print("  ok  " + what, flush=True)

    def bytes(self, n):
        return self.rng.randbytes(n)

    def expect(self, path, want, kept, what):
        """One open and full read on a keeping mount."""
        cached, got = open_read(path)
        if got != want:
            first = next(
                (i for i, (x, y) in enumerate(zip(got, want)) if x != y),
                min(len(got), len(want)),
            )
            raise Failed(
                f"{what}: wrong data ({len(got)} bytes, want {len(want)}, "
                f"first difference at {first}, {cached} pages cached at open)"
            )
        pages = (len(want) + PAGE - 1) // PAGE
        if kept is True:
            check(cached == pages, f"{what}: {cached}/{pages} pages cached, not kept")
        elif kept is False:
            check(cached == 0, f"{what}: {cached} pages survived an open that must drop")

    def refills(self, path, want, what):
        """After a change: the next open drops, the one after keeps."""
        self.expect(path, want, False, what + " (first open)")
        self.expect(path, want, True, what + " (second open)")
        self.expect(path, want, True, what + " (third open)")
        self.ok(what)

    # Changes by another mount; `a` keeps, `b` writes.
    def remote_changes(self, fs, ttl_ms, sizes_may_change):
        a = self.m.mount(f"a-{fs}", fs, True, ttl_ms)
        b = self.m.mount(f"b-{fs}", fs, False, 0)
        control = self.m.mount(f"c-{fs}", fs, False, ttl_ms)
        n = 6 * MIB + 123
        data = self.bytes(n)
        write(b + "/f", data)
        fa = a + "/f"
        self.refills(fa, data, "a file written by another mount")
        for _ in range(3):
            cached, got = open_read(control + "/f")
            check(got == data and cached == 0, f"without the flag: {cached} pages kept")
        self.ok("without --keep-cache every open drops the cache")

        new = self.bytes(4096)
        write(b + "/f", new, 3 * MIB)
        old, data = data, patch(data, 3 * MIB, new)
        self.refills(fa, data, "a 4 KiB overwrite elsewhere")
        write(b + "/f", old[3 * MIB : 3 * MIB + 4096], 3 * MIB)
        data = old
        self.refills(fa, data, "the old bytes written back elsewhere")
        before = os.stat(b + "/f")
        data = self.bytes(n)
        write(b + "/f", data)
        os.utime(b + "/f", ns=(before.st_atime_ns, before.st_mtime_ns))
        self.refills(fa, data, "a rewrite with its mtime put back")

        # A descriptor opened before the change, reading around it.
        held = os.open(fa, os.O_RDONLY)
        try:
            read_fd(held, 0, MIB)
            data = patch(data, 4 * MIB, self.bytes(MIB))
            write(b + "/f", data[4 * MIB : 5 * MIB], 4 * MIB)
            read_fd(held, 4 * MIB, MIB)
            self.refills(fa, data, "a change under a descriptor that stays open")
            data = patch(data, 0, self.bytes(100))
            write(b + "/f", data[:100])
            self.expect(fa, data, False, "a second change under it")
        finally:
            os.close(held)
        self.ok("a second change under an open descriptor")

        if sizes_may_change:
            os.truncate(b + "/f", MIB + 17)
            data = data[: MIB + 17]
            self.refills(fa, data, "a truncate elsewhere")
            os.truncate(b + "/f", 2 * MIB)
            data += bytes(2 * MIB - len(data))
            self.refills(fa, data, "a truncate up elsewhere")
            more = self.bytes(70_000)
            write(b + "/f", more, len(data))
            data += more
            self.refills(fa, data, "an append elsewhere")
            data = self.bytes(MIB)
            write(b + "/f.tmp", data)
            os.replace(b + "/f.tmp", b + "/f")
            self.refills(fa, data, "a rename over the file elsewhere")
            os.unlink(b + "/f")
            data = self.bytes(MIB)
            write(b + "/f", data)
            self.refills(fa, data, "unlink and re-create elsewhere")

        # Changes through the keeping mount itself.
        new = self.bytes(5000)
        write(fa, new, 8192)
        data = patch(data, 8192, new)
        self.refills(fa, data, "a write through the keeping mount")
        fd = os.open(fa, os.O_RDWR)
        try:
            with mmap.mmap(fd, len(data)) as view:
                view[100:200] = b"M" * 100
                data = patch(data, 100, b"M" * 100)
                self.expect(fa, data, None, "a mapped store not yet written back")
                view.flush()
        finally:
            os.close(fd)
        self.expect(fa, data, None, "after the mapped store")
        self.expect(fa, data, True, "after the mapped store, again")
        self.expect(b + "/f", data, None, "the mapped store seen by the other mount")
        self.ok("a store through a shared mapping")
        fd = os.open(fa, os.O_WRONLY | os.O_DIRECT)
        try:
            buf = mmap.mmap(-1, PAGE)
            buf.write(b"D" * PAGE)
            os.pwrite(fd, buf, 0)
        finally:
            os.close(fd)
        data = patch(data, 0, b"D" * PAGE)
        self.refills(fa, data, "an O_DIRECT write through the keeping mount")

        s = counters(a)
        check(s["kept"] > 0 and s["dropped_changed"] > 0, f"counters: {s}")
        try:
            os.getxattr(control, XATTR)
            raise Failed("a mount without the flag answered the counters")
        except OSError as e:
            check(e.errno == errno.ENODATA, f"counters without the flag: {e}")
        self.ok(f"counters {s}")
        for m in (a, b, control):
            self.m.unmount(m)

    def concurrent(self, fs, rounds):
        """One writer rewrites a file; readers must never see less than what was closed
        before their open."""
        a = self.m.mount(f"a-{fs}", fs, True)
        b = self.m.mount(f"b-{fs}", fs, False)
        blocks = 256

        def stamped(v):
            return b"".join((v.to_bytes(8, "little") * 512) for _ in range(blocks))

        write(b + "/v", stamped(1))
        closed, stop, errors, reads = [1], threading.Event(), [], [0, 0]

        def reader(seed):
            rng = random.Random(seed)
            while not stop.is_set():
                floor = closed[0]
                try:
                    cached, got = open_read(a + "/v")
                    check(len(got) == blocks * 4096, f"short read: {len(got)}")
                    for off in range(0, len(got), 4096):
                        v = int.from_bytes(got[off : off + 8], "little")
                        check(
                            v >= floor,
                            f"version {v} at {off} read by an open after {floor} "
                            f"was closed ({cached} pages cached at open)",
                        )
                    reads[0] += 1
                    reads[1] += cached == blocks
                except (Failed, OSError) as e:
                    errors.append(str(e))
                    stop.set()
                time.sleep(rng.random() * 0.01)

        threads = [threading.Thread(target=reader, args=(i,)) for i in range(4)]
        for t in threads:
            t.start()
        try:
            for v in range(2, rounds + 2):
                if stop.is_set():
                    break
                write(b + "/v", stamped(v))
                closed[0] = v
                time.sleep(self.rng.random() * 0.15)
        finally:
            stop.set()
            for t in threads:
                t.join()
        check(not errors, f"concurrent readers: {errors[:3]}")
        check(reads[1] > 0, f"no open kept the cache in {reads[0]}")
        self.ok(
            f"{rounds} rewrites under 4 readers: {reads[0]} opens, {reads[1]} fully "
            f"cached, none stale; {counters(a)}"
        )
        self.m.unmount(a)
        self.m.unmount(b)

    def lost_writes(self, fs):
        a = self.m.mount(f"a-{fs}", fs, True, 0)
        b = self.m.mount(f"b-{fs}", fs, False, 0)
        data = self.bytes(MIB)
        write(b + "/f", data)
        self.refills(a + "/f", data, "before the quota")
        self.c.api("PUT", f"/v1/fs/{fs}/quota", {"max_bytes": 2 * MIB})
        # An extension the cluster refuses: accepted by write(), lost, told at close.
        fd = os.open(a + "/f", os.O_WRONLY)
        os.pwrite(fd, b"L" * (2 * MIB), MIB)
        try:
            os.close(fd)
            raise Failed("close did not report the lost write-back")
        except OSError as e:
            check(e.errno == errno.EDQUOT, f"close: {e}")
        before = counters(a)["dropped_lost_writes"]

        def stored():
            return open_read(b + "/f")[1]

        want = stored()
        check(b"L" * 4096 not in want[2 * MIB :], "the quota did not refuse the write")
        for i in range(3):
            self.expect(a + "/f", want, False, f"loss not yet synced, open {i}")
        check(
            counters(a)["dropped_lost_writes"] == before + 3,
            f"counters: {counters(a)}",
        )
        self.ok("no open keeps a file with an unreported loss")
        fd = os.open(a + "/f", os.O_RDONLY)
        try:
            os.fsync(fd)
            raise Failed("fsync did not report the lost write-back")
        except OSError as e:
            check(e.errno == errno.EDQUOT, f"fsync: {e}")
        finally:
            os.close(fd)
        self.refills(a + "/f", stored(), "after the loss was reported")
        # The same, reported by the writer's own fsync.
        fd = os.open(a + "/f", os.O_WRONLY)
        # Less than a full write-back run, so it is the fsync that sends it.
        os.pwrite(fd, b"K" * (3 * MIB), 0)
        try:
            os.fsync(fd)
            raise Failed("fsync did not report the lost write-back")
        except OSError as e:
            check(e.errno == errno.EDQUOT, f"fsync: {e}")
        try:
            os.close(fd)
        except OSError:
            pass
        self.refills(a + "/f", stored(), "after a loss the writer's fsync reported")
        self.m.unmount(a)
        self.m.unmount(b)

    def failover(self, fs):
        a = self.m.mount(f"a-{fs}", fs, True, 0)
        data = self.bytes(2 * MIB)
        b = self.m.mount(f"b-{fs}", fs, False, 0)
        write(b + "/f", data)
        write(b + "/still", data)
        self.refills(a + "/f", data, "before the failover")
        self.refills(a + "/still", data, "an untouched file before the failover")
        old = self.c.leader()
        self.c.procs[old].kill()
        self.c.procs[old].wait()
        self.c.leader(but_not={old})
        data2 = self.bytes(2 * MIB)
        write(b + "/f", data2)
        self.refills(a + "/f", data2, "a write after the leader was killed")
        self.expect(a + "/still", data, True, "an untouched file after the failover")
        self.ok("an untouched file stays cached across a failover")
        self.c.start(old)
        time.sleep(2)
        self.m.unmount(b)

        # A leader that is paused, replaced and resumed may still think it leads. The keeping
        # mount lists it first and last talked to it.
        for round_ in range(4):
            leader = self.c.leader()
            first = [leader] + [n for n in self.c.urls if n != leader]
            urls = dict(self.c.urls)
            self.c.urls = {n: urls[n] for n in first}
            reader = self.m.mount(f"s{round_}-{fs}", fs, True, 0)
            self.c.urls = urls
            path = reader + "/f"
            old_data = open_read(path)[1]
            self.expect(path, old_data, True, "before pausing the leader")
            others = [n for n in self.c.urls if n != leader]
            self.c.procs[leader].send_signal(signal.SIGSTOP)
            try:
                self.c.leader(but_not={leader})
                writer = self.m.mount(f"w{round_}-{fs}", fs, False, 0, nodes=others)
                new = self.bytes(2 * MIB)
                write(writer + "/f", new)
                self.m.unmount(writer)
            finally:
                self.c.procs[leader].send_signal(signal.SIGCONT)
            self.expect(path, new, False, f"round {round_}: the old leader resumed")
            self.expect(path, new, None, f"round {round_}: and again")
            self.m.unmount(reader)
        self.ok("a paused and resumed ex-leader never served old data (4 rounds)")
        self.m.unmount(a)

    def restore(self, fs):
        a = self.m.mount(f"a-{fs}", fs, True, 0)
        b = self.m.mount(f"b-{fs}", fs, False, 0)
        data = self.bytes(4 * MIB)
        write(b + "/f", data)
        self.c.api("POST", f"/v1/fs/{fs}/snapshots", {"id": "s-" + fs, "name": "s"})
        half = 2 * MIB
        # `a` records the snapshot's state, caching only the first half.
        held = os.open(a + "/f", os.O_RDONLY)
        os.posix_fadvise(held, 0, 0, os.POSIX_FADV_RANDOM)
        os.close(os.open(a + "/f", os.O_RDONLY))
        read_fd(held, 0, PAGE)
        newer = self.bytes(half)
        write(b + "/f", newer, half)
        # The second half, as it is now, enters the cache under that record.
        check(read_fd(held, half, half) == newer, "the held descriptor read old data")
        os.close(held)
        self.c.api("DELETE", f"/v1/fs/{fs}")
        self.c.api(
            "POST", f"/v1/fs-snapshots/s-{fs}/clone", {"id": fs, "name": "restored"}
        )
        self.refills(a + "/f", data, "a restore by cloning the snapshot under the same id")
        self.m.unmount(a)
        self.m.unmount(b)
        snap = self.m.mount(f"snap-{fs}", fs, True, extra=("--snapshot", "s-" + fs))
        # The original filesystem is gone; its snapshot still reads.
        self.refills(snap + "/f", data, "a snapshot mount")
        self.m.unmount(snap)

    def run(self, rounds):
        for fs in ("remote0", "remote60", "conc", "lost", "fail", "restore"):
            self.c.api("POST", "/v1/fs", {"id": fs, "name": fs})
        print("remote changes, no attribute caching")
        self.remote_changes("remote0", 0, True)
        print("remote changes, attributes and read-ahead cached for 60 s (sizes fixed)")
        self.remote_changes("remote60", 60_000, False)
        print("concurrent writer and readers")
        self.concurrent("conc", rounds)
        print("lost write-back")
        self.lost_writes("lost")
        print("snapshot restore")
        self.restore("restore")
        print("leader failover and a stale leader")
        self.failover("fail")


def bench(cluster, mounts, mib, files, passes):
    cluster.api("POST", "/v1/fs", {"id": "bench", "name": "bench"})
    w = mounts.mount("bench-writer", "bench", False)
    rng = random.Random(1)
    write(w + "/big", rng.randbytes(mib * MIB))
    os.mkdir(w + "/small")
    for i in range(files):
        write(f"{w}/small/{i}", rng.randbytes(16 * 1024))
    mounts.unmount(w)

    def read_all(path):
        fd = os.open(path, os.O_RDONLY)
        try:
            n = 0
            while True:
                b = os.read(fd, MIB)
                if not b:
                    return n
                n += len(b)
        finally:
            os.close(fd)

    result = {}
    for repeat in range(2):
        for keep in (False, True) if repeat == 0 else (True, False):
            name = "on" if keep else "off"
            m = mounts.mount(f"bench-{name}-{repeat}", "bench", keep)
            small = [f"{m}/small/{i}" for i in range(files)]
            row = result.setdefault(
                name,
                {k: [] for k in ("cold_big", "cold_small", "big", "small", "open")},
            )
            # Two warm-up reads: the first fills (timed as "cold": nothing cached in this
            # mount), the second reaches the steady state.
            for warm in range(2):
                t = time.perf_counter()
                read_all(m + "/big")
                mid = time.perf_counter()
                for p in small:
                    read_all(p)
                if warm == 0:
                    row["cold_big"].append(mib / (mid - t))
                    row["cold_small"].append(files / (time.perf_counter() - mid))
            for _ in range(passes):
                t = time.perf_counter()
                check(read_all(m + "/big") == mib * MIB, "short read")
                row["big"].append(mib / (time.perf_counter() - t))
                t = time.perf_counter()
                for p in small:
                    check(read_all(p) == 16 * 1024, "short read")
                row["small"].append(files / (time.perf_counter() - t))
                t = time.perf_counter()
                for p in small:
                    os.close(os.open(p, os.O_RDONLY))
                row["open"].append((time.perf_counter() - t) / files * 1e6)
            if keep:
                row["counters"] = counters(m)
            mounts.unmount(m)
    print(
        f"repeated reads: one {mib} MiB file; {files} files of 16 KiB; "
        f"{passes} passes in each of 2 runs per setting; median (min..max)"
    )
    for name in ("off", "on"):
        row = result[name]
        for key, unit in (
            ("cold_big", "MiB/s, large file, first read"),
            ("cold_small", "files/s, small files, first read"),
            ("big", "MiB/s, large file"),
            ("small", "files/s, small files"),
            ("open", "us per open+close"),
        ):
            v = row[key]
            print(
                f"  keep-cache {name:3} {statistics.median(v):10.1f} "
                f"({min(v):.1f}..{max(v):.1f}) {unit}"
            )
    print("  counters of the last keeping mount:", result["on"]["counters"])
    print("BENCH_JSON=" + json.dumps(result))


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--node", required=True, help="atlas-native-node binary")
    p.add_argument("--mount", required=True, help="atlas-native-mount binary (fuse)")
    p.add_argument("--work", help="existing directory for data, logs and mount points")
    p.add_argument("--rounds", type=int, default=200, help="concurrent rewrites")
    p.add_argument("--bench", action="store_true")
    p.add_argument("--bench-mib", type=int, default=256)
    p.add_argument("--bench-files", type=int, default=1000)
    p.add_argument("--bench-passes", type=int, default=5)
    args = p.parse_args()
    work = tempfile.mkdtemp(prefix="keep-cache-", dir=args.work)
    mounts = cluster = None
    status = 1
    try:
        cluster = Cluster(os.path.abspath(args.node), work)
        mounts = Mounts(os.path.abspath(args.mount), cluster)
        if args.bench:
            bench(cluster, mounts, args.bench_mib, args.bench_files, args.bench_passes)
        else:
            suite = Suite(cluster, mounts)
            suite.run(args.rounds)
            print(f"{suite.passed} checks passed on Linux {os.uname().release}")
        status = 0
    except (Failed, OSError) as e:
        print(f"FAILED: {e!r}", file=sys.stderr)
        print(f"logs kept in {work}", file=sys.stderr)
    finally:
        if mounts:
            mounts.stop()
        if cluster:
            cluster.stop()
        if status == 0:
            shutil.rmtree(work, ignore_errors=True)
    print(f"KEEP_CACHE_LAB_EXIT={status}")
    return status


if __name__ == "__main__":
    sys.exit(main())
