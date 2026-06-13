//! rpm-style version comparison for kernel version strings
//! ("6.6.30-200.fc39" > "6.6.9-100.fc39", which plain string sort gets wrong).

use std::cmp::Ordering;

/// Compare two version strings, rpmvercmp-style: split into alternating
/// numeric/alphabetic segments (separators ignored), numeric segments
/// compare numerically, and a numeric segment beats an alphabetic one.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    let sa = segments(a);
    let sb = segments(b);
    for (x, y) in sa.iter().zip(sb.iter()) {
        let ord = match (x, y) {
            (Seg::Num(x), Seg::Num(y)) => cmp_numeric(x, y),
            (Seg::Alpha(x), Seg::Alpha(y)) => x.cmp(y),
            (Seg::Num(_), Seg::Alpha(_)) => Ordering::Greater,
            (Seg::Alpha(_), Seg::Num(_)) => Ordering::Less,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    sa.len().cmp(&sb.len())
}

#[derive(Debug)]
enum Seg<'a> {
    Num(&'a str),
    Alpha(&'a str),
}

fn segments(s: &str) -> Vec<Seg<'_>> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            out.push(Seg::Num(&s[start..i]));
        } else if bytes[i].is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            out.push(Seg::Alpha(&s[start..i]));
        } else {
            i += 1; // separator
        }
    }
    out
}

/// Numeric compare without parsing (kernel versions won't overflow u64,
/// but build hashes in version strings might).
fn cmp_numeric(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_segments_compare_numerically() {
        assert_eq!(vercmp("6.6.9", "6.6.30"), Ordering::Less);
        assert_eq!(vercmp("6.6.30", "6.6.30"), Ordering::Equal);
        assert_eq!(vercmp("6.10.1", "6.9.9"), Ordering::Greater);
    }

    #[test]
    fn distro_release_suffixes() {
        assert_eq!(vercmp("6.6.30-200.fc39", "6.6.30-100.fc39"), Ordering::Greater);
        assert_eq!(vercmp("5.15.0-105-generic", "5.15.0-91-generic"), Ordering::Greater);
        assert_eq!(vercmp("5.14.0-362.el9", "5.14.0-70.el9"), Ordering::Greater);
    }

    #[test]
    fn rescue_sorts_below_real_kernels() {
        // "0-rescue-<machineid>" starts with 0, naturally lowest.
        assert_eq!(vercmp("0-rescue-abc123", "6.6.30-200.fc39"), Ordering::Less);
    }

    #[test]
    fn numeric_beats_alpha_and_length_breaks_ties() {
        assert_eq!(vercmp("6.6.1", "6.6.rc1"), Ordering::Greater);
        assert_eq!(vercmp("6.6.30.1", "6.6.30"), Ordering::Greater);
    }
}
