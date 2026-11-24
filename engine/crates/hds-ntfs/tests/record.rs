//! `FILE` records: the probe, the header, the walk over the attributes and
//! what happens when any of it is damaged. Hand-built records only.

mod common;

use common::build::{
    mref, protect, put16, put32, resident, standard_information, Name, NonResident, RecordBuilder,
    WINDOWS_END_TAIL,
};
use hds_ntfs::{
    apply_fixups, attr_type, parse_file_record, parse_file_record_fixed, probe_file_record,
    AttrBody, AttrChain, Error, FileRecord, FileRef, FixupStatus, RecordProbe,
    DEFAULT_MAX_RECORD_SIZE, MAX_RECORD_SIZE, RECORD_IN_USE, RECORD_IS_DIRECTORY,
};

const TIMES: [u64; 4] = [
    0x01D9_1111_0000_0001,
    0x01D9_2222_0000_0002,
    0x01D9_3333_0000_0003,
    0x01D9_4444_0000_0004,
];

fn si() -> Vec<u8> {
    resident(0x10, "", 0, 0, &standard_information(TIMES, 0x20, 72))
}

fn name(text: &str) -> Vec<u8> {
    resident(0x30, "", 0, 1, &Name::new(mref(5, 5), text, 1).value())
}

/// Parses a record that is in its in-memory form.
fn parse(bytes: &[u8]) -> FileRecord<'_> {
    parse_file_record_fixed(bytes, FixupStatus::Ok).expect("record must parse")
}

// ------------------------------------------------------------------ probe

/// The first sector of a record with the given header fields.
fn sector(size: u32, usa_offset: u16, usa_count: u16) -> Vec<u8> {
    let mut s = vec![0u8; 512];
    s[0..4].copy_from_slice(b"FILE");
    put16(&mut s, 0x04, usa_offset);
    put16(&mut s, 0x06, usa_count);
    put32(&mut s, 0x1C, size);
    s
}

#[test]
fn probe_accepts_the_record_sizes_windows_writes() {
    assert_eq!(
        probe_file_record(&sector(1024, 0x30, 3), DEFAULT_MAX_RECORD_SIZE),
        Some(RecordProbe {
            record_size: 1024,
            usa_offset: 0x30,
            usa_count: 3
        })
    );
    assert_eq!(
        probe_file_record(&sector(4096, 0x30, 9), DEFAULT_MAX_RECORD_SIZE),
        Some(RecordProbe {
            record_size: 4096,
            usa_offset: 0x30,
            usa_count: 9
        })
    );
    assert_eq!(
        probe_file_record(&sector(2048, 0x30, 5), DEFAULT_MAX_RECORD_SIZE),
        Some(RecordProbe {
            record_size: 2048,
            usa_offset: 0x30,
            usa_count: 5
        })
    );
    // The 42-byte header of NTFS 3.0 and older.
    assert_eq!(
        probe_file_record(&sector(1024, 0x2A, 3), DEFAULT_MAX_RECORD_SIZE),
        Some(RecordProbe {
            record_size: 1024,
            usa_offset: 0x2A,
            usa_count: 3
        })
    );
}

#[test]
fn probe_size_is_a_power_of_two_between_1024_and_the_given_maximum() {
    assert_eq!(DEFAULT_MAX_RECORD_SIZE, 4096);
    assert_eq!(MAX_RECORD_SIZE, 65_536);
    for shift in 0..32u32 {
        let size = 1u32 << shift;
        let count = (size / 512 + 1).min(0xFFFF) as u16;
        let first = sector(size, 0x30, count);
        for max in [
            0,
            512,
            1024,
            2048,
            4096,
            5000,
            8192,
            65_536,
            1 << 20,
            u32::MAX,
        ] {
            let accepted = probe_file_record(&first, max).is_some();
            let expected = (1024..=65_536).contains(&size) && size <= max;
            assert_eq!(accepted, expected, "size {size}, max {max}");
        }
    }
    // Not powers of two, with the count such a size would need.
    for size in [0u32, 1000, 1025, 1536, 3072, 4095, 4097, 6144, 65_535] {
        let first = sector(size, 0x30, (size / 512 + 1) as u16);
        assert_eq!(
            probe_file_record(&first, MAX_RECORD_SIZE),
            None,
            "size {size}"
        );
    }
}

#[test]
fn probe_count_is_size_over_512_plus_one() {
    for (size, good) in [(1024u32, 3u16), (4096, 9), (65_536, 129)] {
        for count in [0, 1, good - 1, good, good + 1, good * 2, 0xFFFF] {
            assert_eq!(
                probe_file_record(&sector(size, 0x30, count), MAX_RECORD_SIZE).is_some(),
                count == good,
                "size {size}, count {count}"
            );
        }
    }
}

