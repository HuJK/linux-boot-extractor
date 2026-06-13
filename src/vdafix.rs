//! Rewrite `root=`/`resume=` device references in a kernel cmdline to
//! `PARTUUID=` form, so a cmdline written for e.g. `/dev/sda2` boots
//! unchanged when the disk shows up as `/dev/vda2` under virtio.
//!
//! The device name's partition *number* is looked up in the partition
//! table of the image the cmdline came from — a single-disk assumption:
//! `root=/dev/sdb3` on a multi-disk install maps to the wrong partition,
//! but such cmdlines are rare and the caller can always disable the fix.
//! Values already using `UUID=`/`PARTUUID=`/`LABEL=` are left alone, as is
//! a whole-disk `root=/dev/sda`. `PARTUUID=` is resolved by the kernel
//! itself, so the rewrite works even without an initramfs.

use crate::part::PartitionTable;

/// Kernel parameters whose value names the block device to fix.
const PARAMS: &[&str] = &["root", "resume"];

/// Returns the rewritten cmdline, or `None` when nothing needed fixing or
/// nothing could be resolved against `table`. Whitespace is preserved.
pub fn fix_cmdline(cmdline: &str, table: &PartitionTable) -> Option<String> {
    let mut out = String::with_capacity(cmdline.len() + 32);
    let mut changed = false;
    let mut rest = cmdline;
    while !rest.is_empty() {
        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (token, tail) = rest.split_at(token_end);
        match fix_token(token, table) {
            Some(fixed) => {
                out.push_str(&fixed);
                changed = true;
            }
            None => out.push_str(token),
        }
        let ws_end = tail
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(tail.len());
        out.push_str(&tail[..ws_end]);
        rest = &tail[ws_end..];
    }
    changed.then_some(out)
}

fn fix_token(token: &str, table: &PartitionTable) -> Option<String> {
    let (key, value) = token.split_once('=')?;
    if !PARAMS.contains(&key) {
        return None;
    }
    let n = partition_number(value)?;
    let part = table.partitions.iter().find(|p| p.index == n)?;
    let uuid = part.part_uuid.as_ref()?;
    Some(format!("{key}=PARTUUID={uuid}"))
}

/// Partition number of a `/dev/...` block device path, for the styles a
/// distro installer would write: `sdXN`/`vdXN`/`hdXN`/`xvdXN`, and
/// `nvme0n1pN`/`mmcblk0pN`. Anything else (no `/dev/` prefix, UUID=,
/// whole-disk without partition digits) returns `None`.
fn partition_number(dev: &str) -> Option<usize> {
    let name = dev.strip_prefix("/dev/")?;
    // Trailing digits are the partition number...
    let digits_at = name
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    if digits_at == 0 || digits_at >= name.len() {
        return None;
    }
    let num: usize = name[digits_at..].parse().ok()?;
    let stem = &name[..digits_at];
    // ...but only with a disk-name stem we recognize in front of them.
    let valid = if let Some(base) = stem.strip_suffix('p') {
        // nvme0n1p2 / mmcblk0p1: digits follow a 'p' separator.
        base.starts_with("nvme") || base.starts_with("mmcblk")
    } else {
        ["sd", "vd", "hd", "xvd"].iter().any(|prefix| {
            stem.strip_prefix(prefix)
                .is_some_and(|l| !l.is_empty() && l.bytes().all(|c| c.is_ascii_lowercase()))
        })
    };
    valid.then_some(num)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::part::{Partition, TableKind};

    fn table(parts: &[(usize, Option<&str>)]) -> PartitionTable {
        PartitionTable {
            kind: TableKind::Gpt,
            partitions: parts
                .iter()
                .map(|&(index, uuid)| Partition {
                    index,
                    start_byte: 0,
                    size_bytes: 0,
                    kind: "Linux filesystem".into(),
                    name: None,
                    probe_worthy: true,
                    part_uuid: uuid.map(str::to_string),
                })
                .collect(),
        }
    }

    #[test]
    fn partition_numbers() {
        assert_eq!(partition_number("/dev/sda2"), Some(2));
        assert_eq!(partition_number("/dev/vdb13"), Some(13));
        assert_eq!(partition_number("/dev/xvda1"), Some(1));
        assert_eq!(partition_number("/dev/nvme0n1p3"), Some(3));
        assert_eq!(partition_number("/dev/mmcblk0p2"), Some(2));
        assert_eq!(partition_number("/dev/sda"), None); // whole disk
        assert_eq!(partition_number("/dev/nvme0n1"), None); // whole disk
        assert_eq!(partition_number("/dev/dm-0"), None);
        assert_eq!(partition_number("UUID=abcd"), None);
        assert_eq!(partition_number("/dev/sd2"), None); // no disk letter
    }

    #[test]
    fn rewrites_root_and_resume() {
        let t = table(&[(1, Some("aaaa-01")), (2, Some("bbbb-02"))]);
        let fixed = fix_cmdline("root=/dev/sda2 ro  resume=/dev/sda1 quiet", &t);
        assert_eq!(
            fixed.as_deref(),
            Some("root=PARTUUID=bbbb-02 ro  resume=PARTUUID=aaaa-01 quiet")
        );
    }

    #[test]
    fn leaves_portable_and_unresolvable_alone() {
        let t = table(&[(2, Some("bbbb-02"))]);
        assert_eq!(fix_cmdline("root=UUID=1234 quiet", &t), None);
        assert_eq!(fix_cmdline("root=PARTUUID=bbbb-02", &t), None);
        assert_eq!(fix_cmdline("root=LABEL=cloudimg-rootfs", &t), None);
        // Partition 3 not in the table; partition 2 has no part_uuid.
        assert_eq!(fix_cmdline("root=/dev/sda3", &t), None);
        let no_uuid = table(&[(2, None)]);
        assert_eq!(fix_cmdline("root=/dev/sda2", &no_uuid), None);
        // rootfstype= must not be mistaken for root=.
        assert_eq!(fix_cmdline("rootfstype=ext4", &t), None);
    }

    #[test]
    fn partial_fix_still_counts() {
        let t = table(&[(2, Some("bbbb-02"))]);
        let fixed = fix_cmdline("root=/dev/vda2 resume=/dev/vda9", &t);
        assert_eq!(fixed.as_deref(), Some("root=PARTUUID=bbbb-02 resume=/dev/vda9"));
    }
}
