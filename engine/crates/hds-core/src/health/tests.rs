//! Tests of ALGORITHMS G (and the I4 hook): each flag, the overlap
//! conflicts and the ratings on constructed images.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::single_range_in_vec_init
)]

use std::ops::Range;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use hds_io::testing::{FaultySource, MemSource};
use hds_io::{BlockSource, RangedSource};
use hds_recover::{Flag, Rating};

use crate::catalog::tests::support::*;
use crate::catalog::{CarvedEntry, InstanceSetError, NodeId};
use crate::geometry::Heads;
use crate::instances::testlab::*;
use crate::instances::{analyze, AnalysisInput, InstanceKind, Overrides};
use crate::live::{take_snapshot, LiveState};
use crate::scan::{scan, NoObserver, ScanOptions};
use crate::session::{Session, SessionMeta, SourceIdentity};

/// Scans and analyses `source` over `range` (as `Lab::run`, any source).
fn run_source(source: Arc<dyn BlockSource>, range: Range<u64>) -> Run {
    let ranged = RangedSource::new(source.clone(), range.clone()).unwrap();
    let options = ScanOptions::default();
    let meta = SessionMeta::new(SourceIdentity::from(&source.info()), range, options).unwrap();
    let mut session = Session::new(meta);
    let cancel = AtomicBool::new(false);
    scan(&ranged, &options, &mut session, &mut NoObserver, &cancel);
    let snapshot = take_snapshot(&ranged, SCAN, &cancel).unwrap();
    let live = LiveState {
        scan_time: Some(SCAN),
        snapshot: Some(snapshot),
    };
    let mut heads = Heads::new();
    heads.attach(Some(ranged.clone()));
    let layout = {
        let input = AnalysisInput::new(session.store(), session.meta(), &live);
        analyze(&input, &mut heads, &Overrides::new(), &cancel).unwrap()
    };
    Run {
        session,
        live,
        heads,
        layout,
        ranged,
    }
}

fn named(n: u32, name: &str) -> Rec {
    Rec::new(n).si([t(10); 4], 0x20).name(ROOT, name, 1, 0x20)
}

fn one_run(r: Rec, lcn: u64, flags: u16) -> Rec {
    r.data_extent(0, 0, (4096, 4096, 4096), &runlist(&[(Some(lcn), 1)]), flags)
}

#[test]
fn g1_flags_the_records_decide_and_g3_ratings() {
    let mut lab = base_lab(32, 24);
    lab.rec(ORIGIN, &one_run(named(100, "comp.bin"), 3000, 0x0001));
    lab.rec(ORIGIN, &one_run(named(101, "enc.bin"), 3002, 0x4000));
    lab.rec(
        ORIGIN,
        &named(102, "holes.bin").data_extent(
            0,
            2,
            (3 * 4096, 3 * 4096, 3 * 4096),
            &runlist(&[(Some(3004), 1), (None, 1), (Some(3006), 1)]),
            0,
        ),
    );
    lab.rec(
        ORIGIN,
        &one_run(named(103, "cloud.bin"), 3008, 0).reparse(0x9000_101A),
    );
    lab.rec(
        ORIGIN,
        &one_run(named(104, "dedup.bin"), 3010, 0).reparse(0x8000_0013),
    );
    lab.rec(
        ORIGIN,
        &one_run(named(105, "wof.bin"), 3012, 0).reparse(0x8000_0017),
    );
    lab.rec(ORIGIN, &freed(one_run(named(106, "gone.bin"), 3014, 0)));
    let mut torn = one_run(named(107, "torn.bin"), 3016, 0).build();
    tear(&mut torn, 1);
    lab.put(off(107), &torn);
    lab.rec(ORIGIN, &one_run(named(108, "fine.bin"), 3018, 0));
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let check = |p: &str, flag: Option<Flag>, rating: Rating| {
        let id = at(&cat, store, p);
        if let Some(f) = flag {
            assert!(cat.flags(id).contains(f), "{p}: {:?}", cat.flags(id));
        }
        assert_eq!(cat.rating(id), Some(rating), "{p}: {:?}", cat.flags(id));
    };
    check("comp.bin", Some(Flag::Compressed), Rating::Unsupported);
    check("enc.bin", Some(Flag::Encrypted), Rating::Unsupported);
    check("holes.bin", Some(Flag::Sparse), Rating::Good);
    check("cloud.bin", Some(Flag::Placeholder), Rating::Unsupported);
    check("dedup.bin", Some(Flag::Placeholder), Rating::Unsupported);
    check("wof.bin", Some(Flag::Compressed), Rating::Unsupported);
    check("gone.bin", Some(Flag::Deleted), Rating::Uncertain);
    check("torn.bin", Some(Flag::TornRecord), Rating::Uncertain);
    check("fine.bin", None, Rating::Good);
    // read_file refuses what is not stored as plain clusters.
    assert!(matches!(
        cat.readable(at(&cat, store, "comp.bin")),
        Err(crate::query::QueryError::Unsupported(_))
    ));
    assert_eq!(cat.readable(at(&cat, store, "fine.bin")), Ok(()));
    assert!(matches!(
        cat.readable(cat.instances()[0].root),
        Err(crate::query::QueryError::BadRequest(_))
    ));
    assert!(cat.flags(at(&cat, store, "fine.bin")).is_empty());
    // F3 stored the heads of the PNG files: signature_ok without a read.
    assert!(cat
        .flags(at(&cat, store, "p0.png"))
        .contains(Flag::SignatureOk));
    assert_eq!(cat.rating(at(&cat, store, "p0.png")), Some(Rating::Good));
}