#[test]
fn probe_array_lies_behind_the_header_and_before_the_tail() {
    // Lower bound: 0x2A, the end of the oldest header.
    for (usa_offset, valid) in [
        (0u16, false),
        (8, false),
        (0x28, false),
        (0x29, false),
        (0x2A, true),
        (0x2B, true),
        (0x30, true),
    ] {
        assert_eq!(
            probe_file_record(&sector(1024, usa_offset, 3), MAX_RECORD_SIZE).is_some(),
            valid,
            "offset {usa_offset:#x}"
        );
    }
    // Upper bound: offset + 2 * count <= 510.
    for (size, count, last_good) in [(1024u32, 3u16, 504u16), (4096, 9, 492), (65_536, 129, 252)] {
        assert!(probe_file_record(&sector(size, last_good, count), MAX_RECORD_SIZE).is_some());
        assert!(probe_file_record(&sector(size, last_good + 1, count), MAX_RECORD_SIZE).is_none());
        assert!(probe_file_record(&sector(size, 0xFFFF, count), MAX_RECORD_SIZE).is_none());
    }
}

#[test]
fn probe_needs_the_magic_and_the_header() {
    let good = sector(1024, 0x30, 3);
    for position in 0..4 {
        let mut bad = good.clone();
        bad[position] ^= 0x20; // "fILE", "FiLE", ...
        assert_eq!(probe_file_record(&bad, MAX_RECORD_SIZE), None);
    }
    let mut baad = good.clone();
    baad[0..4].copy_from_slice(b"BAAD");
    assert_eq!(probe_file_record(&baad, MAX_RECORD_SIZE), None);
    assert_eq!(probe_file_record(&[0u8; 512], MAX_RECORD_SIZE), None);
    assert_eq!(probe_file_record(&[], MAX_RECORD_SIZE), None);
    assert_eq!(probe_file_record(b"FILE", MAX_RECORD_SIZE), None);
    // 0x30 bytes are enough to decide, fewer are not.
    assert!(probe_file_record(&good[..0x30], MAX_RECORD_SIZE).is_some());
    assert_eq!(probe_file_record(&good[..0x2F], MAX_RECORD_SIZE), None);
}

// ----------------------------------------------------------------- header

#[test]
fn header_fields_are_read_from_their_offsets() {
    let bytes = RecordBuilder::new(1024)
        .lsn(0x0102_0304_0506_0708)
        .sequence(0x1234)
        .links(0x0203)
        .flags(0x0003)
        .base(mref(0x0000_1122_3344_5566, 0x7788))
        .next_attr_id(0x0099)
        .number(0xDEAD_BEEF)
        .attr(&si())
        .finish();
    let record = parse(&bytes);
    let header = record.header();
    assert_eq!(header.lsn, 0x0102_0304_0506_0708);
    assert_eq!(header.sequence, 0x1234);
    assert_eq!(header.link_count, 0x0203);
    assert_eq!(header.attrs_offset, 0x38);
    assert_eq!(header.flags, 0x0003);
    assert_eq!(header.used_size, 0x38 + 0x60 + 8);
    assert_eq!(header.allocated_size, 1024);
    assert_eq!(header.base, FileRef::new(0x0000_1122_3344_5566, 0x7788));
    assert_eq!(header.next_attr_id, 0x0099);
    assert_eq!(header.record_number, Some(0xDEAD_BEEF));
    assert_eq!((header.usa_offset, header.usa_count), (0x30, 3));
    // The same through the shortcuts.
    assert_eq!(record.lsn(), 0x0102_0304_0506_0708);
    assert_eq!(record.sequence(), 0x1234);
    assert_eq!(record.link_count(), 0x0203);
    assert_eq!(record.flags(), 0x0003);
    assert_eq!(record.used_size(), 160);
    assert_eq!(record.allocated_size(), 1024);
    assert_eq!(record.base().number(), 0x0000_1122_3344_5566);
    assert_eq!(record.record_number(), Some(0xDEAD_BEEF));
    assert!(record.is_extension());
    assert_eq!(record.bytes(), &bytes[..]);
    assert_eq!(record.fixup(), FixupStatus::Ok);
}

#[test]
fn flags_in_use_and_directory() {
    assert_eq!((RECORD_IN_USE, RECORD_IS_DIRECTORY), (1, 2));
    for (flags, in_use, directory) in [
        (0x0000u16, false, false), // deleted file
        (0x0001, true, false),     // file
        (0x0002, false, true),     // deleted directory: the bit is kept
        (0x0003, true, true),      // directory
        (0x0009, true, false),     // $Secure
        (0x000D, true, false),     // $Quota, $ObjId, $Reparse
        (0xFFFC, false, false),
    ] {
        let bytes = RecordBuilder::new(1024).flags(flags).attr(&si()).finish();
        let record = parse(&bytes);
        assert_eq!(record.flags(), flags);
        assert_eq!(record.is_in_use(), in_use, "{flags:#06x}");
        assert_eq!(record.is_directory(), directory, "{flags:#06x}");
    }
}

