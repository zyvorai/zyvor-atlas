#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
"""Deterministic fixtures of the agent profile, generated once on the benchmark host.

Usage: agent_fixtures.py ensure <dir> [options]    generate if <dir> is absent, otherwise check it
       agent_fixtures.py check <dir> [options]

  --seed N (1)  --files N (2000)  --repo-files N (1000)  --commits N (20)
  --fork-sizes N,N,N (1000,10000,100000)  --fork-file-bytes N (1024)

Written to <dir>:
  pkgs.tar              files d%03d/f%05d, 20 per directory; exactly 70% of 200 B-4 KiB, 25% of
                        4-32 KiB and 5% of 32-256 KiB (a chosen stand-in, not measured agent traffic)
  repo.git              bare repository with fixed authors and dates, built by git fast-import
  manifest-pkgs.sha256  every file of pkgs.tar
  manifest.sha256       those, and the working tree of a clone in src/ after the 20 fixed edits
  base-<N>.tar, manifest-base-<N>.sha256    the trees of phase D: N files of one size
  fixtures.json         parameters, git object ids and the SHA-256 of every file above

File contents and sizes come from SHAKE-128 of the seed and the path, so a rerun gives the same
hashes on any machine. repo.git has the same object ids everywhere; its pack bytes depend on the git
version, which fixtures.json records.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.dont_write_bytecode = True
import agent_lib as lib  # noqa: E402

FORMAT = 1
PER_DIR = 20
MTIME = 1700000000
EDITS = 20
CLASSES = ((200, 4096), (4097, 32768), (32769, 262144))
SHARES = (0.25, 0.05)  # medium and large; the rest is small

DEFAULTS = {
    "seed": 1,
    "files": 2000,
    "repo_files": 1000,
    "commits": 20,
    "fork_sizes": [1000, 10000, 100000],
    "fork_file_bytes": 1024,
}


def stream(label, size):
    return hashlib.shake_128(label.encode()).digest(size)


def number(label):
    return int.from_bytes(hashlib.sha256(label.encode()).digest()[:8], "big")


def tree_path(i):
    return f"d{i // PER_DIR:03d}/f{i:05d}"


def pkg_sizes(seed, count):
    """{path: size}, with exact class shares: which file is in which class follows from the seed."""
    paths = [tree_path(i) for i in range(count)]
    medium, large = round(count * SHARES[0]), round(count * SHARES[1])
    order = sorted(paths, key=lambda p: number(f"{seed}:class:{p}"))
    classes = [0] * (count - medium - large) + [1] * medium + [2] * large
    sizes = {}
    for path, c in zip(order, classes):
        lo, hi = CLASSES[c]
        sizes[path] = lo + number(f"{seed}:size:{path}") % (hi - lo + 1)
    return {p: sizes[p] for p in paths}


def write_tar(path, files):
    """A USTAR archive of {path: bytes} with fixed metadata, directories before their files."""

    def info(name, kind, mode, size):
        t = tarfile.TarInfo(name)
        t.type, t.mode, t.size, t.mtime = kind, mode, size, MTIME
        t.uid = t.gid = 0
        t.uname = t.gname = "root"
        return t

    seen = set()
    with tarfile.open(path, "w", format=tarfile.USTAR_FORMAT) as tar:
        for name, data in files:
            parts = name.split("/")
            for depth in range(1, len(parts)):
                d = "/".join(parts[:depth])
                if d not in seen:
                    seen.add(d)
                    tar.addfile(info(d, tarfile.DIRTYPE, 0o755, 0))
            tar.addfile(info(name, tarfile.REGTYPE, 0o644, len(data)), io.BytesIO(data))


# ---------------------------------------------------------------- the repository


def text_file(label, size):
    """`size` bytes of source-like text."""
    out, i = [], 0
    length = 0
    while length < size:
        line = f"value_{i} = '{stream(f'{label}:{i}', 20).hex()}'\n"
        out.append(line)
        length += len(line)
        i += 1
    return "".join(out).encode()[: size - 1] + b"\n"


def repo_path(i):
    return f"m{i // PER_DIR:02d}/f{i:04d}.py"


def repo_history(seed, files, commits):
    """[{path: bytes}] of what each commit writes: the first adds every file, each later one
    rewrites a fortieth of them."""
    def content(path, version):
        label = f"{seed}:repo:{path}:{version}"
        return text_file(label, 200 + number(label) % 3897)

    history = [{repo_path(i): content(repo_path(i), 0) for i in range(files)}]
    changed = max(1, files // 40)
    for c in range(1, commits):
        order = sorted(range(files), key=lambda i: number(f"{seed}:commit:{c}:{i}"))
        history.append({repo_path(i): content(repo_path(i), c) for i in sorted(order[:changed])})
    return history


def object_id(kind, data):
    return hashlib.sha1(b"%s %d\0" % (kind.encode(), len(data)) + data).digest()


def git_tree_id(files):
    """The id git gives the tree holding {path: bytes}, all regular non-executable files."""
    root = {}
    for path, data in files.items():
        node = root
        parts = path.split("/")
        for p in parts[:-1]:
            node = node.setdefault(p, {})
        node[parts[-1]] = data

    def build(node):
        entries = []
        for name, value in node.items():
            if isinstance(value, dict):
                entries.append((name.encode() + b"/", b"40000 " + name.encode() + b"\0" + build(value)))
            else:
                entries.append((name.encode(), b"100644 " + name.encode() + b"\0" + object_id("blob", value)))
        return object_id("tree", b"".join(e for _, e in sorted(entries)))

    return build(root).hex()


def fast_import_stream(history):
    out = io.BytesIO()
    for n, changes in enumerate(history, 1):
        when = MTIME + n * 3600
        message = f"commit {n}\n".encode()
        out.write(b"commit refs/heads/main\nmark :%d\n" % n)
        for role in (b"author", b"committer"):
            out.write(role + b" bench <bench@invalid> %d +0000\n" % when)
        out.write(b"data %d\n" % len(message) + message)
        if n > 1:
            out.write(b"from :%d\n" % (n - 1))
        for path in sorted(changes):
            data = changes[path]
            out.write(b"M 100644 inline %s\ndata %d\n" % (path.encode(), len(data)) + data + b"\n")
        out.write(b"\n")
    return out.getvalue()


def build_repo(dest, history):
    with tempfile.TemporaryDirectory(dir=dest.parent) as scratch:
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": scratch,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
        }

        def git(*args, **kw):
            return subprocess.run(["git", *args], env=env, check=True, stdout=subprocess.PIPE, **kw).stdout

        empty = Path(scratch) / "template"
        empty.mkdir()
        git("init", "--quiet", "--bare", "--initial-branch=main", f"--template={empty}", str(dest))
        git("-C", str(dest), "fast-import", "--quiet", input=fast_import_stream(history))
        head = git("-C", str(dest), "rev-parse", "main").decode().strip()
        tree = git("-C", str(dest), "rev-parse", "main^{tree}").decode().strip()
        version = git("--version").decode().strip()
    return head, tree, version


# ---------------------------------------------------------------- the set


def file_hashes(root):
    return {p: lib.sha256_file(os.path.join(root, p)) for p in lib.walk_files(root) if p != "fixtures.json"}


def generate(dest, params):
    dest = Path(dest)
    if dest.exists():
        raise SystemExit(f"{dest} already exists")
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = Path(tempfile.mkdtemp(prefix=dest.name + ".tmp-", dir=dest.parent))
    try:
        seed = params["seed"]
        pkgs = {p: stream(f"{seed}:data:{p}", n) for p, n in pkg_sizes(seed, params["files"]).items()}
        write_tar(tmp / "pkgs.tar", pkgs.items())
        manifest = {p: lib.sha256_bytes(d) for p, d in pkgs.items()}
        (tmp / "manifest-pkgs.sha256").write_text(lib.format_manifest(manifest))

        history = repo_history(seed, params["repo_files"], params["commits"])
        head, tree, git_version = build_repo(tmp / "repo.git", history)
        work = {}
        for changes in history:
            work.update(changes)
        if git_tree_id(work) != tree:
            raise SystemExit("the computed tree differs from git's: the expected results would be wrong")
        edited = lib.edit_targets(work, min(EDITS, len(work)))
        after = dict(work)
        for p in edited:
            after[p] = work[p] + b"bench\n"
        manifest.update({f"src/{p}": lib.sha256_bytes(d) for p, d in after.items()})
        (tmp / "manifest.sha256").write_text(lib.format_manifest(manifest))

        size = params["fork_file_bytes"]
        for n in params["fork_sizes"]:
            hashes = {}

            def files(count=n, hashes=hashes):
                for i in range(count):
                    p = tree_path(i)
                    data = stream(f"{seed}:base:{p}", size)
                    hashes[p] = lib.sha256_bytes(data)
                    yield p, data

            write_tar(tmp / f"base-{n}.tar", files())
            (tmp / f"manifest-base-{n}.sha256").write_text(lib.format_manifest(hashes))

        info = {
            "format": FORMAT,
            "params": params,
            "git_version": git_version,
            "head": head,
            "tree": tree,
            "tree_after_edits": git_tree_id(after),
            "edited": edited,
            "files": file_hashes(tmp),
        }
        (tmp / "fixtures.json").write_text(json.dumps(info, indent=1, sort_keys=True) + "\n")
        # Readable by the container's root, which maps to this user.
        os.chmod(tmp, 0o755)
        tmp.rename(dest)
    finally:
        if tmp.exists():
            shutil.rmtree(tmp)
    return info


def load(directory):
    return json.loads((Path(directory) / "fixtures.json").read_text())


def check(directory, params):
    """Problems with an existing fixture directory."""
    try:
        info = load(directory)
        problems = []
        if info.get("format") != FORMAT:
            problems.append(f"fixture format {info.get('format')}, expected {FORMAT}")
        if info.get("params") != params:
            problems.append(f"generated with {info.get('params')}, wanted {params}")
        recorded = info["files"]
        found = file_hashes(directory)
    except (OSError, ValueError, KeyError) as e:
        return [f"cannot read {directory}: {e!r}"]
    for p in sorted(set(recorded) | set(found)):
        if recorded.get(p) != found.get(p):
            problems.append(f"{p}: " + ("missing" if p not in found else "not recorded" if p not in recorded else "differs"))
    return problems


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=["ensure", "check"])
    parser.add_argument("directory")
    for key in ("seed", "files", "repo_files", "commits", "fork_file_bytes"):
        parser.add_argument("--" + key.replace("_", "-"), type=int, default=DEFAULTS[key])
    parser.add_argument("--fork-sizes", default=",".join(str(n) for n in DEFAULTS["fork_sizes"]))
    args = parser.parse_args(argv)
    params = {k: getattr(args, k) for k in ("seed", "files", "repo_files", "commits", "fork_file_bytes")}
    params["fork_sizes"] = [int(n) for n in args.fork_sizes.replace(",", " ").split()]
    if min([params[k] for k in ("files", "repo_files", "commits", "fork_file_bytes")] + params["fork_sizes"]) < 1:
        parser.error("counts and sizes must be positive")
    return args.command, args.directory, params


def main(argv):
    command, directory, params = parse_args(argv)
    if command == "ensure" and not os.path.exists(directory):
        info = generate(directory, params)
        print(f"generated {directory}: {len(info['files'])} files, repository head {info['head']}")
    else:
        problems = check(directory, params)
        if problems:
            print("\n".join(problems[:20]), file=sys.stderr)
            print(f"{directory} does not match; remove it or choose another directory", file=sys.stderr)
            return 1
        print(f"{directory} matches: head {load(directory)['head']}")
    for name, digest in sorted(load(directory)["files"].items()):
        if not name.startswith("repo.git/"):
            print(f"{digest}  {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
