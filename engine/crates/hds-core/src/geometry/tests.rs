//! ALGORITHMS F1-F7 on constructed instances, and the `HEADS` payload.

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

use super::*;
use crate::instances::testlab::*;
use crate::instances::{InstanceKind, KindChoice, Overrides};

const MIB: u64 = 1 << 20;
const CS: u64 = 4096;

/// The own geometry of the instance holding the record at `off`.
fn own(run: &Run, off: u64) -> Geometry {
    let i = run.inst(off).unwrap();
    run.layout
        .instance(i)
        .unwrap()
        .own_geometry
        .clone()
        .unwrap()
}

fn resident_files(lab: &mut Lab, origin: u64, range: std::ops::Range<u32>) {
    for n in range {
        lab.rec(origin, &file(n, 1, ROOT, "r.txt", t(100), None));
    }
}

#[test]
fn f1_cluster_size_from_attributes_then_boot_then_default() {
    // 8 KiB clusters by the attributes.
    let mut lab = Lab::new(8);
    for n in 30..50 {
        lab.rec(
            MIB,
            &file_cs(
                n,
                1,
                ROOT,
                "a.bin",
                t(100),
                Some((100 + u64::from(n) * 4, 3)),
                8192,
            ),
        );
    }
    let run = lab.run(None);
    let g = own(&run, MIB + 30 * 1024);
    assert_eq!(
        (g.cluster_size, g.cluster_source),
        (8192, ClusterSource::Attr)
    );
    // No non-resident attribute: the boot sector at the range start.
    let mut lab = Lab::new(8);
    lab.put(0, &boot_sector(512, 32, 16383, 4, 8, -10, 1, 0));
    resident_files(&mut lab, MIB, 30..50);
    let run = lab.run(None);
    let g = own(&run, MIB + 30 * 1024);
    assert_eq!(
        (g.cluster_size, g.cluster_source),
        (16384, ClusterSource::Boot)
    );
    // A boot sector elsewhere does not count: 4096 by default.
    let mut lab = Lab::new(8);
    lab.put(512 * 7, &boot_sector(512, 32, 16383, 4, 8, -10, 1, 0));
    resident_files(&mut lab, MIB, 30..50);
    let run = lab.run(None);
    let g = own(&run, MIB + 30 * 1024);
    assert_eq!(
        (g.cluster_size, g.cluster_source),
        (4096, ClusterSource::Default)
    );
}

#[test]
fn f2_the_mft_map_places_the_records_and_validates_structurally() {
    // Volume at 2 MiB; its $MFT at LCN 256 (disk 3 MiB); record 0 survived.
    let mut lab = Lab::new(16);
    let vol = 2 * MIB;
    let origin = vol + 256 * CS;
    let r0 = Rec::new(0)
        .si([t(1); 4], 6)
        .name(ROOT, "$MFT", 3, 6)
        .data_extent(
            0,
            63,
            (64 * CS, 256 * 1024, 256 * 1024),
            &runlist(&[(Some(256), 64)]),
            0,
        );
    lab.rec(origin, &r0);
    resident_files(&mut lab, origin, 30..50);
    let run = lab.run(None);
    let g = own(&run, origin);
    assert_eq!(g.volume_offset, vol);
    assert_eq!(g.offset_source, OffsetSource::Mft);
    assert_eq!(g.mft_agree, 20, "record 0 itself does not count");
    assert!(
        g.validated,
        "16 or more agreeing records are structural evidence"
    );
    assert_eq!(g.signature_checked, 0);
}

#[test]
fn f2_index_votes_count_distinct_directories() {
    // Volume at 1 MiB; three directories with $I30 at LCN 500, 510, 520;
    // their INDX blocks are found there (two for the first).
    let mut lab = Lab::new(16);
    let vol = MIB;
    let origin = 4 * MIB;
    for (k, lcn) in [500u64, 510, 520].into_iter().enumerate() {
        let n = 30 + k as u32;
        let d = dir(n, 1, ROOT, "d", t(100)).index_allocation(
            0,
            1,
            2 * CS,
            &runlist(&[(Some(lcn), 2)]),
        );
        lab.rec(origin, &d);
        let e = index_entry(
            fref(40 + k as u64, 1),
            fref(u64::from(n), 1),
            "child",
            1,
            0x20,
            [t(100); 4],
        );
        lab.put(vol + lcn * CS, &indx_block(0, 1, 4096, &[e.clone()]));
        if k == 0 {
            lab.put(vol + (lcn + 1) * CS, &indx_block(1, 1, 4096, &[e]));
        }
    }
    resident_files(&mut lab, origin, 40..60);
    let run = lab.run(None);
    let g = own(&run, origin + 30 * 1024);
    assert_eq!(g.volume_offset, vol);
    assert_eq!(g.offset_source, OffsetSource::Index);
    assert_eq!((g.index_dirs, g.index_blocks), (3, 4));
    assert!(g.validated);
}

