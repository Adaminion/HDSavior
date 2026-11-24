//! What the accident scenarios promise in their documentation, checked on
//! the image and the manifest they return, for several seeds. If a scenario
//! stopped containing the situation it exists for, the engine tests built
//! on it would silently test nothing; these tests fail instead.

mod common;

use std::collections::BTreeMap;

use common::{
    assert_reads_back, build, catalog, changepoint_between, contradictions, expected_path,
    fragment_records, instance_of, intact_records, meets, must, must_not, shown, slots, warning,
    MIB, SEEDS,
};
use hds_testkit::disk::Built;
use hds_testkit::facts::{ArtifactState, RecordFact, RecordRole};
use hds_testkit::manifest::{
    Entry, EntryKind, Flag, FragmentFact, InstanceKind, InstanceRole, Manifest, Presence,
    RecordClass, RecordState,
};
use hds_testkit::populate::folder_name;

/// Runs `check` on the scenario built with every test seed.
fn each(name: &str, check: impl Fn(&Built)) {
    for seed in SEEDS {
        eprintln!("{name}, seed {seed}");
        check(&build(name, seed));
    }
}

/// The fragment of old records at the origin of the current volume's `$MFT`.
fn anchor_fragment(m: &Manifest) -> &FragmentFact {
    let current = m
        .instances
        .iter()
        .find(|i| i.role == InstanceRole::Current)
        .expect("a current volume");
    let origin = current.geometry.mft_extents[0].offset as i64;
    m.fragments
        .iter()
        .find(|f| f.origin == origin && f.record_size == current.geometry.record_size)
        .expect("old records at the origin of the current $MFT")
}

/// The manifest's D2 analysis of a fragment agrees with the independent
/// reading of `common`: the same contradictions, kept cuts that are
/// changepoints supported by two stale records, and exactly the stale
/// records of unexplained contradictions marked.
fn check_d2(m: &Manifest, f: &FragmentFact) {
    let records = fragment_records(m, f);
    let independent = contradictions(&records);
    let listed: BTreeMap<u64, u64> = f.contradictions.iter().map(|c| (c.hi, c.lo)).collect();
    assert_eq!(
        listed, independent,
        "contradictions of fragment {}",
        f.origin
    );
    for cut in &f.cuts {
        let support: Vec<u64> = f
            .contradictions
            .iter()
            .filter(|c| c.lo < cut.position && cut.position <= c.hi)
            .map(|c| c.hi)
            .collect();
        assert!(
            support.len() >= 2 && support.len() as u32 == cut.support,
            "cut {} support {support:?}",
            cut.position
        );
        assert!(cut.older_newest < cut.newer_oldest, "B_c < A_c");
    }
    for c in &f.contradictions {
        let explained = f
            .cuts
            .iter()
            .any(|cut| c.lo < cut.position && cut.position <= c.hi);
        assert_eq!(c.explained_by.is_some(), explained);
        let hi = records
            .iter()
            .find(|r| r.number == Some(c.hi))
            .expect("the stale record");
        assert_eq!(
            hi.expect.older_generation,
            Some(!explained),
            "mark of record {}",
            c.hi
        );
    }
}

fn entry_meets(e: &Entry, ranges: &[(u64, u64)]) -> bool {
    e.extents
        .iter()
        .any(|x| x.offset.is_some_and(|o| meets(ranges, o, x.bytes)))
}

#[test]
fn manifests_survive_json() {
    for name in [
        "current_volume",
        "post_format_activity",
        "embedded_volume_fragmented",
        "odd_names",
    ] {
        let built = build(name, 3);
        let json = built.manifest.to_json();
        let back = Manifest::from_json(&json).expect("valid JSON");
        // the bytes of an embedded image do not travel; everything else must
        assert_eq!(back.to_json(), json, "{name}");
        assert_eq!(back.records, built.manifest.records);
        assert_eq!(back.instances, built.manifest.instances);
        assert_eq!(
            Manifest::from_json(&built.manifest.to_json_pretty())
                .unwrap()
                .to_json(),
            json
        );
    }
}

#[test]
fn current_volume_is_live_and_nothing_else() {
    each("current_volume", |built| {
        let m = &built.manifest;
        assert_eq!(m.instances.len(), 1);
        assert_eq!(m.instances[0].role, InstanceRole::Current);
        assert_eq!(m.instances[0].expected_kind, None);
        assert!(m
            .entries
            .iter()
            .all(|e| e.expect.presence == Presence::Hidden));
        assert_eq!(m.boot_sectors.len(), 2);
        assert!(m.boot_sectors[1].backup && m.boot_sectors[1].offset == m.image_size - 512);
        // the live bitmap covers the MFT and at least the system area
        let live: u64 = m.instances[0].live_allocated.iter().map(|r| r.1).sum();
        assert!(live > 4 * MIB);
        assert!(m.overwritten_set.iter().map(|r| r.1).sum::<u64>() >= live);
        // four files were deleted on it: their slots are free in the bitmap
        let deleted: Vec<&Entry> = m.entries.iter().filter(|e| e.deleted).collect();
        assert_eq!(deleted.len(), 4);
        let in_bitmap = |slot: u64| {
            m.instances[0]
                .live_slots
                .iter()
                .any(|&(s, n)| (s..s + n).contains(&slot))
        };
        assert!(deleted.iter().all(|e| !in_bitmap(e.record)));
        assert!(m
            .entries
            .iter()
            .filter(|e| !e.deleted && e.record >= 24)
            .all(|e| in_bitmap(e.record)));
        // as on a real Windows volume: 0-15 marked, the reserved 16-23 not
        assert!(in_bitmap(0) && in_bitmap(15) && !in_bitmap(16) && !in_bitmap(23) && in_bitmap(24));
        // every live file reads back
        for e in m.user_files().filter(|e| !e.deleted) {
            assert!(e.must_recover_exactly(), "{}", e.path);
            assert_reads_back(built, e);
        }
    });
}

