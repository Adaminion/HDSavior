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
use crate::instances::{AnalysisInput, InstanceKind, KindChoice, Overrides, MAX_WITNESSES};

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

#[test]
fn f4_an_mft_map_beats_a_boot_sector_consistent_with_its_hidden_sectors() {
    // F4: mft, then boot. The hidden-sector preference of F2 only orders
    // the boot readings among themselves.
    let mut lab = Lab::new(32);
    lab.put(0, &boot_sector(512, 8, 16384, 100, 8, -10, 1, 0));
    let vol = 16 * MIB;
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
    assert_eq!((g.volume_offset, g.offset_source), (vol, OffsetSource::Mft));
    assert!(g.validated);
}

#[test]
fn f3_a_new_snapshot_can_need_heads_that_were_not_stored() {
    // Review finding: the F3 sample is taken over the files outside the
    // overwritten set, so a new LIVE snapshot shifts it. With more than
    // 200 checkable files an analysis without a source then misses heads;
    // `misses` reports it, and an analysis with the source stores them.
    let mut lab = Lab::new(16);
    lab.current_volume(0, 4096, 16, 64, t(1500), &[], &[]);
    for i in 0..300u32 {
        let lcn = 1000 + u64::from(i);
        lab.rec(
            12 * MIB,
            &file(
                30 + i,
                1,
                ROOT,
                &format!("p{i}.png"),
                t(100),
                Some((lcn, 1)),
            ),
        );
        // The PNG signature only: it matches the extension, but is no
        // carve hit, so no signature anchor offsets (which would read
        // the heads of every file) are tried.
        lab.put(lcn * CS, b"\x89PNG\r\n\x1a\n");
    }
    let run = lab.run(None);
    assert_eq!(run.store().carve_hits().count(), 0);
    let id = run.inst(12 * MIB + 30 * 1024).unwrap();
    let stored = {
        let mut h = Heads::new();
        h.merge_payload(&run.heads.to_payload()).unwrap();
        h
    };
    // The current volume writes over the clusters of files 10..100.
    let mut live = run.live.clone();
    live.snapshot.as_mut().unwrap().volumes[0]
        .allocated
        .insert(1010 * CS..1100 * CS);
    let input = AnalysisInput::new(run.store(), run.session.meta(), &live);
    let cancel = AtomicBool::new(false);
    let mut heads = Heads::new();
    heads.merge_payload(&stored.to_payload()).unwrap();
    let without =
        crate::instances::analyze(&input, &mut heads, &Overrides::new(), &cancel).unwrap();
    assert!(heads.misses() > 0);
    let g = without.instance(id).unwrap().own_geometry.clone().unwrap();
    assert!(g.signature_checked < 200, "{}", g.signature_checked);
    // With the source: nothing missing, 200 checked, the new heads stored.
    let mut heads = Heads::new();
    heads.merge_payload(&stored.to_payload()).unwrap();
    heads.attach(Some(run.ranged.clone()));
    let with = crate::instances::analyze(&input, &mut heads, &Overrides::new(), &cancel).unwrap();
    assert_eq!(heads.misses(), 0);
    let g = with.instance(id).unwrap().own_geometry.clone().unwrap();
    assert_eq!((g.signature_checked, g.signature_ok), (200, 200));
    // From then on, the analysis without a source comes out the same.
    let mut again = Heads::new();
    again.merge_payload(&heads.to_payload()).unwrap();
    let later = crate::instances::analyze(&input, &mut again, &Overrides::new(), &cancel).unwrap();
    assert_eq!(again.misses(), 0);
    assert_eq!(later.instances(), with.instances());
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
    let mut o = Overrides::new();
    o.set_geometry(run.store(), &run.layout, id, Some((4096, MIB)));
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

/// Signature-anchor votes per offset counted naively: for every PNG file
/// of the instance at `origin`, every PNG hit `d` votes for `d - lcn x cs`.
fn naive_png_votes(run: &Run, lcns: &[u64]) -> std::collections::HashMap<u64, u64> {
    let hits: Vec<u64> = run
        .store()
        .carve_hits()
        .filter(|h| h.kind() == hds_carve::FileKind::Png)
        .map(|h| h.disk_offset())
        .collect();
    let mut votes = std::collections::HashMap::new();
    for &l in lcns {
        let mut mine = std::collections::BTreeSet::new();
        for &d in &hits {
            if d >= l * CS {
                mine.insert(d - l * CS);
            }
        }
        for o in mine {
            *votes.entry(o).or_insert(0u64) += 1;
        }
    }
    votes
}

#[test]
fn f2_many_signature_anchors_keep_only_the_best_candidates() {
    // Review finding: every offset with 3 votes became a stored candidate
    // (and the vote streams were copied per file). 10,240 PNG heads at
    // every sector of [6 MiB, 11 MiB) give thousands of offsets with
    // votes from all 25 files.
    let mut lab = Lab::new(16);
    signatures(&mut lab, 25, 12 * MIB);
    let mut o = 6 * MIB;
    while o < 11 * MIB {
        lab.put(o, &png_head());
        o += 512;
    }
    let run = lab.run(None);
    let inst = run
        .layout
        .instance(run.inst(12 * MIB + 30 * 1024).unwrap())
        .unwrap();
    let lcns: Vec<u64> = (0..25).map(png_lcn).collect();
    let naive = naive_png_votes(&run, &lcns);
    let anchored = naive.values().filter(|&&v| v >= ANCHOR_VOTES).count();
    assert!(anchored > 5000, "{anchored} offsets with 3 votes");
    assert!(
        inst.candidates.len() <= MAX_CANDIDATES,
        "{} candidates kept",
        inst.candidates.len()
    );
    // The votes counted by the merge equal the naive count, and the kept
    // ones are the best: all with the most votes.
    let top = naive.values().copied().max().unwrap();
    assert_eq!(top, 25);
    for c in &inst.candidates {
        assert_eq!(
            c.signature_votes,
            naive.get(&c.volume_offset).copied().unwrap_or(0),
            "votes at {}",
            c.volume_offset
        );
    }
    assert!(inst.candidates[..CHECK_CANDIDATES]
        .iter()
        .all(|c| c.signature_votes == top));
    // The true offset still wins and is validated.
    let g = inst.own_geometry.as_ref().unwrap();
    assert_eq!(
        (g.volume_offset, g.signature_checked, g.signature_ok),
        (0, 25, 25)
    );
    assert!(g.validated);
    // F7 on an offset that is not among the kept candidates: its evidence
    // is gathered for that pair.
    let (&far, &votes) = naive
        .iter()
        .filter(|&(o, &v)| v >= ANCHOR_VOTES && o % 512 == 0)
        .filter(|(o, _)| inst.candidates.iter().all(|c| c.volume_offset != **o))
        .max_by_key(|&(o, v)| (*v, *o))
        .unwrap();
    let mut ov = Overrides::new();
    ov.set_geometry(run.store(), &run.layout, inst.id, Some((4096, far)));
    let l = run.reanalyze(&ov);
    let g = l.instance(inst.id).unwrap().geometry.clone().unwrap();
    assert_eq!(
        (g.volume_offset, g.offset_source),
        (far, OffsetSource::Manual)
    );
    assert_eq!(g.signature_votes, votes);
}

#[test]
fn f2_f5_many_boot_hits_are_not_quadratic() {
    // Review finding: every candidate's F5 scanned every boot hit, and all
    // 2 x hits candidates were sized and stored.
    let n = 5000u64;
    let mut lab = Lab::new(16 + (n * 2048 / MIB) as usize + 2);
    for k in 0..n {
        let off = 16 * MIB + k * 2048;
        lab.put(
            off,
            &boot_sector(512, 8, 16383 + k, 4, 8, -10, 1, (off / 512) as u32),
        );
    }
    resident_files(&mut lab, 4 * MIB, 30..60);
    let t0 = std::time::Instant::now();
    let run = lab.run(None);
    let secs = t0.elapsed().as_secs_f64();
    let inst = run
        .layout
        .instance(run.inst(4 * MIB + 30 * 1024).unwrap())
        .unwrap();
    assert!(
        inst.candidates.len() <= MAX_CANDIDATES,
        "{} candidates kept",
        inst.candidates.len()
    );
    // The best boot reading (consistent with its hidden sectors) is chosen
    // and sized by its own boot sector.
    let g = inst.own_geometry.as_ref().unwrap();
    assert_eq!(g.offset_source, OffsetSource::Boot);
    assert_eq!(g.volume_offset, 16 * MIB);
    assert_eq!(g.total_clusters, 16383 * 512 / CS);
    assert!(secs < 30.0, "{secs:.1} s");
}

#[test]
fn d4b_reads_no_heads_for_instances_too_small_to_check() {
    // Review finding: D4(b) read one head per (validated instance,
    // one-record instance) pair, though fewer than 3 checkable files give
    // neither evidence nor a contradiction.
    let n = 300u64;
    let mut lab = Lab::new(64);
    signatures(&mut lab, 25, 12 * MIB);
    for k in 0..n {
        let num = 100_000 + k as u32 * 7;
        lab.rec_at(
            20 * MIB + k * 3 * 1024,
            &file(num, 1, ROOT, "x.jpg", t(100), Some((3000 + k, 1))),
        );
    }
    let run = lab.run(None);
    let main = own(&run, 12 * MIB + 30 * 1024);
    assert!(main.validated);
    let mut alone = Lab::new(64);
    signatures(&mut alone, 25, 12 * MIB);
    let base = alone.run(None).heads.reads();
    assert_eq!(
        run.heads.reads(),
        base,
        "the one-record instances cost no head reads"
    );
    // They are strays, as before.
    assert_eq!(run.kind(20 * MIB), Some(InstanceKind::Strays));
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
    o.set_kind(run.store(), &run.layout, older.id, Some(KindChoice::Other));
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
    o.push_kind(vec![10, 4096], KindChoice::Main);
    o.push_kind(vec![30], KindChoice::Other);
    o.push_geometry(vec![20, 1 << 40], 8192, 1 << 20);
    assert!(!o.is_empty());
    let p = o.to_payload();
    assert_eq!(Overrides::from_payload(&p).unwrap(), o);
    for cut in 0..p.len() {
        assert!(Overrides::from_payload(&p[..cut]).is_err());
    }
    // A count bomb, an empty target, too many witnesses, a wrong version.
    let mut bomb = p[..5].to_vec();
    bomb.extend_from_slice(&(u64::MAX / 2).to_le_bytes());
    assert!(Overrides::from_payload(&bomb).is_err());
    let mut empty = Overrides::new();
    empty.push_kind(Vec::new(), KindChoice::Main);
    assert!(Overrides::from_payload(&empty.to_payload()).is_err());
    let mut many = Overrides::new();
    many.push_kind((0..=MAX_WITNESSES as u64).collect(), KindChoice::Main);
    assert!(Overrides::from_payload(&many.to_payload()).is_err());
    let mut v9 = p.clone();
    v9[4] = 9;
    assert!(Overrides::from_payload(&v9).is_err());
    // An impossible cluster size is refused.
    let mut bad = Overrides::new();
    bad.push_geometry(vec![1], 3000, 0);
    assert!(Overrides::from_payload(&bad.to_payload()).is_err());
    // Version 1 (one lowest-record offset per override) is still read.
    let mut v1 = b"HDOV".to_vec();
    v1.push(1);
    v1.extend_from_slice(&1u64.to_le_bytes());
    v1.extend_from_slice(&4096u64.to_le_bytes());
    v1.push(0);
    v1.extend_from_slice(&1u64.to_le_bytes());
    v1.extend_from_slice(&8192u64.to_le_bytes());
    v1.extend_from_slice(&4096u32.to_le_bytes());
    v1.extend_from_slice(&(1u64 << 20).to_le_bytes());
    let o1 = Overrides::from_payload(&v1).unwrap();
    assert_eq!(o1.kinds().len(), 1);
    assert_eq!(o1.kinds()[0].0.offsets(), &[4096]);
    assert_eq!(o1.kinds()[0].1, KindChoice::Main);
    assert_eq!(o1.geometries()[0].0.offsets(), &[8192]);
    assert_eq!(
        (o1.geometries()[0].1, o1.geometries()[0].2),
        (4096, 1 << 20)
    );
}
