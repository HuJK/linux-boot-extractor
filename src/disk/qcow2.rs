//! Read-only qcow2 (v2/v3) implementation.
//!
//! Supported: standard clusters, sparse (unallocated) clusters, the
//! all-zeroes cluster flag, zlib-compressed clusters, backing files
//! (attached by `disk::open`).
//! Not supported (clean error, not corruption): zstd compression,
//! encryption, external data files, extended L2 entries.

use crate::blockdev::ReadAt;
use crate::{Error, Result};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub const MAGIC: [u8; 4] = *b"QFI\xfb";

// Incompatible feature bits (header offset 72, v3 only).
const INCOMPAT_DIRTY: u64 = 1 << 0; // stale refcounts only; harmless for reads
const INCOMPAT_CORRUPT: u64 = 1 << 1;
const INCOMPAT_EXTERNAL_DATA: u64 = 1 << 2;
const INCOMPAT_COMPRESSION_TYPE: u64 = 1 << 3; // set = non-zlib (zstd)
const INCOMPAT_EXTENDED_L2: u64 = 1 << 4;

const L1_OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
const L2_OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
const L2_COMPRESSED: u64 = 1 << 62;
const L2_ZERO_FLAG: u64 = 1; // standard cluster descriptor bit 0

/// Decompressed-cluster cache size. Filesystem walks re-read the same
/// metadata clusters constantly; without this every hit re-inflates 64 KiB.
const CACHE_CLUSTERS: usize = 32;

enum Mapping {
    /// Host file offset of the cluster's data.
    Data(u64),
    /// Reads as zeroes regardless of any backing file.
    Zero,
    /// Not allocated here: defer to the backing file (or zeroes).
    Unallocated,
    /// zlib-compressed cluster at `offset`, `csize` compressed bytes.
    Compressed { offset: u64, csize: usize },
}

pub struct Qcow2<D> {
    host: D,
    virtual_size: u64,
    cluster_bits: u32,
    l1: Vec<u64>,
    backing_name: Option<String>,
    backing: Option<Box<dyn ReadAt + Send + Sync>>,
    /// (host offset -> decompressed cluster), small LRU.
    cache: Mutex<VecDeque<(u64, Arc<Vec<u8>>)>>,
}

impl<D: ReadAt> Qcow2<D> {
    pub fn open(host: D) -> Result<Qcow2<D>> {
        // v2 header is 72 bytes; v3 extension fields are read separately below.
        let mut hdr = [0u8; 72];
        host.read_at(0, &mut hdr)?;

        let be32 = |off: usize| u32::from_be_bytes(hdr[off..off + 4].try_into().unwrap());
        let be64 = |off: usize| u64::from_be_bytes(hdr[off..off + 8].try_into().unwrap());

        if hdr[..4] != MAGIC {
            return Err(Error::Qcow2("bad magic".into()));
        }
        let version = be32(4);
        if version != 2 && version != 3 {
            return Err(Error::Qcow2(format!("unsupported version {version}")));
        }

        let incompatible = if version == 3 {
            let mut b = [0u8; 8];
            host.read_at(72, &mut b)?;
            u64::from_be_bytes(b)
        } else {
            0
        };

        let backing_file_offset = be64(8);
        let backing_file_size = be32(16) as usize;
        let cluster_bits = be32(20);
        let virtual_size = be64(24);
        let crypt_method = be32(32);
        let l1_size = be32(36) as usize;
        let l1_table_offset = be64(40);

        if crypt_method != 0 {
            return Err(Error::Qcow2Unsupported("encryption".into()));
        }
        if incompatible & INCOMPAT_CORRUPT != 0 {
            return Err(Error::Qcow2("image is marked corrupt".into()));
        }
        if incompatible & INCOMPAT_EXTERNAL_DATA != 0 {
            return Err(Error::Qcow2Unsupported("external data file".into()));
        }
        if incompatible & INCOMPAT_COMPRESSION_TYPE != 0 {
            return Err(Error::Qcow2Unsupported(
                "compression type other than zlib (likely zstd)".into(),
            ));
        }
        if incompatible & INCOMPAT_EXTENDED_L2 != 0 {
            return Err(Error::Qcow2Unsupported("extended L2 entries".into()));
        }
        if incompatible & !INCOMPAT_DIRTY != 0 {
            return Err(Error::Qcow2Unsupported(format!(
                "unknown incompatible features {incompatible:#x}"
            )));
        }
        if !(9..=21).contains(&cluster_bits) {
            return Err(Error::Qcow2(format!("implausible cluster_bits {cluster_bits}")));
        }

        let backing_name = if backing_file_offset != 0 && backing_file_size > 0 {
            if backing_file_size > 1023 {
                return Err(Error::Qcow2("implausible backing file name length".into()));
            }
            let mut name = vec![0u8; backing_file_size];
            host.read_at(backing_file_offset, &mut name)?;
            Some(String::from_utf8_lossy(&name).into_owned())
        } else {
            None
        };

        // The L1 table is small (one entry maps cluster_size/8 * cluster_size
        // bytes; 8 KiB of L1 covers 64 GiB at 64 KiB clusters) — load it whole.
        let mut l1_raw = vec![0u8; l1_size * 8];
        host.read_at(l1_table_offset, &mut l1_raw)?;
        let l1 = l1_raw
            .chunks_exact(8)
            .map(|c| u64::from_be_bytes(c.try_into().unwrap()))
            .collect();

        Ok(Qcow2 {
            host,
            virtual_size,
            cluster_bits,
            l1,
            backing_name,
            backing: None,
            cache: Mutex::new(VecDeque::new()),
        })
    }