#[test]
fn quick_format_destroys_exactly_the_first_256_records() {
    each("quick_format_same_geometry", |built| {
        let m = &built.manifest;
        let (old, new) = (m.instance("old"), m.instance("new"));
        assert_eq!(
            (old.role, old.expected_kind, old.generation),
            (InstanceRole::Lost, Some(InstanceKind::Main), 1)
        );
        assert_eq!(
            (new.role, new.expected_kind, new.generation),
            (InstanceRole::Current, None, 0)
        );
        assert_eq!(
            old.geometry.mft_extents[0].offset, new.geometry.mft_extents[0].offset,
            "the new MFT lies on the old one"
        );
        let survivors = slots(&intact_records(m, "old"));
        assert_eq!(survivors.first(), Some(&256));
        assert!(survivors.len() > 300);
        // every old entry from record 256 on is intact; nothing below is
        for e in m.entries_of("old") {
            assert_eq!(
                e.truth.record_state == RecordState::Intact,
                e.record >= 256,
                "{} (record {})",
                e.path,
                e.record
            );
            if e.record >= 256 && e.kind == EntryKind::File && !e.deleted {
                assert!(e.must_recover_exactly(), "{}", e.path);
                assert_reads_back(built, e);
            }
        }
        // the first media files: record gone, content contiguous and untouched
        let early: Vec<&Entry> = m
            .entries_of("old")
            .filter(|e| e.name().starts_with("first_"))
            .collect();
        assert_eq!(early.len(), 24);
        for e in &early {
            assert_eq!(e.expect.presence, Presence::Gone);
            assert_eq!(e.extents.len(), 1, "{} is contiguous", e.path);
            assert_reads_back(built, e);
        }
        // the root's first index block was replaced, later ones survive
        let root_blocks: Vec<_> = m
            .index_blocks
            .iter()
            .filter(|b| b.owner.number == 5)
            .collect();
        let new_root = root_blocks
            .iter()
            .find(|b| b.instance == new.id)
            .expect("new root block");
        assert_eq!(new_root.vcn, 0);
        let old_root: Vec<u64> = root_blocks
            .iter()
            .filter(|b| b.instance == old.id)
            .map(|b| b.vcn)
            .collect();
        assert!(
            !old_root.contains(&0) && !old_root.is_empty(),
            "old root blocks left: {old_root:?}"
        );
        // so some lost top-level folders have a name and some do not
        let children: Vec<&Entry> = m
            .entries_of("old")
            .filter(|e| shown(e) && e.components.len() == 2)
            .collect();
        let named = children
            .iter()
            .filter(|e| expected_path(e) == e.path)
            .count();
        let nameless: Vec<&&Entry> = children
            .iter()
            .filter(|e| expected_path(e).starts_with("[Lost folders]/"))
            .collect();
        assert!(
            named >= 20 && nameless.len() >= 20,
            "named {named}, nameless {}",
            nameless.len()
        );
        assert_eq!(named + nameless.len(), children.len());
        for e in nameless {
            let parent = e.names[0].parent;
            assert_eq!(
                expected_path(e),
                format!("[Lost folders]/Folder #{}/{}", parent.number, e.name())
            );
            assert!(parent.number < 256);
        }
        // nothing old is flagged overwritten: same layout, no new files
        for e in m
            .entries_of("old")
            .filter(|e| shown(e) && e.kind == EntryKind::File)
        {
            must_not(e, Flag::Overwritten);
            must_not(e, Flag::OlderGeneration);
        }
    });
}

#[test]
fn blank_records_and_untouched_slots() {
    each("quick_format_blank_records", |built| {
        let m = &built.manifest;
        let new = m.instance("new").id;
        let blanks: Vec<&RecordFact> = m
            .records
            .iter()
            .filter(|r| r.instance == new && r.role == RecordRole::Blank)
            .collect();
        assert_eq!(
            blanks.len(),
            256 - 35 + 8,
            "slots 16-23 and 35-255 of a fresh Windows 7 MFT"
        );
        for r in &blanks {
            assert_eq!((r.flags, r.sequence, r.number), (0, 1, Some(r.slot)));
            assert!(r.attr_types.is_empty() && r.lsn != 0);
        }
        // 8.3 aliases come first in the record
        let aliased: Vec<&Entry> = m.entries_of("old").filter(|e| e.names.len() == 2).collect();
        assert!(aliased.len() > 300);
        assert!(aliased
            .iter()
            .all(|e| e.names[0].namespace == 2 && e.names[1].namespace == 1));
        // and the old volume had blank records in its unused slots, too
        let old_blanks = intact_records(m, "old")
            .iter()
            .filter(|r| r.role == RecordRole::Blank)
            .count();
        assert!(old_blanks > 100, "{old_blanks}");
    });

    each("quick_format_untouched_slots", |built| {
        let m = &built.manifest;
        let survivors = slots(&intact_records(m, "old"));
        assert_eq!(
            survivors
                .iter()
                .filter(|&&s| (38..256).contains(&s))
                .count(),
            218,
            "old records inside the live MFT"
        );
        assert!(!survivors.iter().any(|&s| s < 38));
        let new = m.instance("new");
        assert!(new.live_slots.iter().all(|&(s, n)| s + n <= 38));
        assert_eq!(
            new.geometry.mft_initialized_records, 256,
            "they are below the initialized size"
        );
        let inside_live: Vec<&Entry> = m
            .entries_of("old")
            .filter(|e| e.record < 256 && shown(e))
            .collect();
        assert_eq!(inside_live.len(), 218);
        // their MFT change times are before the format
        assert!(inside_live
            .iter()
            .all(|e| e.times.mft_changed < new.format_time));
        // with all folders alive, every path is the original one
        for e in m.entries_of("old").filter(|e| shown(e)) {
            assert_eq!(expected_path(e), e.path);
        }
    });
}

