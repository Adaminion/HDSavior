//! Mapping pairs ("runlist") encoder (notes section 5).

use serde::{Deserialize, Serialize};

/// One run of clusters: `len` clusters at `lcn`, or a sparse hole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Run {
    /// First cluster, relative to the start of the volume; `None` = hole.
    pub lcn: Option<u64>,
    /// Number of clusters.
    pub len: u64,
}

impl Run {
    /// `len` clusters at `lcn`.
    pub const fn at(lcn: u64, len: u64) -> Self {
        Run {
            lcn: Some(lcn),
            len,
        }
    }

    /// A sparse hole of `len` clusters.
    pub const fn hole(len: u64) -> Self {
        Run { lcn: None, len }
    }
}

/// Total number of clusters (holes included) of a run list.
pub fn total_clusters(runs: &[Run]) -> u64 {
    runs.iter().map(|r| r.len).sum()
}

/// Number of bytes needed to store `v` as a signed little-endian integer.
fn signed_len(v: i64) -> usize {
    let mut n = 1;
    while n < 8 {
        let shift = 8 * n - 1;
        // fits when everything above the sign bit is a copy of it
        if (v >> shift) == 0 || (v >> shift) == -1 {
            break;
        }
        n += 1;
    }
    n
}

/// Encodes runs as mapping pairs, terminator included.
///
/// Per run: a header byte (low nibble = size of the length field, high
/// nibble = size of the offset field), the length, then the offset as a
/// signed delta to the LCN of the previous run that had one (0 before the
/// first). A hole has an offset size of 0. Lengths are stored as *signed*
/// values like Microsoft's encoder does, so 128 clusters take two bytes
/// (`80 00`). A real run at LCN 0 gets an explicit zero delta, as `$Boot`
/// has on every volume.
pub fn encode_runlist(runs: &[Run]) -> Vec<u8> {
    let mut out = Vec::with_capacity(runs.len() * 5 + 1);
    let mut prev_lcn: i64 = 0;
    for run in runs {
        assert!(
            run.len > 0 && run.len <= i64::MAX as u64,
            "run length {} cannot be encoded",
            run.len
        );
        let len_bytes = signed_len(run.len as i64);
        match run.lcn {
            None => {
                out.push(len_bytes as u8);
                out.extend_from_slice(&run.len.to_le_bytes()[..len_bytes]);
            }
            Some(lcn) => {
                assert!(lcn <= i64::MAX as u64, "LCN {lcn} cannot be encoded");
                let delta = lcn as i64 - prev_lcn;
                let off_bytes = signed_len(delta);
                out.push(((off_bytes as u8) << 4) | len_bytes as u8);
                out.extend_from_slice(&run.len.to_le_bytes()[..len_bytes]);
                out.extend_from_slice(&delta.to_le_bytes()[..off_bytes]);
                prev_lcn = lcn as i64;
            }
        }
    }
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_field_sizes() {
        assert_eq!(signed_len(0), 1);
        assert_eq!(signed_len(127), 1);
        assert_eq!(signed_len(128), 2);
        assert_eq!(signed_len(-128), 1);
        assert_eq!(signed_len(-129), 2);
        assert_eq!(signed_len(32767), 2);
        assert_eq!(signed_len(32768), 3);
        assert_eq!(signed_len(33_095), 3);
        assert_eq!(signed_len(-49_953), 3);
        assert_eq!(signed_len(i64::MAX), 8);
        assert_eq!(signed_len(i64::MIN), 8);
    }

    #[test]
    fn byte_sequences_quoted_in_the_notes() {
        // notes section 5, each verified there against real bytes:
        // `$Boot`: 2 clusters at LCN 0 (ntfs-3g) and 16 clusters (Windows)
        assert_eq!(encode_runlist(&[Run::at(0, 2)]), [0x11, 0x02, 0x00, 0x00]);
        assert_eq!(encode_runlist(&[Run::at(0, 16)]), [0x11, 0x10, 0x00, 0x00]);
        // sparse.bin: LCN 33 095, hole of 255, LCN 33 351
        assert_eq!(
            encode_runlist(&[Run::at(33_095, 1), Run::hole(255), Run::at(33_351, 1)]),
            [0x31, 0x01, 0x47, 0x81, 0x00, 0x02, 0xFF, 0x00, 0x21, 0x01, 0x00, 0x01, 0x00]
        );
        // 128 clusters need a second length byte: "22 80 00 68 40"
        assert_eq!(
            encode_runlist(&[Run::at(0x4068, 128)]),
            [0x22, 0x80, 0x00, 0x68, 0x40, 0x00]
        );
        // Windows: "32 80 00 20 BF 00" (LCN 0xBF20 needs a third, zero byte)
        assert_eq!(
            encode_runlist(&[Run::at(0xBF20, 128)]),
            [0x32, 0x80, 0x00, 0x20, 0xBF, 0x00, 0x00]
        );
        // a hole of 15 clusters: "01 0F"; `$BadClus:$Bad`: "03 7F 77 02"
        assert_eq!(encode_runlist(&[Run::hole(15)]), [0x01, 0x0F, 0x00]);
        assert_eq!(
            encode_runlist(&[Run::hole(0x02_777F)]),
            [0x03, 0x7F, 0x77, 0x02, 0x00]
        );
        // a negative delta: "31 09 DF 3C FF" = -49 953 from the previous run
        let first = 100_000;
        let bytes = encode_runlist(&[Run::at(first, 1), Run::at(first - 49_953, 9)]);
        assert_eq!(
            &bytes[bytes.len() - 6..],
            &[0x31, 0x09, 0xDF, 0x3C, 0xFF, 0x00]
        );
    }

    #[test]
    fn deltas_skip_holes_and_empty_list_is_just_the_terminator() {
        assert_eq!(encode_runlist(&[]), [0x00]);
        // the delta after a hole is relative to the last real run, not to 0
        let bytes = encode_runlist(&[Run::at(1000, 4), Run::hole(8), Run::at(1010, 4)]);
        assert_eq!(
            bytes,
            [0x21, 0x04, 0xE8, 0x03, 0x01, 0x08, 0x11, 0x04, 0x0A, 0x00]
        );
        assert_eq!(
            total_clusters(&[Run::at(1000, 4), Run::hole(8), Run::at(1010, 4)]),
            16
        );
    }

    #[test]
    fn large_values_round_trip_through_an_independent_decoder() {
        // A decoder written here from the same notes, only for this test.
        fn decode(bytes: &[u8]) -> Vec<Run> {
            let mut runs = Vec::new();
            let mut pos = 0;
            let mut lcn = 0i64;
            loop {
                let head = bytes[pos];
                pos += 1;
                if head == 0 {
                    return runs;
                }
                let (ln, on) = ((head & 15) as usize, (head >> 4) as usize);
                let mut len = [0u8; 8];
                len[..ln].copy_from_slice(&bytes[pos..pos + ln]);
                pos += ln;
                if on == 0 {
                    runs.push(Run::hole(u64::from_le_bytes(len)));
                    continue;
                }
                let negative = bytes[pos + on - 1] & 0x80 != 0;
                let mut off = [if negative { 0xFF } else { 0 }; 8];
                off[..on].copy_from_slice(&bytes[pos..pos + on]);
                pos += on;
                lcn += i64::from_le_bytes(off);
                runs.push(Run::at(lcn as u64, u64::from_le_bytes(len)));
            }
        }
        let runs = vec![
            Run::at(1 << 40, 1 << 33),
            Run::at(5, 127),
            Run::hole(1 << 20),
            Run::at((1 << 47) - 1, 128),
            Run::at(0, 1),
            Run::at(255, 255),
            Run::at(256, 32_768),
        ];
        assert_eq!(decode(&encode_runlist(&runs)), runs);
    }
}
