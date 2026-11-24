//! Real structures, on every test run.
//!
//! `tests/data/structures.bin` holds 30 structures copied out of the
//! fixture images (volumes written by mkntfs and ntfs-3g): 23 `FILE`
//! records, 4 `INDX` blocks and 3 boot sectors, one per feature, laid one
//! behind the other at 512-byte boundaries. `tests/data/structures.json` is
//! what `scripts/linux/inspect-ntfs.py --json` says about that file, and
//! `tests/data/make-structures.py` rebuilds both and prints the list.
//!
//! The first test runs the full comparison of `tests/fixtures.rs` on them,
//! so that the parser is checked against real bytes and an independent
//! reading on every platform, without fixtures and without Python. The
//! others state, in the terms of the public interface, what some of the
//! structures are.

mod common;

use common::json::Json;
use common::xcheck::cross_check;
use hds_ntfs::{
    attr_type, parse_boot_sector, parse_file_record, parse_index_block, AttrChain, AttributeList,
    FileRecord, FileRef, FixupStatus, Namespace, Run, RunlistStatus, WalkEnd, I30,
};

fn structures() -> Vec<u8> {
    std::fs::read(common::data_dir().join("structures.bin")).expect("tests/data/structures.bin")
}

/// The record at `offset` of the sample file, fixed up in `scratch`.
fn record_at<'a>(
    image: &[u8],
    offset: usize,
    size: usize,
    scratch: &'a mut Vec<u8>,
) -> FileRecord<'a> {
    scratch.clear();
    scratch.extend_from_slice(&image[offset..offset + size]);
    let record = parse_file_record(scratch).expect("sample record");
    assert_eq!(record.fixup(), FixupStatus::Ok);
    assert_eq!(record.attr_chain(), AttrChain::Complete);
    record
}

#[test]
fn the_sample_structures_match_the_inspector() {
    let image = structures();
    let doc = Json::parse(
        &std::fs::read(common::data_dir().join("structures.json"))
            .expect("tests/data/structures.json"),
    )
    .expect("structures.json");
    assert_eq!(
        doc.get("image_size").u64(),
        image.len() as u64,
        "structures.json is not about structures.bin"
    );
    let (counts, failures) = cross_check("structures.bin", &image, &doc);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // Nothing was skipped.
    assert_eq!(counts.sectors, 93);
    assert_eq!((counts.records, counts.records_invalid), (23, 0));
    assert_eq!((counts.index_blocks, counts.index_blocks_invalid), (4, 0));
    assert_eq!(counts.boot_sectors, 3);
    assert_eq!(counts.records, doc.get("counts").get("file_records").u64());
    assert_eq!(
        counts.index_blocks,
        doc.get("counts").get("index_blocks").u64()
    );
    assert_eq!(doc.get("counts").get("invalid").u64(), 0);
    // What the 30 structures hold, as counted by the comparison.
    assert_eq!(counts.attributes, 93);
    assert_eq!(counts.extents, 16);
    assert_eq!(counts.runs, 249);
    assert_eq!(counts.index_roots, 9);
    assert_eq!(counts.root_entries, 7);
    assert_eq!(counts.block_entries, 69);
    assert_eq!(counts.foreign_entries, 4);
}

#[test]
fn the_mft_record_describes_the_table() {
    let image = structures();
    let mut scratch = Vec::new();
    let record = record_at(&image, 0, 1024, &mut scratch);
    assert_eq!((record.record_number(), record.sequence()), (Some(0), 1));
    assert!(record.is_in_use() && !record.is_directory() && !record.is_extension());
    let name = record.file_names().next().unwrap();
    assert!(name.name == "$MFT");
    assert_eq!(name.namespace, Namespace::Win32AndDos);
    assert_eq!(name.parent, FileRef::new(5, 5));
    let data = record.find(attr_type::DATA, "").unwrap().extent().unwrap();
    assert_eq!((data.start_vcn, data.last_vcn), (0, 6));
    let sizes = data.sizes().unwrap();
    assert_eq!(
        (sizes.allocated, sizes.real, sizes.initialized),
        (28_672, 27_648, 27_648)
    );
    assert_eq!(
        data.runs().collect::<Vec<Run>>(),
        [Run {
            lcn: Some(4),
            length: 7
        }]
    );
    let bitmap = record
        .find(attr_type::BITMAP, "")
        .unwrap()
        .extent()
        .unwrap();
    assert_eq!(bitmap.sizes().unwrap().real, 8);
    assert_eq!(
        bitmap.runs().collect::<Vec<Run>>(),
        [Run {
            lcn: Some(2),
            length: 1
        }]
    );
}

