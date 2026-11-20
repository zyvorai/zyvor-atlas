// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Block-aligned devices for data nodes: a regular file or a raw block device accessed in whole
//! 4 KiB blocks, through positional I/O ([`PreadIo`]) or `io_uring` with `O_DIRECT`
//! ([`crate::uring::UringIo`], Linux, `io-uring` feature).
//!
//! The engine addresses replicas by byte offset and length, so [`RawDevice`] does the alignment:
//! appends start on a block boundary (the rest of their last block is padding nobody else is
//! handed), and a write that starts or ends inside a block reads that block, merges and writes
//! it back under a lock striped by block number, since a neighbouring range may share it.
//!
//! A raw block device has no file length to say how much of it is in use, so the device keeps a
//! high-water mark in a sidecar file: no offset at or above it has ever been handed out. It is
//! raised in [`HWM_CHUNK`] steps before an append that crosses it returns, and a restart resumes
//! appending there, wasting at most one chunk.

use std::{
    alloc::{self, Layout},
    fmt,
    fs::{File, OpenOptions},
    io,
    ops::{Deref, DerefMut},
    os::unix::fs::{FileExt, FileTypeExt, OpenOptionsExt},
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::Mutex,
};

use crate::{device::BlockStore, durable, engine::NativeError};

/// I/O unit and alignment of every device access.
pub const BLOCK: u64 = 4096;
/// Step the persisted high-water mark advances by.
pub const HWM_CHUNK: u64 = 256 << 20;
const LOCK_STRIPES: usize = 64;

fn align_down(v: u64) -> u64 {
    v & !(BLOCK - 1)
}

fn align_up(v: u64) -> u64 {
    v.div_ceil(BLOCK) * BLOCK
}

/// A zeroed buffer whose address and length are multiples of [`BLOCK`], as `O_DIRECT` needs.
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: AlignedBuf owns its allocation exclusively, like a Vec<u8>.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared access only hands out &[u8].
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// `len` must be a non-zero multiple of [`BLOCK`].
    pub fn zeroed(len: usize) -> Self {
        assert!(len > 0 && (len as u64).is_multiple_of(BLOCK), "unaligned buffer length {len}");
        let layout = Self::layout(len);
        // SAFETY: layout has a non-zero size.
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        let Some(ptr) = NonNull::new(ptr) else {
            alloc::handle_alloc_error(layout);
        };
        Self { ptr, len }
    }

    fn layout(len: usize) -> Layout {
        Layout::from_size_align(len, BLOCK as usize).expect("valid layout")
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr is valid for len initialized (zeroed) bytes for the buffer's lifetime.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in deref, and &mut self guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `zeroed` with this exact layout.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), Self::layout(self.len)) }
    }
}

/// Whole-block reads and durable writes at block-aligned offsets.
pub trait AlignedIo: fmt::Debug + Send + Sync {
    fn read_at(&self, offset: u64, buf: &mut AlignedBuf) -> io::Result<()>;
    /// Returns once the data is on stable storage.
    fn write_at(&self, offset: u64, buf: &AlignedBuf) -> io::Result<()>;
    /// Size of a raw block device; `None` for a regular file, which grows as it is written.
    fn capacity(&self) -> Option<u64>;
}

/// Positional I/O through the page cache, opened `O_DSYNC` so each write is durable on return.
#[derive(Debug)]
pub struct PreadIo {
    file: File,
    capacity: Option<u64>,
}

impl PreadIo {
    pub fn open(path: &Path) -> io::Result<Self> {
        let (file, capacity) = open_device(path, libc::O_DSYNC)?;
        Ok(Self { file, capacity })
    }
}

