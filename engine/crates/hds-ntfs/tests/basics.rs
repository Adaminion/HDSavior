//! File references and the reference rule, FILETIME conversion, FNV-1a and
//! borrowed UTF-16 names.

use hds_ntfs::{
    filetime_is_valid, filetime_to_unix_ms, filetime_to_unix_time, fnv1a64, next_seq,
    reference_resolves, unix_ms_to_filetime, FileRef, Utf16Name, FILETIME_UNIX_EPOCH,
};

// ---------------------------------------------------------------- FileRef

#[test]
fn file_ref_splits_48_and_16_bits() {
    let reference = FileRef::from_raw(0xABCD_0000_0000_0005);
    assert_eq!(reference.number(), 5);
    assert_eq!(reference.sequence(), 0xABCD);
    assert_eq!(reference.raw(), 0xABCD_0000_0000_0005);

    // The highest record number and sequence that fit.
    let top = FileRef::from_raw(u64::MAX);
    assert_eq!(top.number(), 0x0000_FFFF_FFFF_FFFF);
    assert_eq!(top.sequence(), 0xFFFF);

    // Bit 47 belongs to the number, bit 48 to the sequence.
    assert_eq!(FileRef::from_raw(1 << 47).number(), 1 << 47);
    assert_eq!(FileRef::from_raw(1 << 47).sequence(), 0);
    assert_eq!(FileRef::from_raw(1 << 48).number(), 0);
    assert_eq!(FileRef::from_raw(1 << 48).sequence(), 1);
}

#[test]
fn file_ref_is_built_from_its_parts() {
    let reference = FileRef::new(0x1234_5678_9ABC, 0x00FE);
    assert_eq!(reference.raw(), 0x00FE_1234_5678_9ABC);
    assert_eq!(reference.number(), 0x1234_5678_9ABC);
    assert_eq!(reference.sequence(), 0x00FE);
    // Only 48 bits of the number are used; the sequence is not disturbed.
    let cut = FileRef::new(0xFFFF_0000_0000_0007, 3);
    assert_eq!((cut.number(), cut.sequence()), (7, 3));
    // The root directory: record 5, sequence 5.
    assert_eq!(FileRef::new(5, 5).raw(), 0x0005_0000_0000_0005);
}

#[test]
fn file_ref_conversions_and_zero() {
    let raw = 0x0002_0000_0000_0021_u64;
    let reference: FileRef = raw.into();
    assert_eq!(u64::from(reference), raw);
    assert!(FileRef::ZERO.is_zero());
    assert!(FileRef::default().is_zero());
    assert!(!reference.is_zero());
    // A sequence alone makes it non-zero.
    assert!(!FileRef::new(0, 1).is_zero());
    assert_eq!(format!("{reference}"), "33/2");
    assert_eq!(format!("{reference:?}"), "FileRef(33, 2)");
}

#[test]
fn file_ref_orders_by_number_then_sequence() {
    let mut refs = [
        FileRef::new(7, 1),
        FileRef::new(5, 9),
        FileRef::new(5, 2),
        FileRef::new(1 << 40, 0),
        FileRef::new(6, 0xFFFF),
    ];
    refs.sort();
    let sorted: Vec<(u64, u16)> = refs.iter().map(|r| (r.number(), r.sequence())).collect();
    assert_eq!(sorted, [(5, 2), (5, 9), (6, 0xFFFF), (7, 1), (1 << 40, 0)]);
    // The raw values order differently (sequence first): the ordering is
    // not the one of the raw integers.
    assert!(FileRef::new(7, 1).raw() < FileRef::new(5, 2).raw());
    assert!(FileRef::new(7, 1) > FileRef::new(5, 2));
}

// --------------------------------------------------------- reference rule

