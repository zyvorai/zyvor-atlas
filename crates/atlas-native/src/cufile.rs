// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Optional cuFile probe. Compiled only with `--features cufile`.
//!
//! It `dlopen`s `libcufile` and looks for `cuFileDriverOpen`, `cuFileRead` and `cuFileWrite`.
//! It does not call them and performs no I/O. Nothing is linked at build time: a host without
//! the library still compiles, and the probe reports the library as absent.
//!
//! `dlopen` can run a library's constructors. This module never calls into cuFile after that.

use serde::Serialize;

pub const NOTE: &str = "cuFile symbols were looked up only; no cuFile I/O was performed; unverified on real GPU hardware";

#[derive(Debug, Clone, Serialize)]
pub struct Probe {
    pub library_present: bool,
    /// Which soname answered, when one did.
    pub library: Option<String>,
    pub driver_open_symbol: bool,
    pub read_symbol: bool,
    pub write_symbol: bool,
    /// Always false.
    pub io_performed: bool,
    pub note: &'static str,
}

pub fn probe() -> Probe {
    #[cfg(unix)]
    {
        unix_probe()
    }
    #[cfg(not(unix))]
    {
        absent()
    }
}

fn absent() -> Probe {
    Probe {
        library_present: false,
        library: None,
        driver_open_symbol: false,
        read_symbol: false,
        write_symbol: false,
        io_performed: false,
        note: NOTE,
    }
}

#[cfg(unix)]
fn unix_probe() -> Probe {
    use std::ffi::CString;

    let names = ["libcufile.so.0", "libcufile.so"];
    for name in names {
        let Ok(c_name) = CString::new(name) else {
            continue;
        };
        // SAFETY: the name is a NUL-terminated string we own for the call. RTLD_LAZY does not
        // resolve symbols yet; we resolve three names ourselves and never call them.
        let handle = unsafe { libc::dlopen(c_name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        if handle.is_null() {
            continue;
        }
        let has = |sym: &str| -> bool {
            let Ok(c) = CString::new(sym) else {
                return false;
            };
            unsafe {
                libc::dlerror();
                let ptr = libc::dlsym(handle, c.as_ptr());
                let err = libc::dlerror();
                !ptr.is_null() && err.is_null()
            }
        };
        let probe = Probe {
            library_present: true,
            library: Some(name.to_string()),
            driver_open_symbol: has("cuFileDriverOpen"),
            read_symbol: has("cuFileRead"),
            write_symbol: has("cuFileWrite"),
            io_performed: false,
            note: NOTE,
        };
        unsafe {
            libc::dlclose(handle);
        }
        return probe;
    }
    absent()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_never_claims_io() {
        let p = probe();
        assert!(!p.io_performed);
        assert!(p.note.contains("no cuFile I/O"));
        if !p.library_present {
            assert!(p.library.is_none());
            assert!(!p.driver_open_symbol && !p.read_symbol && !p.write_symbol);
        }
    }
}
