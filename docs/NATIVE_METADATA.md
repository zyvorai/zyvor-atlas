<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Atlas Native metadata durability — Phases 2 and 3

Phase 2 introduced a deterministic metadata state machine and a write-ahead log (WAL). Phase 3 adds
WAL checkpoint/compaction, device-space free lists, and a Raft core that replicates the same log.

## Commit invariant (single-node engine)

For every metadata mutation:

1. build a `MetaCommand`;
2. under the catalog write lock, apply it in place; a command that fails validation is rejected
   here, leaves the catalog untouched (asserted in debug builds) and is never logged;
3. append `{term,index,command}` to `metadata.wal`;
4. `fsync` the WAL;
5. release the lock, publishing the new state. If the append fails, the catalog is reloaded from
   disk (the catalog store plus the WAL), since whether the record landed is unknown.

The catalog store is only written at a checkpoint (below), not on every commit, so a commit costs
one fsync.

On restart, Atlas loads the catalog store and replays every WAL record whose index is greater than
`catalog.applied_index`. A torn final WAL line (a record that never finished its fsync, so was never
acknowledged) is discarded on open; corruption anywhere else is a hard error.

## Catalog store

The catalog is checkpointed to `catalog.redb`, an embedded [redb](https://github.com/cberner/redb)
key-value store with one record per volume, volume snapshot, extent, filesystem, inode, directory
entry and filesystem snapshot, plus one record for the rest (applied index, term, free list, membership,
Raft addresses). The catalog's large maps record which keys change between checkpoints
(`tracked::Tracked`: every mutation goes through it, so the record is complete by construction),
and a checkpoint writes only those records, in one durable transaction. A checkpoint therefore
costs time proportional to what changed since the last one, not to the size of the catalog. A
catalog the store doesn't already hold (a new one, one read from a legacy `catalog.json`, or one
installed from a Raft snapshot) is written in full.

Upgrading: a node that finds a `catalog.json` and an empty store loads the JSON, writes it to the
store and then deletes `catalog.json`. The store is preferred whenever it holds a checkpoint, so a
crash between those steps is harmless. There is no way back to `catalog.json`, so downgrading past
this version means restoring a backup.

A directory's inode record carries its entry count, not its entries; each entry is its own
`(fs, dir, name) -> ino` record, so a create in a directory of any size checkpoints five records (state, filesystem header,
new inode, directory inode, new entry). `NativeEngine::checkpoint` and Raft compaction write one
redb transaction; `store::bench` (ignored) measures it at ~4–5 ms per 1024 creates from 8k to 65k
inodes on tmpfs, growing only with the B-tree's depth.

### Paged inode tables

Inodes are the bulk of a catalog, and once the store holds a filesystem they are paged
(`inodes::InodeTable`). Memory keeps the inodes changed since the last checkpoint, plus a bounded
LRU cache of unchanged ones (`EngineConfig::catalog_cache_inodes` / `RaftConfig::catalog_cache_inodes`,
default 262,144). Everything else is read on demand from a redb read snapshot taken at the
checkpoint. A checkpoint hands the changed inodes to the next snapshot's cache, which keeps every
still-valid entry of the last one. A catalog cloned before a checkpoint keeps reading the
snapshot it was cloned on, so a Raft snapshot in flight or the leader's speculative catalog sees
one consistent state. Inodes are shared (`Arc`): reads take no copy, an edit copies only an inode
someone else still holds, and a filesystem snapshot or clone shares the inodes of the tree it
copies. Each filesystem keeps `file_bytes` and `used_bytes` counters in every command, so `statfs`
and the filesystem list never walk the inodes. Catalogs written before the counters existed are
counted once on load.

Directory entries are paged the same way (`inodes::DirEntries`). Once the store holds a
directory, its inode keeps only the entry count and the entries changed since the last checkpoint
(a removal is remembered as a changed name with no entry). A lookup that memory can't answer reads
the one `(fs, dir, name)` record, and a listing merges the changed names with a range read of the
store, a page at a time (`GET .../entries?after=&limit=`). A checkpoint writes the changed entries
and drops them from memory. A snapshot tree, a Raft snapshot chunk and the debug-build
consistency check read directories whole (`InodeTable::full`). Inode records written before
entries were paged hold `{}` in place of the count; their entries are counted when the inode is
read, until the directory is next written.

