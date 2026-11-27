<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# atlas-native-ublk: block volumes as Linux block devices

`atlas-native-ublk` (`crates/atlas-native-ublk`) attaches one atlas-native block volume as
`/dev/ublkbN` through [ublk](https://docs.kernel.org/block/ublk.html), the counterpart of Ceph's
krbd. It is a userspace server: the kernel passes it block requests, and it turns them into calls on
the cluster's volume API (`docs/NATIVE_NODE.md`).

**Status: experimental. The kernel path has never been run** (see [Verified](#verified-and-not)).

## Requirements

- Linux with `ublk_drv` (`CONFIG_BLK_DEV_UBLK`). `libublk`, the library used, states Linux 6.0 or
  newer.
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

It stays in the foreground. SIGINT, SIGTERM or SIGHUP stops the device: unmount it first. Requests
the kernel still has in flight are served normally while stopping; a second signal fails the ones
still waiting for the cluster. Without `--features ublk` (or on another OS) the binary only
checks the volume. `--check` does that with either build, without privileges.

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
| `--share-mount-namespace` | off | By default the server moves to a private mount namespace, so it cannot deadlock by being the one who unmounts its own device on exit. |

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
  `extent_bytes` reduces the cost. No performance has been measured.
- The device does not follow a volume resize; reattach.
- No ublk user recovery: if the server dies, I/O on the device fails and the filesystem must be
  remounted after a new attach. A device left behind by a killed server has to be removed with a
  ublk tool.
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
`crates/atlas-native/tests/read_barrier.rs`. With the `ublk` feature the crate builds, links and
passes clippy and the same tests on Linux (x86-64, Rust 1.98.1, in a container without ublk).

**Not verified:** everything that needs the kernel. No ublk device has been created, so the
`ublk` module has never executed: device creation and parameters, the queue and eventfd loop,
signals and shutdown, the private mount namespace, filesystems on the device, and behaviour under
memory pressure are untested. `deploy/native/ublk-live-check.sh` exercises all of these but memory
pressure; it needs root and has not been run (only `bash -n` and shellcheck). No performance, TLS
or multi-host test was done.

```sh
# As root, with ublk_drv loaded. Starts its own three-node cluster on localhost and only touches
# the device its own server creates.
sudo ATLAS_UBLK_LIVE=yes deploy/native/ublk-live-check.sh \
    target/release/atlas-native-node target/release/atlas-native-ublk
```

### Known gap in the read barrier

The read guarantee is only as good as the engine's read barrier, and that has a hole. A leader may
skip the quorum round while it holds a read lease, and the check-quorum round grants one
(`RaftNode::tick` in `crates/atlas-native/src/raft.rs`) on acknowledgements that can be almost an
election timeout old. A leader that was just partitioned can therefore take a lease while the other
voters are electing a successor, and answer reads from it for up to a quarter of the election
timeout (125 ms with the default tick) after the new leader committed writes.

This was reproduced with a throwaway test on the sans-I/O `RaftNode` (not committed): the cut-off
leader, still believing it led, granted a local read at index 3 after the new leader had applied
index 5. It was not reproduced through HTTP or the block device. A client reaches it only if it
reads from the old leader inside that window after having a write acknowledged by the new one. The
fix belongs in the Raft code (grant a lease only from a quorum answer to a round whose send time
bounds the lease, not from the check-quorum round).
