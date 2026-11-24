//! The carving job (ALGORITHMS I) end to end: real files written by real
//! encoders (the samples of `hds-carve`) laid into disk images, scanned by
//! the real scanner, carved, stored in and loaded from session files, and
//! recovered through `hds-recover` with SHA-256 comparison.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

#[path = "carve_common.rs"]
mod common;

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use hds_carve::FileKind;
use hds_core::carve::{
    discard_carved, store_run, CarveFailure, CarveGeometry, CarveInput, CarveObserver,
    CarveProgress, CarveStatus, CarvedFile, CarvedSet, Carver, KindSet, NoCarveObserver,
};
use hds_core::scan::{scan_with, NoObserver, ScanOptions, ScanTuning, WARN_CARVE_HITS_CAPPED};
use hds_core::session::{Session, SessionGuard, SessionMeta, SourceIdentity};
use hds_core::store::CarveHit;
use hds_io::testing::FaultySource;
use hds_io::{
    BlockSource, Protected, RangeSet, RangedSource, SourceInfo, SourceKind, SourceLocation,
};
use hds_recover::{FileStatus, Flag, FlagSet};

const KIB: u64 = 1024;
const MIB: u64 = 1 << 20;

fn geometry(cluster: u64, offset: u64) -> CarveGeometry {
    CarveGeometry::new(cluster, offset).unwrap()
}

fn none() -> RangeSet {
    RangeSet::new()
}

/// Lays `samples` one after the other onto the cluster grid of `g`,
/// starting at cluster `first`: each at the next cluster boundary behind
/// the previous one, with one empty cluster in between every third time.
fn lay_out(
    disk: &mut Disk,
    g: CarveGeometry,
    first: u64,
    samples: &[Sample],
) -> Vec<(u64, Sample)> {
    let c = g.cluster_size();
    let mut at = g.volume_offset() + first * c;
    let mut placed = Vec::new();
    for (i, s) in samples.iter().enumerate() {
        disk.put(at, &s.data);
        placed.push((at, s.clone()));
        at += (s.data.len() as u64).div_ceil(c) * c;
        if i % 3 == 2 {
            at += c;
        }
    }
    placed
}

/// What the carved file of a placed sample must be.
fn expected(at: u64, s: &Sample) -> CarvedFile {
    CarvedFile::from_measured(at, s.kind, &alone(s)).unwrap()
}

#[test]
fn every_kind_on_the_cluster_grid_is_carved_and_recovered_bit_identical() {
    let all = samples();
    assert!(all.len() >= 120, "{} samples", all.len());
    // a volume at 1 MiB, and one at sector 63 (its grid is not 4 KiB
    // aligned on the disk), and one with 512-byte clusters
    for g in [
        geometry(4096, MIB),
        geometry(4096, 63 * 512),
        geometry(512, 0),
    ] {
        let mut disk = Disk::new(8 * MIB as usize);
        let placed = lay_out(&mut disk, g, 2, &all);
        let source = disk.source();
        let range = 0..source.size();
        let session = scan_source(&source, range.clone());
        let (run, summary) = carve(
            &source,
            &session,
            input(&session, KindSet::all(), g, none(), none(), none()),
        );
        assert_eq!(summary.status, CarveStatus::Completed);
        let want: Vec<CarvedFile> = placed.iter().map(|(at, s)| expected(*at, s)).collect();
        assert_eq!(run.files, want, "{g:?}");
        assert_eq!(summary.found, want.len() as u64);
        assert_eq!(summary.bytes_found, run.bytes());
        assert_eq!(summary.candidates_done, summary.candidates_total);
        assert_eq!(run.geometry, g);

        // recovered through hds-recover, as the catalog's plan would
        let (rsum, recovered) = recover_files(&source, range, &run.files, &|f| {
            f.health_flags(&none(), session.store().bad_ranges())
        });
        assert_eq!(rsum.files_total, want.len() as u64);
        let by_offset: BTreeMap<u64, &Sample> = placed.iter().map(|(a, s)| (*a, s)).collect();
        for r in &recovered {
            let s = by_offset[&r.file.offset()];
            let content = r.content.as_ref().unwrap_or_else(|| panic!("{}", s.name));
            assert_eq!(sha256_hex(content), sha256_hex(&s.data), "{}", s.name);
            assert!(r.outcome.output_path.ends_with(&format!(
                "{}.{}",
                r.file.sector_name(),
                r.file.ext()
            )));
            // `good` - and `ok` when the extension has an opinion - exactly
            // when the end was verified
            match &r.outcome.status {
                FileStatus::Ok | FileStatus::OkUnverified => {
                    assert!(r.file.end_verified(), "{}", s.name)
                }
                FileStatus::Warning { flags, context } => {
                    assert!(!r.file.end_verified(), "{}", s.name);
                    assert!(context.carved_end_unverified);
                    assert!(!flags.contains(Flag::SignatureMismatch), "{}", s.name);
                }
                other => panic!("{}: {other}", s.name),
            }
        }
    }
}

/// `f0000001234` of a carved file.
trait SectorName {
    fn sector_name(&self) -> String;
}

impl SectorName for CarvedFile {
    fn sector_name(&self) -> String {
        format!("f{:010}", self.sector())
    }
}

