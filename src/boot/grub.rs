//! grub.cfg parsing.
//!
//! Not a GRUB shell interpreter — we extract the literal structure that
//! grub-mkconfig emits: `menuentry`/`submenu` blocks, `linux*`/`initrd*`
//! lines, `set default=`, and the `blscfg` marker (Fedora/RHEL: entries
//! live in BLS files, grub.cfg is a shim). The only variables expanded
//! are the grubenv ones (`saved_entry`, `kernelopts`, `tuned_params`, …);
//! unknown variables expand to empty.

use super::{BootEntry, Source, expand};
use crate::Result;
use crate::fsys::FileSystem;
use std::collections::BTreeMap;

pub struct GrubScan {
    pub entries: Vec<BootEntry>,
    /// Index into `entries` of the default menu entry.
    pub default: Option<usize>,
    /// `blscfg` seen: the real entries are in /loader/entries.
    pub uses_bls: bool,
}

struct RawEntry {
    entry: BootEntry,
    /// Position among top-level menu items (submenu counts as one item).
    top: usize,
    /// Position inside its submenu, if any.
    sub: Option<usize>,
}

/// A `submenu` block, so a `default` path can name it by id or title.
struct RawSubmenu {
    top: usize,
    id: Option<String>,
    title: Option<String>,
}

pub fn parse(
    fs: &dyn FileSystem,
    cfg_path: &str,
    env: &BTreeMap<String, String>,
) -> Result<GrubScan> {
    let data = fs.read_file(cfg_path)?;
    Ok(parse_str(&String::from_utf8_lossy(&data), env))
}

pub(crate) fn parse_str(text: &str, env: &BTreeMap<String, String>) -> GrubScan {
    let mut raw: Vec<RawEntry> = Vec::new();
    let mut submenus: Vec<RawSubmenu> = Vec::new();
    let mut default_spec: Option<String> = None;
    let mut uses_bls = false;

    let mut depth: i32 = 0;
    let mut top_count: usize = 0;
    // (depth at which the submenu opened, its top index, next child index)
    let mut submenu: Option<(i32, usize, usize)> = None;
    // (depth at which the menuentry opened, entry under construction)
    let mut current: Option<(i32, RawEntry)> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let words = split_words(line);
        if words.is_empty() {
            continue;
        }
        let opens_block = line.ends_with('{');
        let kw = words[0].as_str();

        if let Some((_, cur)) = current.as_mut() {
            // Inside a menuentry body: only kernel/initrd lines matter.
            match kw {
                "linux" | "linux16" | "linuxefi" if words.len() >= 2 => {
                    cur.entry.kernel = Some(clean_path(&words[1]));
                    let cmdline = expand(&words[2..].join(" "), env);
                    let cmdline = cmdline.trim();
                    if !cmdline.is_empty() {
                        cur.entry.cmdline = Some(cmdline.to_string());
                    }
                }
                "initrd" | "initrd16" | "initrdefi" => {
                    cur.entry.initrd = words[1..].iter().map(|w| clean_path(w)).collect();
                }
                // os-prober writes a `chainloader` item per foreign OS; the
                // argument is the boot manager to hand over to (options
                // like `--force` come first). `chainloader +1` (a partition
                // boot sector) names no file and resolves to nothing.
                "chainloader" => {
                    cur.entry.chainloader = words[1..]
                        .iter()
                        .find(|w| !w.starts_with('-'))
                        // `+1` is a block list (a partition's boot sector),
                        // not a path: keep it verbatim so nothing takes it
                        // for a file.
                        .map(|w| if w.starts_with('+') { w.clone() } else { clean_path(w) });
                }
                _ => {}
            }
        } else {
            match kw {
                "menuentry" if opens_block => {
                    let title = words.get(1).cloned();
                    let id = words
                        .iter()
                        .position(|w| w == "$menuentry_id_option")
                        .and_then(|i| words.get(i + 1))
                        .cloned();
                    let (top, sub) = match submenu.as_mut() {
                        Some((_, t, next)) => {
                            let s = *next;
                            *next += 1;
                            (*t, Some(s))
                        }
                        None => {
                            let t = top_count;
                            top_count += 1;
                            (t, None)
                        }
                    };
                    current = Some((
                        depth,
                        RawEntry {
                            entry: BootEntry {
                                title,
                                id,
                                source: Source::Grub,
                                ..Default::default()
                            },
                            top,
                            sub,
                        },
                    ));
                }
                "submenu" if opens_block => {
                    submenus.push(RawSubmenu {
                        top: top_count,
                        id: words
                            .iter()
                            .position(|w| w == "$menuentry_id_option")
                            .and_then(|i| words.get(i + 1))
                            .cloned(),
                        title: words.get(1).cloned(),
                    });
                    submenu = Some((depth, top_count, 0));
                    top_count += 1;
                }
                "set" if words.len() >= 2 => {
                    if let Some(value) = words[1].strip_prefix("default=") {
                        default_spec = Some(expand(value, env));
                    }
                }
                _ => {}
            }
            if words.iter().any(|w| w == "blscfg") {
                uses_bls = true;
            }
        }

        // Brace bookkeeping. grub-mkconfig output opens blocks at line end
        // and closes them with a lone `}`.
        if opens_block {
            depth += 1;
        } else if line == "}" {
            depth -= 1;
            if let Some((open_depth, _)) = current.as_ref()
                && depth == *open_depth
            {
                raw.push(current.take().unwrap().1);
            } else if let Some((open_depth, _, _)) = submenu.as_ref()
                && depth == *open_depth
            {
                submenu = None;
            }
        }
    }
    if let Some((_, entry)) = current.take() {
        raw.push(entry); // unterminated block: salvage what we parsed
    }

    let default = resolve_default(&raw, &submenus, default_spec.as_deref().unwrap_or("0"));
    GrubScan {
        entries: raw.into_iter().map(|r| r.entry).collect(),
        default,
        uses_bls,
    }
}

