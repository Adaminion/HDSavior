//! Damage and oddities: torn records, old headers, lying records, stale
//! copies, cycles, files that are not what their runlist says.

use crate::disk::{Built, Disk};
use crate::facts::{ArtifactState, BootFact};
use crate::manifest::{Flag, InstanceKind, Presence};
use crate::ondisk::attr::reparse_tag;
use crate::ondisk::boot::{encode_boot_sector, BootParams};
use crate::ondisk::record::{blank_model, BlankForm, ATTR_COMPRESSED};
use crate::ondisk::runlist::Run;
use crate::ondisk::FileRef;
use crate::populate::{populate, TreeSpec};
use crate::rng::derive;
use crate::time;
use crate::volume::params::{BlankSlots, Flavor, ListForm, VolumeParams};
use crate::volume::spec::{DirSpec, FileSpec, MultiSpec, Quirks, Times};

use super::{finish_formatted, old_volume, reformat, stage, stage_with, MIB};

/// Records whose multi-sector write was interrupted.
///
/// **Tests `apply_fixups` (a mismatch is reported, parsing continues),
/// ALGORITHMS A4 (fixup status), G1 `torn_record`, G3 (`uncertain`).**
/// Below `Test`: a file whose second 512-byte block still ends in the
/// previous update sequence number (its attributes all lie in the first
/// block and are unharmed); a file with 600 bytes of resident data that
/// reach into the torn block; a multi-record file whose extension record
/// is torn; a torn folder; and intact neighbours. Only the two fixup bytes
/// of a torn block differ from what a complete write leaves, which is what
/// a real torn write looks like when both versions of the sector agree
/// otherwise: the files' contents are intact, but nothing vouches for it.
/// (Block 0 holds the update sequence array itself; no interrupted write
/// can make it disagree with its own sector.)
pub fn torn_records(seed: u64) -> Built {
    let (mut disk, old, test) = stage("torn_records", seed, "FILE records with broken fixups.", &["A4", "G1", "G3"]);
    let v = disk.volume(old);
    let cs = v.cluster_size();
    let torn = |blocks: Vec<usize>| Quirks { torn_blocks: blocks, ..Quirks::default() };
    v.add_file(test, "second-block-torn.bin", FileSpec::tagged(derive(seed, "a"), 40_000).quirks(torn(vec![1])));
    v.add_file(test, "resident-torn.txt", FileSpec::tagged(derive(seed, "b"), 600).resident().quirks(torn(vec![1])));
    let mut quirks = Quirks::default();
    quirks.torn_extension_blocks = vec![(1, vec![1])];
    v.add_file(
        test,
        "extension-torn.bin",
        FileSpec::tagged(derive(seed, "c"), 8 * cs)
            .fragments(4)
            .multi(MultiSpec::new(ListForm::Resident).data(&[(1, 2), (2, 2)]))
            .quirks(quirks),
    );
    let dir = v.mkdir_with(test, "Torn folder", DirSpec::new().quirks(torn(vec![1])));
    v.add_tagged(dir, "inside.dat", 900);
    v.add_tagged(test, "intact.dat", 900);
    finish_formatted(disk, old)
}

/// A volume written by Windows 2000: no record carries its number.
///
/// **Tests `parse_file_record` (record number present exactly when the
/// update sequence offset is ≥ 0x30), ALGORITHMS D1 (unnumbered records
/// that are on no numbered fragment's grid go to the strays), D6;
/// ARCHITECTURE §8.** The old volume has the 42-byte record header
/// throughout (update sequence array at 0x2A, first attribute at 0x30) and
/// kept its MFT away from where Windows 10 put the new one, so all of its
/// records survive — but none says which number it has, and the only
/// numbered records near them belong to the current volume. They are
/// strays: listed, rated at most `uncertain`, not arranged in a tree.
pub fn pre31_volume(seed: u64) -> Built {
    let mut disk = Disk::new("pre31_volume", seed, 96 * MIB);
    disk.describe(
        "Windows 2000 volume (records without record number), quick-formatted by Windows 10; its MFT was elsewhere.",
        &["D1", "D6"],
    );
    let mut params = VolumeParams::new(0, 96 * MIB).label("old").flavor(Flavor::Win2000);
    let lcn = params.total_clusters() * 3 / 5;
    params = params.mft_at(lcn, 1024);
    let (old, _) = old_volume(&mut disk, params, &TreeSpec::small());
    reformat(&mut disk, old);
    disk.expect_kind(old, Some(InstanceKind::Strays));
    let mut built = disk.finish();
    for entry in built.manifest.entries.iter_mut().filter(|e| e.expect.presence == Presence::Shown) {
        // without numbers no parent reference can be resolved
        entry.expect.path = None;
    }
    built
}