#[test]
fn a_base_record_has_a_zero_base_reference() {
    let bytes = RecordBuilder::new(1024).attr(&si()).finish();
    let record = parse(&bytes);
    assert!(record.base().is_zero());
    assert!(!record.is_extension());
    // An extension record of base (3085, 2): flags 0x0001 only, no links.
    let bytes = RecordBuilder::new(1024)
        .base(mref(3085, 2))
        .links(0)
        .attr(
            &NonResident {
                start_vcn: 468,
                last_vcn: 900,
                runlist: &[0x21, 0x10, 0x00, 0x10, 0x00],
                ..NonResident::default()
            }
            .build(),
        )
        .finish();
    let record = parse(&bytes);
    assert!(record.is_extension());
    assert_eq!(
        (record.base().number(), record.base().sequence()),
        (3085, 2)
    );
    assert_eq!(record.link_count(), 0);
}

#[test]
fn the_record_number_exists_exactly_from_usa_offset_0x30_on() {
    // NTFS 3.1 layout.
    let bytes = RecordBuilder::new(1024).number(77).attr(&si()).finish();
    assert_eq!(parse(&bytes).record_number(), Some(77));
    assert_eq!(parse(&bytes).header().attrs_offset, 0x38);

    // NTFS 3.0 and older: the array starts at 0x2A, the bytes at 0x2C are
    // array entries (saved block tails), not a number.
    let mut old = RecordBuilder::old_layout(1024).attr(&si()).finish();
    assert_eq!(parse(&old).header().attrs_offset, 0x30);
    put32(&mut old, 0x2C, 0x0001_0001);
    let record = parse(&old);
    assert_eq!(record.record_number(), None);
    assert_eq!(record.header().usa_offset, 0x2A);
    assert_eq!(record.attribute_count(), 1);
    assert!(record.standard_information().is_some());

    // Everything in between and beyond.
    for usa_offset in [0x2Ausize, 0x2B, 0x2C, 0x2E, 0x2F] {
        let bytes = RecordBuilder::with_usa_offset(1024, usa_offset)
            .attr(&si())
            .finish();
        assert_eq!(
            parse(&bytes).record_number(),
            None,
            "offset {usa_offset:#x}"
        );
    }
    for usa_offset in [0x30usize, 0x31, 0x38, 0x100] {
        let bytes = RecordBuilder::with_usa_offset(1024, usa_offset)
            .number(9)
            .attr(&si())
            .finish();
        assert_eq!(
            parse(&bytes).record_number(),
            Some(9),
            "offset {usa_offset:#x}"
        );
    }
    // mkntfs leaves 0 in records 16-23: a claim, reported as it is.
    let bytes = RecordBuilder::new(1024).number(0).attr(&si()).finish();
    assert_eq!(parse(&bytes).record_number(), Some(0));
}

#[test]
fn records_of_4096_bytes_start_their_attributes_at_0x48() {
    let bytes = RecordBuilder::new(4096)
        .number(5)
        .attr(&si())
        .attr(&name("big"))
        .finish();
    let record = parse(&bytes);
    assert_eq!(record.header().usa_count, 9);
    assert_eq!(record.header().attrs_offset, 0x48);
    assert_eq!(record.allocated_size(), 4096);
    assert_eq!(record.attribute_count(), 2);
    let offsets: Vec<u32> = record.attributes().map(|a| a.offset).collect();
    assert_eq!(offsets, [0x48, 0x48 + 0x60]);
}

// ------------------------------------------------------- never-used state

#[test]
fn a_never_used_record_has_no_attributes() {
    // Windows 7 era and mkntfs: sequence 1; Windows XP: sequence 0.
    for sequence in [0u16, 1] {
        let bytes = RecordBuilder::new(1024)
            .flags(0)
            .sequence(sequence)
            .number(200)
            .finish();
        let record = parse(&bytes);
        assert_eq!(record.used_size(), 0x40);
        assert_eq!(record.attribute_count(), 0);
        assert_eq!(record.attr_chain(), AttrChain::Complete);
        assert!(record.is_never_used());
        assert_eq!(record.attributes().count(), 0);
        assert_eq!(record.sequence(), sequence);
    }
    // In use without attributes is damaged, not never used.
    let bytes = RecordBuilder::new(1024).flags(1).finish();
    assert!(!parse(&bytes).is_never_used());
    // Not in use with attributes is a deleted file.
    let bytes = RecordBuilder::new(1024).flags(0).attr(&si()).finish();
    assert!(!parse(&bytes).is_never_used());
}

#[test]
fn old_attribute_bytes_behind_an_early_end_marker_are_not_attributes() {
    // ntfs-3g empties a released extension record by writing the end
    // marker first; the old attributes stay behind it.
    let mut bytes = RecordBuilder::new(1024)
        .flags(0)
        .attr(&si())
        .attr(&name("stale.txt"))
        .finish();
    put32(&mut bytes, 0x38, 0xFFFF_FFFF);
    put32(&mut bytes, 0x18, 0x40);
    let record = parse(&bytes);
    assert_eq!(record.attribute_count(), 0);
    assert!(record.is_never_used());
    assert_eq!(record.file_names().count(), 0);
}

