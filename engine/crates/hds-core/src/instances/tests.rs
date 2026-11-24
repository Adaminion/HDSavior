//! One constructed case per rule of ALGORITHMS D (and the S2 list), each
//! failing if the rule were dropped; hostile input; the scale test.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::single_range_in_vec_init,
    clippy::cast_possible_wrap,
    clippy::manual_is_multiple_of,
    clippy::cloned_ref_to_slice_refs
)]

use std::sync::atomic::AtomicBool;

use super::testlab::*;
use super::*;

const MIB: u64 = 1 << 20;

fn files(lab: &mut Lab, origin: u64, range: std::ops::Range<u32>, parent: u64, days: u64) {
    for n in range {
        lab.rec(
            origin,
            &file(
                n,
                1,
                parent,
                &format!("f{n}.txt"),
                t(days + u64::from(n)),
                None,
            ),
        );
    }
}

/// Two generations on one grid: newer records 40..69 (dir 40 with
/// sequence 2), older records 70..99 that name the old dir 40 (sequence
/// 1). `contra` older records name it.
fn generations(lab: &mut Lab, origin: u64, contra: u32, late_old: bool) {
    lab.rec(origin, &dir(40, 2, ROOT, "new", t(1000)));
    for n in 41..70 {
        lab.rec(
            origin,
            &file(
                n,
                1,
                fref(40, 2),
                &format!("n{n}.txt"),
                t(1000 + u64::from(n)),
                None,
            ),
        );
    }
    lab.rec(origin, &dir(75, 1, ROOT, "old", t(10)));
    for n in 70..100 {
        if n == 75 {
            continue;
        }
        let parent = if n < 70 + contra {
            fref(40, 1)
        } else {
            fref(75, 1)
        };
        let when = if late_old && n == 80 {
            t(1700)
        } else {
            t(u64::from(n))
        };
        lab.rec(
            origin,
            &file(n, 1, parent, &format!("o{n}.txt"), when, None),
        );
    }
}

#[test]
fn d1_fragments_and_unnumbered_records() {
    let mut lab = Lab::new(16);
    let (a, b) = (MIB, 8 * MIB);
    files(&mut lab, a, 30..61, ROOT, 0);
    files(&mut lab, b, 30..51, ROOT, 0);
    // On A's grid within 4096 records of its span: joins with number 70.
    lab.rec_at(
        a + 70 * 1024,
        &file(0, 1, ROOT, "u.txt", t(5), None).unnumbered(),
    );
    // On A's grid but 4100 records above its span: a stray.
    lab.rec_at(
        a + (60 + 4097) * 1024,
        &file(0, 1, ROOT, "far.txt", t(5), None).unnumbered(),
    );
    let run = lab.run(None);
    let l = &run.layout;
    assert_eq!(l.fragments().len(), 2);
    let near = run.id(a + 70 * 1024);
    assert_eq!(l.number(near), Some(70));
    assert_eq!(l.instance_of(near), l.instance_of(run.id(a + 30 * 1024)));
    let far = run.id(a + (60 + 4097) * 1024);
    assert_eq!(l.number(far), None);
    assert_eq!(run.kind(a + (60 + 4097) * 1024), Some(InstanceKind::Strays));
    // Equal numbers on two grids never join.
    assert_ne!(run.inst(a + 30 * 1024), run.inst(b + 30 * 1024));
}

#[test]
fn d2_a_cut_needs_a_time_boundary_and_two_contradictions() {
    let mut lab = Lab::new(4);
    generations(&mut lab, MIB, 2, false);
    let run = lab.run(None);
    let l = &run.layout;
    assert_eq!(l.cuts().len(), 1, "{:?}", l.cuts());
    let c = &l.cuts()[0];
    assert_eq!(c.at, 70);
    // D3: B = newest of the older side, A = oldest of the newer side.
    assert_eq!(c.older_newest, Some(t(99)));
    assert_eq!(c.newer_oldest, Some(t(1000)));
    let newer = run.inst(MIB + 50 * 1024).unwrap();
    let older = run.inst(MIB + 90 * 1024).unwrap();
    assert_ne!(newer, older);
    assert_eq!(l.main(), Some(newer));
    assert_eq!(run.kind(MIB + 90 * 1024), Some(InstanceKind::Older));
    assert!(!l.warnings().contains(&WARN_MIXED_GENERATIONS));
    assert!(l.older_generation_records().is_empty());
    assert_eq!(l.contradiction_counts(), (2, 0));
}

