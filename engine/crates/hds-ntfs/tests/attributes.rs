//! The attributes a recovery reads, decoded from hand-built records:
//! `$STANDARD_INFORMATION`, `$FILE_NAME`, `$DATA`, `$ATTRIBUTE_LIST`,
//! `$INDEX_ROOT`, `$INDEX_ALLOCATION`, `$BITMAP`, `$VOLUME_NAME` and
//! `$REPARSE_POINT`, with the pitfalls of docs/notes/ntfs-on-disk.md.

mod common;

use common::build::{
    attr_list_entry, index_entry, index_root, last_entry, mref, put16, put32, resident,
    standard_information, units_le, Name, NonResident, RecordBuilder,
};
use hds_ntfs::{
    attr_type, parse_file_record_fixed, AttrBody, AttrListEntries, Attribute, AttributeList,
    FileName, FileRecord, FileRef, FixupStatus, Namespace, Run, RunlistError, RunlistStatus,
    StreamSizes, Times, WalkEnd, ATTR_FLAG_COMPRESSION_MASK, ATTR_FLAG_ENCRYPTED, ATTR_FLAG_SPARSE,
    FILE_ATTR_DIRECTORY, FILE_NAME_MIN_LEN, I30, REPARSE_TAG_CLOUD, REPARSE_TAG_CLOUD_MASK,
    REPARSE_TAG_DEDUP, REPARSE_TAG_MOUNT_POINT, REPARSE_TAG_SYMLINK, REPARSE_TAG_WOF,
};

const TIMES: [u64; 4] = [
    0x01D9_1111_0000_0001,
    0x01D9_2222_0000_0002,
    0x01D9_3333_0000_0003,
    0x01D9_4444_0000_0004,
];

fn parse(bytes: &[u8]) -> FileRecord<'_> {
    parse_file_record_fixed(bytes, FixupStatus::Ok).expect("record must parse")
}

/// A record holding just the given attributes.
fn record_of(attributes: &[Vec<u8>]) -> Vec<u8> {
    let mut builder = RecordBuilder::new(4096);
    for attribute in attributes {
        builder = builder.attr(attribute);
    }
    builder.finish()
}

/// The only attribute of a one-attribute record.
fn only(bytes: &[u8]) -> Attribute<'_> {
    let record = parse(bytes);
    assert_eq!(record.attribute_count(), 1);
    record.attributes().next().unwrap()
}

// -------------------------------------------------- $STANDARD_INFORMATION

#[test]
fn standard_information_in_both_sizes() {
    for len in [48usize, 72] {
        let bytes = record_of(&[resident(
            0x10,
            "",
            0,
            0,
            &standard_information(TIMES, 0x0000_2026, len),
        )]);
        let si = parse(&bytes).standard_information().unwrap();
        assert_eq!(
            si.times,
            Times {
                created: TIMES[0],
                modified: TIMES[1],
                mft_changed: TIMES[2],
                accessed: TIMES[3],
            },
            "{len} bytes"
        );
        assert_eq!(si.flags, 0x0000_2026);
    }
}

#[test]
fn standard_information_shorter_than_48_bytes_is_not_decoded() {
    for len in [0usize, 36, 47] {
        let bytes = record_of(&[resident(
            0x10,
            "",
            0,
            0,
            &standard_information(TIMES, 6, 72)[..len],
        )]);
        let record = parse(&bytes);
        assert_eq!(record.attribute_count(), 1);
        assert!(record.standard_information().is_none(), "{len} bytes");
        assert!(!only(&bytes).is_malformed(), "the attribute itself is fine");
    }
    // Longer values (a future layout) still give the known fields.
    let mut long = standard_information(TIMES, 6, 72);
    long.extend([0xFF; 24]);
    let bytes = record_of(&[resident(0x10, "", 0, 0, &long)]);
    assert_eq!(parse(&bytes).standard_information().unwrap().flags, 6);
}

#[test]
fn standard_information_must_be_resident_and_of_type_0x10() {
    // The same value under another type code.
    let bytes = record_of(&[resident(
        0x40,
        "",
        0,
        0,
        &standard_information(TIMES, 6, 72),
    )]);
    assert!(only(&bytes).standard_information().is_none());
    assert!(parse(&bytes).standard_information().is_none());
    // A non-resident attribute of type 0x10 has no value to decode.
    let bytes = record_of(&[NonResident {
        type_code: 0x10,
        ..NonResident::default()
    }
    .build()]);
    assert!(only(&bytes).standard_information().is_none());
}

// ------------------------------------------------------------ $FILE_NAME

#[test]
fn file_name_fields_are_read_from_their_offsets() {
    let value = Name {
        parent: mref(0x0000_0A0B_0C0D_0E0F, 0x1234),
        times: TIMES,
        allocated: 0x0011_2233_4455_6677,
        real: 0x0001_0203_0405_0607,
        flags: 0x1000_0026,
        reparse: 0xA000_000C,
        namespace: 3,
        name: "Report 2024.docx".encode_utf16().collect(),
    }
    .value();
    assert_eq!(value.len(), 0x42 + 2 * 16);
    let bytes = record_of(&[resident(0x30, "", 0, 9, &value)]);
    let attr = only(&bytes);
    assert_eq!(attr.id, 9);
    let name = attr.file_name().unwrap();
    assert_eq!(name.parent, FileRef::new(0x0000_0A0B_0C0D_0E0F, 0x1234));
    assert_eq!(name.times.created, TIMES[0]);
    assert_eq!(name.times.modified, TIMES[1]);
    assert_eq!(name.times.mft_changed, TIMES[2]);
    assert_eq!(name.times.accessed, TIMES[3]);
    assert_eq!(name.allocated_size, 0x0011_2233_4455_6677);
    assert_eq!(name.real_size, 0x0001_0203_0405_0607);
    assert_eq!(name.flags, 0x1000_0026);
    assert_eq!(name.reparse_or_ea, 0xA000_000C);
    assert_eq!(name.namespace, Namespace::Win32AndDos);
    assert!(name.name == "Report 2024.docx");
    assert_eq!(name.name.len(), 16);
    assert!(name.is_directory());
    assert_eq!(FileName::parse(&value), Some(name));
}

