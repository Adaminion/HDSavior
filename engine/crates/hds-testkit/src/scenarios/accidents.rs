//! Accidents and histories: formats over formats, activity on the new
//! volume, file systems inside files, volumes larger than the scan.

use crate::disk::{Built, Disk, VolumeId};
use crate::manifest::{InstanceKind, InstanceRole, Presence};
use crate::ondisk::runlist::{total_clusters, Run};
use crate::partition::{write_mbr, Partition};
use crate::populate::{populate, TreeSpec};
use crate::rng::derive;
use crate::time;
use crate::volume::params::{BlankSlots, Flavor, ListForm, MftFragment, VolumeParams};
use crate::volume::spec::{DirSpec, FileSpec, MultiSpec};

use super::{old_volume, reformat, MIB};

/// A healthy Windows 10 volume that nobody formatted.
///
/// **Tests ALGORITHMS C1, C2, C3.** The boot sector at the range start
/// leads to an in-use record 0 named `$MFT`, so this is a current volume:
/// every in-use record is `live` and hidden, the four deleted files are
/// `post_format`, and no instance has anything to show. An implementation
/// that does not recognise current volumes presents the user's healthy
/// files as "rescued".
///
/// The volume is valid enough for real tools: `ntfsinfo`, `ntfsls` and a
/// read-only `ntfs-3g` mount accept it (see `tests/real_tools.rs`).
pub fn current_volume(seed: u64) -> Built {
    let mut disk = Disk::new("current_volume", seed, 64 * MIB);
    disk.describe(
        "A healthy Windows 10 volume that was never formatted over: everything on it is live.",
        &["C1", "C2", "C3"],
    );
    old_volume(&mut disk, VolumeParams::new(0, 64 * MIB).label("live"), &TreeSpec::small());
    disk.finish()
}

/// The accident HDSavior is for: a Windows 10 volume quick-formatted by
/// Windows 10 on the same partition.
///
/// **Tests ALGORITHMS A2 (probe at the boot sector's MFT offset), C1–C3,
/// D8 (1), E5, E6 (a), E9, F2 (index votes), F3, I.** The new `$MFT` lands
/// exactly on the old one: old records 0–255 are gone (rewritten as system
/// files or zeros), among them the root, all 60 top-level folders, the 24
/// first media files and 40 second-level folders; everything from record
/// 256 on is intact. The new root index block replaces the old root's block
/// 0; its later blocks survive elsewhere and still name most top-level
/// folders, the rest end up as `Folder #N` under `[Lost folders]`. The
/// early media files have no record any more but their content is
/// contiguous and untouched: the carving case.
pub fn quick_format_same_geometry(seed: u64) -> Built {
    let mut disk = Disk::new("quick_format_same_geometry", seed, 96 * MIB);
    disk.describe(
        "Windows 10 volume, quick-formatted by Windows 10 on the same partition.",
        &["A2", "C1", "C2", "C3", "D8", "E5", "E6", "E9", "F2", "F3", "I"],
    );
    let (old, _) = old_volume(&mut disk, VolumeParams::new(0, 96 * MIB).label("old"), &TreeSpec::default());
    reformat(&mut disk, old);
    disk.finish()
}

/// The same accident on Windows 7: unused MFT slots hold blank `FILE`
/// records instead of zeros, 8.3 aliases exist.
///
/// **Tests ALGORITHMS A3 (1) (never-used records are counted and dropped),
/// E4 (a DOS alias never wins), C3.** Both the old and the new volume have
/// `FILE` records with sequence 1, no flags and no attributes in every
/// unused slot, with non-zero LSN (docs/notes/quick-format-behaviour.md
/// section 3). The new volume's 221 blank records sit where old user
/// records were. Long names have a DOS alias that comes first in the
/// record and in the parent's index.
pub fn quick_format_blank_records(seed: u64) -> Built {
    let mut disk = Disk::new("quick_format_blank_records", seed, 96 * MIB);
    disk.describe(
        "Windows 7 volume quick-formatted by Windows 7: blank FILE records in unused slots, 8.3 aliases.",
        &["A3", "C3", "E4"],
    );
    let params = VolumeParams::new(0, 96 * MIB).label("old").flavor(Flavor::Win7);
    let (old, _) = old_volume(&mut disk, params, &TreeSpec::default());
    let new = disk.format(disk.reformat_params(old).flavor(Flavor::Win7));
    disk.render(new);
    disk.finish()
}