#[test]
fn cluster_change_shifts_the_new_mft() {
    each("quick_format_cluster_change", |built| {
        let m = &built.manifest;
        let (old, new) = (m.instance("old"), m.instance("new"));
        assert_eq!(
            (old.geometry.cluster_size, new.geometry.cluster_size),
            (4096, 65_536)
        );
        assert_eq!(
            old.geometry.mft_extents[0].offset - new.geometry.mft_extents[0].offset,
            20 * 1024
        );
        let survivors = slots(&intact_records(m, "old"));
        assert_eq!(
            survivors.first(),
            Some(&236),
            "new records 0-255 cover old records 0-235"
        );
        // the two new files lie on the two old ones
        let (live, dead) = (
            m.entry("new", "new-live.bin"),
            m.entry("new", "new-deleted.bin"),
        );
        let over = m.entry("old", "Backup/written-over.bin");
        let freed = m.entry("old", "Backup/written-over-then-freed.bin");
        let between = m.entry("old", "Backup/between.bin");
        assert!(!live.deleted && dead.deleted);
        let range = |e: &Entry| (e.extents[0].offset.unwrap(), e.extents[0].bytes);
        assert!(meets(&[range(live)], range(over).0, range(over).1));
        assert!(meets(&[range(dead)], range(freed).0, range(freed).1));
        assert!(!meets(&[range(live)], range(freed).0, range(freed).1));
        // the deleted one's runlist counts 64 KiB clusters and is in the overwritten set
        assert!(
            dead.extents[0].bytes.is_multiple_of(65_536)
                && dead.extents[0].lcn.unwrap() * 65_536 == range(dead).0
        );
        assert!(entry_meets(dead, &m.overwritten_set));
        assert!(
            !meets(&new.live_allocated, range(dead).0, range(dead).1),
            "its clusters are free in the new $Bitmap"
        );
        for victim in [over, freed] {
            assert!(shown(victim) && victim.truth.record_intact);
            assert_eq!(victim.truth.content_intact, Some(false));
            must(victim, Flag::Overwritten);
        }
        // every expected `overwritten` has a reason, and the untouched are expected clean
        let mut hit = 0;
        for e in m
            .entries_of("old")
            .filter(|e| shown(e) && e.kind == EntryKind::File)
        {
            if entry_meets(e, &m.overwritten_set) {
                must(e, Flag::Overwritten);
                hit += 1;
            } else {
                must_not(e, Flag::Overwritten);
                if !e.deleted {
                    assert!(e.must_recover_exactly(), "{}", e.path);
                }
            }
        }
        assert!(hit >= 2);
        assert!(shown(between));
    });
}

#[test]
fn repartitioned_disk_keeps_the_whole_old_mft() {
    each("repartitioned_other_offset", |built| {
        let m = &built.manifest;
        assert_eq!(m.scan_range, (MIB, 128 * MIB));
        assert_eq!(m.partitions.len(), 1);
        let old = m.instance("old");
        assert_eq!(old.geometry.volume_offset, 63 * 512);
        assert_eq!(old.expected_kind, Some(InstanceKind::Main));
        let records = intact_records(m, "old");
        assert!(
            records.iter().all(|r| r.offset % 1024 == 512),
            "sector 63: every record at an odd sector"
        );
        let mft = records
            .iter()
            .find(|r| r.slot == 0)
            .expect("old record 0 survives");
        assert_eq!(mft.names[0].name, "$MFT");
        assert!(mft.offset >= m.scan_range.0);
        // Windows XP's never-used form: sequence 0
        assert!(records
            .iter()
            .any(|r| r.role == RecordRole::Blank && r.sequence == 0));
        // boot sectors: primary before the range, backup inside it
        let boots: Vec<_> = m
            .boot_sectors
            .iter()
            .filter(|b| b.instance == old.id)
            .collect();
        assert_eq!(boots.len(), 2);
        assert!(boots[0].offset < MIB && !boots[0].backup);
        assert!(boots[1].backup && boots[1].offset > MIB && boots[1].hidden_sectors == 63);
        assert_eq!(
            boots[1].offset - boots[1].total_sectors * 512,
            63 * 512,
            "read as a backup it gives the volume start"
        );
        // old files that begin before 1 MiB
        let out: Vec<&Entry> = m
            .entries_of("old")
            .filter(|e| shown(e) && e.expect.flags_set.contains(&Flag::OutOfBounds))
            .collect();
        assert!(!out.is_empty());
        for e in &out {
            assert!(
                e.extents.iter().any(|x| x.offset.is_some_and(|o| o < MIB))
                    && !e.truth.data_in_range,
                "{}",
                e.path
            );
        }
        // system files of the old volume: there, and hidden
        assert_eq!(
            m.entry("old", "$Extend/$Quota").expect.presence,
            Presence::Hidden
        );
        assert_eq!(m.entry("old", "$MFT").expect.presence, Presence::Hidden);
        assert_eq!(
            m.entry("old", "System Volume Information").expect.presence,
            Presence::Shown
        );
        // DOS + Win32 pairs
        assert!(m
            .entries_of("old")
            .any(|e| e.names.len() == 2 && e.names[0].namespace == 2));
        // every folder survived: all paths are the original ones
        for e in m.entries_of("old").filter(|e| shown(e)) {
            assert_eq!(expected_path(e), e.path);
        }
    });
}

#[test]
fn post_format_activity_leaves_the_evidence_the_rules_need() {
    each("post_format_activity", |built| {
        let m = &built.manifest;
        let new = m.instance("new");
        let live = m.entry("new", "overwriter-live.bin");
        let dead = m.entry("new", "overwriter-deleted.bin");
        assert!(!live.deleted && dead.deleted);
        let same_clusters = |a: &Entry, b: &Entry| {
            a.extents
                .iter()
                .map(|x| (x.offset, x.bytes))
                .eq(b.extents.iter().map(|x| (x.offset, x.bytes)))
        };
        let over = m.entry("old", "Invoices/written-over.bin");
        let freed = m.entry("old", "Invoices/written-over-then-freed.bin");
        assert!(same_clusters(over, live) && same_clusters(freed, dead));
        for victim in [over, freed] {
            assert!(shown(victim) && victim.truth.record_intact);
            must(victim, Flag::Overwritten);
            assert_eq!(victim.truth.content_intact, Some(false));
        }
        // the deleted writer is free in the live bitmaps, the other is not
        assert!(!new
            .live_slots
            .iter()
            .any(|&(s, n)| (s..s + n).contains(&dead.record)));
        assert!(!entry_meets(dead, &new.live_allocated) && entry_meets(live, &new.live_allocated));
        assert!(entry_meets(dead, &m.overwritten_set));
        assert!(dead.times.mft_changed >= new.format_time);
        // a new folder took record 41 with sequence 1 again
        let folder = m.entry("new", "New folder");
        assert_eq!((folder.record, folder.sequence), (41, 1));
        assert_eq!(m.entry("old", "Videos").record, 41);
        let old_children: Vec<&Entry> = m
            .entries_of("old")
            .filter(|e| shown(e) && e.names[0].parent.number == 41 && e.names[0].parent.seq == 1)
            .collect();
        assert!(old_children.len() >= 3);
        for name in ["kept-0.dat", "kept-1.dat", "kept-2.dat"] {
            let kept = old_children
                .iter()
                .find(|e| e.name() == name)
                .expect("a kept file");
            assert!(kept.must_recover_exactly());
            assert_reads_back(built, kept);
        }
        for c in &old_children {
            let path = c.expect.path.as_ref().expect("a path is expected");
            assert_ne!(
                path[0], "New folder",
                "{} must not move into the new folder",
                c.path
            );
        }
        // the deleted multi-record file: free extension records without $STANDARD_INFORMATION
        let multi = m.entry("new", "fragmented-then-deleted.bin");
        assert_eq!(multi.extension_records.len(), 2);
        for (number, offset) in &multi.extension_records {
            let r = m
                .records
                .iter()
                .find(|r| r.offset == *offset)
                .expect("extension record");
            assert_eq!(
                (r.slot, r.flags, r.standard_information),
                (*number, 0, None)
            );
            assert!(r.data.len() == 1 && r.data[0].non_resident.is_some());
            assert!(*number < 256, "inside the live MFT");
        }
        assert!(entry_meets(multi, &m.overwritten_set));
        // the ordinary new files landed on the first old media files
        let ruined = m
            .entries_of("old")
            .filter(|e| e.name().starts_with("first_") && e.truth.content_intact == Some(false))
            .count();
        assert!(ruined >= 5, "{ruined}");
        // the truth about content never contradicts the expectation of a clean file
        for e in m
            .entries_of("old")
            .filter(|e| shown(e) && e.kind == EntryKind::File && !e.deleted)
        {
            if e.expect.flags_clear.contains(&Flag::Overwritten) {
                assert!(
                    e.must_recover_exactly(),
                    "{} is expected clean but is not intact",
                    e.path
                );
            }
        }
    });
}

