//! What the multi-record and name scenarios promise in their
//! documentation, checked on the image and the manifest they return.

mod common;

use common::{
    assert_reads_back, build, expected_path, intact_records, must, must_not, read_structure, shown,
    SEEDS,
};
use hds_testkit::disk::Built;
use hds_testkit::facts::{ArtifactState, ExtentFact, RecordFact, RecordRole};
use hds_testkit::manifest::{Entry, EntryKind, Flag, Manifest, Presence, RecordState};
use hds_testkit::ondisk::runlist::Run;
use hds_testkit::ondisk::FileRef;

/// Runs `check` on the scenario built with every test seed.
fn each(name: &str, check: impl Fn(&Built)) {
    for seed in SEEDS {
        eprintln!("{name}, seed {seed}");
        check(&build(name, seed));
    }
}

/// The record that the old volume last wrote into `slot`, if it survives.
fn record(m: &Manifest, slot: u64) -> &RecordFact {
    let id = m.instance("old").id;
    m.records
        .iter()
        .find(|r| {
            r.instance == id
                && r.slot == slot
                && r.state == ArtifactState::Intact
                && r.role != RecordRole::Mirror
                && r.role != RecordRole::StaleCopy
        })
        .unwrap_or_else(|| panic!("no intact record in slot {slot} of the old volume"))
}

/// The unnamed `$DATA` extents of an entry as its surviving records hold
/// them, ordered by start VCN (ALGORITHMS E2).
fn data_chain<'a>(m: &'a Manifest, e: &Entry) -> Vec<(u64, &'a ExtentFact)> {
    let id = e.instance;
    let mut chain: Vec<(u64, &ExtentFact)> = m
        .records
        .iter()
        .filter(|r| {
            r.instance == id && r.state == ArtifactState::Intact && r.role != RecordRole::StaleCopy
        })
        .filter(|r| {
            (r.slot == e.record && r.base == FileRef::NONE)
                || r.base == FileRef::new(e.record, e.sequence)
        })
        .flat_map(|r| {
            r.data
                .iter()
                .filter(|d| d.name.is_empty())
                .filter_map(move |d| Some((r.slot, d.non_resident.as_ref()?)))
        })
        .collect();
    chain.sort_by_key(|(_, x)| x.lowest_vcn);
    chain
}

/// Asserts that the extents in the records form one gapless chain that
/// equals the manifest's extent list.
#[track_caller]
fn assert_chain_complete(m: &Manifest, e: &Entry) {
    let chain = data_chain(m, e);
    let mut next = 0i64;
    let mut runs: Vec<Run> = Vec::new();
    for (slot, x) in &chain {
        assert_eq!(
            x.lowest_vcn, next,
            "{}: the extent in record {slot} does not continue the chain",
            e.path
        );
        next += x.runs.iter().map(|r| r.len as i64).sum::<i64>();
        runs.extend(x.runs.iter().copied());
    }
    let listed: Vec<Run> = e
        .extents
        .iter()
        .map(|x| Run {
            lcn: x.lcn,
            len: x.clusters,
        })
        .collect();
    assert_eq!(runs, listed, "{}", e.path);
}

fn test_file<'a>(m: &'a Manifest, name: &str) -> &'a Entry {
    m.entry("old", &format!("Test/{name}"))
}

