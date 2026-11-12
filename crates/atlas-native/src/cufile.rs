// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Optional cuFile bindings. Compiled only with `--features cufile`.
//!
//! `libcufile` is `dlopen`ed (`libcufile.so.0`, then `libcufile.so`). Nothing is linked at build
//! time. When the library or a symbol is missing, [`read`] and [`write`] return
//! [`CuFileError`] and perform no call. When the symbols exist, those wrappers call
//! `cuFileRead` / `cuFileWrite`. [`probe`] also tries `cuFileDriverOpen` once per process: that
//! is the test call behind `io_performed`. Success means the driver open returned
//! `CU_FILE_SUCCESS` and was then closed. It is not a read or write of a GPU buffer, and it has
//! not been run on a machine that has the library.
//!
//! The mapping is kept for the process lifetime (it is not `dlclose`d) so a later read or write
//! is not a use-after-unload. `dlopen` can run the library's constructors.

use std::ffi::{c_void, CString};
use std::sync::OnceLock;

pub const NOTE_ABSENT: &str =
    "libcufile was not found; no cuFile call was made; unverified on real GPU hardware";
pub const NOTE_NO_IO: &str = "cuFile symbols were resolved; the driver-open test call did not succeed; no cuFile read or write was performed; unverified on real GPU hardware";
pub const NOTE_OPENED: &str = "cuFileDriverOpen succeeded and was closed; no cuFile read or write of a buffer was performed; unverified on real GPU hardware";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CuFileError {
    /// `dlopen` did not find `libcufile`.
    LibraryAbsent,
    /// The library loaded but this symbol did not resolve.
    SymbolMissing(&'static str),
    /// A zero-length buffer is refused so it cannot be reported as a successful transfer.
    Empty,
    /// `cuFileDriverOpen` / `cuFileDriverClose` returned a non-zero `CUfileOpError`.
    Driver { op: i32, cu: i32 },
    /// `cuFileRead` or `cuFileWrite` returned a negative `ssize_t`.
    Io(isize),
}

impl std::fmt::Display for CuFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CuFileError::LibraryAbsent => write!(f, "libcufile is not installed"),
            CuFileError::SymbolMissing(name) => write!(f, "{name} is not exported by libcufile"),
            CuFileError::Empty => write!(f, "cuFile transfer length is 0"),
            CuFileError::Driver { op, cu } => {
                write!(f, "cuFile driver call returned op {op} cu {cu}")
            }
            CuFileError::Io(n) => write!(f, "cuFile read or write returned {n}"),
        }
    }
}

impl std::error::Error for CuFileError {}

#[repr(C)]
#[derive(Clone, Copy)]
struct CufileErrorRaw {
    op: i32,
    cu: i32,
}

type DriverFn = unsafe extern "C" fn() -> CufileErrorRaw;
type ReadFn = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, i64, i64) -> isize;
type WriteFn = unsafe extern "C" fn(*mut c_void, *const c_void, usize, i64, i64) -> isize;

struct Loaded {
    /// `dlopen` handle kept so the mapping outlives every call. Not dereferenced.
    _handle: usize,
    name: String,
    driver_open: Option<DriverFn>,
    driver_close: Option<DriverFn>,
    read: Option<ReadFn>,
    write: Option<WriteFn>,
}

struct ProbeState {
    loaded: Option<Loaded>,
    io_performed: bool,
    io_error: Option<String>,
    note: &'static str,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Probe {
    pub library_present: bool,
    /// Which soname answered, when one did.
    pub library: Option<String>,
    pub driver_open_symbol: bool,
    pub read_symbol: bool,
    pub write_symbol: bool,
    /// True only when the once-per-process `cuFileDriverOpen` test call returned success.
    pub io_performed: bool,
    /// Why `io_performed` is false, when it is.
    pub io_error: Option<String>,
    pub note: &'static str,
}

fn state() -> &'static ProbeState {
    static STATE: OnceLock<ProbeState> = OnceLock::new();
    STATE.get_or_init(probe_once)
}

pub fn probe() -> Probe {
    let s = state();
    let loaded = s.loaded.as_ref();
    Probe {
        library_present: loaded.is_some(),
        library: loaded.map(|l| l.name.clone()),
        driver_open_symbol: loaded.is_some_and(|l| l.driver_open.is_some()),
        read_symbol: loaded.is_some_and(|l| l.read.is_some()),
        write_symbol: loaded.is_some_and(|l| l.write.is_some()),
        io_performed: s.io_performed,
        io_error: s.io_error.clone(),
        note: s.note,
    }
}

/// Opens the cuFile driver. A missing library or symbol is an error, not a panic.
pub fn driver_open() -> Result<(), CuFileError> {
    let loaded = state().loaded.as_ref().ok_or(CuFileError::LibraryAbsent)?;
    let f = loaded
        .driver_open
        .ok_or(CuFileError::SymbolMissing("cuFileDriverOpen"))?;
    let err = unsafe { f() };
    if err.op == 0 {
        Ok(())
    } else {
        Err(CuFileError::Driver {
            op: err.op,
            cu: err.cu,
        })
    }
}

/// Closes the cuFile driver.
pub fn driver_close() -> Result<(), CuFileError> {
    let loaded = state().loaded.as_ref().ok_or(CuFileError::LibraryAbsent)?;
    let f = loaded
        .driver_close
        .ok_or(CuFileError::SymbolMissing("cuFileDriverClose"))?;
    let err = unsafe { f() };
    if err.op == 0 {
        Ok(())
    } else {
        Err(CuFileError::Driver {
            op: err.op,
            cu: err.cu,
        })
    }
}