#[test]
fn next_seq_wraps_to_one() {
    assert_eq!(next_seq(1), 2);
    assert_eq!(next_seq(2), 3);
    assert_eq!(next_seq(0xFFFE), 0xFFFF);
    assert_eq!(next_seq(0xFFFF), 1);
    assert_eq!(next_seq(0), 1);
    for sequence in 0..=u16::MAX {
        let next = next_seq(sequence);
        assert_ne!(next, 0, "the counter skips 0");
        if sequence != 0xFFFF {
            assert_eq!(u32::from(next), u32::from(sequence) + 1);
        }
    }
}

#[test]
fn a_reference_resolves_to_the_live_record_with_its_sequence() {
    assert!(reference_resolves(1, 1, true));
    assert!(reference_resolves(7, 7, true));
    // In use with the next sequence: the slot holds a different, newer
    // object.
    assert!(!reference_resolves(1, 2, true));
    assert!(!reference_resolves(2, 1, true));
}

#[test]
fn a_reference_resolves_to_the_freed_record_with_the_next_sequence() {
    // Freeing bumps the sequence: the deleted object is found one step on.
    assert!(reference_resolves(1, 2, false));
    assert!(reference_resolves(0xFFFF, 1, false));
    // Free with the same sequence: a never-used slot or another object.
    assert!(!reference_resolves(1, 1, false));
    // Freed twice since.
    assert!(!reference_resolves(1, 3, false));
    assert!(!reference_resolves(0xFFFF, 0, false));
    assert!(!reference_resolves(0xFFFF, 0xFFFF, false));
}

#[test]
fn the_rule_on_the_windows_xp_sample() {
    // docs/notes/ntfs-on-disk.md, section 3: dir1\dir2\frag3.dat resolves
    // through two deleted directories whose children hold (33, 1) and
    // (34, 1) while the records are free with sequence 2.
    assert!(FileRef::new(33, 1).resolves_to(2, false));
    assert!(FileRef::new(34, 1).resolves_to(2, false));
    // sing2.dat holds (37, 1); slot 37 was a directory, was deleted, became
    // a file and was deleted again: free with sequence 3. Not its parent.
    assert!(!FileRef::new(37, 1).resolves_to(3, false));
}

#[test]
fn the_rule_holds_for_every_sequence_number() {
    for sequence in 0..=u16::MAX {
        let next = if sequence == 0xFFFF { 1 } else { sequence + 1 };
        for target in [
            0,
            1,
            2,
            sequence.wrapping_sub(1),
            sequence,
            sequence.wrapping_add(1),
            0xFFFE,
            0xFFFF,
        ] {
            assert_eq!(
                reference_resolves(sequence, target, true),
                target == sequence,
                "in use, reference {sequence}, target {target}"
            );
            assert_eq!(
                reference_resolves(sequence, target, false),
                target == next,
                "free, reference {sequence}, target {target}"
            );
            assert_eq!(
                FileRef::new(99, sequence).resolves_to(target, true),
                target == sequence
            );
            assert_eq!(
                FileRef::new(99, sequence).resolves_to(target, false),
                target == next
            );
        }
    }
}

// --------------------------------------------------------------- FILETIME

/// 2^63, the first FILETIME that is not valid.
const TOP: u64 = 1 << 63;

#[test]
fn filetime_validity_is_the_open_range_from_zero_to_two_to_the_63() {
    assert!(!filetime_is_valid(0));
    assert!(filetime_is_valid(1));
    assert!(filetime_is_valid(FILETIME_UNIX_EPOCH));
    assert!(filetime_is_valid(TOP - 1));
    assert!(!filetime_is_valid(TOP));
    assert!(!filetime_is_valid(u64::MAX));
}

#[test]
fn filetime_to_unix_ms_known_values() {
    assert_eq!(FILETIME_UNIX_EPOCH, 116_444_736_000_000_000);
    assert_eq!(filetime_to_unix_ms(FILETIME_UNIX_EPOCH), Some(0));
    assert_eq!(filetime_to_unix_ms(FILETIME_UNIX_EPOCH + 10_000), Some(1));
    assert_eq!(filetime_to_unix_ms(FILETIME_UNIX_EPOCH + 9_999), Some(0));
    // 2004-02-29 19:57:57.513 UTC, the format time of the XP sample.
    assert_eq!(
        filetime_to_unix_ms(127_225_582_775_130_528),
        Some(1_078_084_677_513)
    );
    // 2021-05-01 17:38:44.052 UTC, from the Windows 10 sample.
    assert_eq!(
        filetime_to_unix_ms(132_643_643_240_526_646),
        Some(1_619_890_724_052)
    );
}