#[test]
fn g1_signature_mismatch_and_unvalidated_geometry() {
    let mut lab = base_lab(32, 24);
    // A .png whose first cluster holds no PNG: mismatch (damaged).
    lab.rec(
        ORIGIN,
        &file(100, 1, ROOT, "fake.png", t(10), Some((3000, 1))),
    );
    lab.put(
        3000 * 4096,
        b"%PDF-1.4 not a png at all, really not a png file ...........",
    );
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let fake = at(&cat, store, "fake.png");
    assert!(cat.flags(fake).contains(Flag::SignatureMismatch));
    assert_eq!(cat.rating(fake), Some(Rating::Damaged));

    // Without checkable files nothing validates the geometry: uncertain.
    let mut lab = Lab::new(32);
    for k in 0..20u32 {
        lab.rec(
            ORIGIN,
            &file(
                64 + k,
                1,
                ROOT,
                &format!("t{k}.txt"),
                t(10),
                Some((3000 + u64::from(k), 1)),
            ),
        );
    }
    let run = lab.run(None);
    let cat = catalog(&run);
    assert!(!cat.instances()[0].geometry.validated);
    assert_eq!(
        cat.rating(at(&cat, run.store(), "t0.txt")),
        Some(Rating::Uncertain)
    );
    assert!(cat.flags(at(&cat, run.store(), "t0.txt")).is_empty());
}

#[test]
fn g1_out_of_bounds_and_out_of_range() {
    let mut lab = base_lab(32, 24);
    // A boot sector at 0 says the volume has 6000 clusters.
    lab.put(0, &boot_sector(512, 8, 6000 * 8, 256, 300, -10, 1, 0));
    lab.rec(ORIGIN, &one_run(named(100, "beyond.bin"), 6500, 0));
    lab.rec(
        ORIGIN,
        &named(101, "straddle.bin").data_extent(
            0,
            1,
            (8192, 8192, 8192),
            &runlist(&[(Some(5999), 2)]),
            0,
        ),
    );
    lab.rec(
        ORIGIN,
        &named(102, "edge.bin").data_extent(
            0,
            1,
            (8192, 8192, 8192),
            &runlist(&[(Some(5998), 2)]),
            0,
        ),
    );
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    assert_eq!(cat.instances()[0].geometry.total_clusters, 6000);
    assert!(cat
        .flags(at(&cat, store, "beyond.bin"))
        .contains(Flag::OutOfBounds));
    // LCN + length > total_clusters, not only LCN >= total_clusters.
    assert!(cat
        .flags(at(&cat, store, "straddle.bin"))
        .contains(Flag::OutOfBounds));
    assert!(!cat
        .flags(at(&cat, store, "edge.bin"))
        .contains(Flag::OutOfBounds));
    assert_eq!(
        cat.rating(at(&cat, store, "straddle.bin")),
        Some(Rating::Damaged)
    );

    // Data outside the session range.
    let mut lab = base_lab(32, 24);
    lab.rec(ORIGIN, &one_run(named(100, "far.bin"), 7000, 0));
    let run = lab.run(Some(0..24 * MIB));
    let cat = catalog(&run);
    let far = at(&cat, run.store(), "far.bin");
    assert!(cat.flags(far).contains(Flag::OutOfBounds));
    assert_eq!(cat.health.out_of_range, (1, 4096));
}

