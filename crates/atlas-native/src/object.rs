// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Object storage for tiered extents: a directory ([`DirObjectStore`]) or, with the `s3`
//! feature, any S3-compatible bucket ([`S3ObjectStore`], Ceph RGW by default) through
//! `atlas_driver_rgw::S3Target`.

use std::{
    fmt::Debug,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Whole-object get/put/delete/list. Implementations must be safe to call from many threads.
pub trait ObjectStore: Send + Sync + Debug {
    /// Stores `data` under `key`, replacing any object there. Durable when it returns.
    fn put(&self, key: &str, data: &[u8]) -> io::Result<()>;
    /// The object's bytes; `NotFound` if there is none.
    fn get(&self, key: &str) -> io::Result<Vec<u8>>;
    /// Removes the object; removing a missing object is not an error.
    fn delete(&self, key: &str) -> io::Result<()>;
    /// Keys starting with `prefix`, in no particular order.
    fn list(&self, prefix: &str) -> io::Result<Vec<String>>;
}

/// Keys are relative paths with `/`-separated segments; `.`/`..`/empty segments are refused.
fn key_path(root: &Path, key: &str) -> io::Result<PathBuf> {
    let mut p = root.to_path_buf();
    for seg in key.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.contains('\\') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid object key {key:?}"),
            ));
        }
        p.push(seg);
    }
    Ok(p)
}

/// Objects as files under a directory (a shared filesystem, or tests).
#[derive(Debug, Clone)]
pub struct DirObjectStore {
    root: PathBuf,
}

impl DirObjectStore {
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        fs::create_dir_all(root.as_ref())?;
        Ok(Self {
            root: root.as_ref().to_path_buf(),
        })
    }

    fn walk(&self, dir: &Path, rel: &str, out: &mut Vec<String>) -> io::Result<()> {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name.starts_with(".tmp-") {
                continue;
            }
            let key = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if entry.file_type()?.is_dir() {
                self.walk(&entry.path(), &key, out)?;
            } else {
                out.push(key);
            }
        }
        Ok(())
    }
}

impl ObjectStore for DirObjectStore {
    fn put(&self, key: &str, data: &[u8]) -> io::Result<()> {
        let path = key_path(&self.root, key)?;
        let dir = path.parent().unwrap_or(&self.root);
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, &path)?;
        fs::File::open(dir)?.sync_all()
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        fs::read(key_path(&self.root, key)?)
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        match fs::remove_file(key_path(&self.root, key)?) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        self.walk(&self.root, "", &mut out)?;
        out.retain(|k| k.starts_with(prefix));
        Ok(out)
    }
}

#[cfg(feature = "s3")]
pub use s3::S3ObjectStore;

#[cfg(feature = "s3")]
mod s3 {
    use std::io;

    use atlas_driver_rgw::S3Target;

    use super::ObjectStore;

    /// A bucket on an S3-compatible endpoint (Ceph RGW, MinIO, AWS, ...). Calls block the
    /// calling thread on a small runtime owned by the store.
    pub struct S3ObjectStore {
        target: S3Target,
        rt: tokio::runtime::Runtime,
        bucket: String,
    }

    impl std::fmt::Debug for S3ObjectStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("S3ObjectStore")
                .field("bucket", &self.bucket)
                .finish_non_exhaustive()
        }
    }

    /// Drops the query string of every URL in `msg`: request errors quote the presigned URL,
    /// whose credential and signature must not reach logs or API responses.
    pub(super) fn redact(msg: &str) -> String {
        let mut out = String::with_capacity(msg.len());
        let mut rest = msg;
        while let Some(i) = rest.find("http") {
            let (head, url) = rest.split_at(i);
            out.push_str(head);
            let end = url
                .find(|c: char| c.is_whitespace() || c == ')' || c == '"')
                .unwrap_or(url.len());
            let (u, tail) = url.split_at(end);
            out.push_str(u.split('?').next().unwrap_or(u));
            rest = tail;
        }
        out.push_str(rest);
        out
    }

    fn other(e: anyhow::Error) -> io::Error {
        io::Error::other(redact(&format!("{e:#}")))
    }

    impl S3ObjectStore {
        pub fn new(
            endpoint: &str,
            region: &str,
            bucket: &str,
            access_key: &str,
            secret_key: &str,
        ) -> io::Result<Self> {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("atlas-native-s3")
                .enable_all()
                .build()?;
            // reqwest wants a runtime context while it builds its client.
            let target = {
                let _g = rt.enter();
                S3Target::new(endpoint, region, bucket, access_key, secret_key).map_err(other)?
            };
            Ok(Self {
                target,
                rt,
                bucket: bucket.to_string(),
            })
        }
    }

    impl ObjectStore for S3ObjectStore {
        fn put(&self, key: &str, data: &[u8]) -> io::Result<()> {
            self.rt
                .block_on(self.target.put_object(key, data.to_vec()))
                .map_err(other)
        }

        fn get(&self, key: &str) -> io::Result<Vec<u8>> {
            self.rt.block_on(self.target.get_object(key)).map_err(|e| {
                let msg = redact(&format!("{e:#}"));
                if msg.contains("HTTP 404") {
                    io::Error::new(io::ErrorKind::NotFound, msg)
                } else {
                    io::Error::other(msg)
                }
            })
        }

        fn delete(&self, key: &str) -> io::Result<()> {
            self.rt
                .block_on(self.target.delete_object(key))
                .map_err(other)
        }

        fn list(&self, prefix: &str) -> io::Result<Vec<String>> {
            let objects = self
                .rt
                .block_on(self.target.list_objects(Some(prefix)))
                .map_err(other)?;
            Ok(objects.into_iter().map(|(k, _)| k).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "s3")]
    #[test]
    fn presigned_urls_are_cut_from_errors() {
        let msg = "list objects: error sending request for url \
                   (http://rgw:80/b/?X-Amz-Credential=AK%2F1&X-Amz-Signature=deadbeef): refused";
        assert_eq!(
            s3::redact(msg),
            "list objects: error sending request for url (http://rgw:80/b/): refused"
        );
        assert_eq!(
            s3::redact("GET k failed: HTTP 404"),
            "GET k failed: HTTP 404"
        );
    }

    #[test]
    fn a_directory_store_round_trips_lists_and_deletes() {
        let td = tempfile::tempdir().unwrap();
        let s = DirObjectStore::new(td.path()).unwrap();
        s.put("p/extents/a", b"one").unwrap();
        s.put("p/extents/b", b"two").unwrap();
        s.put("q/c", b"three").unwrap();
        s.put("p/extents/a", b"uno").unwrap();
        assert_eq!(s.get("p/extents/a").unwrap(), b"uno");
        let mut keys = s.list("p/").unwrap();
        keys.sort();
        assert_eq!(keys, ["p/extents/a", "p/extents/b"]);
        s.delete("p/extents/a").unwrap();
        s.delete("p/extents/a").unwrap();
        assert_eq!(
            s.get("p/extents/a").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        for bad in ["../x", "a//b", "/abs", "a/./b", ""] {
            assert!(s.put(bad, b"x").is_err(), "{bad} accepted");
        }
    }
}
