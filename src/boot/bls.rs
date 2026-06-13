//! Boot Loader Specification entries (`/loader/entries/*.conf`),
//! used by Fedora/RHEL GRUB (`blscfg`) and by systemd-boot.
//!
//! Line format is `key value`: title / version / linux / initrd / options.
//! `options` may reference grubenv variables (`$kernelopts`,
//! `$tuned_params`) on Fedora/RHEL — expanded here.

use super::vercmp::vercmp;
use super::{expand, BootEntry, Source};
use crate::fsys::{FileKind, FileSystem};
use crate::Result;
use std::collections::BTreeMap;

pub struct BlsScan {
    /// Sorted newest-first (BLS-style version sort).
    pub entries: Vec<BootEntry>,
    pub default: Option<usize>,
}

/// `sd_default` is the `default` glob from systemd-boot's loader.conf,
/// if one exists; grubenv `saved_entry` (matched against the entry
/// filename) takes precedence, per GRUB's blscfg behavior.
pub fn parse_dir(
    fs: &dyn FileSystem,
    entries_dir: &str,
    env: &BTreeMap<String, String>,
    sd_default: Option<&str>,
    configs: &mut Vec<String>,
) -> Result<BlsScan> {
    let mut items: Vec<(String, BootEntry)> = Vec::new(); // (filename stem, entry)

    let listing = fs.read_dir(entries_dir)?;
    for f in listing {
        if f.kind == FileKind::Dir || !f.name.ends_with(".conf") {
            continue;
        }
        let path = format!("{entries_dir}/{}", f.name);
        let Ok(data) = fs.read_file(&path) else { continue };
        configs.push(path);

        let stem = f.name.trim_end_matches(".conf").to_string();
        let mut entry = BootEntry {
            id: Some(stem.clone()),
            source: Source::Bls,
            ..Default::default()
        };
        let mut options: Vec<String> = Vec::new();

        for line in String::from_utf8_lossy(&data).lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once(char::is_whitespace) else { continue };
            let value = value.trim();
            match key {
                "title" => entry.title = Some(value.to_string()),
                "version" => entry.version = Some(value.to_string()),
                "linux" => entry.kernel = Some(absolute(value)),
                "initrd" => entry.initrd.extend(value.split_whitespace().map(absolute)),
                "options" => options.push(expand(value, env)),
                _ => {} // machine-id, sort-key, architecture, ...
            }
        }
        let cmdline = options.join(" ").trim().to_string();
        if !cmdline.is_empty() {
            entry.cmdline = Some(cmdline);
        }
        items.push((stem, entry));
    }

    // Newest first, comparing version (falling back to the filename, which
    // also embeds the version on Fedora/RHEL).
    items.sort_by(|(astem, a), (bstem, b)| {
        let av = a.version.as_deref().unwrap_or(astem);
        let bv = b.version.as_deref().unwrap_or(bstem);
        vercmp(bv, av)
    });

    let default = items
        .iter()
        .position(|(stem, _)| {
            env.get("saved_entry")
                .is_some_and(|s| s == stem || s == strip_boot_counter(stem))
        })
        .or_else(|| {
            sd_default.and_then(|pat| {
                items.iter().position(|(stem, _)| {
                    glob_match(pat, stem)
                        || glob_match(pat, &format!("{stem}.conf"))
                        || glob_match(pat, strip_boot_counter(stem))
                })
            })
        })
        .or(if items.is_empty() { None } else { Some(0) });

    Ok(BlsScan { entries: items.into_iter().map(|(_, e)| e).collect(), default })
}

/// systemd-boot boot counting appends `+TRIES` or `+TRIES-DONE` to the stem.
fn strip_boot_counter(stem: &str) -> &str {
    stem.split('+').next().unwrap_or(stem)
}

fn absolute(p: &str) -> String {
    if p.starts_with('/') { p.to_string() } else { format!("/{p}") }
}

/// Minimal `*`/`?` glob, enough for loader.conf `default` patterns.
fn glob_match(pattern: &str, s: &str) -> bool {
    fn rec(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') => rec(&p[1..], s) || (!s.is_empty() && rec(p, &s[1..])),
            Some(b'?') => !s.is_empty() && rec(&p[1..], &s[1..]),
            Some(&c) => s.first() == Some(&c) && rec(&p[1..], &s[1..]),
        }
    }
    rec(pattern.as_bytes(), s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn glob_basics() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("6.6.*-200*", "6.6.30-200.fc39"));
        assert!(glob_match("entry-?.conf", "entry-1.conf"));
        assert!(!glob_match("6.7.*", "6.6.30"));
    }
}