#[test]
fn g1_overwritten_by_the_current_volume_and_bad_sectors() {
    let mut lab = base_lab(32, 24);
    lab.rec(ORIGIN, &one_run(named(100, "hit.bin"), 3000, 0));
    lab.rec(ORIGIN, &one_run(named(101, "safe.bin"), 3100, 0));
    lab.rec(ORIGIN, &one_run(named(102, "unreadable.bin"), 3200, 0));
    // The current volume at 0 (MFT at 16 KiB) allocated cluster 3000.
    lab.current_volume(0, 8192, 4, 64, t(500), &[], &[3000..3001]);
    let faulty = Arc::new(FaultySource::new(Arc::new(MemSource::new(lab.img.clone()))));
    faulty.bad_sectors(3200 * 4096 + 512..3200 * 4096 + 1024);
    let run = run_source(faulty, 0..32 * MIB);
    let store = run.store();
    let cat = catalog(&run);
    let hit = at(&cat, store, "hit.bin");
    assert!(cat.flags(hit).contains(Flag::Overwritten));
    assert_eq!(cat.rating(hit), Some(Rating::Damaged));
    assert!(!cat
        .flags(at(&cat, store, "safe.bin"))
        .contains(Flag::Overwritten));
    let bad = at(&cat, store, "unreadable.bin");
    assert!(cat.flags(bad).contains(Flag::BadSectors));
    assert_eq!(cat.rating(bad), Some(Rating::Damaged));
}

#[test]
fn g2_overlap_conflicts() {
    let mut lab = base_lab(32, 24);
    // Two in-use files of main sharing cluster 3000: the later record
    // time wins; the loser is overwritten, the winner overlaps.
    lab.rec(
        ORIGIN,
        &file(100, 1, ROOT, "old.bin", t(10), Some((3000, 1))),
    );
    lab.rec(
        ORIGIN,
        &file(101, 1, ROOT, "new.bin", t(11), Some((3000, 1))),
    );
    // In use beats deleted; the winner gets no overlap.
    lab.rec(
        ORIGIN,
        &freed(file(102, 1, ROOT, "deleted.bin", t(12), Some((3100, 1)))),
    );
    lab.rec(
        ORIGIN,
        &file(103, 1, ROOT, "alive.bin", t(10), Some((3100, 1))),
    );
    // A full tie: both overlap.
    lab.rec(
        ORIGIN,
        &file(104, 1, ROOT, "tie1.bin", t(10), Some((3200, 1))).lsn(7),
    );
    lab.rec(
        ORIGIN,
        &file(105, 1, ROOT, "tie2.bin", t(10), Some((3200, 1))).lsn(7),
    );
    // Strays (three records elsewhere) lose to main whatever their time.
    let strays = 12 * MIB;
    lab.rec(
        strays,
        &file(400, 1, ROOT, "stray.bin", t(20), Some((3300, 1))),
    );
    lab.rec(strays, &file(401, 1, ROOT, "stray2.bin", t(20), None));
    lab.rec(
        ORIGIN,
        &file(106, 1, ROOT, "main.bin", t(10), Some((3300, 1))),
    );
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let f = |p: &str| cat.flags(at(&cat, store, p));
    assert!(f("old.bin").contains(Flag::Overwritten) && !f("old.bin").contains(Flag::Overlap));
    assert!(f("new.bin").contains(Flag::Overlap) && !f("new.bin").contains(Flag::Overwritten));
    assert!(f("deleted.bin").contains(Flag::Overwritten));
    assert!(f("alive.bin").is_empty(), "{:?}", f("alive.bin"));
    assert!(f("tie1.bin").contains(Flag::Overlap) && f("tie2.bin").contains(Flag::Overlap));
    assert!(!f("tie1.bin").contains(Flag::Overwritten));
    let stray = at(&cat, store, "[Stray records]/Folder #5/stray.bin");
    assert!(cat.flags(stray).contains(Flag::Stray) && cat.flags(stray).contains(Flag::Overwritten));
    assert_eq!(cat.rating(stray), Some(Rating::Damaged));
    assert!(f("main.bin").is_empty(), "{:?}", f("main.bin"));
    assert_eq!(
        cat.rating(at(&cat, store, "[Stray records]/Folder #5/stray2.bin")),
        Some(Rating::Uncertain)
    );
}

