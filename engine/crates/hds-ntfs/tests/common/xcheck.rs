//! The comparison of this crate's reading of an image with the reading of
//! `scripts/linux/inspect-ntfs.py --json` of the same bytes.
//!
//! Used on whole fixture images (`tests/fixtures.rs`, with the inspector run
//! on the spot) and on the committed sample structures (`tests/corpus.rs`,
//! with its output stored next to them).

use std::collections::HashMap;
use std::fmt::Debug;

use super::json::Json;
use hds_ntfs::{
    attr_type, fnv1a64, is_boot_sector_candidate, parse_boot_sector, parse_file_record,
    parse_index_block, probe_file_record, probe_index_block, AttrBody, Attribute, AttributeList,
    FileName, FileRecord, FixupStatus, IndexEntries, Run, RunlistStatus, Utf16Name, WalkEnd,
    FILE_MAGIC, INDX_MAGIC, MAX_RECORD_SIZE, SECTOR_SIZE,
};

/// What was compared, for the report at the end.
#[derive(Default, Debug)]
pub struct Counts {
    pub images: u64,
    pub sectors: u64,
    pub boot_sectors: u64,
    pub records: u64,
    pub records_invalid: u64,
    pub attributes: u64,
    pub names: u64,
    pub data_attributes: u64,
    pub extents: u64,
    pub runs: u64,
    pub attr_list_entries: u64,
    pub index_roots: u64,
    pub root_entries: u64,
    pub index_blocks: u64,
    pub index_blocks_invalid: u64,
    pub block_entries: u64,
    pub foreign_entries: u64,
    pub fields: u64,
}

impl Counts {
    pub fn add(&mut self, other: &Counts) {
        self.images += other.images;
        self.sectors += other.sectors;
        self.boot_sectors += other.boot_sectors;
        self.records += other.records;
        self.records_invalid += other.records_invalid;
        self.attributes += other.attributes;
        self.names += other.names;
        self.data_attributes += other.data_attributes;
        self.extents += other.extents;
        self.runs += other.runs;
        self.attr_list_entries += other.attr_list_entries;
        self.index_roots += other.index_roots;
        self.root_entries += other.root_entries;
        self.index_blocks += other.index_blocks;
        self.index_blocks_invalid += other.index_blocks_invalid;
        self.block_entries += other.block_entries;
        self.foreign_entries += other.foreign_entries;
        self.fields += other.fields;
    }
}

/// Collects disagreements instead of stopping at the first one.
struct Check {
    image: String,
    context: String,
    counts: Counts,
    failures: Vec<String>,
}

impl Check {
    fn eq<T: PartialEq + Debug>(&mut self, what: &str, mine: T, theirs: T) {
        self.counts.fields += 1;
        if mine != theirs {
            self.fail(format!("{what}: hds-ntfs {mine:?}, inspector {theirs:?}"));
        }
    }

    fn fail(&mut self, message: String) {
        if self.failures.len() < 200 {
            self.failures
                .push(format!("{} {}: {message}", self.image, self.context));
        }
    }
}

fn by_offset(items: &Json) -> HashMap<u64, &Json> {
    let map: HashMap<u64, &Json> = items
        .arr()
        .iter()
        .map(|item| (item.get("offset").u64(), item))
        .collect();
    assert_eq!(
        map.len(),
        items.arr().len(),
        "duplicate offsets in the inspector output"
    );
    map
}

fn units(name: Utf16Name<'_>) -> Vec<u16> {
    name.units().collect()
}

fn hex(bytes: &[u8]) -> Vec<u16> {
    bytes
        .iter()
        .flat_map(|b| format!("{b:02x}").encode_utf16().collect::<Vec<_>>())
        .collect()
}

