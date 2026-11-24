//! Unit tests of the carving types: kinds, geometry, names, plans, the
//! fold of runs and the `CARVED` codec. The job itself is tested end to end
//! in `tests/carve.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use hds_carve::{Confidence, FileKind, Measured};
use hds_io::RangeSet;
use hds_recover::{FileSource, Flag, FlagSet, Rating, Run};

use super::set::encode_for_tests;
use super::*;

fn geometry() -> CarveGeometry {
    CarveGeometry::new(4096, 1 << 20).unwrap()
}

fn file(offset: u64, len: u64, kind: FileKind, ext: &str, verified: bool) -> CarvedFile {
    CarvedFile::new(
        offset,
        len,
        kind,
        ExtName::new(ext).unwrap(),
        verified,
        true,
    )
    .unwrap()
}

fn run(kinds: &[FileKind], files: Vec<CarvedFile>) -> CarveRun {
    CarveRun {
        kinds: kinds.iter().copied().collect(),
        geometry: geometry(),
        files,
    }
}

#[test]
fn kind_sets() {
    let all = KindSet::all();
    assert_eq!(all.iter().count(), FileKind::all().len());
    assert_eq!(KindSet::from_bits(all.bits()), Some(all));
    // bit 0 and bits above 17 name no kind
    assert_eq!(KindSet::from_bits(1), None);
    assert_eq!(KindSet::from_bits(1 << 18), None);
    let some: KindSet = [FileKind::Zip, FileKind::Jpeg].into_iter().collect();
    assert!(some.contains(FileKind::Jpeg) && some.contains(FileKind::Zip));
    assert!(!some.contains(FileKind::Png));
    assert_eq!(
        some.iter().collect::<Vec<_>>(),
        [FileKind::Jpeg, FileKind::Zip]
    );
    assert!(KindSet::EMPTY.is_empty() && !some.is_empty());
}

#[test]
fn geometry_grid() {
    assert!(CarveGeometry::new(4096, 1000).is_none());
    assert!(CarveGeometry::new(3000, 0).is_none());
    assert!(CarveGeometry::new(256, 0).is_none());
    assert!(CarveGeometry::new(4 << 20, 0).is_none());
    assert!(CarveGeometry::new(2 << 20, 0).is_some());
    let g = CarveGeometry::new(4096, 32256).unwrap(); // a partition at sector 63
    assert!(g.on_grid(32256) && g.on_grid(32256 + 4096 * 7));
    assert!(!g.on_grid(32256 + 512) && !g.on_grid(36864));
    // the grid continues in front of cluster 0
    assert!(g.on_grid(32256 - 4096) && !g.on_grid(0));
    assert_eq!(g.lcn(32256 + 4096 * 9), Some(9));
    assert_eq!(g.lcn(32256 + 4096 * 9 + 100), Some(9));
    assert_eq!(g.lcn(512), None);
    assert!(g.on_grid(u64::MAX - (u64::MAX - 32256) % 4096));
}

#[test]
fn extension_names() {
    assert_eq!(ExtName::new("jpg").unwrap().as_str(), "jpg");
    assert_eq!(ExtName::new("12345678").unwrap().as_str(), "12345678");
    for bad in ["", "JPG", "j.g", "123456789", "jpé", "a b"] {
        assert!(ExtName::new(bad).is_none(), "{bad:?}");
    }
}