#[test]
fn g2_the_later_record_time_wins_for_a_restored_file() {
    // Both in use, both of main, sharing cluster 3400. "restored.bin" was
    // copied from a backup after "plain.bin" was written: its $SI
    // MFT-change time (day 5) is older, its $FILE_NAME times (day 20) are
    // newer. The record time decides: the copy wins.
    let mut lab = base_lab(32, 24);
    lab.rec(
        ORIGIN,
        &file(110, 1, ROOT, "plain.bin", t(10), Some((3400, 1))),
    );
    lab.rec(
        ORIGIN,
        &copied_file(111, 1, ROOT, "restored.bin", t(5), t(20), Some((3400, 1))),
    );
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let f = |p: &str| cat.flags(at(&cat, store, p));
    assert!(
        f("plain.bin").contains(Flag::Overwritten),
        "{:?}",
        f("plain.bin")
    );
    assert!(f("restored.bin").contains(Flag::Overlap));
    assert!(!f("restored.bin").contains(Flag::Overwritten));
}

#[test]
fn g1_older_generation_files() {
    let mut lab = base_lab(32, 30);
    // A file system from before: its own fragment, older times, text
    // files only (nothing ties it to main).
    let older = 8 * MIB;
    for k in 0..20u32 {
        lab.rec(
            older,
            &file(
                300 + k,
                1,
                ROOT,
                &format!("o{k}.txt"),
                t(1),
                Some((3000 + u64::from(k), 1)),
            ),
        );
    }
    // One of them shares a cluster with a main file: older loses.
    lab.rec(ORIGIN, &file(100, 1, ROOT, "m.txt", t(10), Some((3000, 1))));
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let o = cat
        .instances()
        .iter()
        .find(|i| i.kind == InstanceKind::Older)
        .expect("an older instance");
    assert_eq!(o.label, "[Older file system]");
    let o0 = at(&cat, store, "[Older file system]/o0.txt");
    assert!(cat.flags(o0).contains(Flag::OlderGeneration));
    assert!(cat.flags(o0).contains(Flag::Overwritten));
    assert!(cat
        .flags(at(&cat, store, "[Older file system]/o1.txt"))
        .contains(Flag::OlderGeneration));
    assert_ne!(
        cat.rating(at(&cat, store, "[Older file system]/o1.txt")),
        Some(Rating::Good)
    );
    assert!(cat.flags(at(&cat, store, "m.txt")).is_empty());
}