// ------------------------------------------------------------- the chain

#[test]
fn attributes_are_found_through_their_length_fields() {
    let first = si();
    let second = name("a.txt");
    // An attribute that is longer than its content needs: 16 spare bytes.
    let mut padded = resident(0x80, "", 0, 2, b"hello");
    padded.extend([0xEE; 16]);
    let padded_len = padded.len() as u32;
    put32(&mut padded, 0x04, padded_len);
    let last = resident(0xB0, "", 0, 3, &[0xFF]);
    let bytes = RecordBuilder::new(1024)
        .attr(&first)
        .attr(&second)
        .attr(&padded)
        .attr(&last)
        .finish();
    let record = parse(&bytes);
    assert_eq!(record.attribute_count(), 4);
    assert_eq!(record.attr_chain(), AttrChain::Complete);
    let seen: Vec<(u32, u32, u32, u16)> = record
        .attributes()
        .map(|a| (a.type_code, a.offset, a.length, a.id))
        .collect();
    let o1 = 0x38u32;
    let o2 = o1 + first.len() as u32;
    let o3 = o2 + second.len() as u32;
    let o4 = o3 + padded.len() as u32;
    assert_eq!(
        seen,
        [
            (0x10, o1, first.len() as u32, 0),
            (0x30, o2, second.len() as u32, 1),
            (0x80, o3, padded.len() as u32, 2),
            (0xB0, o4, last.len() as u32, 3),
        ]
    );
    assert_eq!(record.used_size(), o4 + last.len() as u32 + 8);
}

#[test]
fn the_four_bytes_behind_the_end_marker_are_not_a_length() {
    // Windows leaves 82 79 47 11 there; anything else must do no harm
    // either.
    for tail in [0, WINDOWS_END_TAIL, 1, 8, 0x18, 0x7FFF_FFFF, 0xFFFF_FFFF] {
        let bytes = RecordBuilder::new(1024)
            .end_tail(tail)
            .attr(&si())
            .attr(&name("x"))
            .finish();
        let record = parse(&bytes);
        assert_eq!(record.attr_chain(), AttrChain::Complete, "tail {tail:#x}");
        assert_eq!(record.attribute_count(), 2);
    }
    // The marker needs its four type bytes only: a used size that ends
    // right behind them is complete.
    let mut bytes = RecordBuilder::new(1024).attr(&si()).finish();
    let used = 0x38 + 0x60 + 4;
    put32(&mut bytes, 0x18, used);
    let record = parse(&bytes);
    assert_eq!(record.attr_chain(), AttrChain::Complete);
    assert_eq!(record.attribute_count(), 1);
    // One byte less cuts the marker.
    put32(&mut bytes, 0x18, used - 1);
    assert_eq!(
        parse(&bytes).attr_chain(),
        AttrChain::Broken {
            offset: 0x38 + 0x60
        }
    );
}

#[test]
fn a_chain_without_end_marker_is_broken_where_it_stops() {
    // The used size ends right behind the last attribute.
    let mut bytes = RecordBuilder::new(1024)
        .attr(&si())
        .attr(&name("x"))
        .finish();
    let end = parse(&bytes).used_size() - 8;
    put32(&mut bytes, 0x18, end);
    let record = parse(&bytes);
    assert_eq!(record.attr_chain(), AttrChain::Broken { offset: end });
    assert!(!record.attr_chain().is_complete());
    // The attributes before the fault are all there.
    assert_eq!(record.attribute_count(), 2);
    assert_eq!(record.file_names().count(), 1);
    assert!(record.standard_information().is_some());
}

#[test]
fn an_impossible_attribute_length_stops_the_walk() {
    let third_at = 0x38 + 0x60 + name("x").len() as u32;
    for (length, why) in [
        (0u32, "zero"),
        (8, "shorter than a header"),
        (0x10, "shorter than a resident header"),
        (0x17, "shorter than a resident header"),
        (0x1C, "not a multiple of 8"),
        (0x21, "not a multiple of 8"),
        (0x1000, "past the used size"),
        (0x7FFF_FFF8, "huge"),
        (0xFFFF_FFF8, "huge"),
        (0xFFFF_FFFF, "huge"),
    ] {
        let mut bytes = RecordBuilder::new(1024)
            .attr(&si())
            .attr(&name("x"))
            .attr(&resident(0x80, "", 0, 2, b"data"))
            .attr(&resident(0xB0, "", 0, 3, &[1]))
            .finish();
        put32(&mut bytes, third_at as usize + 4, length);
        let record = parse(&bytes);
        assert_eq!(
            record.attr_chain(),
            AttrChain::Broken { offset: third_at },
            "{why}: {length:#x}"
        );
        assert_eq!(record.attribute_count(), 2, "{why}: {length:#x}");
        assert_eq!(record.attributes().count(), 2);
        assert!(record.attributes().all(|a| !a.is_malformed()));
    }
}