#[test]
fn the_volume_record_has_the_label() {
    let image = structures();
    let mut scratch = Vec::new();
    let record = record_at(&image, 0x400, 1024, &mut scratch);
    assert_eq!(record.record_number(), Some(3));
    let label = record.find(attr_type::VOLUME_NAME, "").unwrap();
    assert!(label.volume_name().unwrap() == "NEW-BASIC");
    assert_eq!(label.resident().unwrap().offset, 384);
    // mkntfs writes the 48-byte $STANDARD_INFORMATION here.
    let si = record.find(attr_type::STANDARD_INFORMATION, "").unwrap();
    assert_eq!(si.resident().unwrap().len(), 48);
    assert_eq!(record.standard_information().unwrap().flags, 0x6);
}

#[test]
fn a_never_used_record() {
    let image = structures();
    let mut scratch = Vec::new();
    let record = record_at(&image, 0x1000, 1024, &mut scratch);
    assert!(record.is_never_used());
    assert_eq!(
        (record.record_number(), record.sequence(), record.flags()),
        (Some(28), 1, 0)
    );
    assert_eq!((record.used_size(), record.attribute_count()), (0x40, 0));
}

#[test]
fn a_file_with_a_unicode_name_and_resident_content() {
    let image = structures();
    let mut scratch = Vec::new();
    let record = record_at(&image, 0x1400, 1024, &mut scratch);
    let name = record.file_names().next().unwrap();
    assert_eq!(
        name.name.to_string_lossy(),
        "Caf\u{e9} d\u{e9}j\u{e0} vu \u{2013} na\u{ef}ve fa\u{e7}ade.txt"
    );
    assert_eq!(name.namespace, Namespace::Posix);
    assert_eq!(name.parent, FileRef::new(242, 1));
    let data = record
        .find(attr_type::DATA, "")
        .unwrap()
        .resident()
        .unwrap();
    assert_eq!((data.offset, data.len()), (408, 150));
    assert_eq!(&record.bytes()[408..408 + 150], data.bytes);
    // 2017-11-29T12:03:29.5015975Z, set through the driver.
    assert_eq!(
        hds_ntfs::filetime_to_unix_time(record.standard_information().unwrap().times.created),
        Some((1_511_957_009, 501_597_500))
    );
}

#[test]
fn a_compressed_and_a_sparse_file() {
    let image = structures();
    let mut scratch = Vec::new();
    let record = record_at(&image, 0x1800, 1024, &mut scratch);
    let data = record.find(attr_type::DATA, "").unwrap();
    assert!(data.is_compressed() && !data.is_sparse());
    let extent = data.extent().unwrap();
    assert_eq!(
        (extent.compression_unit, extent.total_allocated),
        (4, Some(61_440))
    );
    let runs: Vec<(Option<u64>, u64)> = extent.runs().map(|r| (r.lcn, r.length)).collect();
    assert_eq!(
        runs,
        [
            (Some(17_504), 7),
            (None, 9),
            (Some(17_511), 7),
            (None, 9),
            (Some(17_518), 1),
            (None, 15)
        ]
    );

    let mut scratch = Vec::new();
    let record = record_at(&image, 0x4C00, 1024, &mut scratch);
    let data = record.find(attr_type::DATA, "").unwrap();
    assert!(
        data.is_sparse() && !data.is_compressed(),
        "compression unit 4 without the flag"
    );
    let extent = data.extent().unwrap();
    assert_eq!(extent.compression_unit, 4);
    let sizes = extent.sizes().unwrap();
    assert_eq!(
        (sizes.allocated, sizes.real, sizes.initialized),
        (3_149_824, 3_145_828, 3_145_828)
    );
    let mut decoder = extent.runs();
    let runs: Vec<(Option<u64>, u64)> = decoder.by_ref().map(|r| (r.lcn, r.length)).collect();
    assert_eq!(
        runs,
        [
            (Some(8825), 2),
            (None, 254),
            (Some(9081), 2),
            (None, 510),
            (Some(9593), 1)
        ]
    );
    assert_eq!(
        (decoder.status(), decoder.clusters()),
        (Some(RunlistStatus::Complete), 769)
    );

    // Initialized size far below the size: the rest reads as zeros.
    let mut scratch = Vec::new();
    let record = record_at(&image, 0x5000, 1024, &mut scratch);
    let sizes = record
        .find(attr_type::DATA, "")
        .unwrap()
        .extent()
        .unwrap()
        .sizes()
        .unwrap();
    assert_eq!((sizes.real, sizes.initialized), (500_000, 5000));
}

