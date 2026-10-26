// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `image.import`: stream a raw disk image over HTTP(S) into an atlas-native volume.
//!
//! The image is read in 4 MiB blocks. Each block is hashed, and written unless it is all zeros (a
//! volume reads never-written bytes as zeros, so sparse images stay sparse). Progress is recorded
//! on the job every 64 MiB, so a retried or re-submitted job resumes from `bytes_done` instead of
//! starting over. When the stream ends the SHA-256 of what was received is compared with the
//! caller's, and then the volume is read back and hashed again: the second comparison is what
//! shows the bytes on the volume, not only the bytes on the wire, match.
//!
//! See `docs/IMAGE_IMPORT.md` for the API and its limits.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use atlas_driver_core::{DriverRegistry, StorageDriver};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::AnyPool;

use crate::spec::JobSpec;

/// Bytes hashed and written at a time; one atlas-native extent by default.
pub(crate) const CHUNK: usize = 4 * 1024 * 1024;
/// Persist `bytes_done` after this many newly written bytes.
const PROGRESS_EVERY: u64 = 64 * 1024 * 1024;
/// Give up on a source that sends nothing for this long.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

static DRIVERS: OnceLock<Arc<DriverRegistry>> = OnceLock::new();

/// Give the job worker access to the gateway's storage drivers. Called once at startup; only
/// `image.import` needs it, because it writes volume data directly.
pub fn set_driver_registry(registry: Arc<DriverRegistry>) {
    let _ = DRIVERS.set(registry);
}

// ---- the pieces `run` is generic over, so the algorithm is testable without a cluster ----

/// Byte-addressed access to one volume.
pub(crate) trait VolumeIo {
    /// `len` bytes at `offset`; never-written bytes read as zeros.
    async fn read(&self, offset: u64, len: u64) -> Result<Vec<u8>>;
    async fn write(&self, offset: u64, data: Vec<u8>) -> Result<()>;
}

/// An image being streamed in.
pub(crate) trait Source {
    /// Start the stream at byte `from`. Returns the length of the whole image when it is known.
    async fn open(&mut self, from: u64) -> Result<Option<u64>>;
    /// The next piece, or `None` at the end of the image.
    async fn next(&mut self) -> Result<Option<Vec<u8>>>;
}

/// Where progress is recorded.
pub(crate) trait Progress {
    async fn update(&self, bytes_done: u64, total: Option<u64>) -> Result<()>;
}

