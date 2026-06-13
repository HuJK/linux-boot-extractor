//! grub.cfg parsing.
//!
//! Not a GRUB shell interpreter — we extract the literal structure that
//! grub-mkconfig emits: `menuentry`/`submenu` blocks, `linux*`/`initrd*`
//! lines, `set default=`, and the `blscfg` marker (Fedora/RHEL: entries
//! live in BLS files, grub.cfg is a shim). The only variables expanded
//! are the grubenv ones (`saved_entry`, `kernelopts`, `tuned_params`, …);
//! unknown variables expand to empty.

use super::{expand, BootEntry, Source};
use crate::fsys::FileSystem;
use crate::Result;
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
                            entry: BootEntry { title, id, source: Source::Grub, ..Default::default() },
                            top,
                            sub,
                        },
                    ));
                }
                "submenu" if opens_block => {
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

    let default = resolve_default(&raw, default_spec.as_deref().unwrap_or("0"));
    GrubScan {
        entries: raw.into_iter().map(|r| r.entry).collect(),
        default,
        uses_bls,
    }
}

/// Resolve a GRUB `default` spec: numeric index, `N>M` submenu path,
/// entry id (`--id` / $menuentry_id_option), or entry title.
fn resolve_default(entries: &[RawEntry], spec: &str) -> Option<usize> {
    if entries.is_empty() {
        return None;
    }
    let spec = if spec.is_empty() { "0" } else { spec };

    let parts: Vec<Option<usize>> =
        spec.split('>').map(|p| p.trim().parse::<usize>().ok()).collect();
    if parts.iter().all(Option::is_some) {
        let top = parts[0].unwrap();
        let sub = parts.get(1).and_then(|p| *p);
        // Exact match first; a bare submenu index falls through to its
        // first child (GRUB boots that when default points at a submenu).
        return entries
            .iter()
            .position(|e| e.top == top && e.sub == sub)
            .or_else(|| entries.iter().position(|e| e.top == top))
            .or(Some(0));
    }

    entries
        .iter()
        .position(|e| e.entry.id.as_deref() == Some(spec))
        .or_else(|| entries.iter().position(|e| e.entry.title.as_deref() == Some(spec)))
        .or(Some(0))
}

/// Strip a `(hd0,gpt2)` / `($root)` device prefix and force `/`-absolute.
fn clean_path(p: &str) -> String {
    let p = if let Some(rest) = p.strip_prefix('(') {
        rest.split_once(')').map(|(_, path)| path).unwrap_or(rest)
    } else {
        p
    };
    if p.starts_with('/') { p.to_string() } else { format!("/{p}") }
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
        assert_eq!(scan.entries[0].initrd, vec!["/boot/initrd.img-5.15.0-105-generic"]);
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
        assert_eq!(scan.entries[0].cmdline.as_deref(), Some("root=UUID=x ro quiet"));
    }
}
