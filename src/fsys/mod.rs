//! Filesystem access behind one read-only trait.
//!
//! Supported: ext2/3/4 (`ext4-view`), FAT12/16/32 (`fatfs`).
//! Detected-but-unsupported (XFS, btrfs, squashfs) get a precise error so
//! users know what they hit. XFS is the next milestone (RHEL 9 /boot).

mod ext4;
mod vfat;

use crate::blockdev::ReadAt;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
}

/// Read-only view of a filesystem. Paths are absolute, `/`-separated,
/// rooted at the filesystem (not the guest's mount tree).
pub trait FileSystem {
    fn fs_type(&self) -> &'static str;
    fn label(&self) -> Option<String>;
    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>>;
    fn read_file(&self, path: &str) -> Result<Vec<u8>>;
    fn exists(&self, path: &str) -> bool;

    /// Size in bytes of a regular file (symlinks followed), from inode /
    /// directory metadata only — no data is read. Callers use this as a
    /// cheap cache-validation key for files they extracted earlier.
    fn file_size(&self, path: &str) -> Result<u64>;

    /// Symlink target, for filesystems that have them (vfat doesn't).
    fn read_link(&self, _path: &str) -> Option<String> {
        None
    }
}

/// Best-effort filesystem type sniffing by magic bytes.
pub fn detect<D: ReadAt>(dev: &D) -> Result<Option<&'static str>> {
    let mut buf = [0u8; 4];

    // ext2/3/4: superblock at 1024, magic 0xEF53 at superblock offset 56.
    if dev.check_bounds(1024 + 56, 2).is_ok() {
        let mut m = [0u8; 2];
        dev.read_at(1024 + 56, &mut m)?;
        if u16::from_le_bytes(m) == 0xef53 {
            return Ok(Some("ext4"));
        }
    }

    // XFS: "XFSB" at offset 0.
    if dev.check_bounds(0, 4).is_ok() {
        dev.read_at(0, &mut buf)?;
        if &buf == b"XFSB" {
            return Ok(Some("xfs"));
        }
        // squashfs: "hsqs" at offset 0.
        if &buf == b"hsqs" {
            return Ok(Some("squashfs"));
        }
    }

    // btrfs: "_BHRfS_M" at 0x10040.
    if dev.check_bounds(0x10040, 8).is_ok() {
        let mut m = [0u8; 8];
        dev.read_at(0x10040, &mut m)?;
        if &m == b"_BHRfS_M" {
            return Ok(Some("btrfs"));
        }
    }

    // FAT: x86 jump at 0 plus an OEM/FS-type marker. fatfs validates the
    // BPB properly; this is only a cheap pre-filter.
    if dev.check_bounds(0, 512).is_ok() {
        let mut sector = [0u8; 512];
        dev.read_at(0, &mut sector)?;
        let jump_ok = matches!(sector[0], 0xeb | 0xe9);
        let fat32 = &sector[82..90] == b"FAT32   ";
        let fat16 = sector[54..62].starts_with(b"FAT");
        if jump_ok && (fat32 || fat16) {
            return Ok(Some("vfat"));
        }
    }

    Ok(None)
}

/// Open the filesystem on `dev`, if it is a supported type.
pub fn open(dev: Box<dyn ReadAt>) -> Result<Box<dyn FileSystem>> {
    match detect(&dev)? {
        Some("ext4") => Ok(Box::new(ext4::Ext4Fs::open(dev)?)),
        Some("vfat") => Ok(Box::new(vfat::VfatFs::open(dev)?)),
        Some(other) => Err(Error::UnknownFilesystem(Some(other))),
        None => Err(Error::UnknownFilesystem(None)),
    }
}