/// `cuFileRead`. `handle` is a `CUfileHandle_t` the caller registered. The buffer is whatever
/// cuFile expects (typically a registered device pointer); this wrapper does not allocate one.
pub fn read(
    handle: *mut c_void,
    buf: &mut [u8],
    file_offset: i64,
    buf_offset: i64,
) -> Result<usize, CuFileError> {
    let loaded = state().loaded.as_ref().ok_or(CuFileError::LibraryAbsent)?;
    if buf.is_empty() {
        return Err(CuFileError::Empty);
    }
    let f = loaded
        .read
        .ok_or(CuFileError::SymbolMissing("cuFileRead"))?;
    let n = unsafe {
        f(
            handle,
            buf.as_mut_ptr().cast(),
            buf.len(),
            file_offset,
            buf_offset,
        )
    };
    if n < 0 {
        Err(CuFileError::Io(n))
    } else {
        Ok(n as usize)
    }
}

/// `cuFileWrite`. Same handle and buffer rules as [`read`].
pub fn write(
    handle: *mut c_void,
    buf: &[u8],
    file_offset: i64,
    buf_offset: i64,
) -> Result<usize, CuFileError> {
    let loaded = state().loaded.as_ref().ok_or(CuFileError::LibraryAbsent)?;
    if buf.is_empty() {
        return Err(CuFileError::Empty);
    }
    let f = loaded
        .write
        .ok_or(CuFileError::SymbolMissing("cuFileWrite"))?;
    let n = unsafe {
        f(
            handle,
            buf.as_ptr().cast(),
            buf.len(),
            file_offset,
            buf_offset,
        )
    };
    if n < 0 {
        Err(CuFileError::Io(n))
    } else {
        Ok(n as usize)
    }
}

fn probe_once() -> ProbeState {
    let Some(loaded) = load_library() else {
        return ProbeState {
            loaded: None,
            io_performed: false,
            io_error: Some("libcufile is not installed".into()),
            note: NOTE_ABSENT,
        };
    };
    let (io_performed, io_error, note) = match loaded.driver_open {
        None => (
            false,
            Some("cuFileDriverOpen is not exported".into()),
            NOTE_NO_IO,
        ),
        Some(f) => {
            let err = unsafe { f() };
            if err.op == 0 {
                if let Some(close) = loaded.driver_close {
                    let _ = unsafe { close() };
                }
                (true, None, NOTE_OPENED)
            } else {
                (
                    false,
                    Some(format!(
                        "cuFileDriverOpen returned op {} cu {}",
                        err.op, err.cu
                    )),
                    NOTE_NO_IO,
                )
            }
        }
    };
    ProbeState {
        loaded: Some(loaded),
        io_performed,
        io_error,
        note,
    }
}

#[cfg(unix)]
fn load_library() -> Option<Loaded> {
    let names = ["libcufile.so.0", "libcufile.so"];
    for name in names {
        let Ok(c_name) = CString::new(name) else {
            continue;
        };
        // SAFETY: `c_name` is NUL-terminated and lives for the call. The handle is stored, not
        // closed, so the mapping remains valid for later calls. RTLD_LOCAL keeps the symbols
        // out of the global namespace.
        let handle = unsafe { libc::dlopen(c_name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        if handle.is_null() {
            continue;
        }
        return Some(Loaded {
            _handle: handle as usize,
            name: name.to_string(),
            driver_open: lookup(handle, "cuFileDriverOpen"),
            driver_close: lookup(handle, "cuFileDriverClose"),
            read: lookup(handle, "cuFileRead"),
            write: lookup(handle, "cuFileWrite"),
        });
    }
    None
}

#[cfg(not(unix))]
fn load_library() -> Option<Loaded> {
    None
}

#[cfg(unix)]
fn lookup<T: Copy>(handle: *mut c_void, name: &str) -> Option<T> {
    let Ok(c) = CString::new(name) else {
        return None;
    };
    unsafe {
        libc::dlerror();
        let ptr = libc::dlsym(handle, c.as_ptr());
        if ptr.is_null() || !libc::dlerror().is_null() {
            return None;
        }
        // The bits of `ptr` are the symbol address. `T` is a function pointer of the same width.
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
        Some(std::mem::transmute_copy(&ptr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_library_performs_no_io_and_the_wrappers_error() {
        let p = probe();
        if p.library_present {
            // A host that actually has libcufile is outside this test. `io_performed` may be
            // true there only if cuFileDriverOpen succeeded.
            return;
        }
        assert!(!p.io_performed);
        assert!(p.library.is_none());
        assert!(!p.driver_open_symbol && !p.read_symbol && !p.write_symbol);
        assert!(p.io_error.unwrap().contains("not installed"));
        assert!(p.note.contains("no cuFile call"));
        let mut buf = [0u8; 4];
        assert_eq!(
            read(std::ptr::null_mut(), &mut buf, 0, 0).unwrap_err(),
            CuFileError::LibraryAbsent
        );
        assert_eq!(
            write(std::ptr::null_mut(), &buf, 0, 0).unwrap_err(),
            CuFileError::LibraryAbsent
        );
        assert_eq!(driver_open().unwrap_err(), CuFileError::LibraryAbsent);
        assert_eq!(driver_close().unwrap_err(), CuFileError::LibraryAbsent);
        assert_eq!(
            read(std::ptr::null_mut(), &mut [], 0, 0).unwrap_err(),
            CuFileError::LibraryAbsent
        );
    }
}