/// Resolve a GRUB `default` spec: a numeric index, an entry id (`--id` /
/// $menuentry_id_option), an entry title, or a `>`-separated path into a
/// submenu whose components are any of those.
///
/// GRUB resolves a path one menu level at a time, so each component has to be
/// matched within its own level -- `parentId>childId` is what grub-mkconfig
/// writes for a GRUB_DEFAULT inside a submenu, and what grub-set-default /
/// grub-reboot store. Matching the whole spec against a single entry id (as
/// this used to) never hits, and the fallback then silently booted the first
/// entry instead of the configured one.
fn resolve_default(entries: &[RawEntry], submenus: &[RawSubmenu], spec: &str) -> Option<usize> {
    if entries.is_empty() {
        return None;
    }
    let spec = if spec.is_empty() { "0" } else { spec };
    let comps: Vec<&str> = spec.split('>').map(str::trim).collect();

    // Any entry, at any level, by id then title. Used for a single-component
    // spec, where GRUB accepts a plain id even for a nested entry.
    let flat = |name: &str| {
        entries
            .iter()
            .position(|e| e.entry.id.as_deref() == Some(name))
            .or_else(|| {
                entries
                    .iter()
                    .position(|e| e.entry.title.as_deref() == Some(name))
            })
    };
    // First entry of a top-level item: the item itself, or -- when it is a
    // submenu -- its first child, which is what GRUB boots.
    let first_of_top = |top: usize| {
        entries
            .iter()
            .position(|e| e.top == top && e.sub.is_none())
            .or_else(|| entries.iter().position(|e| e.top == top))
    };

    if comps.len() == 1 {
        let head = comps[0];
        return match head.parse::<usize>() {
            Ok(top) => first_of_top(top),
            Err(_) => flat(head).or_else(|| {
                submenus
                    .iter()
                    .find(|s| s.id.as_deref() == Some(head) || s.title.as_deref() == Some(head))
                    .and_then(|s| first_of_top(s.top))
            }),
        }
        .or(Some(0));
    }

    // Path: resolve the head to a top-level index, then the next component
    // inside it. Deeper nesting is not modelled (entries carry one sub level),
    // so anything beyond the second component falls back to the submenu.
    let head = comps[0];
    let top = match head.parse::<usize>() {
        Ok(top) => Some(top),
        Err(_) => entries
            .iter()
            .find(|e| {
                e.sub.is_none()
                    && (e.entry.id.as_deref() == Some(head)
                        || e.entry.title.as_deref() == Some(head))
            })
            .map(|e| e.top)
            .or_else(|| {
                submenus
                    .iter()
                    .find(|s| s.id.as_deref() == Some(head) || s.title.as_deref() == Some(head))
                    .map(|s| s.top)
            }),
    };
    let Some(top) = top else {
        // Unknown head: fall back to a flat match on the last component before
        // giving up, so a stale path still has a chance of naming the entry.
        return comps.last().and_then(|c| flat(c)).or(Some(0));
    };

    let child = comps[1];
    match child.parse::<usize>() {
        Ok(sub) => entries
            .iter()
            .position(|e| e.top == top && e.sub == Some(sub)),
        Err(_) => entries.iter().position(|e| {
            e.top == top
                && e.sub.is_some()
                && (e.entry.id.as_deref() == Some(child) || e.entry.title.as_deref() == Some(child))
        }),
    }
    .or_else(|| first_of_top(top))
    .or(Some(0))
}