fn compare_boot(check: &mut Check, sector: &[u8], theirs: &Json) {
    check.counts.boot_sectors += 1;
    let mine = parse_boot_sector(sector);
    check.eq(
        "boot sector valid",
        mine.is_ok(),
        theirs.get("valid").bool(),
    );
    let Ok(mine) = mine else { return };
    check.eq(
        "bytes_per_sector",
        u64::from(mine.bytes_per_sector),
        theirs.get("bytes_per_sector").u64(),
    );
    check.eq(
        "sectors_per_cluster",
        u64::from(mine.sectors_per_cluster),
        theirs.get("sectors_per_cluster").u64(),
    );
    check.eq(
        "cluster_size",
        u64::from(mine.cluster_size),
        theirs.get("cluster_size").u64(),
    );
    check.eq(
        "hidden_sectors",
        u64::from(mine.hidden_sectors),
        theirs.get("hidden_sectors").u64(),
    );
    check.eq(
        "total_sectors",
        i128::from(mine.total_sectors),
        theirs.get("total_sectors").int(),
    );
    check.eq(
        "volume_size",
        i128::from(mine.volume_size()),
        theirs.get("volume_size").int(),
    );
    check.eq(
        "mft_lcn",
        i128::from(mine.mft_lcn),
        theirs.get("mft_lcn").int(),
    );
    check.eq(
        "mftmirr_lcn",
        i128::from(mine.mftmirr_lcn),
        theirs.get("mftmirr_lcn").int(),
    );
    check.eq(
        "mft_record_size",
        u64::from(mine.mft_record_size),
        theirs.get("mft_record_size").u64(),
    );
    check.eq(
        "index_block_size",
        u64::from(mine.index_block_size),
        theirs.get("index_block_size").u64(),
    );
    check.eq(
        "serial",
        format!("{:016X}", mine.serial)
            .encode_utf16()
            .collect::<Vec<_>>(),
        theirs.get("serial").units().to_vec(),
    );
}

fn compare_fixup(check: &mut Check, mine: FixupStatus, theirs: &Json) {
    let bad = theirs.get("fixup_mismatch_sectors").arr();
    check.eq("fixup_ok", mine.is_ok(), theirs.get("fixup_ok").bool());
    match mine {
        FixupStatus::Ok => check.eq("fixup mismatches", 0, bad.len()),
        FixupStatus::Torn {
            first_bad_block,
            bad_blocks,
        } => {
            check.eq("fixup mismatches", usize::from(bad_blocks), bad.len());
            if let Some(first) = bad.first() {
                check.eq(
                    "first mismatching block",
                    u64::from(first_bad_block),
                    first.u64(),
                );
            }
        }
    }
}

fn compare_name(check: &mut Check, mine: &FileName<'_>, theirs: &Json) {
    check.counts.names += 1;
    check.eq(
        "name",
        units(mine.name),
        theirs.get("name").units().to_vec(),
    );
    check.eq(
        "namespace",
        u64::from(mine.namespace.as_u8()),
        theirs.get("namespace").u64(),
    );
    check.eq(
        "parent_record",
        mine.parent.number(),
        theirs.get("parent_record").u64(),
    );
    check.eq(
        "parent_sequence",
        u64::from(mine.parent.sequence()),
        theirs.get("parent_sequence").u64(),
    );
    check.eq(
        "created",
        mine.times.created.cast_signed(),
        theirs.get("created").i64(),
    );
    check.eq(
        "modified",
        mine.times.modified.cast_signed(),
        theirs.get("modified").i64(),
    );
    check.eq(
        "mft_changed",
        mine.times.mft_changed.cast_signed(),
        theirs.get("mft_changed").i64(),
    );
    check.eq(
        "accessed",
        mine.times.accessed.cast_signed(),
        theirs.get("accessed").i64(),
    );
    check.eq(
        "allocated_size",
        mine.allocated_size.cast_signed(),
        theirs.get("allocated_size").i64(),
    );
    check.eq(
        "data_size",
        mine.real_size.cast_signed(),
        theirs.get("data_size").i64(),
    );
    check.eq(
        "file_attributes",
        u64::from(mine.flags),
        theirs.get("file_attributes").u64(),
    );
    check.eq(
        "directory flag",
        mine.is_directory(),
        theirs.get("file_attributes").u64() & 0x1000_0000 != 0,
    );
}