/// Numbered and unnumbered records mixed in one MFT.
///
/// **Tests ALGORITHMS D1 ("a record without a number that lies on the grid
/// of a fragment and within 4096 records of its number span takes the
/// positional number and joins it").** A volume upgraded from Windows 2000
/// keeps old-format records until they are rewritten. Below `Test`: an
/// unnumbered file between numbered neighbours; an unnumbered folder whose
/// (numbered) children refer to it by the number it does not carry; a
/// multi-record file whose extension record is unnumbered. All of them
/// belong to the main instance at their positional number.
pub fn pre31_mixed(seed: u64) -> Built {
    let (mut disk, old, test) = stage(
        "pre31_mixed",
        seed,
        "A few records with the pre-3.1 header (no record number) among numbered ones.",
        &["D1"],
    );
    let v = disk.volume(old);
    let cs = v.cluster_size();
    let old_header = || Quirks { old_header: true, ..Quirks::default() };
    v.add_tagged(test, "numbered-before.dat", 2000);
    v.add_file(test, "unnumbered.bin", FileSpec::tagged(derive(seed, "u"), 30_000).quirks(old_header()));
    v.add_tagged(test, "numbered-after.dat", 2000);
    let dir = v.mkdir_with(test, "Unnumbered folder", DirSpec::new().quirks(old_header()));
    for i in 0..3 {
        v.add_tagged(dir, format!("numbered-child-{i}.dat").as_str(), 1500);
    }
    v.add_file(
        test,
        "unnumbered-records.bin",
        FileSpec::tagged(derive(seed, "m"), 8 * cs)
            .fragments(4)
            .multi(MultiSpec::new(ListForm::Resident).data(&[(0, 2), (1, 2)]))
            .quirks(old_header()),
    );
    finish_formatted(disk, old)
}

/// Record slots that were formatted but never used, in every known form.
///
/// **Tests ALGORITHMS A3 (1): "a record that is not in use and has no
/// attributes (never used) is counted and dropped".** The old volume's
/// unused slots are zero (Windows 10). Three runs of slots were given the
/// other forms by hand: 400–449 `FILE`, sequence 1, no attributes (Windows
/// 7 era, ntfs-3g); 450–499 the same with sequence 0 (Windows XP); 500–507
/// the mkntfs form of records 16–23 (sequence = slot, record number field
/// 0, a lone `$STANDARD_INFORMATION`). The first two forms must leave no
/// trace in the catalog. The mkntfs form is not "without attributes"; its
/// records claim number 0 and so fall into eight one-record fragments —
/// strays without name or data.
pub fn never_used_records(seed: u64) -> Built {
    let params = VolumeParams::new(0, 64 * MIB).label("old").blank_slots(BlankSlots::Zeros);
    let (mut disk, old, test) =
        stage_with("never_used_records", seed, "Never-used MFT slots in all known forms.", &["A3"], params);
    let v = disk.volume(old);
    for i in 0..40 {
        v.add_tagged(test, format!("used-{i:02}.dat").as_str(), 500);
    }
    let time = v.params().format_time;
    for slot in 400..450 {
        v.put_raw_record(slot, blank_model(BlankForm::Empty { sequence: 1 }, slot, 1024, 0x30_0000 + slot, time, false), false);
    }
    for slot in 450..500 {
        v.put_raw_record(slot, blank_model(BlankForm::Empty { sequence: 0 }, slot, 1024, 0, time, false), false);
    }
    for slot in 500..508 {
        v.put_raw_record(slot, blank_model(BlankForm::Mkntfs, slot, 1024, 0, time, false), false);
    }
    finish_formatted(disk, old)
}

/// Copies of MFT records elsewhere on the disk.
///
/// **Tests ALGORITHMS D6 (tiny fragments become strays), E6 (b) (a stale
/// copy of a folder record names a lost folder), E9 (copies of system
/// records are omitted), C3 (the current `$MFTMirr` is not inside the
/// current `$MFT`).** Shadow copies and defragmenters leave old copies of
/// MFT clusters behind. Before the format, the cluster holding old records
/// 36–39 was copied into free space: the copies of records 38 and 39 —
/// the folders `Photos` and `Documents`, in use — survive the format that
/// destroys the originals, and nothing else names these folders (the root
/// index was a single block). The cluster with records 300–303 was copied
/// too: duplicates of files that are also intact in the MFT. The other
/// eight top-level folders stay nameless.
pub fn stale_copies(seed: u64) -> Built {
    let mut disk = Disk::new("stale_copies", seed, 64 * MIB);
    disk.describe(
        "Old copies of MFT clusters in free space: two lost folders are named only by copies of their records.",
        &["D6", "E6", "E9", "C3"],
    );
    let old = disk.format(VolumeParams::new(0, 64 * MIB).label("old"));
    let tree = populate(
        disk.volume(old),
        &TreeSpec {
            folders: 10,
            early_media: 0,
            subfolders: 0,
            files: 320,
            media: 12,
            big_folder: 0,
            deleted: 0,
            max_bytes: 20_000,
            name_shift: 0,
        },
    );
    assert_eq!(disk.volume_ref(old).reference(tree.folders[0]).number, 38);
    disk.render(old);
    let v = disk.volume_ref(old);
    let (low, high) = (v.record_offset(36), v.record_offset(300));
    // free space well behind everything the old and the new volume use
    let spare = v.lcn_to_offset(v.total_clusters() - 2000);
    disk.copy(low, spare, 4096);
    disk.copy(high, spare + 64 * 4096, 4096);
    reformat(&mut disk, old);
    disk.finish()
}

