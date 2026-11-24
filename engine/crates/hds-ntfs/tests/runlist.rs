//! Runlist decoding: every rule of ARCHITECTURE.md 5.1 and the byte
//! sequences recorded from real volumes in docs/notes/ntfs-on-disk.md
//! section 5.

use hds_ntfs::{decode_runlist, DecodedRunlist, Run, RunlistError, RunlistStatus, Runs};

fn run(lcn: u64, length: u64) -> Run {
    Run {
        lcn: Some(lcn),
        length,
    }
}

fn hole(length: u64) -> Run {
    Run { lcn: None, length }
}

/// Decodes into a fresh vector.
fn decode(bytes: &[u8]) -> (Vec<Run>, RunlistStatus) {
    let mut runs = Vec::new();
    let result = decode_runlist(bytes, &mut runs);
    assert_eq!(result.runs, runs.len());
    assert_eq!(result.clusters, runs.iter().map(|r| r.length).sum::<u64>());
    // The iterator and the function are the same decoder.
    let mut iter = Runs::new(bytes);
    assert_eq!(iter.by_ref().collect::<Vec<Run>>(), runs);
    assert_eq!(iter.status(), Some(result.status));
    assert_eq!(iter.consumed(), result.consumed);
    (runs, result.status)
}

const COMPLETE: RunlistStatus = RunlistStatus::Complete;

fn invalid(error: RunlistError) -> RunlistStatus {
    RunlistStatus::Invalid(error)
}

#[test]
fn header_nibbles_give_the_field_sizes() {
    // 0x21: one length byte, two offset bytes.
    assert_eq!(
        decode(&[0x21, 0x10, 0x34, 0x12, 0x00]),
        (vec![run(0x1234, 0x10)], COMPLETE)
    );
    // 0x12: two length bytes, one offset byte.
    assert_eq!(
        decode(&[0x12, 0x34, 0x12, 0x10, 0x00]),
        (vec![run(0x10, 0x1234)], COMPLETE)
    );
    // 0x88: eight and eight.
    let mut wide = vec![0x88];
    wide.extend(0x0102_0304_0506_0708_u64.to_le_bytes());
    wide.extend(0x1112_1314_1516_1718_u64.to_le_bytes());
    wide.push(0);
    assert_eq!(
        decode(&wide),
        (
            vec![run(0x1112_1314_1516_1718, 0x0102_0304_0506_0708)],
            COMPLETE
        )
    );
}

#[test]
fn real_runlists_from_the_reference_sheet() {
    // 128 clusters: the length needs two bytes because it is signed.
    assert_eq!(
        decode(&[0x22, 0x80, 0x00, 0x68, 0x40, 0x00]),
        (vec![run(0x4068, 128)], COMPLETE)
    );
    // Windows 10 sample.
    assert_eq!(
        decode(&[0x22, 0x80, 0x00, 0x8C, 0x13, 0x00]),
        (vec![run(0x138C, 128)], COMPLETE)
    );
    assert_eq!(
        decode(&[0x32, 0x80, 0x00, 0x20, 0xBF, 0x00, 0x00]),
        (vec![run(0x00BF20, 128)], COMPLETE)
    );
    // Three offset bytes for +33 095: the zero byte keeps it positive.
    assert_eq!(
        decode(&[0x31, 0x01, 0x47, 0x81, 0x00, 0x00]),
        (vec![run(33_095, 1)], COMPLETE)
    );
    // A negative offset: 50 000 - 49 953 = 47. (50 000 itself needs three
    // bytes: as two bytes, 0xC350 would be negative.)
    assert_eq!(
        decode(&[0x31, 0x02, 0x50, 0xC3, 0x00, 0x31, 0x09, 0xDF, 0x3C, 0xFF, 0x00]),
        (vec![run(50_000, 2), run(47, 9)], COMPLETE)
    );
    assert_eq!(
        decode(&[0x21, 0x02, 0x50, 0xC3, 0x00]),
        (vec![], invalid(RunlistError::NegativeLcn))
    );
    // $Boot: an offset that is present and zero is a real run at LCN 0.
    assert_eq!(
        decode(&[0x11, 0x02, 0x00, 0x00]),
        (vec![run(0, 2)], COMPLETE)
    );
    assert_eq!(
        decode(&[0x11, 0x10, 0x00, 0x00]),
        (vec![run(0, 16)], COMPLETE)
    );
    // $BadClus:$Bad: one hole over the whole volume, no sparse flag needed.
    assert_eq!(
        decode(&[0x03, 0x7F, 0x77, 0x02, 0x00]),
        (vec![hole(0x02_777F)], COMPLETE)
    );
    assert_eq!(decode(&[0x01, 0x0F, 0x00]), (vec![hole(15)], COMPLETE));
}