A store read error while applying a command panics: the command may be half applied, and a
replica that could read its store would accept it. A restart recovers from the checkpoint and the
log. A read error on a read path is returned to the caller.

`metadata_bench` (local engine, second lab host): ~100k creates/s from one thread at 40k files
with either a 4096-inode or a 262,144-inode cache. `store::bench::memory_of_a_paged_catalog`
(4096-inode cache, 400k files, second lab host) peaks at ~76 MB with every file in one directory
and ~78 MB spread over 1000, flat from 300k files on as redb's 64 MiB page cache fills. Before
directory entries were paged, the same runs peaked at 112–114 MB and were still growing.

Limits: filesystem snapshot trees are still held in memory in full (one `fs_snapshots` record
each). How they are stored and loaded today, and a
proposed versioned-record design, are in [NATIVE_SNAPSHOT_TREE_PAGING.md](NATIVE_SNAPSHOT_TREE_PAGING.md). Checkpoints run on the commit path, under the catalog lock.

## Checkpoint and WAL compaction

A checkpoint writes the catalog store, which then durably covers every applied index, so WAL
records at or below it are redundant. The engine checkpoints and compacts the WAL once it holds
`EngineConfig::wal_compact_after` records (default 1024, `0` disables) and on an explicit
`NativeEngine::checkpoint()`. Compaction rewrites the
log through a temp file + fsync + rename + directory fsync. After compaction the WAL may be empty, so
its index floor is raised to `catalog.applied_index` on open to keep indexes monotonic.

## Extent lifetime and space reuse

Each physical extent has a metadata refcount. Active volumes and snapshots both own references.
Overwrites decrement the previous active extent and install a new immutable extent. Snapshot delete
and volume delete decrement references. Zero-reference extents become GC candidates.

`gc_once` commits `MarkExtentReclaimed` for each candidate. Applying it removes the extent and returns
every replica's `(node, device, offset, len)` to the catalog's free list (`alloc::FreeList`: sorted,
non-overlapping, adjacent ranges coalesced; a double free is rejected).

Writes allocate first-fit from the free list before appending to the device. The data is written to
the free range *before* the `InstallExtent` commit; applying `InstallExtent` is what removes the range
from the free list. So:

- a crash after the data write but before the commit leaves the range free and unreferenced;
- WAL replay and Raft followers rebuild exactly the same free list, because allocation is a
  deterministic consequence of applied commands;
- a single engine-wide write lock spans allocate → write → commit, so two writers can never be handed
  the same range.

Space held by a snapshot is never reused until the snapshot is deleted and GC runs.

## Raft metadata replication (`raft` module)

`RaftNode` replicates `MetaCommand` records across a fixed set of metadata voters and applies them
through `Catalog::apply_committed`. It is sans-IO: the caller drives `tick()`, delivers inbound
messages with `step()` and sends whatever `take_messages()` returns. Implemented:

- randomized election timeouts, `RequestVote` with the up-to-date-log check, one vote per term;
- `AppendEntries` with prev-index/term consistency, conflict truncation (never below the commit
  index) and a conflict hint so the leader backs off a whole term at a time;
- leader commit only for entries of its own term (a new leader appends a `Noop` to commit earlier
  ones), quorum = majority of voters including itself;
- pipelined replication: the leader advances a follower's `next_index` as soon as it sends
  entries, so each proposal ships only new entries; a rejection (or the next heartbeat's
  consistency check after a lost message) rewinds it, and an acknowledgement never moves it back.
  Without this, concurrent proposals each resent the whole unacknowledged tail and throughput
  collapsed under load;
