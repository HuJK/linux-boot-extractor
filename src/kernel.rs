//! Unwrap a distro `vmlinuz` into a raw, direct-bootable arm64 `Image`.
//!
//! A direct-kernel-boot VMM (crosvm, `qemu -kernel`) loads the kernel
//! itself, with no bootloader in front of it. On arm64 it needs the raw
//! `Image`: a 64-byte header whose magic `ARMd` sits at offset 0x38. But
//! distros don't ship that as `vmlinuz` — normally GRUB or the EFI stub
//! decompresses the kernel at boot, so the on-disk `vmlinuz` is wrapped:
//!
//!   * plain **gzip** of the `Image` (`Image.gz`) — Debian/Ubuntu arm64;
//!   * **EFI zboot**: a small PE shell (marker `zimg` at offset 4) carrying
//!     a gzip/zstd/… compressed `Image` as its payload — Fedora arm64;
//!   * a **UKI** (systemd-stub Unified Kernel Image): a PE/EFI executable
//!     bundling kernel + initrd + cmdline + DTBs, with the real kernel in a
//!     `.linux` PE section (itself usually gzip/zboot) — newer Ubuntu arm64.
//!
//! An x86 `vmlinuz` is a self-extracting `bzImage` the VMM already groks,
//! and some arm64 images ship the raw `Image` directly; both are left
//! untouched. Unwrapping recurses — a UKI's `.linux` is unwrapped again —
//! so the result is always a raw `Image`.
//!
//! This module is deliberately split in two so a caller can keep `lbx`'s
//! default behaviour faithful (extract/copy bytes verbatim) and decide for
//! itself when to unwrap: [`compression`] *reports* the wrapper an image
//! uses (cheaply, from a header prefix), and [`to_bootable`] *performs* the
//! unwrapping only when asked.

use crate::{Error, Result};
use std::io::Read;

/// arm64 `Image` header: little-endian u32 magic `0x644d5241` (`"ARMd"`)
/// at byte offset 56.
const ARM64_MAGIC_OFFSET: usize = 56;
const ARM64_MAGIC: &[u8] = b"ARMd";

/// EFI zboot header (`Documentation/arch/arm64/booting.rst`): `"MZ"`, then a
/// `"zimg"` marker at offset 4, the payload's offset and size as u32 LE at
/// offsets 8 and 12, and a NUL-padded compression-type name at offset 24.
const ZIMG_MAGIC_OFFSET: usize = 4;
const ZIMG_MAGIC: &[u8] = b"zimg";
const ZBOOT_PAYLOAD_OFFSET: usize = 8;
const ZBOOT_PAYLOAD_SIZE: usize = 12;
const ZBOOT_COMP_TYPE: usize = 24;
const ZBOOT_COMP_TYPE_LEN: usize = 32;

/// PE/COFF (used by both raw arm64 `Image`s and UKIs): `"MZ"` at 0, the PE
/// header offset as u32 LE at 0x3c, then `"PE\0\0"` and the COFF header
/// (section count at +2, optional-header size at +16, section table after).
const PE_HEADER_OFFSET_AT: usize = 0x3c;
const COFF_NUM_SECTIONS_AT: usize = 2;
const COFF_OPT_HEADER_SIZE_AT: usize = 16;
const COFF_SIZE: usize = 20;
const SECTION_HEADER_SIZE: usize = 40;
const SECTION_RAW_SIZE_AT: usize = 16; // SizeOfRawData within a section header
const SECTION_RAW_PTR_AT: usize = 20; // PointerToRawData
/// systemd-stub UKI section holding the actual kernel.
const UKI_LINUX_SECTION: &[u8] = b".linux\0\0";

/// Smallest prefix [`compression`] needs to classify an image: enough for a
/// whole zboot header, a gzip member inflated past the arm64 magic, and a
/// PE section table (to spot a UKI's `.linux` section). Callers sniffing a
/// kernel only have to read this many bytes, not the whole file.
pub const SNIFF_LEN: usize = 8192;

/// How a `vmlinuz` is wrapped, when it needs unwrapping before a VMM can
/// direct-boot it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compression {
    /// Plain gzip of an arm64 `Image` (`Image.gz`).
    Gzip,
    /// EFI zboot PE shell; the string is the payload's codec name as the
    /// header declares it (e.g. `"gzip"`, `"zstd"`, `"lzma"`).
    Zboot(String),
    /// systemd-stub UKI; the kernel is in the `.linux` PE section (which is
    /// itself unwrapped — usually gzip or zboot).
    Uki,
}

impl Compression {
    /// Short label for diagnostics and the `kernel_compression` field, e.g.
    /// `"gzip"`, `"zboot+zstd"`, or `"uki"`.
    pub fn label(&self) -> String {
        match self {
            Compression::Gzip => "gzip".to_string(),
            Compression::Zboot(codec) => format!("zboot+{codec}"),
            Compression::Uki => "uki".to_string(),
        }
    }
}

