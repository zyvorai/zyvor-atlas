// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use std::{
    fs::{File, OpenOptions},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::engine::NativeError;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

/// Byte-addressed extent storage the engine writes replicas to: a local [`FileDevice`] or a
/// [`crate::data_node::RemoteDevice`].
///
/// `fence` is the writer's Raft term. Remote data nodes reject writes whose fence is below the
/// highest they have accepted; local files ignore it.
pub trait BlockStore: std::fmt::Debug + Send + Sync {
    /// Appends `data` at the end of the device and returns its offset.
    fn append(&self, fence: u64, data: &[u8]) -> Result<u64, NativeError>;
    /// Overwrites `data.len()` bytes at `offset`. Never extends the device.
    fn write_at(&self, fence: u64, offset: u64, data: &[u8]) -> Result<(), NativeError>;
    fn read_exact_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, NativeError>;
    fn len(&self) -> Result<u64, NativeError>;
    fn is_empty(&self) -> Result<bool, NativeError> {
        Ok(self.len()? == 0)
    }
    /// Whether a write takes exactly the space it asks for. Block-aligned devices round it up,
    /// so they hold no deltas.
    fn byte_granular(&self) -> Result<bool, NativeError> {
        Ok(false)
    }
    /// Network address a client can read this device at directly, if it is remote.
    fn endpoint(&self) -> Option<&str> {
        None
    }
}

impl BlockStore for FileDevice {
    fn byte_granular(&self) -> Result<bool, NativeError> {
        Ok(true)
    }
    fn append(&self, _fence: u64, data: &[u8]) -> Result<u64, NativeError> {
        FileDevice::append(self, data)
    }
    fn write_at(&self, _fence: u64, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        FileDevice::write_at(self, offset, data)
    }
    fn read_exact_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, NativeError> {
        FileDevice::read_exact_at(self, offset, len)
    }
    fn len(&self) -> Result<u64, NativeError> {
        FileDevice::len(self)
    }
}

/// Positional I/O on one file. An append only reserves its offset under a lock, then writes and
/// syncs like an in-place write, so concurrent appends (and their syncs) overlap.
#[derive(Debug)]
pub struct FileDevice {
    id: DeviceId,
    path: PathBuf,
    /// End of the space handed out to appends, including ones still being written.
    end: Mutex<u64>,
    positional: File,
}

impl FileDevice {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, NativeError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        let end = file.metadata()?.len();
        Ok(Self {
            id: DeviceId(Uuid::new_v4().to_string()),
            path,
            end: Mutex::new(end),
            positional: file,
        })
    }

    pub fn id(&self) -> &DeviceId {
        &self.id
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, data: &[u8]) -> Result<u64, NativeError> {
        let off = {
            let mut end = self
                .end
                .lock()
                .map_err(|_| NativeError::Poisoned("device"))?;
            let off = *end;
            *end += data.len() as u64;
            off
        };
        let written = self
            .positional
            .write_all_at(data, off)
            .and_then(|()| self.positional.sync_data());
        if let Err(e) = written {
            // Give the space back unless a later append already took the space after it; an
            // unacknowledged append is referenced by nothing either way.
            if let Ok(mut end) = self.end.lock() {
                if *end == off + data.len() as u64 {
                    *end = off;
                }
            }
            return Err(e.into());
        }
        Ok(off)
    }

    /// Overwrites `data.len()` bytes at `offset` (reuse of a freed range). Never extends the file.
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        let end = self.len()?;
        if offset + data.len() as u64 > end {
            return Err(NativeError::Invalid(format!(
                "write_at {offset}+{} past device end {end}",
                data.len()
            )));
        }
        self.positional.write_all_at(data, offset)?;
        self.positional.sync_data()?;
        Ok(())
    }

    pub fn len(&self) -> Result<u64, NativeError> {
        Ok(self.positional.metadata()?.len())
    }

    pub fn is_empty(&self) -> Result<bool, NativeError> {
        Ok(self.len()? == 0)
    }

    pub fn read_exact_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, NativeError> {
        let mut buf = vec![0u8; len];
        self.positional.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_appends_get_disjoint_space() {
        let td = tempfile::tempdir().unwrap();
        let d = FileDevice::open(td.path().join("dev")).unwrap();
        d.append(b"existing").unwrap();
        let offsets: Vec<(u8, u64)> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8u8)
                .map(|i| {
                    let d = &d;
                    s.spawn(move || (i, d.append(&vec![i; 1000 + i as usize]).unwrap()))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (i, off) in &offsets {
            assert!(*off >= 8);
            let got = d.read_exact_at(*off, 1000 + *i as usize).unwrap();
            assert!(got.iter().all(|b| b == i), "append {i} was overwritten");
        }
        let total: u64 = (0..8).map(|i| 1000 + i).sum::<u64>() + 8;
        assert_eq!(d.len().unwrap(), total);
        // A reopened device appends after everything.
        drop(d);
        let d = FileDevice::open(td.path().join("dev")).unwrap();
        assert_eq!(d.append(b"x").unwrap(), total);
    }
}