#[test]
fn d2_an_unexplained_contradiction_marks_its_record() {
    let mut lab = Lab::new(4);
    generations(&mut lab, MIB, 1, false);
    let run = lab.run(None);
    let l = &run.layout;
    assert!(l.cuts().is_empty());
    let hi = run.id(MIB + 70 * 1024);
    assert!(l.is_older_generation(hi));
    assert_eq!(l.older_generation_records(), &[hi]);
    assert!(l.warnings().contains(&WARN_MIXED_GENERATIONS));
    // One instance: nothing proves a second file system.
    assert_eq!(run.inst(MIB + 50 * 1024), run.inst(MIB + 90 * 1024));
}

#[test]
fn d2_times_alone_do_not_cut() {
    // "Delete the oldest folder, copy a new one": high numbers older,
    // but no contradiction.
    let mut lab = Lab::new(4);
    lab.rec(MIB, &dir(40, 1, ROOT, "a", t(1000)));
    for n in 41..70 {
        lab.rec(
            MIB,
            &file(n, 1, fref(40, 1), "x.txt", t(1000 + u64::from(n)), None),
        );
    }
    for n in 70..100 {
        lab.rec(
            MIB,
            &file(n, 1, fref(40, 1), "y.txt", t(u64::from(n)), None),
        );
    }
    let run = lab.run(None);
    assert!(run.layout.cuts().is_empty());
    assert_eq!(run.inst(MIB + 50 * 1024), run.inst(MIB + 90 * 1024));
    assert!(run.layout.older_generation_records().is_empty());
}

#[test]
fn d2_fallback_cut_when_no_changepoint() {
    // One older record carries a late time: no changepoint, but the cut at
    // 70 leaves one of 60 timed records out of order.
    let mut lab = Lab::new(4);
    generations(&mut lab, MIB, 2, true);
    let run = lab.run(None);
    let l = &run.layout;
    assert_eq!(l.cuts().len(), 1);
    let c = &l.cuts()[0];
    assert_eq!(c.at, 70);
    assert_eq!(c.older_newest, Some(t(1700)));
    // D3 as written: no record below is newer than B, so A is undefined.
    assert_eq!(c.newer_oldest, None);
    // The cut separates the two sides; without an anchor D8(3) then takes
    // the side with the newest time (the late record) as main.
    assert_ne!(run.inst(MIB + 50 * 1024), run.inst(MIB + 90 * 1024));
}

#[test]
fn d3_relation_to_a_cut() {
    let c = Cut {
        fragment: 0,
        at: 70,
        older_newest: Some(100),
        newer_oldest: Some(200),
        below: SegmentId(0),
        above: SegmentId(1),
    };
    assert_eq!(c.relation(Some((10, 100))), CutSide::Before);
    assert_eq!(c.relation(Some((101, 200))), CutSide::After);
    assert_eq!(c.relation(Some((101, 199))), CutSide::Undecided);
    assert_eq!(c.relation(Some((50, 300))), CutSide::Undecided);
    assert_eq!(c.relation(None), CutSide::Undecided);
    let open = Cut {
        newer_oldest: None,
        ..c.clone()
    };
    assert_eq!(open.relation(Some((150, 300))), CutSide::Undecided);
}

/// Directories 30..49 on one grid, files 300..339 on another naming them.
fn split_mft(lab: &mut Lab, file_seq_ref: u16) {
    for n in 30..50 {
        lab.rec(MIB, &dir(n, 1, ROOT, &format!("d{n}"), t(100)));
    }
    let o2 = 8 * MIB - 300 * 1024;
    for n in 300..340 {
        let parent = fref(30 + u64::from(n % 20), file_seq_ref);
        lab.rec(o2, &file(n, 1, parent, "f.txt", t(200), None));
    }
}

#[test]
fn d4a_resolving_references_join() {
    let mut lab = Lab::new(16);
    split_mft(&mut lab, 1);
    let run = lab.run(None);
    assert_eq!(run.inst(MIB + 30 * 1024), run.inst(8 * MIB));
    assert_eq!(run.layout.instances().len(), 2); // one instance + strays
}

#[test]
fn d4a_contradicting_references_forbid() {
    let mut lab = Lab::new(16);
    split_mft(&mut lab, 7);
    let run = lab.run(None);
    assert_ne!(run.inst(MIB + 30 * 1024), run.inst(8 * MIB));
}

