//! FNV-1a, the hash that identifies a record's bytes as they were read.

/// FNV-1a 64-bit offset basis.
pub const FNV1A64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64-bit prime.
pub const FNV1A64_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 64-bit hash of `bytes`.
///
/// The scanner hashes every record exactly as it was read from the disk,
/// *before* [`apply_fixups`](crate::apply_fixups); recovery compares the
/// hash before trusting a stored location inside the record again.
///
/// The hash has one multiplication per byte on its critical path, so it
/// costs about a microsecond per KiB whatever the compiler does.
#[inline]
#[must_use]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV1A64_OFFSET_BASIS;
    for &byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(FNV1A64_PRIME);
    }
    hash
}