#[test]
fn growing_and_unusual_live_mfts() {
    each("new_mft_grew", |built| {
        let m = &built.manifest;
        assert_eq!(m.instance("new").geometry.mft_records, 512);
        assert_eq!(slots(&intact_records(m, "old")).first(), Some(&512));
        assert_eq!(
            m.instance("new").live_slots.last(),
            Some(&(24, 344)),
            "slots 24-367 are in use"
        );
    });

    each("live_mft_with_attribute_list", |built| {
        let m = &built.manifest;
        let new = m.instance("new");
        assert_eq!(new.geometry.mft_extents.len(), 2);
        let record0 = m
            .records
            .iter()
            .find(|r| r.instance == new.id && r.slot == 0 && r.role == RecordRole::Base)
            .unwrap();
        let list = record0
            .attribute_list
            .as_ref()
            .expect("$MFT has an attribute list");
        assert!(list.resident);
        assert!(list
            .entries
            .iter()
            .any(|e| e.type_code == 0x80 && e.holder.number == 15 && e.lowest_vcn > 0));
        assert_eq!(
            record0.data[0].non_resident.as_ref().unwrap().runs.len(),
            1,
            "record 0 holds the first extent only"
        );
        let record15 = m
            .records
            .iter()
            .find(|r| r.instance == new.id && r.slot == 15)
            .unwrap();
        assert_eq!(
            (record15.role, record15.base.number, record15.base.seq),
            (RecordRole::Extension, 0, 1)
        );
        assert_eq!(
            record15.data[0].non_resident.as_ref().unwrap().lowest_vcn,
            64
        );
        let second = new.geometry.mft_extents[1].offset;
        let live_in_second = m
            .records
            .iter()
            .filter(|r| r.instance == new.id && r.offset >= second && r.flags & 1 == 1)
            .count();
        assert_eq!(live_in_second, 82);
    });

    each("live_mft_partly_initialized", |built| {
        let m = &built.manifest;
        let new = m.instance("new");
        assert_eq!(
            (
                new.geometry.mft_records,
                new.geometry.mft_initialized_records
            ),
            (512, 256)
        );
        assert!(new.live_slots.contains(&(300, 1)) && new.live_slots.contains(&(310, 1)));
        for slot in [300u64, 310] {
            let e = m.entries_of("old").find(|e| e.record == slot).unwrap();
            assert!(
                shown(e) && e.truth.record_intact && !e.deleted,
                "old record {slot} must be shown"
            );
        }
        assert_eq!(slots(&intact_records(m, "old")).first(), Some(&256));
    });
}

