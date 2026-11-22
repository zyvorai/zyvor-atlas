// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Directory listings for the kernel, one page from the node at a time. The kernel reads a
//! directory in several calls, each resuming at the offset the last entry it took carried; an
//! open directory keeps the page it is in and the name to continue after, so a directory of any
//! size costs one request per page instead of one listing per call.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, PoisonError,
    },
};

use atlas_native::{engine::DirEntry, NodeType};

/// Entries asked of the node per request.
pub const PAGE: usize = 1024;

/// What a page fetch returns: the visible entries and the name to continue after (`None` once
/// the directory is done).
pub type Page = (Vec<DirEntry>, Option<String>);

enum Next {
    Start,
    After(String),
    Done,
}

/// One open directory. Offsets count `.` and `..` first, then entries in name order; the offset
/// handed out with an entry is the one to resume after it.
pub struct DirStream {
    ino: u64,
    /// Offset of `buf[0]`.
    base: u64,
    buf: Vec<(u64, NodeType, String)>,
    next: Next,
    started: bool,
}

impl DirStream {
    pub fn new(ino: u64) -> Self {
        Self {
            ino,
            base: 0,
            buf: Vec::new(),
            next: Next::Done,
            started: false,
        }
    }

    fn rewind(&mut self) {
        self.base = 0;
        self.buf = vec![
            (self.ino, NodeType::Dir, ".".into()),
            (self.ino, NodeType::Dir, "..".into()),
        ];
        self.next = Next::Start;
        self.started = true;
    }

    /// Hands `add` the entries from `offset` on, as `(ino, kind, name, offset of the next)`,
    /// until it reports its buffer full or the directory ends. `fetch` reads the page after a
    /// name (`None`: the first page). Seeking back restarts the listing from the top.
    pub fn read<E>(
        &mut self,
        mut offset: u64,
        mut fetch: impl FnMut(Option<&str>) -> Result<Page, E>,
        mut add: impl FnMut(u64, NodeType, &str, u64) -> bool,
    ) -> Result<(), E> {
        if !self.started || offset < self.base {
            self.rewind();
        }
        loop {
            while offset >= self.base + self.buf.len() as u64 {
                let after = match &self.next {
                    Next::Done => return Ok(()),
                    Next::Start => None,
                    Next::After(name) => Some(name.as_str()),
                };
                let (entries, next) = fetch(after)?;
                self.base += self.buf.len() as u64;
                self.buf = entries
                    .into_iter()
                    .map(|e| (e.ino, e.kind, e.name))
                    .collect();
                self.next = next.map_or(Next::Done, Next::After);
            }
            let start = (offset - self.base) as usize;
            for (i, (ino, kind, name)) in self.buf.iter().enumerate().skip(start) {
                let off = self.base + i as u64 + 1;
                if add(*ino, *kind, name, off) {
                    return Ok(());
                }
            }
            offset = self.base + self.buf.len() as u64;
        }
    }
}

/// The open directories of a mount, by file handle.
#[derive(Default)]
pub struct DirStreams {
    next_fh: AtomicU64,
    open: Mutex<HashMap<u64, Arc<Mutex<DirStream>>>>,
}