#[test]
fn f2_a_backup_boot_sector_consistent_with_its_hidden_sectors_wins() {
    // Volume at 1 MiB (2048 sectors hidden) of 8191 sectors: only its
    // backup boot sector survived, at 1 MiB + 8191 x 512.
    let mut lab = Lab::new(8);
    lab.put(
        MIB + 8191 * 512,
        &boot_sector(512, 8, 8191, 4, 8, -10, 1, 2048),
    );
    resident_files(&mut lab, 3 * MIB, 30..50);
    let run = lab.run(None);
    let g = own(&run, 3 * MIB + 30 * 1024);
    assert_eq!(
        (g.volume_offset, g.offset_source),
        (MIB, OffsetSource::BackupBoot)
    );
    // F5: the boot sector agrees with the geometry: total_clusters from it.
    assert_eq!(g.total_clusters, 8191 * 512 / CS);
    let cands: Vec<(u64, OffsetSource)> = run
        .layout
        .instance(run.inst(3 * MIB + 30 * 1024).unwrap())
        .unwrap()
        .candidates
        .iter()
        .map(|c| (c.volume_offset, c.offset_source))
        .collect();
    assert!(cands.contains(&(MIB + 8191 * 512, OffsetSource::Boot)));
    assert!(cands.contains(&(0, OffsetSource::Default)));
}

/// LCN offsets with pairwise distinct differences (a Sidon set): only
/// the true volume offset collects 3 signature-anchor votes.
fn sidon(n: usize) -> Vec<u64> {
    let mut v: Vec<u64> = Vec::new();
    let mut diffs = std::collections::BTreeSet::new();
    let mut x = 0u64;
    while v.len() < n {
        if v.iter().all(|&a| !diffs.contains(&(x - a))) {
            for &a in &v {
                diffs.insert(x - a);
            }
            v.push(x);
        }
        x += 1;
    }
    v
}

/// The LCN of PNG file `i`.
fn png_lcn(i: usize) -> u64 {
    1000 + sidon(25)[i]
}

/// 25 PNG files (records at `origin`) whose heads are at volume offset 0
/// (the first `good` of them), and a boot sector consistent with its hidden
/// sectors claiming a volume at 1 MiB.
fn signatures(lab: &mut Lab, good: u32, origin: u64) {
    lab.put(MIB, &boot_sector(512, 8, 16383, 4, 8, -10, 1, 2048));
    for i in 0..25u32 {
        let lcn = png_lcn(i as usize);
        lab.rec(
            origin,
            &file(
                30 + i,
                1,
                ROOT,
                &format!("p{i}.png"),
                t(100),
                Some((lcn, 1)),
            ),
        );
        if i < good {
            lab.put(lcn * CS, &png_head());
        } else {
            lab.put(lcn * CS, b"not a picture at all, just text");
        }
    }
}

#[test]
fn f3_f4_signature_matches_decide_and_validate() {
    let mut lab = Lab::new(16);
    signatures(&mut lab, 25, 12 * MIB);
    let run = lab.run(None);
    let g = own(&run, 12 * MIB + 30 * 1024);
    assert_eq!(
        g.volume_offset, 0,
        "25 matches at 0 beat the boot sector at 1 MiB"
    );
    assert_eq!((g.signature_checked, g.signature_ok), (25, 25));
    assert!(g.validated);
    let boot = run
        .layout
        .instance(run.inst(12 * MIB + 30 * 1024).unwrap())
        .unwrap()
        .candidates
        .iter()
        .find(|c| c.volume_offset == MIB)
        .cloned()
        .unwrap();
    assert_eq!((boot.signature_checked, boot.signature_ok), (25, 0));
    // The heads are stored: the analysis from stored state reads nothing.
    let again = run.reanalyze(&Overrides::new());
    assert_eq!(again.instances(), run.layout.instances());
}