#[test]
fn what_carving_leaves_alone() {
    let g = geometry(4096, MIB);
    let c = g.cluster_size();
    let png = sample("png_rgb.png"); // 36 KiB
    let jpg = sample("jpeg_baseline.jpg");
    let gif = sample("gif_static.gif");
    let mut disk = Disk::new(16 * MIB as usize);
    let slot = |n: u64| g.volume_offset() + n * 64 * KIB;
    // A: the control, carved
    disk.put(slot(1), &png.data);
    // B: on a sector, not on the cluster grid: a hit, not a candidate
    disk.put(slot(2) + 512, &png.data);
    // C: not even on a sector: no hit
    disk.put(slot(3) + 100, &jpg.data);
    // D: inside a protected file (an in-use file of main)
    disk.put(slot(4), &gif.data);
    // E: a protected file begins inside it: it cannot be measured within
    // its bound, and is not cut short to fit
    disk.put(slot(5), &png.data);
    // F: inside the overwritten set, G: inside a live-allocated range
    disk.put(slot(6), &jpg.data);
    disk.put(slot(7), &jpg.data);
    // H: runs into a live-allocated range
    disk.put(slot(8), &png.data);
    // I: in a part of the range the scan did not reach
    disk.put(slot(9), &gif.data);
    // J: a kind that is not asked for
    disk.put(slot(10), &jpg.data);
    // K: overlaps the extents of a post-format record behind its start:
    // carved, but damaged
    disk.put(slot(11), &png.data);

    let source = disk.source();
    let range = 0..source.size();
    let session = scan_source(&source, range.clone());
    let hits: Vec<u64> = session
        .store()
        .carve_hits()
        .map(|h| h.disk_offset())
        .collect();
    assert!(hits.contains(&(slot(2) + 512)));
    assert!(!hits.iter().any(|&h| (slot(3)..slot(3) + 4096).contains(&h)));

    let protected = ranges(&[slot(4)..slot(4) + c, slot(5) + 2 * c..slot(5) + 3 * c]);
    let live = ranges(&[slot(7)..slot(7) + 2 * c, slot(8) + 4 * c..slot(8) + 5 * c]);
    let mut overwritten = live.clone();
    overwritten.insert(slot(6)..slot(6) + c);
    overwritten.insert(slot(11) + 2 * c..slot(11) + 3 * c);
    let kinds: KindSet = [FileKind::Png, FileKind::Gif].into_iter().collect();
    let mut inp = input(&session, kinds, g, protected, overwritten.clone(), live);
    inp.done.remove(slot(9)..slot(9) + c);
    let carver = Carver::new(inp, range.clone());
    let candidates: Vec<u64> = carver
        .candidate_hits()
        .iter()
        .map(|h| h.disk_offset())
        .collect();
    assert_eq!(candidates, [slot(1), slot(5), slot(8), slot(11)]);
    let ranged = RangedSource::new(source.clone(), range.clone()).unwrap();
    let (run, summary) = carver.run(&ranged, &mut NoCarveObserver, &AtomicBool::new(false));
    let offsets: Vec<u64> = run.files.iter().map(CarvedFile::offset).collect();
    assert_eq!(offsets, [slot(1), slot(11)]);
    assert_eq!(summary.not_measurable, 2);
    for f in &run.files {
        assert_eq!(f.len(), png.data.len() as u64);
    }
    // K is damaged: rated so, and still written
    let k = run.files[1];
    let flags = k.health_flags(&overwritten, &none());
    assert_eq!(flags, FlagSet::of(&[Flag::Overwritten]));
    let (_, recovered) = recover_files(&source, range, &run.files, &|f| {
        f.health_flags(&overwritten, &none())
    });
    assert_eq!(recovered.len(), 2);
    for r in &recovered {
        assert_eq!(
            sha256_hex(r.content.as_ref().unwrap()),
            sha256_hex(&png.data)
        );
    }
    let damaged = recovered.iter().find(|r| r.file == k).unwrap();
    assert!(matches!(damaged.outcome.status, FileStatus::Warning { .. }));
    assert_eq!(damaged.outcome.rating, hds_recover::Rating::Damaged);
}

/// A stored ZIP whose second entry's data begins `align` bytes into the
/// archive, so that a file stored there sits on the cluster grid.
fn stored_zip(inner: &[u8], align: usize) -> Vec<u8> {
    fn local(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut h = b"PK\x03\x04".to_vec();
        h.extend_from_slice(&10u16.to_le_bytes()); // version
        h.extend_from_slice(&[0; 2]); // flags
        h.extend_from_slice(&[0; 2]); // stored
        h.extend_from_slice(&[0; 4]); // time, date
        h.extend_from_slice(&crc32(data).to_le_bytes());
        h.extend_from_slice(&(data.len() as u32).to_le_bytes());
        h.extend_from_slice(&(data.len() as u32).to_le_bytes());
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&[0; 2]);
        h.extend_from_slice(name);
        h.extend_from_slice(data);
        h
    }
    fn central(name: &[u8], data: &[u8], offset: usize) -> Vec<u8> {
        let mut h = b"PK\x01\x02".to_vec();
        h.extend_from_slice(&20u16.to_le_bytes());
        h.extend_from_slice(&10u16.to_le_bytes());
        h.extend_from_slice(&[0; 2]);
        h.extend_from_slice(&[0; 2]);
        h.extend_from_slice(&[0; 4]);
        h.extend_from_slice(&crc32(data).to_le_bytes());
        h.extend_from_slice(&(data.len() as u32).to_le_bytes());
        h.extend_from_slice(&(data.len() as u32).to_le_bytes());
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&[0; 2 + 2 + 2 + 2 + 4]);
        h.extend_from_slice(&(offset as u32).to_le_bytes());
        h.extend_from_slice(name);
        h
    }
    let first_name = b"pad.bin";
    let second_name = b"photo.jpg";
    // 30 + name + pad + 30 + name = align
    let pad = vec![0x55u8; align - 60 - first_name.len() - second_name.len()];
    let mut z = local(first_name, &pad);
    let second_at = z.len();
    z.extend(local(second_name, inner));
    assert_eq!(second_at + 30 + second_name.len(), align);
    let cd_at = z.len();
    z.extend(central(first_name, &pad, 0));
    z.extend(central(second_name, inner, second_at));
    let cd_len = z.len() - cd_at;
    z.extend_from_slice(b"PK\x05\x06");
    z.extend_from_slice(&[0; 4]);
    z.extend_from_slice(&2u16.to_le_bytes());
    z.extend_from_slice(&2u16.to_le_bytes());
    z.extend_from_slice(&(cd_len as u32).to_le_bytes());
    z.extend_from_slice(&(cd_at as u32).to_le_bytes());
    z.extend_from_slice(&[0; 2]);
    z
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[test]
fn a_file_inside_a_carved_file_is_not_carved_again_in_the_same_run() {
    let g = geometry(4096, 0);
    let jpg = sample("jpeg_exif_thumbnail.jpg");
    let zip = stored_zip(&jpg.data, 8192);
    let mut disk = Disk::new(4 * MIB as usize);
    disk.put(MIB, &zip);
    let source = disk.source();
    let session = scan_source(&source, 0..source.size());
    let hits: Vec<(u64, FileKind)> = session
        .store()
        .carve_hits()
        .map(|h| (h.disk_offset(), h.kind()))
        .collect();
    assert_eq!(hits, [(MIB, FileKind::Zip), (MIB + 8192, FileKind::Jpeg)]);

    // all kinds: the ZIP, and the JPEG inside it is skipped (I2 c)
    let (run, summary) = carve(
        &source,
        &session,
        input(&session, KindSet::all(), g, none(), none(), none()),
    );
    assert_eq!(run.files.len(), 1);
    assert_eq!(
        (run.files[0].offset(), run.files[0].len()),
        (MIB, zip.len() as u64)
    );
    assert!(run.files[0].end_verified());
    assert_eq!(summary.inside_carved, 1);
    assert_eq!(summary.candidates_done, 2);

    // JPEG alone: only "a file already carved in this run" is skipped, so
    // the picture inside the archive is carved on its own
    let jpeg: KindSet = [FileKind::Jpeg].into_iter().collect();
    let (jrun, _) = carve(
        &source,
        &session,
        input(&session, jpeg, g, none(), none(), none()),
    );
    assert_eq!(jrun.files.len(), 1);
    assert_eq!(jrun.files[0].offset(), MIB + 8192);
    let mut set = CarvedSet::new();
    set.apply(&run);
    set.apply(&jrun);
    assert_eq!(set.len(), 2);
    let (_, recovered) = recover_files(&source, 0..source.size(), set.files(), &|_| FlagSet::EMPTY);
    let by_ext: BTreeMap<String, String> = recovered
        .iter()
        .map(|r| {
            (
                r.file.ext().to_string(),
                sha256_hex(r.content.as_ref().unwrap()),
            )
        })
        .collect();
    assert_eq!(by_ext["zip"], sha256_hex(&zip));
    assert_eq!(by_ext["jpg"], sha256_hex(&jpg.data));
}