pub(crate) enum ImportError {
    /// The data does not hash to what the caller said. Recorded progress must be thrown away,
    /// because some of what was written may be wrong.
    Mismatch {
        stage: &'static str,
        expected: String,
        actual: String,
    },
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for ImportError {
    fn from(e: anyhow::Error) -> Self {
        Self::Failed(e)
    }
}

pub(crate) struct Outcome {
    pub bytes: u64,
    pub sha256: String,
    pub zero_bytes_skipped: u64,
}

fn is_zero(block: &[u8]) -> bool {
    block.iter().all(|b| *b == 0)
}

/// Hash `[from, to)` of the volume, reading `CHUNK` at a time.
async fn hash_volume<V: VolumeIo>(
    io: &V,
    hasher: &mut Sha256,
    from: u64,
    to: u64,
) -> Result<()> {
    let mut off = from;
    while off < to {
        let n = (to - off).min(CHUNK as u64);
        let block = io
            .read(off, n)
            .await
            .with_context(|| format!("read back {n} bytes at offset {off}"))?;
        if block.len() as u64 != n {
            bail!("short read at offset {off}: wanted {n} bytes, got {}", block.len());
        }
        hasher.update(&block);
        off += n;
    }
    Ok(())
}

pub(crate) async fn run<V: VolumeIo, S: Source, P: Progress>(
    io: &V,
    src: &mut S,
    progress: &P,
    expected_sha256: &str,
    size_hint: Option<u64>,
    start: u64,
) -> Result<Outcome, ImportError> {
    let expected = expected_sha256.to_ascii_lowercase();
    let mut hasher = Sha256::new();
    // What an earlier attempt already wrote is part of the image, so it goes through the hash too;
    // reading it back also confirms it is still there.
    hash_volume(io, &mut hasher, 0, start).await?;

    let total = src.open(start).await?;
    let mut offset = start;
    let mut since_progress = 0u64;
    let mut skipped = 0u64;
    let mut buf: Vec<u8> = Vec::with_capacity(CHUNK);

    loop {
        let piece = src.next().await?;
        let at_end = piece.is_none();
        if let Some(p) = piece {
            buf.extend_from_slice(&p);
        }
        while buf.len() >= CHUNK || (at_end && !buf.is_empty()) {
            let take = buf.len().min(CHUNK);
            let block: Vec<u8> = buf.drain(..take).collect();
            hasher.update(&block);
            let len = block.len() as u64;
            if is_zero(&block) {
                skipped += len;
            } else {
                io.write(offset, block)
                    .await
                    .with_context(|| format!("write {len} bytes at offset {offset}"))?;
            }
            offset += len;
            since_progress += len;
            if since_progress >= PROGRESS_EVERY {
                progress.update(offset, total).await?;
                since_progress = 0;
            }
        }
        if at_end {
            break;
        }
    }

    if let Some(want) = size_hint.or(total) {
        if offset != want {
            return Err(ImportError::Failed(anyhow!(
                "the image is {offset} bytes but {want} were expected (truncated or wrong file)"
            )));
        }
    }
    let streamed = hex::encode(hasher.finalize());
    if streamed != expected {
        return Err(ImportError::Mismatch {
            stage: "source",
            expected,
            actual: streamed,
        });
    }

    // The stream was right; now check the volume holds it.
    let mut check = Sha256::new();
    hash_volume(io, &mut check, 0, offset).await?;
    let stored = hex::encode(check.finalize());
    if stored != expected {
        return Err(ImportError::Mismatch {
            stage: "readback",
            expected,
            actual: stored,
        });
    }
    Ok(Outcome {
        bytes: offset,
        sha256: stored,
        zero_bytes_skipped: skipped,
    })
}

// ---- the real pieces ----

/// A volume on a storage driver.
struct DriverIo {
    driver: Arc<dyn StorageDriver>,
    native_id: String,
}

impl VolumeIo for DriverIo {
    async fn read(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
        self.driver
            .read_volume(&self.native_id, offset, len)
            .await
            .map_err(|e| anyhow!("{e}"))
    }
    async fn write(&self, offset: u64, data: Vec<u8>) -> Result<()> {
        self.driver
            .write_volume(&self.native_id, offset, data)
            .await
            .map_err(|e| anyhow!("{e}"))
    }
}

/// An image served over HTTP(S). Redirects are refused: the gateway checked the host before the
/// job was created, and following a redirect would let that host send it somewhere else.
pub(crate) struct HttpSource {
    client: reqwest::Client,
    url: String,
    resp: Option<reqwest::Response>,
    /// Bytes to drop from the start of the body, when the server ignored the `Range` request.
    discard: u64,
}

impl HttpSource {
    pub(crate) fn new(url: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .build()
            .context("build HTTP client")?;
        Ok(Self {
            client,
            url: url.to_string(),
            resp: None,
            discard: 0,
        })
    }
}

/// `bytes a-b/total` -> `(a, total)`; `total` is `None` for `*`.
fn parse_content_range(v: &str) -> Option<(u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (a, _b) = range.split_once('-')?;
    Some((a.parse().ok()?, total.parse().ok()))
}

impl Source for HttpSource {
    async fn open(&mut self, from: u64) -> Result<Option<u64>> {
        let mut req = self.client.get(&self.url);
        if from > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={from}-"));
        }
        let resp = req.send().await.context("request the image")?;
        let status = resp.status();
        let total = match status.as_u16() {
            206 => {
                let cr = resp
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_content_range)
                    .ok_or_else(|| anyhow!("206 response without a usable Content-Range"))?;
                if cr.0 != from {
                    bail!("server resumed at byte {} instead of {from}", cr.0);
                }
                cr.1.or_else(|| resp.content_length().map(|l| from + l))
            }
            200 => {
                // No range support: stream from the start and drop what is already written.
                self.discard = from;
                resp.content_length()
            }
            _ => bail!("the image server answered {status}"),
        };
        self.resp = Some(resp);
        Ok(total)
    }

    async fn next(&mut self) -> Result<Option<Vec<u8>>> {
        let resp = self.resp.as_mut().ok_or_else(|| anyhow!("stream not opened"))?;
        loop {
            let chunk = tokio::time::timeout(READ_TIMEOUT, resp.chunk())
                .await
                .map_err(|_| anyhow!("the image server sent nothing for {READ_TIMEOUT:?}"))?
                .context("read the image")?;
            let Some(chunk) = chunk else {
                return Ok(None);
            };
            if self.discard == 0 {
                return Ok(Some(chunk.to_vec()));
            }
            let drop_n = (self.discard).min(chunk.len() as u64) as usize;
            self.discard -= drop_n as u64;
            if drop_n < chunk.len() {
                return Ok(Some(chunk[drop_n..].to_vec()));
            }
        }
    }
}

