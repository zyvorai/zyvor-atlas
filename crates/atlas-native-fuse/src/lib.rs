// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! FUSE client for atlas-native filesystems (binary `atlas-native-mount`). [`ops::Ops`] holds
//! the logic (caching, write-back, leader retry) and is tested without a kernel; the `fuse`
//! feature adds the Linux kernel adapter. [`nfs`] configures the NFS gateway
//! (`atlas-native-nfs`) that re-exports mounts through NFS-Ganesha, [`smb`] the SMB gateway
//! (`atlas-native-smb`) that shares them through Samba. [`replicate`] sends a filesystem's
//! snapshots to a replica on another cluster (`atlas-native-replicate`).

pub mod cache;
pub mod client;
pub mod direct_write;
pub mod dirs;
#[cfg(all(feature = "fuse", target_os = "linux"))]
pub mod fuse;
pub mod locks;
pub mod nfs;
pub mod ops;
pub mod pipeline;
pub mod replicate;
pub mod smb;
#[cfg(unix)]
pub mod supervise;
pub mod writeback;