#[test]
fn a_file_with_an_unverified_end_does_not_swallow_the_files_after_it() {
    // A fragmented faststart MP4: its first cluster at cluster 10, the rest
    // at cluster 30. A lost JPEG sits at cluster 11, inside the length the
    // MP4's header states.
    let g = geometry(4096, 0);
    let mp4 = sample("mp4_ffmpeg_faststart.mp4");
    let jpg = sample("jpeg_baseline.jpg");
    assert!(mp4.data.len() > 4096 && jpg.data.len() < 19 * 4096);
    let mut disk = Disk::new(4 * MIB as usize);
    disk.put(10 * 4096, &mp4.data[..4096]);
    disk.put(11 * 4096, &jpg.data);
    disk.put(30 * 4096, &mp4.data[4096..]);
    let source = disk.source();
    let session = scan_source(&source, 0..source.size());
    let (run, summary) = carve(
        &source,
        &session,
        input(&session, KindSet::all(), g, none(), none(), none()),
    );
    let found: Vec<(u64, &str, bool)> = run
        .files
        .iter()
        .map(|f| (f.offset(), f.ext(), f.end_verified()))
        .collect();
    let video = found
        .iter()
        .find(|f| f.0 == 10 * 4096)
        .unwrap_or_else(|| panic!("{found:?}"));
    assert!(
        !video.2,
        "the fragmented video's end is not verified: {found:?}"
    );
    let photo = run
        .files
        .iter()
        .find(|f| f.offset() == 11 * 4096)
        .unwrap_or_else(|| panic!("the photo inside the video's span is carved: {found:?}"));
    assert!(photo.end_verified());
    assert_eq!(photo.len(), jpg.data.len() as u64);
    assert_eq!(summary.inside_carved, 0);
}

