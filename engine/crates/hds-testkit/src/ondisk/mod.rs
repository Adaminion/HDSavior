//! Encoders for NTFS on-disk structures.
//!
//! Written from `docs/notes/ntfs-on-disk.md` (the section numbers in the
//! comments refer to it) and `/usr/include/ntfs-3g/layout.h`, and from
//! nothing else: this module shares no code with `hds-ntfs`, which is what
//! makes an image built here a meaningful test of that parser. The tests of
//! this crate decode the output with independent readers
//! (`scripts/linux/inspect-ntfs.py`, `ntfsinfo`, `ntfsls`, an `ntfs-3g`
//! mount).
//!
//! The encoders are deliberately dumb: they write exactly the model they are
//! given, including inconsistent or hostile values, because broken structures
//! are what many scenarios exist for. Consistency is the business of
//! [`crate::volume`].

pub mod attr;
pub mod attrdef;
pub mod boot;
pub mod index;
pub mod record;
pub mod runlist;
pub mod upcase;

use serde::{Deserialize, Serialize};

/// A file reference: 48-bit MFT record number and 16-bit sequence number
/// (notes section 3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileRef {
    /// MFT record number (48 bits on disk).
    pub number: u64,
    /// Sequence number.
    pub seq: u16,
}

impl FileRef {
    /// The all-zero reference: "no base record".
    pub const NONE: FileRef = FileRef { number: 0, seq: 0 };

    /// A reference to record `number` with sequence `seq`.
    pub const fn new(number: u64, seq: u16) -> Self {
        FileRef { number, seq }
    }

    /// The 64-bit on-disk form.
    pub const fn raw(self) -> u64 {
        ((self.seq as u64) << 48) | (self.number & 0x0000_FFFF_FFFF_FFFF)
    }

    /// Splits a 64-bit on-disk reference.
    pub const fn from_raw(raw: u64) -> Self {
        FileRef { number: raw & 0x0000_FFFF_FFFF_FFFF, seq: (raw >> 48) as u16 }
    }
}

impl std::fmt::Display for FileRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({}, {})", self.number, self.seq)
    }
}

/// The sequence number a record gets when it is freed: `s + 1`, `0xFFFF`
/// wraps to 1, 0 stays 0 (notes section 3, "Sequence number").
pub const fn next_seq(seq: u16) -> u16 {
    match seq {
        0 => 0,
        0xFFFF => 1,
        s => s + 1,
    }
}

/// Stride of the update sequence array: always 512 bytes, whatever the
/// sector size (notes section 2).
pub const FIXUP_STRIDE: usize = 512;

/// Protects a multi-sector structure: stores `usn` at `usa_offset`, saves
/// the last two bytes of every 512-byte block behind it and replaces them
/// with `usn` (notes section 2).
pub fn apply_fixups(buf: &mut [u8], usa_offset: usize, usn: u16) {
    assert!(buf.len().is_multiple_of(FIXUP_STRIDE), "structure size must be a multiple of 512");
    let blocks = buf.len() / FIXUP_STRIDE;
    assert!(usa_offset + 2 * (blocks + 1) <= FIXUP_STRIDE - 2, "update sequence array does not fit");
    buf[usa_offset..usa_offset + 2].copy_from_slice(&usn.to_le_bytes());
    for block in 0..blocks {
        let tail = (block + 1) * FIXUP_STRIDE - 2;
        let slot = usa_offset + 2 * (block + 1);
        let saved = [buf[tail], buf[tail + 1]];
        buf[slot..slot + 2].copy_from_slice(&saved);
        buf[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
    }
}

/// Rounds up to a multiple of 8.
pub(crate) const fn align8(n: usize) -> usize {
    (n + 7) & !7
}

pub(crate) fn put_u16(buf: &mut [u8], at: usize, v: u16) {
    buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u64(buf: &mut [u8], at: usize, v: u64) {
    buf[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// UTF-16 code units of a string.
pub fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// UTF-16LE bytes of code units.
pub(crate) fn utf16_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_reference_packing() {
        let r = FileRef::new(0x0000_1234_5678_9ABC, 0xFEDC);
        assert_eq!(r.raw(), 0xFEDC_1234_5678_9ABC);
        assert_eq!(FileRef::from_raw(0xFEDC_1234_5678_9ABC), r);
        assert_eq!(FileRef::NONE.raw(), 0);
        // root directory reference as it appears in every top-level name
        assert_eq!(FileRef::new(5, 5).raw().to_le_bytes(), [5, 0, 0, 0, 0, 0, 5, 0]);
    }

    #[test]
    fn sequence_numbers_wrap_past_zero() {
        assert_eq!(next_seq(1), 2);
        assert_eq!(next_seq(0xFFFE), 0xFFFF);
        assert_eq!(next_seq(0xFFFF), 1);
        assert_eq!(next_seq(0), 0);
    }

    #[test]
    fn fixups_move_block_tails_into_the_array() {
        let mut buf = vec![0u8; 1024];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let original = buf.clone();
        apply_fixups(&mut buf, 0x30, 0x0102);
        assert_eq!(&buf[0x30..0x32], &[0x02, 0x01]);
        assert_eq!(&buf[0x32..0x34], &original[510..512]);
        assert_eq!(&buf[0x34..0x36], &original[1022..1024]);
        assert_eq!(&buf[510..512], &[0x02, 0x01]);
        assert_eq!(&buf[1022..1024], &[0x02, 0x01]);
        // nothing else changed
        for i in (0..1024).filter(|i| !(0x30..0x36).contains(i) && !(510..512).contains(i) && *i < 1022) {
            assert_eq!(buf[i], original[i], "byte {i}");
        }
    }

    #[test]
    fn fixup_stride_is_512_for_4096_byte_structures() {
        let mut buf = vec![0xEEu8; 4096];
        apply_fixups(&mut buf, 0x28, 7);
        for block in 0..8 {
            assert_eq!(&buf[block * 512 + 510..block * 512 + 512], &[7, 0]);
            assert_eq!(&buf[0x2A + 2 * block..0x2C + 2 * block], &[0xEE, 0xEE]);
        }
    }
}
