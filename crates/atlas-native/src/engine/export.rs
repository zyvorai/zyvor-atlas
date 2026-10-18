// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Snapshot export to object storage and import back into a volume.
//!
//! An export is a manifest (`<export_prefix>exports/<name>/manifest.json`, written last, so an
//! export without one is incomplete) listing the snapshot's extents by offset, length and
//! SHA-256, plus each extent's bytes as a blob named by its checksum
//! (`<export_prefix>blobs/<sha256>`). Blobs are shared: exporting a later snapshot of the same
//! volume uploads only the extents that changed. Exports don't depend on the cluster that wrote
//! them, so any cluster with the bucket can import one.
//!
//! An export in progress keeps a marker (`<export_prefix>pending/<name>`) fresh, and deleting
//! an export frees no blob while a fresh marker exists: until its manifest is written, an
//! export's blobs — including ones it found already uploaded and is reusing — are referenced
//! by nothing else. The exporter also re-checks its reused blobs after writing the manifest.

use std::{collections::BTreeSet, io};

use serde::{Deserialize, Serialize};

use super::{now_ms, NativeEngine, NativeError};
use crate::{checksum, object::ObjectStore};

pub const EXPORT_FORMAT: u32 = 1;

/// An exporter refreshes its pending marker this often...
const PENDING_REFRESH_MS: u64 = 60_000;
/// ...and a marker older than this is left by an exporter that died.
const PENDING_STALE_MS: u64 = 10 * 60_000;

