// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! What the device tells the kernel about itself, and how a [`Reply`] becomes the result of a
//! ublk command. Kept apart from the ublk server so both are tested on any OS.

use crate::{
    volume::{Geometry, Op, Reply},
    Errno,
};

/// The kernel's sector, the unit of ublk requests whatever the logical block size.
pub const SECTOR: u64 = 512;
/// The largest per-request buffer ublk accepts.
pub const MAX_IO_BYTES: u32 = 32 << 20;

/// There is no volatile-cache or FUA flag because the device has no cache, and no discard limit
/// because the cluster cannot release blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceParams {
    pub read_only: bool,
    pub logical_bs_shift: u8,
    pub physical_bs_shift: u8,
    pub io_min_shift: u8,
    pub io_opt_shift: u8,
    /// Largest read or write, in sectors.
    pub max_sectors: u32,
    pub dev_sectors: u64,
    /// Largest write-zeroes request, in sectors; 0 for a read-only device.
    pub max_write_zeroes_sectors: u32,
}

/// Checks a per-request buffer size: whole 4 KiB pages, up to [`MAX_IO_BYTES`].
pub fn check_io_bytes(io_bytes: u32) -> Result<(), String> {
    if !(4096..=MAX_IO_BYTES).contains(&io_bytes) || !io_bytes.is_multiple_of(4096) {
        return Err(format!(
            "--io-bytes {io_bytes} must be a multiple of 4096 between 4096 and {MAX_IO_BYTES}"
        ));
    }
    Ok(())
}

pub fn device_params(g: Geometry, io_bytes: u32, read_only: bool) -> DeviceParams {
    let logical = g.block_bytes.trailing_zeros() as u8;
    let physical = logical.max(12);
    // Writes of whole extents are the cheap ones.
    let optimal = if g.extent_bytes.is_power_of_two() {
        (g.extent_bytes.trailing_zeros() as u8).clamp(physical, 25)
    } else {
        physical
    };
    let max_sectors = io_bytes / SECTOR as u32;
    DeviceParams {
        read_only,
        logical_bs_shift: logical,
        physical_bs_shift: physical,
        io_min_shift: physical,
        io_opt_shift: optimal,
        max_sectors,
        dev_sectors: g.size_bytes / SECTOR,
        max_write_zeroes_sectors: if read_only { 0 } else { max_sectors },
    }
}

/// The ublk result of a request of `len` bytes: the whole length for a read or a write (a short
/// count would make the kernel send the rest again), 0 for a flush, or a negated errno. A reply
/// that does not match the request is `EIO`, never success.
pub fn completion(op: Op, len: u64, reply: &Reply) -> i32 {
    let fail = |e: Errno| -e;
    match reply {
        Err(e) if *e > 0 => fail(*e),
        Err(_) => fail(libc::EIO),
        Ok(data) if op == Op::Read && data.len() as u64 != len => fail(libc::EIO),
        Ok(_) if op == Op::Flush => 0,
        Ok(_) => i32::try_from(len).unwrap_or_else(|_| fail(libc::EIO)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: Geometry = Geometry {
        size_bytes: 1 << 30,
        block_bytes: 4096,
        extent_bytes: 4 << 20,
    };

    #[test]
    fn parameters_satisfy_the_kernel_checks() {
        for block in [512u32, 1024, 2048, 4096] {
            for extent in [4096, 64 << 10, 4 << 20, 1 << 30, 12345] {
                let p = device_params(
                    Geometry {
                        block_bytes: block,
                        extent_bytes: extent,
                        ..G
                    },
                    1 << 20,
                    false,
                );
                // ublk_validate_params: 9 <= logical <= PAGE_SHIFT, logical <= physical.
                assert!((9..=12).contains(&p.logical_bs_shift));
                assert!(p.logical_bs_shift <= p.physical_bs_shift);
                assert!(p.io_opt_shift >= p.io_min_shift && p.io_opt_shift <= 25);
                assert_eq!(p.max_sectors, 2048);
            }
        }
        let p = device_params(G, 4 << 20, false);
        assert_eq!((p.logical_bs_shift, p.io_opt_shift), (12, 22));
        assert_eq!(p.dev_sectors, 1 << 21);
        assert_eq!(p.max_write_zeroes_sectors, 8192);
        assert_eq!(device_params(G, 4096, true).max_write_zeroes_sectors, 0);
    }

    #[test]
    fn io_buffer_sizes() {
        assert!(check_io_bytes(4096).is_ok());
        assert!(check_io_bytes(MAX_IO_BYTES).is_ok());
        for bad in [0, 512, 6144, MAX_IO_BYTES + 4096] {
            assert!(check_io_bytes(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn results() {
        assert_eq!(completion(Op::Read, 4, &Ok(vec![0; 4])), 4);
        assert_eq!(completion(Op::Read, 4, &Ok(vec![0; 3])), -libc::EIO);
        assert_eq!(completion(Op::Write, 4096, &Ok(Vec::new())), 4096);
        assert_eq!(completion(Op::WriteZeroes, 8192, &Ok(Vec::new())), 8192);
        assert_eq!(completion(Op::Flush, 0, &Ok(Vec::new())), 0);
        assert_eq!(completion(Op::Write, 4096, &Err(libc::EROFS)), -libc::EROFS);
        assert_eq!(completion(Op::Write, 4096, &Err(0)), -libc::EIO);
        assert_eq!(completion(Op::Write, 1 << 31, &Ok(Vec::new())), -libc::EIO);
    }
}
