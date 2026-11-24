//! What the damage scenarios promise in their documentation, checked on the
//! image and the manifest they return.

mod common;

use common::{assert_reads_back, build, expected_path, intact_records, must, must_not, read_back, shown, MIB, SEEDS};
use hds_testkit::disk::Built;
use hds_testkit::facts::{ArtifactState, RecordFact, RecordRole};
use hds_testkit::hash::sha256_hex;
use hds_testkit::manifest::{Entry, EntryKind, Flag, InstanceKind, Manifest, Presence, RecordState};
use hds_testkit::ondisk::FileRef;
use hds_testkit::time;

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

fn test_file<'a>(m: &'a Manifest, name: &str) -> &'a Entry {
    m.entry("old", &format!("Test/{name}"))
}

/// `true` when block `block` of the record at `offset` fails its fixup check.
fn fixup_broken(built: &Built, offset: u64, block: usize) -> bool {
    let bytes = built.image.read_vec(offset, 1024);
    let usa = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    bytes[block * 512 + 510..block * 512 + 512] != bytes[usa..usa + 2]
}

#[test]
fn torn_records_fail_exactly_the_fixup_they_should() {
    each("torn_records", |built| {
        let m = &built.manifest;
        let second = test_file(m, "second-block-torn.bin");
        let resident = test_file(m, "resident-torn.txt");
        let extension = test_file(m, "extension-torn.bin");
        let folder = m.entry("old", "Test/Torn folder");
        for e in [second, resident, folder] {
            let r = record(m, e.record);
            assert_eq!(r.torn_blocks, [1], "{}", e.path);
            assert!(!fixup_broken(built, r.offset, 0) && fixup_broken(built, r.offset, 1), "{}", e.path);
            must(e, Flag::TornRecord);
        }
        // the resident value reaches into the torn block
        let (_, value, len) = resident.resident_at.unwrap();
        assert!(value < 512 && value + len > 512 && len == 600);
        // the extension record is torn, the base is not
        assert!(record(m, extension.record).torn_blocks.is_empty());
        let ext = record(m, extension.extension_records[0].0);
        assert_eq!(ext.torn_blocks, [1]);
        assert!(fixup_broken(built, ext.offset, 1));
        assert!(record(m, extension.extension_records[1].0).torn_blocks.is_empty());
        must(extension, Flag::TornRecord);
        // nothing else is wrong with them: the content is there
        for e in [second, resident, extension] {
            assert!(e.must_recover_exactly(), "{}", e.path);
            assert_reads_back(built, e);
            must_not(e, Flag::MissingFragments);
        }
        // neighbours are clean, also the file inside the torn folder
        let inside = m.entry("old", "Test/Torn folder/inside.dat");
        assert_eq!(expected_path(inside), "Test/Torn folder/inside.dat");
        for e in [inside, test_file(m, "intact.dat")] {
            must_not(e, Flag::TornRecord);
            assert!(!fixup_broken(built, e.record_offset.unwrap(), 1));
        }
    });
}