#[derive(Serialize, Deserialize)]
struct Pending {
    updated_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportManifest {
    pub format: u32,
    pub name: String,
    pub snapshot_id: String,
    pub snapshot_name: String,
    pub size_bytes: u64,
    pub created_ms: u64,
    pub extents: Vec<ExportExtent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportExtent {
    pub logical_offset: u64,
    pub len: usize,
    /// Hex SHA-256; the blob's name.
    pub sha256: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ExportStats {
    pub extents: u64,
    /// Blobs uploaded by this export.
    pub uploaded: u64,
    /// Extents whose blob was already in the bucket (from an earlier export).
    pub reused: u64,
    pub bytes_uploaded: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportInfo {
    pub name: String,
    pub snapshot_id: String,
    pub snapshot_name: String,
    pub size_bytes: u64,
    pub extents: usize,
    pub created_ms: u64,
}

/// Export names: 1–128 of `[A-Za-z0-9._-]`, not starting with `.`.
pub fn check_export_name(name: &str) -> Result<(), NativeError> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(NativeError::Invalid(format!(
            "export name {name:?} must be 1-128 of A-Z a-z 0-9 . _ - and not start with ."
        )))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl NativeEngine {
    fn store(&self) -> Result<&dyn ObjectStore, NativeError> {
        self.cfg
            .objects
            .as_deref()
            .ok_or_else(|| NativeError::Invalid("no object store is configured".into()))
    }

    fn manifest_key(&self, name: &str) -> String {
        format!("{}exports/{name}/manifest.json", self.cfg.export_prefix)
    }

    fn blob_key(&self, sha256: &str) -> String {
        format!("{}blobs/{sha256}", self.cfg.export_prefix)
    }

    fn pending_key(&self, name: &str) -> String {
        format!("{}pending/{name}", self.cfg.export_prefix)
    }

    fn mark_pending(&self, store: &dyn ObjectStore, name: &str) -> io::Result<()> {
        let body = serde_json::to_vec(&Pending {
            updated_ms: now_ms(),
        })?;
        store.put(&self.pending_key(name), &body)
    }

    /// Names of exports in progress (markers refreshed within [`PENDING_STALE_MS`]).
    fn pending_exports(&self, store: &dyn ObjectStore) -> Result<Vec<String>, NativeError> {
        let prefix = self.pending_key("");
        let now = now_ms();
        let mut live = Vec::new();
        for key in store.list(&prefix)? {
            let updated = match store.get(&key) {
                Ok(b) => serde_json::from_slice::<Pending>(&b).map_or(0, |p| p.updated_ms),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if now.saturating_sub(updated) < PENDING_STALE_MS {
                live.push(key[prefix.len()..].to_string());
            }
        }
        Ok(live)
    }

    /// Writes snapshot `snapshot_id` to object storage as export `name`. `progress(done, total)`
    /// is called after each extent; returning false abandons the export (no manifest is
    /// written, and blobs already uploaded are kept for a later export to reuse).
    pub fn export_snapshot(
        &self,
        snapshot_id: &str,
        name: &str,
        progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<ExportStats, NativeError> {
        check_export_name(name)?;
        let store = self.store()?;
        self.read_barrier()?;
        let (snapshot_name, size, mut extents) = self.with_catalog(|c| {
            let s = c
                .snapshots
                .get(snapshot_id)
                .ok_or_else(|| NativeError::NotFound(snapshot_id.into()))?;
            let size = if s.size_bytes > 0 {
                s.size_bytes
            } else {
                c.written_end(&s.extents)
            };
            let extents = s
                .extents
                .values()
                .map(|id| {
                    c.extents
                        .get(id)
                        .map(|m| m.extent.clone())
                        .ok_or_else(|| NativeError::NotFound(id.clone()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, NativeError>((s.name.clone(), size, extents))
        })??;
        extents.sort_by_key(|e| e.logical_offset);
        let key = self.manifest_key(name);
        let exists = match store.get(&key) {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.pending_exports(store)?.iter().any(|p| p == name)
            }
            Err(e) => return Err(e.into()),
        };
        if exists {
            return Err(crate::metadata::MetaError::Exists(format!("export {name}")).into());
        }
        self.mark_pending(store, name)?;
        let r = self.upload_export(
            store,
            name,
            snapshot_id,
            snapshot_name,
            size,
            &extents,
            progress,
        );
        // A failed delete leaves a marker that goes stale on its own.
        let _ = store.delete(&self.pending_key(name));
        r
    }

    #[allow(clippy::too_many_arguments)]
    fn upload_export(
        &self,
        store: &dyn ObjectStore,
        name: &str,
        snapshot_id: &str,
        snapshot_name: String,
        size: u64,
        extents: &[super::ExtentRef],
        mut progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<ExportStats, NativeError> {
        let key = self.manifest_key(name);
        let mut refreshed = now_ms();
        let mut have: BTreeSet<String> = store.list(&self.blob_key(""))?.into_iter().collect();
        let total = extents.len() as u64;
        let mut st = ExportStats::default();
        let mut listed = Vec::with_capacity(extents.len());
        let mut reused = Vec::new();
        for ext in extents {
            if now_ms().saturating_sub(refreshed) >= PENDING_REFRESH_MS {
                self.mark_pending(store, name)?;
                refreshed = now_ms();
            }
            let sha = hex(&ext.content_sha());
            let blob = self.blob_key(&sha);
            if have.contains(&blob) {
                st.reused += 1;
                reused.push((ext, blob));
            } else {
                let data = self.read_extent(ext)?;
                store.put(&blob, &data)?;
                have.insert(blob);
                st.uploaded += 1;
                st.bytes_uploaded += data.len() as u64;
            }
            st.extents += 1;
            listed.push(ExportExtent {
                logical_offset: ext.logical_offset,
                len: ext.len(),
                sha256: sha,
            });
            if !progress(st.extents, total) {
                return Err(NativeError::Invalid(format!("export {name} was stopped")));
            }
        }
        let manifest = ExportManifest {
            format: EXPORT_FORMAT,
            name: name.to_string(),
            snapshot_id: snapshot_id.to_string(),
            snapshot_name,
            size_bytes: size,
            created_ms: now_ms(),
            extents: listed,
        };
        store.put(&key, &serde_json::to_vec_pretty(&manifest)?)?;
        // A delete that started before our marker may have freed a blob we found and reused.
        let have: BTreeSet<String> = store.list(&self.blob_key(""))?.into_iter().collect();
        for (ext, blob) in reused {
            if !have.contains(&blob) {
                let data = self.read_extent(ext)?;
                store.put(&blob, &data)?;
                st.uploaded += 1;
                st.reused -= 1;
                st.bytes_uploaded += data.len() as u64;
            }
        }
        Ok(st)
    }

    /// The manifest of export `name`.
    pub fn read_export(&self, name: &str) -> Result<ExportManifest, NativeError> {
        check_export_name(name)?;
        let bytes = match self.store()?.get(&self.manifest_key(name)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(NativeError::NotFound(format!("export {name}")))
            }
            r => r?,
        };
        let m: ExportManifest = serde_json::from_slice(&bytes)?;
        if m.format != EXPORT_FORMAT {
            return Err(NativeError::Invalid(format!(
                "export {name} has format {}, this node reads {EXPORT_FORMAT}",
                m.format
            )));
        }
        Ok(m)
    }

    /// Every complete export in the bucket, by name.
    pub fn list_exports(&self) -> Result<Vec<ExportInfo>, NativeError> {
        let prefix = format!("{}exports/", self.cfg.export_prefix);
        let mut names: Vec<String> = self
            .store()?
            .list(&prefix)?
            .into_iter()
            .filter_map(|k| {
                k.strip_prefix(&prefix)?
                    .strip_suffix("/manifest.json")
                    .map(str::to_string)
            })
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|n| {
                let m = self.read_export(&n)?;
                Ok(ExportInfo {
                    name: m.name,
                    snapshot_id: m.snapshot_id,
                    snapshot_name: m.snapshot_name,
                    size_bytes: m.size_bytes,
                    extents: m.extents.len(),
                    created_ms: m.created_ms,
                })
            })
            .collect()
    }

    /// Writes export `name` into volume `volume_id` (at least the export's size), verifying
    /// every blob. `progress` as for [`Self::export_snapshot`].
    pub fn import_export(
        &self,
        name: &str,
        volume_id: &str,
        mut progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<u64, NativeError> {
        let m = self.read_export(name)?;
        let store = self.store()?;
        let total = m.extents.len() as u64;
        let mut bytes = 0;
        for (i, ext) in m.extents.iter().enumerate() {
            let data = store.get(&self.blob_key(&ext.sha256))?;
            if data.len() != ext.len || hex(&checksum::sha256(&data)) != ext.sha256 {
                return Err(NativeError::Checksum(format!(
                    "export {name} blob {}",
                    ext.sha256
                )));
            }
            self.write(volume_id, ext.logical_offset, &data)?;
            bytes += data.len() as u64;
            if !progress(i as u64 + 1, total) {
                return Err(NativeError::Invalid(format!(
                    "import of {name} was stopped"
                )));
            }
        }
        Ok(bytes)
    }

    /// Deletes export `name`, then every blob no remaining export references — stopping early
    /// (the rest are freed by a later delete) once an export is in progress. Returns the number
    /// of blobs deleted.
    pub fn delete_export(&self, name: &str) -> Result<u64, NativeError> {
        self.read_export(name)?;
        let store = self.store()?;
        store.delete(&self.manifest_key(name))?;
        let mut live = BTreeSet::new();
        for e in self.list_exports()? {
            for x in self.read_export(&e.name)?.extents {
                live.insert(self.blob_key(&x.sha256));
            }
        }
        let mut deleted = 0;
        for blob in store.list(&self.blob_key(""))? {
            if !live.contains(&blob) {
                if !self.pending_exports(store)?.is_empty() {
                    break;
                }
                store.delete(&blob)?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }
}