/// Compares the entries of one index node. Returns whether this crate's
/// walk reached the end entry.
fn compare_entries(
    check: &mut Check,
    mut mine: IndexEntries<'_>,
    theirs: &Json,
    in_root: bool,
) -> bool {
    let theirs = theirs.arr();
    let mut foreign = 0u32;
    for entry in theirs {
        if entry.get("is_last").bool() {
            continue;
        }
        let name = entry.get("file_name");
        if name.is_null() {
            foreign += 1;
            check.counts.foreign_entries += 1;
            continue;
        }
        let Some(found) = mine.next() else {
            check.fail(format!(
                "index entry at +{} missing",
                entry.get("offset").u64()
            ));
            return false;
        };
        if in_root {
            check.counts.root_entries += 1;
        } else {
            check.counts.block_entries += 1;
        }
        check.eq(
            "entry offset",
            u64::from(found.offset),
            entry.get("offset").u64(),
        );
        check.eq(
            "entry flags",
            u64::from(found.flags),
            entry.get("flags").u64(),
        );
        check.eq(
            "entry record",
            found.child.number(),
            entry.get("record").u64(),
        );
        check.eq(
            "entry sequence",
            u64::from(found.child.sequence()),
            entry.get("sequence").u64(),
        );
        let subnode = entry.get("subnode_vcn");
        check.eq(
            "entry subnode_vcn",
            found.subnode_vcn.map(u64::cast_signed),
            (!subnode.is_null()).then(|| subnode.i64()),
        );
        compare_name(check, &found.file_name, name);
    }
    if let Some(extra) = mine.next() {
        check.fail(format!("extra index entry at +{}", extra.offset));
    }
    check.eq("entries with foreign keys", mine.skipped(), foreign);
    let ended = theirs
        .last()
        .is_some_and(|entry| entry.get("is_last").bool());
    let complete = mine.end() == Some(WalkEnd::Complete);
    check.eq("end entry reached", complete, ended);
    complete
}

