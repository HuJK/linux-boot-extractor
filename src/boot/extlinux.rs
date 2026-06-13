//! extlinux/syslinux config parsing (Alpine and many cloud images).
//!
//! Line-oriented, case-insensitive keywords: `default`, `label`,
//! `kernel`/`linux`, `initrd` (comma-separated), `append`. Alpine puts the
//! initrd in `append initrd=/boot/...`, which we lift into the initrd list.
//! Relative paths are resolved against the config file's directory.

use super::{BootEntry, Source};
use crate::fsys::FileSystem;
use crate::Result;

pub struct ExtlinuxScan {
    pub entries: Vec<BootEntry>,
    pub default: Option<usize>,
}

pub fn parse(fs: &dyn FileSystem, cfg_path: &str) -> Result<ExtlinuxScan> {
    let data = fs.read_file(cfg_path)?;
    let base = cfg_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    Ok(parse_str(&String::from_utf8_lossy(&data), base))
}

pub(crate) fn parse_str(text: &str, base_dir: &str) -> ExtlinuxScan {
    let mut entries: Vec<BootEntry> = Vec::new();
    let mut default_label: Option<String> = None;
    let mut current: Option<BootEntry> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((kw, rest)) = split_keyword(line) else { continue };

        match kw.to_ascii_lowercase().as_str() {
            "default" => default_label = Some(rest.to_string()),
            "label" => {
                if let Some(done) = current.take() {
                    entries.push(done);
                }
                current = Some(BootEntry {
                    title: Some(rest.to_string()),
                    id: Some(rest.to_string()),
                    source: Source::Extlinux,
                    ..Default::default()
                });
            }
            "kernel" | "linux" => {
                if let Some(cur) = current.as_mut() {
                    cur.kernel = Some(resolve(base_dir, rest));
                }
            }
            "initrd" => {
                if let Some(cur) = current.as_mut() {
                    cur.initrd.extend(rest.split(',').map(|p| resolve(base_dir, p.trim())));
                }
            }
            "append" => {
                if let Some(cur) = current.as_mut() {
                    let mut cmdline_words = Vec::new();
                    for word in rest.split_whitespace() {
                        // The kernel itself ignores initrd=; it's a directive
                        // to the bootloader. Lift it out for direct boot.
                        if let Some(paths) = word.strip_prefix("initrd=") {
                            cur.initrd
                                .extend(paths.split(',').map(|p| resolve(base_dir, p)));
                        } else {
                            cmdline_words.push(word);
                        }
                    }
                    if !cmdline_words.is_empty() {
                        cur.cmdline = Some(cmdline_words.join(" "));
                    }
                }
            }
            _ => {} // timeout, prompt, menu *, say, serial, ...
        }
    }
    if let Some(done) = current.take() {
        entries.push(done);
    }

    let default = default_label
        .as_deref()
        .and_then(|d| entries.iter().position(|e| e.id.as_deref() == Some(d)))
        .or(if entries.is_empty() { None } else { Some(0) });

    ExtlinuxScan { entries, default }
}

fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let (kw, rest) = line.split_once(char::is_whitespace)?;
    Some((kw, rest.trim()))
}

fn resolve(base_dir: &str, p: &str) -> String {
    if p.starts_with('/') { p.to_string() } else { format!("{base_dir}/{p}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpine_style_with_initrd_in_append() {
        let cfg = r#"
DEFAULT virt
PROMPT 0
LABEL virt
  MENU LABEL Linux virt
  KERNEL /boot/vmlinuz-virt
  APPEND initrd=/boot/initramfs-virt root=UUID=abc modules=virtio console=hvc0
LABEL lts
  KERNEL /boot/vmlinuz-lts
  INITRD /boot/initramfs-lts
  APPEND root=UUID=abc
"#;
        let scan = parse_str(cfg, "/extlinux");
        assert_eq!(scan.entries.len(), 2);
        assert_eq!(scan.default, Some(0));
        let virt = &scan.entries[0];
        assert_eq!(virt.kernel.as_deref(), Some("/boot/vmlinuz-virt"));
        assert_eq!(virt.initrd, vec!["/boot/initramfs-virt"]);
        assert_eq!(
            virt.cmdline.as_deref(),
            Some("root=UUID=abc modules=virtio console=hvc0")
        );
    }

    #[test]
    fn relative_paths_resolve_against_config_dir() {
        let cfg = "label l\n kernel vmlinuz\n initrd a.img,b.img\n";
        let scan = parse_str(cfg, "/extlinux");
        assert_eq!(scan.entries[0].kernel.as_deref(), Some("/extlinux/vmlinuz"));
        assert_eq!(scan.entries[0].initrd, vec!["/extlinux/a.img", "/extlinux/b.img"]);
    }
}