#[test]
fn generations_are_separable_exactly_when_the_scenario_says_so() {
    each("two_generations", |built| {
        let m = &built.manifest;
        let roles: Vec<_> = m
            .instances
            .iter()
            .map(|i| (i.label.as_str(), i.role, i.expected_kind, i.generation))
            .collect();
        assert_eq!(
            roles,
            [
                ("gen1", InstanceRole::Lost, Some(InstanceKind::Older), 2),
                ("gen2", InstanceRole::Lost, Some(InstanceKind::Main), 1),
                ("current", InstanceRole::Current, None, 0),
            ]
        );
        let (main, older) = (catalog(m, "gen2"), catalog(m, "gen1"));
        assert_eq!(
            (main.kind, main.anchor, older.kind, older.anchor),
            (InstanceKind::Main, true, InstanceKind::Older, false)
        );
        let (gen1, gen2) = (intact_records(m, "gen1"), intact_records(m, "gen2"));
        let (s1, s2) = (slots(&gen1), slots(&gen2));
        assert!(
            *s2.first().unwrap() == 256 && *s2.last().unwrap() < 512 && *s1.first().unwrap() == 512
        );
        // D2: one kept cut at 512, every contradiction explained by it
        let f = anchor_fragment(m);
        check_d2(m, f);
        assert_eq!(f.cuts.iter().map(|c| c.position).collect::<Vec<_>>(), [512]);
        assert!(
            f.contradictions.len() >= 10
                && f.contradictions.iter().all(|c| c.explained_by == Some(512))
        );
        let records = fragment_records(m, f);
        let below = f.contradictions.iter().map(|c| c.lo).max().unwrap();
        assert_eq!(
            changepoint_between(&records, below, 512),
            Some(512),
            "512 is a changepoint"
        );
        assert_eq!(f.segment_instances, [Some(main.id), Some(older.id)]);
        assert_eq!(warning(m, "mixed_generations"), Some(false));
        for e in m.entries_of("gen1").filter(|e| shown(e)) {
            assert_eq!(instance_of(m, e).id, older.id);
            must(e, Flag::OlderGeneration);
        }
        for e in m.entries_of("gen2").filter(|e| shown(e)) {
            assert_eq!(instance_of(m, e).id, main.id);
            must_not(e, Flag::OlderGeneration);
        }
        // the 2020 files were written over the first clusters of the 2017 files
        let reused = m
            .entries_of("gen1")
            .filter(|e| {
                shown(e)
                    && e.truth.content_intact == Some(false)
                    && e.expect.flags_set.contains(&Flag::Overwritten)
            })
            .count();
        assert!(reused >= 10, "{reused}");
        // The trap: both generations gave records 38-97 to their top-level
        // folders, under different names, and index blocks of the 2017 file
        // system still name its folders. They must not name the 2020 ones.
        let gen1_id = m.instance("gen1").id;
        let stale_names: Vec<(u64, &str)> = m
            .index_blocks
            .iter()
            .filter(|b| {
                b.instance == gen1_id && b.owner.number == 5 && b.state == ArtifactState::Intact
            })
            .flat_map(|b| b.entries.iter())
            .filter(|e| (38..98).contains(&e.child.number) && e.child.seq == 1)
            .map(|e| (e.child.number, e.key.name.as_str()))
            .collect();
        assert!(
            stale_names.len() >= 10,
            "surviving 2017 root entries: {}",
            stale_names.len()
        );
        for (number, name) in &stale_names {
            assert_eq!(*name, folder_name(*number as usize - 38));
            let in_2020 = m
                .entries_of("gen2")
                .find(|e| e.record == *number)
                .expect("a 2020 folder with that number");
            assert_eq!(in_2020.name(), folder_name(*number as usize - 38 + 7));
        }
        for e in m
            .entries_of("gen2")
            .filter(|e| shown(e) && e.components.len() == 2)
        {
            let path = e.expect.path.as_ref().expect("a path");
            assert!(
                path[0] == e.components[0] || path[0] == "[Lost folders]",
                "{}: {path:?}",
                e.path
            );
        }
        let misnamed_if_mixed_up = m
            .entries_of("gen2")
            .filter(|e| {
                shown(e)
                    && e.components.len() == 2
                    && expected_path(e).starts_with("[Lost folders]/")
            })
            .filter(|e| {
                stale_names
                    .iter()
                    .any(|(n, _)| *n == e.names[0].parent.number)
            })
            .count();
        assert!(
            misnamed_if_mixed_up > 0,
            "no 2020 folder is nameless while a 2017 entry offers a name for its number"
        );
    });

    each("three_generations", |built| {
        let m = &built.manifest;
        let kinds: Vec<_> = m.instances.iter().map(|i| i.expected_kind).collect();
        assert_eq!(
            kinds,
            [
                Some(InstanceKind::Older),
                Some(InstanceKind::Older),
                Some(InstanceKind::Main),
                None
            ]
        );
        let f = anchor_fragment(m);
        check_d2(m, f);
        assert_eq!(
            f.cuts.iter().map(|c| c.position).collect::<Vec<_>>(),
            [512, 1024]
        );
        assert!(f.contradictions.iter().all(|c| c.explained_by.is_some()));
        let ids: Vec<u32> = ["gen3", "gen2", "gen1"]
            .iter()
            .map(|l| catalog(m, l).id)
            .collect();
        assert_eq!(
            f.segment_instances,
            ids.iter().map(|&i| Some(i)).collect::<Vec<_>>()
        );
        for label in ["gen1", "gen2"] {
            assert_eq!(catalog(m, label).kind, InstanceKind::Older);
            assert!(intact_records(m, label).len() >= 64, "{label}");
        }
    });

    each("restored_from_backup", |built| {
        let m = &built.manifest;
        let kinds: Vec<_> = m
            .instances
            .iter()
            .map(|i| (i.label.as_str(), i.expected_kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("gen1", Some(InstanceKind::Older)),
                ("gen2", Some(InstanceKind::Main)),
                ("current", None)
            ]
        );
        let (main, older) = (catalog(m, "gen2"), catalog(m, "gen1"));
        assert_eq!(
            (main.kind, older.kind),
            (InstanceKind::Main, InstanceKind::Older)
        );
        // The trap: every intact in-use 2020 user record has a
        // $STANDARD_INFORMATION MFT-change time older than every 2017 one,
        // and $FILE_NAME times newer than all of them.
        let si = |r: &RecordFact| r.standard_information.map(|s| s.0[2]);
        let user = |r: &RecordFact| r.slot >= 38 && r.flags & 1 == 1;
        let gen1 = intact_records(m, "gen1");
        let gen2 = intact_records(m, "gen2");
        let newest_2017 = gen1
            .iter()
            .filter(|r| user(r))
            .filter_map(|r| si(r))
            .max()
            .unwrap();
        let restored: Vec<&&RecordFact> = gen2
            .iter()
            .filter(|r| user(r))
            .filter(|r| r.standard_information.is_some() && !r.names.is_empty())
            .collect();
        assert!(restored.len() >= 64, "{}", restored.len());
        for r in &restored {
            assert!(si(r).unwrap() < newest_2017, "slot {}", r.slot);
            let fn_newest = r.names.iter().flat_map(|n| n.times).max().unwrap();
            assert!(fn_newest > newest_2017, "slot {}", r.slot);
        }
        // D2 by record times: the cut at 512, as in two_generations.
        let f = anchor_fragment(m);
        check_d2(m, f);
        assert_eq!(f.cuts.iter().map(|c| c.position).collect::<Vec<_>>(), [512]);
        assert!(
            f.contradictions.len() >= 10
                && f.contradictions.iter().all(|c| c.explained_by == Some(512))
        );
        assert_eq!(f.segment_instances, [Some(main.id), Some(older.id)]);
        assert_eq!(warning(m, "mixed_generations"), Some(false));
        for e in m.entries_of("gen2").filter(|e| shown(e)) {
            assert_eq!(instance_of(m, e).id, main.id);
            must_not(e, Flag::OlderGeneration);
        }
        // C3: the files copied onto the new volume and deleted again are
        // post_format, although their MFT-change times are from 2015.
        let current = m.instance("current").id;
        let copies: Vec<&RecordFact> = m
            .records
            .iter()
            .filter(|r| {
                r.instance == current && r.names.iter().any(|n| n.name.starts_with("restored-"))
            })
            .collect();
        assert_eq!(copies.len(), 3);
        for r in copies {
            assert!(si(r).unwrap() < hds_testkit::time::ymd(2016, 1, 1));
            assert_eq!(
                r.expect.class,
                Some(RecordClass::PostFormat),
                "slot {}",
                r.slot
            );
        }
    });

    each("generations_not_separable", |built| {
        let m = &built.manifest;
        let f = anchor_fragment(m);
        check_d2(m, f);
        assert!(f.cuts.is_empty(), "no cut may exist");
        assert!(f.contradictions.len() >= 10);
        let records = fragment_records(m, f);
        for c in &f.contradictions {
            assert_eq!(c.explained_by, None);
            assert_eq!(
                changepoint_between(&records, c.lo, c.hi),
                None,
                "({}, {}] holds no changepoint",
                c.lo,
                c.hi
            );
        }
        assert_eq!(warning(m, "mixed_generations"), Some(true));
        // one instance holds both file systems
        let main = catalog(m, "gen1+gen2");
        assert_eq!((main.kind, main.sources.len()), (InstanceKind::Main, 2));
        let stale: Vec<u64> = f.contradictions.iter().map(|c| c.hi).collect();
        for e in m.entries.iter().filter(|e| shown(e)) {
            assert_eq!(instance_of(m, e).id, main.id);
            assert!(e.expect.path.is_none());
            if stale.contains(&e.record) && m.instances[e.instance as usize].label == "gen1" {
                must(e, Flag::OlderGeneration);
            } else {
                must_not(e, Flag::OlderGeneration);
            }
        }
        // their record numbers are disjoint, which is what lets them share a segment
        let (s1, s2) = (
            slots(&intact_records(m, "gen1")),
            slots(&intact_records(m, "gen2")),
        );
        assert!(s2.last().unwrap() < s1.first().unwrap());
    });
}