/// Records that claim to be another record.
///
/// **Tests ALGORITHMS D1 (the record number is a claim: fragments are
/// grouped by `offset − number × record size`), D4 (a segment joins an
/// instance only if their record numbers are disjoint), E9.** Three files
/// below `Test` lie about their number:
///
/// * `claims-5300.bin` (slot 300 claims 5300) — a one-record fragment whose
///   parent reference resolves in the main instance and whose number is
///   free there: it joins the main instance and is shown in `Test`.
/// * `claims-5.bin` (slot 301 claims to be the root directory) — joins the
///   same way and is then omitted like every record 0–15.
/// * `claims-303.bin` (slot 302 claims 303, which really exists) — cannot
///   join and ends up among the strays.
///
/// `honest-303.bin` in slot 303 must stay untouched by its impostor.
pub fn wrong_record_numbers(seed: u64) -> Built {
    let (mut disk, old, test) =
        stage("wrong_record_numbers", seed, "Records whose stored record number is not their slot.", &["D1", "D4", "E9"]);
    let v = disk.volume(old);
    let claim = |n: u64| Quirks { claimed_number: Some(n), ..Quirks::default() };
    v.add_file(test, "claims-5300.bin", FileSpec::tagged(derive(seed, "a"), 20_000).record(300).quirks(claim(5300)));
    v.add_file(test, "claims-5.bin", FileSpec::tagged(derive(seed, "b"), 20_000).record(301).quirks(claim(5)));
    v.add_file(test, "claims-303.bin", FileSpec::tagged(derive(seed, "c"), 20_000).record(302).quirks(claim(303)));
    v.add_file(test, "honest-303.bin", FileSpec::tagged(derive(seed, "d"), 20_000).record(303));
    for i in 0..40 {
        v.add_tagged(test, format!("neighbour-{i:02}.dat").as_str(), 800);
    }
    let mut built = finish_formatted(disk, old);
    let m = &mut built.manifest;
    m.entry_mut("old", "Test/claims-5.bin").expect.presence = Presence::Hidden;
    m.entry_mut("old", "Test/claims-5.bin").expect.path = None;
    let liar = m.entry_mut("old", "Test/claims-303.bin");
    liar.expect.path = None;
    liar.expect.set(Flag::Stray);
    m.notes.push("claims-5300.bin is record 5300 of the main instance; claims-303.bin is a stray; claims-5.bin is omitted as record 5.".into());
    built
}

/// Folders that are their own ancestors.
///
/// **Tests ALGORITHMS E8 (cycles are cut by attaching the node where the
/// cycle is detected to `[Lost folders]`) and the "never … unbounded loop"
/// requirement.** Below `Test`: `A` and `B` contain each other; `Self` is
/// its own parent; `X`, `Y`, `Z` form a ring of three. Each holds a file.
/// None of them can be reached from the root any more; all must still be
/// shown, somewhere below `[Lost folders]`, each exactly once. The manifest
/// states no path for them.
pub fn parent_cycles(seed: u64) -> Built {
    let (mut disk, old, test) = stage("parent_cycles", seed, "Folders whose parent references form cycles.", &["E8"]);
    let v = disk.volume(old);
    let a = v.mkdir(test, "A");
    let b = v.mkdir(a, "B");
    v.set_parent(a, b);
    let own = v.mkdir(test, "Self");
    v.set_parent(own, own);
    let x = v.mkdir(test, "X");
    let y = v.mkdir(x, "Y");
    let z = v.mkdir(y, "Z");
    v.set_parent(x, z);
    for (dir, name) in [(a, "in-a.dat"), (b, "in-b.dat"), (own, "in-self.dat"), (x, "in-x.dat"), (y, "in-y.dat"), (z, "in-z.dat")] {
        v.add_tagged(dir, name, 1200);
    }
    v.add_tagged(test, "reachable.dat", 1200);
    finish_formatted(disk, old)
}