/// Compares one attribute. Returns whether this crate found it sound in the
/// sense of the inspector's "no errors".
fn compare_attribute(
    check: &mut Check,
    record: &FileRecord<'_>,
    mine: &Attribute<'_>,
    theirs: &Json,
    runs: &mut Vec<Run>,
) -> bool {
    check.counts.attributes += 1;
    check.eq(
        "attribute type",
        u64::from(mine.type_code),
        theirs.get("type").u64(),
    );
    check.eq(
        "attribute offset",
        u64::from(mine.offset),
        theirs.get("offset").u64(),
    );
    check.eq(
        "attribute length",
        u64::from(mine.length),
        theirs.get("length").u64(),
    );
    check.eq(
        "attribute flags",
        u64::from(mine.flags),
        theirs.get("flags").u64(),
    );
    check.eq(
        "attribute id",
        u64::from(mine.id),
        theirs.get("attr_id").u64(),
    );
    let non_resident = theirs.get("non_resident").bool();
    match mine.body {
        AttrBody::Malformed => return false,
        AttrBody::Resident(_) => check.eq("non_resident", false, non_resident),
        AttrBody::NonResident(_) => check.eq("non_resident", true, non_resident),
    }
    check.eq(
        "attribute name",
        units(mine.name),
        theirs.get("name").units().to_vec(),
    );

    let mut sound = true;
    match mine.body {
        AttrBody::Malformed => {}
        AttrBody::Resident(value) => {
            check.eq(
                "value_offset",
                u64::from(value.offset),
                theirs.get("value_offset").u64(),
            );
            check.eq(
                "value_length",
                u64::from(value.len()),
                theirs.get("value_length").u64(),
            );
            let start = value.offset as usize;
            check.eq(
                "value bytes are where the offset says",
                record.bytes().get(start..start + value.bytes.len()),
                Some(value.bytes),
            );
            match mine.type_code {
                attr_type::STANDARD_INFORMATION => {
                    let si = mine.standard_information();
                    match (si, theirs.opt("standard_information")) {
                        (Some(si), Some(expected)) => {
                            check.eq(
                                "si created",
                                si.times.created.cast_signed(),
                                expected.get("created").i64(),
                            );
                            check.eq(
                                "si modified",
                                si.times.modified.cast_signed(),
                                expected.get("modified").i64(),
                            );
                            check.eq(
                                "si mft_changed",
                                si.times.mft_changed.cast_signed(),
                                expected.get("mft_changed").i64(),
                            );
                            check.eq(
                                "si accessed",
                                si.times.accessed.cast_signed(),
                                expected.get("accessed").i64(),
                            );
                            check.eq(
                                "si file_attributes",
                                u64::from(si.flags),
                                expected.get("file_attributes").u64(),
                            );
                        }
                        (None, None) => sound = false,
                        (mine, theirs) => {
                            // The inspector decodes from 36 bytes on, this crate from 48.
                            check.eq(
                                "standard_information decoded",
                                mine.is_some(),
                                theirs.is_some(),
                            );
                            sound = false;
                        }
                    }
                }
                attr_type::FILE_NAME => {
                    let expected = theirs.get("file_name");
                    match mine.file_name() {
                        Some(name) if !expected.is_null() => compare_name(check, &name, expected),
                        None if expected.is_null() => sound = false,
                        name => {
                            check.eq("file_name decoded", name.is_some(), !expected.is_null());
                            sound = false;
                        }
                    }
                }
                attr_type::ATTRIBUTE_LIST => {
                    let Some(AttributeList::Resident(mut entries)) = mine.attribute_list() else {
                        check.fail("resident attribute list not offered".into());
                        return false;
                    };
                    let expected = theirs.get("attribute_list").arr();
                    for want in expected {
                        let Some(entry) = entries.next() else {
                            check.fail("attribute list entry missing".into());
                            break;
                        };
                        check.counts.attr_list_entries += 1;
                        check.eq(
                            "list entry type",
                            u64::from(entry.type_code),
                            want.get("type").u64(),
                        );
                        check.eq(
                            "list entry name",
                            units(entry.name),
                            want.get("name").units().to_vec(),
                        );
                        check.eq(
                            "list entry lowest_vcn",
                            entry.start_vcn.cast_signed(),
                            want.get("lowest_vcn").i64(),
                        );
                        check.eq(
                            "list entry record",
                            entry.holder.number(),
                            want.get("record").u64(),
                        );
                        check.eq(
                            "list entry sequence",
                            u64::from(entry.holder.sequence()),
                            want.get("sequence").u64(),
                        );
                        check.eq(
                            "list entry attr_id",
                            u64::from(entry.attr_id),
                            want.get("attr_id").u64(),
                        );
                    }
                    check.eq("further list entries", entries.next().is_some(), false);
                    // The inspector has no notion of a broken list; a list
                    // in a sound record must end exactly with its value.
                    sound &= entries.end() == Some(WalkEnd::Complete);
                }
                attr_type::VOLUME_NAME => {
                    check.eq(
                        "volume_name",
                        mine.volume_name().map(units),
                        Some(theirs.get("volume_name").units().to_vec()),
                    );
                }
                attr_type::INDEX_ROOT => match (mine.index_root(), theirs.opt("index_root")) {
                    (Some(root), Some(expected)) => {
                        check.counts.index_roots += 1;
                        check.eq(
                            "root indexed_type",
                            u64::from(root.indexed_type),
                            expected.get("indexed_type").u64(),
                        );
                        check.eq(
                            "root collation",
                            u64::from(root.collation),
                            expected.get("collation").u64(),
                        );
                        check.eq(
                            "root index_block_size",
                            u64::from(root.index_block_size),
                            expected.get("index_block_size").u64(),
                        );
                        check.eq(
                            "root blocks per index block",
                            i64::from(root.blocks_per_index_block.cast_signed()),
                            expected.get("clusters_per_index_block_raw").i64(),
                        );
                        check.eq(
                            "root entries_offset",
                            u64::from(root.node.entries_offset),
                            expected.get("entries_offset").u64(),
                        );
                        check.eq(
                            "root index_length",
                            u64::from(root.node.used_size),
                            expected.get("index_length").u64(),
                        );
                        check.eq(
                            "root allocated_size",
                            u64::from(root.node.allocated_size),
                            expected.get("allocated_size").u64(),
                        );
                        check.eq(
                            "root node_flags",
                            u64::from(root.node.flags),
                            expected.get("node_flags").u64(),
                        );
                        sound &=
                            compare_entries(check, root.entries(), expected.get("entries"), true);
                    }
                    (None, Some(expected)) => {
                        // Present but not walkable for the inspector either.
                        check.eq(
                            "unusable root has no entries",
                            expected.get("entries").arr().len(),
                            0,
                        );
                        sound = false;
                    }
                    (root, None) => {
                        // Values under 32 bytes: no root for either side.
                        check.eq("index_root decoded", root.is_some(), false);
                    }
                },
                attr_type::BITMAP => {
                    check.eq(
                        "bitmap",
                        hex(value.bytes),
                        theirs.get("bitmap_hex").units().to_vec(),
                    );
                }
                attr_type::REPARSE_POINT => {
                    let tag = value
                        .bytes
                        .get(..4)
                        .map(|b| u32::from_le_bytes(b.try_into().unwrap()));
                    check.eq("reparse tag", mine.reparse_tag(), tag);
                }
                _ => {}
            }
        }
        AttrBody::NonResident(extent) => {
            check.counts.extents += 1;
            check.eq(
                "lowest_vcn",
                extent.start_vcn.cast_signed(),
                theirs.get("lowest_vcn").i64(),
            );
            check.eq(
                "highest_vcn",
                extent.last_vcn,
                theirs.get("highest_vcn").i64(),
            );
            check.eq(
                "mapping_pairs_offset",
                u64::from(mine.length) - extent.runlist.len() as u64,
                theirs.get("mapping_pairs_offset").u64(),
            );
            check.eq(
                "compression_unit",
                u64::from(extent.compression_unit),
                theirs.get("compression_unit").u64(),
            );
            check.eq(
                "allocated_size",
                extent.allocated_size.cast_signed(),
                theirs.get("allocated_size").i64(),
            );
            check.eq(
                "data_size",
                extent.real_size.cast_signed(),
                theirs.get("data_size").i64(),
            );
            check.eq(
                "initialized_size",
                extent.initialized_size.cast_signed(),
                theirs.get("initialized_size").i64(),
            );
            let compressed = theirs.get("compressed_size");
            check.eq(
                "compressed_size",
                extent.total_allocated.map(u64::cast_signed),
                (!compressed.is_null()).then(|| compressed.i64()),
            );
            check.eq(
                "sizes offered only at VCN 0",
                extent.sizes().is_some(),
                theirs.get("lowest_vcn").i64() == 0,
            );

            runs.clear();
            let decoded = extent.decode_runs(runs);
            check.eq("runs appended", decoded.runs, runs.len());
            let expected = theirs.get("runs").arr();
            check.eq("number of runs", runs.len(), expected.len());
            let mut vcn = extent.start_vcn.cast_signed();
            for (run, want) in runs.iter().zip(expected) {
                check.counts.runs += 1;
                let lcn = want.get("lcn");
                check.eq("run vcn", vcn, want.get("vcn").i64());
                check.eq(
                    "run lcn",
                    run.lcn.map(u64::cast_signed),
                    (!lcn.is_null()).then(|| lcn.i64()),
                );
                check.eq(
                    "run length",
                    run.length.cast_signed(),
                    want.get("length").i64(),
                );
                vcn += run.length.cast_signed();
            }
            // The inspector's "runlist_ok": terminated, and covering what
            // the header announces when it announces anything.
            let covers = runs.is_empty()
                || extent.last_vcn < extent.start_vcn.cast_signed()
                || i128::from(decoded.clusters)
                    == i128::from(extent.last_vcn) - i128::from(extent.start_vcn) + 1;
            let ok = decoded.status == RunlistStatus::Complete && covers;
            check.eq("runlist_ok", ok, theirs.get("runlist_ok").bool());
            sound &= ok;
        }
    }
    sound
}