/// A quick format that does not touch the unused slots of its new MFT.
///
/// **Tests ALGORITHMS C3, third case.** The notes leave open whether
/// Windows 10 really writes zeros into slots 16–255 of a fresh MFT. Here it
/// does not: old records 38–255 are still in place, inside the extents and
/// below the initialized size of the live `$MFT`, in slots the live bitmap
/// calls free, with MFT change times before the format. They are old
/// records and must be shown like any other; only old records 0–37 were
/// replaced. An implementation that hides everything inside the live MFT
/// loses 218 files and folders.
pub fn quick_format_untouched_slots(seed: u64) -> Built {
    let mut disk = Disk::new("quick_format_untouched_slots", seed, 96 * MIB);
    disk.describe(
        "Quick format whose new MFT leaves its unused slots unwritten: old records survive inside the live MFT.",
        &["C3"],
    );
    let (old, _) = old_volume(&mut disk, VolumeParams::new(0, 96 * MIB).label("old"), &TreeSpec::default());
    let new = disk.format(disk.reformat_params(old).blank_slots(BlankSlots::Untouched));
    disk.render(new);
    disk.finish()
}

/// A 4 KiB-cluster volume quick-formatted with 64 KiB clusters, then used.
///
/// **Tests ALGORITHMS F1 (cluster size from the old attributes, not from
/// the boot sector), C3/C5 (extents of files deleted on the current volume
/// are mapped with the *current* geometry), D1.** The partition size is not
/// a multiple of 64 KiB × 3, so the new `$MFT` starts 20 records before
/// the old one: new records 0–255 cover old records 0–235, and old records
/// 236–255 survive right behind them. The new front area (`$MFTMirr` at
/// LCN 2 of 64 KiB, `$UpCase`, first-mount files) lies partly on old user
/// data. The new file `new-live.bin` lies on the old
/// `Backup/written-over.bin`, and the new, deleted `new-deleted.bin` on the
/// old `Backup/written-over-then-freed.bin`; the deleted one's runlist
/// counts 64 KiB clusters.
pub fn quick_format_cluster_change(seed: u64) -> Built {
    let size = 100 * MIB;
    let mut disk = Disk::new("quick_format_cluster_change", seed, size);
    disk.describe(
        "4 KiB-cluster volume quick-formatted with 64 KiB clusters; new files written and deleted afterwards.",
        &["F1", "C3", "C5", "D1"],
    );
    let old = disk.format(VolumeParams::new(0, size).label("old"));
    let tree = populate(disk.volume(old), &TreeSpec::default());
    // two old files for the new volume to write over, with enough between
    // them that the 64 KiB clusters covering one cannot touch the other
    let v = disk.volume(old);
    let home = tree.folders[5];
    let victims = [
        v.add_file(home, "written-over.bin", FileSpec::tagged(derive(seed, "victim 1"), 300_000)),
        v.add_file(home, "between.bin", FileSpec::tagged(derive(seed, "between"), 200_000)),
        v.add_file(home, "written-over-then-freed.bin", FileSpec::tagged(derive(seed, "victim 2"), 300_000)),
    ];
    let ranges = [v.byte_ranges(victims[0])[0], v.byte_ranges(victims[2])[0]];
    disk.render(old);
    let new = disk.format(disk.reformat_params(old).cluster_size(65_536));
    let new_data_start = {
        let v = disk.volume_ref(new);
        v.lcn_to_offset(v.allocation().cursor())
    };
    assert!(ranges[0].0 >= new_data_start, "the old files must lie behind the new system files");
    let v = disk.volume(new);
    let root = v.root();
    // whole 64 KiB clusters that cover the first run of each victim
    let cover = |v: &crate::volume::Volume, (offset, len): (u64, u64)| {
        let first = v.offset_to_lcn(offset);
        let last = v.offset_to_lcn(offset + len - 1);
        vec![Run::at(first, last - first + 1)]
    };
    let live = cover(v, ranges[0]);
    let dead = cover(v, ranges[1]);
    v.add_file(root, "new-live.bin", FileSpec::tagged(derive(seed, "live"), total_clusters(&live) * 65_536 - 7).runs(live));
    let gone = v.add_file(root, "new-deleted.bin", FileSpec::tagged(derive(seed, "dead"), total_clusters(&dead) * 65_536).runs(dead));
    v.delete(gone);
    disk.render(new);
    disk.finish()
}

