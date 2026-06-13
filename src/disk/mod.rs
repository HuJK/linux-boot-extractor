//! Disk image formats. `open()` sniffs the format and returns a flat
//! byte-addressable view of the *guest-visible* disk contents, opening
//! backing file chains as needed.

mod qcow2;

pub use qcow2::Qcow2;

use crate::blockdev::ReadAt;
use crate::{Error, Result};
use std::fs::File;
use std::path::Path;

const MAX_BACKING_DEPTH: u32 = 8;

pub enum DiskImage {
    Raw(File),
    Qcow2(Qcow2<File>),
}

impl DiskImage {
    pub fn open(path: &Path) -> Result<DiskImage> {
        Self::open_at_depth(path, 0)
    }

    fn open_at_depth(path: &Path, depth: u32) -> Result<DiskImage> {
        if depth > MAX_BACKING_DEPTH {
            return Err(Error::Qcow2("backing file chain too deep (loop?)".into()));
        }
        let file = File::open(path)?;
        let mut magic = [0u8; 4];
        // Images shorter than 4 bytes are treated as raw.
        if file.size() >= 4 {
            ReadAt::read_at(&file, 0, &mut magic)?;
        }
        if magic != qcow2::MAGIC {
            return Ok(DiskImage::Raw(file));
        }

        let mut q = Qcow2::open(file)?;
        if let Some(name) = q.backing_file().map(str::to_string) {
            // Relative backing paths are relative to the referring image.
            let backing_path = if name.starts_with('/') {
                std::path::PathBuf::from(&name)
            } else {
                path.parent().unwrap_or(Path::new(".")).join(&name)
            };
            let backing =
                Self::open_at_depth(&backing_path, depth + 1).map_err(|e| {
                    Error::Qcow2(format!(
                        "opening backing file {}: {e}",
                        backing_path.display()
                    ))
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
