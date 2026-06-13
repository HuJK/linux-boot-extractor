//! Locate kernel / initramfs / boot configuration inside a filesystem.
//!
//! Strategy, in order of fidelity:
//! 1. GRUB (`grub.cfg` + `grubenv` for the saved default entry); a
//!    `blscfg` shim redirects to BLS.
//! 2. BLS `/loader/entries/*.conf` (Fedora/RHEL, systemd-boot), default
//!    from grubenv `saved_entry` or loader.conf, else newest by version.
//! 3. extlinux/syslinux configs (Alpine, cloud images).
//! 4. Fallback: pair vmlinuz-*/initramfs-* by version string — no cmdline.
//!
//! Note on paths: a /boot that is its own partition has kernels at
//! /vmlinuz-*, a combined root partition at /boot/vmlinuz-*. Configs and
//! the files they reference live on the same filesystem either way, so
//! all paths stay filesystem-absolute.

pub mod bls;
pub mod extlinux;
pub mod grub;
pub mod grubenv;
pub mod vercmp;

use crate::fsys::{FileKind, FileSystem};
use crate::Result;
use std::collections::BTreeMap;
use vercmp::vercmp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    Grub,
    Bls,
    Extlinux,
    #[default]
    Fallback,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Source::Grub => "grub",
            Source::Bls => "bls",
            Source::Extlinux => "extlinux",
            Source::Fallback => "fallback",
        })
    }
}

/// One bootable configuration, resolved as far as we can.
/// Paths are filesystem-absolute (see module note about /boot prefixes).
#[derive(Debug, Clone, Default)]
pub struct BootEntry {
    pub title: Option<String>,
    /// Stable identifier: GRUB --id, BLS filename stem, extlinux label.
    pub id: Option<String>,
    pub kernel: Option<String>,
    /// Multiple initrds are real (microcode early-cpio + initramfs);
    /// concatenate in order for direct boot.
    pub initrd: Vec<String>,
    pub cmdline: Option<String>,
    pub version: Option<String>,
    pub source: Source,
}

/// Everything we found on one filesystem.
#[derive(Debug, Default)]
pub struct BootScan {
    pub entries: Vec<BootEntry>,
    /// Index into `entries` of what the bootloader would boot.
    pub default: Option<usize>,
    /// Config files found (paths on this filesystem), for `extract`.
    pub configs: Vec<String>,
}

impl BootScan {
    pub fn default_entry(&self) -> Option<&BootEntry> {
        self.default.or(if self.entries.is_empty() { None } else { Some(0) })
            .and_then(|i| self.entries.get(i))
    }
}

/// Directories where kernels and configs live, relative to fs root.
const BOOT_PREFIXES: &[&str] = &["", "/boot"];

pub fn scan(fs: &dyn FileSystem) -> Result<BootScan> {
    let env = grubenv::load(fs);
    let mut configs = Vec::new();

    // GRUB
    let mut grub_scan: Option<grub::GrubScan> = None;
    let mut uses_bls = false;
    for prefix in BOOT_PREFIXES {
        for cfg in ["/grub/grub.cfg", "/grub2/grub.cfg"] {
            let path = format!("{prefix}{cfg}");
            if !fs.exists(&path) {
                continue;
            }
            configs.push(path.clone());
            if let Ok(g) = grub::parse(fs, &path, &env) {
                uses_bls |= g.uses_bls;
                if !g.entries.is_empty() && grub_scan.is_none() {
                    grub_scan = Some(g);
                }
            }
        }
    }

    // BLS
    let sd_default = loader_conf_default(fs, &mut configs);
    let mut bls_scan: Option<bls::BlsScan> = None;
    for prefix in BOOT_PREFIXES {
        let dir = format!("{prefix}/loader/entries");
        if fs.exists(&dir)
            && let Ok(b) = bls::parse_dir(fs, &dir, &env, sd_default.as_deref(), &mut configs)
            && !b.entries.is_empty()
        {
            bls_scan = Some(b);
            break;
        }
    }

    // extlinux / syslinux
    let mut ext_scan: Option<extlinux::ExtlinuxScan> = None;
    for prefix in BOOT_PREFIXES {
        for cfg in ["/extlinux/extlinux.conf", "/extlinux.conf", "/syslinux/syslinux.cfg", "/syslinux/extlinux.conf"] {
            let path = format!("{prefix}{cfg}");
            if !fs.exists(&path) {
                continue;
            }
            configs.push(path.clone());
            if ext_scan.is_none()
                && let Ok(e) = extlinux::parse(fs, &path)
                && !e.entries.is_empty()
            {
                ext_scan = Some(e);
            }
        }
    }

    // Pick the authoritative source. blscfg in grub.cfg means the grub
    // entries (if any) are leftovers and BLS is the truth.
    let (entries, default) = if let Some(b) = bls_scan.take_if(|_| uses_bls) {
        (b.entries, b.default)
    } else if let Some(g) = grub_scan {
        (g.entries, g.default)
    } else if let Some(b) = bls_scan {
        (b.entries, b.default)
    } else if let Some(e) = ext_scan {
        (e.entries, e.default)
    } else {
        fallback_scan(fs)?
    };

    let (entries, default) = drop_stale_entries(fs, entries, default);
    Ok(BootScan { entries, default, configs })
}

