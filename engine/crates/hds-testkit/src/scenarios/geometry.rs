//! Geometry: cluster, record and sector sizes, volume offsets, MFT
//! placement — what ALGORITHMS F has to detect instead of assume.

use crate::disk::{Built, Disk};
use crate::manifest::InstanceKind;
use crate::ondisk::runlist::Run;
use crate::partition::{write_mbr, Partition};
use crate::populate::{media_file, populate, TreeSpec};
use crate::rng::derive;
use crate::time;
use crate::volume::params::{Flavor, MftFragment, VolumeParams};
use crate::volume::spec::{DirSpec, FileSpec};

use super::{old_volume, reformat, MIB};

/// Old volume with `cluster` bytes per cluster, four folders with index
/// blocks, quick-formatted with the same geometry.
fn cluster_scenario(name: &str, seed: u64, cluster: u32, size: u64, big_folder: usize, records: u64) -> Built {
    let mut disk = Disk::new(name, seed, size);
    disk.describe(
        &format!("Volume with {cluster}-byte clusters, quick-formatted with the same geometry."),
        &["F1", "F2", "F6", "H7"],
    );
    let params = VolumeParams::new(0, size).label("old").cluster_size(cluster).mft_capacity(records);
    let old = disk.format(params);
    let tree = populate(disk.volume(old), &TreeSpec { big_folder, files: 200, ..TreeSpec::default() });
    // three more folders with index blocks, created late so that their
    // records survive: with the big one, four directories vote (F2, F6)
    let v = disk.volume(old);
    for d in 0..3 {
        let dir = v.mkdir(tree.folders[d], "Index test");
        for i in 0..45 {
            v.add_tagged(dir, format!("entry-{i:03}.dat").as_str(), 300 + 7 * i);
        }
    }
    disk.render(old);
    reformat(&mut disk, old);
    disk.finish()
}

/// 512-byte clusters.
///
/// **Tests ALGORITHMS F1, F2 (index votes: block VCNs count clusters and
/// run 0, 8, 16 …), H7; notes section 13 (aliasing).** An MFT record spans
/// two clusters, an index block eight. With 512-byte clusters the position
/// of a block inside its attribute is `vcn × 512`, the same as for every
/// cluster size above 4096: all blocks of one run vote for the same wrong
/// offsets under those hypotheses, which is why votes are counted per
/// directory. Four directories have index blocks.
pub fn cluster_512(seed: u64) -> Built {
    cluster_scenario("cluster_512", seed, 512, 64 * MIB, 150, 1024)
}

/// 1 KiB clusters: index blocks are numbered 0, 4, 8 ….
///
/// **Tests ALGORITHMS F1, F2.** See [`cluster_512`].
pub fn cluster_1k(seed: u64) -> Built {
    cluster_scenario("cluster_1k", seed, 1024, 64 * MIB, 150, 1024)
}

/// 2 KiB clusters: index blocks are numbered 0, 2, 4 ….
///
/// **Tests ALGORITHMS F1, F2.** See [`cluster_512`].
pub fn cluster_2k(seed: u64) -> Built {
    cluster_scenario("cluster_2k", seed, 2048, 64 * MIB, 150, 1024)
}

/// 8 KiB clusters: two index blocks per cluster, VCNs count 512-byte
/// units.
///
/// **Tests ALGORITHMS F1, F2 ("else `v × 512`").** The smallest cluster
/// size above the index block size. See [`cluster_512`].
pub fn cluster_8k(seed: u64) -> Built {
    cluster_scenario("cluster_8k", seed, 8192, 96 * MIB, 150, 1024)
}

/// 64 KiB clusters with one large directory.
///
/// **Tests ALGORITHMS F1, F2; notes sections 9 and 13.** Sixteen index
/// blocks share a cluster; their VCNs run 0, 8, 16 … in 512-byte units
/// while the `$INDEX_ALLOCATION` runlist counts 64 KiB clusters. The large
/// directory (700 files, about 20 blocks in two clusters) is the case the
/// architecture names explicitly; three more directories with one block
/// each make the true offset win over the aliases. The boot sector stores
/// the record size as -10 and the index block size as -12.
pub fn cluster_64k(seed: u64) -> Built {
    cluster_scenario("cluster_64k", seed, 65_536, 160 * MIB, 700, 2048)
}