#[test]
fn i4_carved_files_overwrite_deleted_files_and_get_new_ids() {
    let mut lab = base_lab(32, 24);
    lab.rec(
        ORIGIN,
        &freed(named(100, "deleted.bin").data_extent(
            0,
            3,
            (4 * 4096, 4 * 4096, 4 * 4096),
            &runlist(&[(Some(3000), 4)]),
            0,
        )),
    );
    lab.rec(ORIGIN, &freed(one_run(named(101, "own.jpg"), 3100, 0)));
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let before = cat.node_count();
    let entries = vec![
        CarvedEntry {
            offset: 3001 * 4096,
            len: 4096,
            ext: "png".into(),
            end_verified: true,
        },
        CarvedEntry {
            offset: 3100 * 4096,
            len: 4096,
            ext: "jpg".into(),
            end_verified: false,
        },
    ];
    let c2 = cat.with_carved(store, &entries).unwrap();
    assert!(c2
        .flags(at(&c2, store, "deleted.bin"))
        .contains(Flag::Overwritten));
    // Carved at the deleted file's own first byte: its own content.
    assert!(!c2
        .flags(at(&c2, store, "own.jpg"))
        .contains(Flag::Overwritten));
    let png = at(&c2, store, "[Carved files]/png/f0000024008.png");
    assert!(png.0 as u64 >= before);
    assert_eq!(c2.rating(png), Some(Rating::Good));
    let jpg = at(&c2, store, "[Carved files]/jpg/f0000024800.jpg");
    assert_eq!(c2.rating(jpg), Some(Rating::Uncertain), "end not verified");
    let main_root = c2.instances()[0].root;
    assert_eq!(c2.instances()[0].carved, Some(NodeId(before as u32)));
    assert_eq!(
        c2.row(store, main_root, false).unwrap().files,
        cat.row(store, main_root, false).unwrap().files + 2
    );
    // A new carving run: new ids, the old ones are gone.
    let c3 = c2.with_carved(store, &entries[..1]).unwrap();
    assert!(!c3.contains(png));
    let png3 = at(&c3, store, "[Carved files]/png/f0000024008.png");
    assert!(png3.0 > jpg.0);
    // Discarding (I5): nothing carved, the tree unchanged.
    let c4 = c3.with_carved(store, &[]).unwrap();
    assert!(!c4.contains(png3));
    assert_eq!(c4.instances()[0].carved, None);
    assert_eq!(
        c4.row(store, main_root, false),
        cat.row(store, main_root, false)
    );
}

#[test]
fn observations_update_flags_and_ratings() {
    let lab = base_lab(32, 24);
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let p = at(&cat, store, "p3.png");
    assert!(cat.flags(p).contains(Flag::SignatureOk));
    cat.observe(
        p,
        hds_recover::FlagSet::of(&[Flag::SignatureMismatch, Flag::BadSectors]),
    );
    let f = cat.flags(p);
    assert!(
        f.contains(Flag::SignatureMismatch)
            && !f.contains(Flag::SignatureOk)
            && f.contains(Flag::BadSectors)
    );
    assert_eq!(cat.rating(p), Some(Rating::Damaged));
    // H1: a refreshed overwritten set.
    let mut set = hds_io::RangeSet::new();
    set.insert(2048 * 4096..2049 * 4096);
    assert_eq!(cat.refresh_overwritten(&set), 1);
    assert!(cat
        .flags(at(&cat, store, "p0.png"))
        .contains(Flag::Overwritten));
    // Folders take no observations.
    cat.observe(
        cat.instances()[0].root,
        hds_recover::FlagSet::of(&[Flag::BadSectors]),
    );
    assert!(cat.flags(cat.instances()[0].root).is_empty());
}

#[test]
fn geometry_set_keeps_the_tree_and_recomputes_health() {
    let mut lab = base_lab(32, 24);
    lab.rec(ORIGIN, &dir(100, 1, ROOT, "d", t(10)));
    lab.rec(
        ORIGIN,
        &file(101, 1, fref(100, 1), "x.bin", t(10), Some((3000, 1))),
    );
    let mut run = lab.run(None);
    let cat = catalog(&run);
    let mut overrides = Overrides::new();
    let cancel = AtomicBool::new(false);
    let (c2, main_changed) = crate::catalog::geometry_set(
        &mut run.session,
        &mut run.layout,
        &mut overrides,
        &mut run.heads,
        &cat,
        0,
        (4096, 4096),
        &cancel,
    )
    .unwrap();
    assert!(main_changed);
    let session = &run.session;
    let store = session.store();
    assert_eq!(c2.node_count(), cat.node_count());
    for i in 0..cat.node_count() as u32 {
        assert_eq!(cat.path(store, NodeId(i)), c2.path(store, NodeId(i)));
    }
    let g = &c2.instances()[0].geometry;
    assert_eq!((g.cluster_size, g.volume_offset), (4096, 4096));
    // One cluster later every PNG head is elsewhere: not validated, and
    // the heads read for the manual geometry contradict the extension.
    assert!(!g.validated);
    let p0 = at(&c2, store, "p0.png");
    assert!(c2.flags(p0).contains(Flag::SignatureMismatch));
    assert_eq!(c2.rating(p0), Some(Rating::Damaged));
    assert_eq!(cat.rating(p0), Some(Rating::Good));
    let (src, _) = c2.file_source(store, at(&c2, store, "d/x.bin").0).unwrap();
    assert_eq!(src.first_offset(), Some(3000 * 4096 + 4096));
    // Stored in the session.
    assert!(!Overrides::load(session).unwrap().is_empty());
}