- log compaction after `compact_after` applied entries, and a chunked `InstallSnapshot` for
  followers behind the compaction point (`raft_snapshot`). The leader streams its applied catalog
  from a clone pinned to its store snapshot: a header (everything but extents and filesystem
  inodes), pages of 4096 extents and 1024 inodes, then `Done`. Each chunk waits for the previous
  one's acknowledgement, and a chunk in flight is re-sent after an election timeout. The follower
  stages every chunk in `catalog.redb.incoming` through ordinary incremental checkpoints, renames
  it over `catalog.redb` on `Done`, and reloads. Neither side holds more than a chunk, and no
  frame comes near the 256 MiB limit that a whole-catalog message would hit. An answer of `ack:
  None` (a follower that restarted mid-transfer, or a chunk out of order) restarts the transfer.
  A transfer cut short leaves the follower's store untouched, and a leftover staging file is
  removed on open. Nodes from before chunked snapshots can't install snapshots from newer ones;
  upgrade the whole group;
- pre-vote: a node whose election timer fires first asks for pre-votes for `term + 1` without
  changing anyone's term, and only campaigns once a majority would vote for it, so a partitioned
  node cannot inflate its term and depose a healthy leader when it rejoins;
- check-quorum: a leader that has not heard from a majority within the minimum election timeout
  steps down, and a node that has heard from a live leader within that window ignores
  higher-term vote requests.

Each replica keeps its own `raft_state.json` (term + vote), WAL and catalog store. Durability order:
the vote is fsynced before any reply; entries are fsynced before they are acknowledged or counted
toward the leader's own vote; the catalog is checkpointed before the log is compacted past it,
and an installed snapshot is durable in the swapped-in store before the log entries it covers
are dropped.

Proposals are validated on the leader against its applied catalog plus all uncommitted entries, so
an invalid command is rejected instead of logged. If a committed command still fails to apply, it is
a no-op that consumes its index on every replica, keeping replicas identical.

Tests (`tests/raft.rs`) run 3-node clusters over a simulated network: election, replication,
follower redirect, leader crash, a partitioned minority leader whose uncommitted entry is discarded,
full-cluster restart from disk, snapshot catch-up, and a randomized partition/crash/restart schedule
that checks no acknowledged commit is lost and all replicas converge.

### TCP transport (`raft_server` module)

`RaftServer::start(cfg, listener, peers, tick)` runs a `RaftNode` across processes using only the
standard library:

- frames are a 4-byte big-endian length plus a JSON `Envelope`, capped at 256 MiB;
- a driver thread owns the node, ticks it every `tick` and steps inbound messages in batches;
- one sender thread per peer keeps a connection open, reconnects with a short backoff and drops
  messages while the peer is down (Raft retransmits);
- inbound envelopes are dropped unless `from` is a configured peer and `to` is this node;
- several Raft groups can share one listener: `RaftMux::start(listener, tls)` accepts the
  connections and `RaftServer::start_in(&mux, group, ...)` runs one group on it. Each envelope
  names its `group` (omitted on the wire for group 0, so a single-group node is unchanged) and the
  mux hands it to that group's server; frames for a group not served here are dropped and counted
  in group 0's `rejected_frames`. Every group dials its own connections to its peers, so a busy
  group never queues behind another. `RaftServer::start` is a private mux serving group 0;
- `propose(cmd, timeout)` blocks until the entry is applied locally, or returns `NotLeader` (with a
  leader hint), `LeadershipLost` (outcome unknown), `Timeout` or `Shutdown`;
- a storage error inside the node (e.g. a failed fsync) stops the server and is reported by
  `fatal_error()`; it never keeps serving on a state it could not persist.

Started without a `TlsIdentity` the transport has **no authentication or encryption**; bind it to a
private metadata network only, or enable mutual TLS (below).

### Read barriers (ReadIndex)

`RaftServer::read_barrier(timeout)` (and `NativeEngine::read_barrier()`) blocks until the local
replica reflects every entry committed before the call, on a follower as well as the leader, so a
read served after it is linearizable (Raft thesis §6.4):

