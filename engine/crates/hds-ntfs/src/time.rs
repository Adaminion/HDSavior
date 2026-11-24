//! FILETIME conversion with checked arithmetic.
//!
//! NTFS stores times as FILETIME: unsigned 64-bit counts of 100-nanosecond
//! ticks since 1601-01-01 00:00:00 UTC. The fields are whatever was on the
//! disk; nothing here panics or wraps on any value.

/// FILETIME of the Unix epoch, 1970-01-01 00:00:00 UTC.
pub const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

/// FILETIME ticks (100 ns) per millisecond.
pub const FILETIME_TICKS_PER_MS: u64 = 10_000;

/// FILETIME ticks (100 ns) per second.
pub const FILETIME_TICKS_PER_SECOND: u64 = 10_000_000;

/// Whether a FILETIME is a usable point in time: `0 < filetime < 2^63`
/// (ALGORITHMS.md H8).
///
/// Zero means "not set" and is what unused fields hold; values with the top
/// bit set are negative to Windows, which rejects them as file times.
#[inline]
#[must_use]
pub const fn filetime_is_valid(filetime: u64) -> bool {
    filetime != 0 && filetime < (1_u64 << 63)
}

/// Ticks since the Unix epoch (negative before 1970) of a valid FILETIME.
#[inline]
const fn ticks_since_unix_epoch(filetime: u64) -> Option<i64> {
    if !filetime_is_valid(filetime) {
        return None;
    }
    // Both operands are below 2^63: the conversions keep the values and the
    // difference cannot overflow. Checked all the same.
    filetime
        .cast_signed()
        .checked_sub(FILETIME_UNIX_EPOCH.cast_signed())
}

/// Converts a FILETIME to Unix time in milliseconds, rounding towards
/// negative infinity (times before 1970 are negative).
///
/// Returns `None` for a FILETIME that is not valid per
/// [`filetime_is_valid`]. Every valid value is representable.
#[inline]
#[must_use]
pub const fn filetime_to_unix_ms(filetime: u64) -> Option<i64> {
    match ticks_since_unix_epoch(filetime) {
        Some(ticks) => Some(ticks.div_euclid(FILETIME_TICKS_PER_MS.cast_signed())),
        None => None,
    }
}

/// Converts a FILETIME to Unix time as whole seconds (rounded towards
/// negative infinity) plus nanoseconds in `0..1_000_000_000`, so that no
/// precision is lost.
///
/// Returns `None` for a FILETIME that is not valid per
/// [`filetime_is_valid`].
#[inline]
#[must_use]
pub fn filetime_to_unix_time(filetime: u64) -> Option<(i64, u32)> {
    let ticks = ticks_since_unix_epoch(filetime)?;
    let per_second = FILETIME_TICKS_PER_SECOND.cast_signed();
    let seconds = ticks.div_euclid(per_second);
    // rem_euclid is in 0..10_000_000, so the product stays below 10^9.
    let sub_second_ticks = u32::try_from(ticks.rem_euclid(per_second)).ok()?;
    Some((seconds, sub_second_ticks.checked_mul(100)?))
}

/// Converts Unix time in milliseconds to a FILETIME.
///
/// Returns `None` when the result would not be valid per
/// [`filetime_is_valid`]: at or before 1601-01-01, or beyond the range.
#[inline]
#[must_use]
pub const fn unix_ms_to_filetime(unix_ms: i64) -> Option<u64> {
    let ticks = match unix_ms.checked_mul(FILETIME_TICKS_PER_MS.cast_signed()) {
        Some(ticks) => ticks,
        None => return None,
    };
    match ticks.checked_add(FILETIME_UNIX_EPOCH.cast_signed()) {
        // A positive i64 is below 2^63, which is exactly the valid range.
        Some(filetime) if filetime > 0 => Some(filetime.cast_unsigned()),
        _ => None,
    }
}