fn compare_record(
    check: &mut Check,
    image: &[u8],
    offset: usize,
    theirs: &Json,
    scratch: &mut Vec<u8>,
    runs: &mut Vec<Run>,
) {
    let valid = theirs.get("valid").bool();
    let sector = &image[offset..offset + SECTOR_SIZE];
    let Some(probe) = probe_file_record(sector, MAX_RECORD_SIZE) else {
        check.counts.records_invalid += 1;
        check.eq("record refused by the probe", false, valid);
        return;
    };
    check.eq(
        "record_size",
        u64::from(probe.record_size),
        theirs.get("record_size").u64(),
    );
    check.eq(
        "usa_offset",
        u64::from(probe.usa_offset),
        theirs.get("usa_offset").u64(),
    );
    check.eq(
        "usa_count",
        u64::from(probe.usa_count),
        theirs.get("usa_count").u64(),
    );
    let Some(raw) = image.get(offset..offset + probe.record_size as usize) else {
        check.counts.records_invalid += 1;
        check.eq("record cut off by the end of the image", false, valid);
        return;
    };
    let hash = fnv1a64(raw);
    scratch.clear();
    scratch.extend_from_slice(raw);
    let record = match parse_file_record(scratch) {
        Ok(record) => record,
        Err(error) => {
            check.counts.records_invalid += 1;
            check.eq(&format!("record refused ({error})"), false, valid);
            return;
        }
    };
    check.counts.records += 1;
    check.eq("hash is of the bytes as read", fnv1a64(raw), hash);

    let header = record.header();
    check.eq(
        "bytes_in_use",
        u64::from(header.used_size),
        theirs.get("bytes_in_use").u64(),
    );
    check.eq(
        "allocated size",
        u64::from(header.allocated_size),
        theirs.get("record_size").u64(),
    );
    check.eq("lsn", header.lsn, theirs.get("lsn").u64());
    check.eq(
        "sequence",
        u64::from(header.sequence),
        theirs.get("sequence").u64(),
    );
    check.eq(
        "link_count",
        u64::from(header.link_count),
        theirs.get("link_count").u64(),
    );
    check.eq(
        "attrs_offset",
        u64::from(header.attrs_offset),
        theirs.get("attrs_offset").u64(),
    );
    check.eq("flags", u64::from(header.flags), theirs.get("flags").u64());
    check.eq("in_use", record.is_in_use(), theirs.get("in_use").bool());
    check.eq(
        "is_directory",
        record.is_directory(),
        theirs.get("is_directory").bool(),
    );
    check.eq(
        "base_record",
        header.base.number(),
        theirs.get("base_record").u64(),
    );
    check.eq(
        "base_sequence",
        u64::from(header.base.sequence()),
        theirs.get("base_sequence").u64(),
    );
    check.eq(
        "next_attr_id",
        u64::from(header.next_attr_id),
        theirs.get("next_attr_id").u64(),
    );
    let number = theirs.get("record_number");
    check.eq(
        "record_number",
        header.record_number.map(u64::from),
        (!number.is_null()).then(|| number.u64()),
    );
    compare_fixup(check, record.fixup(), theirs);

    let expected = theirs.get("attributes").arr();
    let mut sound = record.attr_chain().is_complete();
    let mut walked = 0u32;
    for (index, mine) in record.attributes().enumerate() {
        walked += 1;
        match expected.get(index) {
            Some(want) => sound &= compare_attribute(check, &record, &mine, want, runs),
            None => {
                // The inspector stops at faults this crate steps over.
                sound = false;
                if valid {
                    check.fail(format!("attribute {index} unknown to the inspector"));
                }
            }
        }
    }
    check.eq("attribute_count", record.attribute_count(), walked);
    if valid {
        check.eq("number of attributes", walked as usize, expected.len());
    }
    check.eq(
        "never used",
        record.is_never_used(),
        !theirs.get("in_use").bool() && walked == 0,
    );

    // The summaries the scanner works from: names and data streams.
    let names = theirs.get("names").arr();
    let mut mine_names = record.file_names();
    for want in names {
        match mine_names.next() {
            Some(name) => compare_name(check, &name, want),
            None => check.fail("file name missing".into()),
        }
    }
    if valid {
        check.eq("further names", mine_names.next().is_some(), false);
    }
    let streams = theirs.get("data").arr();
    let mut mine_streams = record
        .attributes()
        .filter(|attr| attr.type_code == attr_type::DATA);
    for want in streams {
        let Some(stream) = mine_streams.next() else {
            check.fail("$DATA attribute missing".into());
            break;
        };
        check.counts.data_attributes += 1;
        check.eq(
            "stream name",
            units(stream.name),
            want.get("name").units().to_vec(),
        );
        check.eq(
            "stream attr_id",
            u64::from(stream.id),
            want.get("attr_id").u64(),
        );
        check.eq(
            "stream flags",
            u64::from(stream.flags),
            want.get("flags").u64(),
        );
        check.eq(
            "stream compressed",
            stream.is_compressed(),
            want.get("compressed").bool() && want.get("non_resident").bool(),
        );
        check.eq(
            "stream encrypted",
            stream.is_encrypted(),
            want.get("encrypted").bool(),
        );
        check.eq(
            "stream sparse",
            stream.is_sparse(),
            want.get("sparse").bool(),
        );
        check.eq(
            "stream unnamed",
            stream.is_unnamed(),
            want.get("name").units().is_empty(),
        );
    }
    if valid {
        check.eq(
            "further $DATA attributes",
            mine_streams.next().is_some(),
            false,
        );
    }

    if sound != valid {
        check.fail(format!(
            "verdict differs: hds-ntfs sound = {sound}, inspector valid = {valid}, errors {:?}",
            theirs.get("errors")
        ));
    }
}

