<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# atlas-native-ublk: block volumes as Linux block devices

`atlas-native-ublk` (`crates/atlas-native-ublk`) attaches one atlas-native block volume as
`/dev/ublkbN` through [ublk](https://docs.kernel.org/block/ublk.html), the counterpart of Ceph's
krbd. It is a userspace server: the kernel passes it block requests, and it turns them into calls on
the cluster's volume API (`docs/NATIVE_NODE.md`).

**Status: experimental.** The kernel path has been exercised on one lab host, with one kernel (7.0)
and the whole cluster on that host (see [Verified](#verified-and-not)).

## Requirements

- Linux with `ublk_drv` (`CONFIG_BLK_DEV_UBLK`). `libublk`, the library used, states Linux 6.0 or
  newer; only 7.0 has been tried here.
- `CAP_SYS_ADMIN` (in practice root). Unprivileged ublk is not supported. `CAP_SYS_RESOURCE` is
  used for `PR_SET_IO_FLUSHER`; without it the server warns and runs.
- Nodes whose data read routes run a read barrier (`docs/NATIVE_NODE.md`). **Do not use it against
  older nodes:** they answer reads from whatever a replica has applied, which can be old data, and
  the client cannot tell. Upgrade every metadata node first.

## Build and run

```sh
cargo build --release -p atlas-native-ublk --features ublk

sudo modprobe ublk_drv
sudo atlas-native-ublk --endpoint https://n1:7480,https://n2:7480,https://n3:7480 \
    --volume <id> --token-file token --ca-file ca.pem
# stdout: device /dev/ublkb0
```

It stays in the foreground. SIGINT, SIGTERM or SIGHUP stops the device: unmount it first. While it
is still mounted the server does not exit; it does once the filesystem is unmounted. Requests the
kernel still has in flight are served normally while stopping; a second signal fails the ones still
waiting for the cluster. Without `--features ublk` (or on another OS) the binary only checks the
volume. `--check` does that with either build, without privileges.

| Option | Default | Meaning |
| --- | --- | --- |
| `--read-only` | off | Read-only device. |
| `--block-size` | 4096 | Logical block size: 512 to 4096. A volume whose size is not a multiple is cut short. |
| `--queues` / `--queue-depth` | 2 / 16 | Hardware queues and requests in flight on each. Each request slot has a thread and an `--io-bytes` buffer. |
| `--io-bytes` | 4 MiB | Largest read or write the device accepts (4 KiB multiple, at most 32 MiB). |
| `--max-request-bytes` | 8 MiB | Largest cluster request; must not exceed the nodes' `max_request_bytes`. Checked at attach with one read of that size. |
| `--io-timeout-secs` | 60 | Retry a request this long, then fail it with an I/O error. 0 retries for ever: I/O hangs while the cluster is unavailable. |
| `--request-timeout-secs` | 30 | Give up on one unanswered cluster request and try again. |
| `--settle-secs` | 15 | See [Late writes](#late-writes). |
| `--device-id` | -1 | Device number; -1 takes a free one. |
| `--share-mount-namespace` | off | Stay in the caller's mount namespace. See [Mount namespace](#mount-namespace). |
| `--delete-device N` | | Remove the device a killed server left behind, and exit. Needs no cluster. Refused unless the device is stopped and no process serves it. See [Server death](#server-death). |

Authentication and TLS options (`--token-file`, `--ca-file`, `--identity-file`) are those of
`atlas-native-mount`, and the client is the same (`atlas_native_fuse::client`).

## Behaviour

**Durability.** There is no write cache. A write completes only after the metadata leader answered
success, which it does after the data nodes stored the extents and the Raft group committed them.
The device therefore reports no volatile cache (`write through`), the kernel sends no flushes, and a
flush that arrives anyway succeeds at once. How durable "stored" is depends on the cluster's
`replicas` or `erasure` setting and the data nodes' device backend.

**Reads.** The node runs a Raft read barrier in the same request that reads the data, so a read
includes every write completed before it, whichever replica answers (but see the
[known gap](#known-gap-in-the-read-barrier)). A replica that cannot reach a leader answers 503 and
the client asks the next one. The client never sends `stale=1`.

**Retries.** 421 (not leader), 503, 500, 429, 408, 502, 504, `busy` and connection failures are
retried on the next endpoint, with a pause (up to 1 s) after each full round. Other answers fail the
request at once: a deleted volume or bad credentials give `EIO`, quota `ENOSPC`, a read-only
refusal `EROFS`. A cluster that is out of space answers 503, so it shows as `EIO` after the
timeout. A request that fails may have been partly applied.

**Splitting.** Requests above `--max-request-bytes` are split. A write that covers whole extents is
also cut at the extent boundaries around them, which sends those on the path that reads nothing
back. Pieces go one after another; different requests run concurrently. There is no atomicity above
one block.

**Write zeroes** writes zeros (the blocks stay allocated). **Discard is not offered**: the volume
API cannot release blocks, so `fstrim` and `blkdiscard` report that it is unsupported.

### Mount namespace

By default the server moves to its own mount namespace, so that it is not the process whose exit
unmounts its own device (that could deadlock: the unmount waits for I/O only this process can
serve). The mounts there are slaves of the caller's: an unmount on the host reaches the server's
namespace, and nothing the server mounts reaches the host. This relies on the caller's mounts being
`shared` (the systemd default); where they are private, the server's copies stay private and it keeps
whatever was mounted when it started.

An earlier version made the mounts private, and a server then kept every filesystem that existed when
it started mounted after the host had unmounted it. With two devices, unmounting a filesystem on the
first, mounted before the second server started, succeeded on the host but left that filesystem
active (still listed in `/sys/fs/ext4`), and `mke2fs` reported the device in use.

### Run directory

`libublk` records each device in `/run/ublksrvd/NNNN.json`. It creates that directory writable by
everyone, without the sticky bit, and writes the file following symlinks. On the lab host an
unprivileged user's symlink there made a server running as root overwrite the root-owned file it
pointed at (`fs.protected_symlinks` does not cover a directory without the sticky bit). The server
now creates the directory, or takes it over, before `libublk` writes there: owned by its user, no
write permission for others, symlinks removed. Another ublk program on the same host that relies on
the directory being world-writable (unprivileged ublk) would stop working. A program that creates the
directory afresh and runs without this step is still exposed.

### Server death

If the server is killed, the kernel fails its requests and removes `/dev/ublkbN`, but keeps the
device number (`/dev/ublkcN`, `/sys/class/ublk-char/ublkcN`). Unmount the filesystem, run
`atlas-native-ublk --delete-device N`, wait at least the settle time, because writes the dead server
had in flight may still be applied, and attach again. Writes that had completed, those an `fsync`
had returned for included, are on the cluster. `--delete-device` removes any stopped ublk device that
no process serves, whichever program created it.

### Late writes

The volume write API has no sequence number or fencing token. If the client gives up on a write that
got no answer (timeout, broken connection), the node may still be working on it, and it could land
after a newer write to the same blocks. The client does not complete such a request, whether the
retry succeeded or failed, until `--settle-secs` has passed since it gave up, so nothing the kernel
orders after it can be overtaken **if the abandoned request finishes within that time**. The server
does not bound that time, so this narrows the window and does not close it. A connection that was
refused is known not to have been delivered and is not held. Expect writes to stall for up to the
settle time during a failover.

## Limitations

- **One attachment per volume.** Nothing in the cluster stops a second writer, on this host or
  another, and there is no fencing: a write still in flight from a dead or partitioned client can
  land after a new client attached. The operator must ensure single attach.
- Writes smaller than an extent make the leader read, merge and rewrite the whole extent under a
  per-group lock, so small random writes are slow and serialised on the server. A smaller
  `extent_bytes` reduces the cost. Performance has not been benchmarked.
- **On slow storage small random writes can fail.** Seen once, with three nodes on one 7200 rpm disk
  while a build ran on the same host, and 1 MiB extents: fio 4 KiB random writes at queue depth 16
  completed about one write a second, requests went unanswered until the client gave up on them, and
  a write failed with `EIO` at the 60 s I/O timeout. A request that is retried may still be queued on
  the server, which adds load. Longer `--io-timeout-secs` and `--request-timeout-secs`, a lower
  queue depth or smaller extents ought to help; none of that was tried.
- The device does not follow a volume resize; reattach.
- No ublk user recovery: if the server dies, I/O on the device fails and the filesystem must be
  remounted after a new attach (see [Server death](#server-death)).
- Snapshots cannot be attached; clone one into a volume.
- Do not put swap on the device. Do not mount it on a host that runs the cluster's own data nodes
  without considering that writeback then depends on memory the same host needs.
- No packaging: no container image, Helm chart or CSI integration.

## Licence of the `ublk` feature

The feature adds `libublk` (MIT OR Apache-2.0) and its dependencies. One of them, `bitmaps` 3.2.1,
is **MPL-2.0+**, which `deny.toml` does not allow: `cargo deny --features atlas-native-ublk/ublk
check licenses` (cargo-deny 0.22.2) rejects it. That is the only finding the feature adds; its
advisory, ban and source results match those of the default features. The feature is off by default,
so a default-feature check does not include `bitmaps`. Distributing a binary built with the feature
needs a licence decision first.

## Layout

| Module | Content |
| --- | --- |
| `volume` | Block requests to cluster calls: durability, retries, failover, late writes. Portable. |
| `split` | Request splitting. Portable. |
| `dispatch` | Worker threads; one completion per request. Portable. |
| `params` | Device parameters and result codes. Portable. |
| `ublk` | The kernel server over `libublk` (`ublk` feature, Linux). One thread per queue; a worker signals each completion through an eventfd that the queue's io_uring polls. |

## Verified, and not

Verified by tests against in-process nodes (`cargo test -p atlas-native-ublk`, no kernel):
request round trips and splitting, never-written ranges, write zeroes, refusals (alignment, range,
read-only, discard), concurrent writes into shared extents, leader failover under writes, a cut-off
follower never answering with old bytes, no acknowledgement when replicas cannot be stored, the
settle hold, deleted volumes, two metadata groups. The read barrier on the node side is covered by
`crates/atlas-native/tests/read_barrier.rs`. With and without the `ublk` feature the crate passes
clippy `-D warnings` and its tests on Linux (x86-64, Rust 1.98.1).

### On a kernel

2026-11-28, one lab host: Ubuntu 26.04.1, kernel 7.0.0-44-generic, x86-64, in-tree `ublk_drv`, fio
3.41, release binaries. `deploy/native/ublk-live-check.sh`, with three nodes on localhost (2
replicas, 1 MiB extents), passed with the cluster's data on a tmpfs. The build before the
[run directory](#run-directory) change had passed twice that way, and once with the data on a
7200 rpm disk and the fio steps skipped. It checks:

- the device's size and queue limits: `write through`, no discard, write zeroes;
- that other users cannot write `/run/ublksrvd` (a symlink planted there by an unprivileged user was
  separately seen removed, its target untouched);
- direct I/O compared with the volume read over HTTP, the last block, a refused write past the end;
- fio with crc32c verification (4 KiB random writes at queue depth 16, and 4 KiB to 1 MiB with four
  jobs), verified again after a server restart, and failing on a block overwritten on purpose;
- reads and writes failing after `--io-timeout-secs` with all nodes stopped, and working again;
- with `--io-timeout-secs 0`, a write waiting through the first signal (server CPU near zero) and
  failing at the second, after which the server exits;
- ext4: the metadata leader SIGKILLed during writes, file contents compared, `e2fsck -fn`, then a
  reattach and checksums;
- SIGINT with the filesystem still mounted read-only: the server was still up 10 s later, and exited
  after the unmount;
- the server SIGKILLed during writes to the mounted filesystem: the writer got an error within a
  second, `--delete-device` removed what was left, `e2fsck -p` replayed the journal, and every file
  whose `fsync` had returned had its contents;
- refusal of `--delete-device` on a live device, a read-only attach, 512-byte blocks;
- the server's own mount namespace, and an unmount on the host reaching it.

The kernel log showed no warning, oops or hung-task message, only the I/O errors the failure steps
provoke. Killed or restarted nodes keep their data in the page cache or tmpfs, so nothing here tests
durability against power loss.

**Not verified:** other kernels or architectures; behaviour under memory pressure (what
`PR_SET_IO_FLUSHER` is for); TLS; nodes on other hosts, or network partitions seen through the block
device; the late-write window; `--block-size` 1024 or 2048, `--share-mount-namespace`,
`--device-id`, queue settings other than the default and one queue of depth 4; stopping with writes
in flight to a filesystem mounted read-write; long runs; performance.

```sh
# As root, with ublk_drv loaded. Starts its own three-node cluster on localhost and only writes to
# the devices its own servers create. TMPDIR holds the cluster's data; use fast storage.
sudo ATLAS_UBLK_LIVE=yes TMPDIR=/path/with/space deploy/native/ublk-live-check.sh \
    target/release/atlas-native-node target/release/atlas-native-ublk
```

### What the read barrier relies on

A leader may skip the quorum round while it holds a read lease, granted only by a ReadIndex it
confirmed with a quorum and counted from when that ReadIndex was sent. An earlier version also
granted one from the check-quorum round, which let a just-partitioned leader answer stale reads for
a short window; that grant is removed. The lease is safe only if the leader's monotonic clock does
not run much slow. Partition behaviour has not been tested through HTTP or the block device.
