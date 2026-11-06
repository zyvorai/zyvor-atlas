<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# atlas-native data-path performance (lab, spinning disk)

**These are not NVMe results, and not a PEAK:AIO (or any competitor) comparison.** They are one
run of the atlas-native engine data path on a shared, rotational disk on a single host. They exist
to show the benchmark harness works, to give per-I/O latency and CPU-cost figures, and to be the
baseline NVMe numbers will be compared against once NVMe hardware is available. Nothing here shows
multi-host scaling, RDMA, or direct-to-NVMe behaviour.

## What was measured

- Harness: `crates/atlas-native/tests/datapath_bench.rs` (`datapath_report`), driven by
  `deploy/native/bench.sh`. Run by hand with
  `ATLAS_BENCH_DIR=<dir> ATLAS_BENCH_BACKEND=file|aligned|io_uring|all cargo test --release -p atlas-native --test datapath_bench datapath_report -- --ignored --nocapture`.
- Topology: the engine plus **three data nodes in one process**, all on the same directory (so one
  physical disk), 3 replicas, 1 MiB extents, local metadata, TCP on loopback. Every logical byte
  written is therefore written three times to the same disk. Bandwidth below is **logical** MiB/s
  (what a client sees); the disk absorbed about three times that on writes.
- Phases (one client thread): 256 MiB sequential write then read in 8 MiB I/Os (32 ops); 200
  random 64 KiB writes then 200 random 64 KiB reads on the same volume. 64 KiB writes into 1 MiB
  extents are read-modify-write.
- Latency is per call to `engine.write`/`engine.read`. With 32 ops, p99 and p99.9 are the maximum
  sample; with 200 ops, p99.9 is the maximum. Treat tail figures as indicative only.
- CPU is `getrusage(RUSAGE_SELF)` user+system time over the phase, divided by MiB moved. The data
  nodes run in-process, so it covers client and servers together (not kernel I/O wait).
- Reads: the data files are `fsync`ed and `posix_fadvise(DONTNEED)`ed between the write and read
  phases, so reads mostly come from the disk. The system page cache is not dropped. Some reads
  can still be served from the page cache of other data.
- fio baseline (same directory, 256 MiB file, one job, `psync`): sequential 8 MiB `O_SYNC` write,
  sequential 8 MiB `O_DIRECT` read, random 64 KiB `O_SYNC` write and `O_DIRECT` read (13 MiB each).
  fio writes one copy, Atlas writes three, so fio is a ceiling for the filesystem, not a like-for-like
  baseline.

## Hardware and conditions

- One host (Linux, 12 CPUs, 31 GiB RAM), benchmark directory on `/data`: **XFS on a single
  spinning WD10EZEX 1 TB HDD** (`sdb`, rotational), mounted `noatime`. A directory on the
  filesystem was used; no block device was opened or written.
- **The host is shared**: it also runs a Ceph OSD and k3s, and load average was about 4-7 when sampled
  during the run, so these numbers are noisy and a quiet disk would do better. The benchmark ran under
  `nice -n 10`, inside the lab CI builder container (podman), one run, no repeats.
- Date: 2026-11-06. Raw output: `bench.json` produced by `deploy/native/bench.sh`.
- `io_uring` backend: it compiles with `--features io-uring` (`make native-features`), but the
  benchmark container's default seccomp profile blocks `io_uring_setup` (ENOSYS), so the harness
  reports it as `skipped`. **No io_uring number exists.** Running with the seccomp profile relaxed
  was not done.

## Results (verbatim from the run)

Atlas, logical MiB/s, latencies in ms, CPU in ms per MiB:

| backend | phase | MiB/s | p50 ms | p99 ms | p99.9 ms | CPU ms/MiB | ops |
|---|---|---|---|---|---|---|---|
| file | seq write 8 MiB | 19.29 | 401.49 | 575.40 | 575.40 | 8.08 | 32 |
| file | seq read 8 MiB | 67.25 | 106.94 | 226.66 | 226.66 | 3.09 | 32 |
| file | rand write 64 KiB | 0.44 | 133.43 | 310.02 | 441.73 | 125.14 | 200 |
| file | rand read 64 KiB | 3.53 | 18.93 | 59.01 | 147.11 | 32.99 | 200 |
| aligned | seq write 8 MiB | 16.16 | 483.36 | 708.83 | 708.83 | 10.13 | 32 |
| aligned | seq read 8 MiB | 56.08 | 122.97 | 300.72 | 300.72 | 3.12 | 32 |
| aligned | rand write 64 KiB | 0.38 | 158.39 | 358.42 | 450.09 | 139.14 | 200 |
| aligned | rand read 64 KiB | 3.76 | 14.96 | 51.77 | 80.68 | 34.31 | 200 |
| io_uring | all | skipped: `io error: Function not implemented (os error 38)` | | | | | |

fio, same directory (single copy):

| phase | MiB/s | IOPS | p50 ms | p99 ms | p99.9 ms |
|---|---|---|---|---|---|
| seq write 8 MiB, O_SYNC | 54.94 | 6.87 | 116.92 | 242.22 | 242.22 |
| seq read 8 MiB, O_DIRECT | 155.53 | 19.44 | 46.92 | 80.22 | 80.22 |
| rand write 64 KiB, O_SYNC | 0.68 | 10.86 | 83.36 | 267.39 | 291.50 |
| rand read 64 KiB, O_DIRECT | 11.45 | 183.26 | 5.41 | 10.68 | 15.66 |

## Reading the numbers

- Sequential writes at 16-19 MiB/s logical are about 48-58 MiB/s of physical writes (3 replicas),
  in line with fio's 55 MiB/s `O_SYNC` write to a single copy: on this disk the HDD, not the
  engine, is the limit. Reads are at 36-43% of fio's `O_DIRECT` read (the cause of the gap was not investigated).
- Random 64 KiB writes are a few hundred ms because of the read-modify-write on the replicas plus
  seeks; CPU per MiB looks high there only because each MiB is 16 tiny operations.
- The `file` and `aligned` backends are within run-to-run noise of each other on this disk. This
  run does not show any benefit of the aligned backend; that is expected on an HDD.

## Not covered

- **No NVMe.** No NVMe drive, no `O_DIRECT` NVMe path, no queue-depth scaling, and no
  `io_uring` result.
- **No multi-host scaling.** One host, one disk, loopback networking, three nodes in one process.
  Scaling versus node and client count is untested.
- No RDMA (`rdma` feature needs libibverbs headers, absent on the host: skipped), no FUSE-mount
  workload, no MLPerf Storage run (`docs/MLPERF.md` still has no benchmark result).
- One run, no repeats, no confidence intervals, a noisy shared host.
- Not comparable with any other vendor's published figures (PEAK:AIO included): different
  hardware, different workload, different durability setting (every Atlas write is synced on 3
  replicas).