#[test]
fn fragmented_files_are_never_good() {
    // Each sample of more than one cluster is cut after its first cluster
    // (or after its first half): the first part lies on the grid, the rest
    // eight clusters further on, and in between lies something else -
    // random data, zeros, or a cluster of another sample. What is carved at
    // the first part is never `good` unless it is the whole file's own
    // complete part; the second part has no signature and is not found at
    // all. Several seeds: the outcome must not depend on the data.
    let g = geometry(4096, 0);
    let c = g.cluster_size() as usize;
    let all = samples();
    let mut carved_unverified: BTreeMap<FileKind, usize> = BTreeMap::new();
    let mut not_carved: BTreeMap<FileKind, usize> = BTreeMap::new();
    for seed in [0xF4A6, 12, 13, 0x5EED_0003] {
        let mut rng = Rng::new(seed);
        for (round, filler) in ["random", "zeros", "foreign"].iter().enumerate() {
            let mut disk = Disk::new(32 * MIB as usize);
            let mut placed = Vec::new();
            let mut at = MIB as usize;
            for (i, s) in all.iter().enumerate() {
                let n = s.data.len();
                if n <= c {
                    continue;
                }
                let cut = if i % 2 == 0 {
                    c
                } else {
                    (n / 2).div_ceil(c) * c
                };
                if cut >= n {
                    continue;
                }
                let gap = 8 * c;
                disk.put(at as u64, &s.data[..cut]);
                let between: Vec<u8> = match round {
                    0 => rng.bytes(gap),
                    1 => vec![0; gap],
                    _ => {
                        // the tail of a sample of another kind. (A PDF
                        // continued by the tail of another PDF at the same
                        // offsets - a deleted PDF whose first cluster a new
                        // one reused - passes: the cross-reference offsets
                        // of the old one still point where they did. A
                        // known limit, see the crate documentation of
                        // hds_carve.)
                        let mut k = (i * 7 + 3 + seed as usize) % all.len();
                        while all[k].kind == s.kind {
                            k = (k + 1) % all.len();
                        }
                        let other = &all[k].data;
                        let mut v = other[other.len().min(c)..].to_vec();
                        v.resize(gap, 0x20);
                        v
                    }
                };
                disk.put((at + cut) as u64, &between);
                disk.put((at + cut + gap) as u64, &s.data[cut..]);
                placed.push((at as u64, s.clone(), cut));
                at += (cut + gap + (n - cut)).div_ceil(c) * c + c;
            }
            assert!(at < 32 * MIB as usize);
            let source = disk.source();
            let session = scan_source(&source, 0..source.size());
            let (run, _) = carve(
                &source,
                &session,
                input(&session, KindSet::all(), g, none(), none(), none()),
            );
            let by_offset: BTreeMap<u64, CarvedFile> =
                run.files.iter().map(|f| (f.offset(), *f)).collect();
            for (at, s, cut) in &placed {
                let Some(f) = by_offset.get(at) else {
                    *not_carved.entry(s.kind).or_default() += 1;
                    continue;
                };
                // whatever was carved, its end is not verified - unless it
                // is a complete part of the file inside the first fragment
                // (the first revision of a PDF). JPEG and GIF, which have
                // no checksums, are decoded: running on through the gap to
                // the file's own end marker does not verify it.
                if f.end_verified() {
                    assert!(
                        f.end() <= at + *cut as u64,
                        "seed {seed:#x}, {filler}: {} carved as {f:?}",
                        s.name
                    );
                } else {
                    *carved_unverified.entry(s.kind).or_default() += 1;
                }
                // nothing is carved where the second fragments lie
            }
            for f in &run.files {
                assert!(
                    placed.iter().any(|(a, _, _)| *a == f.offset()),
                    "seed {seed:#x}, {filler}: {f:?}"
                );
            }
        }
    }
    println!("fragmented, not carved: {not_carved:?}");
    println!("fragmented, carved with an unverified end: {carved_unverified:?}");
    // the formats that end in header arithmetic (or are complete after any
    // frame) are carved, rated uncertain, and so are JPEG and GIF files
    // whose data does not decode; those that can only end with a verified
    // end are not carved at all
    for kind in [FileKind::Bmp, FileKind::Wav, FileKind::Mp3, FileKind::Jpeg] {
        assert!(
            carved_unverified.get(&kind).copied().unwrap_or(0) > 0,
            "{kind:?}"
        );
    }
    for kind in [FileKind::Png, FileKind::Zip, FileKind::SevenZip] {
        assert!(not_carved.get(&kind).copied().unwrap_or(0) > 0, "{kind:?}");
        assert_eq!(carved_unverified.get(&kind), None, "{kind:?}");
    }
}

/// Raises the cancel flag after `after` files.
struct CancelAfter<'a> {
    after: usize,
    found: usize,
    flag: &'a AtomicBool,
    progress: Vec<CarveProgress>,
}

impl CarveObserver for CancelAfter<'_> {
    fn progress(&mut self, progress: &CarveProgress) {
        self.progress.push(progress.clone());
    }
    fn found(&mut self, _file: &CarvedFile) {
        self.found += 1;
        if self.found == self.after {
            self.flag.store(true, Ordering::SeqCst);
        }
    }
}

fn guard_for<'a>(source: &'a dyn BlockSource, protected: &'a [Protected]) -> SessionGuard<'a> {
    SessionGuard {
        source,
        protected,
        allow_same_disk: true,
        allow_unknown_disk: true,
    }
}

#[test]
fn cancel_store_load_and_run_again() {
    let g = geometry(4096, MIB);
    let all = samples();
    let mut disk = Disk::new(8 * MIB as usize);
    lay_out(&mut disk, g, 0, &all);
    let source = disk.source();
    let range = 0..source.size();
    let mut session = scan_source(&source, range.clone());
    let dir = TempDir::new("session");
    let path = dir.path().join("s.hdss");
    let protected = [Protected::session(range.clone())];
    session
        .save_as(&path, &guard_for(source.as_ref(), &protected))
        .unwrap();

    let ranged = RangedSource::new(source.clone(), range.clone()).unwrap();
    let full = || {
        Carver::new(
            input(
                &session_copy(&path),
                KindSet::all(),
                g,
                none(),
                none(),
                none(),
            ),
            range.clone(),
        )
        .run(&ranged, &mut NoCarveObserver, &AtomicBool::new(false))
    };
    let (complete, summary) = full();
    assert_eq!(summary.status, CarveStatus::Completed);
    // deterministic: the same files, and the same session bytes
    let (again, _) = full();
    assert_eq!(again, complete);

    // cancelled after 10 files: those 10 are the run
    let flag = AtomicBool::new(false);
    let mut observer = CancelAfter {
        after: 10,
        found: 0,
        flag: &flag,
        progress: Vec::new(),
    };
    let carver = Carver::new(
        input(&session, KindSet::all(), g, none(), none(), none()),
        range.clone(),
    )
    .with_progress_interval(Duration::ZERO);
    let total = carver.candidates();
    let (partial, psum) = carver.run(&ranged, &mut observer, &flag);
    assert_eq!(psum.status, CarveStatus::Cancelled);
    assert_eq!(partial.files, complete.files[..10]);
    assert_eq!(psum.found, 10);
    assert!(psum.candidates_done < total);
    // progress: the members, monotonic, the last one final
    assert!(observer.progress.len() >= 10);
    let last = observer.progress.last().unwrap();
    assert_eq!(
        (last.candidates_done, last.candidates_total, last.found),
        (psum.candidates_done, total, 10)
    );
    assert!(observer
        .progress
        .windows(2)
        .all(|w| w[0].candidates_done <= w[1].candidates_done));
    let json = serde_json::to_value(last).unwrap();
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "bytes_found",
            "candidates_done",
            "candidates_total",
            "eta_seconds",
            "found"
        ]
    );

    // the cancelled run is kept (I5), stored, and loads back
    let mut set = CarvedSet::new();
    set.apply(&partial);
    store_run(&mut session, &partial).unwrap();
    session.checkpoint().unwrap();
    assert_eq!(CarvedSet::from_session(&session_copy(&path)).unwrap(), set);

    // carving again replaces it: the result equals the uninterrupted run
    set.apply(&complete);
    store_run(&mut session, &complete).unwrap();
    session.checkpoint().unwrap();
    let loaded = CarvedSet::from_session(&session_copy(&path)).unwrap();
    assert_eq!(loaded, set);
    assert_eq!(loaded.files(), &complete.files[..]);
    assert_eq!(loaded.geometry(), Some(g));

    // a second session carved the same way holds the same CARVED bytes
    let mut twin = scan_source(&source, range.clone());
    store_run(&mut twin, &partial).unwrap();
    store_run(&mut twin, &again).unwrap();
    assert_eq!(
        twin.opaque(hds_core::session::OpaqueKind::Carved),
        session.opaque(hds_core::session::OpaqueKind::Carved)
    );

    // one kind again: only that kind changes
    let pngs: KindSet = [FileKind::Png].into_iter().collect();
    let (png_run, _) = Carver::new(
        input(&session, pngs, g, none(), none(), none()),
        range.clone(),
    )
    .run(&ranged, &mut NoCarveObserver, &AtomicBool::new(false));
    assert!(!png_run.files.is_empty());
    set.apply(&png_run);
    store_run(&mut session, &png_run).unwrap();
    session.checkpoint().unwrap();
    assert_eq!(CarvedSet::from_session(&session_copy(&path)).unwrap(), set);
    assert_eq!(set, loaded);

    // compaction keeps the set; discarding empties it
    set.store_compacted(&mut session).unwrap();
    session.checkpoint().unwrap();
    let reloaded = session_copy(&path);
    assert_eq!(
        reloaded.opaque(hds_core::session::OpaqueKind::Carved).len(),
        1
    );
    assert_eq!(CarvedSet::from_session(&reloaded).unwrap(), set);
    discard_carved(&mut session).unwrap();
    session.checkpoint().unwrap();
    assert!(CarvedSet::from_session(&session_copy(&path))
        .unwrap()
        .is_empty());
}

