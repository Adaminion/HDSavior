//! Ground truth from the fixture manifests.
//!
//! `scripts/linux/make-fixtures.sh` writes next to every image a manifest
//! of what the ntfs-3g driver itself reported for each file and folder
//! before the accident: record number and sequence, the three times set
//! through the driver, names and namespaces, sizes, the complete runlist,
//! the extension records. This test runs when `HDS_FIXTURES` is set and
//! checks, for every entry whose MFT record survived, that this crate reads
//! the same facts from the raw image. It is independent of
//! `inspect-ntfs.py` (see `fixtures.rs` for that comparison), and it is the
//! one place where `$ATTRIBUTE_LIST` content written by a real driver is
//! walked: the lists are non-resident, so they are read through their
//! runs with the old volume's geometry.

mod common;

use std::collections::HashMap;
use std::path::Path;

use common::json::Json;
use hds_ntfs::{
    attr_type, filetime_to_unix_ms, parse_file_record, probe_file_record, AttrBody,
    AttrListEntries, Attribute, AttributeList, Extent, FileRecord, FileRef, FixupStatus,
    RunlistStatus, WalkEnd, I30, MAX_RECORD_SIZE, SECTOR_SIZE,
};

#[derive(Default, Debug)]
struct Counts {
    images: u64,
    entries: u64,
    live: u64,
    deleted: u64,
    names: u64,
    parents: u64,
    times: u64,
    resident_files: u64,
    non_resident_files: u64,
    runs: u64,
    directories: u64,
    extension_records: u64,
    attribute_lists: u64,
    attribute_list_entries: u64,
    named_streams: u64,
}

/// Every `FILE` record of an image, fixed up, by record number.
struct Records {
    bytes: Vec<Vec<u8>>,
    by_number: HashMap<u32, Vec<usize>>,
    by_offset: HashMap<usize, usize>,
}

impl Records {
    fn scan(image: &[u8]) -> Records {
        let mut records = Records {
            bytes: Vec::new(),
            by_number: HashMap::new(),
            by_offset: HashMap::new(),
        };
        for offset in (0..image.len() / SECTOR_SIZE).map(|sector| sector * SECTOR_SIZE) {
            let sector = &image[offset..offset + SECTOR_SIZE];
            let Some(probe) = probe_file_record(sector, MAX_RECORD_SIZE) else {
                continue;
            };
            let Some(raw) = image.get(offset..offset + probe.record_size as usize) else {
                continue;
            };
            let mut bytes = raw.to_vec();
            let Ok(record) = parse_file_record(&mut bytes) else {
                continue;
            };
            assert_eq!(
                record.fixup(),
                FixupStatus::Ok,
                "torn record at {offset:#x}"
            );
            let number = record.record_number();
            let index = records.bytes.len();
            records.bytes.push(bytes);
            records.by_offset.insert(offset, index);
            if let Some(number) = number {
                records.by_number.entry(number).or_default().push(index);
            }
        }
        records
    }

    /// The record at `index`, parsed again from its fixed-up bytes.
    fn get(&self, index: usize) -> FileRecord<'_> {
        hds_ntfs::parse_file_record_fixed(&self.bytes[index], FixupStatus::Ok).unwrap()
    }

    /// The in-use extension record `number` of the base record `base`.
    fn extension(&self, number: u32, base: FileRef) -> Option<FileRecord<'_>> {
        let mut found = self
            .by_number
            .get(&number)?
            .iter()
            .map(|index| self.get(*index))
            .filter(|record| record.is_in_use() && record.base() == base);
        let record = found.next()?;
        assert!(
            found.next().is_none(),
            "extension record {number} found twice"
        );
        Some(record)
    }
}

fn namespace_code(name: &[u16]) -> u8 {
    match String::from_utf16_lossy(name).as_str() {
        "POSIX" => 0,
        "WIN32" => 1,
        "DOS" => 2,
        "WIN32+DOS" => 3,
        other => panic!("unknown namespace {other:?} in the manifest"),
    }
}

fn is(value: &Json, text: &str) -> bool {
    value.units().iter().copied().eq(text.encode_utf16())
}

/// The bytes a non-resident attribute's runs point at, cut to its real
/// size. `None` when the runlist is faulty, has a hole or leaves the image.
fn read_through_runs(
    image: &[u8],
    volume_offset: u64,
    cluster_size: u64,
    extent: &Extent<'_>,
) -> Option<Vec<u8>> {
    let mut content = Vec::new();
    let mut runs = extent.runs();
    for run in &mut runs {
        let start = usize::try_from(volume_offset + run.lcn? * cluster_size).ok()?;
        let length = usize::try_from(run.length * cluster_size).ok()?;
        content.extend_from_slice(image.get(start..start + length)?);
    }
    if runs.status() != Some(RunlistStatus::Complete) {
        return None;
    }
    content.truncate(usize::try_from(extent.sizes()?.real).ok()?);
    Some(content)
}