    /// Backing file name from the header, exactly as stored (resolution
    /// against the image's directory is the opener's job).
    pub fn backing_file(&self) -> Option<&str> {
        self.backing_name.as_deref()
    }

    pub fn set_backing(&mut self, backing: Box<dyn ReadAt + Send + Sync>) {
        self.backing = Some(backing);
    }

    /// Whether any allocated cluster is zlib-compressed. lbx reads such
    /// clusters fine, but crosvm's qcow2 reader cannot — it returns I/O
    /// errors to the guest, so the disk shows up with the right size yet no
    /// readable partition table. A caller that direct-boots via crosvm uses
    /// this to convert the image first. Scans only present L2 tables (one
    /// whole-cluster read each) and returns at the first compressed entry —
    /// a compressed cloud image hits cluster 0 immediately. The backing
    /// chain is not scanned (imported images are single-file).
    pub fn has_compressed_clusters(&self) -> Result<bool> {
        let mut l2 = vec![0u8; self.cluster_size() as usize];
        for &l1_entry in &self.l1 {
            let l2_table = l1_entry & L1_OFFSET_MASK;
            if l2_table == 0 {
                continue; // L2 table not allocated -> no clusters here
            }
            self.host.read_at(l2_table, &mut l2)?;
            for chunk in l2.chunks_exact(8) {
                let entry = u64::from_be_bytes(chunk.try_into().unwrap());
                if entry & L2_COMPRESSED != 0 {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn cluster_size(&self) -> u64 {
        1 << self.cluster_bits
    }

    fn map_cluster(&self, guest_offset: u64) -> Result<Mapping> {
        let l2_entries = self.cluster_size() / 8;
        let cluster_index = guest_offset >> self.cluster_bits;
        let l1_index = (cluster_index / l2_entries) as usize;
        let l2_index = cluster_index % l2_entries;

        let l2_table = self.l1.get(l1_index).copied().unwrap_or(0) & L1_OFFSET_MASK;
        if l2_table == 0 {
            return Ok(Mapping::Unallocated);
        }

        let mut buf = [0u8; 8];
        self.host.read_at(l2_table + l2_index * 8, &mut buf)?;
        let entry = u64::from_be_bytes(buf);

        if entry & L2_COMPRESSED != 0 {
            // Compressed cluster descriptor (bits 61..0):
            //   x = 62 - (cluster_bits - 8)
            //   bits x-1..0  host offset
            //   bits 61..x   additional 512-byte sectors minus one
            let x = 62 - (self.cluster_bits - 8);
            let desc = entry & !(3 << 62);
            let offset = desc & ((1u64 << x) - 1);
            let sectors = (desc >> x) + 1;
            let csize = (sectors * 512 - (offset & 511)) as usize;
            return Ok(Mapping::Compressed { offset, csize });
        }
        if entry & L2_ZERO_FLAG != 0 {
            return Ok(Mapping::Zero);
        }
        let host_offset = entry & L2_OFFSET_MASK;
        if host_offset == 0 {
            Ok(Mapping::Unallocated)
        } else {
            Ok(Mapping::Data(host_offset))
        }
    }

    fn decompress_cluster(&self, offset: u64, csize: usize) -> Result<Arc<Vec<u8>>> {
        if let Some((_, data)) =
            self.cache.lock().unwrap().iter().find(|(o, _)| *o == offset)
        {
            return Ok(data.clone());
        }

        // csize is rounded up to sector granularity and may reach past the
        // end of the file for the last cluster; read what exists.
        let avail = self.host.size().saturating_sub(offset);
        let n = (csize as u64).min(avail) as usize;
        let mut comp = vec![0u8; n];
        self.host.read_at(offset, &mut comp)?;

        let cluster_size = self.cluster_size() as usize;
        let mut out = vec![0u8; cluster_size];
        let mut z = flate2::Decompress::new(false); // raw deflate, no zlib header
        while (z.total_out() as usize) < cluster_size {
            let in_pos = z.total_in() as usize;
            let out_pos = z.total_out() as usize;
            let status = z
                .decompress(&comp[in_pos..], &mut out[out_pos..], flate2::FlushDecompress::Finish)
                .map_err(|e| Error::Qcow2(format!("cluster decompression failed: {e}")))?;
            let stalled =
                z.total_in() as usize == in_pos && z.total_out() as usize == out_pos;
            if status == flate2::Status::StreamEnd || stalled {
                break;
            }
        }
        if z.total_out() as usize != cluster_size {
            return Err(Error::Qcow2(format!(
                "compressed cluster at {offset:#x} inflated to {} bytes, expected {cluster_size}",
                z.total_out()
            )));
        }

        let data = Arc::new(out);
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= CACHE_CLUSTERS {
            cache.pop_front();
        }
        cache.push_back((offset, data.clone()));
        Ok(data)
    }

    /// Unallocated range: defer to the backing file, zero-filling past its
    /// end (an overlay may be larger than its backing image).
    fn read_backing(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let Some(backing) = &self.backing else {
            buf.fill(0);
            return Ok(());
        };
        let avail = backing.size().saturating_sub(offset);
        let n = (buf.len() as u64).min(avail) as usize;
        if n > 0 {
            backing.read_at(offset, &mut buf[..n])?;
        }
        buf[n..].fill(0);
        Ok(())
    }
}

impl<D: ReadAt> ReadAt for Qcow2<D> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check_bounds(offset, buf.len())?;
        let cluster_size = self.cluster_size();

        let mut pos = offset;
        let mut buf = buf;
        while !buf.is_empty() {
            let within = pos % cluster_size;
            let chunk = ((cluster_size - within) as usize).min(buf.len());
            let (head, tail) = buf.split_at_mut(chunk);
            match self.map_cluster(pos - within)? {
                Mapping::Data(host_cluster) => self.host.read_at(host_cluster + within, head)?,
                Mapping::Zero => head.fill(0),
                Mapping::Unallocated => self.read_backing(pos, head)?,
                Mapping::Compressed { offset, csize } => {
                    let data = self.decompress_cluster(offset, csize)?;
                    head.copy_from_slice(&data[within as usize..within as usize + chunk]);
                }
            }
            pos += chunk as u64;
            buf = tail;
        }
        Ok(())
    }

    fn size(&self) -> u64 {
        self.virtual_size
    }
}