#[test]
fn deleted_records_keep_their_attributes() {
    let image = structures();
    let mut scratch = Vec::new();
    let file = record_at(&image, 0x2400, 1024, &mut scratch);
    assert!(!file.is_in_use() && !file.is_never_used());
    assert_eq!((file.record_number(), file.sequence()), (Some(3399), 2));
    assert!(file.file_names().next().unwrap().name == "e-b.txt");
    assert_eq!(
        file.find(attr_type::DATA, "")
            .unwrap()
            .resident()
            .unwrap()
            .len(),
        120
    );
    // Its parent reference (3397, 1) and its own old reference (3399, 1)
    // follow the rule: freed records are one sequence step on.
    assert!(FileRef::new(3399, 1).resolves_to(file.sequence(), file.is_in_use()));
    assert!(!FileRef::new(3399, 2).resolves_to(file.sequence(), file.is_in_use()));

    let mut scratch = Vec::new();
    let dir = record_at(&image, 0x2800, 1024, &mut scratch);
    assert!(
        !dir.is_in_use() && dir.is_directory(),
        "the directory bit survives deletion"
    );
    let root = dir
        .find(attr_type::INDEX_ROOT, I30)
        .unwrap()
        .index_root()
        .unwrap();
    assert_eq!(root.entries().count(), 0);
}

#[test]
fn a_named_stream_next_to_the_content() {
    let image = structures();
    let mut scratch = Vec::new();
    let record = record_at(&image, 0x2C00, 1024, &mut scratch);
    let streams: Vec<String> = record
        .attributes()
        .filter(|a| a.type_code == attr_type::DATA)
        .map(|a| a.name.to_string_lossy())
        .collect();
    assert_eq!(streams, ["", "Zone.Identifier"]);
    assert_eq!(
        record
            .find(attr_type::DATA, "")
            .unwrap()
            .extent()
            .unwrap()
            .sizes()
            .unwrap()
            .real,
        9002
    );
    let zone = record
        .find(attr_type::DATA, "Zone.Identifier")
        .unwrap()
        .resident()
        .unwrap();
    assert_eq!((zone.offset, zone.len()), (480, 26));
}

#[test]
fn a_base_record_with_its_extension_record() {
    let image = structures();
    let mut scratch = Vec::new();
    let base = record_at(&image, 0x3000, 1024, &mut scratch);
    assert_eq!(base.record_number(), Some(271));
    assert!(base.is_directory());
    // ntfs-3g keeps even a 136-byte list outside the record.
    let Some(AttributeList::NonResident(list)) = base
        .find(attr_type::ATTRIBUTE_LIST, "")
        .unwrap()
        .attribute_list()
    else {
        panic!("non-resident attribute list expected");
    };
    assert_eq!(list.sizes().unwrap().real, 136);
    assert_eq!(
        list.runs().collect::<Vec<Run>>(),
        [Run {
            lcn: Some(25_880),
            length: 1
        }]
    );
    // The directory's index root is not in the base record ...
    assert!(base.find(attr_type::INDEX_ROOT, I30).is_none());
    assert_eq!(base.file_names().next().unwrap().name.len(), 224);

    // ... but in the extension record, which never carries the directory
    // flag and has no links.
    let mut scratch = Vec::new();
    let extension = record_at(&image, 0x3400, 1024, &mut scratch);
    assert_eq!(extension.record_number(), Some(273));
    assert!(extension.is_extension() && extension.is_in_use() && !extension.is_directory());
    assert_eq!(extension.base(), FileRef::new(271, 1));
    assert_eq!(extension.link_count(), 0);
    assert!(extension
        .base()
        .resolves_to(base.sequence(), base.is_in_use()));
    assert!(extension.standard_information().is_none());
    let root = extension
        .find(attr_type::INDEX_ROOT, I30)
        .unwrap()
        .index_root()
        .unwrap();
    let names: Vec<usize> = root.entries().map(|e| e.file_name.name.len()).collect();
    assert_eq!(names, [8, 211]);
    assert_eq!(root.entries().owner(), Some(FileRef::new(271, 1)));
}