struct Volume<'a> {
    image: &'a [u8],
    records: &'a Records,
    offset: u64,
    cluster_size: u64,
    /// Path (UTF-16) of every live folder to its reference.
    folders: HashMap<Vec<u16>, FileRef>,
}

impl Volume<'_> {
    fn check_entry(&self, entry: &Json, counts: &mut Counts) {
        let path = String::from_utf16_lossy(entry.get("path").units());
        let ondisk = entry.get("ondisk");
        let (Some(record_offset), true) = (
            ondisk.opt("record_offset"),
            matches!(
                entry.get("after").opt("record_intact"),
                Some(Json::Bool(true))
            ),
        ) else {
            return;
        };
        counts.entries += 1;
        // Absolute in the image, unlike cluster numbers.
        let offset = usize::try_from(record_offset.u64()).unwrap();
        let index = *self
            .records
            .by_offset
            .get(&offset)
            .unwrap_or_else(|| panic!("{path}: no parsable record at {offset:#x}"));
        let base = self.records.get(index);
        let number = u32::try_from(entry.get("mft_record").u64()).unwrap();
        let is_dir = is(entry.get("type"), "dir");
        let deleted = entry.get("deleted").bool();

        assert_eq!(base.record_number(), Some(number), "{path}: record number");
        assert!(!base.is_extension(), "{path}: base record expected");
        assert!(base.attr_chain().is_complete(), "{path}: attribute chain");
        assert_eq!(base.is_directory(), is_dir, "{path}: directory flag");
        let sequence = entry.get("mft_sequence");
        if deleted {
            // Freed: not in use, sequence bumped once, directory bit kept.
            counts.deleted += 1;
            assert!(!base.is_in_use(), "{path}: deleted but in use");
            if !sequence.is_null() {
                let reference =
                    FileRef::new(u64::from(number), u16::try_from(sequence.u64()).unwrap());
                assert!(
                    reference.resolves_to(base.sequence(), false),
                    "{path}: reference {reference} does not resolve to the freed record (sequence {})",
                    base.sequence()
                );
                assert!(!reference.resolves_to(base.sequence(), true));
            }
            self.check_data(&path, entry, &[base], counts);
            return;
        }
        counts.live += 1;
        assert!(base.is_in_use(), "{path}: live but not in use");
        let reference = FileRef::new(u64::from(number), u16::try_from(sequence.u64()).unwrap());
        assert!(
            reference.resolves_to(base.sequence(), true),
            "{path}: sequence"
        );
        assert!(!reference.resolves_to(base.sequence(), false));

        // The three times that were set through the driver, to the tick.
        let si = base
            .standard_information()
            .unwrap_or_else(|| panic!("{path}: no $STANDARD_INFORMATION"));
        assert_eq!(
            si.times.created,
            entry.get("created").u64(),
            "{path}: created"
        );
        assert_eq!(
            si.times.modified,
            entry.get("modified").u64(),
            "{path}: modified"
        );
        assert_eq!(
            si.times.accessed,
            entry.get("accessed").u64(),
            "{path}: accessed"
        );
        for (time, key) in [
            (si.times.created, "created_unix_ms"),
            (si.times.modified, "modified_unix_ms"),
            (si.times.accessed, "accessed_unix_ms"),
        ] {
            assert_eq!(
                filetime_to_unix_ms(time),
                Some(entry.get(key).i64()),
                "{path}: {key}"
            );
            counts.times += 1;
        }

        // The base record and its extension records.
        let mut group = vec![base];
        for extension in ondisk.get("extension_records").arr() {
            let extension = u32::try_from(extension.u64()).unwrap();
            let record = self
                .records
                .extension(extension, reference)
                .unwrap_or_else(|| panic!("{path}: extension record {extension} not found"));
            assert!(
                !record.is_directory(),
                "{path}: extension with the directory flag"
            );
            assert_eq!(record.link_count(), 0, "{path}: extension with links");
            assert!(record.attr_chain().is_complete());
            counts.extension_records += 1;
            group.push(record);
        }

        // Names: the same (holder record, name, namespace) triples.
        let mut expected: Vec<(u32, Vec<u16>, u8)> = ondisk
            .get("names")
            .arr()
            .iter()
            .map(|name| {
                (
                    u32::try_from(name.get("record").u64()).unwrap(),
                    name.get("name").units().to_vec(),
                    namespace_code(name.get("namespace").units()),
                )
            })
            .collect();
        let mut found = Vec::new();
        let mut parents = Vec::new();
        for record in &group {
            for name in record.file_names() {
                let units: Vec<u16> = name.name.units().collect();
                found.push((
                    record.record_number().unwrap(),
                    units.clone(),
                    name.namespace.as_u8(),
                ));
                parents.push((units, name.parent));
                assert_eq!(
                    name.is_directory(),
                    is_dir,
                    "{path}: directory flag of the name"
                );
            }
        }
        expected.sort();
        found.sort();
        assert_eq!(found, expected, "{path}: names");
        counts.names += found.len() as u64;
        assert_eq!(
            usize::from(base.link_count()),
            found.len(),
            "{path}: link count is the number of names"
        );

        // The name the path ends with points at the folder the path names.
        let path_units = entry.get("path").units();
        let (folder, leaf) = match path_units.iter().rposition(|unit| *unit == u16::from(b'/')) {
            Some(slash) => (Some(&path_units[..slash]), &path_units[slash + 1..]),
            None => (None, path_units),
        };
        let parent = match folder {
            None => Some(FileRef::new(5, 5)),
            Some(folder) => self.folders.get(folder).copied(),
        };
        if let Some(parent) = parent {
            assert!(
                parents
                    .iter()
                    .any(|(name, found)| name == leaf && *found == parent),
                "{path}: no name {:?} with parent {parent} among {parents:?}",
                String::from_utf16_lossy(leaf)
            );
            counts.parents += 1;
        }

        if is_dir {
            counts.directories += 1;
            let attributes: Vec<Attribute<'_>> =
                group.iter().flat_map(|r| r.attributes()).collect();
            let root = attributes
                .iter()
                .find(|a| a.type_code == attr_type::INDEX_ROOT && a.is_named(I30))
                .and_then(|a| a.index_root())
                .unwrap_or_else(|| panic!("{path}: no $INDEX_ROOT:$I30"));
            assert_eq!(root.indexed_type, attr_type::FILE_NAME);
            assert_eq!(root.collation, 1);
            let mut entries = root.entries();
            for index_entry in &mut entries {
                assert_eq!(
                    index_entry.file_name.parent, reference,
                    "{path}: index entry parent"
                );
            }
            assert_eq!(
                entries.end(),
                Some(WalkEnd::Complete),
                "{path}: index root walk"
            );
            assert_eq!(
                attributes
                    .iter()
                    .any(|a| a.type_code == attr_type::INDEX_ALLOCATION && a.is_named(I30)),
                is(ondisk.get("index"), "non-resident"),
                "{path}: $INDEX_ALLOCATION"
            );
        } else {
            self.check_data(&path, entry, &group, counts);
        }

        if is(ondisk.get("attribute_list"), "non-resident") {
            self.check_attribute_list(&path, &group, counts);
        } else {
            assert!(
                group[0].find(attr_type::ATTRIBUTE_LIST, "").is_none(),
                "{path}: unexpected $ATTRIBUTE_LIST"
            );
        }
    }

    /// The unnamed `$DATA` of a file against the manifest's size, form,
    /// flags and complete runlist.
    fn check_data(&self, path: &str, entry: &Json, group: &[FileRecord<'_>], counts: &mut Counts) {
        let ondisk = entry.get("ondisk");
        let Some(resident) = ondisk.opt("resident") else {
            return; // folders, and deleted entries the builder did not look into
        };
        let size = entry.get("size").u64();
        let mut streams: Vec<Attribute<'_>> = group
            .iter()
            .flat_map(|record| record.attributes())
            .filter(|attr| attr.type_code == attr_type::DATA && attr.is_unnamed())
            .collect();
        assert!(!streams.is_empty(), "{path}: no unnamed $DATA");
        assert!(
            streams.iter().all(|attr| !attr.is_malformed()),
            "{path}: malformed $DATA"
        );

        if let Some(names) = entry.opt("streams") {
            let mut expected: Vec<Vec<u16>> =
                names.arr().iter().map(|n| n.units().to_vec()).collect();
            let mut found: Vec<Vec<u16>> = group
                .iter()
                .flat_map(|record| record.attributes())
                .filter(|attr| attr.type_code == attr_type::DATA && !attr.is_unnamed())
                .map(|attr| attr.name.units().collect())
                .collect();
            expected.sort();
            found.sort();
            found.dedup();
            assert_eq!(found, expected, "{path}: named streams");
            counts.named_streams += found.len() as u64;
        }

        if resident.bool() {
            counts.resident_files += 1;
            assert_eq!(streams.len(), 1, "{path}: one resident $DATA");
            let value = streams[0]
                .resident()
                .unwrap_or_else(|| panic!("{path}: not resident"));
            assert_eq!(u64::from(value.len()), size, "{path}: resident size");
            assert!(
                !streams[0].is_compressed(),
                "{path}: resident data is never compressed"
            );
            return;
        }
        counts.non_resident_files += 1;
        streams.sort_by_key(|attr| attr.extent().map(|extent| extent.start_vcn));
        let flags: Vec<String> = ondisk
            .get("data_flags")
            .arr()
            .iter()
            .map(|flag| String::from_utf16_lossy(flag.units()))
            .collect();
        let mut next_vcn = 0u64;
        let mut runs: Vec<(Option<u64>, u64)> = Vec::new();
        for (piece, attr) in streams.iter().enumerate() {
            let extent = attr
                .extent()
                .unwrap_or_else(|| panic!("{path}: not non-resident"));
            assert_eq!(
                extent.start_vcn, next_vcn,
                "{path}: extent {piece} does not continue the chain"
            );
            assert_eq!(
                attr.is_compressed(),
                flags.iter().any(|f| f == "compressed"),
                "{path}: compressed"
            );
            assert_eq!(
                attr.is_sparse(),
                flags.iter().any(|f| f == "sparse"),
                "{path}: sparse"
            );
            let mut decoder = extent.runs();
            for run in &mut decoder {
                runs.push((run.lcn, run.length));
            }
            assert_eq!(
                decoder.status(),
                Some(RunlistStatus::Complete),
                "{path}: runlist"
            );
            next_vcn += decoder.clusters();
            assert_eq!(
                i128::from(extent.last_vcn),
                i128::from(next_vcn) - 1,
                "{path}: last VCN of extent {piece}"
            );
            match extent.sizes() {
                Some(sizes) => {
                    // Only the extent with VCN 0 carries the sizes.
                    assert_eq!(piece, 0);
                    assert_eq!(sizes.real, size, "{path}: real size");
                    assert_eq!(
                        sizes.allocated,
                        ondisk.get("allocated_size").u64(),
                        "{path}: allocated"
                    );
                    assert_eq!(
                        sizes.initialized,
                        ondisk.get("initialized_size").u64(),
                        "{path}: initialized"
                    );
                    assert_eq!(
                        extent.total_allocated,
                        ondisk.opt("compressed_size").map(Json::u64),
                        "{path}: total allocated"
                    );
                    if let Some(unit) = ondisk.opt("compression_unit") {
                        assert_eq!(u64::from(extent.compression_unit), unit.u64());
                    }
                }
                None => {
                    assert_ne!(piece, 0, "{path}: first extent without sizes");
                    assert_eq!(
                        (
                            extent.allocated_size,
                            extent.real_size,
                            extent.initialized_size
                        ),
                        (0, 0, 0),
                        "{path}: sizes of a later extent"
                    );
                }
            }
        }
        let expected: Vec<(Option<u64>, u64)> = ondisk
            .get("runs")
            .arr()
            .iter()
            .map(|run| {
                let lcn = &run.arr()[0];
                ((!lcn.is_null()).then(|| lcn.u64()), run.arr()[1].u64())
            })
            .collect();
        assert_eq!(coalesce(&runs), coalesce(&expected), "{path}: runs");
        counts.runs += runs.len() as u64;
    }

    /// A non-resident `$ATTRIBUTE_LIST`, read through its runs: it must
    /// name every attribute of the base record and its extension records
    /// except itself, each with the right type, name, id and start VCN.
    fn check_attribute_list(&self, path: &str, group: &[FileRecord<'_>], counts: &mut Counts) {
        let list = group[0]
            .find(attr_type::ATTRIBUTE_LIST, "")
            .unwrap_or_else(|| panic!("{path}: no $ATTRIBUTE_LIST in the base record"));
        let Some(AttributeList::NonResident(extent)) = list.attribute_list() else {
            panic!("{path}: $ATTRIBUTE_LIST is not non-resident");
        };
        let content = read_through_runs(self.image, self.offset, self.cluster_size, &extent)
            .unwrap_or_else(|| panic!("{path}: cannot read the attribute list"));
        let mut entries = AttrListEntries::new(&content);
        let mut listed = 0usize;
        for entry in &mut entries {
            listed += 1;
            let holder = group
                .iter()
                .find(|record| {
                    u64::from(record.record_number().unwrap()) == entry.holder.number()
                        && record.sequence() == entry.holder.sequence()
                })
                .unwrap_or_else(|| panic!("{path}: list names unknown record {}", entry.holder));
            let attr = holder
                .attributes()
                .find(|attr| attr.id == entry.attr_id)
                .unwrap_or_else(|| {
                    panic!(
                        "{path}: no attribute id {} in {}",
                        entry.attr_id, entry.holder
                    )
                });
            assert_eq!(attr.type_code, entry.type_code, "{path}: listed type");
            assert_eq!(attr.name, entry.name, "{path}: listed name");
            let start_vcn = match attr.body {
                AttrBody::NonResident(extent) => extent.start_vcn,
                _ => 0,
            };
            assert_eq!(start_vcn, entry.start_vcn, "{path}: listed start VCN");
        }
        assert_eq!(
            entries.end(),
            Some(WalkEnd::Complete),
            "{path}: attribute list walk"
        );
        let attributes: usize = group
            .iter()
            .map(|record| record.attribute_count() as usize)
            .sum();
        assert_eq!(
            listed,
            attributes - 1,
            "{path}: one list entry per attribute but the list"
        );
        counts.attribute_lists += 1;
        counts.attribute_list_entries += listed as u64;
    }
}

/// Merges neighbours that continue each other, so that a runlist split
/// over extents compares equal to the same list written in one piece.
fn coalesce(runs: &[(Option<u64>, u64)]) -> Vec<(Option<u64>, u64)> {
    let mut merged: Vec<(Option<u64>, u64)> = Vec::new();
    for &(lcn, length) in runs {
        match merged.last_mut() {
            Some((Some(last), last_length)) if lcn == Some(*last + *last_length) => {
                *last_length += length
            }
            Some((None, last_length)) if lcn.is_none() => *last_length += length,
            _ => merged.push((lcn, length)),
        }
    }
    merged
}

fn check_image(image_path: &Path, manifest_path: &Path) -> Counts {
    let image = std::fs::read(image_path).unwrap();
    let manifest = Json::parse(&std::fs::read(manifest_path).unwrap())
        .unwrap_or_else(|e| panic!("{}: {e}", manifest_path.display()));
    assert_eq!(
        manifest.get("manifest_version").int(),
        1,
        "unknown manifest version"
    );
    let old = manifest.get("old_volume");
    let records = Records::scan(&image);
    let entries = manifest.get("entries").arr();
    let folders = entries
        .iter()
        .filter(|entry| is(entry.get("type"), "dir") && !entry.get("deleted").bool())
        .map(|entry| {
            (
                entry.get("path").units().to_vec(),
                FileRef::new(
                    entry.get("mft_record").u64(),
                    u16::try_from(entry.get("mft_sequence").u64()).unwrap(),
                ),
            )
        })
        .collect();
    let volume = Volume {
        image: &image,
        records: &records,
        offset: old.get("offset").u64(),
        cluster_size: old.get("cluster_size").u64(),
        folders,
    };
    let mut counts = Counts {
        images: 1,
        ..Counts::default()
    };
    for entry in entries {
        volume.check_entry(entry, &mut counts);
    }
    counts
}

#[test]
fn surviving_records_say_what_the_driver_said() {
    let Some(dir) = common::fixtures_dir("surviving_records_say_what_the_driver_said") else {
        return;
    };
    let mut total = Counts::default();
    for image in common::fixture_images(&dir) {
        let manifest = image.with_extension("manifest.json");
        if !manifest.is_file() {
            eprintln!("{}: no manifest, skipped", image.display());
            continue;
        }
        let counts = check_image(&image, &manifest);
        eprintln!("{}: {counts:?}", image.display());
        total.images += counts.images;
        total.entries += counts.entries;
        total.live += counts.live;
        total.deleted += counts.deleted;
        total.names += counts.names;
        total.parents += counts.parents;
        total.times += counts.times;
        total.resident_files += counts.resident_files;
        total.non_resident_files += counts.non_resident_files;
        total.runs += counts.runs;
        total.directories += counts.directories;
        total.extension_records += counts.extension_records;
        total.attribute_lists += counts.attribute_lists;
        total.attribute_list_entries += counts.attribute_list_entries;
        total.named_streams += counts.named_streams;
    }
    eprintln!("TOTAL: {total:?}");
    assert!(total.entries > 0, "no manifest entry was checked");
}