#[test]
fn filetime_to_unix_ms_rounds_down_before_1970() {
    assert_eq!(filetime_to_unix_ms(FILETIME_UNIX_EPOCH - 1), Some(-1));
    assert_eq!(filetime_to_unix_ms(FILETIME_UNIX_EPOCH - 10_000), Some(-1));
    assert_eq!(filetime_to_unix_ms(FILETIME_UNIX_EPOCH - 10_001), Some(-2));
    // The first valid tick: 100 ns after 1601-01-01.
    assert_eq!(filetime_to_unix_ms(1), Some(-11_644_473_600_000));
    assert_eq!(filetime_to_unix_ms(10_000), Some(-11_644_473_599_999));
}

#[test]
fn filetime_to_unix_ms_refuses_what_is_not_a_time() {
    assert_eq!(filetime_to_unix_ms(0), None);
    assert_eq!(filetime_to_unix_ms(TOP), None);
    assert_eq!(filetime_to_unix_ms(u64::MAX), None);
    // The last valid tick is representable.
    assert_eq!(filetime_to_unix_ms(TOP - 1), Some(910_692_730_085_477));
}

#[test]
fn filetime_to_unix_time_keeps_the_ticks() {
    assert_eq!(filetime_to_unix_time(FILETIME_UNIX_EPOCH), Some((0, 0)));
    assert_eq!(
        filetime_to_unix_time(FILETIME_UNIX_EPOCH + 1),
        Some((0, 100))
    );
    assert_eq!(
        filetime_to_unix_time(FILETIME_UNIX_EPOCH + 9_999_999),
        Some((0, 999_999_900))
    );
    assert_eq!(
        filetime_to_unix_time(FILETIME_UNIX_EPOCH + 10_000_000),
        Some((1, 0))
    );
    assert_eq!(
        filetime_to_unix_time(FILETIME_UNIX_EPOCH - 1),
        Some((-1, 999_999_900))
    );
    assert_eq!(
        filetime_to_unix_time(127_225_582_775_130_528),
        Some((1_078_084_677, 513_052_800))
    );
    assert_eq!(filetime_to_unix_time(1), Some((-11_644_473_600, 100)));
    assert_eq!(
        filetime_to_unix_time(TOP - 1),
        Some((910_692_730_085, 477_580_700))
    );
    assert_eq!(filetime_to_unix_time(0), None);
    assert_eq!(filetime_to_unix_time(TOP), None);
}

#[test]
fn unix_ms_to_filetime_is_checked() {
    assert_eq!(unix_ms_to_filetime(0), Some(FILETIME_UNIX_EPOCH));
    assert_eq!(unix_ms_to_filetime(1), Some(FILETIME_UNIX_EPOCH + 10_000));
    assert_eq!(unix_ms_to_filetime(-1), Some(FILETIME_UNIX_EPOCH - 10_000));
    assert_eq!(
        unix_ms_to_filetime(1_078_084_677_513),
        Some(127_225_582_775_130_000)
    );
    // 1601-01-01 itself is FILETIME 0, which is "not set".
    assert_eq!(unix_ms_to_filetime(-11_644_473_600_000), None);
    assert_eq!(unix_ms_to_filetime(-11_644_473_599_999), Some(10_000));
    assert_eq!(unix_ms_to_filetime(-11_644_473_600_001), None);
    // The last millisecond whose FILETIME is below 2^63.
    assert_eq!(
        unix_ms_to_filetime(910_692_730_085_477),
        Some(9_223_372_036_854_770_000)
    );
    assert_eq!(unix_ms_to_filetime(910_692_730_085_478), None);
    assert_eq!(unix_ms_to_filetime(i64::MAX), None);
    assert_eq!(unix_ms_to_filetime(i64::MIN), None);
}