/// Files whose content is not stored as plain clusters.
///
/// **Tests ALGORITHMS G1 (`compressed`, `encrypted`, `placeholder`,
/// `sparse`), G3 (`unsupported`), H2 (never written), A4 (reparse tag,
/// `WofCompressedData`).** Below `Test`:
///
/// * `onedrive-placeholder.docx` (tag 0x9000001A) and
///   `onedrive-variant.xlsx` (0x9000601A: bits 12–15 vary) — cloud
///   placeholders, sparse streams without clusters;
/// * `dedup-stub.vhdx` (0x80000013);
/// * `compact-os.dll` (0x80000017 and a `WofCompressedData` stream), and
///   `wof-stream-only.dll` (the stream without the tag);
/// * `lznt1.txt` — compression flag, 16-cluster units with holes;
/// * `tiny-compressed-flag.txt` — a *resident* file with the compression
///   flag: its bytes are raw and it must be recovered like any file;
/// * `efs.docx` — encryption flag and `$EFS` stream;
/// * `symlink` and `Junction` — reparse points that are neither
///   placeholders nor compressed;
/// * `plain.dat` — control.
pub fn reparse_and_flags(seed: u64) -> Built {
    let (mut disk, old, test) = stage(
        "reparse_and_flags",
        seed,
        "Cloud placeholders, dedup and WOF stubs, compressed and encrypted files.",
        &["G1", "G3", "H2", "A4"],
    );
    let v = disk.volume(old);
    let s = |label: &str| derive(seed, label);
    v.add_file(test, "onedrive-placeholder.docx", FileSpec::cloud_placeholder(s("c1"), 800_000, reparse_tag::CLOUD));
    v.add_file(test, "onedrive-variant.xlsx", FileSpec::cloud_placeholder(s("c2"), 123_456, reparse_tag::CLOUD_6));
    v.add_file(test, "dedup-stub.vhdx", FileSpec::dedup_stub(s("d"), 5_000_000));
    v.add_file(test, "compact-os.dll", FileSpec::wof_compressed(s("w"), 300_000));
    let mut stream_only = FileSpec::wof_compressed(s("w2"), 200_000);
    stream_only.reparse = None;
    stream_only.file_attributes &= !0x400;
    v.add_file(test, "wof-stream-only.dll", stream_only);
    v.add_file(test, "lznt1.txt", FileSpec::tagged(s("z"), 150_000).compressed());
    let mut tiny = FileSpec::tagged(s("t"), 40).resident();
    tiny.attr_flags = ATTR_COMPRESSED;
    v.add_file(test, "tiny-compressed-flag.txt", tiny);
    v.add_file(test, "efs.docx", FileSpec::tagged(s("e"), 70_000).encrypted());
    v.add_file(test, "symlink", FileSpec::tagged(0, 0).resident().reparse(reparse_tag::SYMLINK, vec![0x55; 0x40]));
    let junction = v.mkdir(test, "Junction");
    v.node_reparse(junction, reparse_tag::MOUNT_POINT, vec![0x33; 0x30]);
    v.add_tagged(test, "plain.dat", 9000);
    finish_formatted(disk, old)
}

/// Boot sectors that are not what they seem.
///
/// **Tests `parse_boot_sector` (validity), ALGORITHMS A3 (3) (`BootHit`),
/// C1 (a boot sector without an `$MFT` behind it is no current volume), F2
/// (boot candidates must survive validation).** The usual accident, plus:
///
/// * a perfectly valid NTFS boot sector in free space describing a volume
///   that is not there (its MFT position holds file data);
/// * a copy of the *current* boot sector in the middle of the volume, as
///   found inside backup files — from there, "offset + MFT LCN" points
///   nowhere;
/// * four sectors with the NTFS signature and broken geometry (bytes per
///   sector 0, bytes per sector 768, sectors per cluster 0x81, record size
///   byte 0): these must not even become boot hits.
///
/// The manifest lists the first two under `boot_sectors`; the broken ones
/// are named in `notes`.
pub fn bad_boot_sectors(seed: u64) -> Built {
    let mut disk = Disk::new("bad_boot_sectors", seed, 96 * MIB);
    disk.describe("Quick format plus decoy boot sectors, valid and invalid.", &["A3", "C1", "F2"]);
    let (old, _) = old_volume(&mut disk, VolumeParams::new(0, 96 * MIB).label("old"), &TreeSpec::default());
    let new = reformat(&mut disk, old);
    let v = disk.volume_ref(new);
    let current = v.boot_params();
    let free = v.lcn_to_offset(v.total_clusters() - 3000);
    // a valid description of a volume that does not exist
    let phantom = BootParams {
        bytes_per_sector: 512,
        cluster_size: 4096,
        total_sectors: 150_000,
        mft_lcn: 100,
        mftmirr_lcn: 2,
        record_size: 1024,
        index_block_size: 4096,
        hidden_sectors: 2048,
        serial: derive(seed, "phantom"),
    };
    let fact = |offset: u64, p: &BootParams| BootFact {
        offset,
        instance: new.0 as u32,
        backup: false,
        bytes_per_sector: p.bytes_per_sector,
        cluster_size: p.cluster_size,
        total_sectors: p.total_sectors,
        mft_lcn: p.mft_lcn,
        mftmirr_lcn: p.mftmirr_lcn,
        record_size: p.record_size,
        index_block_size: p.index_block_size,
        hidden_sectors: p.hidden_sectors,
        serial: p.serial,
        state: ArtifactState::Intact,
    };
    disk.write(free, &encode_boot_sector(&phantom));
    disk.track_boot_sector(fact(free, &phantom));
    let copy_at = free + MIB;
    disk.write(copy_at, &encode_boot_sector(&current));
    disk.track_boot_sector(fact(copy_at, &current));
    // broken ones
    let mut broken = Vec::new();
    for (i, (at, value)) in [(0x0B, [0u8, 0]), (0x0B, [0x00, 0x03]), (0x0D, [0x81, 0]), (0x40, [0, 0])].into_iter().enumerate() {
        let mut sector = encode_boot_sector(&phantom);
        sector[at] = value[0];
        if at == 0x0B {
            sector[at + 1] = value[1];
        }
        let offset = free + (2 + i as u64) * MIB;
        disk.write(offset, &sector);
        broken.push(offset);
    }
    disk.note(&format!(
        "Invalid NTFS boot sectors (must not be boot hits) at {broken:?}: bytes per sector 0, bytes per sector 768, sectors per cluster 0x81, record size 0."
    ));
    disk.note(&format!("Valid boot sectors without a volume at {free} (phantom) and {copy_at} (copy of the current one)."));
    disk.finish()
}

