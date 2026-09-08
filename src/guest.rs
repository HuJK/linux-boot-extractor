//! Find the boot entries no bootloader config declares — today, a
//! **Windows** install.
//!
//! [`boot::scan`](crate::boot::scan) reads one filesystem and finds what
//! the configs on it describe. That misses a whole class of bootable
//! image: Windows keeps its system files on NTFS (which we deliberately
//! don't read) and, on a Windows-only image, no config file mentions it at
//! all. Such an image would look empty — indistinguishable from a Linux
//! one whose `/boot` we failed to parse.
//!
//! So this module walks the *disk* rather than a filesystem, and produces
//! the merged answer [`bootable`]: every partition that boots something,
//! Linux entries as the configs describe them, plus Windows folded in as
//! an entry of its own. A Windows entry carries what starting it takes —
//! firmware, architecture, boot manager — in place of a kernel.
//!
//! It comes from one of two places:
//!
//! * a **`chainloader` menu item** in an existing config: os-prober writes
//!   one per foreign OS, so a dual-boot GRUB menu already declares Windows
//!   and even says whether it is the default. The target is resolved here
//!   against the boot manager actually on the ESP;
//! * **detection**, when nothing declares it — the usual Windows-only
//!   image. What it keys on, all of it readable without touching NTFS:
//!   - the **ESP** is FAT: `/EFI/Microsoft/Boot/bootmgfw.efi` is the
//!     Windows boot manager and settles it, and its PE header gives the
//!     architecture, i.e. which firmware build the VMM needs;
//!   - **partition types** only Windows creates: Microsoft Reserved,
//!     Windows recovery, LDM, Storage Spaces;
//!   - **filesystem magic**: an NTFS or ReFS volume;
//!   - **boot code**: Windows' own MBR bootstrap chaining to an NTFS
//!     volume boot record that loads `BOOTMGR`/`NTLDR` — a BIOS install.
//!
//! [`Windows::evidence`] lists everything a verdict rests on, so it can be
//! judged rather than trusted. Windows volumes with no boot path at all
//! (`firmware: None`) are an attached data disk, not a guest: they produce
//! no entry, only an explanation for why the image boots nothing.

use crate::blockdev::ReadAt;
use crate::boot::{BootEntry, BootScan, Firmware, Source, WindowsBoot};
use crate::fsys::{self, FileSystem};
use crate::part::{self, Partition, TableKind};
use crate::Result;
use std::sync::Arc;

/// A Windows install, as seen from outside its filesystems.
#[derive(Debug, Clone, Default)]
pub struct Windows {
    /// `None` when Windows volumes were found but no boot path — an
    /// attached data disk rather than a bootable guest.
    pub firmware: Option<Firmware>,
    /// URI of the UEFI boot manager, `pN:/EFI/Microsoft/Boot/bootmgfw.efi`.
    pub boot_manager: Option<String>,
    /// URI of the UEFI boot configuration store (`BCD`), when present.
    pub bcd: Option<String>,
    /// Architecture from the boot manager's PE header: `x86_64`,
    /// `aarch64`, `i386` or `arm`. `None` when there is no boot manager to
    /// read (a BIOS install's loader lives on unreadable NTFS).
    pub arch: Option<&'static str>,
    /// Everything the verdict rests on, one line per finding.
    pub evidence: Vec<String>,
}

impl Windows {
    /// One-line description for diagnostics: `"uefi, x86_64"`.
    pub fn summary(&self) -> String {
        let firmware = match self.firmware {
            Some(f) => f.to_string(),
            None => "no boot manager found".to_string(),
        };
        match self.arch {
            Some(arch) => format!("{firmware}, {arch}"),
            None => firmware,
        }
    }
}

/// One partition that boots something, with the handle needed to read the
/// files its entries name.
pub struct PartitionBoot {
    pub partition: Partition,
    /// Filesystem as detected: `"vfat"`, `"ext4"`, `"ntfs"`, ...
    pub fs_type: &'static str,
    /// Open handle, or `None` for a partition we could only sniff (NTFS):
    /// a Windows entry has no per-entry files to probe anyway.
    pub fs: Option<Box<dyn FileSystem>>,
    pub scan: BootScan,
}