#[test]
fn data_split_over_extension_records() {
    for (name, resident_list) in [
        ("attribute_list_resident", true),
        ("attribute_list_nonresident", false),
    ] {
        each(name, |built| {
            let m = &built.manifest;
            let cs = 4096;
            // (file, runs per extent in VCN order, holder of the first extent is the base)
            for (file, pieces, first_in_base) in [
                ("windows-style.bin", vec![4, 4, 4], false),
                ("first-extent-in-base.bin", vec![3, 3, 3], true),
                ("uneven.bin", vec![1, 5, 1], false),
            ] {
                let e = test_file(m, file);
                assert!(shown(e) && e.must_recover_exactly(), "{file}");
                assert_eq!(expected_path(e), e.path);
                for flag in [Flag::MissingFragments, Flag::SizeEstimated, Flag::Deleted] {
                    must_not(e, flag);
                }
                let base = record(m, e.record);
                let list = base.attribute_list.as_ref().expect("an attribute list");
                assert_eq!(list.resident, resident_list, "{file}");
                let chain = data_chain(m, e);
                assert_eq!(
                    chain.iter().map(|(_, x)| x.runs.len()).collect::<Vec<_>>(),
                    pieces,
                    "{file}"
                );
                assert_eq!(chain[0].0 == e.record, first_in_base, "{file}");
                assert_eq!(
                    base.data.is_empty(),
                    !first_in_base,
                    "{file}: $DATA in the base record"
                );
                assert_chain_complete(m, e);
                // sizes only in the extent with VCN 0
                assert_eq!(
                    (chain[0].1.real_size, chain[0].1.allocated_size),
                    (e.size, e.allocated_size)
                );
                assert!(chain[1..].iter().all(|(_, x)| x.real_size == 0
                    && x.allocated_size == 0
                    && x.initialized_size == 0));
                // the extension records: in use, pointing at the base, without $STANDARD_INFORMATION
                assert_eq!(
                    e.extension_records.len(),
                    pieces.len() - usize::from(first_in_base)
                );
                for (number, offset) in &e.extension_records {
                    let ext = record(m, *number);
                    assert_eq!(ext.offset, *offset);
                    assert_eq!(
                        (ext.role, ext.flags, ext.base),
                        (RecordRole::Extension, 1, FileRef::new(e.record, e.sequence))
                    );
                    assert!(ext.standard_information.is_none() && ext.names.is_empty());
                }
                // one list entry per extent, naming the record that holds it
                let listed: Vec<(u64, u64)> = list
                    .entries
                    .iter()
                    .filter(|l| l.type_code == 0x80)
                    .map(|l| (l.lowest_vcn, l.holder.number))
                    .collect();
                let actual: Vec<(u64, u64)> = chain
                    .iter()
                    .map(|(slot, x)| (x.lowest_vcn as u64, *slot))
                    .collect();
                assert_eq!(listed, actual, "{file}");
                if !resident_list {
                    // the list lies in a cluster of its own, which a scan never
                    // reads; the record holds nothing but its runs
                    assert_eq!(list.runs.len(), 1);
                    let at = list.runs[0].lcn.unwrap() * cs;
                    let bytes = built.image.read_vec(at, 0x20);
                    assert_eq!(
                        bytes[..4],
                        [0x10, 0, 0, 0],
                        "{file}: the list starts with $STANDARD_INFORMATION"
                    );
                    assert_eq!(
                        u64::from_le_bytes(bytes[0x10..0x18].try_into().unwrap()),
                        FileRef::new(e.record, e.sequence).raw()
                    );
                    assert!(base.attr_types.contains(&0x20));
                }
                assert_reads_back(built, e);
            }
        });
    }
}

#[test]
fn the_first_extent_and_resident_data_live_in_extension_records() {
    each("vcn0_in_extension", |built| {
        let m = &built.manifest;
        let e = test_file(m, "backwards.bin");
        assert!(e.must_recover_exactly());
        let chain = data_chain(m, e);
        assert_eq!(chain.len(), 2);
        // VCN 0 is in the record with the higher number
        assert!(
            chain[0].0 > chain[1].0,
            "VCN 0 in record {}, VCN 6 in record {}",
            chain[0].0,
            chain[1].0
        );
        assert_eq!((chain[0].1.lowest_vcn, chain[1].1.lowest_vcn), (0, 6));
        assert!(record(m, e.record).data.is_empty());
        // the later extent carries sizes, and they are not the file's
        assert_eq!(chain[0].1.real_size, e.size);
        assert!(chain[1].1.real_size != 0 && chain[1].1.real_size != e.size);
        assert!(chain[1].1.allocated_size != 0 && chain[1].1.allocated_size != e.allocated_size);
        assert_chain_complete(m, e);
        assert_reads_back(built, e);

        let r = test_file(m, "resident-elsewhere.txt");
        assert!(r.resident && r.must_recover_exactly() && r.size == 300);
        let (holder, _, len) = r.resident_at.expect("resident data");
        assert_eq!(len, 300);
        assert_eq!(r.extension_records.len(), 1);
        assert_eq!(
            holder, r.extension_records[0].1,
            "the value is in the extension record"
        );
        assert!(
            record(m, r.record).data.is_empty()
                && record(m, r.extension_records[0].0).data.len() == 1
        );
        assert_reads_back(built, r);
    });
}

#[test]
fn names_that_live_in_extension_records() {
    each("name_in_extension", |built| {
        let m = &built.manifest;
        let file = test_file(m, "named-by-its-child-record.bin");
        let base = record(m, file.record);
        assert!(base.names.is_empty() && base.attribute_list.as_ref().is_some_and(|l| !l.resident));
        let ext = record(m, file.extension_records[0].0);
        assert_eq!(ext.names.len(), 1);
        assert_eq!(ext.names[0].name, "named-by-its-child-record.bin");
        assert_eq!(file.names[0].holder_record, ext.slot);
        assert!(file.must_recover_exactly());
        assert_eq!(expected_path(file), "Test/named-by-its-child-record.bin");
        assert_reads_back(built, file);

        let dir = m.entry("old", "Test/Folder named by its child record");
        assert_eq!(dir.kind, EntryKind::Dir);
        let base = record(m, dir.record);
        assert!(base.names.is_empty() && base.index_root.is_none());
        assert_eq!(base.flags, 3, "the base record carries the directory flag");
        let ext = record(m, dir.extension_records[0].0);
        assert_eq!(ext.flags, 1, "the extension record never does");
        assert_eq!(ext.names.len(), 1);
        assert_eq!(ext.index_root.as_ref().map(Vec::len), Some(3));
        for i in 0..3 {
            let child = m.entry(
                "old",
                &format!("Test/Folder named by its child record/inside-{i}.dat"),
            );
            assert_eq!(expected_path(child), child.path);
            assert_eq!(
                child.names[0].parent,
                FileRef::new(dir.record, dir.sequence)
            );
        }
    });
}