impl AlignedIo for PreadIo {
    fn read_at(&self, offset: u64, buf: &mut AlignedBuf) -> io::Result<()> {
        read_full(&self.file, offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &AlignedBuf) -> io::Result<()> {
        self.file.write_all_at(buf, offset)
    }
    fn capacity(&self) -> Option<u64> {
        self.capacity
    }
}

/// Reads `buf.len()` bytes; past the end of a regular file reads zeros (space that was handed
/// out but whose write never landed).
fn read_full(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => {
                buf[done..].fill(0);
                return Ok(());
            }
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Opens a regular file (created if missing) or a block device with extra `open(2)` flags, and
/// returns the device's capacity if it is a block device.
pub(crate) fn open_device(path: &Path, flags: i32) -> io::Result<(File, Option<u64>)> {
    let is_block = std::fs::metadata(path)
        .map(|m| m.file_type().is_block_device())
        .unwrap_or(false);
    if !is_block {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(!is_block)
        .truncate(false)
        .custom_flags(flags)
        .open(path)?;
    let capacity = if is_block {
        Some(block_device_size(&file)?)
    } else {
        None
    };
    Ok((file, capacity))
}

#[cfg(target_os = "linux")]
fn block_device_size(file: &File) -> io::Result<u64> {
    use std::os::fd::AsRawFd;
    // BLKGETSIZE64 = _IOR(0x12, 114, size_t)
    const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
    let mut size: u64 = 0;
    // SAFETY: BLKGETSIZE64 writes one u64 through the pointer.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), BLKGETSIZE64, &mut size) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(size)
}

#[cfg(not(target_os = "linux"))]
fn block_device_size(file: &File) -> io::Result<u64> {
    use std::io::{Seek, SeekFrom};
    let mut f = file;
    f.seek(SeekFrom::End(0))
}

#[derive(Debug)]
struct Alloc {
    /// Next append offset (always block-aligned).
    end: u64,
    /// Persisted: no offset at or above it has been handed out.
    hwm: u64,
}

/// A [`BlockStore`] over an [`AlignedIo`]; see the module docs.
#[derive(Debug)]
pub struct RawDevice<B: AlignedIo> {
    io: B,
    hwm_path: PathBuf,
    alloc: Mutex<Alloc>,
    locks: Vec<Mutex<()>>,
}

impl<B: AlignedIo> RawDevice<B> {
    /// `hwm_path` holds the high-water mark. Without one, appending starts after `existing_len`
    /// bytes (a regular file's current length; 0 for a fresh device).
    pub fn open(io: B, hwm_path: impl Into<PathBuf>, existing_len: u64) -> Result<Self, NativeError> {
        let hwm_path = hwm_path.into();
        let end = match std::fs::read_to_string(&hwm_path) {
            Ok(s) => s
                .trim()
                .parse()
                .map_err(|e| NativeError::Invalid(format!("corrupt high-water mark: {e}")))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => align_up(existing_len),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            io,
            hwm_path,
            alloc: Mutex::new(Alloc { end, hwm: end }),
            locks: (0..LOCK_STRIPES).map(|_| Mutex::new(())).collect(),
        })
    }

    /// Hands out `[offset, offset + len)`, raising the persisted high-water mark first if needed.
    fn reserve(&self, len: u64) -> Result<u64, NativeError> {
        let mut a = self
            .alloc
            .lock()
            .map_err(|_| NativeError::Poisoned("device alloc"))?;
        let offset = a.end;
        let end = align_up(offset + len.max(1));
        if let Some(cap) = self.io.capacity() {
            if end > cap {
                return Err(NativeError::Invalid(format!(
                    "device full: {len} bytes at {offset} exceed capacity {cap}"
                )));
            }
        }
        if end > a.hwm {
            let mut hwm = end.div_ceil(HWM_CHUNK) * HWM_CHUNK;
            if let Some(cap) = self.io.capacity() {
                hwm = hwm.min(cap);
            }
            durable::write_atomic(&self.hwm_path, hwm.to_string().as_bytes())?;
            a.hwm = hwm;
        }
        a.end = end;
        Ok(offset)
    }

    fn stripe(&self, block: u64) -> usize {
        (block % LOCK_STRIPES as u64) as usize
    }
}

impl<B: AlignedIo> BlockStore for RawDevice<B> {
    fn append(&self, _fence: u64, data: &[u8]) -> Result<u64, NativeError> {
        let offset = self.reserve(data.len() as u64)?;
        if data.is_empty() {
            return Ok(offset);
        }
        let mut buf = AlignedBuf::zeroed(align_up(data.len() as u64) as usize);
        buf[..data.len()].copy_from_slice(data);
        self.io.write_at(offset, &buf)?;
        Ok(offset)
    }

    fn write_at(&self, _fence: u64, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        if data.is_empty() {
            return Ok(());
        }
        let stop = offset + data.len() as u64;
        let end = self
            .alloc
            .lock()
            .map_err(|_| NativeError::Poisoned("device alloc"))?
            .end;
        if stop > end {
            return Err(NativeError::Invalid(format!(
                "write_at {offset}+{} past device end {end}",
                data.len()
            )));
        }
        let (start, span_end) = (align_down(offset), align_up(stop));
        let mut buf = AlignedBuf::zeroed((span_end - start) as usize);
        let head = (offset > start).then_some(start / BLOCK);
        let tail = (stop < span_end).then_some(span_end / BLOCK - 1);
        let mut stripes: Vec<usize> = head.into_iter().chain(tail).map(|b| self.stripe(b)).collect();
        stripes.sort_unstable();
        stripes.dedup();
        let _guards = stripes
            .iter()
            .map(|&i| {
                self.locks[i]
                    .lock()
                    .map_err(|_| NativeError::Poisoned("block lock"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        for block in head.into_iter().chain(tail) {
            let mut one = AlignedBuf::zeroed(BLOCK as usize);
            self.io.read_at(block * BLOCK, &mut one)?;
            let at = (block * BLOCK - start) as usize;
            buf[at..at + BLOCK as usize].copy_from_slice(&one);
        }
        let at = (offset - start) as usize;
        buf[at..at + data.len()].copy_from_slice(data);
        self.io.write_at(start, &buf)?;
        Ok(())
    }

    fn read_exact_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, NativeError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let stop = offset + len as u64;
        let end = self
            .alloc
            .lock()
            .map_err(|_| NativeError::Poisoned("device alloc"))?
            .end;
        if stop > end {
            return Err(NativeError::Invalid(format!(
                "read {offset}+{len} past device end {end}"
            )));
        }
        let start = align_down(offset);
        let mut buf = AlignedBuf::zeroed((align_up(stop) - start) as usize);
        self.io.read_at(start, &mut buf)?;
        let at = (offset - start) as usize;
        Ok(buf[at..at + len].to_vec())
    }

    fn len(&self) -> Result<u64, NativeError> {
        Ok(self
            .alloc
            .lock()
            .map_err(|_| NativeError::Poisoned("device alloc"))?
            .end)
    }
}

/// How a data node accesses one of its devices.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceBackend {
    /// [`crate::device::FileDevice`]: a regular file, byte-addressed, fsync per write.
    #[default]
    File,
    /// [`RawDevice`] over [`PreadIo`]: block-aligned positional I/O, `O_DSYNC`.
    Aligned,
    /// [`RawDevice`] over `io_uring` with `O_DIRECT | O_DSYNC` (Linux, `io-uring` feature).
    IoUring,
}

/// Opens `path` (a regular file or, for the aligned backends, a block device) with `backend`.
/// The aligned backends keep their high-water mark in `<path>.hwm`, or `hwm_dir/<name>.hwm` for
/// a block device, whose directory isn't writable.
pub fn open_store(
    path: &Path,
    backend: DeviceBackend,
    hwm_dir: &Path,
) -> Result<std::sync::Arc<dyn BlockStore>, NativeError> {
    use std::sync::Arc;
    let hwm_path = || -> PathBuf {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "device".into());
        hwm_dir.join(format!("{name}.hwm"))
    };
    let existing = |p: &Path| -> u64 {
        std::fs::metadata(p)
            .ok()
            .filter(|m| m.is_file())
            .map_or(0, |m| m.len())
    };
    if backend != DeviceBackend::File {
        std::fs::create_dir_all(hwm_dir)?;
    }
    Ok(match backend {
        DeviceBackend::File => Arc::new(crate::device::FileDevice::open(path)?),
        DeviceBackend::Aligned => {
            let len = existing(path);
            Arc::new(RawDevice::open(PreadIo::open(path)?, hwm_path(), len)?)
        }
        DeviceBackend::IoUring => {
            #[cfg(all(target_os = "linux", feature = "io-uring"))]
            {
                let len = existing(path);
                Arc::new(RawDevice::open(
                    crate::uring::UringIo::open(path)?,
                    hwm_path(),
                    len,
                )?)
            }
            #[cfg(not(all(target_os = "linux", feature = "io-uring")))]
            {
                return Err(NativeError::Invalid(
                    "the io_uring backend needs a Linux build with the io-uring feature".into(),
                ));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`PreadIo`] that fails any access O_DIRECT would refuse.
    #[derive(Debug)]
    struct Strict(PreadIo);

    impl AlignedIo for Strict {
        fn read_at(&self, offset: u64, buf: &mut AlignedBuf) -> io::Result<()> {
            assert_eq!(offset % BLOCK, 0);
            assert_eq!(buf.as_ptr() as usize % BLOCK as usize, 0);
            self.0.read_at(offset, buf)
        }
        fn write_at(&self, offset: u64, buf: &AlignedBuf) -> io::Result<()> {
            assert_eq!(offset % BLOCK, 0);
            assert_eq!(buf.as_ptr() as usize % BLOCK as usize, 0);
            self.0.write_at(offset, buf)
        }
        fn capacity(&self) -> Option<u64> {
            self.0.capacity()
        }
    }

    fn device(dir: &Path) -> RawDevice<Strict> {
        let p = dir.join("d.data");
        RawDevice::open(Strict(PreadIo::open(&p).unwrap()), dir.join("d.hwm"), 0).unwrap()
    }

    #[test]
    fn appends_are_block_aligned_and_round_trip() {
        let td = tempfile::tempdir().unwrap();
        let d = device(td.path());
        let a = d.append(0, &[1u8; 5000]).unwrap();
        let b = d.append(0, &[2u8; 10]).unwrap();
        assert_eq!((a, b), (0, 8192));
        assert_eq!(d.len().unwrap(), 12288);
        assert_eq!(d.read_exact_at(a, 5000).unwrap(), vec![1u8; 5000]);
        assert_eq!(d.read_exact_at(b, 10).unwrap(), vec![2u8; 10]);
        assert_eq!(d.read_exact_at(4000, 100).unwrap(), vec![1u8; 100]);
        assert!(d.read_exact_at(12000, 1000).is_err());
    }

    #[test]
    fn unaligned_overwrites_keep_neighbouring_bytes() {
        let td = tempfile::tempdir().unwrap();
        let d = device(td.path());
        d.append(0, &[7u8; 3 * BLOCK as usize]).unwrap();
        d.write_at(0, 100, &[9u8; 5000]).unwrap();
        let all = d.read_exact_at(0, 3 * BLOCK as usize).unwrap();
        assert!(all[..100].iter().all(|b| *b == 7));
        assert!(all[100..5100].iter().all(|b| *b == 9));
        assert!(all[5100..].iter().all(|b| *b == 7));
        assert!(d.write_at(0, 3 * BLOCK - 1, &[1, 2]).is_err());
    }

    #[test]
    fn concurrent_writes_sharing_blocks_do_not_lose_bytes() {
        let td = tempfile::tempdir().unwrap();
        let d = device(td.path());
        d.append(0, &vec![0u8; 64 * BLOCK as usize]).unwrap();
        // 100-byte ranges packed back to back: neighbours share blocks.
        std::thread::scope(|s| {
            for t in 0..8u8 {
                let d = &d;
                s.spawn(move || {
                    for i in (t as u64..160).step_by(8) {
                        d.write_at(0, i * 100, &[t + 1; 100]).unwrap();
                    }
                });
            }
        });
        let all = d.read_exact_at(0, 16_000).unwrap();
        for i in 0..160usize {
            let want = (i % 8) as u8 + 1;
            assert!(all[i * 100..(i + 1) * 100].iter().all(|b| *b == want), "range {i}");
        }
    }

    #[test]
    fn high_water_mark_survives_reopen() {
        let td = tempfile::tempdir().unwrap();
        let d = device(td.path());
        d.append(0, b"hello").unwrap();
        drop(d);
        let hwm: u64 = std::fs::read_to_string(td.path().join("d.hwm"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(hwm, HWM_CHUNK);
        let d = device(td.path());
        assert_eq!(d.append(0, b"next").unwrap(), HWM_CHUNK);
        assert_eq!(d.read_exact_at(0, 5).unwrap(), b"hello");
    }

    #[test]
    fn a_file_without_a_mark_appends_after_its_contents() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("old.data");
        std::fs::write(&p, vec![3u8; 5000]).unwrap();
        let d = RawDevice::open(PreadIo::open(&p).unwrap(), td.path().join("old.hwm"), 5000)
            .unwrap();
        assert_eq!(d.read_exact_at(0, 5000).unwrap(), vec![3u8; 5000]);
        assert_eq!(d.append(0, b"x").unwrap(), 8192);
    }
}