fn session_copy(path: &std::path::Path) -> Session {
    Session::load(path).unwrap().0
}

#[test]
fn unreadable_candidates_and_a_lost_device() {
    let g = geometry(4096, 0);
    let all = samples();
    let mut disk = Disk::new(8 * MIB as usize);
    let placed = lay_out(&mut disk, g, 256, &all);
    let mem: Arc<dyn BlockSource> = disk.source();
    let faulty = Arc::new(FaultySource::new(mem.clone()));
    let source: Arc<dyn BlockSource> = faulty.clone();
    let range = 0..source.size();
    let session = scan_source(&source, range.clone());
    let (complete, _) = carve(
        &mem,
        &session,
        input(&session, KindSet::all(), g, none(), none(), none()),
    );

    // a bad sector in the middle of the fifth file: that one is dropped
    let (bad_at, bad_sample) = &placed[4];
    let bad = bad_at + 1024..bad_at + 1536;
    faulty.bad_sectors(bad.clone());
    let (run, summary) = carve(
        &source,
        &session,
        input(&session, KindSet::all(), g, none(), none(), none()),
    );
    assert_eq!(summary.status, CarveStatus::Completed);
    assert_eq!(summary.unreadable, 1, "{}", bad_sample.name);
    let without: Vec<CarvedFile> = complete
        .files
        .iter()
        .copied()
        .filter(|f| f.offset() != *bad_at)
        .collect();
    assert_eq!(run.files, without);
    faulty.clear_faults();

    // the device goes away: the job ends with source_lost, the files found
    // before stay in the run
    faulty.reset_counters();
    faulty.lose_after_reads(40);
    let (run, summary) = carve(
        &source,
        &session,
        input(&session, KindSet::all(), g, none(), none(), none()),
    );
    match &summary.status {
        CarveStatus::Failed(f @ CarveFailure::SourceLost(_)) => assert_eq!(f.code(), "source_lost"),
        other => panic!("{other:?}"),
    }
    assert!(!run.files.is_empty() && run.files.len() < complete.files.len());
    assert_eq!(run.files, complete.files[..run.files.len()]);
    assert_eq!(summary.found, run.files.len() as u64);
}

#[test]
fn a_file_cut_short_by_an_unreadable_sector_is_not_good() {
    // A PDF with two incremental updates (`%%EOF` at 2163, 2726 and 6384)
    // and an unreadable sector inside its last revision. The job measures
    // it again in front of the sector and gets the second revision - a
    // readable document, but the file announced that it goes on: its end
    // is not verified and it is not rated good. A JPEG with an unreadable
    // sector right behind its end is still carved whole and good.
    let g = geometry(4096, 0);
    let pdf = sample("pdf_incremental_update.pdf");
    let jpg = sample("jpeg_jfif_exif_xmp.jpg");
    let mut disk = Disk::new(MIB as usize);
    let pdf_at = 64 * 4096;
    let jpg_at = 128 * 4096;
    disk.put(pdf_at, &pdf.data);
    disk.put(jpg_at, &jpg.data);
    let mem: Arc<dyn BlockSource> = disk.source();
    let faulty = Arc::new(FaultySource::new(mem.clone()));
    let source: Arc<dyn BlockSource> = faulty.clone();
    let session = scan_source(&source, 0..source.size());
    let pdf_bad = pdf_at + 4096..pdf_at + 4608;
    let jpg_bad = (jpg_at + jpg.data.len() as u64).next_multiple_of(512);
    faulty.bad_sectors(pdf_bad.clone());
    faulty.bad_sectors(jpg_bad..jpg_bad + 512);
    let (run, summary) = carve(
        &source,
        &session,
        input(&session, KindSet::all(), g, none(), none(), none()),
    );
    assert_eq!(summary.status, CarveStatus::Completed);
    assert_eq!(summary.unreadable, 0);
    let f = run.files.iter().find(|f| f.offset() == pdf_at).unwrap();
    assert_eq!((f.len(), f.end_verified()), (2727, false));
    let bad = ranges(&[pdf_bad]);
    assert_eq!(
        f.rating(f.health_flags(&none(), &bad), true),
        hds_recover::Rating::Uncertain
    );
    let j = run.files.iter().find(|f| f.offset() == jpg_at).unwrap();
    assert_eq!((j.len(), j.end_verified()), (jpg.data.len() as u64, true));
    assert_eq!(
        j.rating(j.health_flags(&none(), &none()), true),
        hds_recover::Rating::Good
    );
}