/// A disk that was re-partitioned: the old Windows XP volume started at
/// sector 63, the new Windows 10 volume starts at 1 MiB.
///
/// **Tests ALGORITHMS A3 (structures at odd 512-byte offsets), F2 (MFT map,
/// backup boot sector with hidden sectors), E9 and E5 (old system records
/// survive and must be omitted), D8 (3), G1 `out_of_bounds`, S2.** The scan
/// range is the new partition. The old `$MFT` lies somewhere else than the
/// new one and survives completely, record 0 with the MFT's own runlist
/// included; so do the old `$Extend` children and `System Volume
/// Information`. The old primary boot sector is before the range; its
/// backup copy lies inside it and gives the volume start (sector 63) only
/// when read as a backup. Old files that begin before 1 MiB are out of
/// bounds. Every old structure sits at an odd multiple of 512 bytes. Unused
/// old slots are Windows XP's `FILE` records with sequence 0; names come in
/// DOS + Win32 pairs.
pub fn repartitioned_other_offset(seed: u64) -> Built {
    let size = 128 * MIB;
    let mut disk = Disk::new("repartitioned_other_offset", seed, size);
    disk.describe(
        "Windows XP volume at sector 63, disk re-partitioned and formatted by Windows 10 at 1 MiB; scan range: the new partition.",
        &["A3", "F2", "E5", "E9", "D8", "G1", "S2"],
    );
    let old_params = VolumeParams::new(63 * 512, 112 * MIB).label("old").flavor(Flavor::WinXp).mft_capacity(2048);
    let tree = TreeSpec { files: 700, ..TreeSpec::default() };
    let (old, _) = old_volume(&mut disk, old_params, &tree);
    let new = disk.format(
        VolumeParams::new(MIB, size - MIB)
            .label("new")
            .formatted_at(disk.volume_ref(old).clock() + time::DAY)
            .mft_capacity(256),
    );
    disk.render(new);
    write_mbr(&mut disk, 512, &[Partition { offset: MIB, size: size - MIB }]);
    disk.set_scan_range(MIB, size);
    disk.expect_kind(old, InstanceKind::Main);
    disk.finish()
}

/// The new volume was used after the format.
///
/// **Tests ALGORITHMS C3 (`live`, `post_format`, also for records without
/// `$STANDARD_INFORMATION`), C4, C5, E5 ("never a live record"), G1
/// `overwritten`, I2.** On the new volume: a folder created in record 41 —
/// the very `(number, sequence 1)` pair that the old `Videos/kept-0.dat` to
/// `kept-2.dat` (and whatever else was in that old top-level folder) still
/// name as their parent; `overwriter-live.bin`, written exactly over the
/// clusters of the old `Invoices/written-over.bin` and still there
/// (allocated in the new `$Bitmap`); `overwriter-deleted.bin`, written over
/// the old `Invoices/written-over-then-freed.bin` and deleted again (free
/// in the bitmap, the deleted record keeps the runlist); a deleted
/// fragmented file whose extents sit in two free extension records that
/// have no `$STANDARD_INFORMATION`; and six ordinary files that took the
/// first free clusters, which is where the old volume's first media files
/// were.
pub fn post_format_activity(seed: u64) -> Built {
    let mut disk = Disk::new("post_format_activity", seed, 96 * MIB);
    disk.describe(
        "Quick format, then new folders and files on the new volume, some deleted again.",
        &["C3", "C4", "C5", "E5", "G1", "I2"],
    );
    let old = disk.format(VolumeParams::new(0, 96 * MIB).label("old"));
    let tree = populate(disk.volume(old), &TreeSpec::default());
    // created last, so their records survive the format
    let v = disk.volume(old);
    for i in 0..3 {
        let file = FileSpec::tagged(derive(seed, "kept") + i, 20_000 + 9_000 * i);
        v.add_file(tree.folders[3], format!("kept-{i}.dat").as_str(), file);
    }
    let first = v.add_file(tree.folders[7], "written-over.bin", FileSpec::tagged(derive(seed, "victim 1"), 40_000));
    let second =
        v.add_file(tree.folders[7], "written-over-then-freed.bin", FileSpec::tagged(derive(seed, "victim 2"), 50_000));
    let (live_runs, dead_runs) = (v.runs(first), v.runs(second));
    let reused = v.reference(tree.folders[3]).number;
    disk.render(old);
    let new = disk.format(disk.reformat_params(old));
    let v = disk.volume(new);
    let root = v.root();
    let cs = v.cluster_size();
    let folder = v.mkdir_with(root, "New folder", DirSpec::new().record(reused));
    v.add_file(folder, "inside.dat", FileSpec::tagged(derive(seed, "inside"), 2000));
    v.add_file(
        root,
        "overwriter-live.bin",
        FileSpec::tagged(derive(seed, "live"), total_clusters(&live_runs) * cs - 100).runs(live_runs),
    );
    let dead = v.add_file(
        root,
        "overwriter-deleted.bin",
        FileSpec::tagged(derive(seed, "dead"), total_clusters(&dead_runs) * cs).runs(dead_runs),
    );
    v.delete(dead);
    let multi = v.add_file(
        root,
        "fragmented-then-deleted.bin",
        FileSpec::tagged(derive(seed, "multi"), 12 * cs)
            .fragments(6)
            .multi(MultiSpec::new(ListForm::Resident).data(&[(1, 3), (2, 3)])),
    );
    v.delete(multi);
    for i in 0..6 {
        v.add_file(root, format!("new-{i}.bin").as_str(), FileSpec::tagged(derive(seed, "new") + i, 150_000));
    }
    disk.render(new);
    disk.finish()
}