#[test]
fn base_and_extension_records_in_different_mft_fragments() {
    each("extensions_in_other_fragment", |built| {
        // D4 (a): the base references between the fragments resolve
        let m = &built.manifest;
        let fragments: Vec<_> = m.fragments.iter().filter(|f| f.records >= 16).collect();
        assert_eq!(fragments.len(), 2);
        let (a, b) = (
            common::fragment_records(m, fragments[0]),
            common::fragment_records(m, fragments[1]),
        );
        let (counted, resolving) = common::references_between(m, &a, &b);
        assert!(
            resolving >= 3 && resolving == counted,
            "{resolving} of {counted} references resolve"
        );
        let main = m.main_instance().unwrap();
        assert!(a
            .iter()
            .chain(&b)
            .all(|r| r.expect.instance == Some(main.id)));
        let m = &built.manifest;
        let g = &m.instance("old").geometry;
        assert_eq!(g.mft_extents.len(), 2);
        let fragment = |slot: u64| usize::from(slot >= 512);
        let a = test_file(m, "base-in-first-fragment.bin");
        assert_eq!(
            (
                a.record,
                a.extension_records.iter().map(|x| x.0).collect::<Vec<_>>()
            ),
            (300, vec![600, 601])
        );
        let b = m.entry(
            "old",
            "Test/In the second fragment/base-in-second-fragment.bin",
        );
        assert_eq!(
            (
                b.record,
                b.extension_records.iter().map(|x| x.0).collect::<Vec<_>>()
            ),
            (700, vec![400])
        );
        for e in [a, b] {
            assert!(e.must_recover_exactly());
            assert_eq!(expected_path(e), e.path);
            assert_chain_complete(m, e);
            assert_reads_back(built, e);
        }
        // both fragments are seeds, and only three things link them
        let records = intact_records(m, "old");
        for f in 0..2 {
            let n = records.iter().filter(|r| fragment(r.slot) == f).count();
            assert!(n >= 64, "fragment {f} has {n} records");
        }
        let base_links = records
            .iter()
            .filter(|r| r.base != FileRef::NONE && fragment(r.slot) != fragment(r.base.number))
            .count();
        assert_eq!(base_links, 3, "records 600, 601 and 400");
        let parent_links: Vec<&str> = m
            .entries_of("old")
            .filter(|e| {
                shown(e)
                    && e.names[0].parent.number >= 256
                    && fragment(e.record) != fragment(e.names[0].parent.number)
            })
            .map(|e| e.name())
            .collect();
        assert_eq!(parent_links, ["In the second fragment"]);
    });
}

#[test]
fn deleted_files_keep_their_extension_records() {
    each("deleted_file_with_extensions", |built| {
        let m = &built.manifest;
        for (name, sequence) in [("deleted-split.bin", 1u16), ("deleted-twice.bin", 2)] {
            let e = test_file(m, name);
            assert!(e.deleted && shown(e));
            assert_eq!(
                e.sequence, sequence,
                "{name}: the sequence references carry"
            );
            let base = record(m, e.record);
            assert_eq!(
                (base.flags, base.sequence),
                (0, sequence + 1),
                "{name}: freed, sequence bumped"
            );
            assert_eq!(e.extension_records.len(), 2);
            for (number, _) in &e.extension_records {
                let ext = record(m, *number);
                assert_eq!(ext.flags, 0);
                assert_eq!(
                    ext.base,
                    FileRef::new(e.record, sequence),
                    "{name}: the base's old sequence"
                );
                assert_eq!(
                    ext.sequence, 2,
                    "{name}: an extension's own sequence is bumped, too"
                );
            }
            must(e, Flag::Deleted);
            must_not(e, Flag::MissingFragments);
            must_not(e, Flag::SizeEstimated);
            assert_eq!(expected_path(e), e.path);
            assert_chain_complete(m, e);
            assert!(
                e.must_recover_exactly(),
                "{name}: nothing was written over it"
            );
            assert_reads_back(built, e);
        }
        assert_eq!(
            m.entry("old", "Test/scratch.tmp").truth.record_state,
            RecordState::Reused
        );
    });
}

