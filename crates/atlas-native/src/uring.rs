// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `io_uring` backend for [`crate::raw::RawDevice`]: the device is opened `O_DIRECT | O_DSYNC`,
//! so data bypasses the page cache and every write is durable when it completes. Each calling
//! thread submits through its own ring (data-node connections each have their own thread), so
//! concurrent requests reach the device in parallel without sharing a submission queue.

use std::{cell::RefCell, fs::File, io, os::fd::AsRawFd, path::Path};

use io_uring::{opcode, types, IoUring};

use crate::raw::{open_device, AlignedBuf, AlignedIo};

const RING_ENTRIES: u32 = 8;

thread_local! {
    static RING: RefCell<Option<IoUring>> = const { RefCell::new(None) };
}

#[derive(Debug)]
pub struct UringIo {
    file: File,
    capacity: Option<u64>,
}

impl UringIo {
    /// Fails if the filesystem refuses `O_DIRECT` (tmpfs, for one) or `io_uring` is unavailable
    /// (old kernels, or a seccomp profile that blocks it).
    pub fn open(path: &Path) -> io::Result<Self> {
        let (file, capacity) = open_device(path, libc::O_DIRECT | libc::O_DSYNC)?;
        let dev = Self { file, capacity };
        with_ring(|_| Ok(()))?;
        Ok(dev)
    }

    /// Runs one read or write to completion, resubmitting the rest after a short transfer.
    fn transfer(&self, offset: u64, ptr: *mut u8, len: usize, write: bool) -> io::Result<()> {
        let fd = types::Fd(self.file.as_raw_fd());
        let mut done = 0usize;
        while done < len {
            let rest = u32::try_from(len - done).unwrap_or(!(4096 - 1));
            // SAFETY: ptr..ptr+len stays valid and unaliased until this call returns, and the
            // operation completes (submit_and_wait) before we touch the buffer again.
            let at = unsafe { ptr.add(done) };
            let entry = if write {
                opcode::Write::new(fd, at, rest)
                    .offset(offset + done as u64)
                    .build()
            } else {
                opcode::Read::new(fd, at, rest)
                    .offset(offset + done as u64)
                    .build()
            };
            let res = with_ring(|ring| {
                // SAFETY: the entry's buffer outlives the operation (see above).
                unsafe { ring.submission().push(&entry) }
                    .map_err(|_| io::Error::other("io_uring submission queue full"))?;
                wait_for_completion(ring)
            })?;
            if res < 0 {
                return Err(io::Error::from_raw_os_error(-res));
            }
            if res == 0 {
                if write {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "device wrote nothing"));
                }
                // Past the end of a regular file: space handed out whose write never landed.
                // SAFETY: done..len lies within the buffer.
                unsafe { std::ptr::write_bytes(ptr.add(done), 0, len - done) };
                return Ok(());
            }
            done += res as usize;
        }
        Ok(())
    }
}

/// Submits the queued operation and returns its result once it has completed.
///
/// A signal can end the wait early: `io_uring_enter` then reports the submission, not the
/// interruption. Returning without the completion would leave it in the ring, where the next
/// operation would take it for its own and report success before its own write had finished.
fn wait_for_completion(ring: &mut IoUring) -> io::Result<i32> {
    loop {
        match ring.submit_and_wait(1) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
        if let Some(c) = ring.completion().next() {
            return Ok(c.result());
        }
    }
}

/// Runs `f` on this thread's ring. A ring that fails is dropped, so that nothing left queued or
/// uncompleted in it can be mistaken for a later operation's.
fn with_ring<R>(f: impl FnOnce(&mut IoUring) -> io::Result<R>) -> io::Result<R> {
    RING.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(IoUring::new(RING_ENTRIES)?);
        }
        let result = f(slot.as_mut().expect("ring initialized"));
        if result.is_err() {
            *slot = None;
        }
        result
    })
}

impl AlignedIo for UringIo {
    fn read_at(&self, offset: u64, buf: &mut AlignedBuf) -> io::Result<()> {
        let len = buf.len();
        self.transfer(offset, buf.as_mut_ptr(), len, false)
    }