#[test]
#[allow(clippy::single_range_in_vec_init)]
fn i4_a_carved_file_inside_a_deleted_file() {
    const C: u64 = 4096;
    let mut set = CarvedSet::new();
    set.apply(&CarveRun {
        kinds: KindSet::all(),
        geometry: geometry(),
        files: vec![
            file(10 * C, 3 * C, FileKind::Jpeg, "jpg", true),
            file(20 * C, C + 1, FileKind::Png, "png", true),
            file(40 * C, 100, FileKind::Gif, "gif", true),
        ],
    });
    let own = |at: u64| Some(at);
    // nothing carved there, or carved bytes that only touch or cross the
    // extents, do not count
    assert!(!set.overwrites_deleted(&[0..C, 5 * C..9 * C], own(0), 5 * C));
    assert!(!set.overwrites_deleted(&[11 * C..20 * C], own(11 * C), 9 * C));
    assert!(!set.overwrites_deleted(&[20 * C..21 * C], own(20 * C), C));
    assert!(!set.overwrites_deleted(&[], None, 0));
    // the file's own content: at its first byte, no longer than it
    assert!(!set.overwrites_deleted(&[10 * C..13 * C], own(10 * C), 3 * C));
    assert!(!set.overwrites_deleted(&[10 * C..14 * C], own(10 * C), 3 * C + 5));
    // inside, and not its own: written over it
    assert!(set.overwrites_deleted(&[9 * C..14 * C], own(9 * C), 5 * C));
    assert!(set.overwrites_deleted(&[10 * C..14 * C], own(10 * C), 3 * C - 1));
    assert!(set.overwrites_deleted(&[10 * C..14 * C], None, 4 * C));
    assert!(set.overwrites_deleted(&[39 * C..41 * C], own(39 * C), 2 * C));
    // extents in any order, overlapping or touching, are joined: the PNG
    // spans two clusters held by two runs
    assert!(set.overwrites_deleted(&[21 * C..22 * C, 0..C, 20 * C..21 * C], own(0), 3 * C));
    assert!(set.overwrites_deleted(&[20 * C..21 * C + 1, 20 * C..20 * C + 5], own(0), 3 * C));
    assert!(!set.overwrites_deleted(&[21 * C..22 * C, 0..C, 20 * C..21 * C - 1], own(0), 3 * C));
    // empty ranges are ignored
    assert!(!set.overwrites_deleted(&[50 * C..50 * C, 40 * C..40 * C], own(0), 3 * C));
}

#[test]
fn a_carved_file_as_node_and_plan() {
    let measured = Measured {
        len: 70_000,
        confidence: Confidence::Exact,
        ext: "docx",
        end_verified: true,
    };
    let f = CarvedFile::from_measured(123 * 512, FileKind::Zip, &measured).unwrap();
    assert_eq!(f.name(), "f0000000123.docx");
    assert_eq!(f.path(), "[Carved files]/docx/f0000000123.docx");
    assert_eq!(f.folder_name(), "docx");
    assert_eq!(
        (f.kind(), f.ext(), f.len(), f.end()),
        (FileKind::Zip, "docx", 70_000, 123 * 512 + 70_000)
    );
    // more than ten digits when the disk is that large
    let far = file(20_000_000_000_000, 10, FileKind::Png, "png", true);
    assert_eq!(far.name(), "f39062500000.png");
    assert!(CarvedFile::new(
        5,
        0,
        FileKind::Png,
        ExtName::new("png").unwrap(),
        true,
        true
    )
    .is_none());
    assert!(CarvedFile::new(
        u64::MAX,
        1,
        FileKind::Png,
        ExtName::new("png").unwrap(),
        true,
        true
    )
    .is_none());
    assert_eq!(
        far.first_lcn(geometry()),
        Some((20_000_000_000_000 - (1 << 20)) / 4096)
    );

    // rating: good only with a verified end, a validated main geometry
    // (F6) and no damage
    let none = RangeSet::new();
    assert_eq!(f.rating(f.health_flags(&none, &none), true), Rating::Good);
    assert_eq!(f.rating(FlagSet::EMPTY, false), Rating::Uncertain);
    assert!(f.rating_context(false).geometry_not_validated);
    assert!(!f.rating_context(true).geometry_not_validated);
    let unverified = file(1 << 20, 4096, FileKind::Bmp, "bmp", false);
    assert!(unverified.rating_context(true).carved_end_unverified);
    assert!(!unverified.rating_context(true).geometry_not_validated);
    assert_eq!(unverified.rating(FlagSet::EMPTY, true), Rating::Uncertain);
    let over: RangeSet = std::iter::once(f.end() - 1..f.end() + 5).collect();
    let flags = f.health_flags(&over, &none);
    assert_eq!(flags, FlagSet::of(&[Flag::Overwritten]));
    assert_eq!(f.rating(flags, true), Rating::Damaged);
    let touching: RangeSet = std::iter::once(f.end()..f.end() + 5).collect();
    assert!(f.health_flags(&touching, &touching).is_empty());
    assert_eq!(
        f.health_flags(&none, &over),
        FlagSet::of(&[Flag::BadSectors])
    );

    // plan: one run, all initialized, no record, no times
    let p = f.plan_file(77, Some(9), FlagSet::EMPTY, true);
    assert!(
        f.plan_file(77, Some(9), FlagSet::EMPTY, false)
            .context
            .geometry_not_validated
    );
    assert_eq!(p.node, 77);
    assert_eq!(p.folder, Some(9));
    assert_eq!(String::from_utf16(&p.name).unwrap(), "f0000000123.docx");
    assert_eq!(p.ext, "docx");
    assert_eq!(
        p.source,
        FileSource::Extents {
            runs: vec![Run::Data {
                offset: 123 * 512,
                len: 70_000
            }],
            real_size: 70_000,
            initialized_size: 70_000
        }
    );
    assert_eq!(p.size, 70_000);
    assert_eq!(p.source.first_offset(), Some(123 * 512));
    assert_eq!(p.first_data_offset, Some(123 * 512));
    assert_eq!((p.record_number, p.record_offset), (None, None));
    assert!(p.in_use && !p.context.carved_end_unverified && !p.context.geometry_not_validated);
    assert_eq!(p.original_path, "[Carved files]/docx/f0000000123.docx");
    let root = carved_files_folder(1, None);
    assert_eq!(String::from_utf16(&root.name).unwrap(), "[Carved files]");
    let sub = ext_folder(9, 1, "docx");
    assert_eq!(
        (sub.parent, String::from_utf16(&sub.name).unwrap()),
        (Some(1), "docx".to_string())
    );
}