#[test]
fn a_long_name_with_its_dos_alias() {
    let image = structures();
    let mut scratch = Vec::new();
    let record = record_at(&image, 0x3C00, 1024, &mut scratch);
    assert_eq!(record.link_count(), 2);
    let names: Vec<(String, Namespace, FileRef)> = record
        .file_names()
        .map(|n| (n.name.to_string_lossy(), n.namespace, n.parent))
        .collect();
    assert_eq!(
        names,
        [
            ("PROGRA~1".to_string(), Namespace::Dos, FileRef::new(313, 1)),
            (
                "Program Files".to_string(),
                Namespace::Win32,
                FileRef::new(313, 1)
            ),
        ]
    );
}

#[test]
fn an_extension_record_filled_with_runs_to_its_last_byte() {
    let image = structures();
    let mut scratch = Vec::new();
    let record = record_at(&image, 0x4800, 1024, &mut scratch);
    assert_eq!(record.used_size(), 1024);
    assert_eq!(record.base(), FileRef::new(2553, 1));
    assert_eq!(record.attribute_count(), 1);
    let extent = record.find(attr_type::DATA, "").unwrap().extent().unwrap();
    assert_eq!((extent.start_vcn, extent.last_vcn), (428, 678));
    assert_eq!(extent.sizes(), None, "a later extent carries no sizes");
    let mut runs = Vec::new();
    let decoded = extent.decode_runs(&mut runs);
    assert_eq!(
        (decoded.status, decoded.runs, decoded.clusters),
        (RunlistStatus::Complete, 223, 251)
    );
    assert_eq!(
        runs[0],
        Run {
            lcn: Some(14_177),
            length: 1
        }
    );
    assert_eq!(
        runs[1],
        Run {
            lcn: Some(10_091),
            length: 2
        },
        "a negative offset"
    );
    assert!(runs.iter().all(|run| !run.is_sparse()));
}

#[test]
fn records_of_4096_bytes() {
    let image = structures();
    let mut scratch = Vec::new();
    let root = record_at(&image, 0x5400, 4096, &mut scratch);
    assert_eq!(
        (
            root.allocated_size(),
            root.header().usa_count,
            root.header().attrs_offset
        ),
        (4096, 9, 0x48)
    );
    assert_eq!(root.record_number(), Some(5));
    assert!(root.file_names().next().unwrap().name == ".");
    assert!(root.find(attr_type::INDEX_ALLOCATION, I30).is_some());

    // A resident value of 2560 bytes: it lies across five block ends, all
    // of which had to be restored.
    let mut scratch = Vec::new();
    let attrdef = record_at(&image, 0x6400, 4096, &mut scratch);
    let data = attrdef
        .find(attr_type::DATA, "")
        .unwrap()
        .resident()
        .unwrap();
    assert_eq!((data.offset, data.len()), (408, 2560));
    // $AttrDef is a table of 160-byte entries that start with the name of
    // an attribute type in UTF-16. The eighth entry lies across the end of
    // the third 512-byte block: on disk the update sequence number sits in
    // the middle of its name, in the parsed record the name is whole.
    let entry = 7 * 160;
    let name = |bytes: &[u8]| -> Vec<u16> {
        bytes
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect()
    };
    assert_eq!(
        String::from_utf16(&name(&data.bytes[entry..entry + 10])).unwrap(),
        "$DATA"
    );
    let on_disk = &image[0x6400 + 408 + entry..][..10];
    assert_eq!(
        name(on_disk),
        [0x24, 0x44, 0x41, 0x0002, 0x41],
        "\"$DA\", the number 2, \"A\""
    );
    assert_eq!(0x6400 + 408 + entry + 6, 0x6400 + 3 * 512 - 2);
}

