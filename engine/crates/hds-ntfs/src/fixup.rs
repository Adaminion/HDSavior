//! Multi-sector transfer protection ("fixups") of `FILE` records and `INDX`
//! blocks (docs/notes/ntfs-on-disk.md, section 2).
//!
//! On disk, the last two bytes of every 512-byte block of such a structure
//! hold the update sequence number; the bytes that belong there are kept in
//! the update sequence array inside the first block. A structure whose
//! write was interrupted has blocks from two different versions, which
//! shows as a block whose last two bytes are not the number.

use crate::bytes::u16_at;
use crate::error::Error;

/// Length of the blocks that the update sequence array protects, and the
/// granularity at which structures are looked for. **Always 512 bytes**,
/// whatever the sector size of the volume and the size of the structure.
pub const SECTOR_SIZE: usize = 512;

/// Outcome of undoing the multi-sector protection of a record or block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FixupStatus {
    /// The last two bytes of every 512-byte block held the update sequence
    /// number and were replaced by the saved bytes.
    Ok,
    /// At least one block did not end with the update sequence number: the
    /// structure is torn (blocks of different versions) or partly
    /// overwritten. Blocks that matched were restored; the others were left
    /// as they were, and what was parsed from them deserves less trust.
    ///
    /// When *every* block mismatches (`first_bad_block == 0` and
    /// `bad_blocks` is the number of 512-byte blocks), the bytes were never
    /// written as a protected structure. Real volumes have such images:
    /// `$LogFile` pages hold copies of newly created MFT records, sector
    /// aligned, in their in-memory form, with the log page's own update
    /// sequence number where the block tails are. They describe a file as
    /// it was at one moment (often: just created, empty).
    Torn {
        /// Index (0-based) of the first 512-byte block that did not match.
        first_bad_block: u8,
        /// Number of blocks that did not match.
        bad_blocks: u8,
    },
}

impl FixupStatus {
    /// Whether every block matched.
    #[inline]
    #[must_use]
    pub const fn is_ok(self) -> bool {
        matches!(self, FixupStatus::Ok)
    }

    /// Whether at least one block did not match.
    #[inline]
    #[must_use]
    pub const fn is_torn(self) -> bool {
        matches!(self, FixupStatus::Torn { .. })
    }
}

/// Undoes the multi-sector protection of the `FILE` record or `INDX` block
/// (or any structure with a multi-sector header) that fills `block`,
/// in place.
///
/// `block` must be exactly the structure: its length a multiple of 512 from
/// 512 to 65536 bytes. The update sequence array is located through the
/// header fields at 0x04 (offset) and 0x06 (entry count). The stride is
/// 512 bytes regardless of sector and structure size.
///
/// Values inside the structure are valid only after this step: resident
/// data and names can lie across the last two bytes of a 512-byte block.
///
/// Applying the function twice is harmless for the data (a restored block
/// no longer ends with the number and is left alone, unless its real bytes
/// happen to equal the number, in which case the same bytes are written
/// again) but the second call reports [`FixupStatus::Torn`].
///
/// # Errors
///
/// * [`Error::TooShort`]: `block` is shorter than 512 bytes.
/// * [`Error::BadUsa`]: the length is not a multiple of 512 or exceeds
///   65536; the entry count is not `block.len() / 512 + 1`; or the array
///   does not lie between the 8-byte multi-sector header and the last two
///   bytes of the first block (`8 <= offset` and
///   `offset + 2 * count <= 510`).
///
/// Nothing is modified when an error is returned.
pub fn apply_fixups(block: &mut [u8]) -> Result<FixupStatus, Error> {
    if block.len() < SECTOR_SIZE {
        return Err(Error::TooShort);
    }
    let usa_offset = usize::from(u16_at(block, 0x04).ok_or(Error::TooShort)?);
    let usa_count = usize::from(u16_at(block, 0x06).ok_or(Error::TooShort)?);
    if !block.len().is_multiple_of(SECTOR_SIZE)
        || block.len() > 65_536
        || !usa_fits(usa_offset, usa_count, 8)
        || block.len() / SECTOR_SIZE != usa_count.saturating_sub(1)
    {
        return Err(Error::BadUsa);
    }
    Ok(apply_validated(block, usa_offset, usa_count))
}

/// Whether an update sequence array of `usa_count` entries at `usa_offset`
/// starts at or behind `min_offset` and ends before the last two bytes of
/// the first 512-byte block. (That the count fits the size of the
/// structure is the caller's check.)
#[inline]
pub(crate) fn usa_fits(usa_offset: usize, usa_count: usize, min_offset: usize) -> bool {
    usa_offset >= min_offset
        && usa_count
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(usa_offset))
            .is_some_and(|end| end <= SECTOR_SIZE.saturating_sub(2))
}

/// The fixup loop. The caller has validated offset and count against the
/// block; if it has not, the loop still cannot panic, it only reports the
/// blocks it could not reach as bad.
pub(crate) fn apply_validated(
    block: &mut [u8],
    usa_offset: usize,
    usa_count: usize,
) -> FixupStatus {
    let Some(usn) = crate::bytes::rd::<2>(block, usa_offset) else {
        return torn(0, usa_count.saturating_sub(1));
    };
    let mut first_bad = None;
    let mut bad = 0_usize;
    for index in 1..usa_count {
        let restored = restore_block(block, usa_offset, index, usn);
        if !restored {
            bad = bad.saturating_add(1);
            if first_bad.is_none() {
                first_bad = Some(index.saturating_sub(1));
            }
        }
    }
    match first_bad {
        None => FixupStatus::Ok,
        Some(first) => torn(first, bad),
    }
}

/// Restores the last two bytes of block `index - 1` from entry `index` of
/// the array when they hold the update sequence number.
#[inline]
fn restore_block(block: &mut [u8], usa_offset: usize, index: usize, usn: [u8; 2]) -> bool {
    let Some(saved) = index
        .checked_mul(2)
        .and_then(|rel| rel.checked_add(usa_offset))
        .and_then(|at| crate::bytes::rd::<2>(block, at))
    else {
        return false;
    };
    let Some(tail) = index
        .checked_mul(SECTOR_SIZE)
        .and_then(|end| end.checked_sub(2))
        .and_then(|at| block.get_mut(at..))
        .and_then(|rest| rest.first_chunk_mut::<2>())
    else {
        return false;
    };
    if *tail != usn {
        return false;
    }
    *tail = saved;
    true
}

fn torn(first_bad_block: usize, bad_blocks: usize) -> FixupStatus {
    FixupStatus::Torn {
        first_bad_block: u8::try_from(first_bad_block).unwrap_or(u8::MAX),
        bad_blocks: u8::try_from(bad_blocks).unwrap_or(u8::MAX),
    }
}