#[test]
fn runs_replace_their_kinds_only() {
    let jpg = |o| file(o, 1000, FileKind::Jpeg, "jpg", true);
    let zip = |o| file(o, 5000, FileKind::Zip, "zip", true);
    let mut set = CarvedSet::new();
    set.apply(&run(&[FileKind::Jpeg], vec![jpg(1 << 20), jpg(3 << 20)]));
    set.apply(&run(&[FileKind::Zip], vec![zip(2 << 20)]));
    let offsets = |s: &CarvedSet| {
        s.files()
            .iter()
            .map(|f| f.offset() >> 20)
            .collect::<Vec<_>>()
    };
    assert_eq!(offsets(&set), [1, 2, 3]);
    // a new JPEG run replaces both JPEGs, the ZIP stays
    set.apply(&run(&[FileKind::Jpeg], vec![jpg(5 << 20)]));
    assert_eq!(offsets(&set), [2, 5]);
    // a run that found nothing (or was cancelled at once) still replaces
    set.apply(&run(&[FileKind::Zip, FileKind::Png], vec![]));
    assert_eq!(offsets(&set), [5]);
    assert_eq!(set.bytes(), 1000);
    assert_eq!(set.at(5 << 20).unwrap().kind(), FileKind::Jpeg);
    assert!(set.at(4 << 20).is_none());
    // another geometry discards everything first
    let other = CarveRun {
        kinds: [FileKind::Png].into_iter().collect(),
        geometry: CarveGeometry::new(512, 0).unwrap(),
        files: vec![file(7 << 20, 10, FileKind::Png, "png", true)],
    };
    set.apply(&other);
    assert_eq!(offsets(&set), [7]);
    assert_eq!(set.geometry(), Some(other.geometry));
    // covered bytes, for I4
    let covered = set.covered();
    assert!(covered.contains(7 << 20) && !covered.contains((7 << 20) + 10));
}