/// 2 MiB clusters, the largest NTFS has.
///
/// **Tests ALGORITHMS F1 (upper bound), `parse_boot_sector` (sectors per
/// cluster stored as 0xF4), H7.** One cluster holds 2048 MFT records, so
/// even the freshly formatted `$MFT` is that large and the format destroys
/// old records 0–2047; the old volume has 4096 record slots and some 2400
/// records, nearly all of them resident files. `$MFTMirr` is one cluster,
/// a copy of all 2048 slots of the first MFT cluster. The image is 2 GiB of
/// almost nothing.
pub fn cluster_2m(seed: u64) -> Built {
    let size = 2052 * MIB;
    let mut disk = Disk::new("cluster_2m", seed, size);
    disk.describe(
        "Volume with 2 MiB clusters (4096 MFT records in two clusters), quick-formatted with the same geometry.",
        &["F1", "H7"],
    );
    let params = VolumeParams::new(0, size).label("old").cluster_size(2 << 20).mft_capacity(4096);
    let old = disk.format(params);
    let v = disk.volume(old);
    let root = v.root();
    // 2100 small files push the later records beyond slot 2047
    let mut folders = Vec::new();
    for d in 0..30 {
        folders.push(v.mkdir(root, format!("Folder {d:02}").as_str()));
    }
    for i in 0..2100usize {
        v.add_tagged(folders[i % 30], format!("small-{i:04}.dat").as_str(), 40 + (i as u64 % 500));
    }
    // these get records above 2047: they survive, with real clusters
    let late = v.mkdir(root, "Late");
    for i in 0..24 {
        let (name, file) = media_file(derive(seed, "late"), i, 30_000 + 4000 * i as u64);
        v.add_file(late, name.as_str(), file);
    }
    for i in 0..60 {
        v.add_tagged(late, format!("late-{i:02}.dat").as_str(), 100 + 50 * i);
    }
    v.add_file(late, "two-clusters.bin", FileSpec::tagged(derive(seed, "big"), (2 << 20) + 12345));
    disk.render(old);
    reformat(&mut disk, old);
    disk.finish()
}

/// 4 KiB MFT records on 512-byte sectors (`format /L`).
///
/// **Tests `probe_file_record` / `apply_fixups` (the fixup stride is 512
/// bytes whatever the record size: update sequence count 9), ALGORITHMS
/// D1 (origin grouping by record size).** Records are 4096 bytes with the
/// first attribute at 0x48; resident files can be 3 KiB large; the new
/// volume's first 256 records cover 1 MiB.
pub fn records_4k(seed: u64) -> Built {
    let mut disk = Disk::new("records_4k", seed, 96 * MIB);
    disk.describe(
        "Volume with 4096-byte MFT records on 512-byte sectors, quick-formatted likewise.",
        &["A3", "D1"],
    );
    let params = VolumeParams::new(0, 96 * MIB).label("old").record_size(4096);
    let (old, _) = old_volume(&mut disk, params, &TreeSpec::default());
    reformat(&mut disk, old);
    disk.finish()
}

/// A 4Kn disk: 4096-byte sectors, 4096-byte records.
///
/// **Tests `parse_boot_sector` (bytes per sector 4096; total sectors and
/// the backup boot sector's place count 4096-byte sectors), fixups (still
/// every 512 bytes), `BlockSource::sector_size`.** The manifest reports a
/// sector size of 4096; every structure lies on a 4096-byte boundary.
pub fn sector_4k(seed: u64) -> Built {
    let mut disk = Disk::new("sector_4k", seed, 96 * MIB);
    disk.set_sector_size(4096);
    disk.describe(
        "4Kn disk: volume with 4096-byte sectors and 4096-byte MFT records, quick-formatted likewise.",
        &["A3", "D1", "F2"],
    );
    let params = VolumeParams::new(0, 96 * MIB).label("old").sector_size(4096).record_size(4096);
    let (old, _) = old_volume(&mut disk, params, &TreeSpec::default());
    reformat(&mut disk, old);
    disk.finish()
}

