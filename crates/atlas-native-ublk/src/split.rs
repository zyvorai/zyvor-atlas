// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! How one block request becomes cluster requests.
//!
//! The cluster writes a request made of whole extents without reading anything back (and outside
//! its per-group write lock while the data travels); anything else is a read-modify-write of each
//! extent touched. So a write that covers at least one whole extent is cut at the extent
//! boundaries around its whole-extent middle.

/// `(offset, len)` of consecutive pieces covering `[offset, offset + len)`, each at most `max`.
fn chunks(offset: u64, len: u64, max: u64) -> impl Iterator<Item = (u64, usize)> {
    let max = max.max(1);
    (0..len.div_ceil(max)).map(move |i| {
        let at = i * max;
        (offset + at, (len - at).min(max) as usize)
    })
}

/// Pieces of a read: only the request limit matters.
pub fn read_pieces(offset: u64, len: u64, max: u64) -> Vec<(u64, usize)> {
    chunks(offset, len, max).collect()
}

/// Pieces of a write under an extent grid of `extent` bytes and a request limit of `max` bytes.
/// With `max` below `extent` no request can carry a whole extent, so only the limit applies.
pub fn write_pieces(offset: u64, len: u64, extent: u64, max: u64) -> Vec<(u64, usize)> {
    let end = offset + len;
    let whole = (extent > 0 && max >= extent).then(|| {
        let start = offset.next_multiple_of(extent);
        let stop = end - end % extent;
        (start, stop)
    });
    match whole {
        Some((start, stop)) if start < stop => chunks(offset, start - offset, max)
            .chain(chunks(start, stop - start, max - max % extent))
            .chain(chunks(stop, end - stop, max))
            .collect(),
        _ => chunks(offset, len, max).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const E: u64 = 64 << 10;

    fn covers(pieces: &[(u64, usize)], offset: u64, len: u64, max: u64) {
        let mut at = offset;
        for (o, l) in pieces {
            assert_eq!(*o, at, "{pieces:?}");
            assert!(*l > 0 && *l as u64 <= max, "{pieces:?}");
            at += *l as u64;
        }
        assert_eq!(at, offset + len, "{pieces:?}");
    }

    #[test]
    fn small_writes_are_one_request() {
        assert_eq!(write_pieces(4096, 4096, E, 8 * E), [(4096, 4096)]);
        // Straddling a boundary without a whole extent: still one request.
        assert_eq!(write_pieces(E - 4096, 8192, E, 8 * E), [(E - 4096, 8192)]);
    }

    #[test]
    fn whole_extents_go_on_their_own() {
        assert_eq!(
            write_pieces(E - 4096, 2 * E + 8192, E, 8 * E),
            [(E - 4096, 4096), (E, 2 * E as usize), (3 * E, 4096)]
        );
        assert_eq!(write_pieces(E, E, E, 8 * E), [(E, E as usize)]);
        // The middle is cut at multiples of the extent even when the limit is not one.
        let p = write_pieces(0, 5 * E, E, 2 * E + 4096);
        assert_eq!(
            p,
            [
                (0, 2 * E as usize),
                (2 * E, 2 * E as usize),
                (4 * E, E as usize)
            ]
        );
    }

    #[test]
    fn every_split_covers_its_range_within_the_limit() {
        for extent in [4096, E] {
            for max in [4096, 12288, E, 3 * E + 4096] {
                for offset in (0..3 * E).step_by(12288) {
                    for len in [4096, 8192, E, E + 4096, 4 * E - 4096] {
                        covers(&write_pieces(offset, len, extent, max), offset, len, max);
                        covers(&read_pieces(offset, len, max), offset, len, max);
                    }
                }
            }
        }
        assert!(read_pieces(0, 0, E).is_empty());
        assert!(write_pieces(0, 0, E, E).is_empty());
    }
}