#[test]
fn an_attribute_may_end_exactly_at_the_used_size_but_not_behind_it() {
    let mut bytes = RecordBuilder::new(1024)
        .attr(&si())
        .attr(&resident(0x80, "", 0, 1, &[7; 40]))
        .finish();
    let data_at = 0x38 + 0x60;
    let used = parse(&bytes).used_size();
    // Used size cut inside the $DATA attribute.
    put32(&mut bytes, 0x18, used - 16);
    let record = parse(&bytes);
    assert_eq!(record.attr_chain(), AttrChain::Broken { offset: data_at });
    assert_eq!(record.attribute_count(), 1);
    // The iterator stops at the used size as well, although the rest of
    // the attribute is still in the buffer.
    let walked: Vec<u32> = record.attributes().map(|a| a.type_code).collect();
    assert_eq!(walked, [0x10]);
    assert!(record.find(attr_type::DATA, "").is_none());
    // Zeros where an attribute should be.
    let mut bytes = RecordBuilder::new(1024).attr(&si()).finish();
    put32(&mut bytes, data_at as usize, 0);
    put32(&mut bytes, 0x18, 1024);
    assert_eq!(
        parse(&bytes).attr_chain(),
        AttrChain::Broken { offset: data_at }
    );
}

#[test]
fn a_full_record_is_walked_to_its_last_byte() {
    // Attributes up to the very end: the end marker occupies the last 8
    // bytes of the record.
    let filler = 1024 - 0x38 - 0x60 - 8 - 0x18;
    let bytes = RecordBuilder::new(1024)
        .attr(&si())
        .attr(&resident(0x80, "", 0, 1, &vec![0xAB; filler]))
        .finish();
    let record = parse(&bytes);
    assert_eq!(record.used_size(), 1024);
    assert_eq!(record.attr_chain(), AttrChain::Complete);
    let data = record
        .find(attr_type::DATA, "")
        .unwrap()
        .resident()
        .unwrap();
    assert_eq!(data.bytes.len(), filler);
    assert_eq!(data.offset as usize + filler, 1024 - 8);
}

// ------------------------------------------------- malformed attributes

/// A record `[$STANDARD_INFORMATION][victim][$FILE_NAME]` in which the
/// middle attribute has been damaged by `damage`.
fn with_damaged_middle(victim: Vec<u8>, damage: impl Fn(&mut [u8])) -> Vec<u8> {
    let mut victim = victim;
    damage(&mut victim);
    RecordBuilder::new(1024)
        .attr(&si())
        .attr(&victim)
        .attr(&name("after.txt"))
        .finish()
}

fn assert_middle_malformed(bytes: &[u8], what: &str) {
    let record = parse(bytes);
    assert_eq!(record.attr_chain(), AttrChain::Complete, "{what}");
    assert_eq!(record.attribute_count(), 3, "{what}");
    let attributes: Vec<_> = record.attributes().collect();
    assert_eq!(attributes.len(), 3, "{what}");
    assert!(!attributes[0].is_malformed(), "{what}");
    assert!(
        attributes[1].is_malformed(),
        "{what}: the damaged attribute"
    );
    assert!(matches!(attributes[1].body, AttrBody::Malformed));
    assert!(attributes[1].resident().is_none() && attributes[1].extent().is_none());
    // The walk goes on behind it.
    assert_eq!(
        attributes[2].file_name().unwrap().name.to_string_lossy(),
        "after.txt",
        "{what}"
    );
}

#[test]
fn a_resident_value_outside_its_attribute_is_malformed() {
    let victim = || resident(0x80, "", 0, 7, &[0x55; 32]);
    // Length reaching past the attribute.
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put32(a, 0x10, 33)),
        "one byte too long",
    );
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put32(a, 0x10, 0xFFFF_FFFF)),
        "huge value length",
    );
    // Offset past the attribute.
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put16(a, 0x14, 0x40)),
        "offset moved up",
    );
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put16(a, 0x14, 0xFFFF)),
        "huge value offset",
    );
    // A value that would overlap the attribute header.
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put16(a, 0x14, 0x10)),
        "value inside the header",
    );
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put16(a, 0x14, 0)),
        "value at 0",
    );
    // Type, offset, length and id of a malformed attribute are still told.
    let bytes = with_damaged_middle(victim(), |a| put32(a, 0x10, 33));
    let attr = parse(&bytes).attributes().nth(1).unwrap();
    assert_eq!(
        (attr.type_code, attr.offset, attr.length, attr.id),
        (0x80, 0x38 + 0x60, 0x38, 7)
    );
}

#[test]
fn an_empty_resident_value_may_sit_anywhere_inside_the_attribute() {
    for value_offset in [0x18u16, 0, 0x10, 0x20] {
        let bytes = with_damaged_middle(resident(0x80, "", 0, 1, &[0; 8]), |a| {
            put32(a, 0x10, 0);
            put16(a, 0x14, value_offset);
        });
        let record = parse(&bytes);
        let data = record.find(attr_type::DATA, "").unwrap();
        assert!(!data.is_malformed(), "offset {value_offset:#x}");
        assert!(data.resident().unwrap().is_empty());
    }
    // But not behind it.
    assert_middle_malformed(
        &with_damaged_middle(resident(0x80, "", 0, 1, &[0; 8]), |a| {
            put32(a, 0x10, 0);
            put16(a, 0x14, 0x21);
        }),
        "empty value behind the attribute",
    );
}