fn compare_index_block(
    check: &mut Check,
    image: &[u8],
    offset: usize,
    theirs: &Json,
    scratch: &mut Vec<u8>,
) {
    let valid = theirs.get("valid").bool();
    let sector = &image[offset..offset + SECTOR_SIZE];
    let Some(probe) = probe_index_block(sector) else {
        check.counts.index_blocks_invalid += 1;
        check.eq("index block refused by the probe", false, valid);
        return;
    };
    check.eq(
        "block_size",
        u64::from(probe.block_size),
        theirs.get("block_size").u64(),
    );
    check.eq(
        "usa_offset",
        u64::from(probe.usa_offset),
        theirs.get("usa_offset").u64(),
    );
    check.eq(
        "usa_count",
        u64::from(probe.usa_count),
        theirs.get("usa_count").u64(),
    );
    let Some(raw) = image.get(offset..offset + probe.block_size as usize) else {
        check.counts.index_blocks_invalid += 1;
        check.eq("index block cut off by the end of the image", false, valid);
        return;
    };
    scratch.clear();
    scratch.extend_from_slice(raw);
    let block = match parse_index_block(scratch) {
        Ok(block) => block,
        Err(error) => {
            check.counts.index_blocks_invalid += 1;
            check.eq(&format!("index block refused ({error})"), false, valid);
            return;
        }
    };
    check.counts.index_blocks += 1;
    check.eq(
        "size",
        u64::from(block.size()),
        theirs.get("block_size").u64(),
    );
    check.eq("lsn", block.lsn(), theirs.get("lsn").u64());
    check.eq("vcn", block.vcn().cast_signed(), theirs.get("vcn").i64());
    check.eq(
        "entries_offset",
        u64::from(block.node().entries_offset),
        theirs.get("entries_offset").u64(),
    );
    check.eq(
        "index_length",
        u64::from(block.node().used_size),
        theirs.get("index_length").u64(),
    );
    check.eq(
        "allocated_size",
        u64::from(block.node().allocated_size),
        theirs.get("allocated_size").u64(),
    );
    check.eq(
        "node_flags",
        u64::from(block.node().flags),
        theirs.get("node_flags").u64(),
    );
    check.eq(
        "is_leaf",
        !block.node().has_subnodes(),
        theirs.get("is_leaf").bool(),
    );
    compare_fixup(check, block.fixup(), theirs);
    let complete = compare_entries(check, block.entries(), theirs.get("entries"), false);

    // In a sound directory block every entry names the same parent.
    let mut parents: Vec<_> = block
        .entries()
        .map(|entry| entry.file_name.parent)
        .collect();
    parents.dedup();
    if parents.len() == 1 {
        check.eq("owner", block.owner(), parents.first().copied());
    }
    if complete != valid {
        check.fail(format!(
            "verdict differs: hds-ntfs complete = {complete}, inspector valid = {valid}, errors {:?}",
            theirs.get("errors")
        ));
    }
}

