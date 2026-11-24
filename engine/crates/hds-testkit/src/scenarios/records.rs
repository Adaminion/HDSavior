//! Files and folders that need more than one MFT record (ALGORITHMS E1,
//! E2, E7; docs/notes/ntfs-on-disk.md section 8).

use crate::disk::Built;
use crate::manifest::{Flag, Presence};
use crate::ondisk::attr::{file_name, FileNameValue};
use crate::ondisk::record::{attr_type, AttrModel, NonResident, RecordModel};
use crate::ondisk::runlist::{total_clusters, Run};
use crate::ondisk::{utf16, FileRef};
use crate::rng::derive;
use crate::volume::params::{ListForm, MftFragment, VolumeParams};
use crate::volume::spec::{DirSpec, FileSpec, MultiSpec};

use super::{finish_formatted, stage, stage_with, MIB};

fn split_files(name: &str, seed: u64, list: ListForm, description: &str) -> Built {
    let (mut disk, old, test) = stage(name, seed, description, &["E1", "E2", "A4"]);
    let v = disk.volume(old);
    let cs = v.cluster_size();
    // Windows style: every $DATA extent in a child record, the base holds
    // $STANDARD_INFORMATION, the list and the name
    v.add_file(
        test,
        "windows-style.bin",
        FileSpec::tagged(derive(seed, "w"), 24 * cs - 123)
            .fragments(12)
            .multi(MultiSpec::new(list).data(&[(1, 4), (2, 4), (3, 4)])),
    );
    // ntfs-3g style: the first extent stays in the base record
    v.add_file(
        test,
        "first-extent-in-base.bin",
        FileSpec::tagged(derive(seed, "n"), 18 * cs).fragments(9).multi(MultiSpec::new(list).data(&[(0, 3), (1, 3), (2, 3)])),
    );
    // extents of very different lengths, so that start VCNs are not multiples of anything
    v.add_file(
        test,
        "uneven.bin",
        FileSpec::tagged(derive(seed, "u"), 100 * cs + 1)
            .fragments(7)
            .multi(MultiSpec::new(list).data(&[(1, 1), (2, 5), (3, 1)])),
    );
    finish_formatted(disk, old)
}

/// `$DATA` split over extension records, resident `$ATTRIBUTE_LIST`.
///
/// **Tests ALGORITHMS E1 (in-use extension, in-use base, matching
/// sequence), E2 (extents ordered by start VCN, each beginning where the
/// previous one ends; sizes only in the VCN-0 extent), A4 (resident list
/// entries).** Three files below `Test`: one laid out as Windows does it
/// (all three extents in child records, none in the base), one as ntfs-3g
/// does it (first extent in the base record), one with extents of 1, 5 and
/// 1 runs. Real drivers on Linux never produce a *resident* list; Windows
/// always starts with one.
pub fn attribute_list_resident(seed: u64) -> Built {
    split_files(
        "attribute_list_resident",
        seed,
        ListForm::Resident,
        "Files whose $DATA is split over extension records, with a resident attribute list.",
    )
}

/// The same files with a non-resident `$ATTRIBUTE_LIST`.
///
/// **Tests ALGORITHMS E1, E2.** The list lies in a cluster of its own and
/// the scan never reads it: the extension records must find their base
/// through their own base reference alone.
pub fn attribute_list_nonresident(seed: u64) -> Built {
    split_files(
        "attribute_list_nonresident",
        seed,
        ListForm::NonResident,
        "Files whose $DATA is split over extension records, with a non-resident attribute list.",
    )
}

/// The extent with VCN 0 is not in the base record, and extension records
/// are not in VCN order.
///
/// **Tests ALGORITHMS E2 ("sizes are read only from the extent with start
/// VCN 0", ordering by start VCN and not by record number), notes section
/// 14 item 3.** `backwards.bin`: VCN 0–5 in the *second* extension record
/// (the one with the higher number), VCN 6–11 in the first. The later
/// extent carries plausible but wrong non-zero sizes, which are legal
/// there and must be ignored. `resident-elsewhere.txt` keeps its resident
/// `$DATA` in an extension record: recovery has to re-read that record,
/// not the base (H7).
pub fn vcn0_in_extension(seed: u64) -> Built {
    let (mut disk, old, test) = stage(
        "vcn0_in_extension",
        seed,
        "VCN-0 extent in the higher-numbered extension record; resident data in an extension record.",
        &["E2", "H7"],
    );
    let v = disk.volume(old);
    let cs = v.cluster_size();
    let mut spec = FileSpec::tagged(derive(seed, "b"), 12 * cs - 77)
        .fragments(4)
        .multi(MultiSpec::new(ListForm::Resident).data(&[(2, 2), (1, 2)]));
    spec.quirks.sizes_in_later_extents = true;
    v.add_file(test, "backwards.bin", spec);
    v.add_file(
        test,
        "resident-elsewhere.txt",
        FileSpec::tagged(derive(seed, "r"), 300).resident().multi(MultiSpec::new(ListForm::Resident).resident_data_in(1)),
    );
    finish_formatted(disk, old)
}