/// Incompressible data with the local header of a streamed bzip2 entry
/// (bit 3, sizes 0) every `every` bytes: candidates whose descriptor is
/// never found, so each runs into the read limit.
struct StreamedNoise {
    size: u64,
    every: u64,
    /// The compression method of the streamed entry (12: bzip2, 0: stored).
    method: u8,
    read: std::sync::atomic::AtomicU64,
}

impl StreamedNoise {
    fn header(&self) -> Vec<u8> {
        let mut h = vec![
            0x50,
            0x4B,
            0x03,
            0x04,
            46,
            0,
            0x08,
            0,
            self.method,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            5,
            0,
            0,
            0,
        ];
        h.extend_from_slice(b"a.bin");
        h
    }

    fn word(i: u64) -> u64 {
        let mut x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        x ^= x >> 31;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^ (x >> 29)
    }
}

impl BlockSource for StreamedNoise {
    fn size(&self) -> u64 {
        self.size
    }
    fn sector_size(&self) -> u32 {
        512
    }
    fn physical_sector_size(&self) -> u32 {
        512
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.read.fetch_add(buf.len() as u64, Ordering::Relaxed);
        for (k, c) in buf.chunks_mut(8).enumerate() {
            let w = Self::word(offset / 8 + k as u64).to_le_bytes();
            c.copy_from_slice(&w[..c.len()]);
        }
        let h = self.header();
        let end = offset + buf.len() as u64;
        let mut at = offset / self.every * self.every;
        while at < end {
            for (j, &b) in h.iter().enumerate() {
                let p = at + j as u64;
                if (offset..end).contains(&p) {
                    buf[(p - offset) as usize] = b;
                }
            }
            at += self.every;
        }
        Ok(())
    }
    fn info(&self) -> SourceInfo {
        SourceInfo {
            kind: SourceKind::Image,
            path: "streamed-noise".into(),
            size: self.size,
            sector_size: 512,
            physical_sector_size: 512,
            model: None,
            serial: None,
            disk_number: None,
        }
    }
    fn is_alive(&self) -> bool {
        true
    }
}

/// Carves four streamed ZIP candidates of `method` on 2 GiB of noise;
/// returns the bytes read.
fn carve_streamed_noise(method: u8) -> u64 {
    const GIB: u64 = 1 << 30;
    let size = 2 * GIB;
    let every = 256 * MIB;
    let noise = Arc::new(StreamedNoise {
        size,
        every,
        method,
        read: std::sync::atomic::AtomicU64::new(0),
    });
    let src: Arc<dyn BlockSource> = noise.clone();
    let hits: Vec<CarveHit> = (0..4)
        .map(|i| CarveHit::new(i * every, FileKind::Zip).unwrap())
        .collect();
    let input = CarveInput {
        kinds: KindSet::all(),
        geometry: geometry(4096, 0),
        hits,
        done: std::iter::once(0..size).collect(),
        protected_files: none(),
        overwritten: none(),
        live_allocated: none(),
    };
    let carver = Carver::new(input, 0..size);
    assert_eq!(carver.candidates(), 4);
    let (run, summary) = carver.run(
        &RangedSource::new(src, 0..size).unwrap(),
        &mut NoCarveObserver,
        &AtomicBool::new(false),
    );
    let read = noise.read.load(Ordering::Relaxed);
    println!("4 streamed candidates of method {method} on 2 GiB: {read} bytes read");
    assert!(run.files.is_empty());
    assert_eq!(summary.not_measurable, 4);
    read
}

#[test]
fn zip_retries_share_one_pass_over_the_disk() {
    // I3's retry of a ZIP that hit the read limit reads up to the bound,
    // which after a format is usually the end of the range. Four streamed
    // stored entries (whose data can be anything, of any length, so they
    // are searched for a descriptor with the right CRC as far as the data
    // goes), on 2 GiB of noise: every first attempt reads the default
    // 1 GiB, and the retries together read at most the scanned size once
    // more (without the shared budget they would read 2 + 1.75 + 1.5 +
    // 1.25 GiB).
    const GIB: u64 = 1 << 30;
    let read = carve_streamed_noise(0);
    // first attempts 4 GiB, the retries 2 GiB, some read-ahead
    assert!(read <= 6 * GIB + 64 * MIB, "{read} bytes read");
    assert!(
        read >= 6 * GIB - 64 * MIB,
        "{read} bytes read: the first retry ran"
    );
}

#[test]
fn opaque_streamed_entries_cost_a_bounded_window() {
    // The same with bzip2 entries: nothing in their data can be checked,
    // so the search for the descriptor stops after a window of 32 MiB
    // (hds_carve's zip module) instead of reading to the limit, and no
    // retry follows.
    let read = carve_streamed_noise(12);
    assert!(read <= 4 * (34 * MIB), "{read} bytes read");
    assert!(read >= 4 * (32 * MIB), "{read} bytes read");
}