#[test]
fn a_name_outside_its_attribute_is_malformed() {
    let victim = || resident(0x80, "stream", 0, 7, b"content");
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| a[0x09] = 200),
        "name longer than the attribute",
    );
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put16(a, 0x0A, 0xFFF0)),
        "name offset far away",
    );
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put16(a, 0x0A, 0x10)),
        "name inside the header",
    );
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put16(a, 0x0A, 0)),
        "name at 0",
    );
    // Non-resident: the name must lie behind the 0x40-byte header.
    let victim = || {
        NonResident {
            name: "stream",
            ..NonResident::default()
        }
        .build()
    };
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put16(a, 0x0A, 0x38)),
        "name inside the non-resident header",
    );
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| a[0x09] = 0xFF),
        "name too long",
    );
}

#[test]
fn the_name_survives_when_only_the_value_is_bad() {
    let bytes = with_damaged_middle(resident(0x80, "Zone.Identifier", 0, 7, b"x"), |a| {
        put32(a, 0x10, 0x4000)
    });
    let attr = parse(&bytes).attributes().nth(1).unwrap();
    assert!(attr.is_malformed());
    assert!(
        attr.is_named("Zone.Identifier"),
        "a damaged alternate stream must not look like the unnamed one"
    );
    assert!(!attr.is_unnamed());
    let bytes = with_damaged_middle(
        NonResident {
            name: "Zone.Identifier",
            ..NonResident::default()
        }
        .build(),
        |a| put16(a, 0x20, 0x08),
    );
    let attr = parse(&bytes).attributes().nth(1).unwrap();
    assert!(attr.is_malformed() && attr.is_named("Zone.Identifier"));
}

#[test]
fn a_form_byte_other_than_0_or_1_is_malformed() {
    for form in [2u8, 3, 0x80, 0xFF] {
        assert_middle_malformed(
            &with_damaged_middle(resident(0x80, "", 0, 7, b"content"), |a| a[0x08] = form),
            "form byte",
        );
        // Also when everything else is a perfect non-resident attribute
        // (or a resident one that is long enough to be read as one).
        let non_resident = NonResident {
            runlist: &[0x11, 0x01, 0x05, 0x00],
            ..NonResident::default()
        }
        .build();
        assert_middle_malformed(
            &with_damaged_middle(non_resident, |a| a[0x08] = form),
            "form byte of a non-resident attribute",
        );
        let mut long = resident(0x80, "", 0, 7, &[0u8; 0x60]);
        put16(&mut long, 0x20, 0x40);
        assert_middle_malformed(
            &with_damaged_middle(long, |a| a[0x08] = form),
            "form byte of a long resident attribute",
        );
    }
}

#[test]
fn a_non_resident_header_must_be_complete_and_point_into_the_attribute() {
    // A non-resident attribute needs 0x40 bytes of header.
    let short = with_damaged_middle(resident(0x80, "", 0, 7, &[0; 0x20]), |a| a[0x08] = 1);
    assert_eq!(parse(&short).attributes().nth(1).unwrap().length, 0x38);
    assert_middle_malformed(&short, "non-resident with 0x38 bytes");
    // Mapping pairs offset inside the header or behind the attribute.
    let victim = || {
        NonResident {
            runlist: &[0x11, 0x01, 0x05, 0x00],
            ..NonResident::default()
        }
        .build()
    };
    assert_eq!(victim().len(), 0x48);
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put16(a, 0x20, 0x3F)),
        "runlist inside the header",
    );
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put16(a, 0x20, 0)),
        "runlist at 0",
    );
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put16(a, 0x20, 0x49)),
        "runlist behind the attribute",
    );
    assert_middle_malformed(
        &with_damaged_middle(victim(), |a| put16(a, 0x20, 0xFFFF)),
        "runlist far away",
    );
    // Exactly at the end: the attribute is well formed, its runlist empty
    // and therefore unterminated.
    let bytes = with_damaged_middle(victim(), |a| put16(a, 0x20, 0x48));
    let attr = parse(&bytes).attributes().nth(1).unwrap();
    let extent = attr.extent().expect("well formed");
    assert!(extent.runlist.is_empty());
    let mut runs = extent.runs();
    assert_eq!(runs.next(), None);
    assert_eq!(
        runs.status(),
        Some(hds_ntfs::RunlistStatus::Invalid(
            hds_ntfs::RunlistError::Unterminated
        ))
    );
}

// ----------------------------------------------------------- header errors