#[test]
fn milliseconds_survive_the_round_trip() {
    for ms in [
        -11_644_473_599_999_i64,
        -1,
        0,
        1,
        86_399_999,
        1_078_084_677_513,
        4_102_444_800_000,
        910_692_730_085_477,
    ] {
        let filetime = unix_ms_to_filetime(ms).unwrap();
        assert!(filetime_is_valid(filetime));
        assert_eq!(filetime_to_unix_ms(filetime), Some(ms));
    }
}

// ----------------------------------------------------------------- FNV-1a

#[test]
fn fnv1a64_published_vectors() {
    // From the reference implementation's test suite (Fowler/Noll/Vo).
    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a64(b"b"), 0xaf63_df4c_8601_f1a5);
    assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    assert_eq!(fnv1a64(b"\0"), 0xaf63_bd4c_8601_b7df);
}

#[test]
fn fnv1a64_depends_on_every_byte_and_its_position() {
    let record = vec![0u8; 1024];
    let base = fnv1a64(&record);
    for position in [0, 1, 510, 511, 1022, 1023] {
        let mut changed = record.clone();
        changed[position] = 1;
        assert_ne!(fnv1a64(&changed), base, "byte {position}");
    }
    assert_ne!(fnv1a64(&record[..1023]), base);
    assert_ne!(fnv1a64(b"ab"), fnv1a64(b"ba"));
}

// ------------------------------------------------------------------ names

#[test]
fn utf16_names_are_borrowed_code_units() {
    let bytes = [b'$', 0, b'I', 0, b'3', 0, b'0', 0];
    let name = Utf16Name::from_bytes(&bytes);
    assert_eq!(name.len(), 4);
    assert!(!name.is_empty());
    assert_eq!(name.as_bytes(), &bytes);
    assert_eq!(name.units().collect::<Vec<u16>>(), [0x24, 0x49, 0x33, 0x30]);
    assert_eq!(name.units().len(), 4);
    assert_eq!(name.units().next_back(), Some(0x30));
    assert!(name.eq_str("$I30"));
    assert!(name == "$I30");
    assert!(!name.eq_str("$i30"), "comparison is case-sensitive");
    assert!(!name.eq_str("$I3"));
    assert!(!name.eq_str("$I300"));
    assert_eq!(name.to_string_lossy(), "$I30");
    assert_eq!(format!("{name}"), "$I30");
    assert_eq!(format!("{name:?}"), "\"$I30\"");
}

#[test]
fn utf16_names_keep_what_is_not_valid_utf16() {
    // "a", an unpaired high surrogate, U+1F600 as a pair, "z".
    let units: [u16; 5] = [0x61, 0xD800, 0xD83D, 0xDE00, 0x7A];
    let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
    let name = Utf16Name::from_bytes(&bytes);
    assert_eq!(name.len(), 5);
    assert_eq!(name.units().collect::<Vec<u16>>(), units);
    assert_eq!(name.to_string_lossy(), "a\u{FFFD}\u{1F600}z");
    assert!(
        !name.eq_str("a\u{FFFD}\u{1F600}z"),
        "the replacement is only for display"
    );
    assert!(Utf16Name::from_bytes(&bytes[4..8]).eq_str("\u{1F600}"));
}

#[test]
fn utf16_names_drop_an_odd_trailing_byte() {
    let name = Utf16Name::from_bytes(&[0x41, 0x00, 0x42]);
    assert_eq!(name.len(), 1);
    assert_eq!(name.as_bytes(), &[0x41, 0x00]);
    assert!(name.eq_str("A"));
    let empty = Utf16Name::from_bytes(&[0x41]);
    assert!(empty.is_empty());
    assert_eq!(empty, Utf16Name::EMPTY);
    assert!(Utf16Name::EMPTY.eq_str(""));
    assert_eq!(Utf16Name::default().len(), 0);
}