/// The new volume got so many files that its MFT grew over more old
/// records.
///
/// **Tests ALGORITHMS C2 (the live `$MFT` is what its record 0 says now,
/// not 256 records), C3.** The new `$MFT` holds 512 records. 330 new files
/// use slots 38–367; the rest up to 511 is zero. Old records 256–511 are
/// gone as well, old records from 512 on survive.
pub fn new_mft_grew(seed: u64) -> Built {
    let mut disk = Disk::new("new_mft_grew", seed, 96 * MIB);
    disk.describe("Quick format, then 330 new files: the new MFT grew to 512 records.", &["C2", "C3"]);
    let tree = TreeSpec { files: 500, ..TreeSpec::default() };
    let (old, _) = old_volume(&mut disk, VolumeParams::new(0, 96 * MIB).label("old"), &tree);
    let new = disk.format(disk.reformat_params(old).mft_capacity(512));
    let v = disk.volume(new);
    let root = v.root();
    let folder = v.mkdir(root, "Downloads");
    for i in 0..329 {
        v.add_file(folder, format!("setup-{i:03}.dat").as_str(), FileSpec::tagged(derive(seed, "grown") + i, 300 + 11 * i));
    }
    disk.render(new);
    disk.finish()
}

/// A current volume whose `$MFT` is fragmented and described through an
/// `$ATTRIBUTE_LIST`.
///
/// **Tests ALGORITHMS C2 ("following its `$ATTRIBUTE_LIST` into extension
/// records").** Record 0 of the new volume holds the first `$MFT` extent
/// (256 records) and a resident attribute list; the second extent (256
/// records, three quarters into the volume) is in record 15, an extension
/// record with base reference `(0, 1)`. New files occupy slots 38–337, so
/// 82 live records lie in the second fragment. An implementation that
/// reads only record 0 takes them for an unknown old file system.
pub fn live_mft_with_attribute_list(seed: u64) -> Built {
    let mut disk = Disk::new("live_mft_with_attribute_list", seed, 96 * MIB);
    disk.describe(
        "Quick format; the new $MFT has a second fragment that only its attribute list (record 15) describes.",
        &["C2"],
    );
    let (old, _) = old_volume(&mut disk, VolumeParams::new(0, 96 * MIB).label("old"), &TreeSpec::default());
    let mut params = disk.reformat_params(old);
    let second = params.total_clusters() * 3 / 4;
    params.mft = vec![MftFragment { lcn: None, records: 256 }, MftFragment { lcn: Some(second), records: 256 }];
    params.mft_attribute_list = Some(ListForm::Resident);
    let new = disk.format(params);
    let v = disk.volume(new);
    let root = v.root();
    for i in 0..300 {
        v.add_file(root, format!("new-{i:03}.dat").as_str(), FileSpec::tagged(derive(seed, "list") + i, 500));
    }
    disk.render(new);
    disk.finish()
}