#[test]
fn the_first_attribute_must_lie_behind_the_array_inside_the_record_aligned() {
    let good = RecordBuilder::new(1024).attr(&si()).finish();
    for (offset, valid) in [
        (0u16, false),
        (0x30, false), // on the array
        (0x35, false), // inside the array
        (0x36, false), // its end, but not a multiple of 8
        (0x38, true),
        (0x3C, false),  // not a multiple of 8
        (0x400, false), // the record size
        (0x408, false),
        (0xFFF8, false),
    ] {
        let mut bytes = good.clone();
        put16(&mut bytes, 0x14, offset);
        let result = parse_file_record_fixed(&bytes, FixupStatus::Ok);
        if valid {
            assert!(result.is_ok(), "offset {offset:#x}");
        } else {
            assert_eq!(
                result.err(),
                Some(Error::BadAttrsOffset),
                "offset {offset:#x}"
            );
        }
    }
    // Later starts are fine as long as an attribute or the end marker is
    // there.
    let mut bytes = good.clone();
    put16(&mut bytes, 0x14, 0x38 + 0x60);
    let record = parse(&bytes);
    assert_eq!(
        (record.attribute_count(), record.attr_chain()),
        (0, AttrChain::Complete)
    );
}

#[test]
fn the_used_size_must_cover_the_first_attribute_type_and_fit_the_record() {
    let good = RecordBuilder::new(1024).finish(); // end marker at 0x38, used 0x40
    for (used, valid) in [
        (0u32, false),
        (0x38, false),
        (0x3B, false),
        (0x3C, true), // the 4 type bytes of the end marker
        (0x40, true),
        (1024, true),
        (1025, false),
        (0x1_0000, false),
        (u32::MAX, false),
    ] {
        let mut bytes = good.clone();
        put32(&mut bytes, 0x18, used);
        let result = parse_file_record_fixed(&bytes, FixupStatus::Ok);
        if valid {
            assert_eq!(
                result.unwrap().attr_chain(),
                AttrChain::Complete,
                "used {used:#x}"
            );
        } else {
            assert_eq!(result.err(), Some(Error::BadUsedSize), "used {used:#x}");
        }
    }
}

#[test]
fn the_record_must_be_all_there() {
    let mut disk = RecordBuilder::new(4096).attr(&si()).finish_on_disk(5);
    let pristine = disk.clone();
    assert_eq!(
        parse_file_record(&mut disk[..4095]).err(),
        Some(Error::TooShort)
    );
    assert_eq!(
        parse_file_record(&mut disk[..1024]).err(),
        Some(Error::TooShort)
    );
    assert_eq!(
        parse_file_record(&mut disk[..0x2F]).err(),
        Some(Error::TooShort)
    );
    assert_eq!(parse_file_record(&mut []).err(), Some(Error::TooShort));
    assert_eq!(disk, pristine, "a refused buffer must not be modified");
    assert_eq!(
        parse_file_record_fixed(&disk[..4095], FixupStatus::Ok).err(),
        Some(Error::TooShort)
    );
    // The header checks of the probe apply.
    let mut bad = pristine.clone();
    bad[0] = b'f';
    assert_eq!(parse_file_record(&mut bad).err(), Some(Error::BadMagic));
    let mut bad = pristine.clone();
    put32(&mut bad, 0x1C, 3000);
    assert_eq!(
        parse_file_record(&mut bad).err(),
        Some(Error::BadRecordSize)
    );
    let mut bad = pristine.clone();
    put16(&mut bad, 0x06, 8);
    assert_eq!(parse_file_record(&mut bad).err(), Some(Error::BadUsa));
    let mut bad = pristine.clone();
    put16(&mut bad, 0x04, 0x20);
    assert_eq!(parse_file_record(&mut bad).err(), Some(Error::BadUsa));
}

#[test]
fn bytes_behind_the_record_are_neither_read_nor_modified() {
    let mut buffer = RecordBuilder::new(1024)
        .number(3)
        .attr(&si())
        .finish_on_disk(0x0E0E);
    buffer.extend([0x0E; 3072]); // looks like more blocks ending in the number
    let record = parse_file_record(&mut buffer).unwrap();
    assert_eq!(record.bytes().len(), 1024);
    assert_eq!(record.fixup(), FixupStatus::Ok);
    assert_eq!(record.record_number(), Some(3));
    assert!(buffer[1024..].iter().all(|b| *b == 0x0E));
}

// ------------------------------------------------------------------ fixups

