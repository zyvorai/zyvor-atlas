#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
"""What must be written ahead of an append position so that fdatasync stops committing metadata.

Usage: prealloc-syncs.py <empty directory>

For each way of preparing a file, writes 4 KiB blocks one after another into it, each followed by
fdatasync. While doing so the process is named fx-<layout>, so that a tracer can count the
filesystem log forces each layout causes (prealloc-probe.sh does, on XFS). This script only does the
I/O and shows the extents before and after; it measures nothing by itself.
"""

import ctypes
import os
import shutil
import subprocess
import sys
from pathlib import Path

BLOCK = 4096
WRITES = 200
SIZE = BLOCK * WRITES
LAYOUTS = ("append", "sparse", "fallocate", "zeroed", "zero-nosync")

libc = ctypes.CDLL(None, use_errno=True)
libc.fallocate.argtypes = (ctypes.c_int, ctypes.c_int, ctypes.c_long, ctypes.c_long)


def set_name(name):
    libc.prctl(15, name.encode(), 0, 0, 0)  # PR_SET_NAME


def prepare(fd, layout):
    if layout == "sparse":  # the size is set, no blocks
        os.ftruncate(fd, SIZE)
    elif layout == "fallocate":  # size and blocks, as unwritten extents
        if libc.fallocate(fd, 0, 0, SIZE) != 0:
            raise OSError(ctypes.get_errno(), "fallocate")
    elif layout.startswith("zero"):  # written
        os.pwrite(fd, bytes(SIZE), 0)
    if layout != "zero-nosync":
        os.fsync(fd)


def extents(path):
    if shutil.which("filefrag") is None:
        return "filefrag is not installed"
    # No -s: it would sync the file and change what is being looked at.
    return subprocess.run(["filefrag", "-v", path], capture_output=True, text=True).stdout.strip()


def main():
    directory = Path(sys.argv[1])
    directory.mkdir(parents=True, exist_ok=True)
    data = os.urandom(BLOCK)
    for layout in LAYOUTS:
        path = str(directory / layout)
        fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            prepare(fd, layout)
            print(f"== {layout}: before\n{extents(path)}")
            set_name(f"fx-{layout}")
            for i in range(WRITES):
                os.pwrite(fd, data, i * BLOCK)
                os.fdatasync(fd)
            set_name("python3")
            print(f"== {layout}: after {WRITES} x (4 KiB write, fdatasync)\n{extents(path)}")
            print(f"SYNCS {layout} {WRITES}", flush=True)
        finally:
            set_name("python3")
            os.close(fd)
            os.unlink(path)


if __name__ == "__main__":
    main()