/// A Windows XP-style disk: the partition starts at sector 63.
///
/// **Tests ALGORITHMS A3 (every 512-byte offset is probed; a record or
/// index block that crosses the end of an 8 MiB chunk is read on its own),
/// F2 (partition start as offset candidate).** Sector 63 is not on a 1 KiB
/// boundary: every `FILE` record of the image lies at an odd multiple of
/// 512 bytes. The MFT is placed so that old record 560 begins 512 bytes
/// before the 8 MiB mark of the image, and the folder `Many files` has its
/// first index block 512 bytes before the 16 MiB mark. The whole disk is
/// the scan range; an MBR names the partition.
pub fn partition_at_sector_63(seed: u64) -> Built {
    let offset = 63 * 512;
    let size = 96 * MIB;
    let mut disk = Disk::new("partition_at_sector_63", seed, size + MIB);
    disk.describe(
        "MBR disk with the NTFS partition at sector 63; a FILE record and an INDX block straddle 8 MiB chunk ends.",
        &["A3", "F2"],
    );
    // record k is at 32256 + lcn * 4096 + k * 1024; lcn 1900 puts record 560 at 8 MiB - 512
    let old = disk.format(VolumeParams::new(offset, size).label("old").mft_at(1900, 1024));
    let v = disk.volume(old);
    assert_eq!(v.record_offset(560), 8 * MIB - 512);
    // keep the cluster for the index block free while the tree is built
    v.allocator().alloc_at(4088, 1);
    populate(v, &TreeSpec { big_folder: 0, ..TreeSpec::default() });
    let root = v.root();
    // an index block at lcn 4088 begins at 16 MiB - 512
    v.allocator().free(4088, 1);
    let big = v.mkdir_with(root, "Many files", DirSpec::new().index_runs(vec![Run::at(4088, 1)]));
    assert_eq!(v.lcn_to_offset(4088), 16 * MIB - 512);
    for i in 0..150 {
        v.add_tagged(big, format!("item {i:05}.dat").as_str(), 400 + 3 * i);
    }
    assert!(v.node_at(560).is_some(), "record 560 must exist");
    disk.render(old);
    let new = disk.format(disk.reformat_params(old).mft_at(1900, 256));
    disk.render(new);
    write_mbr(&mut disk, 512, &[Partition { offset, size }]);
    disk.finish()
}

/// An old MFT in four fragments.
///
/// **Tests ALGORITHMS D1 (fragments by origin, signed arithmetic), D4
/// (segments are joined into one instance through parent references),
/// ARCHITECTURE §3 ("a fragmented MFT is only found completely by scanning
/// the whole range").** Records 0–255 are in the usual place and die in
/// the format, together with the runlist of the MFT. Records 256–511 lie
/// two thirds into the volume, 512–5119 at cluster 2000 (some 400 of these
/// slots are used, the rest is zeros), and 5120–5375 at cluster 1200: the
/// last piece of a grown MFT found room so early on the disk that its
/// `origin = offset − number × record size` is negative. Files and their
/// folders are spread over all three surviving fragments: `Late arrivals`
/// (record 5120) holds 60 files of its own fragment and 40 files whose
/// records are in the second fragment; `Many files` (third fragment) holds
/// 40 files with records in the last one.
pub fn fragmented_mft(seed: u64) -> Built {
    let mut disk = Disk::new("fragmented_mft", seed, 96 * MIB);
    disk.describe(
        "Volume whose MFT has four fragments (one with a negative origin), quick-formatted.",
        &["D1", "D4"],
    );
    let mut params = VolumeParams::new(0, 96 * MIB).label("old");
    let far = params.total_clusters() * 2 / 3;
    params.mft = vec![
        MftFragment { lcn: None, records: 256 },
        MftFragment { lcn: Some(far), records: 256 },
        MftFragment { lcn: Some(2000), records: 4608 },
        MftFragment { lcn: Some(1200), records: 256 },
    ];
    let old = disk.format(params);
    let tree = populate(disk.volume(old), &TreeSpec { files: 560, ..TreeSpec::default() });
    let v = disk.volume(old);
    assert!((v.record_offset(5120) as i64) < 5120 * 1024, "the last fragment has a negative origin");
    let root = v.root();
    let late = v.mkdir_with(root, "Late arrivals", DirSpec::new().record(5120));
    for i in 0..60u64 {
        let file = FileSpec::tagged(derive(seed, "arrival") + i, 700 + 90 * i).record(5121 + i);
        v.add_file(late, format!("arrival-{i:02}.dat").as_str(), file);
    }
    let many = tree.big_folder.expect("the tree has a big folder");
    for i in 0..40u64 {
        let file = FileSpec::tagged(derive(seed, "addendum") + i, 1500 + 40 * i).record(5200 + i);
        v.add_file(many, format!("addendum {i:02}.dat").as_str(), file);
    }
    let movers: Vec<_> =
        tree.files.iter().copied().filter(|&f| (256..296).contains(&v.reference(f).number)).collect();
    assert_eq!(movers.len(), 40, "forty files with records in the second fragment");
    for file in movers {
        v.set_parent(file, late);
    }
    disk.render(old);
    let new = reformat(&mut disk, old);
    assert!(
        disk.volume_ref(new).allocation().is_free(1200, 64),
        "the new volume must not write over the last old MFT fragment"
    );
    disk.finish()
}