/// Holes, valid-data length and slack.
///
/// **Tests ALGORITHMS H7 (holes and everything from the initialized size
/// on are zeros; output truncated to the real size), G1 `sparse`,
/// `decode_runlist` (sparse runs do not move the LCN base).** Below `Test`:
///
/// * `sparse.bin` — data, a hole of 255 clusters, data; sparse flag and the
///   0x48-byte attribute header;
/// * `hole-without-flag.bin` — a hole in an attribute that is not flagged
///   sparse (as `$BadClus:$Bad` has it);
/// * `stale-tail.bin` — 12 clusters allocated, real size 40 000, initialized
///   size 10 000: the clusters hold stale bytes behind byte 10 000, which a
///   reader ignoring the initialized size returns instead of zeros;
/// * `never-written.bin` — initialized size 0 with a full allocation;
/// * `slack.bin` — real size 5000 in two clusters whose remainder holds
///   stale bytes;
/// * `preallocated.bin` — 20 clusters allocated for 3 clusters of data.
pub fn sparse_and_initialized(seed: u64) -> Built {
    let (mut disk, old, test) = stage(
        "sparse_and_initialized",
        seed,
        "Sparse files and files whose initialized size is below their size, with stale bytes on disk.",
        &["H7", "G1"],
    );
    let v = disk.volume(old);
    let cs = v.cluster_size();
    let mft = v.mft_extents()[0];
    let base = mft.lcn + mft.clusters + 500;
    let s = |label: &str| derive(seed, label);
    v.add_file(
        test,
        "sparse.bin",
        FileSpec::tagged(s("sparse"), 265 * cs - 10)
            .runs(vec![Run::at(base, 6), Run::hole(255), Run::at(base + 300, 4)])
            .sparse(),
    );
    v.add_file(
        test,
        "hole-without-flag.bin",
        FileSpec::tagged(s("hole"), 24 * cs).runs(vec![Run::at(base + 400, 8), Run::hole(8), Run::at(base + 500, 8)]),
    );
    let mut stale = FileSpec::tagged(s("stale"), 40_000).initialized(10_000);
    stale.extra_clusters = 2;
    v.add_file(test, "stale-tail.bin", stale);
    v.add_file(test, "never-written.bin", FileSpec::tagged(s("never"), 6 * cs).initialized(0));
    v.add_file(test, "slack.bin", FileSpec::tagged(s("slack"), 5000).non_resident());
    let mut pre = FileSpec::tagged(s("pre"), 3 * cs - 1).non_resident();
    pre.extra_clusters = 17;
    v.add_file(test, "preallocated.bin", pre);
    finish_formatted(disk, old)
}

