//! Instance separation: situations of ALGORITHMS C3 and D2 to D6 that a
//! builder can lay out record by record, which real drivers produce only
//! by chance. (Real-driver versions of the instance-separation histories of
//! ARCHITECTURE section 7 item 3 live in `scripts/linux/instances/`.)

use std::sync::Arc;

use crate::content::ContentSpec;
use crate::disk::{Built, Disk};
use crate::ondisk::runlist::{total_clusters, Run};
use crate::populate::{populate, TreeSpec};
use crate::rng::derive;
use crate::time;
use crate::volume::params::{BlankSlots, ListForm, VolumeParams};
use crate::volume::spec::{DirSpec, FileSpec, MultiSpec};

use super::{old_volume, reformat, MIB};

/// Formats a volume of one of the generations below.
fn generation(disk: &mut Disk, label: &str, year: u32, records: u64) -> crate::disk::VolumeId {
    disk.format(
        VolumeParams::new(0, 128 * MIB)
            .label(label)
            .volume_name(label)
            .formatted_at(time::ymd(year, 5, 17))
            .mft_capacity(records),
    )
}

/// Two generations whose records contradict each other only through
/// references: no cluster is mapped by both.
///
/// **Tests ALGORITHMS D2 (contradiction (B): a parent reference to a slot
/// whose in-use record is not a directory or has another sequence number;
/// a base reference to a slot whose in-use record has another sequence
/// number), D3, D4 (a kept cut forbids joining), D9, E5/E6 (a parent of
/// the older generation is not looked up in the newer one, and index
/// entries of the newer generation do not name it).** 2017: folders `Old
/// folder 0` … `3` in records 300–303 with ten resident files each in
/// records 600–639, and a fragmented file whose base record is 304 and
/// whose extension record 700 maps clusters far behind everything else.
/// 2020: formatted again with a 512-record MFT, which destroys records
/// 0–511 of 2017; the new file system puts a file into slot 300, a folder
/// in its second life (sequence 2) into 301, folders with sequence 1 into
/// 302 and 303 (so the references of the files of `Old folder 2` and `3`
/// resolve by coincidence), and a file in its second life into 304. 2023:
/// the accident. The 2017 files of folders 0 and 1 and the extension record
/// contradict 2020 records: 21 different stale records, a changepoint at
/// 600, a kept cut. An implementation that looks only for shared clusters
/// finds no contradiction, keeps one segment and files the 2017 records
/// into 2020 folders.
pub fn generations_by_references(seed: u64) -> Built {
    let mut disk = Disk::new("generations_by_references", seed, 128 * MIB);
    disk.describe(
        "Two generations on one partition that contradict each other through parent and base references only.",
        &["D2", "D3", "D4", "D9", "E5", "E6"],
    );
    let gen1 = generation(&mut disk, "gen1", 2017, 1024);
    let v = disk.volume(gen1);
    let root = v.root();
    let cs = v.cluster_size();
    let early = v.mkdir(root, "Early");
    let mut i = 0;
    while v.reference(early).number + i + 1 < 300 {
        i += 1;
        v.add_tagged(early, format!("early-{i:03}.dat").as_str(), 100 + i);
    }
    let folders: Vec<_> = (0..4u64)
        .map(|k| {
            v.mkdir_with(
                root,
                format!("Old folder {k}").as_str(),
                DirSpec::new().record(300 + k),
            )
        })
        .collect();
    let mut record = 600;
    for (k, &dir) in folders.iter().enumerate() {
        for j in 0..10u64 {
            let file = FileSpec::tagged(derive(seed, "kept") + 10 * k as u64 + j, 200 + j)
                .resident()
                .record(record);
            v.add_file(dir, format!("kept-{k}-{j}.txt").as_str(), file);
            record += 1;
        }
    }
    let far = v.total_clusters() - 5000;
    let runs: Vec<Run> = (0..4).map(|k| Run::at(far + 10 * k, 2)).collect();
    v.add_file(
        folders[3],
        "split-elsewhere.bin",
        FileSpec::tagged(derive(seed, "split"), 8 * cs - 100)
            .runs(runs)
            .record(304)
            .multi(
                MultiSpec::new(ListForm::Resident)
                    .data(&[(1, 0)])
                    .extension_records(&[700]),
            ),
    );
    disk.render(gen1);

    let gen2 = generation(&mut disk, "gen2", 2020, 512);
    let v = disk.volume(gen2);
    let root = v.root();
    v.add_file(
        root,
        "a file now.dat",
        FileSpec::tagged(derive(seed, "file 300"), 5000).record(300),
    );
    let short = v.mkdir_with(root, "Short-lived", DirSpec::new().record(301));
    v.delete(short);
    v.mkdir_with(root, "Second life", DirSpec::new().record(301));
    v.mkdir_with(root, "Same number 302", DirSpec::new().record(302));
    v.mkdir_with(root, "Same number 303", DirSpec::new().record(303));
    let temp = v.add_file(
        root,
        "temp.dat",
        FileSpec::tagged(derive(seed, "temp"), 100).record(304),
    );
    v.delete(temp);
    v.add_file(
        root,
        "second life.dat",
        FileSpec::tagged(derive(seed, "file 304"), 7000).record(304),
    );
    populate(
        v,
        &TreeSpec {
            files: 130,
            name_shift: 7,
            ..TreeSpec::default()
        },
    );
    disk.render(gen2);

    let current = generation(&mut disk, "current", 2023, 256);
    disk.render(current);
    disk.finish()
}

