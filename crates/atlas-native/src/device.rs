// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    durable,
    engine::NativeError,
    metrics::{Histogram, HistogramSnapshot},
    sync_guard::SyncGuard,
};

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
    /// Count and latency of the device's data syncs, for a device that syncs with a call of its
    /// own. `None` otherwise: remote devices, and those that write through with `O_DSYNC`.
    fn sync_latency(&self) -> Option<HistogramSnapshot> {
        None
    }
    /// Network address a client can read this device at directly, if it is remote.
    fn endpoint(&self) -> Option<&str> {
        None
    }
    /// Why this device refuses writes until it is reopened, if a local device does: making
    /// data durable failed. `None` for a remote device, whose node knows.
    fn failure(&self) -> Option<String> {
        None
    }
    /// Asks the device why it refuses writes until reopened, if it does: for a remote device a
    /// request to its node. An error means the device could not be asked.
    fn probe(&self) -> Result<Option<String>, NativeError> {
        self.len()?;
        Ok(self.failure())
    }
}

/// Stands in for a device that could not be opened, so that its node still starts and serves its
/// other devices. It keeps the position (replicas record their device index), refuses every
/// access and reports why through [`BlockStore::failure`].
#[derive(Debug)]
pub(crate) struct FailedDevice {
    cause: String,
}

impl FailedDevice {
    pub fn new(path: &Path, error: &io::Error) -> Self {
        let cause = format!("{} could not be opened: {error}", path.display());
        eprintln!("atlas-native: device {cause}");
        Self { cause }
    }

    fn refuse<T>(&self) -> Result<T, NativeError> {
        Err(io::Error::other(format!("device {}", self.cause)).into())
    }
}

impl BlockStore for FailedDevice {
    fn append(&self, _fence: u64, _data: &[u8]) -> Result<u64, NativeError> {
        self.refuse()
    }
    fn write_at(&self, _fence: u64, _offset: u64, _data: &[u8]) -> Result<(), NativeError> {
        self.refuse()
    }
    fn read_exact_at(&self, _offset: u64, _len: usize) -> Result<Vec<u8>, NativeError> {
        self.refuse()
    }
    /// Nothing is known to be in use.
    fn len(&self) -> Result<u64, NativeError> {
        Ok(0)
    }
    fn failure(&self) -> Option<String> {
        Some(self.cause.clone())
    }
}

/// Makes the data written to a file durable. [`DataSync`] is what devices and the metadata log
/// use; tests substitute one that stalls or fails.
pub trait DeviceSync: std::fmt::Debug + Send + Sync {
    fn sync_data(&self, file: &File) -> io::Result<()>;
}

/// `fdatasync` ([`File::sync_data`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct DataSync;

impl DeviceSync for DataSync {
    fn sync_data(&self, file: &File) -> io::Result<()> {
        file.sync_data()
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
    fn sync_latency(&self) -> Option<HistogramSnapshot> {
        Some(self.syncs.snapshot())
    }
    fn failure(&self) -> Option<String> {
        self.guard.failure()
    }
}

/// Positional I/O on one file. An append only reserves its offset under a lock, then writes and
/// syncs like an in-place write, so concurrent appends (and their syncs) overlap.
///
/// A write returns only once its data is durable. After a failed sync the device refuses writes
/// until it is reopened, and a write is not acknowledged while a sync that could have been told
/// about its data is still running (see `sync_guard.rs`). Reads go on: ranges acknowledged
/// before the failure are unaffected, and nothing may refer to the others.
#[derive(Debug)]
pub struct FileDevice {
    id: DeviceId,
    path: PathBuf,
    /// End of the space handed out to appends, including ones still being written.
    end: Mutex<u64>,
    positional: File,
    /// The `sync_data` calls of appends and overwrites.
    syncs: Histogram,
    sync: Box<dyn DeviceSync>,
    guard: SyncGuard,
}

impl FileDevice {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, NativeError> {
        Self::open_with_sync(path, Box::new(DataSync))
    }

    /// [`Self::open`] with `sync` in place of `fdatasync`.
    #[doc(hidden)]
    pub fn open_with_sync(
        path: impl AsRef<Path>,
        sync: Box<dyn DeviceSync>,
    ) -> Result<Self, NativeError> {
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
        // After a failed sync, in this process or an earlier one, the page cache can hold data
        // the disk never got. Read from the disk.
        durable::drop_cached(&path);
        let end = file.metadata()?.len();
        Ok(Self {
            id: DeviceId(Uuid::new_v4().to_string()),
            guard: SyncGuard::new(format!("device {}", path.display())),
            path,
            end: Mutex::new(end),
            positional: file,
            syncs: Histogram::default(),
            sync,
        })
    }

    pub fn id(&self) -> &DeviceId {
        &self.id
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns once the data this thread has written is durable.
    fn make_durable(&self) -> Result<(), NativeError> {
        self.guard.run(|| self.sync_data())?;
        Ok(())
    }

    pub fn append(&self, data: &[u8]) -> Result<u64, NativeError> {
        self.guard.check()?;
        let off = {
            let mut end = self
                .end
                .lock()
                .map_err(|_| NativeError::Poisoned("device"))?;
            let off = *end;
            *end += data.len() as u64;
            off
        };
        if let Err(e) = self.positional.write_all_at(data, off) {
            // Give the space back unless a later append already took the space after it; an
            // unacknowledged append is referenced by nothing either way.
            if let Ok(mut end) = self.end.lock() {
                if *end == off + data.len() as u64 {
                    *end = off;
                }
            }
            return Err(e.into());
        }
        // After a failed sync the space stays handed out: what the disk holds there is unknown.
        self.make_durable()?;
        Ok(off)
    }

    /// Overwrites `data.len()` bytes at `offset` (reuse of a freed range). Never extends the file.
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        self.guard.check()?;
        let end = self.len()?;
        if offset + data.len() as u64 > end {
            return Err(NativeError::Invalid(format!(
                "write_at {offset}+{} past device end {end}",
                data.len()
            )));
        }
        self.positional.write_all_at(data, offset)?;
        self.make_durable()
    }

    fn sync_data(&self) -> std::io::Result<()> {
        self.syncs.time(|| self.sync.sync_data(&self.positional))
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