/// Deleting, and reusing what was freed.
///
/// **Tests the reference rule (ALGORITHMS, vocabulary), E5 (a parent must
/// be a directory and must *resolve*), E6 (`Folder #P.s`), G2 (in use wins
/// over deleted), G1 `deleted`.** Below `Test`:
///
/// * `deleted.bin` — record free, sequence 2, content untouched.
/// * `Deleted folder` with two deleted files: their parent reference
///   `(N, 1)` meets a free record with sequence 2 and resolves.
/// * `Folder replaced by a file`: deleted with its child, then its record
///   was reused by the file `usurper.bin` (in use, sequence 2). The deleted
///   child still says `(N, 1)`; a file is no parent and sequence 2 in use
///   is another object: lost folder.
/// * `Folder replaced by a folder`: the same, but the new owner of the
///   record is a folder (`Newer folder`, sequence 2) with a child of its
///   own. The old child must not move into it.
/// * `victim.bin`, deleted, whose clusters were then reused by
///   `reuser.bin`: the deleted file is `overwritten`, the live one is not
///   touched by the conflict.
/// * `deleted-twice.bin` in a record whose sequence is now 3.
pub fn deleted_and_reused(seed: u64) -> Built {
    let (mut disk, old, test) =
        stage("deleted_and_reused", seed, "Deleted files and folders, reused records, reused clusters.", &["E5", "E6", "G1", "G2"]);
    let v = disk.volume(old);
    let cs = v.cluster_size();
    let s = |label: &str| derive(seed, label);
    let gone = v.add_file(test, "deleted.bin", FileSpec::tagged(s("gone"), 30_000));
    let dir = v.mkdir(test, "Deleted folder");
    v.add_tagged(dir, "child-1.dat", 5000);
    v.add_tagged(dir, "child-2.dat", 700);
    // folder -> file
    let by_file = v.mkdir(test, "Folder replaced by a file");
    v.add_file(by_file, "orphaned-by-a-file.dat", FileSpec::tagged(s("of"), 9000));
    let file_slot = v.reference(by_file).number;
    // folder -> folder
    let by_dir = v.mkdir(test, "Folder replaced by a folder");
    v.add_file(by_dir, "orphaned-by-a-folder.dat", FileSpec::tagged(s("od"), 9000));
    let dir_slot = v.reference(by_dir).number;
    let victim = v.add_file(test, "victim.bin", FileSpec::tagged(s("victim"), 10 * cs));
    let victim_runs = v.runs(victim);
    let twice = v.add_tagged(test, "first-life.tmp", 100);
    let twice_slot = v.reference(twice).number;
    v.add_tagged(test, "bystander.dat", 3000);

    v.delete(gone);
    v.delete_tree(dir);
    v.delete_tree(by_file);
    v.delete_tree(by_dir);
    v.delete(victim);
    v.delete(twice);
    v.add_file(test, "usurper.bin", FileSpec::tagged(s("usurper"), 12_000).record(file_slot));
    let newer = v.mkdir_with(test, "Newer folder", DirSpec::new().record(dir_slot));
    v.add_tagged(newer, "child-of-the-newer-folder.dat", 2000);
    v.add_file(test, "reuser.bin", FileSpec::tagged(s("reuser"), 10 * cs - 17).runs(victim_runs));
    let second = v.add_file(test, "deleted-twice.bin", FileSpec::tagged(s("twice"), 20_000).record(twice_slot));
    v.delete(second);
    finish_formatted(disk, old)
}