    fn write_at(&self, offset: u64, buf: &AlignedBuf) -> io::Result<()> {
        // The kernel only reads from the buffer for a write.
        self.transfer(offset, buf.as_ptr() as *mut u8, buf.len(), true)
    }

    fn capacity(&self) -> Option<u64> {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{device::BlockStore, raw::RawDevice};

    /// Skips (returns None) where the kernel, sandbox or filesystem can't do O_DIRECT io_uring.
    fn device(dir: &Path) -> Option<RawDevice<UringIo>> {
        match UringIo::open(&dir.join("u.data")) {
            Ok(io) => Some(RawDevice::open(io, dir.join("u.hwm"), 0).unwrap()),
            Err(e) => {
                eprintln!("skipping io_uring test: {e}");
                None
            }
        }
    }

    extern "C" fn interrupt_only(_: libc::c_int) {}

    /// A signal that arrives while a thread waits for its completion must neither fail the
    /// operation nor leave its completion for the next one to take as its own.
    #[test]
    fn signals_during_the_wait_do_not_mix_up_completions() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = std::env::var("ATLAS_URING_TEST_DIR")
            .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string());
        let td = tempfile::tempdir_in(dir).unwrap();
        let Some(d) = device(td.path()) else { return };
        // SAFETY: installs a handler that does nothing, without SA_RESTART, for a signal
        // nothing else in this test binary uses.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = interrupt_only as *const () as usize;
            assert_eq!(
                libc::sigaction(libc::SIGUSR2, &action, std::ptr::null_mut()),
                0
            );
        }
        // SAFETY: always valid for the calling thread.
        let writer = unsafe { libc::pthread_self() };
        let stop = AtomicBool::new(false);
        let outcome = std::thread::scope(|s| {
            s.spawn(|| {
                while !stop.load(Ordering::SeqCst) {
                    // SAFETY: the writer outlives this thread, which the scope joins.
                    unsafe { libc::pthread_kill(writer, libc::SIGUSR2) };
                    std::thread::sleep(std::time::Duration::from_micros(20));
                }
            });
            let outcome = (0..300u32).try_for_each(|i| {
                let want = vec![i as u8; 256 << 10];
                let at = d.append(0, &want).map_err(|e| format!("write {i}: {e}"))?;
                let got = d
                    .read_exact_at(at, want.len())
                    .map_err(|e| format!("read {i}: {e}"))?;
                (got == want)
                    .then_some(())
                    .ok_or_else(|| format!("read {i} returned other bytes"))
            });
            stop.store(true, Ordering::SeqCst);
            outcome
        });
        assert_eq!(outcome, Ok(()));
    }

    #[test]
    fn round_trips_through_io_uring() {
        // Not /tmp by default: it is often tmpfs, which refuses O_DIRECT.
        let dir = std::env::var("ATLAS_URING_TEST_DIR")
            .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string());
        let td = tempfile::tempdir_in(dir).unwrap();
        let Some(d) = device(td.path()) else { return };
        let a = d.append(0, &[5u8; 10_000]).unwrap();
        let b = d.append(0, &[6u8; 3]).unwrap();
        assert_eq!((a, b), (0, 12288));
        d.write_at(0, 4000, &[8u8; 200]).unwrap();
        let got = d.read_exact_at(0, 10_000).unwrap();
        assert!(got[..4000].iter().all(|x| *x == 5));
        assert!(got[4000..4200].iter().all(|x| *x == 8));
        assert!(got[4200..].iter().all(|x| *x == 5));
        assert_eq!(d.read_exact_at(b, 3).unwrap(), vec![6u8; 3]);
        std::thread::scope(|s| {
            for t in 0..4u8 {
                let d = &d;
                s.spawn(move || {
                    for _ in 0..16 {
                        let off = d.append(0, &[t; 70_000]).unwrap();
                        assert_eq!(d.read_exact_at(off, 70_000).unwrap(), vec![t; 70_000]);
                    }
                });
            }
        });
    }
}