impl DirStreams {
    /// Opens `ino`; returns its file handle (never 0).
    pub fn open(&self, ino: u64) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1;
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(fh, Arc::new(Mutex::new(DirStream::new(ino))));
        fh
    }

    /// The stream behind `fh`, or a fresh one for a handle this mount didn't open.
    pub fn get(&self, fh: u64, ino: u64) -> Arc<Mutex<DirStream>> {
        let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        match open.get(&fh) {
            Some(s) => s.clone(),
            None => Arc::new(Mutex::new(DirStream::new(ino))),
        }
    }

    pub fn close(&self, fh: u64) {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&fh);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of `n` files paged `page` at a time; counts the fetches.
    fn dir(n: usize, page: usize) -> (Vec<String>, impl FnMut(Option<&str>) -> Result<Page, ()>) {
        let names: Vec<String> = (0..n).map(|i| format!("f{i:05}")).collect();
        let all = names.clone();
        let fetch = move |after: Option<&str>| {
            let from = after.map_or(0, |a| all.partition_point(|n| n.as_str() <= a));
            let entries: Vec<DirEntry> = all[from..]
                .iter()
                .take(page)
                .enumerate()
                .map(|(i, name)| DirEntry {
                    name: name.clone(),
                    ino: (from + i + 2) as u64,
                    kind: NodeType::File,
                })
                .collect();
            let next = (entries.len() == page)
                .then(|| entries.last().map(|e| e.name.clone()))
                .flatten();
            Ok((entries, next))
        };
        (names, fetch)
    }

    /// Reads the whole stream as the kernel does: calls of at most `room` entries, each resuming
    /// at the last offset taken.
    fn read_all(
        s: &mut DirStream,
        fetch: &mut impl FnMut(Option<&str>) -> Result<Page, ()>,
        room: usize,
    ) -> Vec<String> {
        let mut out = Vec::new();
        let mut offset = 0;
        loop {
            let mut got = 0;
            s.read(offset, &mut *fetch, |_, _, name, off| {
                out.push(name.to_string());
                offset = off;
                got += 1;
                got == room
            })
            .unwrap();
            if got == 0 {
                return out;
            }
        }
    }

    #[test]
    fn a_large_directory_is_read_a_page_at_a_time() {
        let (names, mut fetch) = dir(2500, 1000);
        let mut fetches = 0;
        let mut counted = |a: Option<&str>| {
            fetches += 1;
            fetch(a)
        };
        let mut s = DirStream::new(1);
        let got = read_all(&mut s, &mut counted, 37);
        let mut want = vec![".".to_string(), "..".to_string()];
        want.extend(names);
        assert_eq!(got, want);
        // Three pages, the last one short; no listing is fetched twice.
        assert_eq!(fetches, 3);
    }

    #[test]
    fn seeking_back_restarts_and_an_empty_directory_has_only_dots() {
        let (_, mut fetch) = dir(10, 4);
        let mut s = DirStream::new(1);
        let first = read_all(&mut s, &mut fetch, 3);
        assert_eq!(first.len(), 12);
        let again = read_all(&mut s, &mut fetch, 100);
        assert_eq!(again, first);
        // Resuming mid-way after a rewind lands on the same entry.
        let mut at5 = Vec::new();
        s.read(5, &mut fetch, |_, _, n, _| {
            at5.push(n.to_string());
            true
        })
        .unwrap();
        assert_eq!(at5, [first[5].clone()]);

        let (_, mut empty) = dir(0, 4);
        let mut e = DirStream::new(7);
        assert_eq!(read_all(&mut e, &mut empty, 10), [".", ".."]);
    }

    #[test]
    fn a_page_of_only_hidden_entries_moves_on_to_the_next() {
        let mut calls = 0;
        let mut fetch = |after: Option<&str>| -> Result<Page, ()> {
            calls += 1;
            Ok(match after {
                None => (Vec::new(), Some("hidden-last".into())),
                Some(_) => (
                    vec![DirEntry {
                        name: "visible".into(),
                        ino: 9,
                        kind: NodeType::File,
                    }],
                    None,
                ),
            })
        };
        let mut s = DirStream::new(1);
        assert_eq!(read_all(&mut s, &mut fetch, 10), [".", "..", "visible"]);
        assert_eq!(calls, 2);
    }

    #[test]
    fn handles_are_distinct_and_unknown_ones_get_a_fresh_stream() {
        let d = DirStreams::default();
        let (a, b) = (d.open(1), d.open(2));
        assert!(a != 0 && b != 0 && a != b);
        assert!(Arc::ptr_eq(&d.get(a, 1), &d.get(a, 1)));
        d.close(a);
        assert!(!Arc::ptr_eq(&d.get(a, 1), &d.get(a, 1)));
    }
}