#[test]
fn records_without_a_record_number() {
    each("pre31_volume", |built| {
        let m = &built.manifest;
        let old = m.instance("old");
        assert_eq!(old.expected_kind, Some(InstanceKind::Strays));
        let records = intact_records(m, "old");
        assert!(records.len() > 80 && records.iter().any(|r| r.slot == 0), "the whole old MFT survives");
        for r in &records {
            assert_eq!(r.number, None, "slot {}", r.slot);
            let bytes = built.image.read_vec(r.offset, 0x30);
            assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 0x2A, "update sequence array at 0x2A");
            if r.role != RecordRole::Blank {
                assert_eq!(u16::from_le_bytes([bytes[0x14], bytes[0x15]]), 0x30, "first attribute at 0x30");
            }
        }
        // every numbered record in the image belongs to the current volume
        let new = m.instance("new").id;
        assert!(m.records.iter().filter(|r| r.number.is_some()).all(|r| r.instance == new));
        let shown_entries: Vec<&Entry> = m.entries_of("old").filter(|e| shown(e)).collect();
        assert!(shown_entries.len() > 50);
        for e in shown_entries {
            assert!(e.expect.path.is_none(), "{}", e.path);
            must(e, Flag::Stray);
            if e.kind == EntryKind::File && !e.deleted && !e.system {
                assert!(e.truth.record_intact, "{}", e.path);
            }
        }
    });

    each("pre31_mixed", |built| {
        let m = &built.manifest;
        let unnumbered = test_file(m, "unnumbered.bin");
        assert_eq!(record(m, unnumbered.record).number, None);
        for name in ["numbered-before.dat", "numbered-after.dat"] {
            let e = test_file(m, name);
            assert_eq!(record(m, e.record).number, Some(e.record));
        }
        assert_eq!(test_file(m, "numbered-before.dat").record + 1, unnumbered.record);
        assert_eq!(test_file(m, "numbered-after.dat").record, unnumbered.record + 1);
        let folder = m.entry("old", "Test/Unnumbered folder");
        assert_eq!(record(m, folder.record).number, None);
        for i in 0..3 {
            let child = m.entry("old", &format!("Test/Unnumbered folder/numbered-child-{i}.dat"));
            assert_eq!(child.names[0].parent, FileRef::new(folder.record, 1));
            assert_eq!(record(m, child.record).number, Some(child.record));
            assert_eq!(expected_path(child), child.path);
        }
        let multi = test_file(m, "unnumbered-records.bin");
        assert_eq!(record(m, multi.record).number, None);
        let ext = record(m, multi.extension_records[0].0);
        assert_eq!((ext.number, ext.base), (None, FileRef::new(multi.record, 1)));
        for e in [unnumbered, multi] {
            assert!(e.must_recover_exactly());
            assert_eq!(expected_path(e), e.path);
            must_not(e, Flag::Stray);
            must_not(e, Flag::MissingFragments);
            assert_reads_back(built, e);
        }
    });
}

#[test]
fn never_used_slots_in_every_form() {
    each("never_used_records", |built| {
        let m = &built.manifest;
        for slot in 400..450 {
            let r = record(m, slot);
            assert_eq!((r.flags, r.sequence, r.number, r.attr_types.len()), (0, 1, Some(slot), 0));
            assert!(r.lsn != 0 && r.entry.is_none());
        }
        for slot in 450..500 {
            let r = record(m, slot);
            assert_eq!((r.flags, r.sequence, r.number, r.attr_types.len(), r.lsn), (0, 0, Some(slot), 0, 0));
        }
        for slot in 500..508 {
            let r = record(m, slot);
            assert_eq!((r.flags, r.sequence, r.number), (0, slot as u16, Some(0)), "the mkntfs form claims number 0");
            assert_eq!(r.attr_types, [0x10], "a lone $STANDARD_INFORMATION");
            assert!(r.names.is_empty() && r.data.is_empty());
        }
        // the other unused slots of the old volume are zeros
        let old = m.instance("old");
        let at = old.geometry.mft_extents[0].offset + 600 * 1024;
        assert!(built.image.is_zero(at, 1024));
        // and the 40 real files are untouched
        for i in 0..40 {
            let e = test_file(m, &format!("used-{i:02}.dat"));
            assert!(e.must_recover_exactly());
            assert!(e.record < 400);
        }
    });
}

#[test]
fn stale_copies_name_lost_folders() {
    each("stale_copies", |built| {
        let m = &built.manifest;
        let old = m.instance("old").id;
        let copies: Vec<&RecordFact> =
            m.records.iter().filter(|r| r.instance == old && r.role == RecordRole::StaleCopy).collect();
        assert_eq!(copies.iter().map(|r| r.slot).collect::<Vec<_>>(), [36, 37, 38, 39, 300, 301, 302, 303]);
        assert!(copies.iter().all(|r| r.state == ArtifactState::Intact && r.number == Some(r.slot)));
        // the originals of 36-39 are gone, those of 300-303 are still in the MFT
        for slot in 36..40 {
            assert!(!intact_records(m, "old").iter().any(|r| r.slot == slot));
        }
        for slot in 300..304 {
            let original = record(m, slot);
            let copy = copies.iter().find(|r| r.slot == slot).unwrap();
            assert_eq!(original.fnv1a64, copy.fnv1a64, "a byte-identical duplicate");
            assert_ne!(original.offset, copy.offset);
        }
        // Photos (38) and Documents (39) are named by the copies of their records alone
        for (name, number) in [("Photos", 38u64), ("Documents", 39)] {
            let folder = m.entry("old", name);
            assert_eq!((folder.record, folder.truth.record_state), (number, RecordState::Destroyed));
            assert_eq!(folder.truth.surviving_index_entries, 0, "no index entry names {name}");
            let copy = copies.iter().find(|r| r.slot == number).unwrap();
            assert_eq!((copy.flags, copy.names[0].name.as_str()), (3, name));
            let children: Vec<&Entry> = m.entries_of("old").filter(|e| shown(e) && e.parent == Some(folder.id)).collect();
            assert!(children.len() >= 5, "{name} has {} surviving children", children.len());
            for child in children {
                assert_eq!(expected_path(child), child.path);
            }
        }
        // the other eight folders stay nameless
        let mut nameless = 0;
        for number in 40..48u64 {
            let folder = m.entries_of("old").find(|e| e.record == number).unwrap();
            for child in m.entries_of("old").filter(|e| shown(e) && e.parent == Some(folder.id)) {
                assert_eq!(expected_path(child), format!("[Lost folders]/Folder #{number}/{}", child.name()));
                nameless += 1;
            }
        }
        assert!(nameless > 50);
        // copies of system-area records exist too and must not become files
        assert!(copies.iter().any(|r| r.names.first().is_some_and(|n| n.name == "System Volume Information")));
    });
}

