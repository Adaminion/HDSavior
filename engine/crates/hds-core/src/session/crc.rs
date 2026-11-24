//! CRC-32 (IEEE 802.3, the polynomial of zlib and GPT), slicing by 8.
//!
//! Every block of a session file carries the CRC of its payload. Payloads
//! of millions of records are checked when a session is loaded, so the
//! byte-at-a-time loop (about 400 MB/s) is too slow; eight table lookups per
//! eight bytes run at several GB/s.

// The tables have 256 entries and are indexed by a byte, and by a constant
// below 8: no index can be out of bounds.
#![allow(clippy::indexing_slicing)]

const POLY: u32 = 0xEDB8_8320;

// Compile-time table construction: the loops are bounded by the table
// sizes, so neither the counters nor the narrowing of `i` (below 256) can
// misbehave.
#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
const fn make_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0_u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
            bit += 1;
        }
        tables[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut k = 1;
        while k < 8 {
            let prev = tables[k - 1][i];
            tables[k][i] = (prev >> 8) ^ tables[0][(prev & 0xFF) as usize];
            k += 1;
        }
        i += 1;
    }
    tables
}

static TABLES: [[u32; 256]; 8] = make_tables();

/// Feeds `data` into a running CRC state (start with `!0`, finish with `!`).
fn update(mut crc: u32, data: &[u8]) -> u32 {
    let t = &TABLES;
    let (words, rest) = data.as_chunks::<8>();
    for w in words {
        let [a, b, c, d] = (u32::from_le_bytes([w[0], w[1], w[2], w[3]]) ^ crc).to_le_bytes();
        crc = t[7][usize::from(a)]
            ^ t[6][usize::from(b)]
            ^ t[5][usize::from(c)]
            ^ t[4][usize::from(d)]
            ^ t[3][usize::from(w[4])]
            ^ t[2][usize::from(w[5])]
            ^ t[1][usize::from(w[6])]
            ^ t[0][usize::from(w[7])];
    }
    for &byte in rest {
        let [low, ..] = crc.to_le_bytes();
        crc = t[0][usize::from(low ^ byte)] ^ (crc >> 8);
    }
    crc
}

/// CRC-32 of `data`.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    !update(!0, data)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_possible_truncation)]

    use super::*;

    fn bytewise(data: &[u8]) -> u32 {
        let mut crc = !0_u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    POLY ^ (crc >> 1)
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn known_values() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
    }

    #[test]
    fn slicing_equals_the_bitwise_definition_at_every_length_and_alignment() {
        let data: Vec<u8> = (0..300_u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for start in 0..9 {
            for end in start..data.len() {
                assert_eq!(
                    crc32(&data[start..end]),
                    bytewise(&data[start..end]),
                    "{start}..{end}"
                );
            }
        }
    }
}
