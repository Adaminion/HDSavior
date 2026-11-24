//! `$UpCase` and file-name collation.
//!
//! A directory index is sorted by the upper-cased UTF-16 units of the names,
//! using the 65 536-entry table stored in the volume's own `$UpCase` file
//! (`layout.h` `COLLATION_FILE_NAME`). A driver looks names up with the
//! table it reads from the volume, so the table written by this crate and
//! the order of its index entries only have to agree with each other.
//!
//! The table here covers ASCII, Latin-1, Latin Extended-A, Greek, Cyrillic
//! and full-width Latin letters by fixed rules. It is **not** the table
//! Windows writes (which has 973 mappings); a Windows `chkdsk` would replace
//! it. Nothing HDSavior does depends on `$UpCase`.

use std::cmp::Ordering;

/// Upper-case form of one UTF-16 unit under this crate's `$UpCase` table.
pub const fn upcase(u: u16) -> u16 {
    match u {
        0x0061..=0x007A => u - 0x20,
        0x00E0..=0x00F6 | 0x00F8..=0x00FE => u - 0x20,
        0x00FF => 0x0178,
        // Latin Extended-A: pairs of (upper, lower) with a few odd spots
        0x0100..=0x012F | 0x0132..=0x0137 | 0x014A..=0x0177 => u & !1,
        0x0139..=0x0148 | 0x0179..=0x017E => {
            if u & 1 == 0 {
                u - 1
            } else {
                u
            }
        }
        // Greek
        0x03AC => 0x0386,
        0x03AD..=0x03AF => u - 0x25,
        0x03B1..=0x03C1 | 0x03C3..=0x03CB => u - 0x20,
        0x03CC => 0x038C,
        0x03CD | 0x03CE => u - 0x3F,
        // Cyrillic
        0x0430..=0x044F => u - 0x20,
        0x0450..=0x045F => u - 0x50,
        // full-width Latin
        0xFF41..=0xFF5A => u - 0x20,
        _ => u,
    }
}

/// The content of `$UpCase`: 65 536 little-endian UTF-16 units (128 KiB).
pub fn upcase_table() -> Vec<u8> {
    let mut out = Vec::with_capacity(131_072);
    for u in 0..=0xFFFFu16 {
        out.extend_from_slice(&upcase(u).to_le_bytes());
    }
    out
}

/// `COLLATION_FILE_NAME`: by upper-cased units; names that are equal
/// ignoring case (possible in the POSIX namespace) are ordered by their raw
/// units so that the order is total.
pub fn collate(a: &[u16], b: &[u16]) -> Ordering {
    let upper = a
        .iter()
        .map(|&u| upcase(u))
        .cmp(b.iter().map(|&u| upcase(u)));
    upper.then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_shape_and_the_mappings_shared_with_real_tables() {
        let table = upcase_table();
        assert_eq!(table.len(), 131_072);
        let at = |u: u16| u16::from_le_bytes([table[2 * u as usize], table[2 * u as usize + 1]]);
        // ASCII and Latin-1 mappings are the same in the mkntfs / Windows tables
        assert_eq!(at(b'a' as u16), b'A' as u16);
        assert_eq!(at(b'z' as u16), b'Z' as u16);
        assert_eq!(at(b'A' as u16), b'A' as u16);
        assert_eq!(at(b'$' as u16), b'$' as u16);
        assert_eq!(at(0xE0), 0xC0);
        assert_eq!(at(0xF7), 0xF7, "the division sign has no upper case");
        assert_eq!(at(0xFE), 0xDE);
        assert_eq!(at(0xFF), 0x178);
        assert_eq!(at(0x101), 0x100);
        assert_eq!(at(0x13A), 0x139);
        assert_eq!(at(0x17E), 0x17D);
        assert_eq!(at(0x3B1), 0x391);
        assert_eq!(at(0x3C2), 0x3C2, "final sigma is left alone");
        assert_eq!(at(0x3CE), 0x38F);
        assert_eq!(at(0x430), 0x410);
        assert_eq!(at(0x451), 0x401);
        assert_eq!(at(0xD800), 0xD800, "surrogates map to themselves");
        // upper-casing is idempotent, as a collation table has to be
        for u in 0..=0xFFFFu16 {
            assert_eq!(upcase(upcase(u)), upcase(u), "unit {u:#x}");
        }
    }

    #[test]
    fn collation_ignores_case_first() {
        let n = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        // '$' (0x24) sorts before '.', digits before letters, 'a' with 'A'
        assert_eq!(collate(&n("$MFT"), &n(".")), Ordering::Less);
        assert_eq!(collate(&n("b"), &n("A")), Ordering::Greater);
        assert_eq!(
            collate(&n("a_file"), &n("another_file")),
            Ordering::Greater,
            "'_' (0x5F) > 'N' (0x4E)"
        );
        assert_eq!(collate(&n("abc"), &n("ABD")), Ordering::Less);
        assert_eq!(collate(&n("abc"), &n("abcd")), Ordering::Less);
        // equal ignoring case: still a total order, upper case first
        assert_eq!(collate(&n("README.TXT"), &n("readme.txt")), Ordering::Less);
        assert_eq!(collate(&n("Readme.txt"), &n("Readme.txt")), Ordering::Equal);
    }
}