/// A current volume whose `$MFT` is allocated further than it is
/// initialized.
///
/// **Tests ALGORITHMS C3 ("below the initialized size").** The new `$MFT`
/// has 512 record slots allocated but only 256 initialized; nothing was
/// ever written into slots 256–511, so the old records there are intact.
/// The new MFT bitmap has the bits of slots 300 and 310 set (the state a
/// crash between "bit set" and "record written" leaves). Those two slots
/// hold old, in-use records; they are beyond the initialized size and must
/// be treated as old records, not as live ones.
pub fn live_mft_partly_initialized(seed: u64) -> Built {
    let mut disk = Disk::new("live_mft_partly_initialized", seed, 96 * MIB);
    disk.describe(
        "Quick format; the new $MFT is allocated for 512 records, initialized for 256, with stray bitmap bits beyond.",
        &["C3"],
    );
    let (old, _) = old_volume(&mut disk, VolumeParams::new(0, 96 * MIB).label("old"), &TreeSpec::default());
    let mut params = disk.reformat_params(old).mft_capacity(512);
    params.mft_initialized_records = Some(256);
    let new = disk.format(params);
    disk.volume(new).mark_slot_in_bitmap(300);
    disk.volume(new).mark_slot_in_bitmap(310);
    disk.render(new);
    disk.finish()
}

/// Formats `count` generations over each other on one partition and
/// returns them oldest first; the last one is the current volume.
fn generations(disk: &mut Disk, size: u64, files: &[usize], capacities: &[u64], years: &[u32]) -> Vec<VolumeId> {
    let mut ids = Vec::new();
    let count = files.len();
    for (i, ((&files, &capacity), &year)) in files.iter().zip(capacities).zip(years).enumerate() {
        let label = if i + 1 == count { "current".to_string() } else { format!("gen{}", i + 1) };
        let params = VolumeParams::new(0, size)
            .label(&label)
            .volume_name(&label)
            .formatted_at(time::ymd(year, 5, 17))
            .mft_capacity(capacity);
        let id = disk.format(params);
        if files > 0 {
            // other folder names in every generation
            let tree = TreeSpec { files, name_shift: 7 * i, ..TreeSpec::default() };
            populate(disk.volume(id), &tree);
        }
        disk.render(id);
        ids.push(id);
    }
    ids
}

/// Two file systems before the current one on the same partition.
///
/// **Tests ALGORITHMS D2 (a generation cut needs a changepoint and
/// contradictions with two different stale records), D3, D4 (a kept cut
/// forbids joining), D8 (1), D9 `older`, E6 (index entries of the older
/// generation must not name folders of the newer one), G1
/// `older_generation`, G2.** 2017: a volume with some 1000 records. 2020:
/// formatted and filled again, its MFT grew to 512 records. 2023: the
/// accidental format. Every format put the MFT at the same place, so the
/// fragment now reads: current records 0–255, records 256–~490 of the 2020
/// file system (the one to rescue: `main`), zeros to 511, then records
/// 512–~1000 of the 2017 file system (`older`), with MFT change times
/// strictly older than everything before them. The 2020 files were written
/// over the first clusters of the 2017 files: dozens of in-use 2017 records
/// map clusters that in-use 2020 records map too, which is what keeps the
/// cut at 512. Both generations gave records 38–97 to their top-level
/// folders, under different names.
pub fn two_generations(seed: u64) -> Built {
    let mut disk = Disk::new("two_generations", seed, 128 * MIB);
    disk.describe(
        "Three formats on one partition (2017, 2020, 2023): the tail of the 2017 MFT survives behind the 2020 one.",
        &["D2", "D3", "D4", "D8", "D9", "E6", "G1", "G2"],
    );
    generations(&mut disk, 128 * MIB, &[650, 130, 0], &[1024, 512, 256], &[2017, 2020, 2023]);
    disk.finish()
}

/// Three file systems before the current one.
///
/// **Tests ALGORITHMS D2 (several kept cuts in one fragment), D3, D4, D9.**
/// As [`two_generations`] with one more layer: 2014 (about 1700 records,
/// MFT of 2048), 2017 (MFT of 1024), 2020 (MFT of 512), 2023 the accident.
/// Kept cuts at 512 and 1024 split the fragment into three segments; the
/// two older ones are `older` instances of their own.
pub fn three_generations(seed: u64) -> Built {
    let mut disk = Disk::new("three_generations", seed, 160 * MIB);
    disk.describe(
        "Four formats on one partition (2014, 2017, 2020, 2023): two older generations behind the main one.",
        &["D2", "D3", "D4", "D9"],
    );
    generations(&mut disk, 160 * MIB, &[1300, 650, 130, 0], &[2048, 1024, 512, 256], &[2014, 2017, 2020, 2023]);
    disk.finish()
}