#[test]
fn geometry_set_to_the_geometry_in_effect_does_not_change_main() {
    // PROTOCOL geometry_set: carved nodes are discarded only when the main
    // instance changes; a pair equal to the one in effect changes nothing.
    let mut lab = base_lab(32, 24);
    lab.rec(ORIGIN, &dir(100, 1, ROOT, "d", t(10)));
    let mut run = lab.run(None);
    let cat = catalog(&run);
    let g = cat.instances()[0].geometry.clone();
    let mut overrides = Overrides::new();
    let cancel = AtomicBool::new(false);
    let (c2, main_changed) = crate::catalog::geometry_set(
        &mut run.session,
        &mut run.layout,
        &mut overrides,
        &mut run.heads,
        &cat,
        0,
        (g.cluster_size, g.volume_offset),
        &cancel,
    )
    .unwrap();
    assert!(
        !main_changed,
        "the same geometry again is no change of main"
    );
    let (_, main_changed) = crate::catalog::geometry_set(
        &mut run.session,
        &mut run.layout,
        &mut overrides,
        &mut run.heads,
        &c2,
        0,
        (g.cluster_size, g.volume_offset + 4096),
        &cancel,
    )
    .unwrap();
    assert!(main_changed);
}

#[test]
fn g2_sweep_is_not_quadratic_in_overlapping_files() {
    // 60 000 files on the same cluster: equal keys (all overlap), then
    // ascending keys (all but the last overwritten).
    let n = 60_000u32;
    let ext: Vec<(u64, u64, u32)> = (0..n).map(|i| (4096, 8192, i)).collect();
    let started = std::time::Instant::now();
    let keys = vec![(4u8, 1u8, 7u64, 7u64); n as usize];
    let mut flags = vec![0u32; n as usize];
    super::overlap_sweep(&ext, &keys, &mut flags);
    assert!(flags.iter().all(|&f| f == Flag::Overlap.bit()));
    let keys: Vec<(u8, u8, u64, u64)> = (0..n).map(|i| (4, 1, u64::from(i), 0)).collect();
    let mut flags = vec![0u32; n as usize];
    super::overlap_sweep(&ext, &keys, &mut flags);
    assert!(flags[..n as usize - 1]
        .iter()
        .all(|&f| f & Flag::Overwritten.bit() != 0));
    assert_eq!(flags[n as usize - 1], Flag::Overlap.bit());
    // A file overlapping only itself (two runs on one cluster) is clean.
    let mut flags = vec![0u32; 1];
    super::overlap_sweep(&[(0, 10, 0), (5, 15, 0)], &[(4, 1, 0, 0)], &mut flags);
    assert_eq!(flags[0], 0);
    assert!(started.elapsed().as_secs() < 20, "{:?}", started.elapsed());
}

#[test]
fn i4_a_carved_file_at_a_later_fragment_of_a_deleted_file_overwrites_it() {
    // A deleted file with two fragments: clusters 3000-3001 and 3500-3501.
    let mut lab = base_lab(32, 24);
    lab.rec(
        ORIGIN,
        &freed(named(100, "frag.bin").data_extent(
            0,
            3,
            (4 * 4096, 4 * 4096, 4 * 4096),
            &runlist(&[(Some(3000), 2), (Some(3500), 2)]),
            0,
        )),
    );
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let carved = |offset: u64, len: u64| CarvedEntry {
        offset,
        len,
        ext: "png".into(),
        end_verified: true,
    };
    assert!(!cat
        .flags(at(&cat, store, "frag.bin"))
        .contains(Flag::Overwritten));
    // At the first byte of the SECOND fragment: another file's content.
    let c2 = cat
        .with_carved(store, &[carved(3500 * 4096, 4096)])
        .unwrap();
    assert!(c2
        .flags(at(&c2, store, "frag.bin"))
        .contains(Flag::Overwritten));
    // At the file's own first byte, also spanning both fragments' bytes
    // in absolute terms: the file's own content.
    let c3 = cat
        .with_carved(store, &[carved(3000 * 4096, 501 * 4096)])
        .unwrap();
    assert!(!c3
        .flags(at(&c3, store, "frag.bin"))
        .contains(Flag::Overwritten));
    // Inside the first fragment but not at its start.
    let c4 = cat
        .with_carved(store, &[carved(3001 * 4096, 4096)])
        .unwrap();
    assert!(c4
        .flags(at(&c4, store, "frag.bin"))
        .contains(Flag::Overwritten));
}