#[test]
fn a_free_extension_of_a_live_base_is_a_leftover() {
    each("free_extension_leftover", |built| {
        let m = &built.manifest;
        let current = test_file(m, "current.bin");
        let other = test_file(m, "somebody-else.bin");
        let leftover = record(m, 400);
        assert_eq!(
            (leftover.role, leftover.flags, leftover.sequence),
            (RecordRole::Raw, 0, 2)
        );
        assert_eq!(
            leftover.base,
            FileRef::new(current.record, 1),
            "it names the base with its live sequence"
        );
        assert_eq!(record(m, current.record).sequence, 1);
        let stale = leftover.data[0].non_resident.as_ref().unwrap();
        assert_eq!(stale.lowest_vcn, 0);
        assert!(stale.real_size > current.size);
        let others: Vec<Run> = other
            .extents
            .iter()
            .map(|x| Run {
                lcn: x.lcn,
                len: x.clusters,
            })
            .collect();
        assert_eq!(stale.runs, others, "its runs are another file's clusters");
        assert_eq!(leftover.names[0].name, "old name of current.bin");
        // the file is what its base record says, nothing else
        assert_eq!(current.extents.iter().map(|x| x.clusters).sum::<u64>(), 8);
        for e in [current, other] {
            assert!(e.must_recover_exactly());
            for flag in [
                Flag::MissingFragments,
                Flag::SizeEstimated,
                Flag::Overwritten,
                Flag::Overlap,
            ] {
                must_not(e, flag);
            }
            assert_eq!(expected_path(e), e.path);
            assert_reads_back(built, e);
        }
        assert!(leftover.entry.is_none(), "the leftover belongs to no file");
    });
}

#[test]
fn orphan_groups_of_a_lost_base() {
    each("lost_base_with_extensions", |built| {
        let m = &built.manifest;
        let lost = |name: &str, record_number: u64| {
            let e = test_file(m, name);
            assert_eq!(
                (e.record, e.truth.record_state),
                (record_number, RecordState::Destroyed),
                "{name}"
            );
            assert!(!e.truth.record_intact);
            e
        };
        let named = lost("named.bin", 200);
        assert_eq!(
            (named.expect.presence, expected_path(named).as_str()),
            (Presence::Shown, "Test/named.bin")
        );
        assert!(named.truth.lost_extension_records.is_empty());
        assert!([300, 301]
            .iter()
            .all(|&n| record(m, n).base == FileRef::new(200, 1)));
        assert_eq!(
            record(m, 300).names.len() + record(m, 301).names.len(),
            1,
            "the name is in an extension record"
        );
        assert_chain_complete(m, named);
        assert_eq!(named.truth.content_intact, Some(true));
        assert_reads_back(built, named);
        must_not(named, Flag::MissingFragments);

        let nameless = lost("nameless.bin", 201);
        assert_eq!(expected_path(nameless), "[Orphaned files]/File #201");
        assert!(record(m, 302).names.is_empty() && record(m, 303).names.is_empty());
        assert_chain_complete(m, nameless);
        assert_reads_back(built, nameless);
        must_not(nameless, Flag::MissingFragments);
        must_not(nameless, Flag::SizeEstimated);

        let headless = lost("headless.bin", 202);
        assert_eq!(expected_path(headless), "[Orphaned files]/File #202");
        must(headless, Flag::MissingFragments);
        must(headless, Flag::SizeEstimated);
        let chain = data_chain(m, headless);
        assert_eq!(
            chain
                .iter()
                .map(|(slot, x)| (*slot, x.lowest_vcn))
                .collect::<Vec<_>>(),
            [(304, 4), (305, 8)]
        );
        let highest = chain
            .iter()
            .map(|(_, x)| x.lowest_vcn + x.runs.iter().map(|r| r.len as i64).sum::<i64>())
            .max()
            .unwrap();
        assert_eq!(highest * 4096, 49_152, "the size a scan can estimate");
        assert!(m.notes.iter().any(|n| n.contains("49152")));

        let only_name = lost("only-a-name.bin", 203);
        assert_eq!(
            only_name.expect.presence,
            Presence::Gone,
            "an orphan group without $DATA is dropped"
        );
        let ext = record(m, 306);
        assert!(ext.names.len() == 1 && ext.data.is_empty() && ext.base == FileRef::new(203, 1));

        let low = record(m, 310);
        assert_eq!(
            (low.role, low.base, low.flags),
            (RecordRole::Raw, FileRef::new(20, 1), 1)
        );
        assert!(low.entry.is_none());
    });
}