/// What can and cannot give a lost folder its name back.
///
/// **Tests ALGORITHMS E6 (index entries: DOS namespace only as a last
/// resort, highest LSN wins, stale blocks count; `Folder #P.s`), C4
/// (entries of the current volume do not count), E5.** A Windows 7 volume
/// with 8.3 aliases. Its top-level folders had records below 256; each has
/// one file that survives. After the format:
///
/// * `Archive 00` … `Archive 21`: their Win32 index entries were in the
///   root's first index block (destroyed); only their DOS aliases, which
///   sort behind, survive in a later block. They come back as `ARCHIV~1` ….
/// * `Archive 22`: its Win32 entry was the separator kept in the root's
///   `$INDEX_ROOT` (destroyed with record 5); alias only.
/// * `Archive 23` … `Archive 29`: Win32 and DOS entries survive: Win32.
/// * `Archive 25` was renamed to `Renamed 25` after a copy of the index
///   block had been left elsewhere (a shadow copy): the copy still says
///   `Archive 25` with a lower LSN; the newer name wins.
/// * `AAA first life` was deleted with its file and its record reused by
///   `AAA second life`; both are nameless now and share a record number
///   with different sequence numbers: `Folder #N.1` and `Folder #N.2`.
/// * `Inner survivor` (record 255) is named by the resident index root of
///   its surviving parent `Test`.
pub fn lost_folder_names(seed: u64) -> Built {
    let mut disk = Disk::new("lost_folder_names", seed, 96 * MIB);
    disk.describe(
        "Lost top-level folders named by Win32 entries, by DOS aliases only, by a stale index block, or not at all.",
        &["E5", "E6", "C4"],
    );
    let old = disk.format(VolumeParams::new(0, 96 * MIB).label("old").flavor(Flavor::Win7));
    let v = disk.volume(old);
    let root = v.root();
    let first_life = v.mkdir(root, "AAA first life");
    let shared = v.reference(first_life).number;
    let archives: Vec<_> = (0..30).map(|i| v.mkdir(root, format!("Archive {i:02}").as_str())).collect();
    // push the next automatic record number beyond 255
    let filler = v.mkdir(root, "Filler");
    for i in 0..(255 - v.reference(filler).number - 1) {
        v.add_tagged(filler, format!("F{i:03}.DAT").as_str(), 64);
    }
    // the last low record is kept for a folder below Test
    let test = v.mkdir_with(root, "Test", DirSpec::new().record(256));
    let inner = v.mkdir_with(test, "Inner survivor", DirSpec::new().record(255));
    assert!(v.node_at(254).is_some() && v.reference(test).number == 256);
    v.add_tagged(inner, "in inner survivor.dat", 3000);
    let gone = v.add_tagged(first_life, "from the first life.dat", 4000);
    for (i, &dir) in archives.iter().enumerate() {
        v.add_tagged(dir, format!("content of archive {i:02}.dat").as_str(), 2000 + 10 * i as u64);
    }
    // first life ends, the record number is used again
    v.delete(gone);
    v.delete(first_life);
    let second_life = v.mkdir_with(root, "AAA second life", DirSpec::new().record(shared));
    assert_eq!(v.reference(second_life), FileRef::new(shared, 2));
    v.add_tagged(second_life, "from the second life.dat", 4000);
    disk.render(old);
    // a shadow copy keeps the root's second index block as it is now
    let v = disk.volume_ref(old);
    let root_blocks = v.index_runs(v.root());
    let second_block = v.lcn_to_offset(root_blocks[1].lcn.expect("allocated"));
    let spare = v.lcn_to_offset(v.total_clusters() - 2000);
    disk.copy(second_block, spare, 4096);
    disk.volume(old).rename(archives[25], "Renamed 25");
    disk.render(old);
    let new = disk.format(disk.reformat_params(old).flavor(Flavor::Win7));
    disk.render(new);
    disk.finish()
}

/// Files that share clusters.
///
/// **Tests ALGORITHMS G2 (winner by instance kind, in use over deleted,
/// later MFT change time, higher LSN; `overlap` and `overwritten`).** Five
/// pairs below `Test`, each pair cross-linked onto the same clusters:
///
/// 1. two in-use files — the one changed later wins and gets `overlap`,
///    the other is `overwritten`;
/// 2. a deleted file and an in-use one written over it — the deleted one
///    is `overwritten`, the winner gets nothing and is intact;
/// 3. two deleted files — the later change time wins (here: not the one
///    whose bytes are on the disk), no `overlap`;
/// 4. two in-use files with the same change time — the higher LSN wins;
/// 5. two in-use files with the same change time and LSN — a full tie,
///    both get `overlap`, neither `overwritten`.
///
/// The bytes on disk are those of the file written last in each pair.
pub fn overlap_conflicts(seed: u64) -> Built {
    let (mut disk, old, test) = stage("overlap_conflicts", seed, "Pairs of files cross-linked onto the same clusters.", &["G2"]);
    let v = disk.volume(old);
    let cs = v.cluster_size();
    let mft = v.mft_extents()[0];
    let base = mft.lcn + mft.clusters + 800;
    let t0 = time::ymd(2021, 6, 1);
    let mut pair = |n: u64, later_first: bool| {
        let runs = vec![Run::at(base + 100 * n, 8)];
        let a = v.add_file(test, format!("pair{n}-a.bin").as_str(), FileSpec::tagged(derive(seed, "pa") + n, 8 * cs).runs(runs.clone()));
        let b = v.add_file(test, format!("pair{n}-b.bin").as_str(), FileSpec::tagged(derive(seed, "pb") + n, 8 * cs).forced_runs(runs));
        let (ta, tb) = if later_first { (t0 + time::HOUR, t0) } else { (t0, t0 + time::HOUR) };
        v.set_times(a, Times::all(ta));
        v.set_times(b, Times::all(tb));
        (a, b)
    };
    pair(1, false);
    let (deleted, _) = pair(2, false);
    let (d1, d2) = pair(3, true);
    let (e1, e2) = pair(4, false);
    let (f1, f2) = pair(5, false);
    v.delete(deleted);
    v.delete(d1);
    v.delete(d2);
    for node in [e1, e2, f1, f2] {
        v.set_times(node, Times::all(t0));
    }
    v.set_lsn(e1, 0x50_0000);
    v.set_lsn(e2, 0x50_1000);
    v.set_lsn(f1, 0x60_0000);
    v.set_lsn(f2, 0x60_0000);
    finish_formatted(disk, old)
}