/// Every partition that boots something: the entries its configs declare,
/// plus the Windows install as an entry of its own. Partitions that boot
/// nothing are left out, so an empty result means the image boots nothing
/// we understand — ask [`windows`] why.
pub fn bootable<D: ReadAt + 'static>(disk: &Arc<D>) -> Result<Vec<PartitionBoot>> {
    let mut walk = walk(disk, true)?;
    let win = walk.windows.take();
    let bm_path = win.as_ref().and_then(|w| w.boot_manager.as_deref()).map(path_of);

    // A `chainloader` item is kept only if we can say what it starts: the
    // boot manager we found on the ESP. Anything else (another loader,
    // `chainloader +1`) goes, as a kernel-less entry always did.
    let mut declared = false;
    for pb in &mut walk.parts {
        let keep: Vec<bool> = pb
            .scan
            .entries
            .iter_mut()
            .map(|e| {
                let Some(target) = e.chainloader.as_deref() else { return true };
                if !bm_path.is_some_and(|bm| same_file(target, bm)) {
                    return false;
                }
                e.windows = Some(WindowsBoot {
                    firmware: Firmware::Uefi, // it chainloads an EFI binary
                    arch: win.as_ref().and_then(|w| w.arch),
                    loader: win.as_ref().and_then(|w| w.boot_manager.clone()),
                });
                declared = true;
                true
            })
            .collect();
        let (entries, default) =
            crate::boot::retain(std::mem::take(&mut pb.scan.entries), pb.scan.default, &keep);
        pb.scan.entries = entries;
        pb.scan.default = default;
    }

    // Nothing declared it, but it is there and bootable: add the entry.
    if !declared
        && let Some(win) = &win
        && let Some(firmware) = win.firmware
    {
        let host = win
            .boot_manager
            .as_deref()
            .and_then(uri_partition)
            .or(walk.bios_part);
        if let Some(index) = host
            && let Some(pb) = walk.parts.iter_mut().find(|pb| pb.partition.index == index)
        {
            pb.scan.entries.push(BootEntry {
                title: Some("Windows Boot Manager".to_string()),
                id: Some("windows".to_string()),
                source: Source::Windows,
                windows: Some(WindowsBoot {
                    firmware,
                    arch: win.arch,
                    loader: win.boot_manager.clone(),
                }),
                ..Default::default()
            });
            // Nothing else on that partition boots, so it is the default.
            pb.scan.default.get_or_insert(pb.scan.entries.len() - 1);
        }
    }

    walk.parts.retain(|pb| !pb.scan.entries.is_empty());
    Ok(walk.parts)
}

/// The Windows evidence on its own — for explaining an image that boots
/// nothing, where a full [`bootable`] scan has already come up empty.
pub fn windows<D: ReadAt + 'static>(disk: &Arc<D>) -> Result<Option<Windows>> {
    Ok(walk(disk, false)?.windows)
}

/// Partition types only Windows creates. "Microsoft basic data" is
/// deliberately absent: it is the generic "holds a filesystem" type and
/// says nothing on its own.
const WINDOWS_TYPES: &[&str] = &[
    part::gpt_type::MS_RESERVED,
    part::gpt_type::WINDOWS_RECOVERY,
    part::gpt_type::MS_LDM_METADATA,
    part::gpt_type::MS_LDM_DATA,
    part::gpt_type::MS_STORAGE_SPACES,
    "0x27", // MBR hidden NTFS: the recovery partition
];

/// Path components of the UEFI Windows boot manager and its configuration
/// store, relative to the ESP root.
const BOOT_MANAGER: &[&str] = &["EFI", "Microsoft", "Boot", "bootmgfw.efi"];
const BCD: &[&str] = &["EFI", "Microsoft", "Boot", "BCD"];

/// The NTFS volume boot record is 16 sectors; its boot code and messages
/// (`BOOTMGR is missing`, ...) fit well inside that.
const VBR_LEN: u64 = 16 * 512;

struct Walk {
    parts: Vec<PartitionBoot>,
    windows: Option<Windows>,
    /// Partition of a BIOS install's system volume, to hang its entry on.
    bios_part: Option<usize>,
}