/// Two generations whose MFT change times do not separate them.
///
/// **Tests ALGORITHMS D2 (no cut without a changepoint; every contradiction
/// is then unexplained and marks its stale record `older_generation`,
/// warning `mixed_generations`), D4 (one segment, one instance), E5 (a
/// marked record is never a parent).** As [`two_generations`], but the last
/// 40 records of the 2017 file system carry MFT change times from 2021 (a
/// wrong clock, or files touched by a tool): newer than the 2020 records in
/// front of them, so no changepoint exists. The contradictions are there
/// (shared clusters), but nothing explains them: the fragment stays one
/// segment, both file systems form one `main` instance, and exactly the
/// 2017 records that contradict a 2020 record are flagged
/// `older_generation`. That is the documented limit of the method. The
/// manifest states instance, kind and flags; it leaves the paths open,
/// because where 2017 files land among 2020 folders of equal record number
/// is an accident of the trees, not something the scenario is about.
pub fn generations_not_separable(seed: u64) -> Built {
    let mut disk = Disk::new("generations_not_separable", seed, 128 * MIB);
    disk.describe(
        "Like two_generations, but 40 records of the 2017 file system have MFT change times from 2021: no generation cut exists.",
        &["D2", "D4", "E5"],
    );
    let size = 128 * MIB;
    let gen1 = disk.format(
        VolumeParams::new(0, size).label("gen1").volume_name("gen1").formatted_at(time::ymd(2017, 5, 17)).mft_capacity(1024),
    );
    let tree = populate(disk.volume(gen1), &TreeSpec { files: 650, ..TreeSpec::default() });
    // the highest-numbered records: the big folder's files
    let v = disk.volume(gen1);
    for (i, &node) in tree.big_files.iter().rev().take(40).enumerate() {
        let mut times = v.times(node);
        times.mft_changed = time::ymd(2021, 2, 1) + i as u64 * time::MINUTE;
        v.set_times(node, times);
    }
    disk.render(gen1);
    let gen2 = disk.format(
        VolumeParams::new(0, size).label("gen2").volume_name("gen2").formatted_at(time::ymd(2020, 5, 17)).mft_capacity(512),
    );
    populate(disk.volume(gen2), &TreeSpec { files: 130, name_shift: 7, ..TreeSpec::default() });
    disk.render(gen2);
    let current = disk.format(
        VolumeParams::new(0, size).label("current").formatted_at(time::ymd(2023, 5, 17)).mft_capacity(256),
    );
    disk.render(current);
    disk.expect_kind(gen1, InstanceKind::Main);
    disk.expect_kind(gen2, InstanceKind::Main);
    let mut built = disk.finish();
    for entry in built.manifest.entries.iter_mut().filter(|e| e.expect.presence == Presence::Shown) {
        entry.expect.path = None;
    }
    built.manifest.notes.push(
        "gen1 and gen2 cannot be told apart by ALGORITHMS D2: a correct scan reports one instance holding both, \
         with the contradicting 2017 records marked older_generation. Paths are left open."
            .into(),
    );
    built
}

fn embedded(name: &str, seed: u64, fragments: u64) -> Built {
    // the inner disk: a plain volume image with more records than its host
    let mut inner = Disk::new("inner", derive(seed, "inner"), 24 * MIB);
    // the inner volume was used before the image file was copied onto the
    // host: the container is newer than everything inside it (D7)
    let inner_params = VolumeParams::new(0, 24 * MIB)
        .label("volume")
        .volume_name("VM disk")
        .mft_capacity(1024)
        .formatted_at(time::ymd(2020, 9, 1));
    let inner_volume = inner.format(inner_params);
    let inner_tree = TreeSpec {
        folders: 30,
        subfolders: 30,
        files: 500,
        media: 12,
        early_media: 6,
        big_folder: 120,
        deleted: 6,
        max_bytes: 9000,
        name_shift: 11,
    };
    populate(inner.volume(inner_volume), &inner_tree);
    inner.render(inner_volume);
    let inner_records = inner.volume_ref(inner_volume).user_nodes().len();

    let mut disk = Disk::new(name, seed, 128 * MIB);
    let (old, tree) = old_volume(
        &mut disk,
        VolumeParams::new(0, 128 * MIB).label("old"),
        &TreeSpec { files: 150, big_folder: 60, ..TreeSpec::default() },
    );
    let v = disk.volume(old);
    let root = v.root();
    // two more folders with index blocks: three voting directories validate
    // the host's geometry (ALGORITHMS F6), which D7 requires
    for d in 0..2 {
        let dir = v.mkdir(root, format!("Scans {d}").as_str());
        for i in 0..60 {
            v.add_tagged(dir, format!("page-{i:03}.dat").as_str(), 900);
        }
    }
    let documents = tree.folders[1];
    let spec = if fragments > 1 { FileSpec::tagged(0, 0).fragments(fragments) } else { FileSpec::tagged(0, 0) };
    let host = disk.embed(old, documents, "old-laptop.img", inner, spec);
    assert!(disk.volume_ref(old).reference(host).number >= 256, "the image file's record must survive the format");
    let host_records = disk.volume_ref(old).user_nodes().len();
    assert!(inner_records > host_records, "the inner file system must have more records than its host");
    disk.render(old);
    reformat(&mut disk, old);
    disk.note(
        "The volume inside the image file is an embedded instance (ALGORITHMS D7): its files are shown browse-only and \
         the image file itself is not `overwritten`. Its boot sector is not at the range start or a partition start, so \
         it never defines a current volume (C1), even though an in-use record 0 named $MFT lies where it points.",
    );
    disk.finish()
}

