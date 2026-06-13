//! The byte-addressed read-only device abstraction everything stacks on.

use crate::{Error, Result};
use std::io::{self, Read, Seek, SeekFrom, Write};

/// A read-only device addressable by absolute byte offset.
///
/// `&self` on purpose: implementations must be usable without external
/// synchronization for reads (e.g. `pread(2)` on files), so layers above can
/// share one device.
pub trait ReadAt {
    /// Read exactly `buf.len()` bytes at `offset`. Short reads are errors.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Total size in bytes.
    fn size(&self) -> u64;

    fn check_bounds(&self, offset: u64, len: usize) -> Result<()> {
        if offset.checked_add(len as u64).is_none_or(|end| end > self.size()) {
            return Err(Error::OutOfBounds { offset, len, size: self.size() });
        }
        Ok(())
    }
}

impl ReadAt for std::fs::File {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        self.read_exact_at(buf, offset)?;
        Ok(())
    }

    fn size(&self) -> u64 {
        self.metadata().map(|m| m.len()).unwrap_or(0)
    }
}

impl<T: ReadAt + ?Sized> ReadAt for Box<T> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
    fn size(&self) -> u64 {
        (**self).size()
    }
}

impl<T: ReadAt + ?Sized> ReadAt for std::sync::Arc<T> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
    fn size(&self) -> u64 {
        (**self).size()
    }
}

/// A byte-range window over a parent device. Used to expose one partition
/// as its own device.
pub struct Slice<D> {
    parent: D,
    start: u64,
    len: u64,
}

impl<D: ReadAt> Slice<D> {
    pub fn new(parent: D, start: u64, len: u64) -> Self {
        Slice { parent, start, len }
    }
}

impl<D: ReadAt> ReadAt for Slice<D> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check_bounds(offset, buf.len())?;
        self.parent.read_at(self.start + offset, buf)
    }

    fn size(&self) -> u64 {
        self.len
    }
}

/// Adapts a `ReadAt` into `std::io::{Read, Seek, Write}` for crates that
/// want stream-style I/O (`fatfs`). Writes always fail: this tool is
/// strictly read-only.
pub struct IoAdapter<D> {
    dev: D,
    pos: u64,
}

impl<D: ReadAt> IoAdapter<D> {
    pub fn new(dev: D) -> Self {
        IoAdapter { dev, pos: 0 }
    }
}

impl<D: ReadAt> Read for IoAdapter<D> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.dev.size().saturating_sub(self.pos);
        let n = (buf.len() as u64).min(remaining) as usize;
        if n == 0 {
            return Ok(0);
        }
        self.dev
            .read_at(self.pos, &mut buf[..n])
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<D: ReadAt> Seek for IoAdapter<D> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(o) => Some(o),
            SeekFrom::End(d) => self.dev.size().checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        match new {
            Some(p) => {
                self.pos = p;
                Ok(p)
            }
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start")),
        }
    }
}

impl<D: ReadAt> Write for IoAdapter<D> {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "device is read-only"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