#[test]
fn records_that_lie_about_their_number() {
    each("wrong_record_numbers", |built| {
        let m = &built.manifest;
        for (slot, claims) in [(300u64, 5300u64), (301, 5), (302, 303), (303, 303)] {
            assert_eq!(record(m, slot).number, Some(claims), "slot {slot}");
        }
        let far = test_file(m, "claims-5300.bin");
        assert_eq!((far.record, expected_path(far).as_str()), (300, "Test/claims-5300.bin"));
        assert!(far.must_recover_exactly());
        let root = test_file(m, "claims-5.bin");
        assert_eq!((root.expect.presence, root.expect.path.as_ref()), (Presence::Hidden, None));
        let liar = test_file(m, "claims-303.bin");
        assert!(shown(liar) && liar.expect.path.is_none());
        must(liar, Flag::Stray);
        let honest = test_file(m, "honest-303.bin");
        assert_eq!((honest.record, expected_path(honest).as_str()), (303, "Test/honest-303.bin"));
        must_not(honest, Flag::Stray);
        assert!(honest.must_recover_exactly());
        assert_reads_back(built, honest);
        // the two records that claim 303 are different files
        assert_ne!(record(m, 302).fnv1a64, record(m, 303).fnv1a64);
        assert_ne!(liar.sha256, honest.sha256);
    });
}

#[test]
fn folders_that_are_their_own_ancestors() {
    each("parent_cycles", |built| {
        let m = &built.manifest;
        let dir = |name: &str| m.entries_of("old").find(|e| e.kind == EntryKind::Dir && e.name() == name).unwrap();
        let reference = |e: &Entry| FileRef::new(e.record, e.sequence);
        let (a, b, own, x, y, z) = (dir("A"), dir("B"), dir("Self"), dir("X"), dir("Y"), dir("Z"));
        assert_eq!((a.names[0].parent, b.names[0].parent), (reference(b), reference(a)));
        assert_eq!(own.names[0].parent, reference(own));
        assert_eq!((x.names[0].parent, y.names[0].parent, z.names[0].parent), (reference(z), reference(x), reference(y)));
        for e in [a, b, own, x, y, z] {
            assert!(shown(e) && e.truth.record_intact, "{}", e.name());
            assert!(e.expect.path.is_none() && !e.truth.ancestors_intact, "{}", e.name());
        }
        // the records on disk say the same
        assert_eq!(record(m, own.record).names[0].parent, reference(own));
        let files: Vec<&Entry> = m.entries_of("old").filter(|e| e.name().starts_with("in-")).collect();
        assert_eq!(files.len(), 6);
        for f in files {
            assert!(shown(f) && f.expect.path.is_none() && f.must_recover_exactly(), "{}", f.name());
        }
        let reachable = test_file(m, "reachable.dat");
        assert_eq!(expected_path(reachable), "Test/reachable.dat");
    });
}