/// A disk image file lying on the formatted volume.
///
/// **Tests ALGORITHMS D7 (embedded instances), D8 (the main instance is not
/// simply the one with most records), E6 ("usable" index entries), G3
/// `unsupported`.** `Documents/old-laptop.img` is a 24 MiB raw image of
/// another NTFS volume with about 740 records — more than the 530 files
/// and folders the volume it is stored on ever had, of which fewer than
/// 300 records survive the format. Its `FILE` records, `INDX` blocks and boot
/// sector are all found by the scan. They form an instance of their own
/// whose records lie inside the extents of one in-use file of the host:
/// `embedded`, browse-only. The host has three folders with index blocks
/// and 36 media files, enough for a validated geometry. The image file is
/// stored in one piece.
pub fn embedded_volume(seed: u64) -> Built {
    let mut built = embedded("embedded_volume", seed, 1);
    built.manifest.description = "Quick-formatted volume holding a 24 MiB disk image file with an NTFS volume of its own.".into();
    built.manifest.rules = ["D7", "D8", "E6", "G3"].map(String::from).to_vec();
    built
}

/// The same with the image file stored in five fragments.
///
/// **Tests ALGORITHMS D1/D4 (the inner MFT is split over several origins
/// and joined by references), D7 (90 % of the records inside *any* data
/// extent of in-use files).** The fragments are whole clusters, so no inner
/// record is torn apart, but the inner volume no longer has one volume
/// offset: `Instance::offset_in_host` and the host file's extents say
/// where each of its clusters is.
pub fn embedded_volume_fragmented(seed: u64) -> Built {
    let mut built = embedded("embedded_volume_fragmented", seed, 5);
    built.manifest.description = "Like embedded_volume, with the image file in five fragments.".into();
    built.manifest.rules = ["D1", "D4", "D7"].map(String::from).to_vec();
    built
}