#[test]
fn files_that_lost_extension_records() {
    each("lost_extension_records", |built| {
        let m = &built.manifest;
        let check = |name: &str, lost: &[u64], estimated: bool| {
            let e = test_file(m, name);
            assert!(
                shown(e) && e.truth.record_state == RecordState::Intact,
                "{name}"
            );
            assert_eq!(e.truth.lost_extension_records, lost, "{name}");
            assert!(
                !e.truth.record_intact && !e.must_recover_exactly(),
                "{name}"
            );
            must(e, Flag::MissingFragments);
            if estimated {
                must(e, Flag::SizeEstimated);
            } else {
                must_not(e, Flag::SizeEstimated);
            }
            assert_eq!(expected_path(e), e.path);
            e
        };
        let hole = check("hole-in-the-middle.bin", &[210], false);
        let chain = data_chain(m, hole);
        assert_eq!(
            chain.iter().map(|(_, x)| x.lowest_vcn).collect::<Vec<_>>(),
            [0, 8],
            "VCN 4-7 are gone"
        );
        let head = check("no-beginning.bin", &[211], true);
        assert_eq!(
            data_chain(m, head)
                .iter()
                .map(|(_, x)| x.lowest_vcn)
                .collect::<Vec<_>>(),
            [4, 8]
        );
        let none = check("nothing-left.bin", &[212], false);
        assert!(data_chain(m, none).is_empty());
        assert!(record(m, none.record).attribute_list.is_some());
        // the clusters themselves are untouched: the damage is in the records only
        for e in [hole, head, none] {
            assert_eq!(e.truth.content_intact, Some(true), "{}", e.path);
        }
        let ok = test_file(m, "complete.bin");
        assert!(ok.must_recover_exactly());
        must_not(ok, Flag::MissingFragments);
        assert_chain_complete(m, ok);
        assert_reads_back(built, ok);
    });
}

#[test]
fn index_allocations_split_over_extension_records() {
    each("index_allocation_split", |built| {
        let m = &built.manifest;
        let old = m.instance("old");
        let index_extents = |slot: u64| -> Vec<&ExtentFact> {
            record(m, slot)
                .index_allocation
                .iter()
                .filter_map(|s| s.non_resident.as_ref())
                .collect()
        };
        let big = m.entry("old", "Test/Big folder");
        let blocks: Vec<_> = m
            .index_blocks
            .iter()
            .filter(|b| b.owner_entry == Some(big.id))
            .collect();
        // twelve leaves and the node above them: twelve separators do not fit the record
        assert_eq!(blocks.len(), 13);
        assert_eq!(blocks.iter().filter(|b| b.leaf).count(), 12);
        let node = blocks.iter().find(|b| !b.leaf).expect("a node block");
        assert_eq!(
            node.entries.len(),
            11,
            "eleven separators between twelve leaves"
        );
        assert!(node.entries.iter().all(|e| e.subnode.is_some()));
        let in_root = record(m, big.record)
            .index_root
            .as_ref()
            .expect("an index root")
            .len();
        assert_eq!(in_root, 0, "the root only points at the node block");
        assert_eq!(blocks.iter().map(|b| b.entries.len()).sum::<usize>(), 400);
        let base = index_extents(big.record);
        assert_eq!(
            (base.len(), base[0].lowest_vcn, base[0].runs.len()),
            (1, 0, 4)
        );
        assert_eq!(big.extension_records.len(), 2);
        let mut all_runs: Vec<Run> = base[0].runs.clone();
        for ((number, _), expect) in big.extension_records.iter().zip([(4, 4), (8, 5)]) {
            let ext = index_extents(*number);
            assert_eq!(
                (ext.len(), (ext[0].lowest_vcn, ext[0].runs.len())),
                (1, expect)
            );
            all_runs.extend(ext[0].runs.iter().copied());
        }
        // every block lies where VCN x cluster size says, through the extent that contains it
        for block in &blocks {
            let run = &all_runs[block.vcn as usize];
            assert_eq!(run.len, 1);
            assert_eq!(
                block.offset,
                old.geometry.volume_offset + run.lcn.unwrap() * 4096,
                "block {}",
                block.vcn
            );
        }
        let outside_base = blocks.iter().filter(|b| b.vcn >= 4).count();
        assert_eq!(
            outside_base, 9,
            "most blocks can only be placed through the extension records"
        );

        let scattered = m.entry("old", "Test/Scattered folder");
        let base = record(m, scattered.record);
        assert!(
            base.names.is_empty() && base.index_root.is_none() && base.index_allocation.is_empty()
        );
        assert!(base.attribute_list.as_ref().is_some_and(|l| !l.resident));
        assert_eq!(scattered.extension_records.len(), 2);
        let first = record(m, scattered.extension_records[0].0);
        let second = record(m, scattered.extension_records[1].0);
        assert!(first.index_root.is_some() && first.names.is_empty());
        assert_eq!(second.names.len(), 1);
        let lowest = |r: &RecordFact| {
            r.index_allocation[0]
                .non_resident
                .as_ref()
                .unwrap()
                .lowest_vcn
        };
        assert_eq!(
            (lowest(first), lowest(second)),
            (0, 3),
            "both halves of the index allocation are in extension records"
        );
        assert_eq!(scattered.names[0].holder_record, second.slot);
        let child = m.entry("old", "Test/Scattered folder/piece 0100.dat");
        assert_eq!(expected_path(child), child.path);
    });
}