#[test]
fn directory_is_the_0x10000000_flag_never_0x10() {
    assert_eq!(FILE_ATTR_DIRECTORY, 0x1000_0000);
    for (flags, directory) in [
        (0x1000_0000u32, true),
        (0x1000_0006, true),
        (0x10, false),
        (0x20, false),
        (0x2000_0000, false),
        (0, false),
    ] {
        let mut name = Name::new(mref(5, 5), "x", 0);
        name.flags = flags;
        assert_eq!(
            FileName::parse(&name.value()).unwrap().is_directory(),
            directory,
            "{flags:#x}"
        );
    }
}

#[test]
fn every_name_of_a_record_is_offered_in_on_disk_order() {
    // As Windows writes them: the DOS alias before the long name; a hard
    // link in another directory behind them.
    let bytes = record_of(&[
        resident(0x10, "", 0, 0, &standard_information(TIMES, 0x20, 72)),
        resident(
            0x30,
            "",
            0,
            3,
            &Name::new(mref(5, 5), "SYSTEM~1", 2).value(),
        ),
        resident(
            0x30,
            "",
            0,
            2,
            &Name::new(mref(5, 5), "System Volume Information", 1).value(),
        ),
        resident(0x30, "", 0, 4, &Name::new(mref(3081, 7), "link", 0).value()),
        resident(0x80, "", 0, 5, b""),
    ]);
    let record = parse(&bytes);
    let names: Vec<(String, Namespace, u64, u16)> = record
        .file_names()
        .map(|n| {
            (
                n.name.to_string_lossy(),
                n.namespace,
                n.parent.number(),
                n.parent.sequence(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            ("SYSTEM~1".to_string(), Namespace::Dos, 5, 5),
            (
                "System Volume Information".to_string(),
                Namespace::Win32,
                5,
                5
            ),
            ("link".to_string(), Namespace::Posix, 3081, 7),
        ]
    );
    // With their attribute ids, through the attribute iterator.
    let ids: Vec<u16> = record
        .attributes()
        .filter(|a| a.file_name().is_some())
        .map(|a| a.id)
        .collect();
    assert_eq!(ids, [3, 2, 4]);
}

#[test]
fn namespaces_0_to_3() {
    for (code, namespace) in [
        (0u8, Namespace::Posix),
        (1, Namespace::Win32),
        (2, Namespace::Dos),
        (3, Namespace::Win32AndDos),
    ] {
        let value = Name::new(mref(5, 5), "n", code).value();
        assert_eq!(FileName::parse(&value).unwrap().namespace, namespace);
        assert_eq!(namespace.as_u8(), code);
        assert_eq!(Namespace::from_u8(code), Some(namespace));
    }
    for code in [4u8, 5, 0x80, 0xFF] {
        assert_eq!(Namespace::from_u8(code), None);
        let value = Name::new(mref(5, 5), "n", code).value();
        assert_eq!(FileName::parse(&value), None, "namespace {code}");
    }
}

#[test]
fn name_lengths_from_1_to_255_units() {
    assert_eq!(FILE_NAME_MIN_LEN, 0x44);
    let longest: String = "n".repeat(255);
    let value = Name::new(mref(5, 5), &longest, 1).value();
    assert_eq!(value.len(), 0x42 + 510);
    assert_eq!(FileName::parse(&value).unwrap().name.len(), 255);
    let bytes = record_of(&[resident(0x30, "", 0, 0, &value)]);
    assert!(only(&bytes).file_name().unwrap().name == longest.as_str());

    // No name at all is no $FILE_NAME.
    let mut empty = Name::new(mref(5, 5), "x", 1).value();
    empty[0x40] = 0;
    assert_eq!(FileName::parse(&empty), None);
    // A name length that reaches past the value.
    let mut overrun = Name::new(mref(5, 5), "abc", 1).value();
    overrun[0x40] = 4;
    assert_eq!(FileName::parse(&overrun), None);
    // Too short for the fixed part.
    let value = Name::new(mref(5, 5), "abc", 1).value();
    for len in [0, 1, 0x41, 0x42, 0x43] {
        assert_eq!(FileName::parse(&value[..len]), None, "{len} bytes");
    }
    assert!(FileName::parse(&value[..0x48]).is_some());
}

#[test]
fn bytes_behind_the_name_are_tolerated_in_an_attribute_but_not_in_an_index_key() {
    let value = Name::new(mref(5, 5), "abc", 1).value();
    let exact = FileName::parse_exact(&value).unwrap();
    assert!(exact.name == "abc");
    let mut padded = value.clone();
    padded.extend([0u8; 6]);
    assert_eq!(FileName::parse(&padded), Some(exact));
    assert_eq!(FileName::parse_exact(&padded), None);
    assert_eq!(FileName::parse_exact(&padded[..0x49]), None);
    assert_eq!(FileName::parse_exact(&padded[..0x48]), Some(exact));
    assert_eq!(FileName::parse_exact(&padded[..0x47]), None);
}

#[test]
fn names_are_code_units_not_text() {
    // An unpaired surrogate, a NUL and a character outside the BMP.
    let odd = [0x0041u16, 0xDC00, 0x0000, 0xD83D, 0xDE00];
    let mut name = Name::new(mref(5, 5), "", 0);
    name.name = odd.to_vec();
    let value = name.value();
    let parsed = FileName::parse(&value).unwrap();
    assert_eq!(parsed.name.units().collect::<Vec<u16>>(), odd);
    assert_eq!(parsed.name.as_bytes(), &units_le(&odd)[..]);
    assert_eq!(parsed.name.to_string_lossy(), "A\u{FFFD}\u{0}\u{1F600}");
}

#[test]
fn file_name_must_be_resident_and_of_type_0x30() {
    let value = Name::new(mref(5, 5), "abc", 1).value();
    assert!(only(&record_of(&[resident(0x80, "", 0, 0, &value)]))
        .file_name()
        .is_none());
    assert!(only(&record_of(&[NonResident {
        type_code: 0x30,
        ..NonResident::default()
    }
    .build()]))
    .file_name()
    .is_none());
    // A damaged name is skipped by the convenience iterator, not the walk.
    let mut bad = value.clone();
    bad[0x41] = 9;
    let bytes = record_of(&[
        resident(0x30, "", 0, 0, &bad),
        resident(0x30, "", 0, 1, &value),
    ]);
    let record = parse(&bytes);
    assert_eq!(record.attribute_count(), 2);
    assert_eq!(record.file_names().count(), 1);
}

// ----------------------------------------------------------------- $DATA

#[test]
fn resident_data_is_a_location_inside_the_record() {
    let content = b"The quick brown fox";
    let bytes = RecordBuilder::new(1024)
        .attr(&resident(
            0x10,
            "",
            0,
            0,
            &standard_information(TIMES, 0x20, 72),
        ))
        .attr(&resident(0x80, "", 0, 4, content))
        .finish();
    let record = parse(&bytes);
    let data = record.find(attr_type::DATA, "").unwrap();
    assert!(data.is_unnamed());
    assert_eq!(data.id, 4);
    let value = data.resident().unwrap();
    // 0x38 + 0x60 is where the attribute starts, 0x18 its header.
    assert_eq!(value.offset, 0x38 + 0x60 + 0x18);
    assert_eq!(value.len(), 19);
    assert!(!value.is_empty());
    assert_eq!(value.bytes, content);
    // Offset and length are enough to get the content from the record.
    let start = value.offset as usize;
    assert_eq!(
        &record.bytes()[start..start + value.len() as usize],
        content
    );
    assert!(data.extent().is_none());
}

#[test]
fn an_empty_resident_data_attribute_is_an_empty_file() {
    // What Windows writes for a new file: 80 00 00 00 18 00 00 00 ...
    let bytes = record_of(&[resident(0x80, "", 0, 1, b"")]);
    let data = only(&bytes);
    assert_eq!(data.length, 0x18);
    let value = data.resident().unwrap();
    assert!(value.is_empty());
    assert_eq!(value.len(), 0);
    assert_eq!(value.offset, 0x48 + 0x18);
}

#[test]
fn resident_data_is_never_compressed_whatever_the_flags_say() {
    // A file created in a compressed folder: resident, flag 0x0001, raw
    // bytes ("tiny" in the reference sheet).
    let bytes = record_of(&[resident(0x80, "", 0x0001, 2, b"tiny")]);
    let data = only(&bytes);
    assert_eq!(data.flags, 0x0001);
    assert_ne!(data.flags & ATTR_FLAG_COMPRESSION_MASK, 0);
    assert!(!data.is_compressed());
    assert_eq!(data.resident().unwrap().bytes, b"tiny");
    // The same flag on a non-resident attribute means compressed.
    let bytes = record_of(&[NonResident {
        flags: 0x0001,
        compression_unit: 4,
        total_allocated: Some(4096),
        ..NonResident::default()
    }
    .build()]);
    assert!(only(&bytes).is_compressed());
    // Any bit of the mask counts.
    let bytes = record_of(&[NonResident {
        flags: 0x0080,
        ..NonResident::default()
    }
    .build()]);
    assert!(only(&bytes).is_compressed());
}

#[test]
fn the_compression_unit_does_not_decide_and_sparse_and_encrypted_are_flags() {
    // Sparse, uncompressed, as ntfs-3g writes it: flag 0x8000, unit 4,
    // 0x48-byte header.
    let bytes = record_of(&[NonResident {
        flags: 0x8000,
        compression_unit: 4,
        last_vcn: 122,
        allocated: 503_808,
        real: 500_000,
        initialized: 5000,
        total_allocated: Some(8192),
        runlist: &[0x21, 0x02, 0xF6, 0x09, 0x01, 0x79, 0x00],
        ..NonResident::default()
    }
    .build()]);
    let data = only(&bytes);
    assert!(!data.is_compressed(), "unit 4 alone is not compression");
    assert!(data.is_sparse());
    assert!(!data.is_encrypted());
    assert_eq!((ATTR_FLAG_SPARSE, ATTR_FLAG_ENCRYPTED), (0x8000, 0x4000));
    let extent = data.extent().unwrap();
    assert_eq!(extent.compression_unit, 4);
    assert_eq!(extent.total_allocated, Some(8192));
    assert_eq!(
        extent.sizes(),
        Some(StreamSizes {
            allocated: 503_808,
            real: 500_000,
            initialized: 5000
        })
    );
    assert_eq!(
        extent.runs().collect::<Vec<Run>>(),
        [
            Run {
                lcn: Some(2550),
                length: 2
            },
            Run {
                lcn: None,
                length: 121
            }
        ]
    );

    let bytes = record_of(&[NonResident {
        flags: 0x4000,
        ..NonResident::default()
    }
    .build()]);
    let data = only(&bytes);
    assert!(data.is_encrypted() && !data.is_sparse() && !data.is_compressed());
}

#[test]
fn a_plain_non_resident_data_attribute() {
    let bytes = record_of(&[NonResident {
        id: 6,
        last_vcn: 47,
        allocated: 49_152,
        real: 39_936,
        initialized: 39_000,
        runlist: &[0x21, 0x10, 0xD5, 0x07, 0x21, 0x20, 0xC2, 0x08, 0x00],
        ..NonResident::default()
    }
    .build()]);
    let data = only(&bytes);
    assert_eq!(data.type_code, attr_type::DATA);
    assert_eq!(data.id, 6);
    assert!(data.resident().is_none());
    let extent = data.extent().unwrap();
    assert_eq!(extent.start_vcn, 0);
    assert_eq!(extent.last_vcn, 47);
    assert_eq!(extent.compression_unit, 0);
    assert_eq!(
        (
            extent.allocated_size,
            extent.real_size,
            extent.initialized_size
        ),
        (49_152, 39_936, 39_000)
    );
    assert_eq!(extent.total_allocated, None, "0x40-byte header");
    // The $MFT of the XP sample: 2005+16, 4247+32.
    let mut runs = Vec::new();
    let decoded = extent.decode_runs(&mut runs);
    assert_eq!(decoded.status, RunlistStatus::Complete);
    assert_eq!(decoded.clusters, 48);
    assert_eq!(
        runs,
        [
            Run {
                lcn: Some(2005),
                length: 16
            },
            Run {
                lcn: Some(4247),
                length: 32
            }
        ]
    );
}

#[test]
fn name_and_runlist_are_where_their_offset_fields_say() {
    // Named stream with the 0x40-byte header: name at 0x40, runs at 0x58
    // ("bigstream" in the reference sheet).
    let named = NonResident {
        name: "bigstream",
        runlist: &[0x11, 0x03, 0x64, 0x00],
        ..NonResident::default()
    }
    .build();
    assert_eq!(
        (
            u16::from_le_bytes([named[0x0A], named[0x0B]]),
            u16::from_le_bytes([named[0x20], named[0x21]])
        ),
        (0x40, 0x58)
    );
    let bytes = record_of(&[named]);
    let data = only(&bytes);
    assert!(data.is_named("bigstream"));
    let extent = data.extent().unwrap();
    assert_eq!(extent.total_allocated, None);
    assert_eq!(
        extent.runs().collect::<Vec<Run>>(),
        [Run {
            lcn: Some(100),
            length: 3
        }]
    );

    // $I30 index allocation: name at 0x40, runs at 0x48 - a 0x40-byte
    // header although the runlist starts at 0x48.
    let i30 = NonResident {
        type_code: 0xA0,
        name: "$I30",
        runlist: &[0x11, 0x01, 0x2C, 0x00],
        ..NonResident::default()
    }
    .build();
    assert_eq!(
        (
            u16::from_le_bytes([i30[0x0A], i30[0x0B]]),
            u16::from_le_bytes([i30[0x20], i30[0x21]])
        ),
        (0x40, 0x48)
    );
    let bytes = record_of(&[i30]);
    let attr = only(&bytes);
    assert!(attr.is_named(I30));
    assert_eq!(
        attr.extent().unwrap().total_allocated,
        None,
        "the name sits where the extra field would be"
    );
    assert_eq!(
        attr.extent().unwrap().runs().collect::<Vec<Run>>(),
        [Run {
            lcn: Some(44),
            length: 1
        }]
    );

    // Sparse named stream: 0x48-byte header, name at 0x48.
    let sparse = NonResident {
        name: "s",
        flags: 0x8000,
        total_allocated: Some(0x3000),
        runlist: &[0x01, 0x10, 0x00],
        ..NonResident::default()
    }
    .build();
    assert_eq!(u16::from_le_bytes([sparse[0x0A], sparse[0x0B]]), 0x48);
    let bytes = record_of(&[sparse]);
    let attr = only(&bytes);
    assert!(attr.is_named("s"));
    assert_eq!(attr.extent().unwrap().total_allocated, Some(0x3000));

    // A runlist moved further back than the builder would put it: still
    // found through its offset.
    let mut moved = NonResident {
        runlist: &[0u8; 24],
        ..NonResident::default()
    }
    .build();
    moved[0x50..0x54].copy_from_slice(&[0x11, 0x07, 0x09, 0x00]);
    put16(&mut moved, 0x20, 0x50);
    let bytes = record_of(&[moved]);
    let extent = only(&bytes).extent().unwrap();
    assert_eq!(extent.runlist.len(), 8);
    assert_eq!(
        extent.runs().collect::<Vec<Run>>(),
        [Run {
            lcn: Some(9),
            length: 7
        }]
    );
}

#[test]
fn the_name_offset_of_an_unnamed_attribute_means_nothing() {
    // Windows writes 0 there in about half of the unnamed attributes, and
    // 0x18 / 0x40 in the others. Whatever is there must not matter.
    for name_offset in [0u16, 0x18, 0x40, 0x07, 0x1000, 0xFFFF] {
        let mut resident_attr = resident(0x80, "", 0, 1, b"data");
        put16(&mut resident_attr, 0x0A, name_offset);
        let mut non_resident = NonResident {
            runlist: &[0x11, 0x01, 0x05, 0x00],
            ..NonResident::default()
        }
        .build();
        put16(&mut non_resident, 0x0A, name_offset);
        let bytes = record_of(&[resident_attr, non_resident]);
        let attributes: Vec<_> = parse(&bytes).attributes().collect();
        assert_eq!(attributes.len(), 2);
        assert!(
            attributes[0].is_unnamed() && !attributes[0].is_malformed(),
            "offset {name_offset:#x}"
        );
        assert_eq!(attributes[0].resident().unwrap().bytes, b"data");
        assert!(
            attributes[1].is_unnamed() && !attributes[1].is_malformed(),
            "offset {name_offset:#x}"
        );
        let extent = attributes[1].extent().unwrap();
        assert_eq!(
            extent.runs().collect::<Vec<Run>>(),
            [Run {
                lcn: Some(5),
                length: 1
            }]
        );
        // Without a name, only the runlist offset tells the header form.
        assert_eq!(extent.total_allocated, None);
    }
}

#[test]
fn sizes_are_offered_only_by_the_extent_with_start_vcn_0() {
    let first = NonResident {
        last_vcn: 467,
        allocated: 4_816_896,
        real: 4_816_000,
        initialized: 4_816_000,
        ..NonResident::default()
    }
    .build();
    // A later extent: ntfs-3g leaves the size fields 0, but nothing says
    // they could not hold garbage.
    let later = NonResident {
        start_vcn: 468,
        last_vcn: 900,
        allocated: 0xDEAD,
        real: 0xBEEF,
        initialized: 0xF00D,
        ..NonResident::default()
    }
    .build();
    let bytes = record_of(&[first, later]);
    let extents: Vec<_> = parse(&bytes)
        .attributes()
        .map(|a| a.extent().unwrap())
        .collect();
    assert_eq!(
        extents[0].sizes(),
        Some(StreamSizes {
            allocated: 4_816_896,
            real: 4_816_000,
            initialized: 4_816_000
        })
    );
    assert_eq!(extents[1].start_vcn, 468);
    assert_eq!(extents[1].last_vcn, 900);
    assert_eq!(extents[1].sizes(), None);
    // The raw fields are still there for whoever wants to look.
    assert_eq!(
        (
            extents[1].allocated_size,
            extents[1].real_size,
            extents[1].initialized_size
        ),
        (0xDEAD, 0xBEEF, 0xF00D)
    );
}

#[test]
fn stored_last_vcn_values_that_are_not_a_count() {
    // An empty non-resident attribute: lowest 0, highest -1, no runs
    // ($Repair's unnamed $DATA on Windows 10).
    let bytes = record_of(&[NonResident {
        last_vcn: -1,
        runlist: &[0x00],
        ..NonResident::default()
    }
    .build()]);
    let extent = only(&bytes).extent().unwrap();
    assert_eq!(extent.last_vcn, -1);
    assert_eq!(
        extent.sizes(),
        Some(StreamSizes {
            allocated: 0,
            real: 0,
            initialized: 0
        })
    );
    let mut runs = extent.runs();
    assert_eq!(runs.next(), None);
    assert_eq!(runs.status(), Some(RunlistStatus::Complete));
    // "Can be 0 for single extent attributes": the runs tell the length.
    let bytes = record_of(&[NonResident {
        last_vcn: 0,
        runlist: &[0x11, 0x08, 0x10, 0x00],
        ..NonResident::default()
    }
    .build()]);
    let extent = only(&bytes).extent().unwrap();
    assert_eq!(extent.last_vcn, 0);
    assert_eq!(extent.runs().map(|r| r.length).sum::<u64>(), 8);
    // Values are handed out as stored, however odd.
    let bytes = record_of(&[NonResident {
        start_vcn: u64::MAX,
        last_vcn: i64::MIN,
        allocated: u64::MAX,
        real: u64::MAX,
        initialized: u64::MAX,
        ..NonResident::default()
    }
    .build()]);
    let extent = only(&bytes).extent().unwrap();
    assert_eq!((extent.start_vcn, extent.last_vcn), (u64::MAX, i64::MIN));
    assert_eq!(extent.sizes(), None);
    assert_eq!(extent.real_size, u64::MAX);
}

#[test]
fn a_faulty_runlist_does_not_make_the_attribute_malformed() {
    let bytes = record_of(&[NonResident {
        runlist: &[0x11, 0x04, 0x20, 0x19, 0x01],
        ..NonResident::default()
    }
    .build()]);
    let attr = only(&bytes);
    assert!(!attr.is_malformed());
    let mut runs = attr.extent().unwrap().runs();
    assert_eq!(
        runs.next(),
        Some(Run {
            lcn: Some(0x20),
            length: 4
        })
    );
    assert_eq!(runs.next(), None);
    assert_eq!(
        runs.status(),
        Some(RunlistStatus::Invalid(RunlistError::FieldTooLarge))
    );
}

#[test]
fn named_streams_are_told_apart_from_the_content() {
    let bytes = record_of(&[
        resident(0x80, "", 0, 1, b"content"),
        resident(0x80, "Zone.Identifier", 0, 2, b"[ZoneTransfer]"),
        NonResident {
            name: "WofCompressedData",
            id: 3,
            runlist: &[0x11, 0x02, 0x30, 0x00],
            ..NonResident::default()
        }
        .build(),
    ]);
    let record = parse(&bytes);
    let streams: Vec<(String, bool)> = record
        .attributes()
        .filter(|a| a.type_code == attr_type::DATA)
        .map(|a| (a.name.to_string_lossy(), a.is_unnamed()))
        .collect();
    assert_eq!(
        streams,
        [
            (String::new(), true),
            ("Zone.Identifier".to_string(), false),
            ("WofCompressedData".to_string(), false),
        ]
    );
    // A 15-character name puts the resident value at 0x38.
    let zone = record.find(attr_type::DATA, "Zone.Identifier").unwrap();
    assert_eq!(zone.resident().unwrap().offset, zone.offset + 0x38);
    assert_eq!(zone.resident().unwrap().bytes, b"[ZoneTransfer]");
    assert!(record
        .find(attr_type::DATA, hds_ntfs::WOF_COMPRESSED_DATA)
        .unwrap()
        .extent()
        .is_some());
    assert!(
        record.find(attr_type::DATA, "wofcompresseddata").is_none(),
        "names are compared exactly"
    );
    assert!(record.find(attr_type::DATA, "missing").is_none());
    assert_eq!(
        record
            .find(attr_type::DATA, "")
            .unwrap()
            .resident()
            .unwrap()
            .bytes,
        b"content"
    );
}

// ------------------------------------------------------- $ATTRIBUTE_LIST

#[test]
fn a_resident_attribute_list_is_decoded_entry_by_entry() {
    let mut list = Vec::new();
    list.extend(attr_list_entry(0x10, "", 0, mref(2553, 1), 0));
    list.extend(attr_list_entry(0x30, "", 0, mref(2554, 1), 2));
    list.extend(attr_list_entry(0x80, "", 0, mref(2555, 3), 0));
    list.extend(attr_list_entry(0x80, "", 468, mref(2556, 1), 0));
    list.extend(attr_list_entry(
        0xA0,
        "$I30",
        134,
        mref(0x0000_FFFF_FFFF_FFFF, 0xFFFF),
        0xABCD,
    ));
    assert_eq!(list.len(), 4 * 0x20 + 0x28);
    let bytes = record_of(&[resident(0x20, "", 0, 1, &list)]);
    let Some(AttributeList::Resident(mut entries)) = only(&bytes).attribute_list() else {
        panic!("a resident list expected");
    };
    assert_eq!(entries.end(), None);
    let all: Vec<_> = entries.by_ref().collect();
    assert_eq!(entries.end(), Some(WalkEnd::Complete));
    let seen: Vec<(u32, String, u64, u64, u16, u16)> = all
        .iter()
        .map(|e| {
            (
                e.type_code,
                e.name.to_string_lossy(),
                e.start_vcn,
                e.holder.number(),
                e.holder.sequence(),
                e.attr_id,
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (0x10, String::new(), 0, 2553, 1, 0),
            (0x30, String::new(), 0, 2554, 1, 2),
            (0x80, String::new(), 0, 2555, 3, 0),
            (0x80, String::new(), 468, 2556, 1, 0),
            (
                0xA0,
                "$I30".to_string(),
                134,
                0x0000_FFFF_FFFF_FFFF,
                0xFFFF,
                0xABCD
            ),
        ]
    );
}

#[test]
fn a_non_resident_attribute_list_gives_its_extent() {
    let bytes = record_of(&[NonResident {
        type_code: 0x20,
        last_vcn: 0,
        allocated: 4096,
        real: 288,
        initialized: 288,
        runlist: &[0x21, 0x01, 0x33, 0x12, 0x00],
        ..NonResident::default()
    }
    .build()]);
    let Some(AttributeList::NonResident(extent)) = only(&bytes).attribute_list() else {
        panic!("a non-resident list expected");
    };
    assert_eq!(extent.sizes().unwrap().real, 288);
    assert_eq!(
        extent.runs().collect::<Vec<Run>>(),
        [Run {
            lcn: Some(0x1233),
            length: 1
        }]
    );
    // Other types are not attribute lists.
    assert!(only(&record_of(&[resident(0x80, "", 0, 0, b"x")]))
        .attribute_list()
        .is_none());
}

#[test]
fn an_attribute_list_walk_is_bounded_and_reports_how_it_ended() {
    let good = attr_list_entry(0x80, "", 0, mref(100, 1), 0);
    let walk = |bytes: &[u8]| {
        let mut entries = AttrListEntries::new(bytes);
        let count = entries.by_ref().count();
        (count, entries.end().unwrap())
    };
    assert_eq!(walk(&[]), (0, WalkEnd::Complete));
    assert_eq!(walk(&good), (1, WalkEnd::Complete));
    let two = [good.clone(), good.clone()].concat();
    assert_eq!(walk(&two), (2, WalkEnd::Complete));
    // Bytes that are no entry behind the last one.
    let mut trailing = two.clone();
    trailing.extend([0u8; 8]);
    assert_eq!(walk(&trailing), (2, WalkEnd::Broken));
    // ... however few: a list ends exactly where its last entry ends.
    for spare in 1..8 {
        assert_eq!(
            walk(&trailing[..two.len() + spare]),
            (2, WalkEnd::Broken),
            "{spare} spare byte(s)"
        );
    }
    // An entry length of 0 must not loop.
    let mut zero = two.clone();
    put16(&mut zero, 0x20 + 4, 0);
    assert_eq!(walk(&zero), (1, WalkEnd::Broken));
    // Shorter than the fixed part.
    let mut short = two.clone();
    put16(&mut short, 0x20 + 4, 0x19);
    assert_eq!(walk(&short), (1, WalkEnd::Broken));
    // The smallest legal length: 0x1A.
    let mut minimal = good.clone();
    minimal.truncate(0x1A);
    put16(&mut minimal, 4, 0x1A);
    assert_eq!(walk(&minimal), (1, WalkEnd::Complete));
    // Reaching past the value.
    let mut overrun = two.clone();
    put16(&mut overrun, 0x20 + 4, 0x28);
    assert_eq!(walk(&overrun), (1, WalkEnd::Broken));
    assert_eq!(walk(&two[..0x3F]), (1, WalkEnd::Broken));
    // A name that does not fit in its entry, or overlaps the fixed part.
    let named = attr_list_entry(0xA0, "$I30", 0, mref(100, 1), 0);
    assert_eq!(walk(&named), (1, WalkEnd::Complete));
    let mut long_name = named.clone();
    long_name[0x06] = 8;
    assert_eq!(walk(&long_name), (0, WalkEnd::Broken));
    let mut early_name = named.clone();
    early_name[0x07] = 0x10;
    assert_eq!(walk(&early_name), (0, WalkEnd::Broken));
    // The name offset of an unnamed entry is not looked at.
    let mut unnamed = good.clone();
    unnamed[0x07] = 0xFF;
    assert_eq!(walk(&unnamed), (1, WalkEnd::Complete));
    // Fused.
    let mut entries = AttrListEntries::new(&zero);
    assert_eq!(entries.by_ref().count(), 1);
    assert!(entries.next().is_none() && entries.next().is_none());
}

// ---------------------------------------------- directories: $INDEX_ROOT

fn key(parent: u64, text: &str, namespace: u8, flags: u32) -> Vec<u8> {
    let mut name = Name::new(parent, text, namespace);
    name.flags = flags;
    name.value()
}

#[test]
fn index_root_entries_come_with_record_relative_offsets() {
    let dir = mref(64, 3);
    let entries = vec![
        index_entry(mref(70, 1), &key(dir, "alpha.txt", 1, 0x20), 0, None),
        index_entry(mref(71, 2), &key(dir, "ALPHA~1.TXT", 2, 0x20), 0, None),
        index_entry(mref(72, 1), &key(dir, "sub", 3, 0x1000_0000), 0, None),
        last_entry(None),
    ];
    let value = index_root(4096, 1, 0, &entries);
    let bytes = RecordBuilder::new(1024)
        .flags(3)
        .attr(&resident(
            0x10,
            "",
            0,
            0,
            &standard_information(TIMES, 0, 72),
        ))
        .attr(&resident(0x90, "$I30", 0, 1, &value))
        .finish();
    let record = parse(&bytes);
    let attr = record.find(attr_type::INDEX_ROOT, I30).unwrap();
    // A resident attribute named $I30 keeps its value at 0x20.
    let value_at = attr.resident().unwrap().offset;
    assert_eq!(value_at, attr.offset + 0x20);
    let root = attr.index_root().unwrap();
    assert_eq!(root.indexed_type, 0x30);
    assert_eq!(root.collation, 1);
    assert_eq!(root.index_block_size, 4096);
    assert_eq!(root.blocks_per_index_block, 1);
    assert_eq!(root.node.entries_offset, 0x10);
    assert_eq!(root.node.used_size, value.len() as u32 - 0x10);
    assert_eq!(root.node.allocated_size, value.len() as u32 - 0x10);
    assert!(!root.node.has_subnodes());

    let mut walker = root.entries();
    let found: Vec<_> = walker.by_ref().collect();
    assert_eq!(walker.end(), Some(WalkEnd::Complete));
    assert_eq!(walker.skipped(), 0);
    assert_eq!(found.len(), 3);
    // Entries start 0x20 into the value, one behind the other.
    let mut expected_offset = value_at + 0x20;
    for (entry, built) in found.iter().zip(&entries) {
        assert_eq!(entry.offset, expected_offset);
        assert_eq!(
            &record.bytes()[entry.offset as usize..][..built.len()],
            &built[..]
        );
        expected_offset += built.len() as u32;
        assert_eq!(entry.file_name.parent, FileRef::new(64, 3));
        assert_eq!(entry.subnode_vcn, None);
        assert_eq!(entry.flags, 0);
    }
    assert_eq!(
        found
            .iter()
            .map(|e| (
                e.child.number(),
                e.child.sequence(),
                e.file_name.name.to_string_lossy(),
                e.file_name.namespace,
                e.file_name.is_directory()
            ))
            .collect::<Vec<_>>(),
        [
            (70, 1, "alpha.txt".to_string(), Namespace::Win32, false),
            (71, 2, "ALPHA~1.TXT".to_string(), Namespace::Dos, false),
            (72, 1, "sub".to_string(), Namespace::Win32AndDos, true),
        ]
    );
    assert_eq!(root.entries().owner(), Some(FileRef::new(64, 3)));
}

#[test]
fn an_index_root_of_a_large_directory_points_at_sub_nodes() {
    let dir = mref(5, 5);
    let entries = vec![
        index_entry(mref(90, 1), &key(dir, "m-separator", 0, 0x20), 0, Some(0)),
        last_entry(Some(1)),
    ];
    let value = index_root(4096, 8, 1, &entries);
    let bytes = record_of(&[resident(0x90, "$I30", 0, 0, &value)]);
    let root = only(&bytes).index_root().unwrap();
    assert!(root.node.has_subnodes());
    assert_eq!(root.blocks_per_index_block, 8);
    let found: Vec<_> = root.entries().collect();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].subnode_vcn, Some(0));
    assert_eq!(found[0].flags, 1);
    assert!(found[0].file_name.name == "m-separator");
}

#[test]
fn an_empty_index_root_is_complete_and_has_no_owner() {
    // A deleted directory's root holds just the end entry.
    let value = index_root(4096, 1, 0, &[last_entry(None)]);
    let bytes = record_of(&[resident(0x90, "$I30", 0, 0, &value)]);
    let root = only(&bytes).index_root().unwrap();
    let mut walker = root.entries();
    assert_eq!(walker.next(), None);
    assert_eq!(walker.end(), Some(WalkEnd::Complete));
    assert_eq!(root.entries().owner(), None);
}

#[test]
fn index_roots_with_other_keys_yield_no_entries() {
    // $Secure:$SII: 4-byte keys, the first 8 bytes of an entry are a data
    // offset and length, not a file reference.
    let entries = vec![
        index_entry(0x0014_0018, &[0x00, 0x01, 0x00, 0x00], 0, None),
        index_entry(0x0014_0018, &[0x01, 0x01, 0x00, 0x00], 0, None),
        last_entry(None),
    ];
    let mut value = index_root(4096, 1, 0, &entries);
    put32(&mut value, 0, 0); // no indexed attribute type
    put32(&mut value, 4, 0x10); // collation: ULONG
    let bytes = record_of(&[resident(0x90, "$SII", 0, 0, &value)]);
    let attr = only(&bytes);
    assert!(attr.is_named("$SII") && !attr.is_named(I30));
    let root = attr.index_root().unwrap();
    assert_eq!((root.indexed_type, root.collation), (0, 0x10));
    let mut walker = root.entries();
    assert_eq!(walker.next(), None);
    assert_eq!(walker.skipped(), 2);
    assert_eq!(walker.end(), Some(WalkEnd::Complete));
}

#[test]
fn an_index_root_whose_node_header_lies_is_not_decoded() {
    let good = index_root(
        4096,
        1,
        0,
        &[
            index_entry(mref(70, 1), &key(mref(5, 5), "a", 0, 0), 0, None),
            last_entry(None),
        ],
    );
    let decoded = |value: &[u8]| {
        only(&record_of(&[resident(0x90, "$I30", 0, 0, value)]))
            .index_root()
            .is_some()
    };
    assert!(decoded(&good));
    // Entries inside the node header.
    let mut bad = good.clone();
    put32(&mut bad, 0x10, 0x08);
    assert!(!decoded(&bad));
    // Entries behind the used size.
    let mut bad = good.clone();
    put32(&mut bad, 0x10, 0x1000);
    assert!(!decoded(&bad));
    // Used size beyond the value.
    let mut bad = good.clone();
    put32(&mut bad, 0x14, good.len() as u32 - 0x10 + 8);
    assert!(!decoded(&bad));
    let mut bad = good.clone();
    put32(&mut bad, 0x14, 0xFFFF_FFF0);
    assert!(!decoded(&bad));
    // Shorter than its headers.
    assert!(!decoded(&good[..0x1F]));
    assert!(!decoded(&[]));
    // A used size that cuts the entries short is decoded; the walk tells.
    let mut cut = good.clone();
    put32(&mut cut, 0x14, 0x10 + 0x10);
    let bytes = record_of(&[resident(0x90, "$I30", 0, 0, &cut)]);
    let root = only(&bytes).index_root().unwrap();
    let mut walker = root.entries();
    assert_eq!(walker.next(), None);
    assert_eq!(walker.end(), Some(WalkEnd::Broken));
    // Not an index root at all.
    assert!(only(&record_of(&[resident(0x80, "$I30", 0, 0, &good)]))
        .index_root()
        .is_none());
}

// ------------------------- $INDEX_ALLOCATION, $BITMAP, $VOLUME_NAME, 0xC0

#[test]
fn a_directory_with_index_blocks() {
    let bytes = RecordBuilder::new(1024)
        .flags(3)
        .attr(&resident(
            0x90,
            "$I30",
            0,
            6,
            &index_root(4096, 1, 1, &[last_entry(Some(0))]),
        ))
        .attr(
            &NonResident {
                type_code: 0xA0,
                name: "$I30",
                id: 8,
                last_vcn: 3,
                allocated: 16_384,
                real: 16_384,
                initialized: 16_384,
                runlist: &[0x21, 0x04, 0x0D, 0x10, 0x00],
                ..NonResident::default()
            }
            .build(),
        )
        .attr(&resident(0xB0, "$I30", 0, 7, &[0x0F, 0, 0, 0, 0, 0, 0, 0]))
        .finish();
    let record = parse(&bytes);
    assert!(record.is_directory());
    let allocation = record.find(attr_type::INDEX_ALLOCATION, I30).unwrap();
    let extent = allocation.extent().unwrap();
    assert_eq!((extent.start_vcn, extent.last_vcn), (0, 3));
    assert_eq!(extent.sizes().unwrap().allocated, 16_384);
    assert_eq!(
        extent.runs().collect::<Vec<Run>>(),
        [Run {
            lcn: Some(0x100D),
            length: 4
        }]
    );
    let bitmap = record.find(attr_type::BITMAP, I30).unwrap();
    assert_eq!(
        bitmap.resident().unwrap().bytes,
        &[0x0F, 0, 0, 0, 0, 0, 0, 0]
    );
    // The unnamed $BITMAP (of $MFT) is another attribute.
    assert!(record.find(attr_type::BITMAP, "").is_none());
}

#[test]
fn the_mft_bitmap_resident_or_not() {
    let bytes = record_of(&[
        NonResident {
            last_vcn: 15,
            runlist: &[0x11, 0x10, 0x04, 0x00],
            ..NonResident::default()
        }
        .build(),
        NonResident {
            type_code: 0xB0,
            id: 5,
            last_vcn: 0,
            allocated: 4096,
            real: 8,
            initialized: 8,
            runlist: &[0x11, 0x01, 0x03, 0x00],
            ..NonResident::default()
        }
        .build(),
    ]);
    let record = parse(&bytes);
    let bitmap = record.find(attr_type::BITMAP, "").unwrap();
    match bitmap.body {
        AttrBody::NonResident(extent) => {
            assert_eq!(extent.sizes().unwrap().real, 8);
            assert_eq!(
                extent.runs().collect::<Vec<Run>>(),
                [Run {
                    lcn: Some(3),
                    length: 1
                }]
            );
        }
        other => panic!("non-resident expected, got {other:?}"),
    }
    let bytes = record_of(&[resident(0xB0, "", 0, 5, &[0xFF, 0xFF, 0x03])]);
    assert_eq!(
        parse(&bytes)
            .find(attr_type::BITMAP, "")
            .unwrap()
            .resident()
            .unwrap()
            .bytes,
        &[0xFF, 0xFF, 0x03]
    );
}

#[test]
fn the_volume_name() {
    let label: Vec<u8> = units_le(&"NTFS_DEL".encode_utf16().collect::<Vec<u16>>());
    let bytes = record_of(&[resident(0x60, "", 0, 4, &label)]);
    let attr = only(&bytes);
    assert!(attr.volume_name().unwrap() == "NTFS_DEL");
    assert_eq!(attr.volume_name().unwrap().len(), 8);
    // No label.
    let bytes = record_of(&[resident(0x60, "", 0, 4, &[])]);
    assert!(only(&bytes).volume_name().unwrap().is_empty());
    // Other attributes have none.
    let bytes = record_of(&[resident(0x70, "", 0, 4, &label)]);
    assert!(only(&bytes).volume_name().is_none());
}

#[test]
fn the_reparse_tag_is_the_first_four_bytes_of_the_value() {
    // A symbolic link as Windows 10 writes it: 0C 00 00 A0 80 00 00 00 ...
    let mut value = vec![0x0C, 0x00, 0x00, 0xA0, 0x80, 0x00, 0x00, 0x00];
    value.extend([0x55; 0x80]);
    let bytes = record_of(&[resident(0xC0, "", 0, 3, &value)]);
    assert_eq!(only(&bytes).reparse_tag(), Some(REPARSE_TAG_SYMLINK));
    for tag in [
        REPARSE_TAG_MOUNT_POINT,
        REPARSE_TAG_DEDUP,
        REPARSE_TAG_WOF,
        0x9000_001A,
        0x9000_701A,
        0,
    ] {
        let mut value = vec![0u8; 16];
        put32(&mut value, 0, tag);
        let bytes = record_of(&[resident(0xC0, "", 0, 3, &value)]);
        assert_eq!(only(&bytes).reparse_tag(), Some(tag));
    }
    assert_eq!(
        (
            REPARSE_TAG_MOUNT_POINT,
            REPARSE_TAG_SYMLINK,
            REPARSE_TAG_DEDUP,
            REPARSE_TAG_WOF
        ),
        (0xA000_0003, 0xA000_000C, 0x8000_0013, 0x8000_0017)
    );
    // Every cloud tag matches under the mask, others do not.
    for tag in [0x9000_001Au32, 0x9000_101A, 0x9000_F01A] {
        assert_eq!(tag & REPARSE_TAG_CLOUD_MASK, REPARSE_TAG_CLOUD);
    }
    assert_ne!(REPARSE_TAG_WOF & REPARSE_TAG_CLOUD_MASK, REPARSE_TAG_CLOUD);
    // Too short for a tag, the wrong type, or not in the record.
    assert_eq!(
        only(&record_of(&[resident(0xC0, "", 0, 3, &[1, 2, 3])])).reparse_tag(),
        None
    );
    assert_eq!(
        only(&record_of(&[resident(0x80, "", 0, 3, &value)])).reparse_tag(),
        None
    );
    let bytes = record_of(&[NonResident {
        type_code: 0xC0,
        runlist: &[0x11, 0x04, 0x50, 0x00],
        ..NonResident::default()
    }
    .build()]);
    let attr = only(&bytes);
    assert_eq!(attr.reparse_tag(), None);
    assert!(attr.extent().is_some(), "the extent says where the tag is");
}

#[test]
fn types_this_crate_does_not_decode_are_still_walked() {
    let bytes = record_of(&[
        resident(0x40, "", 0, 0, &[0x11; 16]),
        resident(0x50, "", 0, 1, &[0x22; 100]),
        resident(0x70, "", 0, 2, &[0; 12]),
        resident(0x100, "$TXF_DATA", 0, 3, &[0x44; 56]),
        resident(0xD0, "", 0, 4, &[0; 8]),
        resident(0x1000, "", 0, 5, &[0; 8]),
    ]);
    let record = parse(&bytes);
    assert_eq!(record.attribute_count(), 6);
    let types: Vec<u32> = record.attributes().map(|a| a.type_code).collect();
    assert_eq!(types, [0x40, 0x50, 0x70, 0x100, 0xD0, 0x1000]);
    assert!(record.attributes().all(|a| a.resident().is_some()));
    assert!(record
        .find(attr_type::LOGGED_UTILITY_STREAM, "$TXF_DATA")
        .is_some());
}