/// systemd-boot loader.conf `default` glob, if present.
fn loader_conf_default(fs: &dyn FileSystem, configs: &mut Vec<String>) -> Option<String> {
    for prefix in BOOT_PREFIXES {
        let path = format!("{prefix}/loader/loader.conf");
        let Ok(data) = fs.read_file(&path) else { continue };
        configs.push(path);
        for line in String::from_utf8_lossy(&data).lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("default") {
                let v = rest.trim();
                if !v.is_empty() {
                    return Some(v.trim_end_matches(".conf").to_string());
                }
            }
        }
    }
    None
}

/// Configs can reference kernels that were since removed. Drop entries
/// whose kernel file is missing — unless that would drop everything, in
/// which case keep the originals (better a stale answer than none).
fn drop_stale_entries(
    fs: &dyn FileSystem,
    entries: Vec<BootEntry>,
    default: Option<usize>,
) -> (Vec<BootEntry>, Option<usize>) {
    let keep: Vec<bool> = entries
        .iter()
        .map(|e| e.kernel.as_deref().is_some_and(|k| fs.exists(k)))
        .collect();
    if !keep.iter().any(|&k| k) {
        return (entries, default);
    }
    let new_default = default.map(|d| keep[..d].iter().filter(|&&k| k).count());
    let kept: Vec<BootEntry> = entries
        .into_iter()
        .zip(&keep)
        .filter_map(|(e, &k)| k.then_some(e))
        .collect();
    let new_default =
        new_default.filter(|_| default.is_some_and(|d| keep[d])).or(Some(0));
    (kept, new_default)
}

/// Expand `$var` / `${var}` from the grubenv map; unknown vars -> empty.
pub(crate) fn expand(s: &str, env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        if chars.peek() == Some(&'{') {
            chars.next();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                name.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    name.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
        }
        if name.is_empty() {
            out.push('$');
        } else if let Some(value) = env.get(&name) {
            out.push_str(value);
        }
    }
    out
}