/// The bytes of the mapping pairs of the first non-resident `$DATA` of a
/// record, up to the end of the attribute.
fn runlist_bytes(built: &Built, r: &RecordFact) -> Vec<u8> {
    let bytes = read_structure(built, r.offset, r.size as usize);
    let attr = r.data[0].attr_offset as usize;
    let length = u32::from_le_bytes(bytes[attr + 4..attr + 8].try_into().unwrap()) as usize;
    let pairs = u16::from_le_bytes([bytes[attr + 0x20], bytes[attr + 0x21]]) as usize;
    bytes[attr + pairs..attr + length].to_vec()
}

#[test]
fn unusual_runlists_and_headers() {
    each("runlist_oddities", |built| {
        let m = &built.manifest;
        let extent = |e: &Entry| {
            record(m, e.record).data[0]
                .non_resident
                .clone()
                .expect("non-resident")
        };

        let zero = test_file(m, "highest-vcn-zero.bin");
        assert_eq!(
            (
                extent(zero).highest_vcn,
                extent(zero).runs.iter().map(|r| r.len).sum::<u64>()
            ),
            (0, 9)
        );

        let garbage = test_file(m, "garbage-after-runlist.bin");
        let pairs = runlist_bytes(built, record(m, garbage.record));
        let mut at = 0;
        while pairs[at] != 0 {
            at += 1 + (pairs[at] & 0x0F) as usize + (pairs[at] >> 4) as usize;
        }
        assert_eq!(pairs[at + 1], 0xFF, "FF directly behind the terminator");
        assert_eq!(garbage.extents.len(), 3);

        let descending = test_file(m, "descending.bin");
        let lcns: Vec<u64> = descending.extents.iter().map(|x| x.lcn.unwrap()).collect();
        assert!(lcns.len() == 5 && lcns.windows(2).all(|w| w[1] < w[0]));
        let pairs = runlist_bytes(built, record(m, descending.record));
        // second run: one length byte, then a negative delta (high bit of its last byte set)
        let first_len = 1 + (pairs[0] & 0x0F) as usize + (pairs[0] >> 4) as usize;
        let header = pairs[first_len];
        let delta_last = pairs[first_len + (header & 0x0F) as usize + (header >> 4) as usize];
        assert!(delta_last & 0x80 != 0, "a negative LCN delta");

        let run128 = test_file(m, "run-of-128.bin");
        assert_eq!(
            run128
                .extents
                .iter()
                .map(|x| x.clusters)
                .collect::<Vec<_>>(),
            [128]
        );
        let pairs = runlist_bytes(built, record(m, run128.record));
        assert_eq!(
            (pairs[0] & 0x0F, pairs[1], pairs[2]),
            (2, 0x80, 0x00),
            "128 needs two length bytes"
        );

        let empty = test_file(m, "empty-nonresident.bin");
        assert!(!empty.resident && empty.size == 0 && empty.extents.is_empty());
        let x = extent(empty);
        assert_eq!(
            (x.lowest_vcn, x.highest_vcn, x.runs.len(), x.allocated_size),
            (0, -1, 0, 0)
        );

        for e in [zero, garbage, descending, run128, empty] {
            assert!(e.must_recover_exactly(), "{}", e.path);
            must_not(e, Flag::MissingFragments);
            assert_reads_back(built, e);
        }
    });
}