/// The only `$FILE_NAME` lives in an extension record.
///
/// **Tests ALGORITHMS E2 ("names come from the base and all its extensions
/// alike"), E3.** A file and a folder whose base records hold no name at
/// all — what ntfs-3g does when a record overflows. The folder also keeps
/// its `$INDEX_ROOT` in the extension record; its base record still
/// carries the directory flag, the extension record never does. An
/// implementation that reads names from base records only shows both as
/// nameless orphans and loses the folder's children.
pub fn name_in_extension(seed: u64) -> Built {
    let (mut disk, old, test) = stage(
        "name_in_extension",
        seed,
        "A file and a folder whose only $FILE_NAME (and, for the folder, $INDEX_ROOT) is in an extension record.",
        &["E2", "E3"],
    );
    let v = disk.volume(old);
    v.add_file(
        test,
        "named-by-its-child-record.bin",
        FileSpec::tagged(derive(seed, "f"), 50_000).multi(MultiSpec::new(ListForm::NonResident).names_in(1)),
    );
    let dir = v.mkdir_with(
        test,
        "Folder named by its child record",
        DirSpec::new().multi(MultiSpec::new(ListForm::Resident).names_in(1).index_root_in(1)),
    );
    for i in 0..3 {
        v.add_tagged(dir, format!("inside-{i}.dat").as_str(), 500 + i);
    }
    finish_formatted(disk, old)
}

/// Base and extension records in different fragments of the MFT.
///
/// **Tests ALGORITHMS D4 (base references join fragments into one
/// instance) and E1.** The MFT has two fragments of 512 records. One file
/// has its base in the first fragment (record 300) and its extension
/// records in the second (600, 601); another the other way round (base
/// 700, extension 400). Apart from these and one folder per fragment,
/// nothing links the two fragments.
pub fn extensions_in_other_fragment(seed: u64) -> Built {
    let mut params = VolumeParams::new(0, 64 * MIB).label("old");
    let far = params.total_clusters() * 3 / 4;
    params.mft = vec![MftFragment { lcn: None, records: 512 }, MftFragment { lcn: Some(far), records: 512 }];
    let (mut disk, old, test) = stage_with(
        "extensions_in_other_fragment",
        seed,
        "Two MFT fragments; files whose base record is in one and whose extension records are in the other.",
        &["D4", "E1"],
        params,
    );
    let v = disk.volume(old);
    let cs = v.cluster_size();
    v.add_file(
        test,
        "base-in-first-fragment.bin",
        FileSpec::tagged(derive(seed, "a"), 16 * cs)
            .fragments(8)
            .record(300)
            .multi(MultiSpec::new(ListForm::Resident).data(&[(1, 4), (2, 4)]).extension_records(&[600, 601])),
    );
    let far_dir = v.mkdir_with(test, "In the second fragment", DirSpec::new().record(650));
    v.add_file(
        far_dir,
        "base-in-second-fragment.bin",
        FileSpec::tagged(derive(seed, "b"), 10 * cs)
            .fragments(4)
            .record(700)
            .multi(MultiSpec::new(ListForm::Resident).data(&[(0, 2), (1, 2)]).extension_records(&[400])),
    );
    for i in 0..70u64 {
        v.add_file(far_dir, format!("far-{i:02}.dat").as_str(), FileSpec::tagged(derive(seed, "far") + i, 300).record(520 + i));
    }
    // the first fragment gets a population of its own (records 257 and up)
    for i in 0..80u64 {
        v.add_tagged(test, format!("near-{i:02}.dat").as_str(), 200 + i);
    }
    finish_formatted(disk, old)
}