#[test]
fn i4_a_deleted_file_without_its_vcn0_extent_has_no_own_first_byte() {
    // Only the extent at VCN 2 survived (clusters 3500-3501).
    let mut lab = base_lab(32, 24);
    lab.rec(
        ORIGIN,
        &freed(named(100, "tail.bin").data_extent(
            2,
            3,
            (4 * 4096, 4 * 4096, 4 * 4096),
            &runlist(&[(Some(3500), 2)]),
            0,
        )),
    );
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let c2 = cat
        .with_carved(
            store,
            &[CarvedEntry {
                offset: 3500 * 4096,
                len: 4096,
                ext: "png".into(),
                end_verified: true,
            }],
        )
        .unwrap();
    assert!(c2
        .flags(at(&c2, store, "tail.bin"))
        .contains(Flag::Overwritten));
}

/// What `geometry_set` / `instance_set` must leave alone when they fail.
fn layout_state(run: &Run) -> impl PartialEq + std::fmt::Debug {
    (
        run.layout.main(),
        run.layout.instances().to_vec(),
        run.layout.protected().to_vec(),
        Overrides::load(&run.session).unwrap(),
    )
}

#[test]
fn a_failed_geometry_set_changes_nothing() {
    let mut lab = base_lab(32, 24);
    lab.rec(ORIGIN, &file(101, 1, ROOT, "x.bin", t(10), Some((3000, 1))));
    let mut run = lab.run(None);
    let cat = catalog(&run);
    let mut overrides = Overrides::new();
    // A first manual geometry is set and kept.
    let (c1, _) = crate::catalog::geometry_set(
        &mut run.session,
        &mut run.layout,
        &mut overrides,
        &mut run.heads,
        &cat,
        0,
        (4096, 4096),
        &AtomicBool::new(false),
    )
    .unwrap();
    let kept = overrides.clone();
    assert!(!kept.is_empty());
    let before = layout_state(&run);
    // A second one fails: the heads it needs are new and the job is
    // cancelled (or the source is lost) while they are read.
    let r = crate::catalog::geometry_set(
        &mut run.session,
        &mut run.layout,
        &mut overrides,
        &mut run.heads,
        &c1,
        0,
        (4096, 1536),
        &AtomicBool::new(true),
    );
    assert!(
        matches!(
            r,
            Err(InstanceSetError::Analysis(
                crate::instances::AnalysisError::Cancelled
            ))
        ),
        "{r:?}"
    );
    assert_eq!(overrides, kept, "the caller's overrides");
    assert_eq!(layout_state(&run), before, "layout and session");
    let g = run.layout.instances()[0].geometry.clone().unwrap();
    assert_eq!((g.cluster_size, g.volume_offset), (4096, 4096));
    // The same with the source gone instead of a cancel.
    run.heads.attach(Some(lost_source(&lab)));
    let r = crate::catalog::geometry_set(
        &mut run.session,
        &mut run.layout,
        &mut overrides,
        &mut run.heads,
        &c1,
        0,
        (4096, 2560),
        &AtomicBool::new(false),
    );
    assert!(
        matches!(
            r,
            Err(InstanceSetError::Analysis(
                crate::instances::AnalysisError::SourceLost(_)
            ))
        ),
        "{r:?}"
    );
    assert_eq!(overrides, kept);
    assert_eq!(layout_state(&run), before);
}

/// The lab's source, failing every read as a lost device.
fn lost_source(lab: &Lab) -> RangedSource {
    let faulty = FaultySource::new(lab.source());
    faulty.lose_after_reads(0);
    let faulty: Arc<dyn BlockSource> = Arc::new(faulty);
    let size = faulty.size();
    RangedSource::new(faulty, 0..size).unwrap()
}