/// One pass over the disk: what every partition is, and — with
/// `want_entries` — what the configs on it declare.
///
/// `'static`: each partition is handed to `fsys` as a `Box<dyn ReadAt>`.
fn walk<D: ReadAt + 'static>(disk: &Arc<D>, want_entries: bool) -> Result<Walk> {
    let table = part::scan(disk.as_ref())?;
    let mut parts: Vec<PartitionBoot> = Vec::new();
    let mut evidence: Vec<String> = Vec::new();
    let mut boot_manager = None;
    let mut bcd = None;
    let mut arch = None;
    let mut ntfs_loader = None; // NTFS volume that chains to bootmgr/ntldr
    let mut volumes = false; // an NTFS/ReFS volume exists at all
    let mut win_type = false; // a Windows-only partition type

    // Disk level: the bootstrap code Windows writes into the MBR.
    let mbr_bootstrap = matches!(table.kind, TableKind::Mbr) && {
        let mut sector = [0u8; 512];
        disk.check_bounds(0, 512).is_ok()
            && disk.read_at(0, &mut sector).is_ok()
            && is_windows_bootstrap(&sector)
    };
    if mbr_bootstrap {
        evidence.push("mbr: Windows bootstrap code".into());
    }

    for p in &table.partitions {
        if WINDOWS_TYPES.contains(&p.type_id.as_str()) {
            win_type = true;
            evidence.push(format!("p{}: {} partition", p.index, p.kind));
        }

        let dev: Box<dyn ReadAt> = Box::new(p.open(Arc::clone(disk)));
        let Some(fs_type) = fsys::detect(&dev).unwrap_or(None) else { continue };
        match fs_type {
            "ntfs" | "refs" => {
                volumes = true;
                match vbr_loader(&dev) {
                    // Every Windows-formatted NTFS volume carries this boot
                    // code, data volumes included — it marks a BIOS boot
                    // path only together with a Windows MBR bootstrap.
                    Some(loader) => {
                        ntfs_loader.get_or_insert(p.index);
                        evidence.push(format!(
                            "p{}: {fs_type} volume, VBR chains to {loader}",
                            p.index
                        ));
                    }
                    None => evidence.push(format!("p{}: {fs_type} volume", p.index)),
                }
                parts.push(PartitionBoot {
                    partition: p.clone(),
                    fs_type,
                    fs: None,
                    scan: BootScan::default(),
                });
            }
            "ext4" | "vfat" => {
                let Ok(fs) = fsys::open(dev) else { continue };
                if let Some(path) = find_path(fs.as_ref(), BOOT_MANAGER) {
                    evidence.push(format!("p{}: Windows boot manager {path}", p.index));
                    if arch.is_none() {
                        arch = fs
                            .read_prefix(&path, 4096)
                            .ok()
                            .and_then(|head| pe_arch(&head));
                    }
                    boot_manager.get_or_insert(uri(p.index, &path));
                }
                if let Some(path) = find_path(fs.as_ref(), BCD) {
                    evidence.push(format!("p{}: boot configuration store {path}", p.index));
                    bcd.get_or_insert(uri(p.index, &path));
                }
                let scan = if want_entries {
                    crate::boot::scan(fs.as_ref()).unwrap_or_default()
                } else {
                    BootScan::default()
                };
                parts.push(PartitionBoot {
                    partition: p.clone(),
                    fs_type,
                    fs: Some(fs),
                    scan,
                });
            }
            _ => {}
        }
    }

    // A BIOS install is Windows' own MBR bootstrap chaining to an NTFS
    // volume that loads bootmgr/ntldr. Either half alone is too weak: the
    // bootstrap survives an OS replacement, and the VBR code ships with
    // every Windows-formatted volume.
    let bios_part = ntfs_loader.filter(|_| mbr_bootstrap);
    let firmware = match (boot_manager.is_some(), bios_part.is_some()) {
        (true, true) => Some(Firmware::Both),
        (true, false) => Some(Firmware::Uefi),
        (false, true) => Some(Firmware::Bios),
        (false, false) => None,
    };

    // Enough to call it Windows: a boot path, a partition type only
    // Windows creates, its boot configuration store, or an NTFS/ReFS
    // volume. (Not exFAT — it is the interchange format of every camera
    // and SD card, and says nothing about the guest.)
    let is_windows = firmware.is_some() || win_type || bcd.is_some() || volumes;
    let windows = is_windows.then_some(Windows {
        firmware,
        boot_manager,
        bcd,
        arch,
        evidence,
    });
    Ok(Walk { parts, windows, bios_part })
}