#[test]
fn every_namespace_and_combination() {
    each("namespaces", |built| {
        let m = &built.manifest;
        fn spaces(e: &Entry) -> Vec<(&str, u8)> {
            e.names
                .iter()
                .map(|n| (n.name.as_str(), n.namespace))
                .collect()
        }
        assert_eq!(
            spaces(test_file(m, "posix-only.dat")),
            [("posix-only.dat", 0)]
        );
        assert_eq!(
            spaces(test_file(m, "win32-only.dat")),
            [("win32-only.dat", 1)]
        );
        assert_eq!(spaces(test_file(m, "DOSONLY.DAT")), [("DOSONLY.DAT", 2)]);
        assert_eq!(spaces(test_file(m, "BOTH.DAT")), [("BOTH.DAT", 3)]);
        let alias_first = test_file(m, "long name with alias.dat");
        assert_eq!(
            spaces(alias_first),
            [("LONGNA~1.DAT", 2), ("long name with alias.dat", 1)]
        );
        let win32_first = test_file(m, "win32 first.dat");
        assert_eq!(
            spaces(win32_first),
            [("win32 first.dat", 1), ("WIN32F~1.DAT", 2)]
        );
        let posix_and_win32 = test_file(m, "posix name of the same file.dat");
        assert_eq!(
            expected_path(posix_and_win32),
            "Test/win32 name of a posix file.dat",
            "Win32 beats POSIX"
        );
        for (e, path) in [
            (alias_first, "Test/long name with alias.dat"),
            (win32_first, "Test/win32 first.dat"),
            (test_file(m, "DOSONLY.DAT"), "Test/DOSONLY.DAT"),
            (test_file(m, "posix-only.dat"), "Test/posix-only.dat"),
            (test_file(m, "BOTH.DAT"), "Test/BOTH.DAT"),
        ] {
            assert_eq!(expected_path(e), path);
        }
        // the three names that differ only in case are three files
        let records: Vec<u64> = ["Readme.txt", "README.TXT", "readme.txt"]
            .iter()
            .map(|n| test_file(m, n).record)
            .collect();
        assert!(records[0] != records[1] && records[1] != records[2]);
        let dir = m.entry("old", "Test/Folder with alias");
        assert_eq!(spaces(dir), [("FOLDER~1", 2), ("Folder with alias", 1)]);
        assert_eq!(
            expected_path(m.entry("old", "Test/Folder with alias/child.dat")),
            "Test/Folder with alias/child.dat"
        );
        // in the record, names are stored in this order with these attribute ids
        let r = record(m, alias_first.record);
        assert_eq!(
            r.names.iter().map(|n| n.namespace).collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(
            r.link_count, 2,
            "Windows counts the alias: a DOS + Win32 pair has link count 2"
        );
        for e in m
            .entries_of("old")
            .filter(|e| e.path.starts_with("Test/") && e.kind == EntryKind::File)
        {
            assert!(e.must_recover_exactly(), "{}", e.path);
        }
    });
}

#[test]
fn hard_links_in_different_folders() {
    each("hard_links", |built| {
        let m = &built.manifest;
        let two = m.entry("old", "Test/A/two-homes.dat");
        assert_eq!(two.names.len(), 2);
        assert_eq!(expected_path(two), "Test/A/two-homes.dat");
        assert_eq!(record(m, two.record).link_count, 2);

        let gone = m.entry("old", "Destroyed folder");
        assert_eq!(
            (gone.record, gone.truth.record_state),
            (38, RecordState::Destroyed)
        );
        assert_eq!(
            gone.truth.surviving_index_entries, 0,
            "nothing names the destroyed folder any more"
        );
        let second = m.entry("old", "Destroyed folder/name-in-destroyed-folder.dat");
        assert_eq!(second.names[0].parent, FileRef::new(38, 1));
        assert_eq!(expected_path(second), "Test/B/second-home-survives.dat");
        assert!(second.must_recover_exactly());

        let three = m.entry("old", "Test/A/three-links.dat");
        assert_eq!(three.names.len(), 3);
        assert_eq!(record(m, three.record).link_count, 3);
        assert_eq!(expected_path(three), "Test/A/three-links.dat");

        let rooted = m.entry("old", "root-and-folder.dat");
        assert_eq!(rooted.names[0].parent.number, 5);
        assert_eq!(expected_path(rooted), "root-and-folder.dat");
        // the first name in the record is not the one with the lowest attribute id
        let ordered = m.entry("old", "Test/A/stored first, created last.dat");
        let stored = &record(m, ordered.record).names;
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].name, "stored first, created last.dat");
        assert!(
            stored[0].attr_id > stored[1].attr_id,
            "ids {} and {}",
            stored[0].attr_id,
            stored[1].attr_id
        );
        assert_eq!(
            ordered.names.iter().map(|n| n.attr_id).collect::<Vec<_>>(),
            [stored[0].attr_id, stored[1].attr_id]
        );
        assert_eq!(
            expected_path(ordered),
            "Test/B/stored last, created first.dat"
        );
        // each file exists once, whatever the number of names
        for e in [two, second, three, rooted, ordered] {
            assert_eq!(
                m.entries_of("old")
                    .filter(|x| x.record == e.record && x.sequence == e.sequence)
                    .count(),
                1
            );
            assert_reads_back(built, e);
        }
        // every name is in its folder's index
        let b = m.entry("old", "Test/B");
        let listed: Vec<&str> = record(m, b.record)
            .index_root
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| e.key.name.as_str())
            .collect();
        assert_eq!(
            listed,
            [
                "second name of two-homes.dat",
                "second-home-survives.dat",
                "stored last, created first.dat",
                "three-links in B.dat"
            ]
        );
    });
}