/// A lost $MFT in two runs: record 0 at 1 MiB (LCN 256), VCN 64.. at LCN
/// 2048; files 30..49 near record 0, files 300..330 in the second run.
fn mft_map(lab: &mut Lab, init_records: u64) {
    let r0 = Rec::new(0)
        .si([t(1); 4], 6)
        .name(ROOT, "$MFT", 3, 6)
        .data_extent(
            0,
            127,
            (128 * 4096, init_records * 1024, init_records * 1024),
            &runlist(&[(Some(256), 64), (Some(2048), 64)]),
            0,
        );
    lab.rec(MIB, &r0);
    files(lab, MIB, 30..50, ROOT, 0);
    files(lab, 8 * MIB - 256 * 1024, 300..331, ROOT, 0);
}

#[test]
fn d4c_a_record_0_places_the_other_side() {
    let mut lab = Lab::new(16);
    mft_map(&mut lab, 512);
    let run = lab.run(None);
    assert_eq!(run.inst(MIB), run.inst(8 * MIB + 44 * 1024));
}

#[test]
fn d4c_needs_an_initialized_size_covering_the_other_side() {
    let mut lab = Lab::new(16);
    mft_map(&mut lab, 300);
    let run = lab.run(None);
    assert_ne!(run.inst(MIB), run.inst(8 * MIB + 44 * 1024));
}

#[test]
fn d4_intersecting_numbers_never_join() {
    let mut lab = Lab::new(16);
    for n in 30..50 {
        lab.rec(MIB, &dir(n, 1, ROOT, "d", t(100)));
    }
    // Files 45..69 name the directories (and resolve) but share 45..49.
    for n in 45..70 {
        lab.rec(
            8 * MIB - 45 * 1024,
            &file(n, 1, fref(30, 1), "f.txt", t(100), None),
        );
    }
    let run = lab.run(None);
    assert_ne!(run.inst(MIB + 30 * 1024), run.inst(8 * MIB));
}

/// A current volume at 0 ($MFT at 1 MiB, 64 slots) whose free slots 20..40
/// hold old records (dirs), and 16 files at another grid naming them,
/// numbered from `first`.
fn anchored(lab: &mut Lab, first: u32) {
    lab.current_volume(0, 4096, 256, 64, t(1500), &[], &[]);
    for n in 20..41 {
        lab.rec(MIB, &dir(n, 1, ROOT, "d", t(10)));
    }
    let o = 8 * MIB - u64::from(first) * 1024;
    for n in first..first + 16 {
        lab.rec(
            o,
            &file(n, 1, fref(20 + u64::from(n % 20), 1), "f.txt", t(20), None),
        );
    }
}

#[test]
fn d4_records_below_the_anchor_never_join_it() {
    let mut lab = Lab::new(16);
    anchored(&mut lab, 2);
    let run = lab.run(None);
    assert_ne!(run.inst(MIB + 20 * 1024), run.inst(8 * MIB));
    // The same references from numbers above the anchor's join it.
    let mut lab = Lab::new(16);
    anchored(&mut lab, 50);
    let run = lab.run(None);
    assert_eq!(run.inst(MIB + 20 * 1024), run.inst(8 * MIB));
}

#[test]
fn d6_small_instances_and_copy_sets_are_strays() {
    let mut lab = Lab::new(16);
    files(&mut lab, MIB, 30..70, ROOT, 0);
    // A copy of records 40..59 elsewhere (same number, sequence, creation).
    files(&mut lab, 8 * MIB - 40 * 1024, 40..60, ROOT, 0);
    // 10 records of their own.
    files(&mut lab, 12 * MIB - 200 * 1024, 200..210, ROOT, 0);
    let run = lab.run(None);
    let l = &run.layout;
    assert_eq!(run.kind(8 * MIB), Some(InstanceKind::Strays));
    assert_eq!(run.kind(12 * MIB), Some(InstanceKind::Strays));
    let main = run.inst(MIB + 30 * 1024).unwrap();
    assert_eq!(l.main(), Some(main));
    let g = l.stray_group(run.id(8 * MIB)).unwrap();
    assert!(g.copy_set);
    assert_eq!(g.copy_of, vec![main]);
    let small = l.stray_group(run.id(12 * MIB)).unwrap();
    assert!(!small.copy_set && small.copy_of.is_empty());
}

#[test]
fn d6_copies_need_the_creation_time() {
    // Same numbers and sequences, other creation times: not a copy.
    let mut lab = Lab::new(16);
    files(&mut lab, MIB, 30..70, ROOT, 0);
    files(&mut lab, 8 * MIB - 40 * 1024, 40..60, ROOT, 500);
    let run = lab.run(None);
    assert_ne!(run.kind(8 * MIB), Some(InstanceKind::Strays));
}

