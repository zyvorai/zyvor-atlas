// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

use crate::{
    checksum,
    device::{BlockStore, FileDevice},
    ec::{EcLayout, EcScheme},
    gc,
    grant::GrantKey,
    metadata::{
        Catalog, DeltaMap, DeltaRef, ExtentId, ExtentRef, MetaCommand, MetaError, ReplicaRef,
        SnapshotId, VolumeId,
    },
    metrics::PromText,
    namespace::{FsOp, InodeKind},
    object::ObjectStore,
    placement::{select_replicas, Node, PlacementPolicy},
    raft::RaftError,
    raft_server::RaftServer,
    store::{
        load_checkpoint, remove_legacy_catalog, CatalogStore, CATALOG_STORE, DEFAULT_CACHE_INODES,
        DEFAULT_STORE_CACHE_BYTES,
    },
    telemetry::NativeIoCounters,
    wal::{Wal, WalError, WalRecord},
};

mod combiner;
mod concurrent;
mod delta;
mod direct;
mod export;
mod files;
mod folder;
mod inflight;
mod leases;
mod locality;
mod rebuild;
mod replica;
mod tier;
use delta::{CellView, Scope};

pub use concurrent::CommitPoint;
pub use delta::{DeltaConfig, DeltaPoint};
pub use direct::{
    DirectCell, DirectCellDone, DirectCommit, DirectLease, DirectReplicaDone, DirectTarget,
};
pub use export::{check_export_name, ExportExtent, ExportInfo, ExportManifest, ExportStats};
pub use files::{
    Attr, DirEntry, FileLayout, FsInfo, FsSnapshotInfo, FsStat, LayoutEc, LayoutExtent,
    LayoutReplica, NewNode,
};
pub(crate) use folder::nap;
pub use folder::{run_folders, MAX_FOLD_CONCURRENCY};
pub use leases::{now_ms, LockRequest, LockTable};
pub use locality::{HostLocality, Locality, NodeLocality, PinReport};
pub use rebuild::Degraded;
pub use replica::{DiffInode, DiffPage, DEFAULT_DIFF_LIMIT};
pub use tier::{TierPolicy, TierStats};

/// Extents fetched concurrently by one read.
const READ_PARALLELISM: usize = 8;
/// Extents whose replicas are written concurrently by one write.
const WRITE_PARALLELISM: usize = 4;

/// What a data write lands in.
enum Target<'a> {
    Volume(&'a str),
    /// `replica`: replication data for a replica filesystem ([`FsOp::InstallFileExtent`]).
    File {
        fs: &'a str,
        ino: u64,
        replica: bool,
    },
}

impl Target<'_> {
    /// The extent grid: the cluster's for volumes, the filesystem's own for files.
    fn grid(&self, c: &Catalog, cluster: usize) -> Result<u64, NativeError> {
        Ok(match self {
            Target::Volume(_) => cluster as u64,
            Target::File { fs, .. } => c.filesystem(fs)?.extent_bytes.unwrap_or(cluster as u64),
        })
    }

    /// The base extent and deltas at grid cell `cell` as `c` has them, if the cell has data.
    fn cell_view(&self, c: &Catalog, cell: u64) -> Result<Option<CellView>, NativeError> {
        let (id, deltas) = match self {
            Target::Volume(v) => c.volumes.get(*v).map_or((None, None), |v| {
                (v.extents.get(&cell).cloned(), v.deltas.get(&cell).cloned())
            }),
            Target::File { fs, ino, .. } => match &c.fs_view(fs)?.inode(*ino)?.kind {
                InodeKind::File {
                    extents, deltas, ..
                } => (extents.get(&cell).cloned(), deltas.get(&cell).cloned()),
                _ => {
                    return Err(
                        MetaError::IsDir(format!("inode {ino} is not a regular file")).into(),
                    )
                }
            },
        };
        let Some(base) = id.and_then(|id| c.extents.get(&id)) else {
            return Ok(None);
        };
        Ok(Some(CellView {
            base: ExtentRef {
                logical_offset: cell,
                ..base.extent.clone()
            },
            deltas: delta_extents(c, cell, deltas.as_deref().unwrap_or_default())?,
        }))
    }

    /// The extent currently at grid cell `cell`, if any.
    fn extent_at(&self, c: &Catalog, cell: u64) -> Result<Option<ExtentRef>, NativeError> {
        let id = match self {
            Target::Volume(v) => c
                .volumes
                .get(*v)
                .and_then(|v| v.extents.get(&cell).cloned()),
            Target::File { fs, ino, .. } => match &c.filesystem(fs)?.inode(*ino)?.kind {
                InodeKind::File { extents, .. } => extents.get(&cell).cloned(),
                _ => {
                    return Err(
                        MetaError::IsDir(format!("inode {ino} is not a regular file")).into(),
                    )
                }
            },
        };
        Ok(id.and_then(|id| c.extents.get(&id)).map(|m| ExtentRef {
            logical_offset: cell,
            ..m.extent.clone()
        }))
    }
}

/// The extents a delta list names, each with the offset it applies at.
fn delta_extents(
    c: &Catalog,
    cell: u64,
    list: &[DeltaRef],
) -> Result<Vec<(u64, ExtentRef)>, NativeError> {
    list.iter()
        .map(|d| {
            c.extents
                .get(&d.id)
                .map(|m| {
                    (
                        d.at,
                        ExtentRef {
                            logical_offset: cell + d.at,
                            ..m.extent.clone()
                        },
                    )
                })
                .ok_or_else(|| NativeError::NotFound(d.id.clone()))
        })
        .collect()
}

/// The replica index a read of extent `id` starts at (FNV-1a, stable across processes).
pub(crate) fn replica_start(id: &str, replicas: usize) -> usize {
    if replicas == 0 {
        return 0;
    }
    let h = id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    });
    (h % replicas as u64) as usize
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[derive(Debug, thiserror::Error)]
pub enum NativeError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("metadata json error: {0}")]
    MetadataJson(#[from] serde_json::Error),
    #[error("metadata state error: {0}")]
    Metadata(#[from] MetaError),
    #[error("wal error: {0}")]
    Wal(#[from] WalError),
    #[error("raft error: {0}")]
    Raft(#[from] RaftError),
    #[error("resource not found: {0}")]
    NotFound(String),
    #[error("insufficient healthy replicas: need {needed}, found {found}")]
    InsufficientReplicas { needed: usize, found: usize },
    #[error("checksum mismatch for extent {0}")]
    Checksum(String),
    #[error("lock poisoned: {0}")]
    Poisoned(&'static str),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("write fenced: a data node has accepted writes from term {current}")]
    Fenced { current: u64 },
    #[error("data node error: {0}")]
    Remote(String),
    #[error("read-only: {0}")]
    ReadOnly(String),
    /// The write may or may not have happened; it was not retried.
    #[error("outcome unknown: {0}")]
    Unknown(String),
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub root: PathBuf,
    pub extent_bytes: usize,
    pub placement: PlacementPolicy,
    /// Checkpoint the catalog and compact the WAL once it retains this many records (0 disables
    /// automatic compaction).
    pub wal_compact_after: u64,
    /// After an I/O failure a node is skipped for placement (and tried last for reads) for this
    /// long, then given another chance.
    pub node_retry_after: Duration,
    /// Unchanged inodes the catalog keeps in memory; the rest stay in the catalog store.
    pub catalog_cache_inodes: usize,
    /// The catalog store's page cache (with [`MetaBackend::Local`]; Raft's store has its own).
    pub catalog_cache_bytes: usize,
    /// Erasure-code new extents of at least `erasure_min_bytes` under this scheme instead of
    /// replicating them (`placement.replicas` still applies to smaller ones).
    pub erasure: Option<EcScheme>,
    pub erasure_min_bytes: usize,
    /// LZ4-compress new extents that shrink by at least 12.5% before they are replicated or
    /// erasure-coded ([`crate::compress`]). Existing extents are left as they are.
    pub compression: bool,
    /// Share identical extents: a write whose content already exists as a live extent of this
    /// engine references that extent instead of storing a second copy. See
    /// `docs/NATIVE_DATA_REDUCTION.md` for what that does and does not protect against.
    pub dedup: bool,
    /// Record that content-defined fingerprints may be exported for a future cross-group index.
    /// Off by default. Writes still deduplicate whole extents inside this engine only; see
    /// [`crate::cdc`].
    pub cross_group_dedup: bool,
    /// Where tiered extents live ([`NativeEngine::tier_once`]); reads of tiered extents fail
    /// without it.
    pub objects: Option<Arc<dyn ObjectStore>>,
    /// Key prefix of this engine's objects. Engines sharing a bucket need distinct prefixes:
    /// each deletes objects under its own prefix that its catalog doesn't reference.
    pub object_prefix: String,
    /// Key prefix of snapshot exports ([`NativeEngine::export_snapshot`]), shared by every
    /// group of a cluster.
    pub export_prefix: String,
    /// Secret shared with the data nodes that check write grants ([`crate::grant`]). Required to
    /// lease direct client writes ([`NativeEngine::direct_write_lease`]).
    pub grant_key: Option<GrantKey>,
    /// The metadata group this engine serves: the group its direct-write grants are fenced as.
    pub meta_group: u32,
    /// Delta writes ([`DeltaConfig`]).
    pub delta_writes: DeltaConfig,
    /// Let writes that create a cell's extent or replace all of it commit concurrently: they
    /// hold the write lock only to propose, not while they wait for the commit
    /// (`docs/NATIVE_METADATA.md`, "Concurrent commits"). Off by default.
    pub concurrent_commits: bool,
}

/// See [`NativeEngine::data_reduction`].
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReductionStats {
    pub extents: u64,
    pub compressed_extents: u64,
    /// Extents referenced more than once.
    pub shared_extents: u64,
    /// [`EngineConfig::cross_group_dedup`]. Sharing across groups is not performed.
    #[serde(default)]
    pub cross_group_dedup: bool,
    pub logical_bytes: u64,
    pub referenced_bytes: u64,
    pub stored_bytes: u64,
}

/// Extents smaller than this stay replicated by default: their shards would be tiny.
pub const DEFAULT_ERASURE_MIN_BYTES: usize = 64 << 10;

impl EngineConfig {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            extent_bytes: 4 * 1024 * 1024,
            placement: PlacementPolicy::default(),
            wal_compact_after: 1024,
            node_retry_after: Duration::from_secs(5),
            catalog_cache_inodes: DEFAULT_CACHE_INODES,
            catalog_cache_bytes: DEFAULT_STORE_CACHE_BYTES,
            erasure: None,
            erasure_min_bytes: DEFAULT_ERASURE_MIN_BYTES,
            compression: false,
            dedup: false,
            cross_group_dedup: false,
            objects: None,
            object_prefix: "atlas-native/".into(),
            export_prefix: "atlas-native/".into(),
            grant_key: None,
            meta_group: 0,
            delta_writes: DeltaConfig::default(),
            // The test feature runs the existing tests with it on.
            concurrent_commits: cfg!(feature = "concurrent-commits-test"),
        }
    }
}

/// A command [`NativeEngine::commit_with`] proposed.
enum Proposed {
    /// Applied already, or there was nothing to propose.
    Done,
    /// In the Raft log at `index`, not necessarily applied.
    Raft { index: u64, term: u64 },
}

/// Where the engine commits metadata.
#[derive(Clone)]
pub enum MetaBackend {
    /// A local WAL and `catalog.json` under `EngineConfig::root`.
    Local,
    /// A replicated log. Only the engine attached to the current leader can mutate; every
    /// engine serves reads from its replica's applied catalog, which can lag the leader.
    Raft {
        server: Arc<RaftServer>,
        /// Bounds waiting for leader readiness and for each proposal to apply.
        timeout: Duration,
    },
}

enum Meta {
    Local {
        catalog: Box<RwLock<Catalog>>,
        wal: Box<Mutex<Wal>>,
        store: CatalogStore,
    },
    Raft {
        server: Arc<RaftServer>,
        timeout: Duration,
    },
}

impl std::fmt::Debug for Meta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Meta::Local { .. } => "Meta::Local",
            Meta::Raft { .. } => "Meta::Raft",
        })
    }
}