#[test]
fn names_a_windows_path_cannot_hold() {
    each("odd_names", |built| {
        let m = &built.manifest;
        let odd: Vec<&Entry> = m
            .entries_of("old")
            .filter(|e| e.path.starts_with("Test/names/"))
            .collect();
        let tagged = |tag: &str| {
            odd.iter()
                .filter(|e| e.tags.iter().any(|t| t == tag))
                .copied()
                .collect::<Vec<&Entry>>()
        };
        let lone_surrogate = |units: &[u16]| {
            (0..units.len()).any(|i| {
                let u = units[i];
                let high = (0xD800..0xDC00).contains(&u);
                let low = (0xDC00..0xE000).contains(&u);
                (high
                    && !units
                        .get(i + 1)
                        .is_some_and(|n| (0xDC00..0xE000).contains(n)))
                    || (low && !(i > 0 && (0xD800..0xDC00).contains(&units[i - 1])))
            })
        };
        let lone = tagged("lone-surrogate");
        assert_eq!(lone.len(), 3);
        for e in &lone {
            let name = &e.names[0];
            assert!(lone_surrogate(&name.name_utf16), "{:04X?}", name.name_utf16);
            assert_eq!(
                name.name.chars().filter(|&c| c == '\u{FFFD}').count(),
                1,
                "sent as U+FFFD"
            );
            assert_eq!(name.namespace, 0);
        }
        let pair = tagged("surrogate-pair");
        assert!(
            pair.len() == 1
                && !lone_surrogate(&pair[0].names[0].name_utf16)
                && pair[0].name().contains('\u{1F600}')
        );
        let long = tagged("255-units");
        assert_eq!(long.len(), 3);
        assert!(long.iter().all(|e| e.names[0].name_utf16.len() == 255));
        assert!(
            long.iter().any(|e| e.name().chars().count() == 128),
            "127 pairs and one unit"
        );
        let illegal = tagged("illegal-char");
        assert_eq!(illegal.len(), 10);
        for c in "<>:\"/\\|?*".chars() {
            assert!(illegal.iter().any(|e| e.name().contains(c)), "{c}");
        }
        let control = tagged("control-char");
        assert_eq!(control.len(), 3);
        assert!(control
            .iter()
            .all(|e| e.names[0].name_utf16.iter().any(|&u| u < 0x20)));
        let dots = tagged("dots-and-spaces");
        assert_eq!(dots.len(), 8);
        assert!(dots.iter().any(|e| e.name() == "...") && dots.iter().any(|e| e.name() == "   "));
        let reserved: Vec<&str> = tagged("reserved-name").iter().map(|e| e.name()).collect();
        for name in [
            "CON",
            "nul.txt",
            "COM1",
            "LPT9.tar.gz",
            "AUX",
            "CONIN$",
            "CONOUT$",
            "COM\u{B9}",
            "con .txt",
            "nul",
            "PRN",
        ] {
            assert!(reserved.contains(&name), "{name}");
        }
        let free: Vec<&str> = tagged("not-reserved").iter().map(|e| e.name()).collect();
        assert_eq!(free.len(), 4);
        assert!(free.contains(&"CONSOLE.txt") && free.contains(&"COM10"));
        assert_eq!(tagged("engine-name").len(), 2);
        // a name with a slash is one component
        let slash = illegal
            .iter()
            .find(|e| e.name() == "illegal / char.dat")
            .unwrap();
        assert_eq!(slash.components, ["Test", "names", "illegal / char.dat"]);
        // folders with impossible names keep their children
        assert_eq!(
            expected_path(m.entry("old", "Test/names/PRN/inside a device name.dat")),
            "Test/names/PRN/inside a device name.dat"
        );
        let inside = m.entry("old", "Test/names/folder ending in a dot./inside.dat");
        assert_eq!(
            inside.expect.path.as_ref().unwrap()[2],
            "folder ending in a dot."
        );
        // all of them are ordinary, intact files
        assert!(odd.len() >= 48, "{}", odd.len());
        for e in odd.iter().filter(|e| e.kind == EntryKind::File) {
            assert!(shown(e) && e.must_recover_exactly(), "{}", e.path);
            assert_eq!(e.expect.path.as_ref(), Some(&e.components), "{}", e.path);
            assert_reads_back(built, e);
        }
        // and the names are in the index of their folder, in collation order
        let names = m.entry("old", "Test/names");
        let blocks: Vec<_> = m
            .index_blocks
            .iter()
            .filter(|b| b.owner_entry == Some(names.id))
            .collect();
        assert!(!blocks.is_empty(), "the folder needs index blocks");
        let indexed: usize = blocks.iter().map(|b| b.entries.len()).sum::<usize>()
            + record(m, names.record)
                .index_root
                .as_ref()
                .map_or(0, Vec::len);
        assert_eq!(
            indexed,
            m.entries_of("old")
                .filter(|e| e.parent == Some(names.id))
                .count()
        );
    });
}