#[test]
fn d6_an_anchor_with_few_records_stays_and_is_main() {
    let mut lab = Lab::new(16);
    lab.current_volume(0, 4096, 256, 64, t(1500), &[], &[]);
    files(&mut lab, MIB, 20..29, ROOT, 10); // 9 old records at the anchor
    files(&mut lab, 8 * MIB, 100..200, ROOT, 0); // a larger, older file system
    let run = lab.run(None);
    let l = &run.layout;
    let anchor = run.inst(MIB + 20 * 1024).unwrap();
    assert_eq!(run.kind(MIB + 20 * 1024), Some(InstanceKind::Main));
    assert_eq!(l.anchors(), vec![anchor]);
    assert!(
        l.instance(anchor).unwrap().own_geometry.is_some(),
        "D5(3): the anchor gets a geometry"
    );
}

/// A host volume at 0 (records at 1 MiB): 25 PNG files (heads written) and
/// a container file 60 at LCN 2048..2560 (8..10 MiB) changed at
/// `container_time`; a guest of 20 records inside the container.
fn embedded(lab: &mut Lab, container_time: u64) {
    for i in 0..25u32 {
        let lcn = 1000 + u64::from(i);
        lab.rec(
            MIB,
            &file(
                30 + i,
                1,
                ROOT,
                &format!("p{i}.png"),
                t(300),
                Some((lcn, 1)),
            ),
        );
        lab.put(lcn * 4096, &png_head());
    }
    lab.rec(
        MIB,
        &file(60, 1, ROOT, "guest.img", container_time, Some((2048, 512))),
    );
    files(lab, 8 * MIB + 64 * 1024 - 30 * 1024, 30..50, ROOT, 400);
}

#[test]
fn d7_an_instance_inside_a_file_is_embedded() {
    let mut lab = Lab::new(16);
    embedded(&mut lab, t(500));
    let run = lab.run(None);
    let l = &run.layout;
    let host = run.inst(MIB + 30 * 1024).unwrap();
    assert_eq!(l.main(), Some(host));
    assert!(l.instance(host).unwrap().validated());
    let guest = l.instance(run.inst(8 * MIB + 64 * 1024).unwrap()).unwrap();
    assert_eq!(guest.kind, InstanceKind::Embedded);
    let c = guest.embedded_in.as_ref().unwrap();
    assert_eq!(c.instance, host);
    assert_eq!(c.record, run.id(MIB + 60 * 1024));
    assert_eq!(guest.label, "[Inside guest.img]");
    // instance_set "other" makes it a volume of its own.
    let mut o = Overrides::new();
    o.set_kind(guest.key, Some(KindChoice::Other));
    let l2 = run.reanalyze(&o);
    let g2 = l2.instance(guest.id).unwrap();
    assert_eq!(g2.kind, InstanceKind::Other);
    assert!(g2.manual && g2.embedded_in.is_none());
}

#[test]
fn d7_a_container_older_than_the_records_inside_is_stale() {
    let mut lab = Lab::new(16);
    embedded(&mut lab, t(100));
    let run = lab.run(None);
    let l = &run.layout;
    let guest = l.instance(run.inst(8 * MIB + 64 * 1024).unwrap()).unwrap();
    assert_ne!(guest.kind, InstanceKind::Embedded);
    let stale = guest.stale_container.as_ref().unwrap();
    assert_eq!(stale.record, run.id(MIB + 60 * 1024));
}

