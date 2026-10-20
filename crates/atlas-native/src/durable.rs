// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use std::{
    fs::{self, File},
    io::Write,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Replaces `path` with `bytes` so that a crash leaves either the old or the new contents:
/// write a sibling temp file, fsync it, rename over the target, then fsync the directory.
///
/// The temp name is unique per call (pid + counter) so concurrent writers to the same path do
/// not clobber each other's temp file, and the temp file is removed if the write or rename fails.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = Path::new(&tmp);
    let written = (|| {
        let mut f = File::create(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(tmp, path)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(tmp);
        return Err(e);
    }
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

pub(crate) fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(all(test, unix))]
mod tests {
    use super::write_atomic;
    use std::fs;

    #[test]
    fn replaces_contents_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("catalog.json");
        write_atomic(&p, b"a much longer original payload").unwrap();
        write_atomic(&p, b"short").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"short");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_rename_removes_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("target");
        fs::create_dir(&p).unwrap(); // rename file -> existing directory fails
        assert!(write_atomic(&p, b"x").is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "temp file leaked");
    }

    #[test]
    fn concurrent_writers_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state");
        std::thread::scope(|s| {
            for i in 0..8u8 {
                let p = &p;
                s.spawn(move || {
                    for _ in 0..20 {
                        write_atomic(p, &[i; 64]).unwrap();
                    }
                });
            }
        });
        let got = fs::read(&p).unwrap();
        assert_eq!(got.len(), 64);
        assert!(got.iter().all(|b| *b == got[0]), "torn write");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