#[test]
fn embedded_volumes_live_inside_one_file() {
    for (name, fragments) in [("embedded_volume", 1), ("embedded_volume_fragmented", 5)] {
        each(name, |built| {
            let m = &built.manifest;
            let inner = m
                .instances
                .iter()
                .find(|i| i.role == InstanceRole::Embedded)
                .expect("an embedded instance");
            assert_eq!(inner.expected_kind, Some(InstanceKind::Embedded));
            let host = &m.entries[inner.embedded_in.unwrap() as usize];
            assert_eq!(host.path, "Documents/old-laptop.img");
            assert!(host.record >= 256 && host.must_recover_exactly() && host.size == 24 * MIB);
            assert_eq!(host.extents.len(), fragments);
            assert_eq!(inner.offset_in_host, Some(0));
            let inside = |offset: u64| {
                host.extents
                    .iter()
                    .any(|x| x.offset.is_some_and(|o| (o..o + x.bytes).contains(&offset)))
            };
            let inner_records: Vec<&RecordFact> = m
                .records
                .iter()
                .filter(|r| r.instance == inner.id)
                .collect();
            let host_records = intact_records(m, "old");
            assert!(
                inner_records.len() > host_records.len(),
                "{} inner records, {} of the host",
                inner_records.len(),
                host_records.len()
            );
            assert!(
                inner_records.iter().all(|r| inside(r.offset)),
                "every inner record lies in the host file"
            );
            assert!(
                m.index_blocks
                    .iter()
                    .filter(|b| b.instance == inner.id)
                    .count()
                    > 10
            );
            // its files are browse-only
            let files: Vec<&Entry> = m
                .entries
                .iter()
                .filter(|e| e.instance == inner.id && shown(e))
                .collect();
            assert!(files.len() > 500);
            for e in &files {
                must(e, Flag::Embedded);
                assert!(
                    e.extents.iter().all(|x| x.offset.is_none_or(inside)),
                    "{}",
                    e.path
                );
            }
            // and they are really there: reading through the translated extents works
            for e in files
                .iter()
                .filter(|e| e.kind == EntryKind::File && !e.deleted)
                .take(60)
            {
                assert_eq!(e.truth.content_intact, Some(true), "{}", e.path);
                assert_reads_back(built, e);
            }
            // the host has what a validated geometry needs: three voting folders
            let voting = host_records
                .iter()
                .filter(|r| !r.index_allocation.is_empty())
                .count();
            assert!(voting >= 3, "{voting}");
            // C1: the inner boot sector is not at the range start or a
            // partition start, so it defines no current volume, even where
            // an in-use record 0 lies exactly where it points
            let boot = m
                .boot_sectors
                .iter()
                .find(|b| b.instance == inner.id && !b.backup)
                .expect("inner boot sector");
            let record0_at = boot.offset + boot.mft_lcn * boot.cluster_size as u64;
            let found = m
                .records
                .iter()
                .any(|r| r.offset == record0_at && r.slot == 0 && r.instance == inner.id);
            assert_eq!(found, fragments == 1, "{name}");
            assert!(
                boot.offset != m.scan_range.0
                    && m.partitions.iter().all(|p| p.offset != boot.offset)
            );
            assert!(m
                .records
                .iter()
                .filter(|r| r.instance == inner.id)
                .all(|r| r.expect.class != Some(RecordClass::Live)));
            // D7: an embedded catalog instance whose container is the image
            // file, newer than everything inside it
            let embedded: Vec<_> = m.catalog_of_kind(InstanceKind::Embedded).collect();
            assert_eq!(embedded.len(), 1, "{name}");
            let c = embedded[0];
            assert_eq!(
                (c.container, c.sources.as_slice()),
                (Some(host.id), &[inner.id][..])
            );
            let newest = inner_records
                .iter()
                .filter_map(|r| r.standard_information.map(|s| s.0[2]))
                .max()
                .unwrap();
            assert!(
                host.times.mft_changed + hds_testkit::time::DAY >= newest,
                "the container is not stale"
            );
            let main = m.main_instance().expect("a main instance");
            assert_eq!(
                (
                    main.label.as_str(),
                    main.geometry.as_ref().unwrap().validated
                ),
                ("old", Some(true))
            );
            assert!(files.iter().all(|e| instance_of(m, e).id == c.id));
            // the host file is not overwritten: it is the container, not a stale file
            must_not(host, Flag::Overwritten);
            // an embedded volume has one volume offset only in one piece
            assert_eq!(c.geometry.is_some(), fragments == 1);
        });
    }
}