#[test]
fn never_a_byte_outside_the_session_range() {
    // The session is one partition of the disk; files lie at its start, at
    // its end (one crossing it), and outside it on both sides.
    let g = geometry(4096, 2 * MIB);
    let png = sample("png_rgb.png");
    let zip = sample("zip_ooxml_word.docx");
    let mut disk = Disk::new(8 * MIB as usize);
    let range = 2 * MIB..5 * MIB;
    disk.put(MIB, &png.data); // before the range
    disk.put(range.start, &png.data); // first cluster of the range
    disk.put(range.start + 64 * KIB, &zip.data);
    disk.put(range.end - 8192, &png.data); // crosses the range end
    disk.put(range.end, &zip.data); // behind the range
    let faulty = Arc::new(FaultySource::new(disk.source()));
    let source: Arc<dyn BlockSource> = faulty.clone();
    let session = scan_source(&source, range.clone());
    let (run, summary) = carve(
        &source,
        &session,
        input(&session, KindSet::all(), g, none(), none(), none()),
    );
    assert_eq!(summary.status, CarveStatus::Completed);
    let offsets: Vec<u64> = run.files.iter().map(CarvedFile::offset).collect();
    assert_eq!(offsets, [range.start, range.start + 64 * KIB]);
    assert_eq!(summary.not_measurable, 1);
    let touched = faulty.touched();
    assert!(!touched.is_empty());
    for r in touched.iter() {
        assert!(range.start <= r.start && r.end <= range.end, "read {r:?}");
    }
    // a source that does not cover the candidates' range is refused unread
    faulty.reset_counters();
    let reads_before = faulty.read_count();
    let carver = Carver::new(
        input(&session, KindSet::all(), g, none(), none(), none()),
        range.clone(),
    );
    let narrower = RangedSource::new(source.clone(), range.start..range.end - 4096).unwrap();
    let (run, summary) = carver.run(&narrower, &mut NoCarveObserver, &AtomicBool::new(false));
    assert!(run.files.is_empty());
    match summary.status {
        CarveStatus::Failed(f) => assert_eq!(f.code(), "internal"),
        other => panic!("{other:?}"),
    }
    assert_eq!(faulty.read_count(), reads_before);
}

#[test]
fn hits_kept_under_the_scanners_cap_are_carved() {
    let g = geometry(4096, 0);
    let all = samples();
    let mut disk = Disk::new(8 * MIB as usize);
    let placed = lay_out(&mut disk, g, 16, &all);
    let source = disk.source();
    let range = 0..source.size();
    let options = ScanOptions::default();
    let meta =
        SessionMeta::new(SourceIdentity::from(&source.info()), range.clone(), options).unwrap();
    let mut session = Session::new(meta);
    let tuning = ScanTuning {
        carve_hit_cap: 12,
        ..ScanTuning::default()
    };
    scan_with(
        &RangedSource::new(source.clone(), range.clone()).unwrap(),
        &options,
        &mut session,
        &mut NoObserver,
        &AtomicBool::new(false),
        &tuning,
    );
    assert_eq!(session.store().carve_hit_count(), 12);
    assert!(session
        .store()
        .warnings()
        .iter()
        .any(|w| w.code == WARN_CARVE_HITS_CAPPED));
    let (run, _) = carve(
        &source,
        &session,
        input(&session, KindSet::all(), g, none(), none(), none()),
    );
    let want: Vec<CarvedFile> = placed[..run.files.len()]
        .iter()
        .map(|(a, s)| expected(*a, s))
        .collect();
    assert!(run.files.len() >= 10);
    assert_eq!(run.files, want);
}

/// A large source that reads as zeros, for the 64-million-hit test.
struct Zeros(u64);

impl BlockSource for Zeros {
    fn size(&self) -> u64 {
        self.0
    }
    fn sector_size(&self) -> u32 {
        512
    }
    fn physical_sector_size(&self) -> u32 {
        512
    }
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(0);
        Ok(())
    }
    fn info(&self) -> SourceInfo {
        SourceInfo {
            kind: SourceKind::Image,
            path: "zeros".to_string(),
            size: self.0,
            sector_size: 512,
            physical_sector_size: 512,
            model: None,
            serial: None,
            disk_number: None,
        }
    }
    fn is_alive(&self) -> bool {
        true
    }
    fn location(&self) -> SourceLocation {
        SourceLocation::Detached
    }
}

#[test]
fn the_64_million_hit_cap() {
    // As many hits as the scanner keeps (A3.4): one per sector over 32 GiB.
    // Choosing candidates is linear in the hits, in place; measuring is per
    // candidate.
    let cap = hds_core::scan::CARVE_HIT_CAP;
    let size = cap as u64 * 512;
    let started = Instant::now();
    let hits: Vec<CarveHit> = (0..cap as u64)
        .map(|i| {
            CarveHit::new(
                i * 512,
                if i % 2 == 0 {
                    FileKind::Jpeg
                } else {
                    FileKind::Png
                },
            )
            .unwrap()
        })
        .collect();
    let built = started.elapsed();
    // in-use files cover everything but the first 64 MiB and a stretch at
    // 20 GiB; one cluster in eight of those is on the grid
    let mut protected = RangeSet::new();
    protected.insert(64 * MIB..20 << 30);
    protected.insert((20 << 30) + 32 * MIB..size);
    let mut done = RangeSet::new();
    done.insert(0..size);
    let input = CarveInput {
        kinds: [FileKind::Jpeg].into_iter().collect(),
        geometry: geometry(4096, 0),
        hits,
        done,
        protected_files: protected,
        overwritten: none(),
        live_allocated: none(),
    };
    let started = Instant::now();
    let carver = Carver::new(input, 0..size);
    let chosen = started.elapsed();
    // JPEG hits are at even sectors; on the 4 KiB grid every 8th sector
    assert_eq!(carver.candidates(), (64 * MIB + 32 * MIB) / 4096);
    let source: Arc<dyn BlockSource> = Arc::new(Zeros(size));
    let ranged = RangedSource::new(source, 0..size).unwrap();
    let started = Instant::now();
    let (run, summary) = carver.run(&ranged, &mut NoCarveObserver, &AtomicBool::new(false));
    let measured = started.elapsed();
    assert!(run.files.is_empty());
    assert_eq!(summary.candidates_done, summary.candidates_total);
    assert_eq!(summary.not_measurable, summary.candidates_total);
    println!(
        "{cap} hits: built in {built:?}, candidates chosen in {chosen:?}, {} measured in {measured:?}",
        summary.candidates_total
    );
    assert!(chosen < Duration::from_secs(120), "{chosen:?}");
}

