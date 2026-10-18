// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! In-memory stand-in for a healthy three-node native cluster.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use atlas_driver_core::DriverError;

use crate::{DataNodeInfo, Layout, NativeApi, NativeFs, NativeVolume, NodeStatus, RaftInfo};

#[derive(Default)]
pub struct FakeApi {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next: u64,
    volumes: BTreeMap<String, NativeVolume>,
    /// Snapshot id → the volume as it was (size, extents).
    snapshots: BTreeMap<String, (u64, u64)>,
    /// Written bytes per volume, up to the last written byte.
    data: BTreeMap<String, Vec<u8>>,
    filesystems: BTreeMap<String, NativeFs>,
    /// Filesystem snapshot id → the filesystem as it was.
    fs_snapshots: BTreeMap<String, NativeFs>,
    /// Snapshot id (block or filesystem) → Unix second its retention lock expires at.
    locks: BTreeMap<String, u64>,
}

fn lock(m: &Mutex<State>) -> Result<std::sync::MutexGuard<'_, State>, DriverError> {
    m.lock()
        .map_err(|_| DriverError::Backend("fake native state poisoned".into()))
}

#[async_trait]
impl NativeApi for FakeApi {
    async fn status(&self) -> Result<NodeStatus, DriverError> {
        Ok(NodeStatus {
            node_id: "fake-0".into(),
            metadata: Some(RaftInfo {
                role: "leader".into(),
                term: 1,
                leader: Some("fake-0".into()),
                commit_index: lock(&self.state)?.next,
            }),
            metadata_groups: None,
            layout: Some(Layout {
                extent_bytes: 4 << 20,
                replicas: 3,
            }),
            data_nodes: Some(
                (0..3)
                    .map(|i| DataNodeInfo {
                        id: format!("fake-{i}"),
                        up: true,
                    })
                    .collect(),
            ),
        })
    }

    async fn volumes(&self) -> Result<Vec<NativeVolume>, DriverError> {
        Ok(lock(&self.state)?.volumes.values().cloned().collect())
    }

    async fn create_volume(
        &self,
        id: &str,
        name: &str,
        size_bytes: u64,
    ) -> Result<String, DriverError> {
        let mut s = lock(&self.state)?;
        if s.volumes.contains_key(id) {
            return Ok(id.to_string());
        }
        if s.volumes.values().any(|v| v.name == name) {
            return Err(DriverError::Backend(format!(
                "volume {name} already exists"
            )));
        }
        s.next += 1;
        let id = id.to_string();
        s.volumes.insert(
            id.clone(),
            NativeVolume {
                id: id.clone(),
                name: name.into(),
                size_bytes,
                extents: 0,
            },
        );
        Ok(id)
    }