#[test]
fn volumes_larger_than_the_new_partition() {
    each("old_volume_larger_than_range", |built| {
        let m = &built.manifest;
        assert_eq!(m.scan_range, (MIB, 97 * MIB));
        assert_eq!(
            m.partitions
                .iter()
                .map(|p| (p.offset, p.size))
                .collect::<Vec<_>>(),
            [(MIB, 96 * MIB), (97 * MIB, 64 * MIB)]
        );
        assert_eq!(
            m.instance("data").role,
            InstanceRole::Unscanned,
            "the neighbour partition is never seen"
        );
        assert!(m
            .entries_of("data")
            .all(|e| e.expect.presence == Presence::Gone));
        let outside: Vec<&Entry> = [
            "entirely-outside.bin",
            "also-outside-1.bin",
            "also-outside-2.bin",
            "also-outside-3.bin",
        ]
        .iter()
        .map(|n| m.entry("old", &format!("Documents/{n}")))
        .collect();
        let straddle = m.entry("old", "Documents/straddles-the-end.bin");
        let inside = m.entry("old", "Documents/entirely-inside.bin");
        for e in outside.iter().copied().chain([straddle]) {
            must(e, Flag::OutOfBounds);
            assert!(!e.truth.data_in_range && shown(e));
        }
        must_not(inside, Flag::OutOfBounds);
        assert!(inside.must_recover_exactly());
        assert_reads_back(built, inside);
        assert!(
            straddle.extents[0].offset.unwrap() < 97 * MIB
                && straddle.extents[1].offset.unwrap() > 97 * MIB
        );
        // the bytes behind the range exist in the image, inside partition 2
        for e in &outside {
            let at = e.extents[0].offset.unwrap();
            assert!(
                at >= 97 * MIB
                    && !built.image.is_zero(at, 4096)
                    && e.truth.content_intact == Some(true)
            );
        }
        // the old MFT is elsewhere and complete
        assert_eq!(slots(&intact_records(m, "old")).first(), Some(&0));
        // F5: four files end behind the range, so the new boot sectors (which
        // agree with the old geometry) do not bound the old volume; its
        // range reaches into partition 2 and is protected
        let main = catalog(m, "old");
        let g = main.geometry.as_ref().unwrap();
        assert_eq!(
            (main.kind, g.cluster_size, g.volume_offset, g.validated),
            (InstanceKind::Main, 4096, MIB, Some(true))
        );
        let mut ends: Vec<u64> = m
            .entries_of("old")
            .filter(|e| shown(e) && !e.deleted && e.kind == EntryKind::File)
            .filter_map(|e| {
                e.extents
                    .iter()
                    .filter_map(|x| x.lcn.map(|l| l + x.clusters))
                    .max()
            })
            .collect();
        ends.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(
            g.total_clusters,
            Some(ends[3]),
            "the 4th-largest end of the files"
        );
        let range = main.lost_volume_range.unwrap();
        assert_eq!(range, (MIB, MIB + ends[3] * 4096));
        assert!(range.1 > 97 * MIB + 16 * MIB, "deep into partition 2");
        assert_eq!(main.protected, Some(true));
    });

    each("old_volume_larger_whole_disk", |built| {
        let m = &built.manifest;
        assert_eq!(m.scan_range, (0, 128 * MIB));
        let old = m.instance("old");
        let backup = m
            .boot_sectors
            .iter()
            .find(|b| b.instance == old.id)
            .expect("the old backup boot sector survives");
        assert!(
            backup.backup && backup.offset == 128 * MIB - 512 && backup.total_sectors == 262_143
        );
        assert_eq!(backup.hidden_sectors, 0);
        for name in [
            "behind-the-new-partition.bin",
            "also-behind-1.bin",
            "also-behind-2.bin",
            "also-behind-3.bin",
        ] {
            let behind = m.entry("old", &format!("Documents/{name}"));
            assert!(behind.must_recover_exactly() && behind.extents[0].offset.unwrap() >= 96 * MIB);
            assert_reads_back(built, behind);
            must_not(behind, Flag::OutOfBounds);
        }
        assert!(m
            .entries_of("old")
            .filter(|e| shown(e))
            .all(|e| !e.expect.flags_set.contains(&Flag::OutOfBounds)));
        // the new volume's boot sectors agree with the old geometry, but the
        // files end behind them: the old backup decides the size (F5)
        let new = m.instance("new");
        assert!(m
            .boot_sectors
            .iter()
            .any(|b| b.instance == new.id && b.offset == 0));
        let main = catalog(m, "old");
        let clusters = 262_143 * 512 / 4096;
        assert_eq!(
            main.geometry.as_ref().unwrap().total_clusters,
            Some(clusters)
        );
        assert_eq!(main.lost_volume_range, Some((0, clusters * 4096)));
    });
}

#[test]
fn second_volume_on_the_disk_is_other() {
    each("whole_disk_two_volumes", |built| {
        let m = &built.manifest;
        let other = m.instance("other");
        assert_eq!(
            (other.role, other.expected_kind),
            (InstanceRole::Lost, Some(InstanceKind::Other))
        );
        assert_eq!(m.instance("old").expected_kind, Some(InstanceKind::Main));
        assert!(
            m.boot_sectors.iter().all(|b| b.instance != other.id),
            "its boot sectors are gone"
        );
        let records = intact_records(m, "other");
        assert!(
            slots(&records).iter().all(|&s| s >= 16),
            "records 0-15 were wiped"
        );
        let voting = records
            .iter()
            .filter(|r| !r.index_allocation.is_empty())
            .count();
        assert!(voting >= 3);
        assert_eq!(other.geometry.volume_offset, 65 * MIB);
        // the two lost volumes lived in the same years
        let (a, b) = (
            m.instance("old").time_range.unwrap(),
            other.time_range.unwrap(),
        );
        assert!(a.0 < b.1 && b.0 < a.1);
        assert_eq!(
            m.partitions.len(),
            1,
            "the second partition is no longer in the table"
        );
        // nothing of it was overwritten: every file is recoverable
        for e in m
            .entries_of("other")
            .filter(|e| shown(e) && e.kind == EntryKind::File && !e.deleted)
        {
            assert!(e.must_recover_exactly(), "{}", e.path);
        }
    });
}

/// The class of the record a manifest entry was last written to.
fn class_of(m: &Manifest, e: &Entry) -> Option<RecordClass> {
    let at = e.record_offset?;
    m.records
        .iter()
        .find(|r| r.offset == at && r.entry == Some(e.id))
        .and_then(|r| r.expect.class)
}