#[test]
fn index_blocks() {
    let image = structures();
    let mut scratch = image[0x7400..0x8400].to_vec();
    let leaf = parse_index_block(&mut scratch).unwrap();
    assert_eq!(
        (leaf.vcn(), leaf.size(), leaf.fixup()),
        (8, 4096, FixupStatus::Ok)
    );
    assert!(!leaf.node().has_subnodes());
    let mut entries = leaf.entries();
    assert_eq!(entries.by_ref().count(), 21);
    assert_eq!(
        (entries.end(), entries.skipped()),
        (Some(WalkEnd::Complete), 0)
    );
    let owner = leaf.owner().unwrap();
    assert!(leaf
        .entries()
        .all(|e| e.file_name.parent == owner && e.subnode_vcn.is_none()));

    let mut scratch = image[0x8400..0x9400].to_vec();
    let node = parse_index_block(&mut scratch).unwrap();
    assert_eq!(node.vcn(), 57);
    assert!(node.node().has_subnodes());
    assert_eq!(node.entries().count(), 17);
    assert!(node.entries().all(|e| e.subnode_vcn.is_some()));

    // 64 KiB clusters: the VCN counts 512-byte units, 128 = block 16.
    let mut scratch = image[0x9400..0xA400].to_vec();
    assert_eq!(parse_index_block(&mut scratch).unwrap().vcn(), 128);
}

#[test]
fn boot_sectors() {
    let image = structures();
    let basic = parse_boot_sector(&image[0xB400..0xB600]).unwrap();
    assert_eq!((basic.bytes_per_sector, basic.cluster_size), (512, 4096));
    assert_eq!(
        (basic.total_sectors, basic.mft_lcn, basic.mftmirr_lcn),
        (262_143, 4, 16_383)
    );
    assert_eq!(
        (basic.mft_record_size, basic.index_block_size),
        (1024, 4096)
    );
    assert_eq!(basic.serial, 0x2D9B_B821_139D_1D59);
    let big = parse_boot_sector(&image[0xB600..0xB800]).unwrap();
    assert_eq!(
        (
            big.sectors_per_cluster,
            big.cluster_size,
            big.index_block_size
        ),
        (128, 65_536, 4096)
    );
    let k4 = parse_boot_sector(&image[0xB800..0xBA00]).unwrap();
    assert_eq!(
        (k4.bytes_per_sector, k4.cluster_size, k4.mft_record_size),
        (4096, 4096, 4096)
    );
    assert_eq!(k4.volume_size(), 16_383 * 4096);
}

#[test]
fn json_reader_reads_what_the_inspector_writes() {
    // The inspector escapes everything outside ASCII, unpaired surrogates
    // included. `U` stands for `u` here so that this source file holds no
    // escape sequence a tool could expand.
    let doc = r#"{"a":[1,-2,18446744073709551615],"s":"x\U00e9\Ud83d\Ude00\Ud800\"\\","n":null,"t":true,"f":false,"o":{}}"#
        .replace('U', "u");
    let json = Json::parse(doc.as_bytes()).unwrap();
    assert_eq!(json.get("a").arr()[2].u64(), u64::MAX);
    assert_eq!(json.get("a").arr()[1].i64(), -2);
    assert_eq!(
        json.get("s").units(),
        &[0x78, 0xE9, 0xD83D, 0xDE00, 0xD800, 0x22, 0x5C]
    );
    assert!(json.get("n").is_null());
    assert!(json.get("t").bool());
    assert!(!json.get("f").bool());
    assert!(json.opt("missing").is_none());
    assert!(Json::parse(b"{\"a\":1.5}").is_err());
    assert!(Json::parse(b"[1,2").is_err());
    assert!(Json::parse(b"[1] x").is_err());
}