/// No usable config: pair vmlinuz-*/initramfs-* by version, newest first.
/// The distro-maintained /vmlinuz symlink, when readable, marks the default.
fn fallback_scan(fs: &dyn FileSystem) -> Result<(Vec<BootEntry>, Option<usize>)> {
    let mut entries = Vec::new();
    for prefix in BOOT_PREFIXES {
        let dir = if prefix.is_empty() { "/" } else { prefix };
        let Ok(listing) = fs.read_dir(dir) else { continue };

        let mut kernels: Vec<(String, String)> = Vec::new(); // (version, path)
        let mut initrds: Vec<(String, String)> = Vec::new();
        for e in &listing {
            if e.kind == FileKind::Dir {
                continue;
            }
            let path = format!("{prefix}/{}", e.name);
            if let Some(v) = e.name.strip_prefix("vmlinuz-").or_else(|| e.name.strip_prefix("Image-")) {
                // Rescue/kdump images are not boot candidates.
                if !v.contains("rescue") && !v.contains("kdump") {
                    kernels.push((v.to_string(), path));
                }
            } else if let Some(v) = e
                .name
                .strip_prefix("initramfs-")
                .map(|v| v.trim_end_matches(".img"))
                .or_else(|| e.name.strip_prefix("initrd.img-"))
                .or_else(|| e.name.strip_prefix("initrd-").map(|v| v.trim_end_matches(".img")))
            {
                initrds.push((v.to_string(), path));
            }
        }

        for (version, kernel) in kernels {
            let initrd = initrds
                .iter()
                .find(|(v, _)| *v == version)
                .map(|(_, p)| p.clone())
                .into_iter()
                .collect();
            entries.push(BootEntry {
                kernel: Some(kernel),
                initrd,
                version: Some(version),
                source: Source::Fallback,
                ..Default::default()
            });
        }
    }

    entries.sort_by(|a, b| {
        let av = a.version.as_deref().unwrap_or("");
        let bv = b.version.as_deref().unwrap_or("");
        vercmp(bv, av)
    });

    // Debian/Ubuntu maintain /vmlinuz -> boot/vmlinuz-X; trust it over
    // version sort when present.
    let mut default = if entries.is_empty() { None } else { Some(0) };
    for link in ["/vmlinuz", "/boot/vmlinuz"] {
        if let Some(target) = fs.read_link(link) {
            let base = target.rsplit('/').next().unwrap_or(&target);
            if let Some(i) = entries.iter().position(|e| {
                e.kernel.as_deref().is_some_and(|k| k.rsplit('/').next() == Some(base))
            }) {
                default = Some(i);
                break;
            }
        }
    }

    Ok((entries, default))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsys::DirEntry;
    use std::collections::BTreeMap;

    /// In-memory filesystem for parser tests.
    pub(crate) struct MockFs {
        files: BTreeMap<String, Vec<u8>>,
        links: BTreeMap<String, String>,
    }

    impl MockFs {
        pub fn new<const N: usize>(files: [(&str, &str); N]) -> MockFs {
            MockFs {
                files: files
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
                    .collect(),
                links: BTreeMap::new(),
            }
        }

        pub fn link(mut self, from: &str, to: &str) -> MockFs {
            self.links.insert(from.to_string(), to.to_string());
            self
        }
    }

    impl FileSystem for MockFs {
        fn fs_type(&self) -> &'static str {
            "mock"
        }
        fn label(&self) -> Option<String> {
            None
        }
        fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
            let prefix = format!("{}/", path.trim_end_matches('/'));
            let mut names = std::collections::BTreeSet::new();
            let mut out = Vec::new();
            for (key, data) in &self.files {
                let Some(rest) = key.strip_prefix(&prefix) else { continue };
                let first = rest.split('/').next().unwrap();
                if !names.insert(first.to_string()) {
                    continue;
                }
                out.push(DirEntry {
                    name: first.to_string(),
                    kind: if rest.contains('/') { FileKind::Dir } else { FileKind::File },
                    size: data.len() as u64,
                });
            }
            Ok(out)
        }
        fn read_file(&self, path: &str) -> Result<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| crate::Error::NotFound(path.to_string()))
        }
        fn exists(&self, path: &str) -> bool {
            let prefix = format!("{}/", path.trim_end_matches('/'));
            path == "/"
                || self.files.contains_key(path)
                || self.files.keys().any(|k| k.starts_with(&prefix))
        }
        fn read_link(&self, path: &str) -> Option<String> {
            self.links.get(path).cloned()
        }
        fn file_size(&self, path: &str) -> Result<u64> {
            self.read_file(path).map(|d| d.len() as u64)
        }
    }

    const GRUBENV: &str = "# GRUB Environment Block\nsaved_entry=fedora-6.6.9\nkernelopts=root=UUID=abc ro\n";

    #[test]
    fn bls_with_grubenv_default_and_kernelopts() {
        let fs = MockFs::new([
            ("/grub2/grub.cfg", "insmod blscfg\nblscfg\n"),
            ("/grub2/grubenv", GRUBENV),
            (
                "/loader/entries/fedora-6.6.30.conf",
                "title Fedora 6.6.30\nversion 6.6.30-200.fc39\nlinux /vmlinuz-6.6.30\ninitrd /initramfs-6.6.30.img\noptions $kernelopts quiet\n",
            ),
            (
                "/loader/entries/fedora-6.6.9.conf",
                "title Fedora 6.6.9\nversion 6.6.9-100.fc39\nlinux /vmlinuz-6.6.9\ninitrd /initramfs-6.6.9.img\noptions $kernelopts\n",
            ),
            ("/vmlinuz-6.6.30", "k"),
            ("/vmlinuz-6.6.9", "k"),
            ("/initramfs-6.6.30.img", "i"),
            ("/initramfs-6.6.9.img", "i"),
        ]);
        let scan = scan(&fs).unwrap();
        assert_eq!(scan.entries.len(), 2);
        // Sorted newest first...
        assert_eq!(scan.entries[0].kernel.as_deref(), Some("/vmlinuz-6.6.30"));
        // ...but grubenv saved_entry pins the older one as default.
        assert_eq!(scan.default, Some(1));
        let def = scan.default_entry().unwrap();
        assert_eq!(def.cmdline.as_deref(), Some("root=UUID=abc ro"));
        assert_eq!(def.source, Source::Bls);
    }

    #[test]
    fn stale_grub_entries_are_dropped() {
        let fs = MockFs::new([
            (
                "/boot/grub/grub.cfg",
                "set default=\"1\"\nmenuentry 'gone' {\n linux /boot/vmlinuz-old\n}\nmenuentry 'here' {\n linux /boot/vmlinuz-new\n initrd /boot/initrd.img-new\n}\n",
            ),
            ("/boot/vmlinuz-new", "k"),
            ("/boot/initrd.img-new", "i"),
        ]);
        let scan = scan(&fs).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].kernel.as_deref(), Some("/boot/vmlinuz-new"));
        assert_eq!(scan.default, Some(0));
    }

    #[test]
    fn fallback_sorts_by_version_and_honors_symlink() {
        let fs = MockFs::new([
            ("/boot/vmlinuz-6.6.9-100.fc39", "k"),
            ("/boot/vmlinuz-6.6.30-200.fc39", "k"),
            ("/boot/vmlinuz-0-rescue-deadbeef", "k"),
            ("/boot/initramfs-6.6.9-100.fc39.img", "i"),
            ("/boot/initramfs-6.6.30-200.fc39.img", "i"),
        ])
        .link("/boot/vmlinuz", "vmlinuz-6.6.9-100.fc39");
        let scan = scan(&fs).unwrap();
        // rescue excluded, newest first
        assert_eq!(scan.entries.len(), 2);
        assert_eq!(
            scan.entries[0].kernel.as_deref(),
            Some("/boot/vmlinuz-6.6.30-200.fc39")
        );
        // but the symlink pins the default to 6.6.9
        assert_eq!(scan.default, Some(1));
        assert_eq!(scan.entries[1].initrd, vec!["/boot/initramfs-6.6.9-100.fc39.img"]);
    }

    #[test]
    fn extlinux_default_label() {
        let fs = MockFs::new([
            (
                "/boot/extlinux/extlinux.conf",
                "default lts\nlabel virt\n kernel /boot/vmlinuz-virt\nlabel lts\n kernel /boot/vmlinuz-lts\n append root=/dev/vda1\n",
            ),
            ("/boot/vmlinuz-virt", "k"),
            ("/boot/vmlinuz-lts", "k"),
        ]);
        let scan = scan(&fs).unwrap();
        assert_eq!(scan.default, Some(1));
        assert_eq!(scan.default_entry().unwrap().cmdline.as_deref(), Some("root=/dev/vda1"));
    }
}
