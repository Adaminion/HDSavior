//! File references and the rule that decides whether a reference still
//! points at the record found in its slot.

use core::cmp::Ordering;
use core::fmt;

/// The sequence number a record gets when it is freed while its sequence
/// number is `sequence`: `sequence + 1`, and `0xFFFF` wraps to 1.
///
/// NTFS bumps the sequence number when a record is *freed* (the file is
/// deleted), not when the slot is used again. This is `next(s)` of
/// ALGORITHMS.md.
#[inline]
#[must_use]
pub const fn next_seq(sequence: u16) -> u16 {
    if sequence == 0xFFFF {
        1
    } else {
        sequence.wrapping_add(1)
    }
}

/// The reference rule of ALGORITHMS.md.
///
/// A reference carrying `reference_sequence` resolves to the record found in
/// the referenced slot when that record is in use and has the same sequence
/// number, or is not in use and has the next one (it was the referenced
/// object and has been deleted since). A record that is in use with the next
/// sequence number is a different, newer object.
///
/// The caller has already matched the record number; this function decides
/// on sequence numbers and the in-use state alone.
#[inline]
#[must_use]
pub const fn reference_resolves(
    reference_sequence: u16,
    target_sequence: u16,
    target_in_use: bool,
) -> bool {
    if target_in_use {
        target_sequence == reference_sequence
    } else {
        target_sequence == next_seq(reference_sequence)
    }
}

/// An 8-byte NTFS file reference: a 48-bit MFT record number in the low bits
/// and a 16-bit sequence number in the high bits.
///
/// Ordering is by record number, then by sequence number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FileRef(u64);

impl FileRef {
    /// Mask of the record number inside the raw value.
    pub const NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

    /// The all-zero reference. As a base reference it means "this is a base
    /// record".
    pub const ZERO: FileRef = FileRef(0);

    /// Wraps the 64-bit value as stored on disk.
    #[inline]
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        FileRef(raw)
    }

    /// Builds a reference from its parts. Only the low 48 bits of `number`
    /// are used.
    #[inline]
    #[must_use]
    pub const fn new(number: u64, sequence: u16) -> Self {
        // `sequence as u64` widens; `u64::from` is not callable in const fn.
        FileRef((number & Self::NUMBER_MASK) | ((sequence as u64) << 48))
    }

    /// The 64-bit value as stored on disk.
    #[inline]
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The MFT record number (48 bits).
    #[inline]
    #[must_use]
    pub const fn number(self) -> u64 {
        self.0 & Self::NUMBER_MASK
    }

    /// The sequence number (16 bits).
    #[inline]
    #[must_use]
    pub const fn sequence(self) -> u16 {
        // The shift leaves 16 significant bits, so the cast cuts nothing.
        (self.0 >> 48) as u16
    }

    /// Whether all 64 bits are zero.
    #[inline]
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Whether this reference resolves to a record in its slot that has
    /// sequence number `target_sequence` and the given in-use state. See
    /// [`reference_resolves`].
    #[inline]
    #[must_use]
    pub const fn resolves_to(self, target_sequence: u16, target_in_use: bool) -> bool {
        reference_resolves(self.sequence(), target_sequence, target_in_use)
    }
}

impl From<u64> for FileRef {
    #[inline]
    fn from(raw: u64) -> Self {
        FileRef(raw)
    }
}

impl From<FileRef> for u64 {
    #[inline]
    fn from(reference: FileRef) -> Self {
        reference.0
    }
}

impl Ord for FileRef {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        // Rotating puts the record number above the sequence number.
        self.0.rotate_left(16).cmp(&other.0.rotate_left(16))
    }
}

impl PartialOrd for FileRef {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for FileRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileRef({}, {})", self.number(), self.sequence())
    }
}

impl fmt::Display for FileRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.number(), self.sequence())
    }
}