/// Strip a `(hd0,gpt2)` / `($root)` device prefix and force `/`-absolute.
fn clean_path(p: &str) -> String {
    let p = if let Some(rest) = p.strip_prefix('(') {
        rest.split_once(')').map(|(_, path)| path).unwrap_or(rest)
    } else {
        p
    };
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

/// Shell-ish word split honoring single/double quotes (quotes stripped).
fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let (mut in_single, mut in_double) = (false, false);
    for c in line.chars() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    const UBUNTU_STYLE: &str = r#"
# generated by grub-mkconfig
function gfxmode {
    set gfxpayload="${1}"
}
set default="${saved_entry}"
if [ x"${feature_menuentry_id}" = xy ]; then
  menuentry_id_option="--id"
fi
menuentry 'Ubuntu' --class ubuntu $menuentry_id_option 'gnulinux-simple-uuid1' {
    search --no-floppy --fs-uuid --set=root uuid1
    linux   /boot/vmlinuz-5.15.0-105-generic root=UUID=uuid1 ro console=ttyS0
    initrd  /boot/initrd.img-5.15.0-105-generic
}
submenu 'Advanced options for Ubuntu' $menuentry_id_option 'gnulinux-advanced-uuid1' {
    menuentry 'Ubuntu, with Linux 5.15.0-105-generic' $menuentry_id_option 'gnulinux-5.15.0-105-generic-advanced-uuid1' {
        linux   /boot/vmlinuz-5.15.0-105-generic root=UUID=uuid1 ro
        initrd  /boot/initrd.img-5.15.0-105-generic
    }
    menuentry 'Ubuntu, with Linux 5.15.0-91-generic' $menuentry_id_option 'gnulinux-5.15.0-91-generic-advanced-uuid1' {
        linux   /boot/vmlinuz-5.15.0-91-generic root=UUID=uuid1 ro
        initrd  /boot/initrd.img-5.15.0-91-generic
    }
}
"#;

    /// os-prober's dual-boot item: no kernel, a `chainloader` target, and
    /// `set default` pointing at it by id.
    const OSPROBER: &str = r#"
set default="osprober-efi-1234-ABCD"
menuentry 'Debian GNU/Linux' $menuentry_id_option 'debian' {
    linux /vmlinuz-6.6.9 root=UUID=uuid1 ro
    initrd /initramfs-6.6.9.img
}
menuentry 'Windows Boot Manager (on /dev/sda1)' --class windows $menuentry_id_option 'osprober-efi-1234-ABCD' {
    insmod part_gpt
    search --no-floppy --fs-uuid --set=root 1234-ABCD
    chainloader /EFI/Microsoft/Boot/bootmgfw.efi
}
menuentry 'Older Windows (on /dev/sda3)' $menuentry_id_option 'osprober-chain-sda3' {
    chainloader +1
}
"#;

    #[test]
    fn parses_chainloader_entries() {
        let scan = parse_str(OSPROBER, &BTreeMap::new());
        assert_eq!(scan.entries.len(), 3);
        // The Linux entry is untouched...
        assert_eq!(scan.entries[0].kernel.as_deref(), Some("/vmlinuz-6.6.9"));
        assert_eq!(scan.entries[0].chainloader, None);
        // ...and the foreign ones carry their target instead of a kernel.
        assert_eq!(scan.entries[1].kernel, None);
        assert_eq!(
            scan.entries[1].chainloader.as_deref(),
            Some("/EFI/Microsoft/Boot/bootmgfw.efi")
        );
        assert_eq!(scan.entries[2].chainloader.as_deref(), Some("+1"));
        // `set default` names it by id: GRUB would boot Windows, not Linux.
        assert_eq!(scan.default, Some(1));
    }

    #[test]
    fn parses_entries_and_submenu() {
        let scan = parse_str(UBUNTU_STYLE, &BTreeMap::new());
        assert_eq!(scan.entries.len(), 3);
        assert!(!scan.uses_bls);
        assert_eq!(scan.entries[0].title.as_deref(), Some("Ubuntu"));
        assert_eq!(
            scan.entries[0].kernel.as_deref(),
            Some("/boot/vmlinuz-5.15.0-105-generic")
        );
        assert_eq!(
            scan.entries[0].cmdline.as_deref(),
            Some("root=UUID=uuid1 ro console=ttyS0")
        );
        assert_eq!(
            scan.entries[0].initrd,
            vec!["/boot/initrd.img-5.15.0-105-generic"]
        );
        // saved_entry unset -> "" -> "0" -> first top-level entry
        assert_eq!(scan.default, Some(0));
    }

    #[test]
    fn saved_entry_picks_submenu_child_by_index_path() {
        let mut env = BTreeMap::new();
        env.insert("saved_entry".to_string(), "1>1".to_string());
        let scan = parse_str(UBUNTU_STYLE, &env);
        assert_eq!(scan.default, Some(2));
        assert_eq!(
            scan.entries[2].kernel.as_deref(),
            Some("/boot/vmlinuz-5.15.0-91-generic")
        );
    }

    #[test]
    fn saved_entry_picks_submenu_child_by_id_path() {
        // What grub-mkconfig writes when GRUB_DEFAULT names an entry inside a
        // submenu (and what grub-set-default/grub-reboot store): the components
        // are ids, not indices. GRUB resolves them one menu level at a time.
        let mut env = BTreeMap::new();
        env.insert(
            "saved_entry".to_string(),
            "gnulinux-advanced-uuid1>gnulinux-5.15.0-91-generic-advanced-uuid1".to_string(),
        );
        let scan = parse_str(UBUNTU_STYLE, &env);
        assert_eq!(scan.default, Some(2));
        assert_eq!(
            scan.entries[2].kernel.as_deref(),
            Some("/boot/vmlinuz-5.15.0-91-generic")
        );
    }

    #[test]
    fn saved_entry_id_path_by_title() {
        let mut env = BTreeMap::new();
        env.insert(
            "saved_entry".to_string(),
            "Advanced options for Ubuntu>Ubuntu, with Linux 5.15.0-105-generic".to_string(),
        );
        let scan = parse_str(UBUNTU_STYLE, &env);
        assert_eq!(scan.default, Some(1));
    }

    #[test]
    fn saved_entry_submenu_alone_falls_through_to_first_child() {
        let mut env = BTreeMap::new();
        env.insert(
            "saved_entry".to_string(),
            "gnulinux-advanced-uuid1".to_string(),
        );
        let scan = parse_str(UBUNTU_STYLE, &env);
        assert_eq!(scan.default, Some(1));
    }

    #[test]
    fn saved_entry_mixed_id_and_index_path() {
        let mut env = BTreeMap::new();
        env.insert(
            "saved_entry".to_string(),
            "gnulinux-advanced-uuid1>1".to_string(),
        );
        let scan = parse_str(UBUNTU_STYLE, &env);
        assert_eq!(scan.default, Some(2));
    }

    #[test]
    fn saved_entry_matches_id() {
        let mut env = BTreeMap::new();
        env.insert(
            "saved_entry".to_string(),
            "gnulinux-5.15.0-91-generic-advanced-uuid1".to_string(),
        );
        let scan = parse_str(UBUNTU_STYLE, &env);
        assert_eq!(scan.default, Some(2));
    }

    #[test]
    fn detects_blscfg_and_expands_kernelopts() {
        let cfg = r#"
insmod blscfg
blscfg
"#;
        let scan = parse_str(cfg, &BTreeMap::new());
        assert!(scan.uses_bls);
        assert!(scan.entries.is_empty());

        let mut env = BTreeMap::new();
        env.insert("kernelopts".to_string(), "root=UUID=x ro".to_string());
        let cfg = "menuentry 'F' {\n  linux ($root)/vmlinuz-6.6 $kernelopts quiet\n}\n";
        let scan = parse_str(cfg, &env);
        assert_eq!(scan.entries[0].kernel.as_deref(), Some("/vmlinuz-6.6"));
        assert_eq!(
            scan.entries[0].cmdline.as_deref(),
            Some("root=UUID=x ro quiet")
        );
    }
}