1. the node asks the leader (`ReadIndex`; the leader asks itself) for a barrier;
2. the leader turns it down until it has committed an entry in its own term (until then its commit
   index may trail its predecessor's), else records its commit index and raises its read round;
3. every `AppendEntries` carries the leader's read round and every response echoes it; once a
   quorum has answered a round at or after the barrier's, no newer leader can have committed
   anything before the barrier was taken, and the leader answers with the recorded index;
4. the node waits until it has applied that index.

A leader that steps down turns its pending barriers down and the askers ask again; a lost request
or answer is asked again after 500 ms. An isolated old leader never confirms one: it cannot reach a
quorum, and check-quorum deposes it. Older metadata nodes do not know these messages and drop the
connection that carries one, so upgrade every replica before relying on barriers.

`tests/raft_tcp.rs` runs a real 3-server cluster over localhost: election and replication, follower
redirect, and leader shutdown, failover and rejoin from disk. `tests/raft_groups.rs` runs three
groups on each node's one listener: they replicate independently, and one group failing over (its
leader's server stopped, the listener kept) leaves the other groups' leaders and terms alone.

### Client sessions and file locks (`leases` module)

`MetaCommand::Lease` carries session and lock changes: `Open` (create or renew with a TTL),
`Renew`, `Close`, `Expire{now_ms}`, `Lock` (POSIX `F_SETLK` over an inclusive byte range for one
`(session, owner)`: the owner's overlapping locks are split or replaced, same-kind neighbours
merged; another owner's overlapping lock, where either side is a write lock, rejects it with
`locked`) and `ReleaseOwner`. Every time is the proposing leader's wall clock, carried in the
command, so replicas agree. The leader validates a lock against its speculative catalog, so a
conflicting lock is refused without appending anything.

Sessions and locks live in `Catalog::leases`, which is written whole into the store's state
record at each checkpoint and shipped in a Raft snapshot's header; deleting a filesystem drops
its locks. Each metadata group leader proposes `Expire` once a second when a session's
`expires_ms` has passed. A new leader first waits out the longest TTL of any open session from
when it took over: every live session has then been renewed against its own clock, so a clock
that runs ahead of the previous leader's cannot expire a live client. Limits: TTL 1–300 s,
65 536 sessions per group, 4 096 locks per session.

Cache leases (`node::cache_leases`) are not replicated: the leader keeps them in memory. A
leased read first confirms leadership with a read barrier, then registers the lease, then reads
what it returns. A change first marks its inodes (no lease is granted on them until it commits),
recalls every other session's unexpired lease on them and waits for each to be given back or run
out. A lease from an earlier term is covered by the grace period instead: a new leader, while a
caching session opened before its term is open (`Session::term`), holds changes back for one
lease (5 s) from when it first serves a lease operation. Because the old leader confirmed its
leadership after the client sent its request, the client's lease (counted from the send) ends
before that grace does.

### Namespace sharding (`node::shards`)

A node with `metadata.groups > 1` runs one engine per group (each with its own catalog, WAL, free
list and data-node fence) and routes each request to the group holding its id: new volumes and
filesystems to `FNV-1a(id) % groups`, snapshots and clones to their source's group, and lookups by
checking the local catalogs first and, on a miss, after a read barrier on every group. Data nodes
allocate appends themselves, so groups share devices without coordinating; a range a group frees
returns to that group's free list. Operations and placement rules are in `docs/NATIVE_NODE.md`,
"Metadata groups"; `tests/node.rs` (`http_namespace_sharded_across_raft_groups`,
`http_members_change_reaches_every_raft_group`) covers routing from every node, snapshot and clone
placement, cross-group duplicate ids, merged lists and metrics, a node failure and a membership
change across groups.

### Quorum configuration (`membership` module)

Every Raft quorum decision (pre-votes, votes, commit acknowledgements, check-quorum) goes through
`Membership::has_quorum`. Today the configuration is always `Stable { voters }` (the node plus its
peers). `Joint { old, new }` requires a majority of both sets, which is the rule a joint-consensus
membership change needs; the change protocol itself (committing the joint and then the new
configuration) is not implemented yet.

### Metrics

`RaftServer::render_metrics()` and `NativeEngine::render_metrics()` return Prometheus text; the
process hosting a node serves them on its `/metrics` endpoint (there is no standalone native node
binary yet). All Raft series carry `node` and `group` labels:

- Raft: `atlas_native_raft_{term,commit_index,applied_index,last_index,snapshot_index}`,
  `atlas_native_raft_role{role}` (one-hot), `atlas_native_raft_peer_match_index{peer}` (leader only),
  `atlas_native_raft_{elections,leader_terms,append_rejections}_total`;
- transport, per peer: `atlas_native_transport_{sent,connect_failures,write_failures,dropped}_total`,
  plus `atlas_native_transport_rejected_frames_total` and the node-wide
  `atlas_native_transport_tls_handshake_failures_total`;
- engine: `atlas_native_{reads,writes,read_bytes,write_bytes,checksum_failures,replica_fallbacks}_total`,
  `atlas_native_gc_reclaimed_extents_total`, `atlas_native_metadata_applied_index`,
  `atlas_native_wal_records`, `atlas_native_{volumes,snapshots,extents}`,
  `atlas_native_allocator_free_{bytes,ranges}`, `atlas_native_device_bytes{node}`;
- data node: `atlas_native_data_requests_total{op}`, `atlas_native_data_{fenced_writes,errors,
  written_bytes,read_bytes}_total`, `atlas_native_data_fence{node,group}`, `atlas_native_data_device_bytes{node,device}`.

### Data nodes (`data_node` module)

The engine writes replicas through the `BlockStore` trait: `FileDevice` for local files, or
`RemoteDevice` for a `DataNodeServer` on another host. A data node serves one local device file:

- each message is a 4-byte big-endian header length, a JSON header, then the raw payload the
  header announces (write data in requests, read data in responses); headers are capped at 64 KiB
  and payloads at 256 MiB;
- one thread per connection; `RemoteDevice` keeps one connection open, bounds every connect, read
  and write with a timeout, and retries once on a fresh connection if a reused one fails (a retried
  append can leave an unreferenced copy, the same leak as a crash between data write and commit);
- without a `TlsIdentity` the transport has **no authentication or encryption**, same as Raft.

**Write fencing.** Every `append`/`write_at` carries a fence, the writer's Raft term, and the
metadata group it writes for (`RemoteDevice::in_group`; omitted on the wire for group 0). The node
durably records the highest fence it has accepted from each group (`root/fence` for group 0,
`root/fence.g<N>` for the others, atomic rewrite) and rejects lower ones with `Fenced { current }`,
holding that group's fence lock across the write. Groups have independent terms and never write
the same range, so one group's election never fences another's writes. Before its first write a
client for a group other than 0 asks the node for its fences (`Fences`); a node too old to answer
is refused rather than silently fencing every group against one term. This is what makes free-list
reuse safe across leader changes: a deposed leader that has not stepped down yet can only allocate
ranges that are free in its own state, and the new leader always writes a range to its data nodes
before committing it, so the stale write either lands first and is overwritten, or arrives later and
is fenced.

### Engine over Raft (`MetaBackend::Raft`)

`NativeEngine::open_with(cfg, nodes, MetaBackend::Raft { server, timeout })` commits every
`MetaCommand` through a `RaftServer` instead of the local WAL. One engine runs per metadata replica,
all configured with the same node ids and data-node addresses:

- mutations only succeed on the leader's engine; others return `Raft(NotLeader { leader })`;
- `write` first calls `RaftServer::leader_ready`, which waits until the node is leader and has
  applied its whole log (including the no-op that commits earlier terms), so allocation reads a
  free list that reflects every committed reservation. The returned term fences the data writes,
  and `InstallExtent` is proposed with `propose_in_term`, which refuses if the term moved on;
- `gc_once` waits for the same barrier so it never proposes a reclaim that is already in the log;
- reads come from the local replica's applied catalog, so a follower can lag the leader unless
  the caller runs `read_barrier()` first (below); extent metadata is copied out before the network
  read, so a slow data node never holds the Raft node lock;
- `checkpoint` is refused: the Raft node compacts its own log. `wal_records` reports the Raft log
  length above its compaction point.

Node health and repair are described in the next section.

`tests/data_node.rs` covers the remote device and its bounds, fencing across a data-node restart, an
engine on remote data nodes losing one, and three Raft-backed engines sharing three data nodes
through a leader failover, followed by an overwrite and GC on the new leader that reuse space freed
under the old one. A write with the old leader's term is then fenced.

### Node health, write failover and repair

Each engine keeps a per-node circuit breaker. Any data-node I/O failure (a fenced write excepted:
that means this engine was deposed and is returned to the caller) counts a failure and backs the
node off for `EngineConfig::node_retry_after` (default 5 s); any success clears the back-off. After
the window the node is eligible again, so a recovered node rejoins on its next successful I/O.
Health is local to each engine and never replicated; a node's configured `healthy: false` still
excludes it permanently.

- **Writes** walk every eligible node in placement preference order (rack spread first, distinct
  hosts if required) and stop once `placement.replicas` writes succeed. A node that fails is backed
  off and the replica goes to the next node, so with a spare node a write survives a data node
  dying mid-write. With fewer eligible nodes than replicas the write fails with
  `InsufficientReplicas` before any I/O. Data already written to other replicas of a failed attempt
  is not referenced (the same leak as a crash between data write and commit).
- **Reads** try replicas on eligible nodes first and backed-off nodes last (they may hold the only
  good copy).
- **`repair_once`** scrubs every extent: it reads each replica and verifies its checksum. A replica
  that is unreachable or corrupt is rewritten from a verified copy onto an eligible node that holds
  none of the extent's other replicas (and, when distinct hosts are required, none of their hosts);
  a corrupt replica on a reachable node can move to a fresh range on the same node. The move commits
  as `MetaCommand::ReplaceReplica { extent_id, old, new }`, which reserves the new range and returns
  the old one to the free list, so it replicates through Raft and replays from the WAL like any
  other command. Under Raft it runs on the leader behind the same `leader_ready` barrier and fence
  as writes. It reports `extents_checked`, `replicas_repaired`, `unrecoverable` (no good copy left)
  and `deferred` (no eligible target, or the extent changed underneath; retried on the next pass).
  It holds the write lock only while a replacement is placed and committed, so writers are not
  stalled behind a scan. `repair_extent(id, lost)` repairs one extent, treating parts on `lost`
  nodes as missing without reading them. `lost_nodes`, `probe_nodes` and `degraded_extents`
  (extents with parts on lost nodes, least spare redundancy first, from metadata alone) feed the
  rebuild controller (`crate::rebuild`, `docs/NATIVE_NODE.md`), which schedules, prioritises and
  paces repair; `tests/rebuild.rs` covers it.
- **Tiered extents** (`ExtentRef.object`): `MetaCommand::TierExtent { extent_id, key, replicas }`
  records the object holding the whole extent, clears its replicas and erasure layout and returns
  their ranges to the free list. It is refused unless the extent still has exactly `replicas`, so
  a concurrent repair can neither lose its new range nor have it freed. `ExtentRef.created_ms`,
  which the leader stamps when it places an extent, dates it for the tiering policy (0 for older
  extents). Reads that fail because the extent moved after they looked it up re-read it from
  where it is now. `tests/tiering.rs` covers this.
- **Erasure-coded extents** (`EngineConfig::erasure`, `ExtentRef.ec`) keep one `ReplicaRef` per
  shard, in shard order, plus each shard's length and SHA-256. Every install command checks that
  the layout is consistent and that no two shards share a node. The free list and GC count the
  shard length per replica, so reclaiming a coded extent frees every shard. Reads decode around up
  to m bad shards; `repair_once` reads every shard, rebuilds the bad ones from any k good ones and
  moves each with the same `ReplaceReplica` command, onto a node holding no other shard. An extent
  with more than m bad shards counts as `unrecoverable`. `tests/erasure.rs` covers placement and
  space, partial overwrites and GC, corrupt and lost shards, too many lost shards and too few nodes.

Metrics: `atlas_native_node_up{node}`, `atlas_native_node_failures_total{node}`,
`atlas_native_replica_write_failures_total`, `atlas_native_replicas_repaired_total`;
`atlas_native_device_bytes{node}` omits unreachable nodes instead of failing the scrape.

`tests/health.rs` covers failing over a write to a spare node, skipping a backed-off node and
reusing it after recovery, refusing writes with too few nodes, re-replicating after losing a node
(then surviving the loss of a second original), rewriting a corrupt replica in place and freeing
its old range (across reopen), reporting an extent with no good copy, and `ReplaceReplica`
validation. `tests/data_node.rs` also repairs a lost replica on a Raft leader and checks every
replica's engine reads through the new copy.

### Mutual TLS (`tls` module)

Both transports take an optional `TlsIdentity` (`RaftServer::start_with`,
`DataNodeServer::start_with`, `RemoteDevice::with_tls`), built from PEM: the cluster CA bundle,
the node's certificate chain and its private key. With it every connection is mutual TLS (rustls,
explicit `ring` provider, TLS 1.2/1.3) and both sides verify the other against the cluster CA:

- each node's certificate carries its node id as a DNS SAN, so node ids must be valid DNS names;
- a Raft peer is dialled as its node id, and an inbound envelope is accepted only if its `from` is a
  configured peer **and** a name the connection's client certificate is valid for, so a node can
  only speak for the identity its certificate names;
- a `RemoteDevice` verifies the data node's certificate against the expected node id; a data node
  accepts any client certificate signed by the cluster CA (it does not yet restrict which clients);
- handshakes are bounded by a 5 s timeout and counted in
  `atlas_native_transport_tls_handshake_failures_total` / `atlas_native_data_tls_handshake_failures_total`.

Without an identity both transports stay plaintext, as before. There is no certificate rotation
without restarting the server, and no revocation (CRL/OCSP) checking.

`tests/tls.rs` runs an mTLS Raft cluster, isolates a peer holding a CA-signed certificate for another
node's name (frames rejected, never dialled successfully) and a peer from a different CA (handshake
failures), checks the data node refuses plaintext, wrong-name and rogue-CA clients, and runs
Raft-backed engines writing to TLS data nodes.

Not implemented yet:

- per-client authorization on data nodes, certificate hot reload and revocation;
- membership changes: the voter set is fixed at open (quorum math already supports joint
  configurations);
- lease-based reads (every read barrier costs the leader one round of appends).