fn is_arm64_image(data: &[u8]) -> bool {
    data.len() >= ARM64_MAGIC_OFFSET + ARM64_MAGIC.len()
        && &data[ARM64_MAGIC_OFFSET..ARM64_MAGIC_OFFSET + ARM64_MAGIC.len()] == ARM64_MAGIC
}

fn is_gzip(data: &[u8]) -> bool {
    data.starts_with(&[0x1f, 0x8b])
}

fn is_zboot(data: &[u8]) -> bool {
    data.len() >= ZIMG_MAGIC_OFFSET + ZIMG_MAGIC.len()
        && &data[ZIMG_MAGIC_OFFSET..ZIMG_MAGIC_OFFSET + ZIMG_MAGIC.len()] == ZIMG_MAGIC
}

fn u16_le(data: &[u8], at: usize) -> Option<u16> {
    let b = data.get(at..at + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_le(data: &[u8], at: usize) -> Option<u32> {
    let b = data.get(at..at + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Locate a UKI's `.linux` section in a PE/COFF file — its raw-data byte
/// `(offset, len)`. `None` when `data` isn't a PE or has no such section
/// (e.g. a plain arm64 `Image`, whose PE sections are `.text`/`.data`). The
/// header and section table sit in the first few KiB, so this works on a
/// [`SNIFF_LEN`] prefix for detection, or the whole file for extraction.
fn pe_linux_section(data: &[u8]) -> Option<(usize, usize)> {
    if data.get(0..2)? != b"MZ" {
        return None;
    }
    let pe_off = u32_le(data, PE_HEADER_OFFSET_AT)? as usize;
    if data.get(pe_off..pe_off + 4)? != b"PE\0\0" {
        return None;
    }
    let coff = pe_off + 4;
    let num_sections = u16_le(data, coff + COFF_NUM_SECTIONS_AT)? as usize;
    let opt_size = u16_le(data, coff + COFF_OPT_HEADER_SIZE_AT)? as usize;
    let table = coff + COFF_SIZE + opt_size;
    for i in 0..num_sections {
        let header = table + i * SECTION_HEADER_SIZE;
        if data.get(header..header + 8)? == UKI_LINUX_SECTION {
            let size = u32_le(data, header + SECTION_RAW_SIZE_AT)? as usize;
            let ptr = u32_le(data, header + SECTION_RAW_PTR_AT)? as usize;
            return Some((ptr, size));
        }
    }
    None
}

/// The NUL-terminated compression-type name from a zboot header.
fn zboot_comp_name(data: &[u8]) -> Option<&str> {
    let raw = data.get(ZBOOT_COMP_TYPE..ZBOOT_COMP_TYPE + ZBOOT_COMP_TYPE_LEN)?;
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    std::str::from_utf8(&raw[..end]).ok().filter(|s| !s.is_empty())
}

/// Whether a gzip member's first bytes inflate to an arm64 `Image`. Reads
/// only a header's worth of output, so a gzip that isn't a kernel (e.g. a
/// gzip-compressed initramfs) is rejected without inflating it in full.
fn gunzip_is_arm64(gz: &[u8]) -> bool {
    let mut head = [0u8; ARM64_MAGIC_OFFSET + ARM64_MAGIC.len()];
    flate2::read::GzDecoder::new(gz)
        .read_exact(&mut head)
        .is_ok()
        && is_arm64_image(&head)
}

/// Classify `head` (the start of a `vmlinuz`; pass at least [`SNIFF_LEN`]
/// bytes). `None` means no unwrapping is needed — an already-raw arm64
/// `Image`, an x86 `bzImage`, or a form we don't recognize — and the file
/// should be extracted verbatim. `Some` names the wrapper so the caller can
/// ask [`to_bootable`] to remove it. A recognized-but-unsupported zboot
/// codec is still reported here (honest attribute); the failure surfaces at
/// [`to_bootable`].
pub fn compression(head: &[u8]) -> Option<Compression> {
    if is_arm64_image(head) {
        return None; // already a raw Image
    }
    if is_zboot(head) {
        return Some(Compression::Zboot(
            zboot_comp_name(head).unwrap_or("unknown").to_string(),
        ));
    }
    if is_gzip(head) && gunzip_is_arm64(head) {
        return Some(Compression::Gzip);
    }
    if pe_linux_section(head).is_some() {
        return Some(Compression::Uki);
    }
    None
}

/// Unwrap `data` to a raw arm64 `Image` if it is a recognized wrapped
/// `vmlinuz`; otherwise return it unchanged. Errors only when the wrapper is
/// recognized but its codec isn't supported, so a caller that asked for
/// unwrapping gets a clear failure rather than an unbootable kernel.
pub fn to_bootable(data: Vec<u8>) -> Result<Vec<u8>> {
    to_bootable_inner(data, 0)
}

/// Wrappers can nest (a UKI's `.linux` is itself gzip/zboot), so unwrapping
/// recurses; the depth cap stops a pathological self-referential input.
fn to_bootable_inner(data: Vec<u8>, depth: u32) -> Result<Vec<u8>> {
    const MAX_DEPTH: u32 = 3;
    if depth >= MAX_DEPTH {
        return Err(Error::KernelDecompress("kernel wrappers nested too deep".into()));
    }
    match compression(&data) {
        None => Ok(data),
        Some(Compression::Gzip) => gunzip(&data),
        Some(Compression::Zboot(codec)) => unzboot(&data, &codec),
        Some(Compression::Uki) => {
            let (off, size) = pe_linux_section(&data)
                .ok_or_else(|| Error::KernelDecompress("UKI .linux section not found".into()))?;
            let end = off
                .checked_add(size)
                .ok_or_else(|| Error::KernelDecompress("UKI .linux size overflows".into()))?;
            let inner = data
                .get(off..end)
                .ok_or_else(|| {
                    Error::KernelDecompress(format!(
                        "UKI .linux [{off}..{end}] out of range (file is {} bytes)",
                        data.len()
                    ))
                })?
                .to_vec();
            to_bootable_inner(inner, depth + 1)
        }
    }
}

fn gunzip(gz: &[u8]) -> Result<Vec<u8>> {
    // Kernels inflate a few-fold; pre-size to keep reallocations down.
    let mut out = Vec::with_capacity(gz.len().saturating_mul(3));
    flate2::read::GzDecoder::new(gz)
        .read_to_end(&mut out)
        .map_err(|e| Error::KernelDecompress(format!("gzip: {e}")))?;
    Ok(out)
}

fn unzboot(data: &[u8], codec: &str) -> Result<Vec<u8>> {
    let off = read_u32_le(data, ZBOOT_PAYLOAD_OFFSET)? as usize;
    let size = read_u32_le(data, ZBOOT_PAYLOAD_SIZE)? as usize;
    let end = off.checked_add(size).ok_or_else(|| {
        Error::KernelDecompress("zboot payload size overflows".to_string())
    })?;
    let payload = data.get(off..end).ok_or_else(|| {
        Error::KernelDecompress(format!(
            "zboot payload [{off}..{end}] out of range (file is {} bytes)",
            data.len()
        ))
    })?;
    // The codec name is what the kernel build wrote; match the supported
    // ones, accepting the version-suffixed forms (e.g. "zstd22", "xzkern").
    if codec.starts_with("gzip") {
        gunzip(payload)
    } else if codec.starts_with("zstd") {
        unzstd(payload)
    } else {
        Err(Error::KernelDecompress(format!(
            "unsupported zboot kernel compression: {codec}"
        )))
    }
}

fn unzstd(payload: &[u8]) -> Result<Vec<u8>> {
    let mut dec = ruzstd::decoding::StreamingDecoder::new(payload)
        .map_err(|e| Error::KernelDecompress(format!("zstd: {e}")))?;
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| Error::KernelDecompress(format!("zstd: {e}")))?;
    Ok(out)
}

fn read_u32_le(data: &[u8], at: usize) -> Result<u32> {
    let b = data
        .get(at..at + 4)
        .ok_or_else(|| Error::KernelDecompress(format!("truncated zboot header at {at}")))?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A fake arm64 `Image`: arbitrary bytes with `ARMd` at offset 56.
    fn fake_image(len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len.max(ARM64_MAGIC_OFFSET + 4)];
        v[0] = b'M';
        v[1] = b'Z';
        v[ARM64_MAGIC_OFFSET..ARM64_MAGIC_OFFSET + 4].copy_from_slice(ARM64_MAGIC);
        v
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// Minimal EFI zboot wrapper around an already-compressed payload.
    fn zboot(codec: &str, payload: &[u8]) -> Vec<u8> {
        let payload_off: u32 = 64;
        let mut v = vec![0u8; payload_off as usize];
        v[0..2].copy_from_slice(b"MZ");
        v[ZIMG_MAGIC_OFFSET..ZIMG_MAGIC_OFFSET + 4].copy_from_slice(ZIMG_MAGIC);
        v[ZBOOT_PAYLOAD_OFFSET..ZBOOT_PAYLOAD_OFFSET + 4].copy_from_slice(&payload_off.to_le_bytes());
        v[ZBOOT_PAYLOAD_SIZE..ZBOOT_PAYLOAD_SIZE + 4]
            .copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let name = codec.as_bytes();
        v[ZBOOT_COMP_TYPE..ZBOOT_COMP_TYPE + name.len()].copy_from_slice(name);
        v.extend_from_slice(payload);
        v
    }

    /// Minimal systemd-stub UKI: a PE with a single `.linux` section whose
    /// raw data is `linux` (itself raw/gzip/zboot).
    fn uki(linux: &[u8]) -> Vec<u8> {
        let pe_off = 0x80usize;
        let coff = pe_off + 4;
        let table = coff + COFF_SIZE; // optional header size left 0
        let payload_off = 0x100usize;
        let mut v = vec![0u8; payload_off];
        v[0..2].copy_from_slice(b"MZ");
        v[PE_HEADER_OFFSET_AT..PE_HEADER_OFFSET_AT + 4].copy_from_slice(&(pe_off as u32).to_le_bytes());
        v[pe_off..pe_off + 4].copy_from_slice(b"PE\0\0");
        v[coff..coff + 2].copy_from_slice(&0xaa64u16.to_le_bytes()); // ARM64
        v[coff + COFF_NUM_SECTIONS_AT..coff + COFF_NUM_SECTIONS_AT + 2]
            .copy_from_slice(&1u16.to_le_bytes());
        v[table..table + 8].copy_from_slice(UKI_LINUX_SECTION);
        v[table + SECTION_RAW_SIZE_AT..table + SECTION_RAW_SIZE_AT + 4]
            .copy_from_slice(&(linux.len() as u32).to_le_bytes());
        v[table + SECTION_RAW_PTR_AT..table + SECTION_RAW_PTR_AT + 4]
            .copy_from_slice(&(payload_off as u32).to_le_bytes());
        v.extend_from_slice(linux);
        v
    }

    #[test]
    fn uki_unwraps_linux_section() {
        let img = fake_image(2048);
        // .linux as gzip Image.gz (the newer Ubuntu arm64 case)...
        let u = uki(&gzip(&img));
        assert_eq!(compression(&u), Some(Compression::Uki));
        assert_eq!(to_bootable(u).unwrap(), img);
        // ...as a raw Image...
        let u = uki(&img);
        assert_eq!(compression(&u), Some(Compression::Uki));
        assert_eq!(to_bootable(u).unwrap(), img);
        // ...and as a nested zboot (UKI → zboot → gzip → Image).
        let u = uki(&zboot("gzip", &gzip(&img)));
        assert_eq!(to_bootable(u).unwrap(), img);
    }

    #[test]
    fn raw_image_needs_nothing() {
        let img = fake_image(1024);
        assert_eq!(compression(&img), None);
        assert_eq!(to_bootable(img.clone()).unwrap(), img);
    }

    #[test]
    fn x86_and_unknown_left_alone() {
        // bzImage-ish: "MZ\0\0" but no zimg, no ARMd.
        let mut bz = vec![0u8; 1024];
        bz[0..4].copy_from_slice(&[0x4d, 0x5a, 0x00, 0x00]);
        assert_eq!(compression(&bz), None);
        assert_eq!(to_bootable(bz.clone()).unwrap(), bz);
    }

    #[test]
    fn gzip_image_round_trips() {
        let img = fake_image(2048);
        let gz = gzip(&img);
        assert_eq!(compression(&gz), Some(Compression::Gzip));
        assert_eq!(to_bootable(gz).unwrap(), img);
    }

    #[test]
    fn gzip_non_kernel_is_not_a_kernel() {
        // A gzip that doesn't wrap an Image (e.g. an initramfs) is ignored.
        let gz = gzip(b"not a kernel, just some cpio bytes \x07\x07\x07");
        assert_eq!(compression(&gz), None);
        let clone = gz.clone();
        assert_eq!(to_bootable(gz).unwrap(), clone); // verbatim
    }

    #[test]
    fn zboot_gzip_and_zstd() {
        let img = fake_image(4096);
        let gz = zboot("gzip", &gzip(&img));
        assert_eq!(compression(&gz), Some(Compression::Zboot("gzip".into())));
        assert_eq!(to_bootable(gz).unwrap(), img);

        let zpayload = ruzstd::encoding::compress_to_vec(
            &img[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let zb = zboot("zstd", &zpayload);
        assert_eq!(compression(&zb), Some(Compression::Zboot("zstd".into())));
        assert_eq!(to_bootable(zb).unwrap(), img);
    }

    #[test]
    fn zboot_unsupported_codec_errors() {
        let zb = zboot("lzo", &[1, 2, 3, 4]);
        // Reported honestly by compression()...
        assert_eq!(compression(&zb), Some(Compression::Zboot("lzo".into())));
        // ...but unwrapping fails clearly rather than silently passing through.
        assert!(to_bootable(zb).is_err());
    }
}
