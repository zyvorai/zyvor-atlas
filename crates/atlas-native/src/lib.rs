// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Atlas Native distributed-storage core.
//!
//! Phase 2 added a durable metadata command log, replayable state machine, extent reference
//! counting and safe metadata reclamation. Phase 3 adds WAL checkpoint/compaction, device-space
//! free lists so reclaimed extents are physically reused, and a sans-IO Raft core ([`raft`]) that
//! replicates the same `MetaCommand` log across metadata replicas and applies it through the same
//! `Catalog` state machine. Phase 4 puts replicas on other hosts through [`data_node`] and lets
//! [`NativeEngine`] commit through Raft ([`MetaBackend::Raft`]); both transports support mutual TLS
//! ([`tls`]). [`node::NativeNode`] (binary `atlas-native-node`) runs either or both roles with an
//! HTTP ops/volume endpoint.

pub mod acl;
pub mod activity;
pub mod alloc;
pub mod checksum;
pub mod compress;
pub mod data_node;
pub mod device;
mod durable;
pub mod ec;
pub mod engine;
pub mod gc;
pub mod http;
pub mod inodes;
pub mod leases;
pub mod membership;
pub mod metadata;
pub mod metrics;
pub mod namespace;
pub mod object;
pub mod node;
pub mod placement;
pub mod raft;
pub mod raft_server;
pub mod raft_snapshot;
pub mod raw;
pub mod rebuild;
pub mod store;
pub mod telemetry;
pub mod tls;
pub mod tracked;
#[cfg(all(target_os = "linux", feature = "io-uring"))]
pub mod uring;
#[cfg(all(target_os = "linux", feature = "rdma"))]
pub mod rdma;
pub mod wal;

pub use alloc::{FreeList, FreeRange};
pub use data_node::{DataNodeServer, RemoteDevice};
pub use device::{BlockStore, DeviceId, FileDevice};
pub use engine::{
    EngineConfig, FileLayout, LayoutExtent, LayoutReplica, MetaBackend, NativeEngine, NativeError,
    ExportInfo, ExportManifest, ExportStats, HostLocality, Locality, NodeLocality, NodeStatus,
    ObjectKind, PinReport, RepairStats, TierPolicy, TierStats, VolumeInfo,
};
pub use gc::GcStats;
pub use membership::Membership;
pub use metadata::{Catalog, MetaCommand, SnapshotId, VolumeId};
pub use namespace::{FsId, FsOp, FsQuota, Inode, InodeKind, NodeType, SetAttr, XattrMode, ROOT_INO};
pub use placement::{FailureDomain, Node, PlacementPolicy};
pub use raft::{Envelope, Message, RaftConfig, RaftCounters, RaftError, RaftNode, Role};
pub use raft_server::{RaftMux, RaftServer, RaftStatus};
pub use raw::{open_store, DeviceBackend};
pub use tls::TlsIdentity;