/// Deleted files with extension records.
///
/// **Tests ALGORITHMS E1 ("neither is in use, the base has attributes, and
/// `base.seq` is `s` or `next(s)`").** When NTFS deletes a multi-record
/// file, base and extensions each get their own sequence number bumped,
/// but the extensions' base reference keeps the base's *old* sequence
/// (notes section 3). `deleted-split.bin` (base sequence 1 → 2, extensions
/// still say `(base, 1)`) must be reassembled and is intact;
/// `deleted-twice.bin` lives in a slot that was used before (base sequence
/// 2 → 3, extensions say `(base, 2)`).
pub fn deleted_file_with_extensions(seed: u64) -> Built {
    let (mut disk, old, test) = stage(
        "deleted_file_with_extensions",
        seed,
        "Deleted multi-record files: free extensions keep the base's old sequence number.",
        &["E1"],
    );
    let v = disk.volume(old);
    let cs = v.cluster_size();
    let multi = || MultiSpec::new(ListForm::Resident).data(&[(1, 3), (2, 3)]);
    let first = v.add_file(test, "deleted-split.bin", FileSpec::tagged(derive(seed, "d"), 12 * cs - 9).fragments(6).multi(multi()));
    // a slot with history: created, deleted, reused, deleted again
    let scratch = v.add_tagged(test, "scratch.tmp", 100);
    let slot = v.reference(scratch).number;
    v.delete(scratch);
    let second = v.add_file(
        test,
        "deleted-twice.bin",
        FileSpec::tagged(derive(seed, "e"), 12 * cs).fragments(6).record(slot).multi(multi()),
    );
    assert_eq!(v.reference(second).seq, 2);
    v.add_tagged(test, "alive.dat", 5000);
    v.delete(first);
    v.delete(second);
    finish_formatted(disk, old)
}

/// A free extension record next to its in-use base.
///
/// **Tests ALGORITHMS E1 ("a free extension whose base is in use is a
/// leftover and is ignored").** `current.bin` is a healthy 8-cluster file
/// in one record. Record 400 is what a truncation left behind: not in use,
/// sequence 2, base reference `(current.bin, 1)` — the base's live
/// sequence — still holding a `$DATA` extent from VCN 0 with other runs
/// and a larger size, and an old name. Merging it gives the file the wrong
/// size and another file's bytes.
pub fn free_extension_leftover(seed: u64) -> Built {
    let (mut disk, old, test) = stage(
        "free_extension_leftover",
        seed,
        "An in-use file and a free extension record that still refers to it with the live sequence number.",
        &["E1"],
    );
    let v = disk.volume(old);
    let cs = v.cluster_size();
    let file = v.add_file(test, "current.bin", FileSpec::tagged(derive(seed, "c"), 8 * cs - 300));
    let other = v.add_file(test, "somebody-else.bin", FileSpec::tagged(derive(seed, "o"), 20 * cs));
    let stale_runs = v.runs(other);
    let base = v.reference(file);
    let mut leftover = RecordModel::new(400, 2, 1024);
    leftover.flags = 0;
    leftover.base = base;
    leftover.lsn = v.lsn(file) - 0x400;
    leftover.usn = 3;
    leftover.end_tail = crate::ondisk::record::END_TAIL_WINDOWS;
    let name = file_name(&FileNameValue {
        parent: v.reference(test),
        times: [v.clock(); 4],
        allocated_size: 0,
        real_size: 0,
        flags: 0x20,
        ea_or_reparse: 0,
        namespace: 1,
        name: utf16("old name of current.bin"),
    });
    leftover.attrs.push(AttrModel::resident(attr_type::FILE_NAME, name));
    leftover.attrs.push(
        AttrModel::non_resident(
            attr_type::DATA,
            NonResident {
                lowest_vcn: 0,
                highest_vcn: total_clusters(&stale_runs) as i64 - 1,
                allocated_size: total_clusters(&stale_runs) * cs,
                real_size: total_clusters(&stale_runs) * cs - 5,
                initialized_size: total_clusters(&stale_runs) * cs - 5,
                runs: stale_runs,
                compression_unit: 0,
                compressed_size: None,
                tail: Vec::new(),
            },
        )
        .with_id(1),
    );
    leftover.next_attr_id = 2;
    v.put_raw_record(400, leftover, false);
    finish_formatted(disk, old)
}

