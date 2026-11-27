// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! A Linux block device (ublk) for atlas-native block volumes (binary `atlas-native-ublk`).
//! [`volume::Volume`] turns block requests into calls on the cluster's volume API and holds the
//! rules about durability, retries and failover; [`dispatch::Dispatcher`] runs requests
//! concurrently; [`params`] describes the device and encodes results for the kernel. All of that
//! is tested without a kernel. The `ublk` feature adds the Linux server. See
//! `docs/NATIVE_UBLK.md`.

pub mod dispatch;
pub mod params;
pub mod split;
#[cfg(all(feature = "ublk", target_os = "linux"))]
pub mod ublk;
pub mod volume;

/// A positive errno value.
pub type Errno = i32;