## Failure model covered

- process crash after WAL fsync but before the next checkpoint (commits between checkpoints are
  replayed from the WAL);
- incremental checkpoints read back identical to the in-memory catalog across edits, removals,
  snapshots, clones and a filesystem deleted and recreated under the same id; a paged catalog
  matches an unpaged twin across checkpoints and a reload; a 2000-entry directory keeps only its
  uncheckpointed changes in memory, pages by name to exactly its entries, and an inode record
  from before entries were paged is counted on read; a chunked Raft snapshot through the
  wire encoding loads identical to the leader's catalog, and a lagging follower cut off mid-transfer
  still converges and survives a restart; a legacy `catalog.json` migrates into the store;
- torn final WAL record;
- restart/replay without double-applying committed commands;
- snapshot copy-on-write isolation and space protection;
- refcount underflow and free-list double-free protection;
- monotonic WAL indexes across compaction;
- metadata leader crash, minority partition, rejoin without disruption, isolated-leader step-down
  and full restart (Raft);
- data-node loss (reads fall back, writes move to a spare node, repair re-replicates), corrupt
  replicas (repair rewrites them), stale-leader data writes (fenced, including across a data-node
  restart) and engine continuity through a metadata leader failover.

## Next phase

- a container image, Helm chart and gateway integration for `atlas-native-node`
  (`docs/NATIVE_NODE.md`);
- joint-consensus membership changes;
- hole punching for freed ranges at the device tail;
- io_uring/raw-NVMe data path.