/// Extension records whose base record is gone.
///
/// **Tests ALGORITHMS E7 (orphan groups) and E2 (lost VCN-0 extent: size
/// estimated).** Four files had their base record among the first 256
/// (records 200–203) and their extension records further up, so the format
/// destroyed only the bases:
///
/// * `named.bin` — name and all `$DATA` extents in extension records: the
///   orphan group takes name and parent from its own `$FILE_NAME`; the file
///   is complete.
/// * `nameless.bin` — all extents survive but the name was in the base:
///   `[Orphaned files]/File #201`, complete.
/// * `headless.bin` — the VCN-0 extent was in the base: `File #202`, size
///   estimated from the highest mapped VCN, fragments missing.
/// * `only-a-name.bin` — the surviving extension holds a name but no
///   `$DATA`: dropped.
///
/// A fifth group refers to base record 20: dropped whatever it holds.
pub fn lost_base_with_extensions(seed: u64) -> Built {
    let (mut disk, old, test) = {
        // leave records 200..=210 free for the bases
        let mut disk = crate::disk::Disk::new("lost_base_with_extensions", seed, 64 * MIB);
        disk.describe("Multi-record files whose base record was destroyed while extension records survive.", &["E7", "E2"]);
        let old = disk.format(VolumeParams::new(0, 64 * MIB).label("old"));
        let v = disk.volume(old);
        let root = v.root();
        let first = v.mkdir(root, "First things");
        for i in 0..150 {
            v.add_tagged(first, format!("early-{i:03}.dat").as_str(), 64 + i);
        }
        let test = v.mkdir_with(root, "Test", DirSpec::new().record(256));
        (disk, old, test)
    };
    let v = disk.volume(old);
    let cs = v.cluster_size();
    v.add_file(
        test,
        "named.bin",
        FileSpec::tagged(derive(seed, "a"), 12 * cs - 50)
            .fragments(6)
            .record(200)
            .multi(MultiSpec::new(ListForm::Resident).data(&[(1, 3), (2, 3)]).names_in(1).extension_records(&[300, 301])),
    );
    v.add_file(
        test,
        "nameless.bin",
        FileSpec::tagged(derive(seed, "b"), 8 * cs)
            .fragments(4)
            .record(201)
            .multi(MultiSpec::new(ListForm::Resident).data(&[(1, 2), (2, 2)]).extension_records(&[302, 303])),
    );
    v.add_file(
        test,
        "headless.bin",
        FileSpec::tagged(derive(seed, "c"), 12 * cs - 1000)
            .fragments(6)
            .record(202)
            .multi(MultiSpec::new(ListForm::Resident).data(&[(0, 2), (1, 2), (2, 2)]).extension_records(&[304, 305])),
    );
    v.add_file(
        test,
        "only-a-name.bin",
        FileSpec::tagged(derive(seed, "d"), 4 * cs)
            .record(203)
            .multi(MultiSpec::new(ListForm::Resident).names_in(1).extension_records(&[306])),
    );
    // an extension record of a base below 24
    let mut low = RecordModel::new(310, 1, 1024);
    low.base = FileRef::new(20, 1);
    low.lsn = 0x20_0000;
    low.attrs.push(AttrModel::non_resident(
        attr_type::DATA,
        NonResident {
            lowest_vcn: 0,
            highest_vcn: 3,
            runs: vec![Run::at(9000, 4)],
            compression_unit: 0,
            allocated_size: 4 * cs,
            real_size: 4 * cs,
            initialized_size: 4 * cs,
            compressed_size: None,
            tail: Vec::new(),
        },
    ));
    low.next_attr_id = 1;
    v.put_raw_record(310, low, true);
    let mut built = finish_formatted(disk, old);

    // the mechanical expectation for a destroyed base is "gone"; orphan
    // groups come back by E7, in the instance of their extension records
    let m = &mut built.manifest;
    let orphan = |m: &mut crate::manifest::Manifest, path: &str, shown_at: &[&str]| {
        let id = m.instance("old").id;
        let at: Vec<u64> = m.entry("old", path).extension_records.iter().map(|&(_, o)| o).collect();
        let instance = m
            .records
            .iter()
            .find(|r| r.instance == id && at.contains(&r.offset) && r.expect.instance.is_some())
            .and_then(|r| r.expect.instance)
            .expect("the extension records belong to an instance");
        let kind = m.catalog[instance as usize].kind;
        let e = m.entry_mut("old", path);
        e.expect.presence = Presence::Shown;
        e.expect.instance = Some(instance);
        e.expect.path = Some(shown_at.iter().map(|s| s.to_string()).collect());
        for (flag, on) in [
            (Flag::OlderGeneration, kind == crate::manifest::InstanceKind::Older),
            (Flag::Embedded, false),
            (Flag::Stray, kind == crate::manifest::InstanceKind::Strays),
        ] {
            if on {
                e.expect.set(flag);
            } else {
                e.expect.clear(flag);
            }
        }
    };
    orphan(m, "Test/named.bin", &["Test", "named.bin"]);
    orphan(m, "Test/nameless.bin", &["[Orphaned files]", "File #201"]);
    orphan(m, "Test/headless.bin", &["[Orphaned files]", "File #202"]);
    for path in ["Test/named.bin", "Test/nameless.bin"] {
        let e = m.entry_mut("old", path);
        for flag in [Flag::MissingFragments, Flag::SizeEstimated, Flag::Deleted] {
            e.expect.clear(flag);
        }
    }
    let e = m.entry_mut("old", "Test/headless.bin");
    e.expect.set(Flag::MissingFragments);
    e.expect.set(Flag::SizeEstimated);
    e.expect.clear(Flag::Deleted);
    m.notes.push("Expected size of Test/headless.bin: (highest mapped VCN + 1) x 4096 = 49152 bytes.".into());
    m.notes.push("Record 310 is an extension of base record 20 and must be dropped (E7).".into());
    built
}