fn uri(part: usize, path: &str) -> String {
    format!("p{part}:{path}")
}

/// Partition index of a `pN:/path` URI.
fn uri_partition(uri: &str) -> Option<usize> {
    uri.strip_prefix('p')?.split_once(':')?.0.parse().ok()
}

/// Path half of a `pN:/path` URI.
fn path_of(uri: &str) -> &str {
    uri.split_once(':').map(|(_, path)| path).unwrap_or(uri)
}

/// Do two config paths name the same file? Compared case-insensitively
/// (the ESP is FAT), with `\` accepted for `/`, and by base name too: a
/// config may reach the boot manager by a path we spell differently.
fn same_file(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").trim_start_matches('/').to_ascii_lowercase();
    let (a, b) = (norm(a), norm(b));
    let base = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    a == b || base(&a) == base(&b)
}

/// Resolve `/a/b/c` one component at a time, ignoring case. FAT lookups
/// are case-insensitive already, but the walk also covers an ESP read
/// through another backend, and image builders vary the spelling (`EFI` /
/// `efi`, `Boot` / `boot`). Returns the path as actually spelled on disk.
fn find_path(fs: &dyn FileSystem, parts: &[&str]) -> Option<String> {
    let mut path = String::new();
    for part in parts {
        let dir = if path.is_empty() { "/" } else { path.as_str() };
        let found = fs
            .read_dir(dir)
            .ok()?
            .into_iter()
            .find(|e| e.name.eq_ignore_ascii_case(part))?;
        path = format!("{path}/{}", found.name);
    }
    Some(path)
}

/// The loader an NTFS volume boot record chains to, from the file name
/// embedded in its boot code.
fn vbr_loader<D: ReadAt>(dev: &D) -> Option<&'static str> {
    let len = dev.size().min(VBR_LEN) as usize;
    let mut buf = vec![0u8; len];
    dev.read_at(0, &mut buf).ok()?;
    if contains(&buf, b"BOOTMGR") {
        Some("bootmgr") // Vista and newer
    } else if contains(&buf, b"NTLDR") {
        Some("ntldr") // XP / 2003
    } else {
        None
    }
}

/// Messages in the MBR bootstrap Windows (NT 5 through 10) writes. GRUB
/// and syslinux put their own code — and their own strings — there.
const MBR_STRINGS: &[&[u8]] = &[
    b"Invalid partition table",
    b"Error loading operating system",
    b"Missing operating system",
];