#[test]
fn instance_set_when_the_previous_state_cannot_be_restored() {
    // A manual geometry is in force, its heads are no longer known and
    // the source is gone: the new kind cannot be applied and the old
    // state cannot be re-applied either. The session and the overrides
    // stay unchanged and the error says the layout must be rebuilt.
    let mut lab = base_lab(32, 30);
    for k in 0..20u32 {
        lab.rec(
            8 * MIB,
            &file(
                300 + k,
                1,
                ROOT,
                &format!("o{k}.txt"),
                t(1),
                Some((5000 + u64::from(k), 1)),
            ),
        );
    }
    let mut run = lab.run(None);
    let cat = catalog(&run);
    let mut overrides = Overrides::new();
    let (c1, _) = crate::catalog::geometry_set(
        &mut run.session,
        &mut run.layout,
        &mut overrides,
        &mut run.heads,
        &cat,
        0,
        (4096, 4096),
        &AtomicBool::new(false),
    )
    .unwrap();
    let kept = overrides.clone();
    let stored = Overrides::load(&run.session).unwrap();
    run.heads = Heads::new();
    run.heads.attach(Some(lost_source(&lab)));
    let older = c1
        .instances()
        .iter()
        .find(|i| i.kind == InstanceKind::Older)
        .unwrap()
        .id;
    let r = crate::catalog::instance_set(
        &mut run.session,
        &mut run.layout,
        &mut overrides,
        &mut run.heads,
        &c1,
        older,
        Some(crate::instances::KindChoice::Main),
        &AtomicBool::new(false),
    );
    assert!(matches!(r, Err(InstanceSetError::Restore(_))), "{r:?}");
    assert_eq!(overrides, kept);
    assert_eq!(Overrides::load(&run.session).unwrap(), stored);
}

#[test]
fn instance_set_and_geometry_set_refuse_the_strays() {
    let mut lab = base_lab(32, 24);
    lab.rec(
        12 * MIB,
        &file(400, 1, ROOT, "stray.bin", t(20), Some((3300, 1))),
    );
    let mut run = lab.run(None);
    let cat = catalog(&run);
    let k = cat
        .instances()
        .iter()
        .position(|i| i.kind == InstanceKind::Strays)
        .expect("strays") as u32;
    let before = layout_state(&run);
    let mut overrides = Overrides::new();
    let r = crate::catalog::geometry_set(
        &mut run.session,
        &mut run.layout,
        &mut overrides,
        &mut run.heads,
        &cat,
        k,
        (4096, 8192),
        &AtomicBool::new(false),
    );
    assert!(matches!(r, Err(InstanceSetError::Strays)), "{r:?}");
    let r = crate::catalog::instance_set(
        &mut run.session,
        &mut run.layout,
        &mut overrides,
        &mut run.heads,
        &cat,
        k,
        Some(crate::instances::KindChoice::Other),
        &AtomicBool::new(false),
    );
    assert!(matches!(r, Err(InstanceSetError::Strays)), "{r:?}");
    assert!(overrides.is_empty());
    assert_eq!(layout_state(&run), before);
}

#[test]
fn unnumbered_records_show_their_positional_number() {
    // A record with a pre-3.1 header (no number) in the slot of number
    // 100: D1 gives it number 100.
    let mut lab = base_lab(32, 24);
    lab.rec_at(
        off(100),
        &file(100, 1, ROOT, "old.txt", t(10), Some((3000, 1))).unnumbered(),
    );
    let run = lab.run(None);
    let store = run.store();
    assert_eq!(run.layout.number(run.id(off(100))), Some(100));
    let cat = catalog(&run);
    let n = at(&cat, store, "old.txt");
    let info = cat.node_info(store, n.wire()).unwrap();
    assert_eq!(info.record.unwrap().number, 100);
    let plan = cat
        .plan(
            store,
            &crate::query::Selection {
                include: vec![n.wire()],
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(plan.files[0].record_number, Some(100));
    // A numbered record keeps its header's number.
    let p0 = at(&cat, store, "p0.png");
    let info = cat.node_info(store, p0.wire()).unwrap();
    let rec = cat.record_of(p0).unwrap();
    assert_eq!(
        info.record.unwrap().number,
        u64::from(store.record(rec).unwrap().record_number().unwrap())
    );
}