#[test]
fn files_whose_content_is_not_plain_clusters() {
    each("reparse_and_flags", |built| {
        let m = &built.manifest;
        // name, reparse tag, flags that must be set
        let cases: [(&str, Option<u32>, &[Flag]); 11] = [
            ("onedrive-placeholder.docx", Some(0x9000_001A), &[Flag::Placeholder, Flag::Sparse]),
            ("onedrive-variant.xlsx", Some(0x9000_601A), &[Flag::Placeholder, Flag::Sparse]),
            ("dedup-stub.vhdx", Some(0x8000_0013), &[Flag::Placeholder, Flag::Sparse]),
            ("compact-os.dll", Some(0x8000_0017), &[Flag::Compressed, Flag::Sparse]),
            ("wof-stream-only.dll", None, &[Flag::Compressed, Flag::Sparse]),
            ("lznt1.txt", None, &[Flag::Compressed, Flag::Sparse]),
            ("tiny-compressed-flag.txt", None, &[]),
            ("efs.docx", None, &[Flag::Encrypted]),
            ("symlink", Some(0xA000_000C), &[]),
            ("plain.dat", None, &[]),
            ("Junction", Some(0xA000_0003), &[]),
        ];
        for (name, tag, set) in cases {
            let e = test_file(m, name);
            assert!(shown(e), "{name}");
            assert_eq!(e.reparse_tag, tag, "{name}");
            assert_eq!(record(m, e.record).reparse_tag, tag, "{name}");
            if e.kind == EntryKind::Dir {
                continue;
            }
            for flag in [Flag::Compressed, Flag::Encrypted, Flag::Placeholder, Flag::Sparse] {
                if set.contains(&flag) {
                    must(e, flag);
                } else {
                    must_not(e, flag);
                }
            }
        }
        // placeholders: a size, and not one cluster
        for name in ["onedrive-placeholder.docx", "onedrive-variant.xlsx", "dedup-stub.vhdx", "compact-os.dll"] {
            let e = test_file(m, name);
            assert!(e.size > 100_000 && e.extents.iter().all(|x| x.offset.is_none()), "{name}");
            assert!(e.data_flags & 0x8000 != 0);
        }
        assert_eq!(test_file(m, "compact-os.dll").streams, ["WofCompressedData"]);
        assert_eq!(test_file(m, "wof-stream-only.dll").streams, ["WofCompressedData"]);
        // LZNT1: compression units of 16 clusters, each some data and a hole
        let lz = test_file(m, "lznt1.txt");
        assert_eq!(lz.data_flags & 0xFF, 1);
        assert!(lz.extents.chunks(2).take(2).all(|u| u[0].offset.is_some() && u[1].offset.is_none() && u[0].clusters + u[1].clusters == 16));
        assert_eq!(lz.truth.content_intact, None, "what is on disk is not the content");
        assert_ne!(Some(sha256_hex(&read_back(built, lz))), lz.sha256);
        // a resident file with the compression flag is raw
        let tiny = test_file(m, "tiny-compressed-flag.txt");
        assert!(tiny.resident && tiny.data_flags & 0xFF == 1 && tiny.must_recover_exactly());
        assert_reads_back(built, tiny);
        let efs = test_file(m, "efs.docx");
        assert!(efs.data_flags & 0x4000 != 0 && efs.truth.content_intact.is_none());
        assert!(record(m, efs.record).attr_types.contains(&0x100), "the $EFS stream");
        assert!(test_file(m, "plain.dat").must_recover_exactly());
    });
}

#[test]
fn decoy_boot_sectors() {
    each("bad_boot_sectors", |built| {
        let m = &built.manifest;
        assert_eq!(m.boot_sectors.len(), 4, "new primary and backup, the phantom, the copy");
        let phantom = m.boot_sectors.iter().find(|b| b.total_sectors == 150_000).expect("the phantom");
        let current = &m.boot_sectors[0];
        assert_eq!(current.offset, 0);
        let copy = m
            .boot_sectors
            .iter()
            .find(|b| b.offset != 0 && !b.backup && b.serial == current.serial)
            .expect("the copy of the current boot sector");
        assert_eq!(copy.offset, phantom.offset + MIB);
        assert_eq!(built.image.read_vec(copy.offset, 512), built.image.read_vec(0, 512));
        // neither leads to an $MFT
        for decoy in [phantom, copy] {
            let mft = decoy.offset + decoy.mft_lcn * decoy.cluster_size as u64;
            assert!(mft + 4 > m.image_size || built.image.read_vec(mft, 4) != b"FILE", "decoy at {}", decoy.offset);
            assert!(!m.records.iter().any(|r| r.offset == mft));
        }
        // the four broken ones carry the signature but no valid geometry
        let broken: Vec<(u64, usize, [u8; 2])> = vec![
            (phantom.offset + 2 * MIB, 0x0B, [0x00, 0x00]),
            (phantom.offset + 3 * MIB, 0x0B, [0x00, 0x03]),
            (phantom.offset + 4 * MIB, 0x0D, [0x81, 0x00]),
            (phantom.offset + 5 * MIB, 0x40, [0x00, 0x00]),
        ];
        for (offset, field, value) in broken {
            let sector = built.image.read_vec(offset, 512);
            assert_eq!(&sector[3..11], b"NTFS    ");
            assert_eq!(sector[510..], [0x55, 0xAA]);
            assert_eq!(sector[field], value[0]);
            if field == 0x0B {
                assert_eq!(sector[field + 1], value[1]);
            }
            assert!(!m.boot_sectors.iter().any(|b| b.offset == offset), "not a boot sector: {offset}");
            assert!(m.notes.iter().any(|n| n.contains(&offset.to_string())));
        }
        // the accident itself is the usual one
        assert_eq!(common::slots(&intact_records(m, "old")).first(), Some(&256));
    });
}