fn session_range() -> std::ops::Range<u64> {
    0..1 << 40
}

#[test]
fn the_block_folds_like_apply() {
    // a sequence of runs, stored one payload each, folds into the set that
    // applying them in memory gives
    let jpg = |o| file(o, 1000, FileKind::Jpeg, "jpg", true);
    let runs = vec![
        run(
            &[FileKind::Jpeg, FileKind::Zip],
            vec![jpg(1 << 20), file(2 << 20, 9, FileKind::Zip, "xlsx", false)],
        ),
        run(
            &[FileKind::Png],
            vec![file(4 << 20, 9, FileKind::Png, "png", true)],
        ),
        run(&[FileKind::Jpeg], vec![jpg(6 << 20)]),
        run(&[FileKind::Gif], vec![]),
    ];
    let mut set = CarvedSet::new();
    let mut payloads = Vec::new();
    for r in &runs {
        set.apply(r);
        payloads.push(encode_for_tests(r.kinds, r.geometry, &r.files));
        let loaded = CarvedSet::from_payloads(&payloads, session_range()).unwrap();
        assert_eq!(loaded, set);
    }
    assert_eq!(set.len(), 3);
    // compaction gives the same set from one payload
    let compact = encode_for_tests(KindSet::all(), geometry(), set.files());
    assert_eq!(
        CarvedSet::from_payloads(std::slice::from_ref(&compact), session_range()).unwrap(),
        set
    );
    // a geometry change in the middle drops what came before
    let other = CarveGeometry::new(8192, 0).unwrap();
    payloads.push(encode_for_tests(
        KindSet::EMPTY,
        other,
        &[file(9 << 20, 5, FileKind::Gif, "gif", true)],
    ));
    let loaded = CarvedSet::from_payloads(&payloads, session_range()).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded.geometry(), Some(other));
    // a continuation payload (no kinds) only adds
    let cont = vec![
        encode_for_tests(KindSet::all(), geometry(), &[jpg(1 << 20)]),
        encode_for_tests(KindSet::EMPTY, geometry(), &[jpg(2 << 20)]),
        encode_for_tests([FileKind::Png].into_iter().collect(), geometry(), &[]),
    ];
    assert_eq!(
        CarvedSet::from_payloads(&cont, session_range())
            .unwrap()
            .len(),
        2
    );
    // empty
    assert!(CarvedSet::from_payloads(&[], session_range())
        .unwrap()
        .is_empty());
}