/// Records progress on the job row.
struct JobProgress<'a> {
    pool: &'a AnyPool,
    job_id: &'a str,
    volume_id: &'a str,
}

impl Progress for JobProgress<'_> {
    async fn update(&self, bytes_done: u64, total: Option<u64>) -> Result<()> {
        // 5% is where `mark_running` starts and 100% belongs to success, so stay in between.
        let pct = total
            .filter(|t| *t > 0)
            .map_or(5, |t| (bytes_done.saturating_mul(100) / t).clamp(5, 99) as i64);
        atlas_inventory::jobs::set_progress_result(
            self.pool,
            self.job_id,
            pct,
            &json!({
                "volume_id": self.volume_id,
                "bytes_done": bytes_done,
                "total_bytes": total,
            }),
        )
        .await
    }
}

pub(crate) async fn dispatch_image_import(
    pool: &AnyPool,
    job_id: &str,
    spec: JobSpec,
) -> Result<serde_json::Value> {
    let JobSpec::ImageImport {
        volume_id,
        backend_id,
        native_id,
        url,
        sha256,
        size_bytes,
        resume_from,
    } = spec
    else {
        bail!("not an image.import job");
    };
    let driver = DRIVERS
        .get()
        .and_then(|r| r.get(&backend_id))
        .ok_or_else(|| anyhow!("storage backend {backend_id} is not available"))?;
    let io = DriverIo { driver, native_id };

    // A retry resumes from what the last attempt recorded; a new job can be pointed at an earlier
    // job's progress with `resume_from`. Round down to a block boundary to be safe.
    let recorded = atlas_inventory::jobs::get_job(pool, job_id)
        .await?
        .and_then(|j| j.result.get("bytes_done").and_then(|v| v.as_u64()))
        .unwrap_or(0);
    let start = recorded.max(resume_from) / CHUNK as u64 * CHUNK as u64;
    if start > 0 {
        tracing::info!(job = %job_id, start, "resuming image import");
    }

    let mut src = HttpSource::new(&url)?;
    let progress = JobProgress {
        pool,
        job_id,
        volume_id: &volume_id,
    };
    match run(&io, &mut src, &progress, &sha256, size_bytes, start).await {
        Ok(o) => Ok(json!({
            "volume_id": volume_id,
            "bytes": o.bytes,
            "sha256": o.sha256,
            "verified": true,
            "zero_bytes_skipped": o.zero_bytes_skipped,
            "resumed_from": start,
        })),
        Err(ImportError::Mismatch {
            stage,
            expected,
            actual,
        }) => {
            // Some of what was written may be wrong, so a retry must not trust it.
            let _ = atlas_inventory::jobs::set_progress_result(
                pool,
                job_id,
                5,
                &json!({ "volume_id": volume_id, "bytes_done": 0 }),
            )
            .await;
            bail!("checksum mismatch ({stage}): expected {expected}, got {actual}")
        }
        Err(ImportError::Failed(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An in-memory volume. `corrupt_at` flips a bit in whatever is written across that offset.
    struct MemVolume {
        data: Mutex<Vec<u8>>,
        writes: Mutex<Vec<(u64, usize)>>,
        corrupt_at: Option<u64>,
    }

    impl MemVolume {
        fn new(size: usize) -> Self {
            Self {
                data: Mutex::new(vec![0; size]),
                writes: Mutex::new(Vec::new()),
                corrupt_at: None,
            }
        }
    }

    impl VolumeIo for MemVolume {
        async fn read(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
            let d = self.data.lock().unwrap();
            let end = (offset + len) as usize;
            anyhow::ensure!(end <= d.len(), "read past the end");
            Ok(d[offset as usize..end].to_vec())
        }
        async fn write(&self, offset: u64, mut data: Vec<u8>) -> Result<()> {
            if let Some(c) = self.corrupt_at {
                if c >= offset && c < offset + data.len() as u64 {
                    data[(c - offset) as usize] ^= 1;
                }
            }
            self.writes.lock().unwrap().push((offset, data.len()));
            let mut d = self.data.lock().unwrap();
            let end = offset as usize + data.len();
            anyhow::ensure!(end <= d.len(), "write past the end of the volume");
            d[offset as usize..end].copy_from_slice(&data);
            Ok(())
        }
    }

    /// Serves `image` in `piece`-byte pieces from the requested offset.
    struct MemSource {
        image: Vec<u8>,
        piece: usize,
        pos: usize,
        known_total: bool,
    }

    impl MemSource {
        fn new(image: Vec<u8>, piece: usize) -> Self {
            Self {
                image,
                piece,
                pos: 0,
                known_total: true,
            }
        }
    }

    impl Source for MemSource {
        async fn open(&mut self, from: u64) -> Result<Option<u64>> {
            self.pos = from as usize;
            Ok(self.known_total.then_some(self.image.len() as u64))
        }
        async fn next(&mut self) -> Result<Option<Vec<u8>>> {
            if self.pos >= self.image.len() {
                return Ok(None);
            }
            let end = (self.pos + self.piece).min(self.image.len());
            let out = self.image[self.pos..end].to_vec();
            self.pos = end;
            Ok(Some(out))
        }
    }

    struct Recorder(Mutex<Vec<u64>>);
    impl Progress for Recorder {
        async fn update(&self, bytes_done: u64, _total: Option<u64>) -> Result<()> {
            self.0.lock().unwrap().push(bytes_done);
            Ok(())
        }
    }

    fn recorder() -> Recorder {
        Recorder(Mutex::new(Vec::new()))
    }

    fn pseudo(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8 | 1 // never zero, so no block is accidentally sparse
            })
            .collect()
    }

    fn sha(data: &[u8]) -> String {
        hex::encode(Sha256::digest(data))
    }

    const MIB: usize = 1024 * 1024;

    #[tokio::test]
    async fn imports_an_image_and_verifies_it_by_reading_the_volume_back() {
        let image = pseudo(9 * MIB + 123, 1); // two full blocks and a ragged tail
        let vol = MemVolume::new(10 * MIB);
        let mut src = MemSource::new(image.clone(), 700_000);
        let out = run(&vol, &mut src, &recorder(), &sha(&image), None, 0)
            .await
            .map_err(|_| "import failed")
            .unwrap();
        assert_eq!(out.bytes, image.len() as u64);
        assert_eq!(out.sha256, sha(&image));
        assert_eq!(&vol.data.lock().unwrap()[..image.len()], &image[..]);
    }

    #[tokio::test]
    async fn all_zero_blocks_are_not_written_but_still_hashed() {
        let mut image = pseudo(12 * MIB, 2);
        image[4 * MIB..8 * MIB].fill(0); // the middle block is sparse
        let vol = MemVolume::new(12 * MIB);
        let mut src = MemSource::new(image.clone(), MIB);
        let out = run(&vol, &mut src, &recorder(), &sha(&image), None, 0)
            .await
            .map_err(|_| "import failed")
            .unwrap();
        assert_eq!(out.zero_bytes_skipped, (4 * MIB) as u64);
        let writes = vol.writes.lock().unwrap().clone();
        assert_eq!(writes, vec![(0, 4 * MIB), (8 * MIB as u64, 4 * MIB)]);
        assert_eq!(&*vol.data.lock().unwrap(), &image);
    }

    #[tokio::test]
    async fn resumes_from_what_is_already_on_the_volume() {
        let image = pseudo(12 * MIB, 3);
        let vol = MemVolume::new(12 * MIB);
        // An earlier attempt got two blocks in.
        vol.data.lock().unwrap()[..8 * MIB].copy_from_slice(&image[..8 * MIB]);
        let mut src = MemSource::new(image.clone(), MIB);
        let out = run(&vol, &mut src, &recorder(), &sha(&image), None, (8 * MIB) as u64)
            .await
            .map_err(|_| "import failed")
            .unwrap();
        assert_eq!(out.bytes, image.len() as u64);
        // Only the last block was written this time.
        assert_eq!(vol.writes.lock().unwrap().clone(), vec![(8 * MIB as u64, 4 * MIB)]);
    }

    #[tokio::test]
    async fn resume_catches_a_volume_that_no_longer_matches_the_image() {
        let image = pseudo(12 * MIB, 4);
        let vol = MemVolume::new(12 * MIB);
        vol.data.lock().unwrap()[..8 * MIB].copy_from_slice(&image[..8 * MIB]);
        vol.data.lock().unwrap()[100] ^= 0xff; // damaged since the last attempt
        let mut src = MemSource::new(image.clone(), MIB);
        let err = run(&vol, &mut src, &recorder(), &sha(&image), None, (8 * MIB) as u64).await;
        assert!(matches!(
            err,
            Err(ImportError::Mismatch { stage: "source", .. })
        ));
    }

    #[tokio::test]
    async fn wrong_checksum_is_a_mismatch_not_a_success() {
        let image = pseudo(5 * MIB, 5);
        let vol = MemVolume::new(5 * MIB);
        let mut src = MemSource::new(image, MIB);
        let err = run(&vol, &mut src, &recorder(), &sha(b"something else"), None, 0).await;
        assert!(matches!(
            err,
            Err(ImportError::Mismatch { stage: "source", .. })
        ));
    }

    #[tokio::test]
    async fn readback_catches_a_volume_that_stored_the_wrong_bytes() {
        let image = pseudo(9 * MIB, 6);
        let mut vol = MemVolume::new(9 * MIB);
        vol.corrupt_at = Some((5 * MIB) as u64);
        let mut src = MemSource::new(image.clone(), MIB);
        let err = run(&vol, &mut src, &recorder(), &sha(&image), None, 0).await;
        assert!(matches!(
            err,
            Err(ImportError::Mismatch { stage: "readback", .. })
        ));
    }

    #[tokio::test]
    async fn a_short_image_is_an_error_even_when_its_hash_is_asked_for_anyway() {
        let image = pseudo(5 * MIB, 7);
        let vol = MemVolume::new(8 * MIB);
        let mut src = MemSource::new(image.clone(), MIB);
        // The caller says the image is 6 MiB; the server sent 5.
        let err = run(&vol, &mut src, &recorder(), &sha(&image), Some((6 * MIB) as u64), 0).await;
        assert!(matches!(err, Err(ImportError::Failed(_))));
    }

    #[tokio::test]
    async fn an_image_bigger_than_the_volume_fails_the_write() {
        let image = pseudo(9 * MIB, 8);
        let vol = MemVolume::new(4 * MIB);
        let mut src = MemSource::new(image.clone(), MIB);
        let err = run(&vol, &mut src, &recorder(), &sha(&image), None, 0).await;
        assert!(matches!(err, Err(ImportError::Failed(_))));
    }

    #[tokio::test]
    async fn progress_is_recorded_every_64_mib_at_block_boundaries() {
        let image = pseudo(130 * MIB, 9);
        let vol = MemVolume::new(130 * MIB);
        let rec = recorder();
        let mut src = MemSource::new(image.clone(), 8 * MIB);
        run(&vol, &mut src, &rec, &sha(&image), None, 0)
            .await
            .map_err(|_| "import failed")
            .unwrap();
        let marks = rec.0.lock().unwrap().clone();
        assert_eq!(marks, vec![(64 * MIB) as u64, (128 * MIB) as u64]);
        assert!(marks.iter().all(|m| m % CHUNK as u64 == 0));
    }

    #[tokio::test]
    async fn hash_is_case_insensitive() {
        let image = pseudo(MIB, 10);
        let vol = MemVolume::new(MIB);
        let mut src = MemSource::new(image.clone(), MIB);
        assert!(
            run(&vol, &mut src, &recorder(), &sha(&image).to_ascii_uppercase(), None, 0)
                .await
                .is_ok()
        );
    }

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range("bytes 100-199/1000"), Some((100, Some(1000))));
        assert_eq!(parse_content_range("bytes 0-9/*"), Some((0, None)));
        assert_eq!(parse_content_range("items 0-9/10"), None);
        assert_eq!(parse_content_range("bytes x-9/10"), None);
    }

    // ---- HttpSource against a throwaway local server ----

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve `body` once per connection. `ranges` decides whether `Range` is honoured;
    /// `redirect` answers 302 instead.
    async fn serve(body: Vec<u8>, ranges: bool, redirect: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let range = req
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("range: bytes=").map(String::from))
                        .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok());
                    let head;
                    let payload: &[u8];
                    if redirect {
                        head = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
                        payload = &[];
                    } else if let (true, Some(from)) = (ranges, range) {
                        payload = &body[from..];
                        head = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {from}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len() - 1,
                            body.len(),
                            payload.len()
                        );
                    } else {
                        payload = &body;
                        head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                    }
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(payload).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}/disk.raw")
    }

    async fn drain(src: &mut HttpSource) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(p) = src.next().await.unwrap() {
            out.extend_from_slice(&p);
        }
        out
    }

    #[tokio::test]
    async fn http_source_streams_the_whole_image() {
        let body = pseudo(300_000, 11);
        let url = serve(body.clone(), true, false).await;
        let mut src = HttpSource::new(&url).unwrap();
        assert_eq!(src.open(0).await.unwrap(), Some(body.len() as u64));
        assert_eq!(drain(&mut src).await, body);
    }

    #[tokio::test]
    async fn http_source_resumes_with_a_range_request() {
        let body = pseudo(300_000, 12);
        let url = serve(body.clone(), true, false).await;
        let mut src = HttpSource::new(&url).unwrap();
        assert_eq!(src.open(100_000).await.unwrap(), Some(body.len() as u64));
        assert_eq!(drain(&mut src).await, body[100_000..]);
    }

    #[tokio::test]
    async fn http_source_skips_the_prefix_when_the_server_ignores_range() {
        let body = pseudo(300_000, 13);
        let url = serve(body.clone(), false, false).await;
        let mut src = HttpSource::new(&url).unwrap();
        assert_eq!(src.open(100_000).await.unwrap(), Some(body.len() as u64));
        assert_eq!(drain(&mut src).await, body[100_000..]);
    }

    #[tokio::test]
    async fn http_source_refuses_redirects() {
        let url = serve(Vec::new(), true, true).await;
        let mut src = HttpSource::new(&url).unwrap();
        let err = src.open(0).await.unwrap_err().to_string();
        assert!(err.contains("302"), "{err}");
    }
}