#[test]
fn offsets_are_relative_to_the_last_non_sparse_run() {
    // sparse.bin of the reference sheet: LCN 33 095, a hole of 255, then
    // +256 from 33 095 - not from the hole, and not from 0.
    let bytes = [
        0x31, 0x01, 0x47, 0x81, 0x00, 0x02, 0xFF, 0x00, 0x21, 0x01, 0x00, 0x01, 0x00,
    ];
    assert_eq!(
        decode(&bytes),
        (vec![run(33_095, 1), hole(255), run(33_351, 1)], COMPLETE)
    );
    // Two holes in a row leave the base where it was.
    let bytes = [
        0x11, 0x05, 0x64, 0x01, 0x03, 0x01, 0x04, 0x11, 0x02, 0x0A, 0x00,
    ];
    assert_eq!(
        decode(&bytes),
        (vec![run(100, 5), hole(3), hole(4), run(110, 2)], COMPLETE)
    );
    // A hole first: the base is still 0.
    assert_eq!(
        decode(&[0x01, 0x30, 0x11, 0x04, 0x07, 0x00]),
        (vec![hole(0x30), run(7, 4)], COMPLETE)
    );
}

#[test]
fn a_present_offset_of_zero_is_a_real_run_at_the_same_lcn() {
    // Not a hole: the run starts where the previous run started.
    let bytes = [0x11, 0x05, 0x64, 0x11, 0x03, 0x00, 0x00];
    assert_eq!(decode(&bytes), (vec![run(100, 5), run(100, 3)], COMPLETE));
    // Also with a wider zero field.
    let bytes = [0x41, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00];
    assert_eq!(decode(&bytes), (vec![run(0, 1)], COMPLETE));
}

#[test]
fn the_list_ends_at_the_first_zero_header_and_padding_is_ignored() {
    // "... 00 FF 00 00 00 00": bytes behind the terminator are not zero.
    let bytes = [0x11, 0x01, 0x05, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00];
    let mut runs = Vec::new();
    assert_eq!(
        decode_runlist(&bytes, &mut runs),
        DecodedRunlist {
            status: COMPLETE,
            runs: 1,
            clusters: 1,
            consumed: 4
        }
    );
    assert_eq!(runs, [run(5, 1)]);
    // A run that would follow the terminator is not decoded.
    assert_eq!(decode(&[0x00, 0x11, 0x01, 0x05, 0x00]), (vec![], COMPLETE));
    // The empty list of an empty non-resident attribute.
    assert_eq!(decode(&[0x00]), (vec![], COMPLETE));
}

#[test]
fn lengths_are_signed_and_must_be_positive() {
    // One byte 0x80 is -128, not 128.
    assert_eq!(
        decode(&[0x11, 0x80, 0x05, 0x00]),
        (vec![], invalid(RunlistError::BadLength))
    );
    assert_eq!(
        decode(&[0x11, 0xFF, 0x05, 0x00]),
        (vec![], invalid(RunlistError::BadLength))
    );
    assert_eq!(
        decode(&[0x12, 0x00, 0x80, 0x05, 0x00]),
        (vec![], invalid(RunlistError::BadLength))
    );
    // 0x7F is the largest one-byte length.
    assert_eq!(
        decode(&[0x11, 0x7F, 0x05, 0x00]),
        (vec![run(5, 127)], COMPLETE)
    );
    // Zero clusters.
    assert_eq!(
        decode(&[0x11, 0x00, 0x05, 0x00]),
        (vec![], invalid(RunlistError::BadLength))
    );
    assert_eq!(
        decode(&[0x14, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00]),
        (vec![], invalid(RunlistError::BadLength))
    );
    // No length field at all.
    assert_eq!(
        decode(&[0x10, 0x05, 0x00]),
        (vec![], invalid(RunlistError::BadLength))
    );
    assert_eq!(
        decode(&[0x80, 1, 2, 3, 4, 5, 6, 7, 8, 0x00]),
        (vec![], invalid(RunlistError::BadLength))
    );
    // The sign of an 8-byte length.
    let mut negative = vec![0x08];
    negative.extend((-1i64).to_le_bytes());
    negative.push(0);
    assert_eq!(
        decode(&negative),
        (vec![], invalid(RunlistError::BadLength))
    );
    let mut largest = vec![0x08];
    largest.extend(i64::MAX.to_le_bytes());
    largest.push(0);
    assert_eq!(decode(&largest), (vec![hole(i64::MAX as u64)], COMPLETE));
}