#[test]
fn f6_too_few_matches_without_structure_is_not_validated() {
    let mut lab = Lab::new(16);
    signatures(&mut lab, 15, 12 * MIB); // 60 %
    let run = lab.run(None);
    let g = own(&run, 12 * MIB + 30 * 1024);
    assert_eq!(
        (g.volume_offset, g.signature_checked, g.signature_ok),
        (0, 25, 15)
    );
    assert!(!g.validated);
}

#[test]
fn f3_files_outside_the_range_or_overwritten_are_not_checked() {
    // The range ends after the 20th file's cluster: 20 checkable.
    let mut lab = Lab::new(16);
    signatures(&mut lab, 25, 2 * MIB);
    let end = (png_lcn(19) + 1) * CS;
    let run = lab.run(Some(0..end.div_ceil(MIB) * MIB));
    let g = own(&run, 2 * MIB + 30 * 1024);
    let inside = (0..25)
        .filter(|&i| (png_lcn(i) + 1) * CS <= end.div_ceil(MIB) * MIB)
        .count();
    assert_eq!(g.signature_checked, inside as u64);
    // Clusters a current volume allocated are overwritten: not checkable.
    let mut lab = Lab::new(16);
    lab.current_volume(0, 4096, 16, 64, t(1500), &[], &[png_lcn(0)..png_lcn(4) + 1]);
    signatures(&mut lab, 25, 12 * MIB);
    let run = lab.run(None);
    let g = own(&run, 12 * MIB + 30 * 1024);
    assert_eq!((g.signature_checked, g.signature_ok), (20, 20));
}

#[test]
fn f5_volume_size_and_lost_volume_range() {
    let mut lab = Lab::new(32);
    // File ends (highest cluster + 1): 100, 200, 300, 400, 5000.
    for (i, end) in [100u64, 200, 300, 400, 5000].into_iter().enumerate() {
        lab.rec(
            MIB,
            &file(
                30 + i as u32,
                1,
                ROOT,
                "a.bin",
                t(100),
                Some((end - 10, 10)),
            ),
        );
    }
    resident_files(&mut lab, MIB, 40..52);
    let run = lab.run(None);
    let g = own(&run, MIB + 30 * 1024);
    assert_eq!(g.volume_offset, 0);
    assert_eq!(g.end_by_files, 200, "the 4th-largest end");
    assert_eq!(g.end_of_all, 5000);
    // No boot sector: max(end_by_files, range end / cluster size).
    assert_eq!(g.total_clusters, 32 * MIB / CS);
    assert_eq!(g.lost_volume, 0..32 * MIB);
    // A boot sector at 0 that agrees: total_clusters from it; the range
    // still reaches the largest file end.
    let mut lab2 = Lab::new(32);
    lab2.img = lab.img.clone();
    lab2.put(0, &boot_sector(512, 8, 4096 * 8, 4, 8, -10, 1, 0));
    let run = lab2.run(None);
    let g = own(&run, MIB + 30 * 1024);
    assert_eq!(g.total_clusters, 4096);
    assert_eq!(g.lost_volume, 0..5000 * CS);
    // A boot sector smaller than end_by_files is the new, smaller volume's:
    // ignored.
    let mut lab3 = Lab::new(32);
    lab3.img = lab.img.clone();
    lab3.put(0, &boot_sector(512, 8, 150 * 8, 4, 8, -10, 1, 0));
    let run = lab3.run(None);
    let g = own(&run, MIB + 30 * 1024);
    assert_eq!(g.total_clusters, 32 * MIB / CS);
}

#[test]
fn f5_fewer_than_four_files_take_the_smallest_end() {
    let mut lab = Lab::new(32);
    for (i, end) in [300u64, 7000].into_iter().enumerate() {
        lab.rec(
            MIB,
            &file(
                30 + i as u32,
                1,
                ROOT,
                "a.bin",
                t(100),
                Some((end - 10, 10)),
            ),
        );
    }
    resident_files(&mut lab, MIB, 40..56);
    lab.put(0, &boot_sector(512, 8, 1000 * 8, 4, 8, -10, 1, 0));
    let run = lab.run(None);
    let g = own(&run, MIB + 30 * 1024);
    assert_eq!(g.end_by_files, 300);
    assert_eq!(g.total_clusters, 1000);
    assert_eq!(g.lost_volume, 0..7000 * CS);
}