/// Extension records that were destroyed while the base survives.
///
/// **Tests ALGORITHMS E2 (a hole in the chain → `missing_fragments`; lost
/// VCN-0 extent → `size_estimated`; list without any `$DATA` →
/// `missing_fragments`), H7 (missing clusters are zeros), G3 (`damaged`).**
/// The bases are at records 257 and up; some of their extension records
/// had low numbers (210–215) and died in the format.
///
/// * `hole-in-the-middle.bin` — three extents, the middle one lost.
/// * `no-beginning.bin` — the VCN-0 extent lost, two later ones present.
/// * `nothing-left.bin` — its only extent lost: an attribute list but no
///   `$DATA` at all.
/// * `complete.bin` — control: all extension records survive.
pub fn lost_extension_records(seed: u64) -> Built {
    let (mut disk, old, test) = {
        let mut disk = crate::disk::Disk::new("lost_extension_records", seed, 64 * MIB);
        disk.describe("Multi-record files that lost some of their extension records.", &["E2", "H7", "G3"]);
        let old = disk.format(VolumeParams::new(0, 64 * MIB).label("old"));
        let v = disk.volume(old);
        let root = v.root();
        let first = v.mkdir(root, "First things");
        for i in 0..150 {
            v.add_tagged(first, format!("early-{i:03}.dat").as_str(), 64 + i);
        }
        let test = v.mkdir_with(root, "Test", DirSpec::new().record(256));
        (disk, old, test)
    };
    let v = disk.volume(old);
    let cs = v.cluster_size();
    let file = |name: &str, record: u64, pieces: &[(usize, usize)], extensions: &[u64]| {
        FileSpec::tagged(derive(seed, name), 12 * cs - 11)
            .fragments(6)
            .record(record)
            .multi(MultiSpec::new(ListForm::Resident).data(pieces).extension_records(extensions))
    };
    v.add_file(test, "hole-in-the-middle.bin", file("hole", 257, &[(1, 2), (2, 2), (3, 2)], &[400, 210, 401]));
    v.add_file(test, "no-beginning.bin", file("head", 258, &[(1, 2), (2, 2), (3, 2)], &[211, 402, 403]));
    v.add_file(test, "nothing-left.bin", file("none", 259, &[(1, 6)], &[212]));
    v.add_file(test, "complete.bin", file("ok", 260, &[(1, 2), (2, 2), (3, 2)], &[404, 405, 406]));
    finish_formatted(disk, old)
}