#[test]
fn holes_and_initialized_sizes() {
    each("sparse_and_initialized", |built| {
        let m = &built.manifest;
        let cs = 4096u64;
        let sparse = test_file(m, "sparse.bin");
        assert_eq!(sparse.extents.iter().map(|x| (x.clusters, x.offset.is_some())).collect::<Vec<_>>(), [(6, true), (255, false), (4, true)]);
        assert!(sparse.data_flags & 0x8000 != 0);
        assert_eq!(record(m, sparse.record).data[0].non_resident.as_ref().unwrap().compressed_size, Some(10 * cs));
        must(sparse, Flag::Sparse);
        // the second data run is addressed relative to the first, not to the hole
        assert_eq!(sparse.extents[2].lcn.unwrap(), sparse.extents[0].lcn.unwrap() + 300);

        let hole = test_file(m, "hole-without-flag.bin");
        assert_eq!(hole.data_flags, 0);
        assert!(hole.extents[1].offset.is_none());
        assert_eq!(record(m, hole.record).data[0].non_resident.as_ref().unwrap().compressed_size, None);
        must(hole, Flag::Sparse);

        let stale = test_file(m, "stale-tail.bin");
        assert_eq!((stale.size, stale.initialized_size, stale.allocated_size), (40_000, 10_000, 12 * cs));
        let at = stale.extents[0].offset.unwrap();
        assert!(!built.image.is_zero(at + 10_000, 30_000), "stale bytes lie behind the initialized size");
        let naive = built.image.read_vec(at, 40_000);
        assert_ne!(Some(sha256_hex(&naive)), stale.sha256, "reading the clusters as they are is wrong");
        must_not(stale, Flag::Sparse);

        let never = test_file(m, "never-written.bin");
        assert_eq!((never.size, never.initialized_size), (6 * cs, 0));
        assert!(!built.image.is_zero(never.extents[0].offset.unwrap(), 4096));
        assert_eq!(never.sha256.as_deref(), Some(sha256_hex(&vec![0u8; (6 * cs) as usize]).as_str()));

        let slack = test_file(m, "slack.bin");
        assert_eq!((slack.size, slack.allocated_size), (5000, 2 * cs));
        assert!(!built.image.is_zero(slack.extents[0].offset.unwrap() + 5000, 3000), "stale bytes in the slack");

        let pre = test_file(m, "preallocated.bin");
        assert_eq!((pre.size, pre.allocated_size), (3 * cs - 1, 20 * cs));

        for e in [sparse, hole, stale, never, slack, pre] {
            assert!(e.must_recover_exactly(), "{}", e.path);
            assert_reads_back(built, e);
            assert_eq!(expected_path(e), e.path);
        }
    });
}

