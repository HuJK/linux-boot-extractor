//! Lazy, range-based reader for a remote disk image.
//!
//! Probes the URL once for its size and whether it honours HTTP `Range`,
//! then serves [`read_at`](ReadAt::read_at) out of a cache of fixed 4 MiB
//! chunks, fetching a chunk the first time it is touched and blocking until
//! it arrives:
//!
//!   - range supported → fetch exactly the touched chunk (random access);
//!   - range *not* supported → stream forward from byte 0, caching chunks,
//!     until the wanted one is reached (no seeking, so the bytes in front of
//!     it get downloaded too).
//!
//! Chunks stay cached in memory for the reader's lifetime — qcow2 reads are
//! scattered, so a chunk touched once is usually wanted again — and, when a
//! `--cache-dir` is given, also persist to disk at `<dir>/<md5(url)>/<idx>`
//! so a later run reuses them without re-downloading.

use super::http::HttpFetcher;
use crate::blockdev::ReadAt;
use crate::{Error, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Chunk granularity for lazy fetches and the on-disk cache layout.
const CHUNK: u64 = 4 * 1024 * 1024;

/// What probing the remote endpoint told us.
pub struct Probe {
    pub size: u64,
    pub supports_range: bool,
}

/// A source of byte ranges over the network. Abstracted from the cache
/// engine so the engine is testable without real sockets.
pub trait RangeFetcher: Send + Sync {
    fn probe(&self) -> Result<Probe>;
    /// Bytes `[start, end)` (end exclusive); only used when ranges work.
    fn fetch_range(&self, start: u64, end: u64) -> Result<Vec<u8>>;
    /// A forward stream of the whole body from byte 0, for no-range servers.
    fn fetch_all(&self) -> Result<Box<dyn Read + Send>>;
}

pub struct RemoteReader {
    fetcher: Box<dyn RangeFetcher>,
    size: u64,
    supports_range: bool,
    cache: Option<DiskCache>,
    state: Mutex<State>,
}

struct State {
    /// One slot per chunk index; `None` until fetched.
    chunks: Vec<Option<Box<[u8]>>>,
    /// No-range mode: the open forward stream and how many leading chunks it
    /// has already produced.
    stream: Option<Box<dyn Read + Send>>,
    streamed: usize,
    /// Chunks actually downloaded this run (cache hits don't count), for the
    /// `progress:` lines.
    downloaded: usize,
}

impl RemoteReader {
    pub fn open(url: &str) -> Result<RemoteReader> {
        let cache = super::cache_dir().map(|root| DiskCache::new(root, url));
        Self::build(Box::new(HttpFetcher::new(url)?), cache)
    }

    fn build(fetcher: Box<dyn RangeFetcher>, cache: Option<DiskCache>) -> Result<RemoteReader> {
        let probe = fetcher.probe()?;
        if probe.size == 0 {
            return Err(Error::Http("server reported a zero-length image".into()));
        }
        let nchunks = probe.size.div_ceil(CHUNK) as usize;
        Ok(RemoteReader {
            fetcher,
            size: probe.size,
            supports_range: probe.supports_range,
            cache,
            state: Mutex::new(State {
                chunks: (0..nchunks).map(|_| None).collect(),
                stream: None,
                streamed: 0,
                downloaded: 0,
            }),
        })
    }

    /// Byte length of chunk `idx` (the last one is short).
    fn chunk_len(&self, idx: usize) -> usize {
        let start = idx as u64 * CHUNK;
        (self.size - start).min(CHUNK) as usize
    }

    /// Ensure chunk `idx` is present in `state.chunks`, pulling it from the
    /// disk cache or the network (and, with no range support, everything in
    /// front of it) if not.
    fn ensure(&self, state: &mut State, idx: usize) -> Result<()> {
        if state.chunks[idx].is_some() {
            return Ok(());
        }
        let expected = self.chunk_len(idx);
        if let Some(cache) = &self.cache
            && let Some(data) = cache.load(idx, expected)
        {
            state.chunks[idx] = Some(data.into_boxed_slice());
            return Ok(());
        }
        if self.supports_range {
            let start = idx as u64 * CHUNK;
            let data = self.fetcher.fetch_range(start, start + expected as u64)?;
            if data.len() != expected {
                return Err(Error::Http(format!(
                    "short range read: wanted {expected} got {}",
                    data.len()
                )));
            }
            self.store(state, idx, data);
        } else {
            // Sequential: read (and cache) every chunk up to and including idx.
            if state.stream.is_none() {
                state.stream = Some(self.fetcher.fetch_all()?);
                state.streamed = 0;
            }
            while state.streamed <= idx {
                let n = state.streamed;
                let mut buf = vec![0u8; self.chunk_len(n)];
                let mut stream = state.stream.take().unwrap();
                let res = read_full(&mut stream, &mut buf);
                state.stream = Some(stream);
                res?;
                self.store(state, n, buf);
                state.streamed += 1;
            }
        }
        Ok(())
    }

    /// Place a freshly fetched chunk into memory and the disk cache, and
    /// report progress. Each downloaded chunk emits a `progress: <done>/<total>`
    /// line on stderr (unbuffered) so a caller can show a live count instead
    /// of a spinner, and detect a stall by watching `<done>` stop changing.
    fn store(&self, state: &mut State, idx: usize, data: Vec<u8>) {
        if let Some(cache) = &self.cache {
            cache.store(idx, &data);
        }
        state.chunks[idx] = Some(data.into_boxed_slice());
        state.downloaded += 1;
        eprintln!("progress: {}/{}", state.downloaded, state.chunks.len());
    }
}

impl ReadAt for RemoteReader {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check_bounds(offset, buf.len())?;
        if buf.is_empty() {
            return Ok(());
        }
        let first = (offset / CHUNK) as usize;
        let last = ((offset + buf.len() as u64 - 1) / CHUNK) as usize;
        let mut state = self.state.lock().unwrap();
        for idx in first..=last {
            self.ensure(&mut state, idx)?;
        }
        let mut done = 0usize;
        let mut pos = offset;
        while done < buf.len() {
            let idx = (pos / CHUNK) as usize;
            let within = (pos % CHUNK) as usize;
            let chunk = state.chunks[idx].as_ref().expect("ensured above");
            let n = (chunk.len() - within).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&chunk[within..within + n]);
            done += n;
            pos += n as u64;
        }
        Ok(())
    }

    fn size(&self) -> u64 {
        self.size
    }
}