/// The old MFT was not where Windows puts the new one.
///
/// **Tests ALGORITHMS F2 (MFT map: old record 0 named `$MFT` survives),
/// E9 (records 0–15 omitted), E5 (`$Extend` and everything below it
/// omitted), D6 (the old `$MFTMirr` copies are strays), D8.** The old
/// volume (Windows XP layout rules, but with its MFT three fifths into the
/// volume, as after a conversion from FAT or a defragmenter) keeps its
/// whole MFT: system records, root directory, every user record. The new
/// volume's metadata lands on old *data* instead: the new `$MFT`, `$LogFile`
/// and first-mount files overwrite old files. The old `$MFTMirr` in the
/// middle of the volume survives too: four more records numbered 0–3.
pub fn old_mft_elsewhere(seed: u64) -> Built {
    let mut disk = Disk::new("old_mft_elsewhere", seed, 96 * MIB);
    disk.describe(
        "Old volume with its MFT at three fifths of the volume; the quick format destroys none of its records.",
        &["F2", "E5", "E9", "D6", "D8"],
    );
    let mut params = VolumeParams::new(0, 96 * MIB).label("old").flavor(Flavor::WinXp);
    let lcn = params.total_clusters() * 3 / 5;
    params = params.mft_at(lcn, 1024);
    let (old, _) = old_volume(&mut disk, params, &TreeSpec::default());
    reformat(&mut disk, old);
    disk.expect_kind(old, Some(InstanceKind::Main));
    disk.finish()
}

/// The old volume was larger than the new partition and the whole disk is
/// scanned.
///
/// **Tests ALGORITHMS F2 (boot sector found as a *backup*: volume start =
/// hit − total sectors), F5 (volume size from the boot hit that agrees
/// with the geometry, not from the new partition).** A 128 MiB volume; a
/// 96 MiB partition was formatted over its start. The old backup boot
/// sector in the last sector of the disk survives and still describes the
/// old volume: 262 143 sectors, hidden sectors 0. Old files behind 96 MiB
/// are inside the range and recoverable; nothing is out of bounds.
pub fn old_volume_larger_whole_disk(seed: u64) -> Built {
    let size = 128 * MIB;
    let mut disk = Disk::new("old_volume_larger_whole_disk", seed, size);
    disk.describe(
        "128 MiB volume; a 96 MiB partition was formatted over its start; the whole disk is scanned.",
        &["F2", "F5"],
    );
    let old = disk.format(VolumeParams::new(0, size).label("old"));
    let tree = populate(disk.volume(old), &TreeSpec::default());
    let v = disk.volume(old);
    let cs = v.cluster_size();
    let behind = 100 * MIB / cs;
    v.add_file(tree.folders[1], "behind-the-new-partition.bin", FileSpec::tagged(derive(seed, "behind"), 64 * cs).runs(vec![Run::at(behind, 64)]));
    disk.render(old);
    let new = disk.format(
        VolumeParams::new(0, 96 * MIB).label("new").formatted_at(disk.volume_ref(old).clock() + time::DAY).mft_capacity(256),
    );
    disk.render(new);
    disk.finish()
}