#[test]
fn deleting_and_reusing() {
    each("deleted_and_reused", |built| {
        let m = &built.manifest;
        let gone = test_file(m, "deleted.bin");
        assert!(gone.deleted && gone.sequence == 1);
        assert_eq!((record(m, gone.record).flags, record(m, gone.record).sequence), (0, 2));
        must(gone, Flag::Deleted);
        must_not(gone, Flag::Overwritten);
        assert!(gone.must_recover_exactly());
        assert_reads_back(built, gone);

        // a deleted folder still resolves for its deleted children
        let folder = m.entry("old", "Test/Deleted folder");
        assert_eq!((record(m, folder.record).flags, record(m, folder.record).sequence), (2, 2), "free, directory");
        for child in ["child-1.dat", "child-2.dat"] {
            let e = m.entry("old", &format!("Test/Deleted folder/{child}"));
            assert_eq!(e.names[0].parent, FileRef::new(folder.record, 1));
            assert_eq!(expected_path(e), e.path);
            must(e, Flag::Deleted);
        }

        // record reused by a file, record reused by a folder
        for (old_folder, child, usurper) in [
            ("Folder replaced by a file", "orphaned-by-a-file.dat", "usurper.bin"),
            ("Folder replaced by a folder", "orphaned-by-a-folder.dat", "Newer folder"),
        ] {
            let was = m.entry("old", &format!("Test/{old_folder}"));
            assert_eq!((was.truth.record_state, was.expect.presence), (RecordState::Reused, Presence::Gone));
            let now = test_file(m, usurper);
            assert_eq!((now.record, now.sequence, now.deleted), (was.record, 2, false));
            assert_eq!((record(m, now.record).sequence, record(m, now.record).flags & 1), (2, 1));
            let orphan = m.entry("old", &format!("Test/{old_folder}/{child}"));
            assert_eq!(orphan.names[0].parent, FileRef::new(was.record, 1));
            assert_eq!(expected_path(orphan), format!("[Lost folders]/Folder #{}/{child}", was.record));
            must(orphan, Flag::Deleted);
        }
        let own_child = m.entry("old", "Test/Newer folder/child-of-the-newer-folder.dat");
        assert_eq!(expected_path(own_child), own_child.path);
        assert_eq!(own_child.names[0].parent.seq, 2);

        // clusters reused: the deleted file lost them
        let (victim, reuser) = (test_file(m, "victim.bin"), test_file(m, "reuser.bin"));
        assert_eq!(victim.extents[0].offset, reuser.extents[0].offset);
        assert!(victim.deleted && victim.truth.content_intact == Some(false));
        must(victim, Flag::Overwritten);
        must(victim, Flag::Deleted);
        must_not(reuser, Flag::Overwritten);
        must_not(reuser, Flag::Overlap);
        assert!(reuser.must_recover_exactly());
        assert_reads_back(built, reuser);

        // a slot with two lives
        let first = test_file(m, "first-life.tmp");
        let twice = test_file(m, "deleted-twice.bin");
        assert_eq!((first.truth.record_state, first.record), (RecordState::Reused, twice.record));
        assert_eq!((twice.sequence, record(m, twice.record).sequence, record(m, twice.record).flags), (2, 3, 0));
        must(twice, Flag::Deleted);
        assert!(test_file(m, "bystander.dat").must_recover_exactly());
    });
}