/// On-disk chunk cache: `<root>/<md5(url)>/<chunk-index>`, written via a
/// temp file + rename so a partial download never looks complete.
struct DiskCache {
    dir: PathBuf,
}

impl DiskCache {
    fn new(root: &Path, url: &str) -> DiskCache {
        DiskCache { dir: root.join(format!("{:x}", md5::compute(url.as_bytes()))) }
    }

    fn chunk_path(&self, idx: usize) -> PathBuf {
        self.dir.join(idx.to_string())
    }

    /// A cached chunk, but only if its length matches what we expect (guards
    /// against truncated files or a changed chunk size).
    fn load(&self, idx: usize, expected_len: usize) -> Option<Vec<u8>> {
        let data = std::fs::read(self.chunk_path(idx)).ok()?;
        (data.len() == expected_len).then_some(data)
    }

    fn store(&self, idx: usize, data: &[u8]) {
        if std::fs::create_dir_all(&self.dir).is_err() {
            return; // best-effort cache; a write failure just means a re-fetch
        }
        let tmp = self.dir.join(format!("{idx}.part"));
        if std::fs::write(&tmp, data).is_ok() {
            let _ = std::fs::rename(&tmp, self.chunk_path(idx));
        }
    }
}

fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Err(Error::Http("stream ended before the expected length".into())),
            Ok(n) => filled += n,
            Err(e) => return Err(Error::Http(format!("stream read: {e}"))),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// In-memory fetcher over a fixed blob, counting bytes fetched so tests
    /// can assert laziness.
    struct FakeFetcher {
        data: Vec<u8>,
        supports_range: bool,
        fetched: Arc<AtomicUsize>,
    }

    impl RangeFetcher for FakeFetcher {
        fn probe(&self) -> Result<Probe> {
            Ok(Probe { size: self.data.len() as u64, supports_range: self.supports_range })
        }
        fn fetch_range(&self, start: u64, end: u64) -> Result<Vec<u8>> {
            assert!(self.supports_range);
            self.fetched.fetch_add((end - start) as usize, Ordering::SeqCst);
            Ok(self.data[start as usize..end as usize].to_vec())
        }
        fn fetch_all(&self) -> Result<Box<dyn Read + Send>> {
            // Meter bytes as they're actually read, so the test can observe
            // that streaming stops at the target chunk.
            Ok(Box::new(CountingReader {
                inner: std::io::Cursor::new(self.data.clone()),
                counter: self.fetched.clone(),
            }))
        }
    }

    struct CountingReader {
        inner: std::io::Cursor<Vec<u8>>,
        counter: Arc<AtomicUsize>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.counter.fetch_add(n, Ordering::SeqCst);
            Ok(n)
        }
    }

    fn blob(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    fn reader(data: Vec<u8>, supports_range: bool, cache: Option<DiskCache>) -> (RemoteReader, Arc<AtomicUsize>) {
        let fetched = Arc::new(AtomicUsize::new(0));
        let f = FakeFetcher { data, supports_range, fetched: fetched.clone() };
        (RemoteReader::build(Box::new(f), cache).unwrap(), fetched)
    }

    #[test]
    fn range_fetches_only_touched_chunks() {
        let data = blob((CHUNK * 3 + 1000) as usize);
        let (r, fetched) = reader(data.clone(), true, None);
        // Read 10 bytes inside chunk 2 only.
        let mut buf = [0u8; 10];
        let off = CHUNK * 2 + 5;
        r.read_at(off, &mut buf).unwrap();
        assert_eq!(buf, data[off as usize..off as usize + 10]);
        // Only chunk 2 (a full CHUNK) was downloaded, not chunks 0/1/3.
        assert_eq!(fetched.load(Ordering::SeqCst), CHUNK as usize);
    }

    #[test]
    fn range_spanning_two_chunks() {
        let data = blob((CHUNK * 2 + 50) as usize);
        let (r, fetched) = reader(data.clone(), true, None);
        let off = CHUNK - 5;
        let mut buf = [0u8; 10]; // straddles chunk 0/1 boundary
        r.read_at(off, &mut buf).unwrap();
        assert_eq!(buf, data[off as usize..off as usize + 10]);
        assert_eq!(fetched.load(Ordering::SeqCst), (CHUNK * 2) as usize);
    }

    #[test]
    fn no_range_streams_up_to_target() {
        let data = blob((CHUNK * 3 + 7) as usize);
        let (r, fetched) = reader(data.clone(), false, None);
        let off = CHUNK * 2 + 1;
        let mut buf = [0u8; 4];
        r.read_at(off, &mut buf).unwrap();
        assert_eq!(buf, data[off as usize..off as usize + 4]);
        // Streamed chunks 0,1,2 (everything up to the target); not chunk 3.
        assert_eq!(fetched.load(Ordering::SeqCst), (CHUNK * 3) as usize);
        // A second read of an already-streamed chunk fetches nothing more.
        r.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, data[0..4]);
        assert_eq!(fetched.load(Ordering::SeqCst), (CHUNK * 3) as usize);
    }

    #[test]
    fn disk_cache_persists_and_reuses() {
        let tmp = std::env::temp_dir().join(format!("lbx-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let data = blob((CHUNK * 2 + 3) as usize);
        let off = CHUNK + 10;
        let mut buf = [0u8; 8];

        // First reader downloads chunk 1 and writes it to the cache.
        {
            let (r, fetched) = reader(data.clone(), true, Some(DiskCache::new(&tmp, "http://x/y")));
            r.read_at(off, &mut buf).unwrap();
            assert_eq!(fetched.load(Ordering::SeqCst), CHUNK as usize);
        }
        // The cache file lives at <root>/<md5(url)>/<idx>.
        let key = format!("{:x}", md5::compute(b"http://x/y"));
        assert!(tmp.join(&key).join("1").exists());

        // A fresh reader serves the same chunk from disk, fetching nothing.
        {
            let (r, fetched) = reader(data.clone(), true, Some(DiskCache::new(&tmp, "http://x/y")));
            let mut buf2 = [0u8; 8];
            r.read_at(off, &mut buf2).unwrap();
            assert_eq!(buf2, data[off as usize..off as usize + 8]);
            assert_eq!(fetched.load(Ordering::SeqCst), 0);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