/// `$INDEX_ALLOCATION` split over extension records.
///
/// **Tests ALGORITHMS F2 (index votes map a block "through the extent that
/// contains it"; notes section 13: using only the base record's extent
/// loses votes), E2 for `$INDEX_ALLOCATION:$I30`, A4.** `Big folder` has
/// 400 entries in 13 index blocks — twelve leaves and the node above them,
/// since twelve separators do not fit a 1 KiB record — every block a
/// fragment of its own. The base record holds the first four runs; two
/// extension records hold the others, with start VCNs 4 and 8. `Scattered
/// folder` (200 entries, six blocks) keeps all of its index allocation in
/// extension records and its name and `$INDEX_ROOT` too.
pub fn index_allocation_split(seed: u64) -> Built {
    let (mut disk, old, test) = stage(
        "index_allocation_split",
        seed,
        "Folders whose $INDEX_ALLOCATION is split over extension records.",
        &["F2", "E2", "A4"],
    );
    let v = disk.volume(old);
    let mut spec = DirSpec::new().multi(MultiSpec::new(ListForm::Resident).index_allocation(&[(0, 4), (1, 4), (2, 0)]));
    spec.fragment_index = true;
    let big = v.mkdir_with(test, "Big folder", spec);
    for i in 0..400 {
        v.add_tagged(big, format!("entry {i:04}.dat").as_str(), 100 + i);
    }
    let mut spec = DirSpec::new().multi(
        MultiSpec::new(ListForm::NonResident).index_allocation(&[(1, 3), (2, 0)]).names_in(2).index_root_in(1),
    );
    spec.fragment_index = true;
    let scattered = v.mkdir_with(test, "Scattered folder", spec);
    for i in 0..200 {
        v.add_tagged(scattered, format!("piece {i:04}.dat").as_str(), 80 + i);
    }
    let mut built = finish_formatted(disk, old);
    built.manifest.notes.push(
        "Big folder: 13 index blocks in 13 runs; base record VCN 0-3, extension records VCN 4-7 and 8-12.".into(),
    );
    built
}

/// What a quirky but legal `$DATA` header looks like.
///
/// **Tests ALGORITHMS E2 ("the stored last VCN is only a cross-check when
/// it is > 0"), `decode_runlist` (signed lengths, negative deltas, bytes
/// after the terminator), F1 (such attributes must not derail the cluster
/// size vote).**
///
/// * `highest-vcn-zero.bin` — 9 clusters in one extent whose highest VCN
///   field is 0, as old volumes have it.
/// * `garbage-after-runlist.bin` — `FF` directly behind the terminator.
/// * `descending.bin` — five fragments at descending LCNs: every delta
///   after the first is negative.
/// * `run-of-128.bin` — a run of exactly 128 clusters, whose length needs
///   a second byte because lengths are signed.
/// * `empty-nonresident.bin` — an allocated-then-truncated stream: lowest
///   VCN 0, highest VCN -1, no runs, size 0.
pub fn runlist_oddities(seed: u64) -> Built {
    let (mut disk, old, test) = stage(
        "runlist_oddities",
        seed,
        "Legal but unusual $DATA headers and runlists.",
        &["E2", "F1"],
    );
    let v = disk.volume(old);
    let cs = v.cluster_size();
    let mut spec = FileSpec::tagged(derive(seed, "h"), 9 * cs - 5).non_resident();
    spec.quirks.highest_vcn_zero = true;
    v.add_file(test, "highest-vcn-zero.bin", spec);
    let mut spec = FileSpec::tagged(derive(seed, "g"), 3 * cs).fragments(3);
    spec.quirks.runlist_tail = vec![0xFF];
    v.add_file(test, "garbage-after-runlist.bin", spec);
    // free space behind the MFT
    let mft = v.mft_extents()[0];
    let base = mft.lcn + mft.clusters + 1000;
    let runs: Vec<Run> = (0..5).map(|i| Run::at(base - 100 * i, 3)).collect();
    v.add_file(test, "descending.bin", FileSpec::tagged(derive(seed, "d"), 15 * cs - 1).runs(runs));
    v.add_file(test, "run-of-128.bin", FileSpec::tagged(derive(seed, "r"), 128 * cs).non_resident());
    v.add_file(test, "empty-nonresident.bin", FileSpec::tagged(0, 0).non_resident());
    let mut built = finish_formatted(disk, old);
    built.manifest.notes.push("highest-vcn-zero.bin: 9 clusters mapped, header says highest VCN 0.".into());
    built
}
