use super::{Partition, PartitionTable, TableKind, SECTOR};
use crate::blockdev::ReadAt;
use crate::Result;

const TYPE_EMPTY: u8 = 0x00;
const TYPE_EXTENDED_CHS: u8 = 0x05;
const TYPE_EXTENDED_LBA: u8 = 0x0f;
const TYPE_GPT_PROTECTIVE: u8 = 0xee;

fn type_name(t: u8) -> (String, bool) {
    match t {
        0x0b | 0x0c => ("FAT32".into(), true),
        0x0e => ("FAT16 LBA".into(), true),
        0x82 => ("Linux swap".into(), false),
        0x83 => ("Linux".into(), true),
        0x8e => ("Linux LVM".into(), false), // probe once LVM support lands
        0xef => ("EFI System".into(), true),
        other => (format!("{other:#04x}"), false),
    }
}

struct RawEntry {
    type_byte: u8,
    start_lba: u32,
    sectors: u32,
}

fn parse_entries(sector: &[u8; 512]) -> Option<[RawEntry; 4]> {
    if sector[510..512] != [0x55, 0xaa] {
        return None;
    }
    Some(std::array::from_fn(|i| {
        let e = &sector[446 + i * 16..446 + (i + 1) * 16];
        RawEntry {
            type_byte: e[4],
            start_lba: u32::from_le_bytes(e[8..12].try_into().unwrap()),
            sectors: u32::from_le_bytes(e[12..16].try_into().unwrap()),
        }
    }))
}

/// Returns `Ok(None)` if there is no valid MBR (or it is GPT-protective —
/// the GPT scanner runs first, so seeing 0xee here means a broken GPT).
pub fn scan<D: ReadAt>(disk: &D) -> Result<Option<PartitionTable>> {
    if disk.size() < SECTOR {
        return Ok(None);
    }
    let mut sector = [0u8; 512];
    disk.read_at(0, &mut sector)?;
    let Some(entries) = parse_entries(&sector) else {
        return Ok(None);
    };

    // A boot sector of a bare FAT filesystem also ends in 55aa; require at
    // least one plausible partition entry to call it an MBR.
    let plausible = entries
        .iter()
        .any(|e| e.type_byte != TYPE_EMPTY && e.start_lba > 0 && e.sectors > 0);
    if !plausible {
        return Ok(None);
    }

    // NT disk signature; the kernel derives MBR PARTUUIDs from it as
    // `%08x-%02x` (partition number in hex). All-zero means "not set".
    let disksig = u32::from_le_bytes(sector[440..444].try_into().unwrap());
    let part_uuid =
        |index: usize| (disksig != 0).then(|| format!("{disksig:08x}-{index:02x}"));

    let mut partitions = Vec::new();
    let mut extended_start: Option<u64> = None;

    for (i, e) in entries.iter().enumerate() {
        if e.type_byte == TYPE_EMPTY || e.sectors == 0 {
            continue;
        }
        if e.type_byte == TYPE_GPT_PROTECTIVE {
            continue;
        }
        if matches!(e.type_byte, TYPE_EXTENDED_CHS | TYPE_EXTENDED_LBA) {
            extended_start = Some(e.start_lba as u64 * SECTOR);
            continue;
        }
        let (kind, probe_worthy) = type_name(e.type_byte);
        partitions.push(Partition {
            index: i + 1,
            start_byte: e.start_lba as u64 * SECTOR,
            size_bytes: e.sectors as u64 * SECTOR,
            kind,
            name: None,
            probe_worthy,
            part_uuid: part_uuid(i + 1),
        });
    }

    // Walk the EBR chain; logical partitions are numbered from 5.
    if let Some(ext_base) = extended_start {
        let mut ebr_offset = ext_base;
        let mut index = 5;
        for _ in 0..128 {
            // hard cap against malicious/corrupt chains
            let mut ebr = [0u8; 512];
            if disk.check_bounds(ebr_offset, 512).is_err() {
                break;
            }
            disk.read_at(ebr_offset, &mut ebr)?;
            let Some(entries) = parse_entries(&ebr) else { break };

            let [data, link, ..] = entries;
            if data.type_byte != TYPE_EMPTY && data.sectors != 0 {
                let (kind, probe_worthy) = type_name(data.type_byte);
                partitions.push(Partition {
                    index,
                    start_byte: ebr_offset + data.start_lba as u64 * SECTOR,
                    size_bytes: data.sectors as u64 * SECTOR,
                    kind,
                    name: None,
                    probe_worthy,
                    part_uuid: part_uuid(index),
                });
                index += 1;
            }
            if matches!(link.type_byte, TYPE_EXTENDED_CHS | TYPE_EXTENDED_LBA) {
                ebr_offset = ext_base + link.start_lba as u64 * SECTOR;
            } else {
                break;
            }
        }
    }

    if partitions.is_empty() {
        return Ok(None);
    }
    Ok(Some(PartitionTable { kind: TableKind::Mbr, partitions }))
}