#[test]
fn f7_a_manual_geometry_replaces_the_computed_one() {
    let mut lab = Lab::new(16);
    signatures(&mut lab, 25, 12 * MIB);
    let run = lab.run(None);
    let id = run.inst(12 * MIB + 30 * 1024).unwrap();
    let key = run.layout.instance(id).unwrap().key;
    let mut o = Overrides::new();
    o.set_geometry(key, Some((4096, MIB)));
    let l = run.reanalyze(&o);
    let inst = l.instance(id).unwrap();
    let g = inst.geometry.as_ref().unwrap();
    assert_eq!(
        (g.volume_offset, g.offset_source, g.cluster_source),
        (MIB, OffsetSource::Manual, ClusterSource::Manual)
    );
    assert_eq!((g.signature_checked, g.signature_ok), (25, 0));
    assert!(!g.validated);
    // Not validated: no lost-volume range of it is protected any more.
    assert_eq!(l.protected().len(), 1);
    // The tree is not touched: same instances, same own geometry.
    assert_eq!(
        inst.own_geometry,
        run.layout.instance(id).unwrap().own_geometry
    );
    assert_eq!(inst.records, run.layout.instance(id).unwrap().records);
}

#[test]
fn f4_older_instances_inherit_mains_geometry_and_other_keeps_its_own() {
    let mut lab = Lab::new(16);
    signatures(&mut lab, 25, 12 * MIB); // main, validated at 0, times t(100)
    for n in 30..50 {
        lab.rec(10 * MIB, &file(n, 1, ROOT, "o.txt", t(5), None)); // older, no evidence
    }
    let run = lab.run(None);
    let older = run
        .layout
        .instance(run.inst(10 * MIB + 30 * 1024).unwrap())
        .unwrap();
    assert_eq!(older.kind, InstanceKind::Older);
    let g = older.geometry.as_ref().unwrap();
    assert_eq!(
        (g.offset_source, g.volume_offset, g.validated),
        (OffsetSource::Inherited, 0, true)
    );
    // instance_set "other": read with its own geometry.
    let mut o = Overrides::new();
    o.set_kind(older.key, Some(KindChoice::Other));
    let l = run.reanalyze(&o);
    let g = l.instance(older.id).unwrap().geometry.as_ref().unwrap();
    assert_ne!(g.offset_source, OffsetSource::Inherited);
}

#[test]
fn heads_round_trip_and_hostile_payloads() {
    let mut h = Heads::new();
    let mut b = [0u8; SNIFF_LEN];
    b[0] = 0x89;
    h.insert(4096, Head::Bytes(b));
    h.insert(8192, Head::Unreadable);
    let p = h.take_new().unwrap();
    assert!(h.take_new().is_none());
    let mut g = Heads::new();
    g.merge_payload(&p).unwrap();
    assert_eq!(g.stored(4096), Some(Head::Bytes(b)));
    assert_eq!(g.stored(8192), Some(Head::Unreadable));
    assert_eq!(g.to_payload(), h.to_payload());
    // Without a source an unknown head is unknown, and nothing is read.
    assert_eq!(g.get(12288, &AtomicBool::new(false)).unwrap(), None);
    assert_eq!(g.reads(), 0);
    for cut in 0..p.len() {
        assert!(Heads::new().merge_payload(&p[..cut]).is_err());
    }
    let mut bomb = p[..5].to_vec();
    bomb.extend_from_slice(&(u64::MAX / 2).to_le_bytes());
    assert!(Heads::new().merge_payload(&bomb).is_err());
}

#[test]
fn overrides_round_trip_and_hostile_payloads() {
    let mut o = Overrides::new();
    o.set_kind(10, Some(KindChoice::Main));
    o.set_kind(20, Some(KindChoice::Main)); // replaces the first main
    o.set_kind(30, Some(KindChoice::Other));
    o.set_geometry(20, Some((8192, 1 << 20)));
    assert_eq!(o.kind(10), None);
    assert_eq!(o.kind(20), Some(KindChoice::Main));
    let p = o.to_payload();
    assert_eq!(Overrides::from_payload(&p).unwrap(), o);
    for cut in 0..p.len() {
        assert!(Overrides::from_payload(&p[..cut]).is_err());
    }
    // An impossible cluster size is refused.
    let mut bad = Overrides::new();
    bad.set_geometry(1, Some((3000, 0)));
    assert!(Overrides::from_payload(&bad.to_payload()).is_err());
    o.set_kind(20, None);
    o.set_geometry(20, None);
    o.set_kind(30, None);
    assert!(o.is_empty());
}