/// True if the MBR's 446-byte bootstrap area looks like Windows'. Two of
/// the three messages are enough: the set differs slightly by version.
fn is_windows_bootstrap(sector: &[u8]) -> bool {
    let code = &sector[..446.min(sector.len())];
    MBR_STRINGS.iter().filter(|s| contains(code, s)).count() >= 2
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Architecture from a PE/COFF binary's machine field — for an EFI
/// executable this is the firmware architecture the guest needs.
fn pe_arch(data: &[u8]) -> Option<&'static str> {
    if data.get(0..2)? != b"MZ" {
        return None;
    }
    let pe = u32::from_le_bytes(data.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if data.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let machine = u16::from_le_bytes(data.get(pe + 4..pe + 6)?.try_into().ok()?);
    Some(match machine {
        0x014c => "i386",
        0x8664 => "x86_64",
        0xaa64 => "aarch64",
        0x01c4 => "arm", // ARMNT (Windows RT / 32-bit ARM)
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::tests::MockFs;

    /// Minimal PE: "MZ", the header offset at 0x3c, then "PE\0\0" + machine.
    fn pe(machine: u16) -> Vec<u8> {
        let pe_off = 0x80usize;
        let mut v = vec![0u8; pe_off + 8];
        v[0..2].copy_from_slice(b"MZ");
        v[0x3c..0x40].copy_from_slice(&(pe_off as u32).to_le_bytes());
        v[pe_off..pe_off + 4].copy_from_slice(b"PE\0\0");
        v[pe_off + 4..pe_off + 6].copy_from_slice(&machine.to_le_bytes());
        v
    }

    #[test]
    fn pe_machine_types() {
        assert_eq!(pe_arch(&pe(0x8664)), Some("x86_64"));
        assert_eq!(pe_arch(&pe(0xaa64)), Some("aarch64"));
        assert_eq!(pe_arch(&pe(0x014c)), Some("i386"));
        assert_eq!(pe_arch(&pe(0x5678)), None); // unknown machine
        assert_eq!(pe_arch(b"not a PE at all"), None);
        assert_eq!(pe_arch(&pe(0x8664)[..0x20]), None); // truncated read
    }

    #[test]
    fn windows_mbr_bootstrap_needs_two_messages() {
        let mut sector = [0u8; 512];
        sector[10..33].copy_from_slice(b"Invalid partition table");
        assert!(!is_windows_bootstrap(&sector), "one message is not enough");
        sector[40..64].copy_from_slice(b"Missing operating system");
        assert!(is_windows_bootstrap(&sector));

        // A message living outside the bootstrap area (in the partition
        // table or the 55aa marker region) must not count.
        let mut tail = [0u8; 512];
        tail[446..469].copy_from_slice(b"Invalid partition table");
        tail[470..494].copy_from_slice(b"Missing operating system");
        assert!(!is_windows_bootstrap(&tail));
    }

    #[test]
    fn finds_boot_manager_whatever_the_case() {
        let fs = MockFs::new([
            ("/EFI/Microsoft/Boot/bootmgfw.efi", "MZ"),
            ("/EFI/Microsoft/Boot/BCD", "hive"),
        ]);
        assert_eq!(
            find_path(&fs, BOOT_MANAGER).as_deref(),
            Some("/EFI/Microsoft/Boot/bootmgfw.efi")
        );
        assert_eq!(find_path(&fs, BCD).as_deref(), Some("/EFI/Microsoft/Boot/BCD"));

        let lower = MockFs::new([("/efi/microsoft/boot/BOOTMGFW.EFI", "MZ")]);
        assert_eq!(
            find_path(&lower, BOOT_MANAGER).as_deref(),
            Some("/efi/microsoft/boot/BOOTMGFW.EFI")
        );
    }

    #[test]
    fn linux_esp_has_no_boot_manager() {
        let fs = MockFs::new([
            ("/EFI/BOOT/BOOTAA64.EFI", "MZ"),
            ("/EFI/debian/grub.cfg", "menuentry 'Debian' {}"),
        ]);
        assert_eq!(find_path(&fs, BOOT_MANAGER), None);
        assert_eq!(find_path(&fs, BCD), None);
    }

    #[test]
    fn chainloader_targets_match_the_boot_manager() {
        let found = "/EFI/Microsoft/Boot/bootmgfw.efi";
        assert!(same_file("/EFI/Microsoft/Boot/bootmgfw.efi", found));
        assert!(same_file("/efi/microsoft/boot/bootmgfw.efi", found)); // FAT case
        assert!(same_file("\\EFI\\Microsoft\\Boot\\bootmgfw.efi", found)); // BCD style
        assert!(same_file("/EFI/MICROSOFT/BOOT/BOOTMGFW.EFI", found));
        assert!(!same_file("/EFI/ubuntu/grubx64.efi", found));
        assert!(!same_file("+1", found)); // chainload a boot sector
    }

    #[test]
    fn uri_parts() {
        assert_eq!(uri(2, "/EFI/x.efi"), "p2:/EFI/x.efi");
        assert_eq!(uri_partition("p12:/EFI/x.efi"), Some(12));
        assert_eq!(uri_partition("/EFI/x.efi"), None);
        assert_eq!(path_of("p1:/EFI/x.efi"), "/EFI/x.efi");
    }

    #[test]
    fn summary_reports_what_is_known() {
        let w = Windows {
            firmware: Some(Firmware::Uefi),
            arch: Some("x86_64"),
            ..Default::default()
        };
        assert_eq!(w.summary(), "uefi, x86_64");
        assert_eq!(Windows::default().summary(), "no boot manager found");
    }
}
