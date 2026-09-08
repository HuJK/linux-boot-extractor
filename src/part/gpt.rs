use super::{Partition, PartitionTable, TableKind, SECTOR};
use crate::blockdev::ReadAt;
use crate::{Error, Result};

const SIGNATURE: &[u8; 8] = b"EFI PART";

// Well-known partition type GUIDs (canonical string form).
const ESP: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
const LINUX_FS: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";
const LINUX_XBOOTLDR: &str = "bc13c2ff-59e6-4262-a352-b275fd6f7172";
const LINUX_LVM: &str = "e6d6d379-f507-44c2-a23c-238f2a3df928";
const LINUX_SWAP: &str = "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f";
const LINUX_ROOT_X86_64: &str = "4f68bce3-e8cd-4db1-96e7-fbcaf984b709";
const LINUX_ROOT_ARM64: &str = "b921b045-1df0-41c3-af44-4c6f280d3fae";
const BIOS_BOOT: &str = "21686148-6449-6e6f-744e-656564454649";
// Microsoft. Only Windows creates MSR/WinRE/LDM/Storage-Spaces partitions,
// so `guest` reads them as evidence of a Windows guest; "basic data" is
// weaker (it is also just the generic "some filesystem" type).
pub const MS_RESERVED: &str = "e3c9e316-0b5c-4db8-817d-f92df00215ae";
pub const MS_BASIC_DATA: &str = "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7";
pub const WINDOWS_RECOVERY: &str = "de94bba4-06d1-4d40-a16a-bfd50179d6ac";
pub const MS_LDM_METADATA: &str = "5808c8aa-7e8f-42e0-85d2-e1e90434cfb3";
pub const MS_LDM_DATA: &str = "af9b60a0-1431-4f62-bc68-3311714a69ad";
pub const MS_STORAGE_SPACES: &str = "e75caf8f-f680-4cee-afa3-b001e56efc2d";

fn type_name(guid: &str) -> (&'static str, bool) {
    match guid {
        ESP => ("EFI System", true),
        LINUX_FS => ("Linux filesystem", true),
        LINUX_XBOOTLDR => ("Linux extended boot", true),
        LINUX_ROOT_X86_64 => ("Linux root (x86-64)", true),
        LINUX_ROOT_ARM64 => ("Linux root (ARM64)", true),
        MS_BASIC_DATA => ("Microsoft basic data", true), // ESP-less Windows keeps nothing here we read, but a plain FAT data partition might hold configs
        MS_RESERVED => ("Microsoft reserved", false),
        WINDOWS_RECOVERY => ("Windows recovery", false), // NTFS
        MS_LDM_METADATA => ("Microsoft LDM metadata", false),
        MS_LDM_DATA => ("Microsoft LDM data", false),
        MS_STORAGE_SPACES => ("Microsoft Storage Spaces", false),
        LINUX_LVM => ("Linux LVM", false), // probe once LVM support lands
        LINUX_SWAP => ("Linux swap", false),
        BIOS_BOOT => ("BIOS boot", false),
        _ => ("unknown", false),
    }
}

/// Render the 16-byte on-disk GUID in canonical form (first three fields
/// are little-endian on disk).
fn format_guid(g: &[u8]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g[3], g[2], g[1], g[0], g[5], g[4], g[7], g[6],
        g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15],
    )
}

/// Returns `Ok(None)` when there is no GPT signature at LBA 1.
pub fn scan<D: ReadAt>(disk: &D) -> Result<Option<PartitionTable>> {
    if disk.size() < 2 * SECTOR + 92 {
        return Ok(None);
    }
    let mut header = [0u8; 92];
    disk.read_at(SECTOR, &mut header)?;
    if &header[..8] != SIGNATURE {
        return Ok(None);
    }

    let le64 = |off: usize| u64::from_le_bytes(header[off..off + 8].try_into().unwrap());
    let le32 = |off: usize| u32::from_le_bytes(header[off..off + 4].try_into().unwrap());

    let entries_lba = le64(72);
    let num_entries = le32(80) as u64;
    let entry_size = le32(84) as u64;

    if !(128..=4096).contains(&entry_size) || num_entries > 4096 {
        return Err(Error::Gpt(format!(
            "implausible GPT: {num_entries} entries of {entry_size} bytes"
        )));
    }

    let mut table = vec![0u8; (num_entries * entry_size) as usize];
    disk.read_at(entries_lba * SECTOR, &mut table)?;

    let mut partitions = Vec::new();
    for i in 0..num_entries as usize {
        let e = &table[i * entry_size as usize..][..entry_size as usize];
        let type_guid = &e[..16];
        if type_guid.iter().all(|&b| b == 0) {
            continue;
        }
        let first_lba = u64::from_le_bytes(e[32..40].try_into().unwrap());
        let last_lba = u64::from_le_bytes(e[40..48].try_into().unwrap());
        if last_lba < first_lba {
            continue;
        }

        let name_utf16: Vec<u16> = e[56..128]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes(c.try_into().unwrap()))
            .take_while(|&c| c != 0)
            .collect();
        let name = if name_utf16.is_empty() {
            None
        } else {
            Some(String::from_utf16_lossy(&name_utf16))
        };

        let guid = format_guid(type_guid);
        let (kind, probe_worthy) = type_name(&guid);
        let kind = if kind == "unknown" { guid.clone() } else { kind.to_string() };

        let unique_guid = &e[16..32];
        let part_uuid = if unique_guid.iter().all(|&b| b == 0) {
            None
        } else {
            Some(format_guid(unique_guid))
        };

        partitions.push(Partition {
            index: i + 1,
            start_byte: first_lba * SECTOR,
            size_bytes: (last_lba - first_lba + 1) * SECTOR,
            kind,
            type_id: guid,
            name,
            probe_worthy,
            part_uuid,
        });
    }

    Ok(Some(PartitionTable { kind: TableKind::Gpt, partitions }))
}
