//! Where the bytes of a disk image come from. The whole tool reads through
//! the [`ReadAt`](crate::blockdev::ReadAt) seam; this module only decides
//! what backs it:
//!
//!   - a local path  → `std::fs::File` (random-access `pread`; never
//!     written — the tool is strictly read-only);
//!   - an `http(s)` URL → a lazy 4 MiB-chunk reader that downloads only the
//!     ranges the upper layers actually touch ("cloud analysis").
//!
//! qcow2/raw sniffing and partition/fs parsing don't care which backs them,
//! so a 2 GB cloud image is analysed by pulling a few dozen MB of metadata
//! plus the kernel/initrd clusters — never the whole file.

mod dns;
mod http;
mod remote;
mod tls;

use crate::blockdev::ReadAt;
use crate::Result;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// A byte source usable as a qcow2 host / raw image. `Send + Sync` so it can
/// sit behind the `Arc<DiskImage>` the partition/fs layers share.
pub type Source = Box<dyn ReadAt + Send + Sync>;

/// Process-wide directory for persisting downloaded chunks, set once from
/// the CLI `--cache-dir`. A simple global keeps `DiskImage::open` and every
/// command free of a config parameter they'd only forward.
static CACHE_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Set the chunk cache directory (no-op if already set). Call once at start.
pub fn set_cache_dir(dir: Option<PathBuf>) {
    let _ = CACHE_DIR.set(dir);
}

pub(crate) fn cache_dir() -> Option<&'static Path> {
    CACHE_DIR.get().and_then(|d| d.as_deref())
}

/// Per-connection network timeout for remote images, set once from the CLI
/// `--timeout`. Each chunk uses a fresh connection, so this bounds every
/// chunk independently: a connect or read that stalls this long fails.
static HTTP_TIMEOUT: OnceLock<Duration> = OnceLock::new();

/// Set the network timeout in seconds (no-op if already set, or if `secs` is
/// `None`/0 — keeping the default).
pub fn set_http_timeout(secs: Option<u64>) {
    if let Some(s) = secs.filter(|&s| s > 0) {
        let _ = HTTP_TIMEOUT.set(Duration::from_secs(s));
    }
}

pub(crate) fn http_timeout() -> Duration {
    HTTP_TIMEOUT.get().copied().unwrap_or(Duration::from_secs(60))
}

/// Whether `locator` is an http(s) URL rather than a filesystem path.
pub fn is_url(locator: &str) -> bool {
    locator.starts_with("http://") || locator.starts_with("https://")
}

/// Open a byte source for `locator` (a local path or an http(s) URL).
pub fn open(locator: &str) -> Result<Source> {
    if is_url(locator) {
        Ok(Box::new(remote::RemoteReader::open(locator)?))
    } else {
        Ok(Box::new(std::fs::File::open(Path::new(locator))?))
    }
}

/// Resolve a qcow2 backing-file reference `name` against the `base` locator
/// it was found in: a sibling under the same directory (local path or URL),
/// a full URL, or an absolute local path.
pub fn resolve_relative(base: &str, name: &str) -> String {
    // A full URL, or an absolute path on a local base, stands on its own.
    if is_url(name) || (name.starts_with('/') && !is_url(base)) {
        return name.to_string();
    }
    match base.rfind('/') {
        Some(i) => format!("{}/{}", &base[..i], name),
        None => name.to_string(),
    }
}