/// Patches `data` at `at`.
fn patch(mut data: Vec<u8>, at: usize, bytes: &[u8]) -> Vec<u8> {
    data[at..at + bytes.len()].copy_from_slice(bytes);
    data
}

fn find(data: &[u8], needle: &[u8]) -> usize {
    data.windows(needle.len())
        .position(|w| w == needle)
        .unwrap()
}

#[test]
fn hostile_length_fields_and_loops() {
    let g = geometry(4096, 0);
    let mut hostile: Vec<(&str, FileKind, Vec<u8>)> = Vec::new();
    // a PNG chunk that claims 2 GiB
    let png = sample("png_rgb.png").data;
    hostile.push((
        "png chunk of 2 GiB",
        FileKind::Png,
        patch(png, 33, &0x7FFF_FFF0u32.to_be_bytes()),
    ));
    // a RIFF size of 4 GiB
    let wav = sample("wav_pcm16.wav").data;
    hostile.push((
        "riff of 4 GiB",
        FileKind::Wav,
        patch(wav, 4, &0xFFFF_FFF0u32.to_le_bytes()),
    ));
    // an mdat of 2^60 bytes (64-bit size)
    let mp4 = sample("mp4_ffmpeg_moov_last.mp4").data;
    let mdat = find(&mp4, b"mdat") - 4;
    let mut m = patch(mp4, mdat, &1u32.to_be_bytes());
    m = patch(m, mdat + 8, &(1u64 << 60).to_be_bytes());
    hostile.push(("mdat of 2^60 bytes", FileKind::Mp4, m));
    // a ZIP entry of 4 GiB
    let zip = sample("zip_infozip_deflate.zip").data;
    hostile.push((
        "zip entry of 4 GiB",
        FileKind::Zip,
        patch(zip, 18, &0xFFFF_FFF0u32.to_le_bytes()),
    ));
    // a Matroska segment of 2^48 bytes
    let mkv = sample("mkv_ffmpeg.mkv").data;
    let seg = find(&mkv, &[0x18, 0x53, 0x80, 0x67]);
    hostile.push((
        "segment of 2^48 bytes",
        FileKind::Mkv,
        patch(mkv, seg + 4, &(1u64 << 56 | 1 << 48).to_be_bytes()),
    ));
    // a 7z end header 2^62 bytes away (start header CRC made to match)
    let mut sz = sample("7z_lzma2.7z").data;
    sz[12..20].copy_from_slice(&(1u64 << 62).to_le_bytes());
    let crc = crc32(&sz[12..32]);
    sz[8..12].copy_from_slice(&crc.to_le_bytes());
    hostile.push(("7z header at 2^62", FileKind::SevenZip, sz));
    // a TIFF whose first directory points at itself
    let mut tif = sample("tiff_pillow_uncompressed.tif").data;
    let ifd = u32::from_le_bytes(tif[4..8].try_into().unwrap()) as usize;
    let n = u16::from_le_bytes(tif[ifd..ifd + 2].try_into().unwrap()) as usize;
    tif[ifd + 2 + 12 * n..ifd + 6 + 12 * n].copy_from_slice(&(ifd as u32).to_le_bytes());
    hostile.push(("tiff directory loop", FileKind::Tiff, tif));
    // a BMP that says it is 4 GiB long
    let bmp = sample("bmp_rgb24.bmp").data;
    hostile.push((
        "bmp of 4 GiB",
        FileKind::Bmp,
        patch(bmp, 2, &0xFFFF_FF00u32.to_le_bytes()),
    ));
    // a JPEG of fill bytes for ever, a PDF whose startxref points at itself,
    // a ZIP entry streamed to the end of the disk
    let jpg = sample("jpeg_baseline.jpg").data;
    let mut fill = jpg[..find(&jpg, &[0xFF, 0xDA])].to_vec();
    fill.resize(3 * MIB as usize, 0xFF);
    hostile.push(("jpeg fill bytes", FileKind::Jpeg, fill));
    let pdf = sample("pdf_pypdf.pdf").data;
    let sx = pdf.windows(9).rposition(|w| w == b"startxref").unwrap();
    let mut p = pdf[..sx].to_vec();
    p.extend_from_slice(format!("startxref\n{sx}\n%%EOF\n").as_bytes());
    hostile.push(("pdf startxref loop", FileKind::Pdf, p));
    let mut streamed = sample("zip_python_stream_descriptors.zip").data;
    streamed.truncate(64);
    streamed.resize(4 * MIB as usize, 0);
    hostile.push(("zip streamed into the void", FileKind::Zip, streamed));

    let mut disk = Disk::new(64 * MIB as usize);
    let mut at = MIB;
    let mut placed = Vec::new();
    for (name, kind, data) in &hostile {
        assert_eq!(hds_carve::sniff(data), Some(*kind), "{name}");
        disk.put(at, data);
        placed.push((at, *name));
        at += (data.len() as u64).div_ceil(MIB) * MIB + MIB;
    }
    let faulty = Arc::new(FaultySource::new(disk.source()));
    let source: Arc<dyn BlockSource> = faulty.clone();
    let session = scan_source(&source, 0..source.size());
    faulty.reset_counters();
    let started = Instant::now();
    let (run, summary) = carve(
        &source,
        &session,
        input(&session, KindSet::all(), g, none(), none(), none()),
    );
    let took = started.elapsed();
    assert_eq!(summary.status, CarveStatus::Completed);
    let reads = faulty.read_count();
    println!(
        "hostile: {} candidates, {reads} reads, {took:?}; carved {:?}",
        summary.candidates_total, run.files
    );
    for (at, name) in &placed {
        if let Some(f) = run.files.iter().find(|f| f.offset() == *at) {
            // whatever is believed is not verified, and lies on the disk
            assert!(!f.end_verified(), "{name}: {f:?}");
            assert!(f.end() <= 64 * MIB, "{name}: {f:?}");
        }
    }
    // a few dozen reads per candidate at most, and quickly
    assert!(
        reads < 200 * summary.candidates_total.max(1),
        "{reads} reads"
    );
    assert!(took < Duration::from_secs(60), "{took:?}");
}
