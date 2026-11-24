//! Bounds-checked little-endian field readers and lossless integer
//! conversions. Everything here returns `None` instead of panicking.

/// `N` bytes of `b` starting at `off`, or `None` when they are not all there.
#[inline(always)]
pub(crate) fn rd<const N: usize>(b: &[u8], off: usize) -> Option<[u8; N]> {
    b.get(off..)?.first_chunk::<N>().copied()
}

/// The byte at `off`.
#[inline(always)]
pub(crate) fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

/// The little-endian `u16` at `off`.
#[inline(always)]
pub(crate) fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    rd(b, off).map(u16::from_le_bytes)
}

/// The little-endian `u32` at `off`.
#[inline(always)]
pub(crate) fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    rd(b, off).map(u32::from_le_bytes)
}

/// The little-endian `u64` at `off`.
#[inline(always)]
pub(crate) fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    rd(b, off).map(u64::from_le_bytes)
}

/// The little-endian `i64` at `off`.
#[inline(always)]
pub(crate) fn i64_at(b: &[u8], off: usize) -> Option<i64> {
    rd(b, off).map(i64::from_le_bytes)
}

/// `v` as a `usize`. Lossless on every target with at least 32-bit
/// pointers; elsewhere the value saturates, which makes every bounds check
/// that follows fail safely.
#[inline(always)]
pub(crate) fn usize_from(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

/// `v` as a `u32`, saturating. Callers pass offsets and lengths inside a
/// record or index block (at most 64 KiB), so nothing is ever cut.
#[inline(always)]
pub(crate) fn u32_from(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// A little-endian two's-complement integer of 1 to 8 bytes, sign-extended.
/// An empty field reads as 0; bytes beyond the eighth are ignored (callers
/// never pass more than eight).
#[inline]
pub(crate) fn signed_le(field: &[u8]) -> i64 {
    let negative = field.last().is_some_and(|b| b & 0x80 != 0);
    let mut buf = if negative { [0xFF_u8; 8] } else { [0_u8; 8] };
    for (dst, src) in buf.iter_mut().zip(field) {
        *dst = *src;
    }
    i64::from_le_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_check_bounds() {
        let b = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        assert_eq!(u8_at(&b, 8), Some(0x09));
        assert_eq!(u8_at(&b, 9), None);
        assert_eq!(u16_at(&b, 0), Some(0x0201));
        assert_eq!(u16_at(&b, 7), Some(0x0908));
        assert_eq!(u16_at(&b, 8), None);
        assert_eq!(u32_at(&b, 5), Some(0x0908_0706));
        assert_eq!(u32_at(&b, 6), None);
        assert_eq!(u64_at(&b, 1), Some(0x0908_0706_0504_0302));
        assert_eq!(u64_at(&b, 2), None);
        assert_eq!(i64_at(&[0xFF; 8], 0), Some(-1));
        assert_eq!(u16_at(&b, usize::MAX), None);
        assert_eq!(rd::<0>(&b, 9), Some([]));
        assert_eq!(rd::<0>(&b, 10), None);
    }

    #[test]
    fn signed_fields_are_sign_extended() {
        assert_eq!(signed_le(&[]), 0);
        assert_eq!(signed_le(&[0x7F]), 127);
        assert_eq!(signed_le(&[0x80]), -128);
        assert_eq!(signed_le(&[0x80, 0x00]), 128);
        assert_eq!(signed_le(&[0xDF, 0x3C, 0xFF]), -49_953);
        assert_eq!(signed_le(&[0x47, 0x81, 0x00]), 33_095);
        assert_eq!(signed_le(&[0xFF; 8]), -1);
        assert_eq!(signed_le(&[0, 0, 0, 0, 0, 0, 0, 0x80]), i64::MIN);
        assert_eq!(
            signed_le(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F]),
            i64::MAX
        );
    }

    #[test]
    fn conversions_do_not_wrap() {
        assert_eq!(usize_from(65_536), 65_536);
        assert_eq!(u32_from(65_536), 65_536);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(u32_from(usize::MAX), u32::MAX);
    }
}
