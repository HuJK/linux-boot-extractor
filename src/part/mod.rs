//! Partition table parsing (MBR incl. extended partitions, GPT).
//!
//! LVM is deliberately a later milestone: /boot is almost always a plain
//! partition even on LVM installs. When added, it will slot in here as
//! another layer that turns PV slices into LV `ReadAt` devices.

mod gpt;
mod mbr;

use crate::blockdev::{ReadAt, Slice};
use crate::Result;
use std::sync::Arc;

pub const SECTOR: u64 = 512;

#[derive(Debug, Clone)]
pub enum TableKind {
    Gpt,
    Mbr,
    /// No table; the whole disk is (possibly) one filesystem.
    None,
}

#[derive(Debug, Clone)]
pub struct Partition {
    /// 1-based index as a user would address it (GPT slot / MBR position).
    pub index: usize,
    pub start_byte: u64,
    pub size_bytes: u64,
    /// Human-readable type, e.g. "EFI System", "Linux filesystem", "0x83".
    pub kind: String,
    /// GPT partition name, if any.
    pub name: Option<String>,
    /// True for types worth probing for boot files (ESP, Linux, XBOOTLDR).
    pub probe_worthy: bool,
    /// What Linux calls PARTUUID: the GPT unique partition GUID, or
    /// `<mbr-disk-signature>-<NN>` for MBR disks. `None` when the table
    /// provides neither (zeroed GUID, MBR without a signature).
    pub part_uuid: Option<String>,
}

impl Partition {
    /// Expose this partition as its own read-only device.
    pub fn open<D: ReadAt>(&self, disk: Arc<D>) -> Slice<Arc<D>> {
        Slice::new(disk, self.start_byte, self.size_bytes)
    }
}

pub struct PartitionTable {
    pub kind: TableKind,
    pub partitions: Vec<Partition>,
}

/// Detect and parse the partition table. A disk with no recognizable table
/// returns `TableKind::None` and a single pseudo-partition spanning the disk,
/// so callers can treat filesystem-on-bare-disk images uniformly.
pub fn scan<D: ReadAt>(disk: &D) -> Result<PartitionTable> {
    if let Some(table) = gpt::scan(disk)? {
        return Ok(table);
    }
    if let Some(table) = mbr::scan(disk)? {
        return Ok(table);
    }
    Ok(PartitionTable {
        kind: TableKind::None,
        partitions: vec![Partition {
            index: 1,
            start_byte: 0,
            size_bytes: disk.size(),
            kind: "whole disk".into(),
            name: None,
            probe_worthy: true,
            part_uuid: None,
        }],
    })
}