#[test]
fn hostile_payloads_are_refused() {
    let good = encode_for_tests(
        KindSet::all(),
        geometry(),
        &[
            file(1 << 20, 100, FileKind::Jpeg, "jpg", true),
            file(2 << 20, 100, FileKind::Mp4, "mov", false),
        ],
    );
    assert_eq!(
        CarvedSet::from_payloads(std::slice::from_ref(&good), session_range())
            .unwrap()
            .len(),
        2
    );
    let refused =
        |p: Vec<u8>| CarvedSet::from_payloads(&[good.clone(), p], session_range()).unwrap_err();
    // cut anywhere
    for cut in 0..good.len() {
        assert_eq!(refused(good[..cut].to_vec()).payload, 1, "cut {cut}");
    }
    // a count that claims more than is there: refused without allocating
    let mut p = good.clone();
    p[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
    refused(p);
    let mut p = good.clone();
    p[24..32].copy_from_slice(&1u64.to_le_bytes());
    refused(p);
    // magic, kinds, geometry
    let mut p = good.clone();
    p[0] = b'X';
    refused(p);
    let mut p = good.clone();
    p[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    refused(p);
    let mut p = good.clone();
    p[8..16].copy_from_slice(&3000u64.to_le_bytes());
    refused(p);
    // files: out of order, duplicate offsets, outside the range, empty,
    // overflowing, unknown kind, flags, extension
    let at = 32;
    let mut p = good.clone();
    p[at..at + 8].copy_from_slice(&(3u64 << 20).to_le_bytes());
    assert_eq!(refused(p).reason, "files out of order");
    let mut p = good.clone();
    p[at..at + 8].copy_from_slice(&(2u64 << 20).to_le_bytes());
    refused(p);
    let mut p = good.clone();
    p[at + 8..at + 16].copy_from_slice(&(1u64 << 41).to_le_bytes());
    assert_eq!(refused(p).reason, "file outside the session range");
    let mut p = good.clone();
    p[at + 8..at + 16].copy_from_slice(&0u64.to_le_bytes());
    refused(p);
    let mut p = good.clone();
    p[at + 8..at + 16].copy_from_slice(&u64::MAX.to_le_bytes());
    refused(p);
    for (byte, value) in [
        (16, 0u8),
        (16, 18),
        (17, 4),
        (18, 0),
        (18, 9),
        (19, b'A'),
        (22, b'x'),
        (27, 1),
    ] {
        let mut p = good.clone();
        p[at + byte] = value;
        refused(p);
    }
    // every single-byte change either decodes or is refused: never a panic
    for i in 0..good.len() {
        for v in [0u8, 1, 0x7F, 0x80, 0xFF] {
            let mut p = good.clone();
            p[i] = v;
            let _ = CarvedSet::from_payloads(&[p], session_range());
        }
    }
}

#[test]
fn files_out_of_order_in_a_run_keep_the_set_sorted() {
    let mut set = CarvedSet::new();
    set.apply(&run(
        &[FileKind::Png],
        vec![
            file(3 << 20, 1, FileKind::Png, "png", true),
            file(1 << 20, 1, FileKind::Png, "png", true),
            file(3 << 20, 1, FileKind::Png, "png", true),
        ],
    ));
    let offsets: Vec<u64> = set.files().iter().map(|f| f.offset() >> 20).collect();
    assert_eq!(offsets, [1, 3]);
}

// ------------------------------------------------- next to MFT records --

mod next_to_mft_records {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use hds_carve::FileKind;
    use hds_io::testing::MemSource;
    use hds_io::{BlockSource, RangeSet, RangedSource};

    use super::super::*;
    use crate::scan::{scan, NoObserver, ScanOptions, ScanStatus};
    use crate::session::{Session, SessionMeta, SourceIdentity};
    use crate::store::DataExtent;
    use crate::test_build::{fref, runlist, Rec};

    fn sample(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../hds-carve/tests/data")
            .join(name);
        std::fs::read(path).unwrap()
    }

    const C: u64 = 4096;

    fn file_record(number: u32, in_use: bool, name: &str, lcn: u64, len: u64) -> Vec<u8> {
        let clusters = len.div_ceil(C);
        Rec::new(number)
            .flags(u16::from(in_use))
            .si([1, 2, 3, 4], 0)
            .name(fref(5, 5), name, 1, 0)
            .data_extent(
                0,
                i64::try_from(clusters).unwrap() - 1,
                (clusters * C, len, len),
                &runlist(&[(Some(lcn), clusters)]),
                0,
            )
            .build()
    }

    /// The protected files of I2 (a), as the catalog derives them: the
    /// clusters of every in-use record's runs, under the geometry.
    fn protected_from_store(session: &Session, g: CarveGeometry) -> RangeSet {
        let store = session.store();
        let mut set = RangeSet::new();
        for (id, record) in store.records() {
            if !record.is_in_use() {
                continue;
            }
            for extent in store.data_extents(id) {
                if let DataExtent::NonResident(e) = extent {
                    for run in e.runs() {
                        if let Some(lcn) = run.lcn {
                            let start = g.volume_offset() + lcn * g.cluster_size();
                            set.insert(start..start + run.length * g.cluster_size());
                        }
                    }
                }
            }
        }
        set
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn files_next_to_old_mft_records() {
        let g = CarveGeometry::new(C, 0).unwrap();
        let png = sample("png_rgb.png");
        let jpg = sample("jpeg_baseline.jpg");
        let mp3 = sample("mp3_lame_cbr_info_first.mp3");
        let gif = sample("gif_static.gif");
        let mut disk = vec![0_u8; 4 << 20];
        let mut put = |at: u64, bytes: &[u8]| {
            disk[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
        };
        // A JPEG that ends in the cluster right in front of the old MFT
        put(2 * C, &jpg);
        // the old MFT at cluster 4: an in-use PNG (protected), a deleted
        // GIF (its clusters are free for carving) and filler records
        let mut mft = Vec::new();
        for n in 16..48_u32 {
            mft.extend(match n {
                30 => file_record(n, true, "kept.png", 100, png.len() as u64),
                31 => file_record(n, false, "gone.gif", 200, gif.len() as u64),
                _ => Rec::new(n)
                    .si([1, 2, 3, 4], 0)
                    .name(fref(5, 5), &format!("f{n}"), 1, 0)
                    .build(),
            });
        }
        put(4 * C, &mft);
        put(100 * C, &png);
        put(200 * C, &gif);
        // an MP3 (complete after any frame) with old MFT records right
        // behind it, in its last cluster
        let mp3_at = 300 * C;
        put(mp3_at, &mp3);
        let behind = mp3_at + mp3.len() as u64;
        let behind = behind.div_ceil(1024) * 1024;
        put(behind, &mft[..4096]);

        let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(disk));
        let range = 0..source.size();
        let options = ScanOptions::default();
        let meta =
            SessionMeta::new(SourceIdentity::from(&source.info()), range.clone(), options).unwrap();
        let mut session = Session::new(meta);
        let ranged = RangedSource::new(source.clone(), range.clone()).unwrap();
        let cancel = AtomicBool::new(false);
        let summary = scan(&ranged, &options, &mut session, &mut NoObserver, &cancel);
        assert_eq!(summary.status, ScanStatus::Completed);
        assert!(session.store().record_count() >= 32);

        let protected = protected_from_store(&session, g);
        assert!(protected.contains(100 * C) && !protected.contains(200 * C));
        let input = CarveInput {
            kinds: KindSet::all(),
            geometry: g,
            hits: session.store().carve_hits_sorted(),
            done: session.store().done().clone(),
            protected_files: protected,
            overwritten: RangeSet::new(),
            live_allocated: RangeSet::new(),
        };
        let (run, summary) = Carver::new(input, range).run(&ranged, &mut NoCarveObserver, &cancel);
        assert_eq!(summary.status, CarveStatus::Completed);
        let found: Vec<(u64, u64, FileKind)> = run
            .files
            .iter()
            .map(|f| (f.offset() / C, f.len(), f.kind()))
            .collect();
        assert_eq!(
            found,
            [
                (2, jpg.len() as u64, FileKind::Jpeg),
                (200, gif.len() as u64, FileKind::Gif),
                (300, mp3.len() as u64, FileKind::Mp3),
            ]
        );
        // the MP3 is complete after any frame: its end is not verified
        assert!(!run.files[2].end_verified() && run.files[0].end_verified());
        // I4: the carved GIF is the deleted file's own content (at its
        // first byte, no longer than it): the deleted record keeps its data
        let mut set = CarvedSet::new();
        set.apply(&run);
        let gone = [200 * C..200 * C + (gif.len() as u64).div_ceil(C) * C];
        assert!(set.covered().intersects(gone[0].clone()));
        assert!(!set.overwrites_deleted(&gone, Some(200 * C), gif.len() as u64));
        // had the deleted file been something else (a larger file of
        // which the GIF now holds the first clusters), the GIF was written
        // over it
        let larger = [199 * C..203 * C];
        assert!(set.overwrites_deleted(&larger, Some(199 * C), 4 * C));
        assert!(set.overwrites_deleted(&larger, None, 4 * C));
        // a file at the same place that was shorter than the GIF
        assert!(set.overwrites_deleted(&gone, Some(200 * C), gif.len() as u64 - 1));
        assert!(!set.overwrites_deleted(&[100 * C..101 * C], Some(100 * C), C));
    }
}