/// A changepoint with a single contradicting record: the cut is dropped.
///
/// **Tests ALGORITHMS D2 ("a chosen cut is kept when it lies in the
/// intervals of contradictions with at least 2 different hi records";
/// otherwise its contradictions are unexplained, mark their `hi` record
/// `older_generation` and raise `mixed_generations`), G2 (a marked record
/// loses its overlap conflict), ARCHITECTURE §8 (leftovers nothing
/// contradicts stay in the main tree).** 2017: a volume whose records
/// 512–532 survive the 2020 format: `stale.bin` (record 512) and twenty
/// resident files in the root. 2020: a new file system whose `fresh.bin`
/// (record 300) was written onto the clusters of `stale.bin`. 2023: the
/// accident. The change times separate the generations (a changepoint lies
/// in `(300, 512]`), but only one 2017 record contradicts a 2020 record.
/// The cut is dropped: one segment, one `main` instance holding both file
/// systems, `stale.bin` marked `older_generation` and `overwritten`,
/// `fresh.bin` the winner with `overlap`, and the twenty other 2017 files
/// in the root of `main` without a flag. An implementation that keeps a cut
/// supported by one record splits them off as an `older` instance.
pub fn generation_cut_dropped(seed: u64) -> Built {
    let mut disk = Disk::new("generation_cut_dropped", seed, 128 * MIB);
    disk.describe(
        "Two generations with a time boundary but a single contradiction: no cut, one stale record marked.",
        &["D2", "G2"],
    );
    let gen1 = generation(&mut disk, "gen1", 2017, 1024);
    let v = disk.volume(gen1);
    let root = v.root();
    let cs = v.cluster_size();
    let early = v.mkdir(root, "Early");
    let mut i = 0;
    while v.reference(early).number + i + 1 < 512 {
        i += 1;
        v.add_tagged(early, format!("early-{i:03}.dat").as_str(), 100 + i);
    }
    let shared = vec![Run::at(v.total_clusters() - 4000, 6)];
    v.add_file(
        root,
        "stale.bin",
        FileSpec::tagged(derive(seed, "stale"), 6 * cs)
            .runs(shared.clone())
            .record(512),
    );
    for j in 0..20u64 {
        v.add_file(
            root,
            format!("left-behind-{j:02}.txt").as_str(),
            FileSpec::tagged(derive(seed, "left") + j, 300 + j).resident(),
        );
    }
    disk.render(gen1);

    let gen2 = generation(&mut disk, "gen2", 2020, 512);
    let v = disk.volume(gen2);
    let root = v.root();
    v.add_file(
        root,
        "fresh.bin",
        FileSpec::tagged(derive(seed, "fresh"), 6 * cs - 1)
            .runs(shared)
            .record(300),
    );
    populate(
        v,
        &TreeSpec {
            files: 130,
            name_shift: 7,
            ..TreeSpec::default()
        },
    );
    disk.render(gen2);

    let current = generation(&mut disk, "current", 2023, 256);
    disk.render(current);
    disk.finish()
}

/// Copies of MFT blocks: strays, not a second tree.
///
/// **Tests ALGORITHMS D1 (a copy lies at another origin), D4 (its record
/// numbers intersect those of the main instance: no join), D6 (a *copy
/// set* — an instance at least 90 % of whose records repeat number and
/// sequence of a record of a larger instance — goes to the strays), D7 (a
/// deleted file is no container).** The usual accident on a Windows 10
/// volume. Before the format, records 300–363 (64 records, 16 clusters)
/// were copied into free space, as shadow-copy storage keeps them, and
/// records 400–431 into the clusters of `defrag.tmp`, a file that was
/// deleted afterwards. The originals survive in the MFT. Each copy is a
/// fragment of its own with more than 16 records; an implementation without
/// the copy-set rule shows them as two more file systems.
///
/// Not covered on purpose: copies inside an *in-use* file. ALGORITHMS D5
/// runs the embedded test (D7) before the copy-set test (D6), so such a
/// copy would be an embedded instance, while ARCHITECTURE §7 item 3 expects
/// strays; the contract leaves that case open.
pub fn copies_of_mft_blocks(seed: u64) -> Built {
    let mut disk = Disk::new("copies_of_mft_blocks", seed, 96 * MIB);
    disk.describe(
        "Copies of MFT blocks in free space and inside a deleted file: copy sets, merged into the strays.",
        &["D1", "D4", "D6", "D7"],
    );
    let (old, _) = old_volume(
        &mut disk,
        VolumeParams::new(0, 96 * MIB).label("old"),
        &TreeSpec::default(),
    );
    let v = disk.volume_ref(old);
    let total = v.total_clusters();
    let spare = v.lcn_to_offset(total - 3000);
    let block_a = v.record_offset(300);
    let block_b = v.record_offset(400);
    assert!(v.node_at(431).is_some(), "the copied records must exist");
    let bytes_b = disk.image().read_vec(block_b, 32 * 1024);
    disk.copy(block_a, spare, 64 * 1024);
    let v = disk.volume(old);
    let root = v.root();
    let container = v.add_file(
        root,
        "defrag.tmp",
        FileSpec::new(ContentSpec::blob(Arc::new(bytes_b))).runs(vec![Run::at(total - 6000, 8)]),
    );
    v.delete(container);
    disk.render(old);
    let at = disk.volume_ref(old).lcn_to_offset(total - 6000);
    // the same bytes again: what the copy registers is what the file holds
    disk.copy(block_b, at, 32 * 1024);
    reformat(&mut disk, old);
    disk.finish()
}