/// Timestamps at the edges.
///
/// **Tests ALGORITHMS H8 (only values with `0 < FILETIME < 2^63` are
/// applied; checked conversion), PROTOCOL (times as Unix milliseconds or
/// `null`), E4/notes section 7 (times come from `$STANDARD_INFORMATION`,
/// not from the stale `$FILE_NAME` copy).** Files below `Test` whose
/// creation, modification, change and access times are: all 0; 1; the last
/// tick before 2^63; exactly 2^63; all ones; 1601-01-02; 1969-12-31;
/// 1980-01-01; 2038-01-19; 2107-12-31; and one with four different
/// ordinary times whose `$FILE_NAME` copy says something else entirely.
pub fn timestamp_edges(seed: u64) -> Built {
    let (mut disk, old, test) = stage("timestamp_edges", seed, "Files with extreme and invalid timestamps.", &["H8"]);
    let v = disk.volume(old);
    let cases: [(&str, u64); 10] = [
        ("zero.dat", 0),
        ("one-tick.dat", 1),
        ("last-valid.dat", (1 << 63) - 1),
        ("two-to-the-63.dat", 1 << 63),
        ("all-ones.dat", u64::MAX),
        ("year-1601.dat", time::ymd(1601, 1, 2)),
        ("year-1969.dat", time::ymd_hms(1969, 12, 31, 23, 59, 59)),
        ("year-1980.dat", time::ymd(1980, 1, 1)),
        ("year-2038.dat", time::ymd_hms(2038, 1, 19, 3, 14, 8)),
        ("year-2107.dat", time::ymd_hms(2107, 12, 31, 23, 59, 58)),
    ];
    for (i, (name, t)) in cases.into_iter().enumerate() {
        v.add_file(test, name, FileSpec::tagged(derive(seed, "t") + i as u64, 3000).times(Times::all(t)));
    }
    let node = v.add_tagged(test, "four-different-times.dat", 3000);
    v.set_times(
        node,
        Times {
            created: time::ymd_hms(2015, 3, 4, 5, 6, 7) + 1_234_567,
            modified: time::ymd_hms(2016, 4, 5, 6, 7, 8) + 7_654_321,
            mft_changed: time::ymd_hms(2021, 5, 6, 7, 8, 9) + 1,
            accessed: time::ymd_hms(2021, 6, 7, 8, 9, 10) + 9_999_999,
        },
    );
    finish_formatted(disk, old)
}

/// Named streams and files without an unnamed stream.
///
/// **Tests ALGORITHMS A4 ("alternate data streams are otherwise ignored"),
/// E2 (`no_data`; a base with an attribute list but no `$DATA` is
/// `missing_fragments`), G1.** Below `Test`:
///
/// * `download.exe` — data plus a `Zone.Identifier` stream: the stream
///   must not be taken for the file's content;
/// * `big-stream.bin` — a small unnamed stream and a 200 KiB named one
///   (named streams sort *after* the unnamed one in the record);
/// * `only-a-stream.dat` — a named stream but no unnamed `$DATA` at all:
///   `no_data`, 0 bytes;
/// * `no-data-attribute.dat` — neither;
/// * `empty.txt` — a resident `$DATA` of length 0: an ordinary empty file,
///   not `no_data`;
/// * `list-without-data.bin` — an attribute list (its name lives in an
///   extension record) but no `$DATA`: `missing_fragments`.
pub fn streams_and_no_data(seed: u64) -> Built {
    let (mut disk, old, test) =
        stage("streams_and_no_data", seed, "Alternate data streams; files without an unnamed stream.", &["A4", "E2", "G1"]);
    let v = disk.volume(old);
    let s = |label: &str| derive(seed, label);
    v.add_file(
        test,
        "download.exe",
        FileSpec::tagged(s("dl"), 60_000).stream("Zone.Identifier", crate::content::ContentSpec::tagged(s("zone"), 26)),
    );
    let mut big = FileSpec::tagged(s("small"), 100).resident();
    big.streams.push(crate::volume::spec::StreamSpec {
        name: "bigstream".into(),
        content: crate::content::ContentSpec::tagged(s("big"), 200 * 1024),
        non_resident: true,
    });
    v.add_file(test, "big-stream.bin", big);
    let mut only = FileSpec::tagged(0, 0).stream("payload", crate::content::ContentSpec::tagged(s("payload"), 5000));
    only.no_data_attribute = true;
    v.add_file(test, "only-a-stream.dat", only);
    let mut none = FileSpec::tagged(0, 0);
    none.no_data_attribute = true;
    v.add_file(test, "no-data-attribute.dat", none);
    v.add_file(test, "empty.txt", FileSpec::tagged(0, 0).resident());
    let mut list = FileSpec::tagged(0, 0).multi(MultiSpec::new(ListForm::Resident).names_in(1));
    list.no_data_attribute = true;
    v.add_file(test, "list-without-data.bin", list);
    finish_formatted(disk, old)
}