#[test]
fn d8_the_anchor_is_main_and_instance_set_overrules_it() {
    let mut lab = Lab::new(16);
    lab.current_volume(0, 4096, 256, 64, t(1500), &[], &[]);
    files(&mut lab, MIB, 20..40, ROOT, 10);
    files(&mut lab, 8 * MIB, 100..300, ROOT, 900); // larger and newer
    let run = lab.run(None);
    let l = &run.layout;
    let anchor = run.inst(MIB + 20 * 1024).unwrap();
    let big = run.inst(8 * MIB + 100 * 1024).unwrap();
    assert_eq!(l.main(), Some(anchor));
    assert_eq!(l.instance(big).unwrap().kind, InstanceKind::Other);
    // instance_set main on the other one; the previous main becomes other.
    let mut o = Overrides::new();
    o.set_kind(l.instance(big).unwrap().key, Some(KindChoice::Main));
    let l2 = run.reanalyze(&o);
    assert_eq!(l2.main(), Some(big));
    assert_eq!(l2.instance(anchor).unwrap().kind, InstanceKind::Other);
    assert!(l2.instance(big).unwrap().manual);
    // The same choice re-run on the stored layout alone (instance_set):
    let mut again = run.reanalyze(&Overrides::new());
    let mut heads = crate::geometry::Heads::new();
    again
        .apply(run.store(), &o, &mut heads, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(again.instances(), l2.instances());
    assert_eq!(again.protected(), l2.protected());
    // "auto" withdraws it.
    o.set_kind(l.instance(big).unwrap().key, None);
    assert_eq!(run.reanalyze(&o).main(), Some(anchor));
}

#[test]
fn d8_among_anchors_the_newest_format_wins() {
    // Two partitions, each a current volume with old records at its $MFT.
    let mut lab = Lab::new(40);
    lab.put(0, &mbr(&[(2048, 32768, 7), (36864, 32768, 7)]));
    let p1 = MIB;
    let p2 = 18 * MIB;
    lab.current_volume(p1, 4096, 256, 64, t(1500), &[], &[]);
    lab.current_volume(p2, 4096, 256, 64, t(1600), &[], &[]);
    files(&mut lab, p1 + MIB, 20..60, ROOT, 10); // more records, older format
    files(&mut lab, p2 + MIB, 20..40, ROOT, 10);
    let run = lab.run(None);
    let l = &run.layout;
    assert_eq!(l.live_volumes().len(), 2);
    assert_eq!(l.main(), run.inst(p2 + MIB + 20 * 1024));
    assert_eq!(l.anchors().len(), 2);
}

#[test]
fn d8_without_anchor_the_newest_wins_and_d9_older_by_time() {
    let mut lab = Lab::new(16);
    files(&mut lab, MIB, 30..60, ROOT, 1000);
    files(&mut lab, 8 * MIB, 30..60, ROOT, 0); // all older than the first's oldest
    let run = lab.run(None);
    let l = &run.layout;
    assert_eq!(l.main(), run.inst(MIB + 30 * 1024));
    assert_eq!(run.kind(8 * MIB + 30 * 1024), Some(InstanceKind::Older));
    let older = l.instance(run.inst(8 * MIB + 30 * 1024).unwrap()).unwrap();
    assert_eq!(older.label, "[Older file system]");
    // F4: an older instance without evidence of its own takes main's geometry.
    let g = older.geometry.as_ref().unwrap();
    assert_eq!(g.offset_source, crate::geometry::OffsetSource::Inherited);
    // Overlapping times make it "other".
    let mut lab = Lab::new(16);
    files(&mut lab, MIB, 30..60, ROOT, 1000);
    files(&mut lab, 8 * MIB, 30..60, ROOT, 1010);
    let run = lab.run(None);
    let k = run.kind(MIB + 30 * 1024);
    assert!(k == Some(InstanceKind::Other) || k == Some(InstanceKind::Main));
    assert_eq!(
        run.layout
            .instance(run.inst(8 * MIB + 30 * 1024).unwrap())
            .unwrap()
            .kind,
        InstanceKind::Main
    );
    let other = run
        .layout
        .instance(run.inst(MIB + 30 * 1024).unwrap())
        .unwrap();
    assert_eq!(other.kind, InstanceKind::Other);
    assert_eq!(other.label, "[File system 2]");
}

#[test]
fn s2_protected_ranges_session_first_then_validated_lost_volumes() {
    let mut lab = Lab::new(16);
    embedded(&mut lab, t(500));
    let run = lab.run(Some(0..12 * MIB));
    let p = run.layout.protected();
    assert_eq!(p[0], hds_io::Protected::session(0..12 * MIB));
    let main = run.layout.instance(run.layout.main().unwrap()).unwrap();
    let g = main.geometry.as_ref().unwrap();
    assert!(g.validated);
    assert_eq!(p[1], hds_io::Protected::lost_volume(g.lost_volume.clone()));
    // The host's files reach to cluster 2560 (10 MiB).
    assert!(g.lost_volume.start == 0 && g.lost_volume.end >= 10 * MIB);
    // The embedded instance is not protected.
    assert_eq!(p.len(), 2);
}

#[test]
fn analysis_from_stored_state_is_identical() {
    let mut lab = Lab::new(16);
    embedded(&mut lab, t(500));
    let run = lab.run(None);
    let again = run.reanalyze(&Overrides::new());
    assert_eq!(again.instances(), run.layout.instances());
    assert_eq!(again.main(), run.layout.main());
    assert_eq!(again.protected(), run.layout.protected());
    assert_eq!(again.cuts(), run.layout.cuts());
}

#[test]
fn hostile_cycles_huge_numbers_and_contradictory_boots() {
    let mut lab = Lab::new(16);
    // A parent cycle: 30 -> 31 -> 30, and a self-parent.
    lab.rec(MIB, &dir(30, 1, fref(31, 1), "a", t(1)));
    lab.rec(MIB, &dir(31, 1, fref(30, 1), "b", t(1)));
    lab.rec(MIB, &dir(32, 1, fref(32, 1), "c", t(1)));
    // Huge numbers, references and clusters.
    let big = u32::MAX - 3;
    lab.rec_at(
        2 * MIB,
        &file(
            big,
            0xFFFF,
            fref(u64::from(u32::MAX), 0xFFFF),
            "x",
            u64::MAX,
            Some((u64::MAX >> 9, 1 << 40)),
        ),
    );
    lab.rec_at(
        2 * MIB + 1024,
        &file(
            1,
            1,
            fref(0xFFFF_FFFF_FFFF, 3),
            "y",
            0,
            Some((1 << 50, 1 << 50)),
        ),
    );
    // Boot sectors that contradict each other.
    for i in 0..8u64 {
        lab.put(
            4 * MIB + i * 512,
            &boot_sector(
                512,
                1 << (i % 7),
                1 << (20 + i),
                i * 1000,
                7,
                -10,
                1,
                (i * 99) as u32,
            ),
        );
    }
    // Records that each share clusters with every other one.
    for n in 40..80 {
        lab.rec(
            MIB,
            &file(
                n,
                1,
                fref(n as u64 + 1, 1),
                "z",
                t(u64::from(n % 7)),
                Some((0, 1 << 30)),
            ),
        );
    }
    let run = lab.run(None);
    let again = run.reanalyze(&Overrides::new());
    assert_eq!(again.instances(), run.layout.instances());
}

#[test]
fn hostile_random_records_never_panic_and_stay_deterministic() {
    let mut rng = hds_io::testing::XorShift::new(0x5EED);
    for round in 0..6 {
        let mut lab = Lab::new(8);
        for _ in 0..300 {
            let n = rng.below(400) as u32;
            let origin = rng.below(4) * MIB;
            let parent = fref(rng.below(400), rng.below(4) as u16);
            let when = t(rng.below(3000));
            let r = if rng.below(4) == 0 {
                dir(n, rng.below(3) as u16, parent, "d", when)
            } else {
                let run = (rng.below(3) != 0).then(|| (rng.below(3000), 1 + rng.below(64)));
                file(n, rng.below(3) as u16, parent, "f.png", when, run)
            };
            let r = if rng.below(10) == 0 { r.flags(0) } else { r };
            let r = if rng.below(10) == 0 {
                r.base(fref(rng.below(400), 1))
            } else {
                r
            };
            lab.rec(origin + 64 * 1024, &r);
        }
        if round % 2 == 0 {
            lab.current_volume(0, 2048, 16, 64, t(1500), &[30, 31], &[100..200]);
        }
        let run = lab.run(None);
        let again = run.reanalyze(&Overrides::new());
        assert_eq!(again.instances(), run.layout.instances(), "round {round}");
        assert_eq!(again.main(), run.layout.main());
    }
}

#[test]
fn hostile_many_tiny_fragments_stay_fast() {
    // 20000 records, each on a grid of its own, naming random parents:
    // 20000 instances that all go to strays, without quadratic work.
    let mut lab = Lab::new(64);
    let mut rng = hds_io::testing::XorShift::new(99);
    for k in 0..20_000u64 {
        let n = 100 + rng.below(1 << 20) as u32;
        let parent = fref(100 + rng.below(1 << 20), 1);
        lab.rec_at(
            k * 3 * 1024,
            &file(n, 1, parent, "f.jpg", t(k % 900), Some((k, 1))),
        );
    }
    let started = std::time::Instant::now();
    let run = lab.run(None);
    let l = &run.layout;
    assert_eq!(l.instances().len(), 1);
    assert_eq!(l.instances()[0].kind, InstanceKind::Strays);
    assert_eq!(l.instances()[0].records.len(), 20_000);
    eprintln!("20000 tiny fragments: {:?}", started.elapsed());
}

/// Ten million synthetic records through C and D (no source reads): run
/// with `cargo test --release -p hds-core -- --ignored scale --nocapture`.
#[test]
#[ignore]
fn scale_ten_million_records() {
    use crate::live::{LiveSnapshot, LiveState};
    use crate::scan::{scan, NoObserver, ScanOptions};
    use crate::session::{Session, SessionMeta, SourceIdentity};
    use std::time::Instant;

    let total: u64 = std::env::var("HDS_SCALE_RECORDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000_000);
    let src = synth::Synth::new(total);
    let source: std::sync::Arc<dyn hds_io::BlockSource> = std::sync::Arc::new(src);
    let range = 0..source.size();
    let ranged = hds_io::RangedSource::new(source.clone(), range.clone()).unwrap();
    let options = ScanOptions {
        collect_carve_hits: false,
        ..ScanOptions::default()
    };
    let meta = SessionMeta::new(SourceIdentity::from(&source.info()), range, options).unwrap();
    let mut session = Session::new(meta);
    let cancel = AtomicBool::new(false);
    let t0 = Instant::now();
    let _ = scan(&ranged, &options, &mut session, &mut NoObserver, &cancel);
    let t_scan = t0.elapsed();
    let rss0 = synth::rss_mib();
    let records = session.store().record_count();
    let live = LiveState {
        scan_time: Some(SCAN),
        snapshot: Some(LiveSnapshot::default()),
    };
    let t1 = Instant::now();
    let mut heads = crate::geometry::Heads::new();
    let input = AnalysisInput::new(session.store(), session.meta(), &live);
    let layout = analyze(&input, &mut heads, &Overrides::new(), &cancel).unwrap();
    let t_an = t1.elapsed();
    let peak = synth::peak_rss_mib();
    eprintln!(
        "SCALE: {records} records ({} fragments, {} cuts, {} instances, main {:?}); scan {:.1}s; \
         analysis {:.1}s; RSS after scan {rss0} MiB, peak {peak} MiB",
        layout.fragments().len(),
        layout.cuts().len(),
        layout.instances().len(),
        layout.main(),
        t_scan.as_secs_f64(),
        t_an.as_secs_f64()
    );
    assert!(records as u64 >= total);
    // The synthetic disk: 50 fragments of one file system and an older
    // generation behind the last one.
    let main = layout.instance(layout.main().unwrap()).unwrap();
    assert!(main.records.len() as u64 >= total * 9 / 10);
    assert_eq!(layout.cuts().len(), 1);
}

/// A synthetic source generating records on the fly (scale test).
mod synth {
    use super::*;
    use hds_io::{BlockSource, SourceInfo};

    pub struct Synth {
        total: u64,
        frag: u64,
    }

    pub const OLDER: u64 = 200_000;

    impl Synth {
        /// `total` records of one file system in 50 fragments, records
        /// built when they are read.
        pub fn new(total: u64) -> Synth {
            Synth {
                total,
                frag: total.div_ceil(50),
            }
        }

        /// Fragment k holds records [k*frag, (k+1)*frag) at disk
        /// 64 MiB + k * (frag * 1024 + 1 MiB); the older generation (OLDER
        /// records numbered above `total`) follows the last fragment on its
        /// grid.
        fn locate(&self, off: u64) -> Option<(u64, bool)> {
            let base = 64 * MIB;
            if off < base {
                return None;
            }
            let stride = self.frag * 1024 + MIB;
            let k = ((off - base) / stride).min(49);
            let within = off - base - k * stride;
            if within % 1024 != 0 {
                return None;
            }
            let i = within / 1024;
            if k < 49 {
                (i < self.frag).then_some((k * self.frag + i, false))
            } else if k == 49 {
                let n = 49 * self.frag + i;
                if n < self.total {
                    Some((n, false))
                } else if n < self.total + OLDER {
                    Some((n, true))
                } else {
                    None
                }
            } else {
                None
            }
        }

        fn record(&self, n: u64, older: bool) -> Vec<u8> {
            let n32 = n as u32;
            let dirs_every = 50u64;
            let when = if older { t(n % 900) } else { t(1000 + n % 900) };
            if n % dirs_every == 0 || n < 30 {
                // System records 0..29 sit in the root (sequence 5 like it).
                let seq = if n < 30 {
                    5
                } else if older {
                    1
                } else {
                    2
                };
                let parent = if n < 30 {
                    ROOT
                } else {
                    fref((n / 2500) * dirs_every + 50, seq)
                };
                dir(n32, seq, parent, "d", when).build()
            } else {
                // Older records name directories that were reused with a
                // new sequence: contradictions on top of the time boundary.
                let parent = if older && n % 100 == 1 {
                    fref(
                        (49 * self.frag).div_ceil(dirs_every) * dirs_every
                            + (n % 1000 / 50) * dirs_every,
                        1,
                    )
                } else if n < dirs_every {
                    ROOT
                } else {
                    fref((n / dirs_every) * dirs_every, if older { 1 } else { 2 })
                };
                file(n32, 1, parent, "f.jpg", when, Some((n * 3, 2))).build()
            }
        }
    }

    impl BlockSource for Synth {
        fn size(&self) -> u64 {
            64 * MIB + 50 * (self.frag * 1024 + MIB) + (OLDER + self.total) * 1024 + 64 * MIB
        }
        fn sector_size(&self) -> u32 {
            512
        }
        fn physical_sector_size(&self) -> u32 {
            512
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
            buf.fill(0);
            let mut pos = offset.div_ceil(1024) * 1024;
            let end = offset + buf.len() as u64;
            while pos + 1024 <= end {
                if let Some((n, older)) = self.locate(pos) {
                    let r = self.record(n, older);
                    let at = (pos - offset) as usize;
                    buf[at..at + 1024].copy_from_slice(&r);
                }
                pos += 1024;
            }
            Ok(())
        }
        fn info(&self) -> SourceInfo {
            let mut i = hds_io::testing::MemSource::new(vec![0; 512]).info();
            i.size = self.size();
            i
        }
        fn is_alive(&self) -> bool {
            true
        }
    }

    fn status_kib(key: &str) -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with(key))
                    .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
            })
            .unwrap_or(0)
    }

    pub fn rss_mib() -> u64 {
        status_kib("VmRSS:") / 1024
    }

    pub fn peak_rss_mib() -> u64 {
        status_kib("VmHWM:") / 1024
    }
}