#[test]
fn records_are_classified_as_c3_says() {
    each("current_volume", |built| {
        let m = &built.manifest;
        assert!(m.catalog.is_empty() && m.fragments.is_empty());
        assert_eq!(warning(m, "mixed_generations"), Some(false));
        for e in &m.entries {
            let expected = if e.deleted {
                RecordClass::PostFormat
            } else {
                RecordClass::Live
            };
            assert_eq!(class_of(m, e), Some(expected), "{}", e.path);
        }
        // the $MFTMirr copies are live too
        assert!(m
            .records
            .iter()
            .filter(|r| r.role == RecordRole::Mirror)
            .all(|r| r.expect.class == Some(RecordClass::Live)));
    });

    each("quick_format_blank_records", |built| {
        let m = &built.manifest;
        // never-used records of both volumes are dropped (A3)
        let blanks: Vec<&RecordFact> = m
            .records
            .iter()
            .filter(|r| r.role == RecordRole::Blank)
            .collect();
        assert!(blanks.len() > 300);
        assert!(blanks
            .iter()
            .all(|r| r.expect.class == Some(RecordClass::Dropped)));
    });

    each("quick_format_untouched_slots", |built| {
        let m = &built.manifest;
        let new = m.instance("new");
        let mft = new.geometry.mft_extents[0].offset;
        let inside: Vec<&RecordFact> = intact_records(m, "old")
            .into_iter()
            .filter(|r| r.offset >= mft && r.offset < mft + 256 * 1024)
            .collect();
        assert_eq!(inside.len(), 218);
        assert!(
            inside
                .iter()
                .all(|r| r.expect.class == Some(RecordClass::Old)),
            "old records in free slots are old"
        );
    });

    each("post_format_activity", |built| {
        let m = &built.manifest;
        for name in ["overwriter-deleted.bin", "fragmented-then-deleted.bin"] {
            let e = m.entry("new", name);
            assert_eq!(class_of(m, e), Some(RecordClass::PostFormat), "{name}");
            assert_eq!(e.expect.presence, Presence::Hidden);
        }
        // free extension records without $STANDARD_INFORMATION whose base
        // is a post_format record are post_format themselves
        let multi = m.entry("new", "fragmented-then-deleted.bin");
        for (_, offset) in &multi.extension_records {
            let r = m.records.iter().find(|r| r.offset == *offset).unwrap();
            assert_eq!(r.expect.class, Some(RecordClass::PostFormat));
        }
        assert_eq!(
            class_of(m, m.entry("new", "New folder")),
            Some(RecordClass::Live)
        );
        // C5: the overwritten set is the live bitmap plus post_format extents
        let new = m.instance("new");
        for &(start, len) in &new.live_allocated {
            assert!(meets(&m.overwritten_set, start, len));
        }
        assert!(entry_meets(
            m.entry("new", "overwriter-deleted.bin"),
            &m.overwritten_set
        ));
    });

    each("quick_format_cluster_change", |built| {
        let m = &built.manifest;
        let dead = m.entry("new", "new-deleted.bin");
        assert_eq!(class_of(m, dead), Some(RecordClass::PostFormat));
        // its extents are mapped with the current volume's 64 KiB clusters
        let x = &dead.extents[0];
        assert!(m
            .overwritten_set
            .iter()
            .any(|&(s, l)| s <= x.offset.unwrap() && x.offset.unwrap() + x.bytes <= s + l));
        // no old record lies at the new MFT's origin: main by volume offset
        let main = catalog(m, "old");
        assert!(!main.anchor && main.kind == InstanceKind::Main);
        assert_eq!(
            main.geometry
                .as_ref()
                .map(|g| (g.cluster_size, g.volume_offset)),
            Some((4096, 0))
        );
    });

    each("live_mft_partly_initialized", |built| {
        let m = &built.manifest;
        for slot in [300u64, 310] {
            let e = m.entries_of("old").find(|e| e.record == slot).unwrap();
            assert_eq!(
                class_of(m, e),
                Some(RecordClass::Old),
                "beyond the initialized size: old"
            );
        }
    });

    each("live_mft_with_attribute_list", |built| {
        let m = &built.manifest;
        let new = m.instance("new").id;
        let record15 = m
            .records
            .iter()
            .find(|r| r.instance == new && r.slot == 15 && r.role == RecordRole::Extension)
            .unwrap();
        assert_eq!(record15.expect.class, Some(RecordClass::Live));
        let second = m.instance("new").geometry.mft_extents[1].offset;
        let live = m
            .records
            .iter()
            .filter(|r| r.instance == new && r.offset >= second && r.flags & 1 == 1);
        assert!(live
            .into_iter()
            .all(|r| r.expect.class == Some(RecordClass::Live)));
    });
}

#[test]
fn catalogs_have_one_main_and_protect_validated_volumes() {
    for scenario in [
        "quick_format_same_geometry",
        "post_format_activity",
        "whole_disk_two_volumes",
        "repartitioned_other_offset",
    ] {
        each(scenario, |built| {
            let m = &built.manifest;
            let mains: Vec<_> = m.catalog_of_kind(InstanceKind::Main).collect();
            assert_eq!(mains.len(), 1, "{scenario}");
            let main = mains[0];
            let old = m.instance("old");
            let g = main.geometry.as_ref().unwrap();
            assert_eq!(
                (g.cluster_size, g.volume_offset),
                (old.geometry.cluster_size, old.geometry.volume_offset)
            );
            assert_eq!(
                (g.validated, main.protected),
                (Some(true), Some(true)),
                "{scenario}"
            );
            let (start, end) = main.lost_volume_range.unwrap();
            assert_eq!(start, old.geometry.volume_offset);
            assert!(
                end >= start + old.geometry.total_clusters * 4096 - 4096 && end <= m.image_size
            );
        });
    }
    each("quick_format_same_geometry", |built| {
        let m = &built.manifest;
        let main = m.main_instance().unwrap();
        assert!(main.anchor, "old records at the new MFT's origin");
        // the anchor holds segment 0 of that fragment
        assert_eq!(anchor_fragment(m).segment_instances, [Some(main.id)]);
    });
    each("whole_disk_two_volumes", |built| {
        let m = &built.manifest;
        let other = catalog(m, "other");
        let g = other.geometry.as_ref().unwrap();
        assert_eq!(
            (other.kind, g.volume_offset, g.validated, other.protected),
            (InstanceKind::Other, 65 * MIB, Some(true), Some(true))
        );
        assert!(g.voting_directories >= 3);
        let (start, end) = other.lost_volume_range.unwrap();
        assert_eq!(start, 65 * MIB);
        assert!(end > 120 * MIB && end <= m.image_size);
    });
    each("repartitioned_other_offset", |built| {
        let m = &built.manifest;
        let main = m.main_instance().unwrap();
        // the old MFT survives completely: its record 0 maps the others
        assert!(main.geometry.as_ref().unwrap().mft_map_records >= 16);
        assert!(!main.anchor);
    });
}
