//! Small shared helpers: bounds-checked integer reads, byte searches, CRC-32.

/// `N` bytes of `b` at `off`, or `None` when the slice is too short.
#[inline]
pub(crate) fn rd<const N: usize>(b: &[u8], off: usize) -> Option<[u8; N]> {
    let end = off.checked_add(N)?;
    <[u8; N]>::try_from(b.get(off..end)?).ok()
}

#[inline]
pub(crate) fn le16(b: &[u8], off: usize) -> Option<u16> {
    rd(b, off).map(u16::from_le_bytes)
}

#[inline]
pub(crate) fn le32(b: &[u8], off: usize) -> Option<u32> {
    rd(b, off).map(u32::from_le_bytes)
}

#[inline]
pub(crate) fn le64(b: &[u8], off: usize) -> Option<u64> {
    rd(b, off).map(u64::from_le_bytes)
}

#[inline]
pub(crate) fn be16(b: &[u8], off: usize) -> Option<u16> {
    rd(b, off).map(u16::from_be_bytes)
}

#[inline]
pub(crate) fn be32(b: &[u8], off: usize) -> Option<u32> {
    rd(b, off).map(u32::from_be_bytes)
}

#[inline]
pub(crate) fn be64(b: &[u8], off: usize) -> Option<u64> {
    rd(b, off).map(u64::from_be_bytes)
}

/// Position of the first `needle` in `hay`.
///
/// Word-at-a-time, so that the JPEG entropy scan and the PDF/ZIP keyword
/// scans are not the bottleneck next to a disk.
#[inline]
pub(crate) fn find_byte(hay: &[u8], needle: u8) -> Option<usize> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let rep = LO.wrapping_mul(needle as u64);
    let mut skipped = 0usize;
    for chunk in hay.chunks_exact(8) {
        let mut a = [0u8; 8];
        a.copy_from_slice(chunk);
        let x = u64::from_ne_bytes(a) ^ rep;
        if x.wrapping_sub(LO) & !x & HI != 0 {
            break;
        }
        skipped += 8;
    }
    hay.get(skipped..)?
        .iter()
        .position(|&b| b == needle)
        .map(|p| p + skipped)
}

/// Position of the first occurrence of `needle` (non-empty) in `hay`.
pub(crate) fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let (&first, rest) = needle.split_first()?;
    let mut from = 0usize;
    while let Some(p) = find_byte(hay.get(from..)?, first) {
        let at = from + p;
        let tail = hay.get(at + 1..)?;
        if tail.len() < rest.len() {
            return None;
        }
        if tail.starts_with(rest) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// Printable ASCII (space to tilde).
#[inline]
pub(crate) fn is_printable(c: u8) -> bool {
    (0x20..=0x7E).contains(&c)
}

const fn crc_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut c = t[0][i];
        let mut s = 1;
        while s < 8 {
            c = t[0][(c & 0xFF) as usize] ^ (c >> 8);
            t[s][i] = c;
            s += 1;
        }
        i += 1;
    }
    t
}

static CRC_TABLES: [[u32; 256]; 8] = crc_tables();

/// CRC-32 (IEEE 802.3, the polynomial of PNG, ZIP, 7z and RAR), slicing-by-8.
#[derive(Clone, Copy)]
pub(crate) struct Crc32(u32);

impl Crc32 {
    pub(crate) fn new() -> Self {
        Crc32(!0)
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        let t = &CRC_TABLES;
        let mut crc = self.0;
        let mut chunks = data.chunks_exact(8);
        for c in &mut chunks {
            let mut a = [0u8; 8];
            a.copy_from_slice(c);
            let lo = u32::from_le_bytes([a[0], a[1], a[2], a[3]]) ^ crc;
            let hi = u32::from_le_bytes([a[4], a[5], a[6], a[7]]);
            crc = t[7][(lo & 0xFF) as usize]
                ^ t[6][((lo >> 8) & 0xFF) as usize]
                ^ t[5][((lo >> 16) & 0xFF) as usize]
                ^ t[4][(lo >> 24) as usize]
                ^ t[3][(hi & 0xFF) as usize]
                ^ t[2][((hi >> 8) & 0xFF) as usize]
                ^ t[1][((hi >> 16) & 0xFF) as usize]
                ^ t[0][(hi >> 24) as usize];
        }
        for &b in chunks.remainder() {
            crc = t[0][((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
        self.0 = crc;
    }

    pub(crate) fn finish(self) -> u32 {
        !self.0
    }
}

/// CRC-32 of one buffer.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
        // PNG's IEND chunk: CRC over the type alone
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn crc32_split_points_agree() {
        let data: Vec<u8> = (0..1000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        let whole = crc32(&data);
        for split in [0, 1, 3, 7, 8, 9, 15, 16, 17, 500, 999, 1000] {
            let mut c = Crc32::new();
            c.update(&data[..split]);
            c.update(&data[split..]);
            assert_eq!(c.finish(), whole, "split at {split}");
        }
        // bitwise reference
        let mut r = !0u32;
        for &b in &data {
            r ^= b as u32;
            for _ in 0..8 {
                r = if r & 1 != 0 {
                    0xEDB8_8320 ^ (r >> 1)
                } else {
                    r >> 1
                };
            }
        }
        assert_eq!(!r, whole);
    }

    #[test]
    fn find_byte_everywhere() {
        for len in 0..40usize {
            let mut v = vec![0x11u8; len];
            assert_eq!(find_byte(&v, 0xFF), None);
            for p in 0..len {
                v[p] = 0xFF;
                assert_eq!(find_byte(&v, 0xFF), Some(p), "len {len} pos {p}");
                // an earlier near-miss must not confuse the word trick
                if p > 0 {
                    v[p - 1] = 0xFE;
                    assert_eq!(find_byte(&v, 0xFF), Some(p));
                    v[p - 1] = 0x11;
                }
                v[p] = 0x11;
            }
        }
        assert_eq!(find_byte(&[0, 0, 0], 0), Some(0));
        assert_eq!(find_byte(&[1, 0x80, 0], 0), Some(2));
    }

    #[test]
    fn find_sub_basics() {
        assert_eq!(find_sub(b"hello %%EOF world", b"%%EOF"), Some(6));
        assert_eq!(find_sub(b"%%%EOF", b"%%EOF"), Some(1));
        assert_eq!(find_sub(b"%%EO", b"%%EOF"), None);
        assert_eq!(find_sub(b"", b"x"), None);
        assert_eq!(find_sub(b"abc", b""), None);
        assert_eq!(find_sub(b"aaab", b"ab"), Some(2));
    }

    #[test]
    fn integer_reads_are_bounds_checked() {
        let b = [1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(le16(&b, 0), Some(0x0201));
        assert_eq!(be16(&b, 6), Some(0x0708));
        assert_eq!(be16(&b, 7), None);
        assert_eq!(le32(&b, 4), Some(0x0807_0605));
        assert_eq!(be32(&b, 5), None);
        assert_eq!(le64(&b, 0), Some(0x0807_0605_0403_0201));
        assert_eq!(be64(&b, 1), None);
        assert_eq!(le16(&b, usize::MAX), None);
    }
}
