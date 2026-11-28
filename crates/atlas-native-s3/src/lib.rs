// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! S3 front end for atlas-native filesystems (`docs/NATIVE_S3.md`): each bucket is a filesystem
//! (or a snapshot of one), each object a file, served through the same node API the FUSE client
//! uses, so S3, NFS and FUSE clients share one namespace.

pub mod checksum;
pub mod config;
pub mod service;
pub mod store;
pub mod tagging;