#[test]
fn what_names_a_lost_folder() {
    each("lost_folder_names", |built| {
        let m = &built.manifest;
        let old = m.instance("old").id;
        // index entries of the old root that can still be read, per child record
        let root_entries: Vec<(u64, u8, &str, bool, u64)> = m
            .index_blocks
            .iter()
            .filter(|b| b.instance == old && b.owner.number == 5 && b.state == ArtifactState::Intact)
            .flat_map(|b| b.entries.iter().map(move |e| (e.child.number, e.key.namespace, e.key.name.as_str(), b.stale_copy, b.lsn)))
            .collect();
        let content = |i: usize| m.entries_of("old").find(|e| e.name() == format!("content of archive {i:02}.dat")).unwrap();
        let mut by_alias = 0;
        let mut by_name = 0;
        for i in 0..30 {
            let file = content(i);
            let folder = file.names.iter().find(|n| n.namespace != 2).unwrap().parent;
            assert!(folder.number < 255, "the folder's record died in the format");
            let win32: Vec<&(u64, u8, &str, bool, u64)> =
                root_entries.iter().filter(|e| e.0 == folder.number && e.1 != 2).collect();
            let dos: Vec<&(u64, u8, &str, bool, u64)> = root_entries.iter().filter(|e| e.0 == folder.number && e.1 == 2).collect();
            let expected = expected_path(file);
            let top = expected.split('/').next().unwrap();
            if i == 25 {
                // two Win32 names survive: the stale copy's and the newer one
                assert_eq!(win32.len(), 2);
                let (stale, live): (Vec<&&(u64, u8, &str, bool, u64)>, Vec<_>) = win32.iter().partition(|e| e.3);
                assert_eq!((stale[0].2, live[0].2), ("Archive 25", "Renamed 25"));
                assert!(stale[0].4 < live[0].4, "the copy has the lower LSN");
                assert_eq!(top, "Renamed 25");
            } else if win32.is_empty() {
                assert_eq!(dos.iter().filter(|e| !e.3).count(), 1, "Archive {i:02}: its alias survives");
                assert_eq!(top, dos[0].2);
                assert!(top.starts_with("ARCHI") && top.contains('~'), "{top}");
                by_alias += 1;
            } else {
                assert_eq!(top, format!("Archive {i:02}"));
                assert!(!dos.is_empty(), "the alias survives too and must lose");
                by_name += 1;
            }
            assert!(file.must_recover_exactly());
        }
        assert_eq!((by_alias, by_name), (25, 4), "Archive 00-24 by alias, 26-29 by name");
        // two lives of one record number
        let first = m.entries_of("old").find(|e| e.name() == "from the first life.dat").unwrap();
        let second = m.entries_of("old").find(|e| e.name() == "from the second life.dat").unwrap();
        let number = first.names[0].parent.number;
        assert_eq!((first.names[0].parent.seq, second.names[0].parent), (1, FileRef::new(number, 2)));
        assert_eq!(expected_path(first), format!("[Lost folders]/Folder #{number}.1/from the first life.dat"));
        assert_eq!(expected_path(second), format!("[Lost folders]/Folder #{number}.2/from the second life.dat"));
        must(first, Flag::Deleted);
        assert!(!root_entries.iter().any(|e| e.0 == number), "nothing names either of them");
        // a folder named by the index root of its surviving parent
        let inner = m.entry("old", "Test/Inner survivor");
        assert_eq!((inner.record, inner.truth.record_state), (255, RecordState::Destroyed));
        let test = m.entry("old", "Test");
        let root = record(m, test.record).index_root.as_ref().expect("Test has a resident index");
        assert!(root.iter().any(|e| e.child == FileRef::new(255, 1) && e.key.name == "Inner survivor"));
        let deep = m.entry("old", "Test/Inner survivor/in inner survivor.dat");
        assert_eq!(expected_path(deep), deep.path);
        // the current volume's own root index names nothing of the old one
        let new = m.instance("new").id;
        assert!(m.index_blocks.iter().any(|b| b.instance == new && b.owner.number == 5));
    });
}

#[test]
fn cross_linked_files() {
    each("overlap_conflicts", |built| {
        let m = &built.manifest;
        let pair = |n: u32| (test_file(m, &format!("pair{n}-a.bin")), test_file(m, &format!("pair{n}-b.bin")));
        let state = |e: &Entry| (e.expect.flags_set.contains(&Flag::Overwritten), e.expect.flags_set.contains(&Flag::Overlap));
        for n in 1..=5 {
            let (a, b) = pair(n);
            assert_eq!(a.extents[0].offset, b.extents[0].offset, "pair {n} shares its clusters");
            for e in [a, b] {
                assert!(shown(e));
                // nothing is left open for these files
                for flag in [Flag::Overwritten, Flag::Overlap] {
                    assert!(e.expect.flags_set.contains(&flag) || e.expect.flags_clear.contains(&flag));
                }
            }
        }
        // 1: both in use, b changed later: b wins and is marked as overlapping
        let (a, b) = pair(1);
        assert!(a.times.mft_changed < b.times.mft_changed && !a.deleted && !b.deleted);
        assert_eq!((state(a), state(b)), ((true, false), (false, true)));
        // 2: deleted against in use
        let (a, b) = pair(2);
        assert!(a.deleted && !b.deleted);
        assert_eq!((state(a), state(b)), ((true, false), (false, false)));
        assert!(b.must_recover_exactly(), "the bytes on disk are those of the file in use");
        assert_reads_back(built, b);
        // 3: both deleted, a changed later: a wins although b's bytes are on disk
        let (a, b) = pair(3);
        assert!(a.deleted && b.deleted && a.times.mft_changed > b.times.mft_changed);
        assert_eq!((state(a), state(b)), ((false, false), (true, false)));
        assert_eq!((a.truth.content_intact, b.truth.content_intact), (Some(false), Some(true)));
        // 4: same time, b has the higher LSN
        let (a, b) = pair(4);
        assert!(a.times.mft_changed == b.times.mft_changed && a.lsn < b.lsn);
        assert_eq!((record(m, a.record).lsn, record(m, b.record).lsn), (0x50_0000, 0x50_1000));
        assert_eq!((state(a), state(b)), ((true, false), (false, true)));
        // 5: a full tie
        let (a, b) = pair(5);
        assert!(a.times.mft_changed == b.times.mft_changed && a.lsn == b.lsn);
        assert_eq!((state(a), state(b)), ((false, true), (false, true)));
    });
}