#[derive(Debug, Default)]
struct NodeHealth {
    failures: AtomicU64,
    down_until: Mutex<Option<Instant>>,
    /// First failure since the node last answered; what the rebuild controller ages.
    down_since: Mutex<Option<Instant>>,
}

#[derive(Debug)]
struct NodeRuntime {
    spec: Node,
    devices: Vec<Arc<dyn BlockStore>>,
    health: NodeHealth,
    /// Round-robin cursor striping new replicas across `devices`.
    next_device: std::sync::atomic::AtomicUsize,
}

/// A node's health as the engine currently sees it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NodeStatus {
    pub id: String,
    /// Configured healthy and not inside an I/O-failure back-off window.
    pub up: bool,
    pub failures: u64,
}

/// The kinds of catalog object an id can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Volume,
    Snapshot,
    Filesystem,
    FsSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VolumeInfo {
    pub id: VolumeId,
    pub name: String,
    pub size_bytes: u64,
    pub extents: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RepairStats {
    pub extents_checked: u64,
    pub replicas_repaired: u64,
    /// Extents with no replica that still reads back with a valid checksum.
    pub unrecoverable: u64,
    /// Bad replicas left for a later pass: no eligible target, or the extent changed meanwhile.
    pub deferred: u64,
    /// Bytes read from data nodes to check and rebuild extents.
    pub bytes_read: u64,
    /// Bytes written to data nodes as replacement replicas or shards.
    pub bytes_written: u64,
}

impl RepairStats {
    pub fn add(&mut self, o: &RepairStats) {
        self.extents_checked += o.extents_checked;
        self.replicas_repaired += o.replicas_repaired;
        self.unrecoverable += o.unrecoverable;
        self.deferred += o.deferred;
        self.bytes_read += o.bytes_read;
        self.bytes_written += o.bytes_written;
    }
}

#[derive(Debug)]
pub struct NativeEngine {
    cfg: EngineConfig,
    nodes: Vec<NodeRuntime>,
    meta: Meta,
    /// A write or sync of the local metadata log failed; the engine must be reopened.
    log_failed: AtomicBool,
    /// Serializes writes that read what they overwrite (partial extents) through their commit,
    /// and every write's commit; aligned whole-extent writes place their data without it. With
    /// [`EngineConfig::concurrent_commits`], a write that reads nothing holds it only to
    /// propose, not through its commit.
    write_lock: Mutex<()>,
    concurrent: concurrent::Concurrent,
    truncates: files::Truncates,
    /// Free ranges held by placements whose commit hasn't applied yet.
    inflight: inflight::Inflight,
    delta: delta::DeltaState,
    /// Leases clients hold to write data nodes directly, until they commit or expire.
    direct: direct::DirectWrites,
    /// One repair at a time, so two never write replacements for the same part.
    repair_lock: Mutex<()>,
    /// When each extent was last read (ms since the epoch), tracked only with an object store,
    /// so recently read extents aren't tiered. Not replicated: a new leader treats everything
    /// as read when it opened its engine.
    reads: Mutex<std::collections::HashMap<String, u64>>,
    opened_ms: u64,
    pub telemetry: NativeIoCounters,
    /// Per-filesystem activity windows (ransomware assessment input).
    pub fs_activity: crate::activity::FsActivity,
    /// Content hash -> a live extent holding those bytes (a hint, rebuilt from the catalog on
    /// first use: every share is re-checked when it is committed).
    dedup_index: Mutex<DedupIndex>,
}

#[derive(Debug, Default)]
struct DedupIndex {
    built: bool,
    by_content: std::collections::HashMap<[u8; 32], ExtentId>,
}

impl NativeEngine {
    /// Local devices under `root/nodes/<id>/nvme0.data` and a local metadata WAL.
    pub fn open(cfg: EngineConfig, nodes: Vec<Node>) -> Result<Self, NativeError> {
        let mut stores = Vec::new();
        for n in nodes {
            let d = FileDevice::open(cfg.root.join("nodes").join(&n.id).join("nvme0.data"))?;
            stores.push((n, Arc::new(d) as Arc<dyn BlockStore>));
        }
        Self::open_with(cfg, stores, MetaBackend::Local)
    }

    /// Opens an engine over caller-supplied devices (e.g. [`crate::data_node::RemoteDevice`])
    /// with the given metadata backend. Every engine sharing a Raft group must be given the
    /// same node ids and devices.
    pub fn open_with(
        cfg: EngineConfig,
        nodes: Vec<(Node, Arc<dyn BlockStore>)>,
        meta: MetaBackend,
    ) -> Result<Self, NativeError> {
        let nodes = nodes.into_iter().map(|(n, d)| (n, vec![d])).collect();
        Self::open_with_devices(cfg, nodes, meta)
    }

    /// [`Self::open_with`] with several devices per node (a replica's `device_index` is its
    /// position); new replicas are striped across a node's devices.
    pub fn open_with_devices(
        cfg: EngineConfig,
        nodes: Vec<(Node, Vec<Arc<dyn BlockStore>>)>,
        meta: MetaBackend,
    ) -> Result<Self, NativeError> {
        if cfg.extent_bytes == 0 {
            return Err(NativeError::Invalid("extent_bytes must be > 0".into()));
        }
        cfg.delta_writes.validate()?;
        if let Some((n, _)) = nodes.iter().find(|(_, d)| d.is_empty()) {
            return Err(NativeError::Invalid(format!(
                "node {} has no devices",
                n.id
            )));
        }
        let runtimes = nodes
            .into_iter()
            .map(|(spec, devices)| NodeRuntime {
                spec,
                devices,
                health: NodeHealth::default(),
                next_device: std::sync::atomic::AtomicUsize::new(0),
            })
            .collect();
        let meta = match meta {
            MetaBackend::Local => {
                let store = CatalogStore::open_with_cache(
                    cfg.root.join(CATALOG_STORE),
                    cfg.catalog_cache_bytes,
                )?
                .with_cache_inodes(cfg.catalog_cache_inodes);
                let (catalog, wal) = load_local(&cfg.root, &store)?;
                Meta::Local {
                    store,
                    catalog: Box::new(RwLock::new(catalog)),
                    wal: Box::new(Mutex::new(wal)),
                }
            }
            MetaBackend::Raft { server, timeout } => Meta::Raft { server, timeout },
        };
        let engine = Self {
            cfg,
            nodes: runtimes,
            meta,
            log_failed: AtomicBool::new(false),
            write_lock: Mutex::new(()),
            concurrent: Default::default(),
            truncates: Default::default(),
            inflight: Default::default(),
            delta: Default::default(),
            direct: Default::default(),
            repair_lock: Mutex::new(()),
            reads: Mutex::new(Default::default()),
            opened_ms: now_ms(),
            telemetry: NativeIoCounters::default(),
            fs_activity: Default::default(),
            dedup_index: Mutex::new(DedupIndex::default()),
        };
        if let Meta::Local { catalog, store, .. } = &engine.meta {
            let mut c = catalog
                .write()
                .map_err(|_| NativeError::Poisoned("catalog"))?;
            store.checkpoint(&mut c)?;
            remove_legacy_catalog(&engine.cfg.root)?;
        }
        Ok(engine)
    }

    pub fn create_volume(
        &self,
        name: impl Into<String>,
        size_bytes: u64,
    ) -> Result<VolumeId, NativeError> {
        self.create_volume_as(Uuid::new_v4().to_string(), name, size_bytes)
    }

    /// [`Self::create_volume`] with a caller-chosen id: repeating the same call is a no-op, so a
    /// client can safely retry after an ambiguous failure.
    pub fn create_volume_as(
        &self,
        id: String,
        name: impl Into<String>,
        size_bytes: u64,
    ) -> Result<VolumeId, NativeError> {
        if size_bytes == 0 {
            return Err(NativeError::Invalid("volume size must be > 0".into()));
        }
        self.commit(
            MetaCommand::CreateVolume {
                id: id.clone(),
                name: name.into(),
                size_bytes,
            },
            None,
        )?;
        Ok(id)
    }

    pub fn delete_volume(&self, volume_id: &str) -> Result<(), NativeError> {
        self.commit(
            MetaCommand::DeleteVolume {
                volume_id: volume_id.to_string(),
            },
            None,
        )
    }

    /// Grows `volume_id` to `size_bytes` (never shrinks).
    pub fn resize_volume(&self, volume_id: &str, size_bytes: u64) -> Result<(), NativeError> {
        self.commit(
            MetaCommand::ResizeVolume {
                volume_id: volume_id.to_string(),
                size_bytes,
            },
            None,
        )
    }

    /// A new volume with the snapshot's contents, sharing its extents until either side is
    /// written. `size_bytes` defaults to the snapshot's size.
    pub fn clone_snapshot(
        &self,
        snapshot_id: &str,
        name: impl Into<String>,
        size_bytes: Option<u64>,
    ) -> Result<VolumeId, NativeError> {
        self.clone_snapshot_as(Uuid::new_v4().to_string(), snapshot_id, name, size_bytes)
    }

    /// [`Self::clone_snapshot`] with a caller-chosen id for the new volume (idempotent).
    pub fn clone_snapshot_as(
        &self,
        id: String,
        snapshot_id: &str,
        name: impl Into<String>,
        size_bytes: Option<u64>,
    ) -> Result<VolumeId, NativeError> {
        self.commit(
            MetaCommand::CloneSnapshot {
                id: id.clone(),
                name: name.into(),
                snapshot_id: snapshot_id.to_string(),
                size_bytes,
            },
            None,
        )?;
        Ok(id)
    }

    pub fn write(&self, volume_id: &str, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        if data.is_empty() {
            return Ok(());
        }
        let in_bounds = |c: &Catalog| {
            let size = c
                .volumes
                .get(volume_id)
                .map(|v| v.size_bytes)
                .ok_or_else(|| NativeError::NotFound(volume_id.into()))?;
            if offset.saturating_add(data.len() as u64) > size {
                return Err(NativeError::Invalid("write exceeds volume size".into()));
            }
            Ok(())
        };
        let target = Target::Volume(volume_id);
        // Deltas first: they take no write lock to place their data.
        if self.try_delta(&target, offset, data)? {
            return Ok(());
        }
        if self.write_unlocked(&target, offset, data, in_bounds)? {
            return Ok(());
        }
        let _write = self
            .write_lock
            .lock()
            .map_err(|_| NativeError::Poisoned("write"))?;
        let fence = self.write_fence()?;
        self.with_catalog(in_bounds)??;
        self.write_locked(&target, offset, data, fence)
    }

    /// Allocation reads the applied free list, so under Raft every earlier entry (including
    /// other terms') must be applied first; the term then fences the data writes. Call with
    /// `write_lock` held.
    fn write_fence(&self) -> Result<u64, NativeError> {
        Ok(match &self.meta {
            Meta::Local { .. } => 0,
            Meta::Raft { server, timeout } => server.leader_ready(*timeout)?,
        })
    }

    /// Writes `data` at `offset` of `target` under `write_lock`.
    fn write_locked(
        &self,
        target: &Target,
        offset: u64,
        data: &[u8],
        fence: u64,
    ) -> Result<(), NativeError> {
        // Extents sit on a fixed grid (extent i covers [i*E, (i+1)*E)), so a write never leaves
        // two extents covering the same bytes. A write that covers only part of an existing
        // extent's bytes rewrites the whole extent with the old bytes merged in.
        let grid = self.with_catalog(|c| target.grid(c, self.cfg.extent_bytes))??;
        let mut cells: Vec<(u64, std::borrow::Cow<[u8]>)> = Vec::new();
        let mut pos = offset;
        let mut rest = data;
        while !rest.is_empty() {
            let cell = pos - pos % grid;
            let within = (pos - cell) as usize;
            let n = rest.len().min(grid as usize - within);
            let (part, tail) = rest.split_at(n);
            // One look at the base and its deltas: the new base replaces both.
            let existing = self.with_catalog(|c| target.cell_view(c, cell))??;
            let content = match existing {
                Some(view) if within > 0 || n < view.base.len() => {
                    let (_, mut buf) = self.read_view(view, Some(target))?;
                    if buf.len() < within + n {
                        buf.resize(within + n, 0);
                    }
                    buf[within..within + n].copy_from_slice(part);
                    buf.into()
                }
                None if within > 0 => {
                    let mut buf = vec![0u8; within + n];
                    buf[within..].copy_from_slice(part);
                    buf.into()
                }
                _ => part.into(),
            };
            cells.push((cell, content));
            pos += n as u64;
            rest = tail;
        }
        if let [(cell, content)] = cells.as_slice() {
            if !self.cfg.dedup {
                return self.install_extent(target, *cell, content, fence);
            }
        }
        self.place_and_commit(target, &cells, fence)
    }

    /// Places `cells` and commits them, deduplicating against live extents when configured. A
    /// share that went stale between lookup and commit (its extent was freed meanwhile) is
    /// rejected by the catalog; the cells are then written without sharing.
    fn place_and_commit(
        &self,
        target: &Target,
        cells: &[(u64, std::borrow::Cow<[u8]>)],
        fence: u64,
    ) -> Result<(), NativeError> {
        let alloc = self.lease(fence)?;
        let (placed, shared) = self.place_cells(cells, fence, &alloc, true)?;
        match self.commit_cells(target, placed, shared, fence, &alloc) {
            Err(e) if is_stale_share(&e) => {
                let alloc = self.lease(fence)?;
                let (placed, _) = self.place_cells(cells, fence, &alloc, false)?;
                self.commit_cells(target, placed, Vec::new(), fence, &alloc)
            }
            r => r,
        }
    }

    /// The writes that place their data without `write_lock`: whole grid cells, or with
    /// concurrent commits every write that reads nothing back. False: not one of them.
    fn write_unlocked(
        &self,
        target: &Target,
        offset: u64,
        data: &[u8],
        in_bounds: impl Fn(&Catalog) -> Result<(), NativeError>,
    ) -> Result<bool, NativeError> {
        if self.cfg.concurrent_commits {
            self.write_concurrent(target, offset, data, in_bounds)
        } else {
            self.write_aligned(target, offset, data, in_bounds)
        }
    }

    /// Writes whole grid cells without holding `write_lock` while their data goes to the data
    /// nodes: nothing is read back and merged, so only the commit needs the lock. Does nothing
    /// and returns false unless `offset` and the length are multiples of the target's grid.
    fn write_aligned(
        &self,
        target: &Target,
        offset: u64,
        data: &[u8],
        in_bounds: impl Fn(&Catalog) -> Result<(), NativeError>,
    ) -> Result<bool, NativeError> {
        if data.is_empty() {
            return Ok(false);
        }
        // The fence first: on a follower it fails as "not the leader", and on the leader it
        // waits until the catalog has applied every earlier entry (the target's creation).
        let fence = {
            let _write = self
                .write_lock
                .lock()
                .map_err(|_| NativeError::Poisoned("write"))?;
            self.write_fence()?
        };
        let grid = self.with_catalog(|c| target.grid(c, self.cfg.extent_bytes))??;
        if !offset.is_multiple_of(grid) || !(data.len() as u64).is_multiple_of(grid) {
            return Ok(false);
        }
        self.with_catalog(&in_bounds)??;
        let cells: Vec<(u64, std::borrow::Cow<[u8]>)> = data
            .chunks(grid as usize)
            .enumerate()
            .map(|(i, chunk)| (offset + i as u64 * grid, chunk.into()))
            .collect();
        let alloc = self.lease(fence)?;
        let (placed, shared) = self.place_cells(&cells, fence, &alloc, true)?;
        let _write = self
            .write_lock
            .lock()
            .map_err(|_| NativeError::Poisoned("write"))?;
        self.with_catalog(&in_bounds)??;
        match self.commit_cells(target, placed, shared, fence, &alloc) {
            Err(e) if is_stale_share(&e) => {
                let alloc = self.lease(fence)?;
                let (placed, _) = self.place_cells(&cells, fence, &alloc, false)?;
                self.commit_cells(target, placed, Vec::new(), fence, &alloc)?;
            }
            r => r?,
        }
        Ok(true)
    }

    /// Places each cell's content (a few at a time) and returns the extents, in cell order, and
    /// the ids among them that already existed (deduplicated against live extents).
    fn place_cells(
        &self,
        cells: &[(u64, std::borrow::Cow<[u8]>)],
        fence: u64,
        alloc: &inflight::Lease<'_>,
        dedup: bool,
    ) -> Result<(Vec<ExtentRef>, Vec<ExtentId>), NativeError> {
        let cells: Vec<(u64, &[u8])> = cells.iter().map(|(c, d)| (*c, d.as_ref())).collect();
        if !(dedup && self.cfg.dedup) {
            return Ok((self.place_new(&cells, fence, alloc)?, Vec::new()));
        }
        enum Plan {
            Existing(Box<ExtentRef>),
            New,
            SameAs(usize),
        }
        let hashes: Vec<[u8; 32]> = cells.iter().map(|(_, d)| checksum::sha256(d)).collect();
        let lens: Vec<usize> = cells.iter().map(|(_, d)| d.len()).collect();
        let found = self.dedup_lookup(&hashes, &lens)?;
        let mut plan = Vec::with_capacity(cells.len());
        let mut fresh: Vec<(u64, &[u8])> = Vec::new();
        let mut first_new: std::collections::HashMap<[u8; 32], usize> = Default::default();
        for (i, f) in found.into_iter().enumerate() {
            plan.push(match f {
                Some(ext) => Plan::Existing(Box::new(ext)),
                None => match first_new.get(&hashes[i]) {
                    Some(j) => Plan::SameAs(*j),
                    None => {
                        first_new.insert(hashes[i], i);
                        fresh.push(cells[i]);
                        Plan::New
                    }
                },
            });
        }
        let mut placed_new = self.place_new(&fresh, fence, alloc)?.into_iter();
        let mut out: Vec<ExtentRef> = Vec::with_capacity(cells.len());
        let mut shared = Vec::new();
        for (i, p) in plan.iter().enumerate() {
            let mut ext = match p {
                Plan::Existing(e) => {
                    shared.push(e.id.clone());
                    self.telemetry.dedup_hit(e.len());
                    (**e).clone()
                }
                Plan::New => {
                    let e = placed_new
                        .next()
                        .ok_or(NativeError::Poisoned("dedup plan"))?;
                    if let Ok(mut idx) = self.dedup_index.lock() {
                        idx.by_content.insert(e.content_sha(), e.id.clone());
                    }
                    e
                }
                Plan::SameAs(j) => {
                    let e = out[*j].clone();
                    if shared.contains(&e.id) {
                        shared.push(e.id.clone());
                    }
                    self.telemetry.dedup_hit(e.len());
                    e
                }
            };
            ext.logical_offset = cells[i].0;
            out.push(ext);
        }
        Ok((out, shared))
    }

    /// Live extents already holding exactly these bytes, per cell.
    fn dedup_lookup(
        &self,
        hashes: &[[u8; 32]],
        lens: &[usize],
    ) -> Result<Vec<Option<ExtentRef>>, NativeError> {
        let mut idx = self
            .dedup_index
            .lock()
            .map_err(|_| NativeError::Poisoned("dedup index"))?;
        if !idx.built {
            let scanned: Vec<([u8; 32], ExtentId)> = self.with_catalog(|c| {
                c.extents
                    .values()
                    .filter(|m| !m.tombstoned && m.refs > 0 && !m.extent.delta)
                    .map(|m| (m.extent.content_sha(), m.extent.id.clone()))
                    .collect()
            })?;
            idx.by_content.extend(scanned);
            idx.built = true;
        }
        let ids: Vec<Option<ExtentId>> = hashes
            .iter()
            .map(|h| idx.by_content.get(h).cloned())
            .collect();
        drop(idx);
        self.with_catalog(|c| {
            ids.iter()
                .zip(lens)
                .map(|(id, len)| {
                    id.as_ref()
                        .and_then(|id| c.extents.get(id))
                        .filter(|m| {
                            !m.tombstoned && m.refs > 0 && !m.extent.delta && m.extent.len() == *len
                        })
                        .map(|m| m.extent.clone())
                })
                .collect()
        })
    }

    /// Writes each cell's content to fresh replicas or shards, in cell order.
    fn place_new(
        &self,
        cells: &[(u64, &[u8])],
        fence: u64,
        alloc: &inflight::Lease<'_>,
    ) -> Result<Vec<ExtentRef>, NativeError> {
        let mut placed_all = Vec::with_capacity(cells.len());
        for window in cells.chunks(WRITE_PARALLELISM) {
            let placed: Vec<Result<ExtentRef, NativeError>> = if window.len() == 1 {
                vec![self.place_extent(window[0].1, fence, alloc)]
            } else {
                std::thread::scope(|s| {
                    let handles: Vec<_> = window
                        .iter()
                        .map(|(_, content)| s.spawn(|| self.place_extent(content, fence, alloc)))
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join()
                                .unwrap_or(Err(NativeError::Poisoned("extent writer")))
                        })
                        .collect()
                })
            };
            for ((cell, _), extent) in window.iter().zip(placed) {
                let mut extent = extent?;
                extent.logical_offset = *cell;
                placed_all.push(extent);
            }
        }
        Ok(placed_all)
    }

    /// Commits placed extents into `target` as one command.
    fn commit_cells(
        &self,
        target: &Target,
        placed_all: Vec<ExtentRef>,
        shared: Vec<ExtentId>,
        fence: u64,
        alloc: &inflight::Lease<'_>,
    ) -> Result<(), NativeError> {
        let bytes: usize = placed_all.iter().map(|e| e.len()).sum();
        let cmd = Self::cells_command(target, placed_all, shared);
        self.commit(cmd, Some(fence))
            .inspect_err(|e| alloc.settle(e))?;
        self.telemetry.record_write(bytes);
        Ok(())
    }

    /// The command that installs placed extents into `target`.
    fn cells_command(
        target: &Target,
        placed_all: Vec<ExtentRef>,
        shared: Vec<ExtentId>,
    ) -> MetaCommand {
        let end = placed_all
            .iter()
            .map(|e| e.logical_offset + e.len() as u64)
            .max()
            .unwrap_or(0);
        match target {
            Target::Volume(volume_id) => MetaCommand::InstallExtents {
                volume_id: volume_id.to_string(),
                extents: placed_all,
                shared,
            },
            Target::File { fs, ino, replica } => MetaCommand::Fs {
                op: FsOp::InstallFileExtents {
                    fs: fs.to_string(),
                    ino: *ino,
                    extents: placed_all,
                    size: end,
                    now_ns: now_ns(),
                    replica: *replica,
                    shared,
                },
            },
        }
    }

    /// Writes `chunk` to fresh replicas and commits it as the extent at `logical`.
    fn install_extent(
        &self,
        target: &Target,
        logical: u64,
        chunk: &[u8],
        fence: u64,
    ) -> Result<(), NativeError> {
        let alloc = self.lease(fence)?;
        let extent = self.place_extent(chunk, fence, &alloc)?;
        self.commit_extent(target, logical, chunk.len(), extent, fence)
            .inspect_err(|e| alloc.settle(e))
    }

    /// Writes `chunk` to `replicas` fresh copies (in parallel) and returns the extent to commit.
    fn place_extent(
        &self,
        chunk: &[u8],
        fence: u64,
        alloc: &inflight::Lease<'_>,
    ) -> Result<ExtentRef, NativeError> {
        let compressed = self
            .cfg
            .compression
            .then(|| crate::compress::compress(chunk))
            .flatten();
        let (payload, raw_len, content) = match &compressed {
            Some(p) => (p.as_slice(), chunk.len(), Some(checksum::sha256(chunk))),
            None => (chunk, 0, None),
        };
        if let Some(scheme) = self
            .cfg
            .erasure
            .filter(|_| payload.len() >= self.cfg.erasure_min_bytes)
        {
            return self.place_shards(scheme, payload, raw_len, content, fence, alloc);
        }
        let chunk = payload;
        let order = self.placement_order(chunk.len() as u64, |_| false);
        let replicas = self.write_replicas(order, chunk, fence, alloc)?;
        Ok(ExtentRef {
            id: Uuid::new_v4().to_string(),
            logical_offset: 0,
            payload_len: chunk.len(),
            raw_len,
            checksum: checksum::sha256(chunk),
            content,
            replicas,
            ec: None,
            created_ms: now_ms(),
            object: None,
            delta: false,
        })
    }

    /// Writes `chunk` to the configured number of nodes of `order` and returns the replicas.
    fn write_replicas(
        &self,
        order: Vec<String>,
        chunk: &[u8],
        fence: u64,
        alloc: &inflight::Lease<'_>,
    ) -> Result<Vec<ReplicaRef>, NativeError> {
        let needed = self.cfg.placement.replicas;
        if order.len() < needed {
            return Err(NativeError::InsufficientReplicas {
                needed,
                found: order.len(),
            });
        }
        // Write to the first `needed` nodes in parallel (each replica write is fsynced on its
        // node); a node that fails is replaced by the next eligible one in preference order.
        let mut replicas = Vec::new();
        let mut order = order.into_iter();
        while replicas.len() < needed {
            let batch: Vec<String> = order.by_ref().take(needed - replicas.len()).collect();
            if batch.is_empty() {
                break;
            }
            let placed: Vec<Result<Option<ReplicaRef>, NativeError>> = std::thread::scope(|s| {
                let handles: Vec<_> = batch
                    .iter()
                    .map(|node_id| s.spawn(|| self.place_replica(node_id, fence, chunk, alloc)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or(Err(NativeError::Poisoned("replica writer")))
                    })
                    .collect()
            });
            for r in placed {
                if let Some(r) = r? {
                    replicas.push(r);
                }
            }
        }
        if replicas.len() < needed {
            return Err(NativeError::InsufficientReplicas {
                needed,
                found: replicas.len(),
            });
        }
        Ok(replicas)
    }

    /// Whether every device of a node takes writes of any length without padding them: only
    /// such nodes hold deltas, since the padding of a block-aligned device would be lost.
    fn byte_granular(&self, node_id: &str) -> bool {
        let Ok(node) = self.node(node_id) else {
            return false;
        };
        node.devices.iter().all(|d| match d.byte_granular() {
            Ok(b) => b,
            Err(_) => {
                self.mark_down(node);
                false
            }
        })
    }

    /// Writes a delta's bytes to plain replicas (never compressed, erasure-coded or shared).
    /// `None` if too few nodes take byte-granular writes.
    fn place_delta(
        &self,
        data: &[u8],
        fence: u64,
        alloc: &inflight::Lease<'_>,
    ) -> Result<Option<ExtentRef>, NativeError> {
        let order = self.placement_order(data.len() as u64, |n| !self.byte_granular(&n.id));
        if order.len() < self.cfg.placement.replicas {
            return Ok(None);
        }
        let replicas = self.write_replicas(order, data, fence, alloc)?;
        Ok(Some(ExtentRef {
            id: Uuid::new_v4().to_string(),
            logical_offset: 0,
            payload_len: data.len(),
            raw_len: 0,
            checksum: checksum::sha256(data),
            content: None,
            replicas,
            ec: None,
            created_ms: now_ms(),
            object: None,
            delta: true,
        }))
    }

    /// Erasure-codes `chunk` and writes each shard to its own node (in parallel); a node that
    /// fails is replaced by the next eligible one.
    fn place_shards(
        &self,
        scheme: EcScheme,
        chunk: &[u8],
        raw_len: usize,
        content: Option<[u8; 32]>,
        fence: u64,
        alloc: &inflight::Lease<'_>,
    ) -> Result<ExtentRef, NativeError> {
        let enc = scheme.encode(chunk)?;
        let needed = scheme.shards();
        let mut spare = self
            .placement_order(enc.layout.shard_len as u64, |_| false)
            .into_iter();
        let mut slots: Vec<Option<ReplicaRef>> = vec![None; needed];
        let mut pending = Vec::with_capacity(needed);
        for i in 0..needed {
            match spare.next() {
                Some(node) => pending.push((i, node)),
                None => return Err(NativeError::InsufficientReplicas { needed, found: i }),
            }
        }
        while !pending.is_empty() {
            let placed: Vec<Result<Option<ReplicaRef>, NativeError>> = std::thread::scope(|s| {
                let handles: Vec<_> = pending
                    .iter()
                    .map(|(i, node)| {
                        let shard = &enc.shards[*i];
                        s.spawn(move || self.place_replica(node, fence, shard, alloc))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or(Err(NativeError::Poisoned("shard writer")))
                    })
                    .collect()
            });
            let mut retry = Vec::new();
            for ((i, _), r) in pending.iter().zip(placed) {
                match r? {
                    Some(rep) => slots[*i] = Some(rep),
                    None => match spare.next() {
                        Some(node) => retry.push((*i, node)),
                        None => {
                            return Err(NativeError::InsufficientReplicas {
                                needed,
                                found: slots.iter().flatten().count(),
                            })
                        }
                    },
                }
            }
            pending = retry;
        }
        Ok(ExtentRef {
            id: Uuid::new_v4().to_string(),
            logical_offset: 0,
            payload_len: chunk.len(),
            raw_len,
            checksum: checksum::sha256(chunk),
            content,
            replicas: slots.into_iter().flatten().collect(),
            ec: Some(enc.layout),
            created_ms: now_ms(),
            object: None,
            delta: false,
        })
    }

    /// Commits a placed extent of `len` bytes at `logical` of `target`.
    fn commit_extent(
        &self,
        target: &Target,
        logical: u64,
        len: usize,
        mut extent: ExtentRef,
        fence: u64,
    ) -> Result<(), NativeError> {
        extent.logical_offset = logical;
        let cmd = match target {
            Target::Volume(volume_id) => MetaCommand::InstallExtent {
                volume_id: volume_id.to_string(),
                logical_offset: logical,
                extent,
            },
            Target::File { fs, ino, replica } => MetaCommand::Fs {
                op: FsOp::InstallFileExtent {
                    fs: fs.to_string(),
                    ino: *ino,
                    logical_offset: logical,
                    size: logical + len as u64,
                    extent,
                    now_ns: now_ns(),
                    replica: *replica,
                },
            },
        };
        self.commit(cmd, Some(fence))?;
        self.telemetry.record_write(len);
        Ok(())
    }

    /// Reads `len` bytes at any `offset` within the volume, across extents. Never-written ranges
    /// read as zeros.
    pub fn read(&self, volume_id: &str, offset: u64, len: usize) -> Result<Vec<u8>, NativeError> {
        let cells = self.with_catalog(|c| {
            let vol = c
                .volumes
                .get(volume_id)
                .ok_or_else(|| NativeError::NotFound(volume_id.into()))?;
            let grid = self.cfg.extent_bytes as u64;
            let maps = (&vol.extents, Some(&vol.deltas));
            self.cells_in(c, maps, grid, vol.size_bytes, offset, len)
        })??;
        self.read_range(cells, offset, len, Some(&Target::Volume(volume_id)))
    }

    pub fn create_snapshot(
        &self,
        volume_id: &str,
        name: impl Into<String>,
    ) -> Result<SnapshotId, NativeError> {
        self.create_snapshot_as(Uuid::new_v4().to_string(), volume_id, name)
    }

    /// [`Self::create_snapshot`] with a caller-chosen id (idempotent).
    pub fn create_snapshot_as(
        &self,
        id: String,
        volume_id: &str,
        name: impl Into<String>,
    ) -> Result<SnapshotId, NativeError> {
        let cmd = MetaCommand::CreateSnapshot {
            id: id.clone(),
            volume_id: volume_id.to_string(),
            name: name.into(),
        };
        self.commit_folded(Scope::Volume(volume_id.to_string()), |fence| {
            self.commit(cmd.clone(), fence)
        })?;
        Ok(id)
    }

    pub fn delete_snapshot(&self, snapshot_id: &str) -> Result<(), NativeError> {
        self.commit(
            MetaCommand::DeleteSnapshot {
                snapshot_id: snapshot_id.to_string(),
                now_unix: unix_now(),
            },
            None,
        )
    }

    /// Retention-locks a snapshot for `retain_secs` from now (WORM). The lock can later be
    /// extended but never shortened, and `delete_snapshot` is refused until it expires.
    pub fn lock_snapshot(&self, snapshot_id: &str, retain_secs: u64) -> Result<u64, NativeError> {
        let retain_until_unix = unix_now().saturating_add(retain_secs);
        self.commit(
            MetaCommand::LockSnapshot {
                snapshot_id: snapshot_id.to_string(),
                retain_until_unix,
            },
            None,
        )?;
        Ok(retain_until_unix)
    }

    /// Clears a retention lock. The gateway calls this only after two distinct admins confirm.
    /// Refused when the snapshot is not locked.
    pub fn unlock_snapshot(&self, snapshot_id: &str) -> Result<(), NativeError> {
        self.commit(
            MetaCommand::UnlockSnapshot {
                snapshot_id: snapshot_id.to_string(),
            },
            None,
        )
    }

    pub fn read_snapshot(
        &self,
        snapshot_id: &str,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, NativeError> {
        let cells = self.with_catalog(|c| {
            let s = c
                .snapshots
                .get(snapshot_id)
                .ok_or_else(|| NativeError::NotFound(snapshot_id.into()))?;
            let size = if s.size_bytes > 0 {
                s.size_bytes
            } else {
                c.written_end(&s.extents)
            };
            self.cells_in(
                c,
                (&s.extents, None),
                self.cfg.extent_bytes as u64,
                size,
                offset,
                len,
            )
        })??;
        self.read_range(cells, offset, len, None)
    }

    /// What compression and sharing save, from the live catalog. `referenced_bytes` counts an
    /// extent once per reference (volume, file, snapshot, clone), `logical_bytes` once; so their
    /// ratio is sharing of every kind (snapshots and clones included, not only deduplication),
    /// while `stored_bytes / logical_bytes` is compression alone. `stored_bytes` is one copy of
    /// the payload; replicas or erasure coding multiply it.
    pub fn data_reduction(&self) -> Result<ReductionStats, NativeError> {
        self.with_catalog(|c| {
            let mut s = ReductionStats::default();
            for m in c.extents.values().filter(|m| !m.tombstoned && m.refs > 0) {
                let len = m.extent.len() as u64;
                s.extents += 1;
                s.logical_bytes += len;
                s.referenced_bytes += len * m.refs;
                s.stored_bytes += m.extent.payload_len as u64;
                if m.extent.raw_len > 0 {
                    s.compressed_extents += 1;
                }
                if m.refs > 1 {
                    s.shared_extents += 1;
                }
            }
            s.cross_group_dedup = self.cfg.cross_group_dedup;
            s
        })
    }

    /// Reclaims dead extents. While delta writes are in use, one is reclaimed only after it has
    /// been seen dead for [`DeltaConfig::gc_grace_ms`].
    pub fn gc_once(&self) -> Result<gc::GcStats, NativeError> {
        let _pass = self
            .delta
            .reclaiming
            .lock()
            .map_err(|_| NativeError::Poisoned("gc"))?;
        // Candidates come from applied state; an extent already reclaimed by an applied-but-
        // unseen entry would otherwise be proposed again and rejected.
        let term = match &self.meta {
            Meta::Local { .. } => 0,
            Meta::Raft { server, timeout } => server.leader_ready(*timeout)?,
        };
        let enabled = self.cfg.delta_writes.enabled;
        let (candidates, free_before) = self.with_catalog(|c| {
            let in_use = enabled || !c.dirty.is_empty();
            let candidates: Vec<(ExtentId, bool)> = gc::collect_candidates(c)
                .into_iter()
                .map(|id| {
                    let graced = in_use || c.extents.get(&id).is_some_and(|m| m.extent.delta);
                    (id, graced)
                })
                .collect();
            (candidates, c.free.total_bytes())
        })?;
        let mut stats = gc::GcStats {
            candidates: candidates.len() as u64,
            ..Default::default()
        };
        let ripe = self.ripe(term, candidates)?;
        self.reclaim(&ripe, term, |batch| stats.reclaimed += batch.len() as u64)?;
        stats.freed_bytes = self.free_bytes()?.saturating_sub(free_before);
        self.telemetry.gc_reclaimed(stats.reclaimed);
        Ok(stats)
    }

    /// Scrubs every extent (reads each replica and verifies its checksum) and re-replicates
    /// replicas that are unreachable or corrupt onto healthy nodes, preserving host diversity.
    /// Under Raft only the leader's engine can repair. Writers are held off only while a
    /// replacement is placed and committed, not for the whole scan.
    pub fn repair_once(&self) -> Result<RepairStats, NativeError> {
        let fence = self.write_fence()?;
        let _repair = self
            .repair_lock
            .lock()
            .map_err(|_| NativeError::Poisoned("repair"))?;
        let ids: Vec<String> = self.with_catalog(|c| c.extents.keys().cloned().collect())?;
        let mut st = RepairStats::default();
        for id in ids {
            self.repair_extent_with(&id, &BTreeSet::new(), fence, &mut st)?;
        }
        Ok(st)
    }

    /// Checks one extent and rebuilds its bad parts. Parts on `lost` nodes count as bad without
    /// being read. An extent that no longer exists is skipped.
    fn repair_extent_with(
        &self,
        id: &str,
        lost: &BTreeSet<String>,
        fence: u64,
        st: &mut RepairStats,
    ) -> Result<(), NativeError> {
        let Some(ext) = self.with_catalog(|c| c.extents.get(id).map(|e| e.extent.clone()))? else {
            return Ok(());
        };
        st.extents_checked += 1;
        if let Some(ec) = &ext.ec {
            let shards: Vec<Option<Vec<u8>>> = (0..ec.shards())
                .map(|i| {
                    if lost.contains(&ext.replicas[i].node_id) {
                        return None;
                    }
                    let shard = self.read_shard(&ext, ec, i);
                    st.bytes_read += shard.as_ref().map_or(0, |b| b.len() as u64);
                    shard
                })
                .collect();
            let bad: Vec<usize> = (0..shards.len()).filter(|i| shards[*i].is_none()).collect();
            if bad.is_empty() {
                return Ok(());
            }
            let Ok(all) = ec.rebuild(shards) else {
                st.unrecoverable += 1;
                return Ok(());
            };
            let bad = bad
                .into_iter()
                .map(|i| (ext.replicas[i].clone(), all[i].as_slice()))
                .collect();
            return self.replace_replicas(&ext, bad, fence, st);
        }
        let mut good: Option<Vec<u8>> = None;
        let mut bad = Vec::new();
        for r in &ext.replicas {
            let Some((node, device)) = self
                .nodes
                .iter()
                .find(|n| n.spec.id == r.node_id && n.spec.healthy && !lost.contains(&n.spec.id))
                .and_then(|n| n.devices.get(r.device_index).map(|d| (n, d)))
            else {
                bad.push(r.clone());
                continue;
            };
            match device.read_exact_at(r.offset, ext.payload_len) {
                Ok(buf) if checksum::verify(&buf, &ext.checksum) => {
                    st.bytes_read += buf.len() as u64;
                    self.mark_up(node);
                    good.get_or_insert(buf);
                }
                Ok(_) => {
                    self.telemetry.checksum_failure();
                    bad.push(r.clone());
                }
                Err(_) => {
                    self.mark_down(node);
                    bad.push(r.clone());
                }
            }
        }
        if bad.is_empty() {
            return Ok(());
        }
        let Some(data) = good else {
            st.unrecoverable += 1;
            return Ok(());
        };
        let bad = bad.into_iter().map(|r| (r, data.as_slice())).collect();
        self.replace_replicas(&ext, bad, fence, st)
    }

    /// Writes each bad replica's (or shard's) bytes to a new node that holds no other part of
    /// the extent, then commits the move. Stops at the first commit the extent's state rejects.
    /// Holds `write_lock`, and does nothing if the extent changed since it was read.
    fn replace_replicas(
        &self,
        ext: &ExtentRef,
        bad: Vec<(ReplicaRef, &[u8])>,
        fence: u64,
        st: &mut RepairStats,
    ) -> Result<(), NativeError> {
        let _write = self
            .write_lock
            .lock()
            .map_err(|_| NativeError::Poisoned("write"))?;
        let unchanged = self.with_catalog(|c| {
            c.extents
                .get(&ext.id)
                .is_some_and(|e| e.extent.replicas == ext.replicas)
        })?;
        if !unchanged {
            st.deferred += 1;
            return Ok(());
        }
        let mut current = ext.replicas.clone();
        for (old, data) in bad {
            let others: Vec<&ReplicaRef> = current.iter().filter(|r| **r != old).collect();
            let taken_nodes: BTreeSet<&str> = others.iter().map(|r| r.node_id.as_str()).collect();
            let taken_hosts: BTreeSet<&str> = others
                .iter()
                .filter_map(|r| self.node(&r.node_id).ok())
                .map(|n| n.spec.failure_domain.host.as_str())
                .collect();
            let distinct = self.cfg.placement.require_distinct_hosts;
            let order = self.placement_order(data.len() as u64, |n| {
                taken_nodes.contains(n.id.as_str())
                    || (distinct && taken_hosts.contains(n.failure_domain.host.as_str()))
                    || (ext.delta && !self.byte_granular(&n.id))
            });
            let mut placed = None;
            let alloc = self.lease(fence)?;
            for node_id in order {
                if let Some(r) = self.place_replica(&node_id, fence, data, &alloc)? {
                    placed = Some(r);
                    break;
                }
            }
            let Some(new) = placed else {
                st.deferred += 1;
                continue;
            };
            let cmd = MetaCommand::ReplaceReplica {
                extent_id: ext.id.clone(),
                old: old.clone(),
                new: new.clone(),
            };
            match self
                .commit(cmd, Some(fence))
                .inspect_err(|e| alloc.settle(e))
            {
                Ok(()) => {
                    st.replicas_repaired += 1;
                    st.bytes_written += data.len() as u64;
                    self.telemetry.replica_repaired();
                    if let Some(slot) = current.iter_mut().find(|r| **r == old) {
                        *slot = new;
                    }
                }
                // The extent was reclaimed or rewritten meanwhile; the next pass sees the new state.
                Err(NativeError::Metadata(_) | NativeError::Raft(RaftError::Rejected(_))) => {
                    st.deferred += 1;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Errors with "not the leader" unless this engine is the leader and has applied its whole
    /// log (so it serves every committed change, even right after an election). Always true for
    /// a local WAL.
    pub fn ensure_leader(&self) -> Result<(), NativeError> {
        if let Meta::Raft { server, timeout } = &self.meta {
            server.leader_ready(*timeout)?;
        }
        Ok(())
    }

    /// Whether this replica's applied catalog holds an object of `kind` named `id`.
    pub fn holds(&self, kind: ObjectKind, id: &str) -> Result<bool, NativeError> {
        self.with_catalog(|c| match kind {
            ObjectKind::Volume => c.volumes.contains_key(id),
            ObjectKind::Snapshot => c.snapshots.contains_key(id),
            ObjectKind::Filesystem => c.filesystems.contains_key(id),
            ObjectKind::FsSnapshot => c.fs_snapshots.contains_key(id),
        })
    }

    /// Waits until this replica's catalog reflects every metadata write committed before the
    /// call, so reads served from it are linearizable on a follower too (see
    /// [`RaftServer::read_barrier`]). A local engine is always current. The Raft leader may
    /// skip the quorum round while its read lease is still valid.
    pub fn read_barrier(&self) -> Result<(), NativeError> {
        if let Meta::Raft { server, timeout } = &self.meta {
            server.read_barrier(*timeout)?;
        }
        Ok(())
    }

    /// Volumes in the applied catalog, ordered by id.
    pub fn volumes(&self) -> Result<Vec<VolumeInfo>, NativeError> {
        self.with_catalog(|c| {
            c.volumes
                .values()
                .map(|v| VolumeInfo {
                    id: v.id.clone(),
                    name: v.name.clone(),
                    size_bytes: v.size_bytes,
                    extents: v.extents.len(),
                })
                .collect()
        })
    }

    /// A copy of the applied catalog (for tests).
    #[doc(hidden)]
    pub fn catalog_copy(&self) -> Result<Catalog, NativeError> {
        self.with_catalog(Catalog::clone)
    }

    /// See [`Catalog::check_delta_invariants`] (for tests).
    #[doc(hidden)]
    pub fn check_delta_invariants(&self) -> Result<(), NativeError> {
        self.with_catalog(Catalog::check_delta_invariants)?
            .map_err(NativeError::Invalid)
    }

    /// Current per-node health.
    pub fn node_status(&self) -> Vec<NodeStatus> {
        self.nodes
            .iter()
            .map(|n| NodeStatus {
                id: n.spec.id.clone(),
                up: self.is_up(n),
                failures: n.health.failures.load(Ordering::Relaxed),
            })
            .collect()
    }

    pub fn applied_index(&self) -> Result<u64, NativeError> {
        self.with_catalog(|c| c.applied_index)
    }

    /// Device bytes (summed over replicas) currently on the free lists.
    pub fn free_bytes(&self) -> Result<u64, NativeError> {
        self.with_catalog(|c| c.free.total_bytes())
    }

    /// Bytes in use across `node_id`'s devices.
    pub fn device_len(&self, node_id: &str) -> Result<u64, NativeError> {
        self.node(node_id)?.devices.iter().map(|d| d.len()).sum()
    }

    /// Records currently retained in the metadata log (the local WAL, or the Raft log above its
    /// compaction point).
    pub fn wal_records(&self) -> Result<u64, NativeError> {
        match &self.meta {
            Meta::Local { wal, .. } => {
                Ok(wal.lock().map_err(|_| NativeError::Poisoned("wal"))?.len())
            }
            Meta::Raft { server, .. } => Ok(server.log_records()?),
        }
    }

    /// Prometheus text exposition for I/O counters, metadata and free space.
    pub fn render_metrics(&self) -> Result<String, NativeError> {
        let t = self.telemetry.snapshot();
        let (tiered, tiered_bytes) = self.with_catalog(|c| {
            c.extents
                .values()
                .filter(|e| e.extent.object.is_some())
                .fold((0u64, 0u64), |(n, b), e| {
                    (n + 1, b + e.extent.payload_len as u64)
                })
        })?;
        let (applied, volumes, snapshots, extents, free_bytes, free_ranges, sessions, locks) = self
            .with_catalog(|c| {
                (
                    c.applied_index,
                    c.volumes.len(),
                    c.snapshots.len(),
                    c.extents.len(),
                    c.free.total_bytes(),
                    c.free.ranges().len(),
                    c.leases.sessions.len(),
                    c.leases.lock_count(),
                )
            })?;
        let wal_records = self.wal_records()?;
        let (dirty_cells, delta) = self.with_catalog(|c| (c.dirty.len(), c.delta_totals))?;
        let mut p = PromText::new();
        for (name, help, v) in [
            (
                "atlas_native_concurrent_commits_total",
                "Writes committed without holding the write lock for the commit.",
                t.concurrent_commits,
            ),
            (
                "atlas_native_concurrent_commit_conflicts_total",
                "Such writes whose cell changed before they could be proposed.",
                t.concurrent_conflicts,
            ),
            ("atlas_native_reads_total", "Extent reads served.", t.reads),
            ("atlas_native_writes_total", "Extents written.", t.writes),
            (
                "atlas_native_read_bytes_total",
                "Bytes returned by reads.",
                t.read_bytes,
            ),
            (
                "atlas_native_write_bytes_total",
                "Bytes written (before replication).",
                t.write_bytes,
            ),
            (
                "atlas_native_checksum_failures_total",
                "Replica reads that failed checksum verification.",
                t.checksum_failures,
            ),
            (
                "atlas_native_replica_fallbacks_total",
                "Reads served by a non-primary replica.",
                t.replica_fallbacks,
            ),
            (
                "atlas_native_gc_reclaimed_extents_total",
                "Extents reclaimed by GC.",
                t.gc_reclaimed,
            ),
            (
                "atlas_native_replica_write_failures_total",
                "Replica writes that failed and moved to another node.",
                t.replica_write_failures,
            ),
            (
                "atlas_native_replicas_repaired_total",
                "Replicas re-created by repair.",
                t.replicas_repaired,
            ),
            (
                "atlas_native_extents_tiered_total",
                "Extents moved to object storage.",
                t.extents_tiered,
            ),
            (
                "atlas_native_object_reads_total",
                "Extent reads served from object storage.",
                t.object_reads,
            ),
            (
                "atlas_native_dedup_hits_total",
                "Extent writes that referenced an existing identical extent.",
                t.dedup_hits,
            ),
            (
                "atlas_native_dedup_saved_bytes_total",
                "Bytes those writes did not have to store.",
                t.dedup_saved_bytes,
            ),
            (
                "atlas_native_delta_writes_total",
                "Writes committed as deltas.",
                t.delta_writes,
            ),
            (
                "atlas_native_delta_folds_total",
                "Cells folded into a new base extent.",
                t.delta_folds,
            ),
            (
                "atlas_native_delta_fallbacks_total",
                "Eligible writes that rewrote their cell instead.",
                t.delta_fallbacks,
            ),
            (
                "atlas_native_delta_fold_conflicts_total",
                "Folds the catalog rejected, normally because the cell changed first.",
                t.delta_fold_conflicts,
            ),
            (
                "atlas_native_delta_folded_deltas_total",
                "Deltas consumed by committed folds.",
                t.delta_folded_deltas,
            ),
        ] {
            p.single(name, "counter", help, v);
        }
        let timers = &self.telemetry;
        for (name, help, phases) in [
            (
                "atlas_native_delta_write_seconds",
                "Delta write attempts: time to replicate the bytes and to commit them.",
                &[
                    ("replicate", &timers.delta_replicate),
                    ("commit", &timers.delta_commit),
                ][..],
            ),
            (
                "atlas_native_delta_fold_seconds",
                "Fold attempts: time to read the cell, write the new base and commit it.",
                &[
                    ("read", &timers.fold_read),
                    ("write", &timers.fold_write),
                    ("commit", &timers.fold_commit),
                ][..],
            ),
        ] {
            p.family(name, "histogram", help);
            for (phase, h) in phases {
                p.histogram(name, &[("phase", phase)], &h.snapshot());
            }
        }
        p.single(
            "atlas_native_metadata_applied_index",
            "gauge",
            "Highest metadata index applied.",
            applied,
        );
        p.single(
            "atlas_native_wal_records",
            "gauge",
            "Records retained in the metadata log.",
            wal_records,
        );
        p.single(
            "atlas_native_volumes",
            "gauge",
            "Volumes in the catalog.",
            volumes,
        );
        p.single(
            "atlas_native_snapshots",
            "gauge",
            "Snapshots in the catalog.",
            snapshots,
        );
        p.single(
            "atlas_native_extents",
            "gauge",
            "Live physical extents.",
            extents,
        );
        p.single(
            "atlas_native_tiered_extents",
            "gauge",
            "Live extents held in object storage instead of on data nodes.",
            tiered,
        );
        p.single(
            "atlas_native_tiered_bytes",
            "gauge",
            "Bytes of the live extents held in object storage.",
            tiered_bytes,
        );
        p.single(
            "atlas_native_delta_dirty_cells",
            "gauge",
            "Cells with pending deltas.",
            dirty_cells,
        );
        p.single(
            "atlas_native_delta_pending",
            "gauge",
            "Deltas not folded yet.",
            delta.pending,
        );
        p.single(
            "atlas_native_delta_pending_bytes",
            "gauge",
            "Device bytes (summed over replicas) of pending deltas.",
            delta.pending_bytes,
        );
        p.single(
            "atlas_native_dead_bytes",
            "gauge",
            "Device bytes (summed over replicas) of dead extents not reclaimed yet.",
            delta.dead_bytes,
        );
        p.single(
            "atlas_native_client_sessions",
            "gauge",
            "Open client sessions (leases).",
            sessions,
        );
        p.single(
            "atlas_native_file_locks",
            "gauge",
            "File locks held by client sessions.",
            locks,
        );
        p.single(
            "atlas_native_allocator_free_bytes",
            "gauge",
            "Device bytes (summed over replicas) on the free lists.",
            free_bytes,
        );
        p.single(
            "atlas_native_allocator_free_ranges",
            "gauge",
            "Free ranges across all devices.",
            free_ranges,
        );
        p.family(
            "atlas_native_device_bytes",
            "gauge",
            "Bytes in use across each reachable node's devices.",
        );
        for n in &self.nodes {
            // An unreachable data node has no sample rather than failing the whole scrape.
            if let Ok(len) = n.devices.iter().map(|d| d.len()).sum::<Result<u64, _>>() {
                p.sample(
                    "atlas_native_device_bytes",
                    &[("node", n.spec.id.as_str())],
                    len,
                );
            }
        }
        p.family(
            "atlas_native_node_up",
            "gauge",
            "1 if the node is eligible for placement, 0 while it is backed off after failures.",
        );
        for st in self.node_status() {
            p.sample(
                "atlas_native_node_up",
                &[("node", st.id.as_str())],
                u8::from(st.up),
            );
        }
        p.family(
            "atlas_native_node_failures_total",
            "counter",
            "I/O failures observed against each node.",
        );
        for st in self.node_status() {
            p.sample(
                "atlas_native_node_failures_total",
                &[("node", st.id.as_str())],
                st.failures,
            );
        }
        Ok(p.finish())
    }

    #[cfg(test)]
    pub(crate) fn set_log_sync(&self, sync: Box<dyn crate::device::DeviceSync>) {
        if let Meta::Local { wal, .. } = &self.meta {
            wal.lock().unwrap().set_sync(sync);
        }
    }

    /// Persists the catalog and drops every WAL record it covers. Returns the records removed.
    /// Raft-backed engines compact their log inside the Raft node instead.
    pub fn checkpoint(&self) -> Result<u64, NativeError> {
        let Meta::Local {
            catalog,
            wal,
            store,
        } = &self.meta
        else {
            return Err(NativeError::Invalid(
                "checkpoint applies to the local WAL; Raft compacts its own log".into(),
            ));
        };
        let mut wal = wal.lock().map_err(|_| NativeError::Poisoned("wal"))?;
        let mut c = catalog
            .write()
            .map_err(|_| NativeError::Poisoned("catalog"))?;
        store.checkpoint(&mut c)?;
        let removed = wal.compact_through(c.applied_index);
        self.note_log_failure(&wal);
        Ok(removed?)
    }

    fn note_log_failure(&self, wal: &Wal) {
        if wal.check().is_err() {
            self.log_failed.store(true, Ordering::SeqCst);
        }
    }

    fn is_up(&self, n: &NodeRuntime) -> bool {
        n.spec.healthy
            && n.health
                .down_until
                .lock()
                .map(|d| d.is_none_or(|t| Instant::now() >= t))
                .unwrap_or(false)
    }

    fn mark_down(&self, n: &NodeRuntime) {
        n.health.failures.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        if let Ok(mut d) = n.health.down_until.lock() {
            *d = Some(now + self.cfg.node_retry_after);
        }
        if let Ok(mut d) = n.health.down_since.lock() {
            d.get_or_insert(now);
        }
    }

    fn mark_up(&self, n: &NodeRuntime) {
        if let Ok(mut d) = n.health.down_until.lock() {
            *d = None;
        }
        if let Ok(mut d) = n.health.down_since.lock() {
            *d = None;
        }
    }

    /// Every eligible node not excluded by `skip`, in placement preference order (rack spread
    /// first, distinct hosts when the policy requires them).
    fn placement_order(&self, len: u64, skip: impl Fn(&Node) -> bool) -> Vec<String> {
        let specs: Vec<Node> = self
            .nodes
            .iter()
            .filter(|n| self.is_up(n) && !skip(&n.spec))
            .map(|n| n.spec.clone())
            .collect();
        let policy = PlacementPolicy {
            replicas: specs.len(),
            ..self.cfg.placement
        };
        select_replicas(&specs, len, policy)
    }

    /// Writes one replica of `data` to `node_id`, reusing free space when there is some. An I/O
    /// failure backs the node off and returns `None` so the caller can try the next node; a
    /// fenced write means this engine was deposed and is returned as an error.
    fn place_replica(
        &self,
        node_id: &str,
        fence: u64,
        data: &[u8],
        alloc: &inflight::Lease<'_>,
    ) -> Result<Option<ReplicaRef>, NativeError> {
        let node = self.node(node_id)?;
        let len = data.len() as u64;
        let device_index = node.next_device.fetch_add(1, Ordering::Relaxed) % node.devices.len();
        let free_off = alloc.take(node_id, device_index, len)?;
        let device = &node.devices[device_index];
        let written = match free_off {
            Some(off) => device.write_at(fence, off, data).map(|()| off),
            None => device.append(fence, data),
        };
        match written {
            Ok(offset) => {
                self.mark_up(node);
                Ok(Some(ReplicaRef {
                    node_id: node_id.to_string(),
                    device_index,
                    offset,
                }))
            }
            Err(e @ NativeError::Fenced { .. }) => Err(e),
            Err(_) => {
                self.mark_down(node);
                self.telemetry.replica_write_failure();
                Ok(None)
            }
        }
    }

    fn node(&self, node_id: &str) -> Result<&NodeRuntime, NativeError> {
        self.nodes
            .iter()
            .find(|n| n.spec.id == node_id)
            .ok_or_else(|| NativeError::NotFound(node_id.into()))
    }

    fn with_catalog<R>(&self, f: impl FnOnce(&Catalog) -> R) -> Result<R, NativeError> {
        match &self.meta {
            Meta::Local { catalog, .. } => {
                let c = catalog
                    .read()
                    .map_err(|_| NativeError::Poisoned("catalog"))?;
                Ok(f(&c))
            }
            Meta::Raft { server, .. } => Ok(server.with_catalog(f)?),
        }
    }

    /// The cells of an extent map overlapping `[offset, offset + len)`, which must lie within
    /// `size`, each with its deltas: one look at the catalog for the whole range.
    fn cells_in(
        &self,
        c: &Catalog,
        (extents, deltas): (&BTreeMap<u64, ExtentId>, Option<&DeltaMap>),
        grid: u64,
        size: u64,
        offset: u64,
        len: usize,
    ) -> Result<Vec<CellView>, NativeError> {
        let end = offset
            .checked_add(len as u64)
            .filter(|e| *e <= size)
            .ok_or_else(|| NativeError::Invalid("read exceeds volume size".into()))?;
        if len == 0 {
            return Ok(Vec::new());
        }
        let first = offset - offset % grid;
        extents
            .range(first..end)
            .map(|(at, eid)| {
                // The catalog keeps one ExtentRef per extent id, but a shared extent can sit at
                // a different offset in each file or volume that references it.
                let base = c
                    .extents
                    .get(eid)
                    .map(|m| ExtentRef {
                        logical_offset: *at,
                        ..m.extent.clone()
                    })
                    .ok_or_else(|| NativeError::NotFound(eid.clone()))?;
                let list = deltas.and_then(|d| d.get(at));
                Ok(CellView {
                    base,
                    deltas: delta_extents(c, *at, list.map_or(&[][..], Vec::as_slice))?,
                })
            })
            .collect()
    }

    /// The bytes of `[offset, offset + len)` from `cells`; ranges no cell covers are zeros.
    /// `source` is where to look a cell up again if it changes while it is read.
    fn read_range(
        &self,
        cells: Vec<CellView>,
        offset: u64,
        len: usize,
        source: Option<&Target>,
    ) -> Result<Vec<u8>, NativeError> {
        let mut out = vec![0u8; len];
        let end = offset + len as u64;
        let wanted: Vec<CellView> = cells
            .into_iter()
            .filter(|v| {
                let at = v.base.logical_offset;
                at.max(offset) < (at + v.base.len() as u64).min(end)
            })
            .collect();
        // `Target` holds only borrowed ids, so each reader can have the reference.
        for window in wanted.chunks(READ_PARALLELISM) {
            let bufs: Vec<Result<Vec<u8>, NativeError>> = if window.len() == 1 {
                vec![self.read_view(window[0].clone(), source).map(|r| r.1)]
            } else {
                std::thread::scope(|s| {
                    let handles: Vec<_> = window
                        .iter()
                        .map(|v| s.spawn(move || self.read_view(v.clone(), source).map(|r| r.1)))
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join()
                                .unwrap_or(Err(NativeError::Poisoned("extent reader")))
                        })
                        .collect()
                })
            };
            for (view, buf) in window.iter().zip(bufs) {
                let buf = buf?;
                let at = view.base.logical_offset;
                let start = at.max(offset);
                // A cell looked up again may have become shorter (a truncate).
                let stop = (at + buf.len() as u64).min(end);
                if start < stop {
                    out[(start - offset) as usize..(stop - offset) as usize]
                        .copy_from_slice(&buf[(start - at) as usize..(stop - at) as usize]);
                }
            }
        }
        Ok(out)
    }

    /// Builds a command from the catalog it will apply on (under Raft, the applied one plus
    /// every entry proposed so far) and proposes it in `term`, with nothing in between.
    /// `build` returning `None` proposes nothing.
    fn commit_with(
        &self,
        term: u64,
        build: impl FnOnce(&Catalog) -> Option<MetaCommand>,
    ) -> Result<Proposed, NativeError> {
        match &self.meta {
            Meta::Local {
                catalog,
                wal,
                store,
            } => {
                let mut wal = wal.lock().map_err(|_| NativeError::Poisoned("wal"))?;
                // As in `commit`: nothing is applied once the log has failed.
                wal.check()?;
                let mut c = catalog
                    .write()
                    .map_err(|_| NativeError::Poisoned("catalog"))?;
                if let Some(command) = build(&c) {
                    self.commit_local(&mut c, &mut wal, store, command)?;
                }
                Ok(Proposed::Done)
            }
            Meta::Raft { server, .. } => Ok(match server.propose_with(term, build)? {
                Some(index) => Proposed::Raft { index, term },
                None => Proposed::Done,
            }),
        }
    }

    /// Waits for what [`Self::commit_with`] proposed to apply.
    fn await_commit(&self, proposed: Proposed) -> Result<(), NativeError> {
        if let (Meta::Raft { server, timeout }, Proposed::Raft { index, term }) =
            (&self.meta, proposed)
        {
            server.wait_applied(index, term, *timeout)?;
        }
        Ok(())
    }

    /// `fence` is the term data was written under; a Raft proposal is refused if leadership
    /// moved to another term since.
    fn commit(&self, command: MetaCommand, fence: Option<u64>) -> Result<(), NativeError> {
        let (catalog, wal, store) = match &self.meta {
            Meta::Local {
                catalog,
                wal,
                store,
            } => (catalog, wal, store),
            Meta::Raft { server, timeout } => {
                match fence {
                    Some(term) => server.propose_in_term(command, term, *timeout)?,
                    None => server.propose(command, *timeout)?,
                };
                return Ok(());
            }
        };
        let mut wal = wal.lock().map_err(|_| NativeError::Poisoned("wal"))?;
        // Before anything is applied: the rollback below replays the log up to this command's
        // index, which after an earlier failure would bring the unacknowledged record back.
        wal.check()?;
        let mut c = catalog
            .write()
            .map_err(|_| NativeError::Poisoned("catalog"))?;
        self.commit_local(&mut c, &mut wal, store, command)
    }

    /// Applies `command` to the local catalog and logs it, with both locks held.
    fn commit_local(
        &self,
        c: &mut Catalog,
        wal: &mut Wal,
        store: &CatalogStore,
        command: MetaCommand,
    ) -> Result<(), NativeError> {
        let index = wal.last_index() + 1;
        let term = c.current_term.max(1);
        let rec = WalRecord {
            term,
            index,
            command,
        };
        // A record that fails to apply must never reach the WAL, or every later replay fails on
        // it. `apply` leaves the catalog untouched when it rejects a command, so it runs in place;
        // readers wait on the write lock, so the new state is only visible once the WAL is durable.
        #[cfg(debug_assertions)]
        let before = serde_json::to_value(&*c)?;
        if let Err(e) = c.apply(term, index, &rec.command) {
            #[cfg(debug_assertions)]
            assert_eq!(
                serde_json::to_value(&*c)?,
                before,
                "a rejected command changed the catalog"
            );
            return Err(e.into());
        }
        if let Err(e) = wal.append(&rec) {
            // Whether the record reached disk is unknown, and neither reading the log back nor
            // syncing it again would tell: the page cache may hold what the disk does not. The
            // log stays failed, so nothing more commits until the engine is reopened, and the
            // catalog goes back to the records acknowledged before this one.
            self.note_log_failure(wal);
            if let Ok(acknowledged) = load_catalog(&self.cfg.root, store, wal, index) {
                *c = acknowledged;
            }
            return Err(e.into());
        }
        if self.cfg.wal_compact_after > 0 && wal.len() >= self.cfg.wal_compact_after {
            // Once the store durably covers `index`, every record up to it is redundant.
            store.checkpoint(c)?;
            let compacted = wal.compact_through(index);
            self.note_log_failure(wal);
            compacted?;
        }
        Ok(())
    }

    /// The whole extent, verified. If it fails because the extent moved since `ext` was looked
    /// up (tiered or repaired, its old ranges reused), it is read again where it is now.
    fn read_extent(&self, ext: &ExtentRef) -> Result<Vec<u8>, NativeError> {
        if self.cfg.objects.is_some() {
            if let Ok(mut r) = self.reads.lock() {
                r.insert(ext.id.clone(), now_ms());
            }
        }
        let payload = match self.read_placed(ext) {
            Err(NativeError::Checksum(id)) => {
                let now = self.with_catalog(|c| c.extents.get(&id).map(|e| e.extent.clone()))?;
                match now {
                    Some(now) if now != *ext => self.read_placed(&now),
                    _ => Err(NativeError::Checksum(id)),
                }
            }
            r => r,
        }?;
        if ext.raw_len > 0 {
            return crate::compress::decompress(&payload, ext.raw_len);
        }
        Ok(payload)
    }

    /// A tiered extent from its object.
    fn read_object(&self, ext: &ExtentRef, key: &str) -> Result<Vec<u8>, NativeError> {
        let store = self.cfg.objects.as_ref().ok_or_else(|| {
            NativeError::Invalid(format!(
                "extent {} is tiered but no object store is configured",
                ext.id
            ))
        })?;
        let buf = store.get(key)?;
        if buf.len() != ext.payload_len || !checksum::verify(&buf, &ext.checksum) {
            self.telemetry.checksum_failure();
            return Err(NativeError::Checksum(ext.id.clone()));
        }
        self.telemetry.object_read();
        self.telemetry.record_read(buf.len());
        Ok(buf)
    }

    /// The whole extent from wherever `ext` says it is: its object, its shards, or the first
    /// replica whose checksum verifies. Replica reads start at a replica chosen from the extent
    /// id, so the read load of many extents spreads over all their replicas.
    fn read_placed(&self, ext: &ExtentRef) -> Result<Vec<u8>, NativeError> {
        if let Some(key) = &ext.object {
            return self.read_object(ext, key);
        }
        if let Some(ec) = &ext.ec {
            return self.read_shards(ext, ec);
        }
        let start = replica_start(&ext.id, ext.replicas.len());
        let mut order: Vec<(&ReplicaRef, &NodeRuntime)> = ext
            .replicas
            .iter()
            .cycle()
            .skip(start)
            .take(ext.replicas.len())
            .filter_map(|r| {
                self.nodes
                    .iter()
                    .find(|n| n.spec.id == r.node_id && n.spec.healthy)
                    .map(|n| (r, n))
            })
            .collect();
        // Backed-off nodes are tried last rather than skipped: they may be the only copy left.
        order.sort_by_key(|(_, n)| !self.is_up(n));
        let mut failed = false;
        for (r, node) in order {
            let Some(device) = node.devices.get(r.device_index) else {
                continue;
            };
            match device.read_exact_at(r.offset, ext.payload_len) {
                Ok(buf) if checksum::verify(&buf, &ext.checksum) => {
                    self.mark_up(node);
                    if failed {
                        self.telemetry.replica_fallback();
                    }
                    self.telemetry.record_read(buf.len());
                    return Ok(buf);
                }
                Ok(_) => self.telemetry.checksum_failure(),
                Err(_) => self.mark_down(node),
            }
            failed = true;
        }
        Err(NativeError::Checksum(ext.id.clone()))
    }

    /// An erasure-coded extent from `data` of its shards, read in parallel: data shards on
    /// nodes that are up first (no decoding needed), then parity, then nodes backed off. Each
    /// shard that fails is replaced by the next in that order.
    fn read_shards(&self, ext: &ExtentRef, ec: &EcLayout) -> Result<Vec<u8>, NativeError> {
        let up = |i: usize| {
            self.nodes
                .iter()
                .any(|n| n.spec.id == ext.replicas[i].node_id && self.is_up(n))
        };
        let mut order: Vec<usize> = (0..ec.shards()).collect();
        order.sort_by_key(|i| (!up(*i), *i >= ec.data));
        let mut order = order.into_iter();
        let mut shards: Vec<Option<Vec<u8>>> = vec![None; ec.shards()];
        let mut have = 0;
        let mut failed = false;
        while have < ec.data {
            let batch: Vec<usize> = order.by_ref().take(ec.data - have).collect();
            if batch.is_empty() {
                return Err(NativeError::Checksum(ext.id.clone()));
            }
            let got: Vec<Option<Vec<u8>>> = std::thread::scope(|s| {
                let handles: Vec<_> = batch
                    .iter()
                    .map(|i| s.spawn(move || self.read_shard(ext, ec, *i)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().ok().flatten())
                    .collect()
            });
            for (i, shard) in batch.into_iter().zip(got) {
                match shard {
                    Some(b) => {
                        shards[i] = Some(b);
                        have += 1;
                    }
                    None => failed = true,
                }
            }
        }
        let data = ec.rebuild_data(shards)?;
        let out = ec.join(&data, ext.payload_len);
        if !checksum::verify(&out, &ext.checksum) {
            self.telemetry.checksum_failure();
            return Err(NativeError::Checksum(ext.id.clone()));
        }
        if failed {
            self.telemetry.replica_fallback();
        }
        self.telemetry.record_read(out.len());
        Ok(out)
    }

    /// Shard `i` of an erasure-coded extent, if its node is configured and healthy and the shard
    /// verifies.
    fn read_shard(&self, ext: &ExtentRef, ec: &EcLayout, i: usize) -> Option<Vec<u8>> {
        let r = ext.replicas.get(i)?;
        let node = self
            .nodes
            .iter()
            .find(|n| n.spec.id == r.node_id && n.spec.healthy)?;
        let device = node.devices.get(r.device_index)?;
        match device.read_exact_at(r.offset, ec.shard_len) {
            Ok(buf) if ec.verify(i, &buf) => {
                self.mark_up(node);
                Some(buf)
            }
            Ok(_) => {
                self.telemetry.checksum_failure();
                None
            }
            Err(_) => {
                self.mark_down(node);
                None
            }
        }
    }
}

/// The checkpointed catalog plus every WAL record past it.
fn load_local(root: &Path, store: &CatalogStore) -> Result<(Catalog, Wal), NativeError> {
    fs::create_dir_all(root)?;
    let mut wal = Wal::open(root.join("wal"))?;
    let catalog = load_catalog(root, store, &wal, u64::MAX)?;
    wal.raise_floor(catalog.applied_index);
    Ok((catalog, wal))
}

/// The checkpointed catalog plus the WAL records past it with an index below `before`.
fn load_catalog(
    root: &Path,
    store: &CatalogStore,
    wal: &Wal,
    before: u64,
) -> Result<Catalog, NativeError> {
    let mut catalog = load_checkpoint(root, store)?;
    for rec in wal.replay::<MetaCommand>()? {
        if rec.index > catalog.applied_index && rec.index < before {
            catalog.apply(rec.term, rec.index, &rec.command)?;
        }
    }
    Ok(catalog)
}

/// Whether the catalog rejected a commit because an extent it was told to share was freed.
fn is_stale_share(e: &NativeError) -> bool {
    e.to_string().contains("shared extent")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