/// Walks every 512-byte offset of `image`, parses what is there with this
/// crate and compares it with the inspector's document `doc` for the same
/// bytes. Returns what was compared and every disagreement.
pub fn cross_check(name: &str, image: &[u8], doc: &Json) -> (Counts, Vec<String>) {
    assert_eq!(
        doc.get("tool").units(),
        "inspect-ntfs".encode_utf16().collect::<Vec<_>>()
    );
    assert_eq!(
        doc.get("format").int(),
        1,
        "unknown inspector output format"
    );
    assert_eq!(doc.get("scan_start").u64(), 0);
    assert_eq!(
        doc.get("scan_end").u64(),
        (image.len() / SECTOR_SIZE * SECTOR_SIZE) as u64
    );
    let boots = by_offset(doc.get("boot_sectors"));
    let records = by_offset(doc.get("file_records"));
    let blocks = by_offset(doc.get("index_blocks"));

    let mut check = Check {
        image: name.to_string(),
        context: String::new(),
        counts: Counts {
            images: 1,
            ..Counts::default()
        },
        failures: Vec::new(),
    };
    let mut scratch = Vec::new();
    let mut runs = Vec::new();
    let (mut seen_boots, mut seen_records, mut seen_blocks) = (0, 0, 0);
    for offset in (0..image.len() / SECTOR_SIZE).map(|sector| sector * SECTOR_SIZE) {
        check.counts.sectors += 1;
        let sector = &image[offset..offset + SECTOR_SIZE];
        let key = offset as u64;

        check.context = format!("BOOT @{offset:#x}");
        match (is_boot_sector_candidate(sector), boots.get(&key)) {
            (true, Some(theirs)) => {
                seen_boots += 1;
                compare_boot(&mut check, sector, theirs);
            }
            (false, None) => {}
            (mine, theirs) => check.eq("boot sector signature", mine, theirs.is_some()),
        }

        check.context = format!("FILE @{offset:#x}");
        match (sector.starts_with(&FILE_MAGIC), records.get(&key)) {
            (true, Some(theirs)) => {
                seen_records += 1;
                compare_record(&mut check, image, offset, theirs, &mut scratch, &mut runs);
            }
            (false, None) => {
                // Not counted as a compared field: this is every sector.
                if probe_file_record(sector, MAX_RECORD_SIZE).is_some() {
                    check.fail("probe accepts a sector without the FILE magic".into());
                }
            }
            (mine, theirs) => check.eq("FILE magic", mine, theirs.is_some()),
        }

        check.context = format!("INDX @{offset:#x}");
        match (sector.starts_with(&INDX_MAGIC), blocks.get(&key)) {
            (true, Some(theirs)) => {
                seen_blocks += 1;
                compare_index_block(&mut check, image, offset, theirs, &mut scratch);
            }
            (false, None) => {
                if probe_index_block(sector).is_some() {
                    check.fail("probe accepts a sector without the INDX magic".into());
                }
            }
            (mine, theirs) => check.eq("INDX magic", mine, theirs.is_some()),
        }
    }
    check.context = "totals".into();
    check.eq("boot sectors seen", seen_boots, boots.len());
    check.eq("FILE records seen", seen_records, records.len());
    check.eq("INDX blocks seen", seen_blocks, blocks.len());
    (check.counts, check.failures)
}