/// Old extension records in free slots of the current `$MFT`.
///
/// **Tests ALGORITHMS C3, last case: a record in a free slot of a current
/// `$MFT` without `$STANDARD_INFORMATION` is old when its in-use flag is
/// still set, or when its base reference resolves to an old record of the
/// same `$MFT`; otherwise it is `post_format`, and its extents join the
/// overwritten set (C5).** A Windows 10 volume quick-formatted by a Windows
/// 10 that leaves unused slots unwritten (as in
/// [`super::accidents::quick_format_untouched_slots`]), then one new file
/// written, which takes slot 38. Three old multi-record files keep their
/// `$DATA` in an extension record:
///
/// * `alive-in-free-slots.bin` — base 41 and extension 132, in use: old,
///   shown, intact;
/// * `deleted-in-free-slots.bin` — base 40 and extension 131, deleted: the
///   free extension's base reference resolves to the deleted old base, so
///   both are old and the deleted file is shown complete;
/// * `orphaned-by-the-new-volume.bin` — base 38, deleted, extension 130:
///   slot 38 now holds the new file, so the free extension resolves to no
///   old record and counts as `post_format`: not shown, no orphan group,
///   and its clusters become part of the overwritten set.
///
/// `reused-the-clusters.bin` (record 42) was written onto the clusters of
/// the orphaned file after that was deleted, and deleted itself. Its bytes
/// are intact, but its extents meet the overwritten set: the rules call it
/// `overwritten` — the conservative side of C5.
pub fn extensions_in_free_slots(seed: u64) -> Built {
    let mut disk = Disk::new("extensions_in_free_slots", seed, 96 * MIB);
    disk.describe(
        "Old extension records without $STANDARD_INFORMATION in free slots of the current MFT.",
        &["C3", "C5", "E1"],
    );
    let old = disk.format(VolumeParams::new(0, 96 * MIB).label("old"));
    let v = disk.volume(old);
    let root = v.root();
    let cs = v.cluster_size();
    let multi = |ext: u64| {
        MultiSpec::new(ListForm::Resident)
            .data(&[(1, 0)])
            .extension_records(&[ext])
    };
    let orphaned = v.add_file(
        root,
        "orphaned-by-the-new-volume.bin",
        FileSpec::tagged(derive(seed, "orphaned"), 6 * cs)
            .fragments(3)
            .multi(multi(130)),
    );
    assert_eq!(
        v.reference(orphaned).number,
        38,
        "the first user record of the old volume"
    );
    let deleted = v.add_file(
        root,
        "deleted-in-free-slots.bin",
        FileSpec::tagged(derive(seed, "deleted"), 6 * cs - 17)
            .fragments(3)
            .record(40)
            .multi(multi(131)),
    );
    v.add_file(
        root,
        "alive-in-free-slots.bin",
        FileSpec::tagged(derive(seed, "alive"), 6 * cs - 33)
            .fragments(3)
            .record(41)
            .multi(multi(132)),
    );
    let runs = v.runs(orphaned);
    v.delete(orphaned);
    let reuser = v.add_file(
        root,
        "reused-the-clusters.bin",
        FileSpec::tagged(derive(seed, "reuser"), total_clusters(&runs) * cs)
            .runs(runs)
            .record(42),
    );
    v.delete(reuser);
    v.delete(deleted);
    populate(v, &TreeSpec::small());
    disk.render(old);
    let new = disk.format(disk.reformat_params(old).blank_slots(BlankSlots::Untouched));
    let v = disk.volume(new);
    let root = v.root();
    let first = v.add_file(
        root,
        "first new file.txt",
        FileSpec::tagged(derive(seed, "new"), 300).resident(),
    );
    assert_eq!(v.reference(first).number, 38);
    disk.render(new);
    disk.finish()
}