/// Nothing that survived has a non-resident attribute.
///
/// **Tests ALGORITHMS F1 (fallback: "the cluster size of a matching boot
/// hit, else 4096") and H7 (resident content).** The volume has 8 KiB
/// clusters. The only files with clusters were the first 218 ever created
/// (their records are gone); what survives are 240 files of a few hundred
/// bytes in 60 folders of four entries each, so every surviving record has
/// resident data or a resident index and no runlist at all. The cluster
/// size cannot be voted on and has to come from the (new) boot sector,
/// which says 8192 — not the default 4096.
pub fn only_resident_survivors(seed: u64) -> Built {
    let mut disk = Disk::new("only_resident_survivors", seed, 96 * MIB);
    disk.describe(
        "8 KiB-cluster volume where every surviving record is resident: no attribute to derive the cluster size from.",
        &["F1", "H7"],
    );
    let old = disk.format(VolumeParams::new(0, 96 * MIB).label("old").cluster_size(8192));
    let v = disk.volume(old);
    let root = v.root();
    let first = v.mkdir(root, "First things");
    for i in 0..217 {
        v.add_file(first, format!("early-{i:03}.bin").as_str(), FileSpec::tagged(derive(seed, "early") + i, 9000 + 100 * i));
    }
    assert_eq!(v.reference(first).number + 217, 255);
    for d in 0..60u64 {
        let dir = v.mkdir(root, format!("Notes {d:02}").as_str());
        for i in 0..4u64 {
            v.add_file(dir, format!("note-{i}.txt").as_str(), FileSpec::tagged(derive(seed, "note") + 4 * d + i, 100 + 37 * i + d).resident());
        }
    }
    disk.render(old);
    reformat(&mut disk, old);
    disk.finish()
}

/// Only file signatures can tell where the old volume began.
///
/// **Tests ALGORITHMS F2 (signature anchors, "only when there are no index
/// votes"; at least 3 votes), F3, F4 ("by signature matches when at least
/// 20 files were checked"), F6.** The old volume started at sector 63; the
/// disk was re-partitioned and formatted at 1 MiB, which is the scan
/// range. The old boot sector is before the range, its backup was
/// overwritten by the new one, the first 256 KiB of the old MFT were
/// overwritten, and no surviving folder is large enough to have an index
/// block (the blocks of `First things` are still there, but its record is
/// not): neither an MFT map, nor a boot sector, nor an index vote exists
/// for the old volume, and the partition start is the wrong answer. What
/// is left are 40 intact PNG, JPEG, PDF, ZIP, BMP and WAV files whose
/// records say at which cluster they begin; the signature hits of the scan
/// say where they are.
pub fn signature_anchors(seed: u64) -> Built {
    let size = 128 * MIB;
    let old_offset = 63 * 512;
    let mut disk = Disk::new("signature_anchors", seed, size);
    disk.describe(
        "Old volume at sector 63 without boot sector, MFT start or index blocks in range; only media files anchor its offset.",
        &["F2", "F3", "F4", "F6"],
    );
    let mut old_params = VolumeParams::new(old_offset, size - old_offset).label("old");
    // well away from where the new volume puts its log and MFT
    let old_mft = old_params.total_clusters() * 7 / 10;
    old_params = old_params.mft_at(old_mft, 1024);
    let old = disk.format(old_params);
    let v = disk.volume(old);
    let root = v.root();
    let first = v.mkdir(root, "First things");
    // takes the clusters that the new volume's system files will land on
    v.add_file(first, "spacer.bin", FileSpec::tagged(derive(seed, "spacer"), 6 * MIB));
    for i in 0..216 {
        v.add_file(first, format!("tiny-{i:03}.txt").as_str(), FileSpec::tagged(derive(seed, "tiny") + i, 200).resident());
    }
    for d in 0..14 {
        let dir = v.mkdir(root, format!("Album {d:02}").as_str());
        for i in 0..3 {
            let (name, file) = media_file(derive(seed, "album"), d * 3 + i, 24_000 + 3000 * (d * 3 + i) as u64);
            v.add_file(dir, name.as_str(), file);
        }
    }
    disk.render(old);
    let new = disk.format(
        VolumeParams::new(MIB, size - MIB).label("new").formatted_at(disk.volume_ref(old).clock() + time::DAY).mft_capacity(256),
    );
    disk.render(new);
    // the first 256 KiB of the old MFT did not survive either
    let old_mft = disk.volume_ref(old).record_offset(0);
    disk.fill(old_mft, 256 * 1024, 0);
    write_mbr(&mut disk, 512, &[Partition { offset: MIB, size: size - MIB }]);
    disk.set_scan_range(MIB, size);
    disk.expect_kind(old, Some(InstanceKind::Main));
    disk.finish()
}