    async fn delete_volume(&self, id: &str) -> Result<(), DriverError> {
        let mut s = lock(&self.state)?;
        s.data.remove(id);
        s.volumes
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| DriverError::Backend(format!("not found: volume {id}")))
    }

    async fn resize_volume(&self, id: &str, size_bytes: u64) -> Result<(), DriverError> {
        let mut s = lock(&self.state)?;
        let v = s
            .volumes
            .get_mut(id)
            .ok_or_else(|| DriverError::Backend(format!("not found: volume {id}")))?;
        if size_bytes < v.size_bytes {
            return Err(DriverError::Backend(format!("volume {id} cannot shrink")));
        }
        v.size_bytes = size_bytes;
        Ok(())
    }

    async fn create_snapshot(
        &self,
        id: &str,
        volume_id: &str,
        _name: &str,
    ) -> Result<String, DriverError> {
        let mut s = lock(&self.state)?;
        let v = s
            .volumes
            .get(volume_id)
            .map(|v| (v.size_bytes, v.extents))
            .ok_or_else(|| DriverError::Backend(format!("not found: volume {volume_id}")))?;
        s.next += 1;
        s.snapshots.insert(id.to_string(), v);
        Ok(id.to_string())
    }

    async fn clone_snapshot(
        &self,
        id: &str,
        snapshot_id: &str,
        name: &str,
        size_bytes: Option<u64>,
    ) -> Result<String, DriverError> {
        let (size, extents) = *lock(&self.state)?
            .snapshots
            .get(snapshot_id)
            .ok_or_else(|| DriverError::Backend(format!("not found: snapshot {snapshot_id}")))?;
        let size_bytes = size_bytes.unwrap_or(size);
        if size_bytes < size {
            return Err(DriverError::Backend(format!(
                "clone of {snapshot_id} needs at least {size} bytes"
            )));
        }
        let id = self.create_volume(id, name, size_bytes).await?;
        if let Some(v) = lock(&self.state)?.volumes.get_mut(&id) {
            v.extents = extents;
        }
        Ok(id)
    }

    async fn read(&self, volume_id: &str, offset: u64, len: u64) -> Result<Vec<u8>, DriverError> {
        let s = lock(&self.state)?;
        let size = s
            .volumes
            .get(volume_id)
            .ok_or_else(|| DriverError::Backend(format!("not found: volume {volume_id}")))?
            .size_bytes;
        if offset.saturating_add(len) > size {
            return Err(DriverError::Backend(
                "invalid: read exceeds volume size".into(),
            ));
        }
        let mut out = vec![0u8; len as usize];
        if let Some(d) = s.data.get(volume_id) {
            let start = (offset as usize).min(d.len());
            let end = ((offset + len) as usize).min(d.len());
            out[..end - start].copy_from_slice(&d[start..end]);
        }
        Ok(out)
    }

    async fn write(&self, volume_id: &str, offset: u64, data: Vec<u8>) -> Result<(), DriverError> {
        let mut s = lock(&self.state)?;
        let size = s
            .volumes
            .get(volume_id)
            .ok_or_else(|| DriverError::Backend(format!("not found: volume {volume_id}")))?
            .size_bytes;
        let end = offset.saturating_add(data.len() as u64);
        if end > size {
            return Err(DriverError::Backend(
                "invalid: write exceeds volume size".into(),
            ));
        }
        let d = s.data.entry(volume_id.to_string()).or_default();
        if d.len() < end as usize {
            d.resize(end as usize, 0);
        }
        d[offset as usize..end as usize].copy_from_slice(&data);
        Ok(())
    }

    async fn delete_snapshot(&self, id: &str) -> Result<(), DriverError> {
        let mut s = lock(&self.state)?;
        refuse_if_locked(&s, id)?;
        if s.snapshots.remove(id).is_some() {
            Ok(())
        } else {
            Err(DriverError::Backend(format!("not found: snapshot {id}")))
        }
    }

    async fn filesystems(&self) -> Result<Vec<NativeFs>, DriverError> {
        Ok(lock(&self.state)?.filesystems.values().cloned().collect())
    }

    async fn create_fs(&self, id: &str, name: &str) -> Result<String, DriverError> {
        let mut s = lock(&self.state)?;
        if let Some(f) = s.filesystems.get(id) {
            return if f.name == name {
                Ok(id.to_string())
            } else {
                Err(DriverError::Backend(format!(
                    "filesystem {id} already exists"
                )))
            };
        }
        s.next += 1;
        s.filesystems.insert(
            id.to_string(),
            NativeFs {
                id: id.to_string(),
                name: name.into(),
                bytes: 0,
                inodes: 1,
                source_snapshot: None,
            },
        );
        Ok(id.to_string())
    }

    async fn delete_fs(&self, id: &str) -> Result<(), DriverError> {
        lock(&self.state)?
            .filesystems
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| DriverError::Backend(format!("not found: filesystem {id}")))
    }

    async fn create_fs_snapshot(
        &self,
        id: &str,
        fs_id: &str,
        _name: &str,
    ) -> Result<String, DriverError> {
        let mut s = lock(&self.state)?;
        let f = s
            .filesystems
            .get(fs_id)
            .cloned()
            .ok_or_else(|| DriverError::Backend(format!("not found: filesystem {fs_id}")))?;
        s.next += 1;
        s.fs_snapshots.insert(id.to_string(), f);
        Ok(id.to_string())
    }

    async fn clone_fs_snapshot(
        &self,
        id: &str,
        snapshot_id: &str,
        name: &str,
    ) -> Result<String, DriverError> {
        let mut s = lock(&self.state)?;
        let mut f = s.fs_snapshots.get(snapshot_id).cloned().ok_or_else(|| {
            DriverError::Backend(format!("not found: filesystem snapshot {snapshot_id}"))
        })?;
        if s.filesystems.contains_key(id) {
            return Ok(id.to_string());
        }
        s.next += 1;
        f.id = id.to_string();
        f.name = name.into();
        f.source_snapshot = Some(snapshot_id.to_string());
        s.filesystems.insert(id.to_string(), f);
        Ok(id.to_string())
    }

    async fn delete_fs_snapshot(&self, id: &str) -> Result<(), DriverError> {
        let mut s = lock(&self.state)?;
        refuse_if_locked(&s, id)?;
        s.fs_snapshots
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| DriverError::Backend(format!("not found: filesystem snapshot {id}")))
    }

    /// Fixture: quiet history, except a filesystem named `ransomware-drill`, whose last window
    /// looks like a mass encrypt-and-rename so the detect-and-protect path can be exercised.
    async fn fs_activity(&self, fs_id: &str) -> Result<Vec<serde_json::Value>, DriverError> {
        let s = lock(&self.state)?;
        let Some(fs) = s.filesystems.get(fs_id) else {
            return Err(DriverError::Backend(format!(
                "not found: filesystem {fs_id}"
            )));
        };
        let w = |m: f64, r: f64, d: f64, e: f64| {
            serde_json::json!({
                "bytes_written": m * 4096.0, "files_modified": m, "files_renamed": r,
                "files_deleted": d, "high_entropy_ratio": e
            })
        };
        let mut v: Vec<_> = (0..10)
            .map(|i| w(20.0 + f64::from(i % 3), 1.0, 1.0, 0.05))
            .collect();
        v.push(if fs.name == "ransomware-drill" {
            w(4000.0, 3900.0, 10.0, 0.97)
        } else {
            w(21.0, 1.0, 1.0, 0.05)
        });
        Ok(v)
    }

    async fn lock_snapshot(&self, id: &str, retain_secs: u64) -> Result<u64, DriverError> {
        let mut s = lock(&self.state)?;
        if !s.snapshots.contains_key(id) {
            return Err(DriverError::Backend(format!("not found: snapshot {id}")));
        }
        extend_lock(&mut s, id, retain_secs)
    }

    async fn lock_fs_snapshot(&self, id: &str, retain_secs: u64) -> Result<u64, DriverError> {
        let mut s = lock(&self.state)?;
        if !s.fs_snapshots.contains_key(id) {
            return Err(DriverError::Backend(format!(
                "not found: filesystem snapshot {id}"
            )));
        }
        extend_lock(&mut s, id, retain_secs)
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn refuse_if_locked(s: &State, id: &str) -> Result<(), DriverError> {
    match s.locks.get(id) {
        Some(until) if *until > unix_now() => Err(DriverError::Backend(format!(
            "snapshot {id} is retention-locked until {until}"
        ))),
        _ => Ok(()),
    }
}

fn extend_lock(s: &mut State, id: &str, retain_secs: u64) -> Result<u64, DriverError> {
    let until = unix_now().saturating_add(retain_secs);
    if s.locks.get(id).is_some_and(|cur| until < *cur) {
        return Err(DriverError::Backend(format!(
            "snapshot {id} is locked longer already; a lock cannot be shortened"
        )));
    }
    s.locks.insert(id.to_string(), until);
    Ok(until)
}
