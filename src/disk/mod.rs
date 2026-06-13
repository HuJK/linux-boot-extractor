//! Disk image formats. `open()` sniffs the format and returns a flat
//! byte-addressable view of the *guest-visible* disk contents, opening
//! backing file chains as needed. The underlying bytes come from a
//! [`source`](crate::source) — a local file or a lazily-downloaded URL —
//! so the same code analyses a path or a cloud image.

mod qcow2;

pub use qcow2::Qcow2;

use crate::blockdev::ReadAt;
use crate::source::{self, Source};
use crate::{Error, Result};

const MAX_BACKING_DEPTH: u32 = 8;

pub enum DiskImage {
    Raw(Source),
    Qcow2(Qcow2<Source>),
}

impl DiskImage {
    /// Open `locator` — a filesystem path or an `http(s)://` URL.
    pub fn open(locator: &str) -> Result<DiskImage> {
        Self::open_at_depth(locator, 0)
    }

    fn open_at_depth(locator: &str, depth: u32) -> Result<DiskImage> {
        if depth > MAX_BACKING_DEPTH {
            return Err(Error::Qcow2("backing file chain too deep (loop?)".into()));
        }
        let src = source::open(locator)?;
        let mut magic = [0u8; 4];
        // Images shorter than 4 bytes are treated as raw.
        if src.size() >= 4 {
            src.read_at(0, &mut magic)?;
        }
        if magic != qcow2::MAGIC {
            return Ok(DiskImage::Raw(src));
        }

        let mut q = Qcow2::open(src)?;
        if let Some(name) = q.backing_file().map(str::to_string) {
            // Backing references resolve relative to the referring image.
            let backing_locator = source::resolve_relative(locator, &name);
            let backing = Self::open_at_depth(&backing_locator, depth + 1).map_err(|e| {
                Error::Qcow2(format!("opening backing file {backing_locator}: {e}"))
            })?;
            q.set_backing(Box::new(backing));
        }
        Ok(DiskImage::Qcow2(q))
    }

    pub fn format(&self) -> &'static str {
        match self {
            DiskImage::Raw(_) => "raw",
            DiskImage::Qcow2(_) => "qcow2",
        }
    }
}

impl ReadAt for DiskImage {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            DiskImage::Raw(f) => f.read_at(offset, buf),
            DiskImage::Qcow2(q) => q.read_at(offset, buf),
        }
    }

    fn size(&self) -> u64 {
        match self {
            DiskImage::Raw(f) => f.size(),
            DiskImage::Qcow2(q) => q.size(),
        }
    }
}