/// Reviewer: many record-0 holders whose $MFT runs cover the whole disk:
/// each placement scans every record inside its runs (holders x records).
#[test]
#[ignore]
fn rv_many_mft_holders() {
    let holders: u64 = std::env::var("RV_HOLDERS").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    let recs: u32 = std::env::var("RV_RECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30000);
    let mib = (8 + u64::from(recs) * 1024 / MIB + holders * 3 * 1024 / MIB + 4) as usize;
    let mut lab = Lab::new(mib);
    for n in 30..30 + recs {
        lab.rec(4 * MIB + holders * 3 * 1024, &file(n, 1, ROOT, "f.txt", t(100), None));
    }
    let base = 2 * MIB;
    for k in 0..holders {
        let r0 = Rec::new(0)
            .si([t(1); 4], 6)
            .name(ROOT, "$MFT", 3, 6)
            .data_extent(0, (1 << 30) - 1, (1 << 42, 1 << 42, 1 << 42), &runlist(&[(Some(0), 1 << 30)]), 0);
        lab.rec_at(base + k * 3 * 1024, &r0);
    }
    let t0 = std::time::Instant::now();
    let run = lab.run(None);
    println!("records {} instances {} scan+analysis {:.2}s", run.store().record_count(), run.layout.instances().len(), t0.elapsed().as_secs_f64());
}

/// Reviewer: an override is keyed by the instance's lowest record offset;
/// records found later at a lower offset (a resumed scan) change the key
/// and the stored choice no longer applies.
#[test]
fn rv_override_key_changes_when_records_are_added() {
    let mut lab = Lab::new(16);
    // B2: directories 30..49 at 5 MiB (old times).
    let o2 = 5 * MIB - 30 * 1024;
    for n in 30..50 {
        lab.rec(o2, &dir(n, 1, ROOT, &format!("d{n}"), t(10)));
    }
    // B1: files 300..329 at 1 MiB naming those directories.
    let o1 = MIB - 300 * 1024;
    for n in 300..330 {
        lab.rec(o1, &file(n, 1, fref(u64::from(30 + n % 20), 1), "b.txt", t(10), None));
    }
    // A: newer files 30..79 at 8 MiB (sequence 2: they never resolve B1's references).
    let oa = 8 * MIB - 30 * 1024;
    for n in 30..80 {
        lab.rec(oa, &file(n, 2, ROOT, "a.txt", t(1000), None));
    }
    // Stopped scan: only [4 MiB, 16 MiB) read so far. The user picks B.
    let partial = lab.run(Some(4 * MIB..16 * MIB));
    let b = partial.layout.instance(partial.inst(5 * MIB).unwrap()).unwrap().clone();
    let mut ov = Overrides::new();
    ov.set_kind(b.key, Some(KindChoice::Main));
    let partial2 = lab.run_with(Some(4 * MIB..16 * MIB), &ov);
    assert_eq!(partial2.kind(5 * MIB), Some(InstanceKind::Main));
    // Resumed scan finds B1 at 1 MiB: same instance, but the choice is gone.
    let full = lab.run_with(None, &ov);
    assert_eq!(full.inst(MIB), full.inst(5 * MIB), "B1 and B2 are one instance");
    assert_eq!(full.kind(5 * MIB), Some(InstanceKind::Main), "the user's choice of main was lost");
}