#[test]
fn values_are_valid_only_after_fixups() {
    // A resident value and a file name that lie across the end of the
    // first 512-byte block.
    let payload: Vec<u8> = (0..400u32).map(|i| (i * 7 + 3) as u8).collect();
    let long_name: String = (0..200)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    for attribute in [resident(0x80, "", 0, 1, &payload), name(&long_name)] {
        let memory = RecordBuilder::new(1024)
            .attr(&si())
            .attr(&attribute)
            .finish();
        let mut disk = memory.clone();
        protect(&mut disk, 0xC1C2);
        assert_eq!(&disk[510..512], &[0xC2, 0xC1]);

        // Parsed without fixups, the number shows up inside the value.
        let raw = parse_file_record_fixed(&disk, FixupStatus::Ok).unwrap();
        let raw_value = raw.attributes().nth(1).unwrap().resident().unwrap();
        assert!(raw_value.offset < 510 && raw_value.offset + raw_value.len() > 512);
        let spoiled = 510 - raw_value.offset as usize;
        assert_eq!(&raw_value.bytes[spoiled..spoiled + 2], &[0xC2, 0xC1]);

        // parse_file_record applies them first.
        let record = parse_file_record(&mut disk).unwrap();
        assert_eq!(record.fixup(), FixupStatus::Ok);
        let value = record.attributes().nth(1).unwrap().resident().unwrap();
        let expected = parse(&memory)
            .attributes()
            .nth(1)
            .unwrap()
            .resident()
            .unwrap();
        assert_eq!(value.bytes, expected.bytes);
        assert_eq!(value.offset, expected.offset);
    }
    // The name comes out whole.
    let mut disk = RecordBuilder::new(1024)
        .attr(&si())
        .attr(&name(&long_name))
        .finish_on_disk(0xFFFF);
    let record = parse_file_record(&mut disk).unwrap();
    assert_eq!(
        record.file_names().next().unwrap().name.to_string_lossy(),
        long_name
    );
}

#[test]
fn a_4k_record_is_fixed_up_at_every_512_bytes() {
    let payload: Vec<u8> = (0..3800u32).map(|i| (i % 253) as u8 | 1).collect();
    let memory = RecordBuilder::new(4096)
        .attr(&si())
        .attr(&resident(0x80, "", 0, 1, &payload))
        .finish();
    let mut disk = memory.clone();
    protect(&mut disk, 0);
    let record = parse_file_record(&mut disk).unwrap();
    assert_eq!(record.fixup(), FixupStatus::Ok);
    let value = record
        .find(attr_type::DATA, "")
        .unwrap()
        .resident()
        .unwrap();
    assert_eq!(
        value.bytes,
        &payload[..],
        "all seven inner block tails restored"
    );
    assert_eq!(value.offset, 0x48 + 0x60 + 0x18);
}

#[test]
fn a_torn_record_is_parsed_as_far_as_it_goes() {
    let mut disk = RecordBuilder::new(1024)
        .number(41)
        .attr(&si())
        .attr(&name("torn.txt"))
        .attr(&resident(0x80, "", 0, 2, &[0x33; 600]))
        .finish_on_disk(0x0101);
    // The second block belongs to another version of the record.
    put16(&mut disk, 1022, 0x0100);
    let record = parse_file_record(&mut disk).unwrap();
    assert_eq!(
        record.fixup(),
        FixupStatus::Torn {
            first_bad_block: 1,
            bad_blocks: 1
        }
    );
    assert!(record.fixup().is_torn());
    assert_eq!(record.record_number(), Some(41));
    assert_eq!(
        record.file_names().next().unwrap().name.to_string_lossy(),
        "torn.txt"
    );
    assert_eq!(record.attribute_count(), 3);
    // The first block's tail was still restored.
    let value = record
        .find(attr_type::DATA, "")
        .unwrap()
        .resident()
        .unwrap();
    assert!(value.bytes.iter().all(|b| *b == 0x33));
}

#[test]
fn the_fixed_variant_trusts_the_caller() {
    let memory = RecordBuilder::new(1024)
        .attr(&si())
        .attr(&name("x"))
        .finish();
    let mut disk = memory.clone();
    protect(&mut disk, 0x2222);
    // Two steps by hand ...
    let mut stepwise = disk.clone();
    let status = apply_fixups(&mut stepwise).unwrap();
    let by_steps = parse_file_record_fixed(&stepwise, status).unwrap();
    // ... equal one step.
    let mut at_once = disk.clone();
    let direct = parse_file_record(&mut at_once).unwrap();
    assert_eq!(by_steps.bytes(), direct.bytes());
    assert_eq!(by_steps.header(), direct.header());
    assert_eq!(by_steps.fixup(), FixupStatus::Ok);
    // The status is recorded as given.
    let torn = FixupStatus::Torn {
        first_bad_block: 1,
        bad_blocks: 1,
    };
    assert_eq!(
        parse_file_record_fixed(&memory, torn).unwrap().fixup(),
        torn
    );
}

#[test]
fn the_hash_belongs_to_the_bytes_as_read() {
    let mut disk = RecordBuilder::new(1024)
        .attr(&si())
        .attr(&resident(0x80, "", 0, 1, &[9; 500]))
        .finish_on_disk(0x4321);
    let as_read = disk.clone();
    let before = hds_ntfs::fnv1a64(&disk);
    parse_file_record(&mut disk).unwrap();
    assert_ne!(disk, as_read, "parsing fixes the buffer up in place");
    assert_ne!(hds_ntfs::fnv1a64(&disk), before);
    assert_eq!(hds_ntfs::fnv1a64(&as_read), before);
}
