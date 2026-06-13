//! GRUB environment block (`grubenv`): a fixed 1024-byte file of
//! `key=value` lines padded with `#`. Holds `saved_entry` (the default
//! menu entry) and on Fedora/RHEL `kernelopts`/`tuned_params` used by
//! BLS entries.

use crate::fsys::FileSystem;
use std::collections::BTreeMap;

const CANDIDATES: &[&str] = &[
    "/grub2/grubenv",
    "/grub/grubenv",
    "/boot/grub2/grubenv",
    "/boot/grub/grubenv",
];

/// Load the first grubenv found. Missing or malformed -> empty map;
/// the env is an enhancement, never a hard requirement.
pub fn load(fs: &dyn FileSystem) -> BTreeMap<String, String> {
    for path in CANDIDATES {
        if let Ok(data) = fs.read_file(path) {
            return parse(&String::from_utf8_lossy(&data));
        }
    }
    BTreeMap::new()
}

pub(crate) fn parse(text: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue; // header comment and '#' padding
        }
        if let Some((key, value)) = line.split_once('=') {
            env.insert(key.trim().to_string(), value.trim_end_matches('#').trim().to_string());
        }
    }
    env
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_block_with_padding() {
        let mut text = String::from("# GRUB Environment Block\nsaved_entry=1>2\nkernelopts=root=UUID=abc ro\n");
        while text.len() < 1024 {
            text.push('#');
        }
        let env = super::parse(&text);
        assert_eq!(env.get("saved_entry").unwrap(), "1>2");
        assert_eq!(env.get("kernelopts").unwrap(), "root=UUID=abc ro");
    }
}
