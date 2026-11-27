// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::FileExt,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, MutexGuard},
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
    /// Network address a client can read this device at directly, if it is remote.
    fn endpoint(&self) -> Option<&str> {
        None
    }
}

impl BlockStore for FileDevice {
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

/// Makes the data written to a [`FileDevice`]'s file durable. [`DataSync`] is what devices use;
/// [`FileDevice::open_with_sync`] lets tests substitute one that stalls or fails.
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

/// Group commit for one file: writers waiting for durability share syncs.
///
/// A writer takes a number once its write has returned. At most one sync runs at a time, run by
/// one of the waiting writers (there is no flusher thread and no batching delay, so a lone writer
/// syncs at once). A sync covers exactly the numbers taken before it started; a write that
/// returns while it runs waits for the next one.
///
/// A failed sync poisons the file for as long as it stays open: the kernel reports a writeback
/// error once and may drop the pages, so a later sync could succeed without the data being on
/// disk. Every writer not covered by an earlier successful sync gets an error.
///
/// It takes no lock but its own, and does not hold that one while syncing.
#[derive(Debug, Default)]
struct GroupCommit {
    state: Mutex<CommitState>,
    /// Signalled when a sync finishes.
    done: Condvar,
}

#[derive(Debug, Default)]
struct CommitState {
    /// The number taken by the latest write to return.
    written: u64,
    /// Writes numbered up to this are covered by a successful sync.
    synced: u64,
    /// A writer is running a sync.
    syncing: bool,
    /// The failure that poisoned the file.
    failed: Option<(io::ErrorKind, String)>,
}

impl CommitState {
    fn check(&self) -> Result<(), NativeError> {
        match &self.failed {
            None => Ok(()),
            Some((kind, cause)) => Err(io::Error::new(
                *kind,
                format!("device sync failed, no writes until it is reopened: {cause}"),
            )
            .into()),
        }
    }
}

impl GroupCommit {
    fn lock(&self) -> Result<MutexGuard<'_, CommitState>, NativeError> {
        self.state
            .lock()
            .map_err(|_| NativeError::Poisoned("device sync"))
    }

    /// Returns once a sync that started after this call did has succeeded. Call it only after
    /// the write it is to make durable has returned.
    fn commit(&self, sync: impl Fn() -> io::Result<()>) -> Result<(), NativeError> {
        let mut state = self.lock()?;
        state.written += 1;
        let mine = state.written;
        loop {
            // Checked before the failure: a later failed sync does not undo an earlier success.
            if state.synced >= mine {
                return Ok(());
            }
            state.check()?;
            if state.syncing {
                state = self
                    .done
                    .wait(state)
                    .map_err(|_| NativeError::Poisoned("device sync"))?;
                continue;
            }
            state.syncing = true;
            let covers = state.written;
            drop(state);
            // A panicking sync must not leave the other writers waiting for it.
            let result = panic::catch_unwind(AssertUnwindSafe(&sync))
                .unwrap_or_else(|_| Err(io::Error::other("sync panicked")));
            state = self.lock()?;
            state.syncing = false;
            match result {
                Ok(()) => state.synced = covers,
                Err(e) => state.failed = Some((e.kind(), e.to_string())),
            }
            self.done.notify_all();
        }
    }
}

/// Positional I/O on one file. An append only reserves its offset under a lock, then writes like
/// an in-place write, so concurrent appends overlap. Both return once the data is durable;
/// concurrent writers share syncs (group commit), and after a failed sync the device refuses
/// writes until it is reopened.
#[derive(Debug)]
pub struct FileDevice {
    id: DeviceId,
    path: PathBuf,
    /// End of the space handed out to appends, including ones still being written.
    end: Mutex<u64>,
    positional: File,
    commit: GroupCommit,
    sync: Box<dyn DeviceSync>,
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
        let end = file.metadata()?.len();
        Ok(Self {
            id: DeviceId(Uuid::new_v4().to_string()),
            path,
            end: Mutex::new(end),
            positional: file,
            commit: GroupCommit::default(),
            sync,
        })
    }

    pub fn id(&self) -> &DeviceId {
        &self.id
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes that have returned and are not covered by a successful sync.
    #[doc(hidden)]
    pub fn unsynced_writes(&self) -> u64 {
        self.commit.lock().map_or(0, |s| s.written - s.synced)
    }

    /// Writes `data` at `offset` and returns once it is durable.
    fn write_durably(&self, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        self.positional.write_all_at(data, offset)?;
        self.commit.commit(|| self.sync.sync_data(&self.positional))
    }

    pub fn append(&self, data: &[u8]) -> Result<u64, NativeError> {
        self.commit.lock()?.check()?;
        let off = {
            let mut end = self
                .end
                .lock()
                .map_err(|_| NativeError::Poisoned("device"))?;
            let off = *end;
            *end += data.len() as u64;
            off
        };
        if let Err(e) = self.write_durably(off, data) {
            // Give the space back unless a later append already took the space after it; an
            // unacknowledged append is referenced by nothing either way.
            if let Ok(mut end) = self.end.lock() {
                if *end == off + data.len() as u64 {
                    *end = off;
                }
            }
            return Err(e);
        }
        Ok(off)
    }

    /// Overwrites `data.len()` bytes at `offset` (reuse of a freed range). Never extends the file.
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        self.commit.lock()?.check()?;
        let end = self.len()?;
        if offset + data.len() as u64 > end {
            return Err(NativeError::Invalid(format!(
                "write_at {offset}+{} past device end {end}",
                data.len()
            )));
        }
        self.write_durably(offset, data)
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