#[test]
fn timestamps_at_the_edges() {
    each("timestamp_edges", |built| {
        let m = &built.manifest;
        let cases: [(&str, u64, Option<i64>); 10] = [
            ("zero.dat", 0, None),
            ("one-tick.dat", 1, Some(-11_644_473_600_000)),
            ("last-valid.dat", (1 << 63) - 1, Some(910_692_730_085_477)),
            ("two-to-the-63.dat", 1 << 63, None),
            ("all-ones.dat", u64::MAX, None),
            ("year-1601.dat", time::ymd(1601, 1, 2), Some(-11_644_387_200_000)),
            ("year-1969.dat", time::ymd_hms(1969, 12, 31, 23, 59, 59), Some(-1000)),
            ("year-1980.dat", time::ymd(1980, 1, 1), Some(315_532_800_000)),
            ("year-2038.dat", time::ymd_hms(2038, 1, 19, 3, 14, 8), Some(2_147_483_648_000)),
            ("year-2107.dat", time::ymd_hms(2107, 12, 31, 23, 59, 58), Some(4_354_819_198_000)),
        ];
        for (name, filetime, unix_ms) in cases {
            let e = test_file(m, name);
            assert_eq!(
                (e.times.created, e.times.modified, e.times.mft_changed, e.times.accessed),
                (filetime, filetime, filetime, filetime),
                "{name}"
            );
            let on_disk = record(m, e.record).standard_information.expect("$STANDARD_INFORMATION").0;
            assert_eq!(on_disk, [filetime; 4], "{name}");
            assert_eq!(time::to_unix_ms(filetime), unix_ms, "{name}");
            assert!(e.must_recover_exactly());
            assert_eq!(expected_path(e), e.path);
        }
        let four = test_file(m, "four-different-times.dat");
        let t = four.times;
        assert!(t.created < t.modified && t.modified < t.mft_changed && t.mft_changed < t.accessed);
        assert!(t.created % 10_000_000 != 0, "sub-second precision");
        let r = record(m, four.record);
        assert_eq!(r.standard_information.unwrap().0, [t.created, t.modified, t.mft_changed, t.accessed]);
        assert!(r.names[0].times.iter().all(|&n| n != t.created && n != t.modified), "the $FILE_NAME copy is stale");
    });
}

#[test]
fn named_streams_and_missing_data() {
    each("streams_and_no_data", |built| {
        let m = &built.manifest;
        let download = test_file(m, "download.exe");
        assert_eq!((download.size, download.streams.as_slice()), (60_000, &["Zone.Identifier".to_string()][..]));
        assert!(download.must_recover_exactly());
        assert_reads_back(built, download);
        let streams = &record(m, download.record).data;
        assert_eq!(streams.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["", "Zone.Identifier"]);

        let big = test_file(m, "big-stream.bin");
        assert!(big.resident && big.size == 100 && big.streams == ["bigstream"]);
        let streams = &record(m, big.record).data;
        assert!(streams[0].name.is_empty() && streams[0].resident.is_some(), "the unnamed stream comes first");
        assert_eq!(streams[1].non_resident.as_ref().unwrap().real_size, 200 * 1024);
        assert_reads_back(built, big);

        let only = test_file(m, "only-a-stream.dat");
        assert_eq!((only.size, only.sha256.as_ref(), only.streams.as_slice()), (0, None, &["payload".to_string()][..]));
        must(only, Flag::NoData);
        let none = test_file(m, "no-data-attribute.dat");
        assert!(record(m, none.record).data.is_empty());
        must(none, Flag::NoData);

        let empty = test_file(m, "empty.txt");
        assert!(empty.resident && empty.size == 0);
        assert_eq!(empty.sha256.as_deref(), Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"));
        must_not(empty, Flag::NoData);
        must_not(empty, Flag::MissingFragments);

        let list = test_file(m, "list-without-data.bin");
        let base = record(m, list.record);
        assert!(base.attribute_list.is_some() && base.data.is_empty() && base.names.is_empty());
        must(list, Flag::MissingFragments);
        must_not(list, Flag::NoData);
        assert_eq!(expected_path(list), list.path);
    });
}