#[test]
fn field_sizes_over_8_are_rejected() {
    for header in [0x19u8, 0x1F, 0x91, 0xF1, 0x99, 0xFF, 0x09, 0x0F] {
        let mut bytes = vec![header];
        bytes.extend([0x01; 40]);
        assert_eq!(
            decode(&bytes),
            (vec![], invalid(RunlistError::FieldTooLarge)),
            "header {header:#04x}"
        );
    }
    // 0x90: offset field too large wins over the missing length.
    assert_eq!(
        decode(&[0x90, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        (vec![], invalid(RunlistError::FieldTooLarge))
    );
}

#[test]
fn negative_cluster_numbers_are_rejected() {
    // Below zero with the first run.
    assert_eq!(
        decode(&[0x11, 0x01, 0xFF, 0x00]),
        (vec![], invalid(RunlistError::NegativeLcn))
    );
    // Below zero later: 5 - 6.
    assert_eq!(
        decode(&[0x11, 0x01, 0x05, 0x11, 0x01, 0xFA, 0x00]),
        (vec![run(5, 1)], invalid(RunlistError::NegativeLcn))
    );
    // Back to exactly zero is fine.
    assert_eq!(
        decode(&[0x11, 0x01, 0x05, 0x11, 0x01, 0xFB, 0x00]),
        (vec![run(5, 1), run(0, 1)], COMPLETE)
    );
    // LCN -1, said to mark holes on NT4 volumes, is not accepted as a run.
    let mut minus_one = vec![0x81, 0x10];
    minus_one.extend((-1i64).to_le_bytes());
    minus_one.push(0);
    assert_eq!(
        decode(&minus_one),
        (vec![], invalid(RunlistError::NegativeLcn))
    );
}

#[test]
fn nothing_overflows_63_bits() {
    let max = i64::MAX.to_le_bytes();
    // LCN i64::MAX: the run would end beyond the last cluster number.
    let mut bytes = vec![0x81, 0x01];
    bytes.extend(max);
    bytes.push(0);
    assert_eq!(decode(&bytes), (vec![], invalid(RunlistError::Overflow)));
    // LCN i64::MAX - 1 with one cluster ends exactly at the limit.
    let mut bytes = vec![0x81, 0x01];
    bytes.extend((i64::MAX - 1).to_le_bytes());
    bytes.push(0);
    assert_eq!(
        decode(&bytes),
        (vec![run(i64::MAX as u64 - 1, 1)], COMPLETE)
    );
    // The LCN sum overflows: (MAX - 1) + MAX.
    let mut bytes = vec![0x81, 0x01];
    bytes.extend((i64::MAX - 1).to_le_bytes());
    bytes.extend([0x81, 0x01]);
    bytes.extend(max);
    bytes.push(0);
    assert_eq!(
        decode(&bytes),
        (
            vec![run(i64::MAX as u64 - 1, 1)],
            invalid(RunlistError::Overflow)
        )
    );
    // The total length overflows: two holes of i64::MAX clusters.
    let mut bytes = vec![0x08];
    bytes.extend(max);
    bytes.push(0x01);
    bytes.push(0x01);
    bytes.push(0);
    assert_eq!(
        decode(&bytes),
        (vec![hole(i64::MAX as u64)], invalid(RunlistError::Overflow))
    );
    // Every run that comes out keeps the promises of `Run`.
    let mut bytes = vec![0x88];
    bytes.extend((1i64 << 62).to_le_bytes());
    bytes.extend(((1i64 << 62) - 1).to_le_bytes());
    bytes.push(0);
    let (runs, status) = decode(&bytes);
    assert_eq!(status, COMPLETE);
    assert_eq!(runs[0].lcn.unwrap() + runs[0].length, i64::MAX as u64);
}

#[test]
fn a_list_without_terminator_is_incomplete() {
    assert_eq!(decode(&[]), (vec![], invalid(RunlistError::Unterminated)));
    assert_eq!(
        decode(&[0x11, 0x01, 0x05]),
        (vec![run(5, 1)], invalid(RunlistError::Unterminated))
    );
    assert_eq!(
        decode(&[0x11, 0x01, 0x05, 0x01, 0x02]),
        (
            vec![run(5, 1), hole(2)],
            invalid(RunlistError::Unterminated)
        )
    );
}

#[test]
fn a_run_that_crosses_the_end_is_truncated() {
    // Header only.
    assert_eq!(decode(&[0x11]), (vec![], invalid(RunlistError::Truncated)));
    // Length there, offset missing.
    assert_eq!(
        decode(&[0x11, 0x01]),
        (vec![], invalid(RunlistError::Truncated))
    );
    // One of two offset bytes.
    assert_eq!(
        decode(&[0x21, 0x01, 0x05]),
        (vec![], invalid(RunlistError::Truncated))
    );
    // A good run before the cut is kept.
    assert_eq!(
        decode(&[0x11, 0x02, 0x09, 0x44, 0x01, 0x00]),
        (vec![run(9, 2)], invalid(RunlistError::Truncated))
    );
}

#[test]
fn runs_decoded_before_a_fault_are_kept_and_the_output_is_appended_to() {
    let mut runs = vec![hole(777)];
    let bytes = [0x11, 0x03, 0x10, 0x01, 0x02, 0x11, 0x00, 0x01, 0x00];
    let result = decode_runlist(&bytes, &mut runs);
    assert_eq!(
        result,
        DecodedRunlist {
            status: invalid(RunlistError::BadLength),
            runs: 2,
            clusters: 5,
            consumed: 5,
        }
    );
    // What was in the vector stays; the two good runs follow it.
    assert_eq!(runs, [hole(777), run(0x10, 3), hole(2)]);
    assert!(!result.status.is_complete());
    assert!(COMPLETE.is_complete());
}

#[test]
fn the_iterator_reports_its_state() {
    let bytes = [0x11, 0x03, 0x10, 0x01, 0x02, 0x00, 0xAA];
    let mut runs = Runs::new(&bytes);
    assert_eq!(runs.status(), None);
    assert_eq!((runs.clusters(), runs.consumed()), (0, 0));
    assert_eq!(runs.next(), Some(run(0x10, 3)));
    assert_eq!(runs.status(), None, "not finished yet");
    assert_eq!((runs.clusters(), runs.consumed()), (3, 3));
    assert_eq!(runs.next(), Some(hole(2)));
    assert!(hole(2).is_sparse() && !run(1, 1).is_sparse());
    assert_eq!((runs.clusters(), runs.consumed()), (5, 5));
    assert_eq!(runs.next(), None);
    assert_eq!(runs.status(), Some(COMPLETE));
    assert_eq!(
        runs.consumed(),
        6,
        "the terminator is consumed, the padding is not"
    );
    // Fused: asking again changes nothing.
    assert_eq!(runs.next(), None);
    assert_eq!(
        (runs.status(), runs.clusters(), runs.consumed()),
        (Some(COMPLETE), 5, 6)
    );

    let mut faulty = Runs::new(&[0x11, 0x01, 0x05, 0x19]);
    assert_eq!(faulty.next(), Some(run(5, 1)));
    assert_eq!(faulty.next(), None);
    assert_eq!(faulty.status(), Some(invalid(RunlistError::FieldTooLarge)));
    assert_eq!(faulty.consumed(), 3, "a fault consumes nothing");
    assert_eq!(faulty.next(), None);
    assert_eq!(faulty.clusters(), 1);
}

#[test]
fn the_number_of_runs_is_bounded_by_the_bytes() {
    // The densest list: two bytes per run.
    let mut bytes = Vec::new();
    for _ in 0..500 {
        bytes.extend([0x01, 0x01]);
    }
    bytes.push(0);
    let (runs, status) = decode(&bytes);
    assert_eq!((runs.len(), status), (500, COMPLETE));
    assert!(runs.len() <= bytes.len() / 2);
    let (lower, upper) = Runs::new(&bytes).size_hint();
    assert_eq!(lower, 0);
    assert!(upper.unwrap() >= 500 && upper.unwrap() <= bytes.len() / 2);
}

#[test]
fn a_fragmented_file_as_ntfs_3g_writes_it() {
    // 216 runs fit into one 1 KiB record (reference sheet, section 8):
    // alternating +3 / -2 steps, each "11 01 xx".
    let mut bytes = Vec::new();
    let mut expected = Vec::new();
    let mut lcn = 1000i64;
    bytes.extend([0x21, 0x01, 0xE8, 0x03]);
    expected.push(run(1000, 1));
    for index in 0..215 {
        let delta: i8 = if index % 2 == 0 { 3 } else { -2 };
        lcn += i64::from(delta);
        bytes.extend([0x11, 0x01, delta as u8]);
        expected.push(run(lcn as u64, 1));
    }
    bytes.push(0);
    assert_eq!(decode(&bytes), (expected, COMPLETE));
}