/// The old volume was larger than what gets scanned.
///
/// **Tests ALGORITHMS G1 `out_of_bounds`, H7 (runs outside the session
/// range yield zeros and are never reported OK), F5 (volume size from the
/// files when the boot sector at the old volume's offset is the new,
/// smaller volume's), safety invariant S2 (nothing outside the range is
/// read; the lost-volume range is protected beyond the scanned
/// partition).** The disk held one 160 MiB volume at 1 MiB. It was
/// re-partitioned: partition 1 (96 MiB, same start) was formatted, and
/// partition 2 behind it (64 MiB) holds a healthy `Data` volume that a scan
/// of partition 1 never sees. Partition 1 is the scan range. The old MFT
/// (at a third of 160 MiB) is inside the range; the new one lies at a third
/// of 96 MiB, so the old MFT survives completely. Old files placed by hand:
/// four entirely behind the range, one that starts inside and ends
/// outside, one inside. Because more than three files end behind the
/// range, the boot sectors of partition 1 do not decide the old volume's
/// size: its lost-volume range reaches into partition 2, which therefore
/// must be refused as a destination. The bytes behind the range are part
/// of the image so that a test can prove they are never read.
pub fn old_volume_larger_than_range(seed: u64) -> Built {
    let size = 162 * MIB;
    let first = Partition { offset: MIB, size: 96 * MIB };
    let second = Partition { offset: 97 * MIB, size: 64 * MIB };
    let mut disk = Disk::new("old_volume_larger_than_range", seed, size);
    disk.describe(
        "160 MiB volume; a 96 MiB partition was formatted over its start and is the scan range, a data partition behind it.",
        &["G1", "H7", "F5", "S2"],
    );
    let old = disk.format(VolumeParams::new(MIB, 160 * MIB).label("old"));
    let tree = populate(disk.volume(old), &TreeSpec::default());
    let v = disk.volume(old);
    let cs = v.cluster_size();
    let boundary = first.size / cs;
    let home = tree.folders[1];
    for (i, at) in [7000u64, 8000, 9000, 10_000].into_iter().enumerate() {
        let name = if i == 0 { "entirely-outside.bin".to_string() } else { format!("also-outside-{i}.bin") };
        let file = FileSpec::tagged(derive(seed, &name), 50 * cs).runs(vec![Run::at(boundary + at, 50)]);
        v.add_file(home, name.as_str(), file);
    }
    v.add_file(
        home,
        "straddles-the-end.bin",
        FileSpec::tagged(derive(seed, "straddle"), 40 * cs - 5).runs(vec![Run::at(boundary - 20, 20), Run::at(boundary + 2000, 20)]),
    );
    v.add_file(home, "entirely-inside.bin", FileSpec::tagged(derive(seed, "in"), 30 * cs).runs(vec![Run::at(boundary - 500, 30)]));
    disk.render(old);
    let formatted = disk.volume_ref(old).clock() + time::DAY;
    let new = disk.format(VolumeParams::new(first.offset, first.size).label("new").formatted_at(formatted).mft_capacity(256));
    disk.render(new);
    let data = disk.format(
        VolumeParams::new(second.offset, second.size).label("data").volume_name("Data").formatted_at(formatted + time::HOUR),
    );
    let v = disk.volume(data);
    let root = v.root();
    let docs = v.mkdir(root, "Documents");
    for i in 0..5u64 {
        v.add_tagged(docs, format!("letter-{i}.dat").as_str(), 3000 + 100 * i);
    }
    disk.render(data);
    write_mbr(&mut disk, 512, &[first, second]);
    disk.set_scan_range(first.offset, first.offset + first.size);
    disk.finish()
}

/// A whole disk with two partitions: one was quick-formatted, the other
/// was deleted and its start wiped.
///
/// **Tests ALGORITHMS D9 `other`, D8 (1), F2 (partition starts), F5, A2
/// (probe offsets per partition), S2.** MBR disk. Partition 1 (1 MiB, 64 MiB) is the
/// usual accident: old volume, quick format. Partition 2 (65 MiB, 62 MiB)
/// held a second volume from the same years; its partition entry is gone
/// and its first 64 KiB (boot sector included) and its `$MFT` records 0–15
/// were zeroed, so it is not a current volume but a second lost file
/// system: `other`. It has three large folders, so its geometry is
/// validated at its own volume offset, and its lost-volume range (F5) is
/// protected by S2 like that of `main`.
pub fn whole_disk_two_volumes(seed: u64) -> Built {
    let size = 128 * MIB;
    let mut disk = Disk::new("whole_disk_two_volumes", seed, size);
    disk.describe(
        "MBR disk: partition 1 quick-formatted, a second NTFS volume behind it deleted and beheaded.",
        &["A2", "D8", "D9", "F2", "F5", "S2"],
    );
    let first = Partition { offset: MIB, size: 64 * MIB };
    let second = Partition { offset: 65 * MIB, size: 62 * MIB };
    let (old, _) = old_volume(
        &mut disk,
        VolumeParams::new(first.offset, first.size).label("old"),
        &TreeSpec { files: 160, ..TreeSpec::default() },
    );
    reformat(&mut disk, old);
    let other = disk.format(VolumeParams::new(second.offset, second.size).label("other").volume_name("Archive"));
    let tree = populate(disk.volume(other), &TreeSpec { files: 160, big_folder: 80, ..TreeSpec::default() });
    let v = disk.volume(other);
    for d in 0..2 {
        let dir = v.mkdir(tree.folders[d], "Bulk");
        for i in 0..70 {
            v.add_tagged(dir, format!("bulk-{i:03}.dat").as_str(), 1200);
        }
    }
    disk.render(other);
    // the partition was deleted and its start wiped
    let mft = disk.volume_ref(other).record_offset(0);
    disk.fill(second.offset, 65_536, 0);
    disk.fill(mft, 16 * 1024, 0);
    let backup = disk.volume_ref(other).backup_boot_offset();
    disk.fill(backup, 512, 0);
    write_mbr(&mut disk, 512, &[first]);
    disk.set_role(other, InstanceRole::Lost);
    disk.expect_kind(other, InstanceKind::Other);
    disk.finish()
}
