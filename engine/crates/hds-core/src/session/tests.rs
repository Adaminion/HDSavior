//! Tests of the session file: round trips, crashes, hostile files, the
//! destination guard, replacement rules, opaque payloads and the identity
//! check.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use hds_io::testing::{FaultySource, MemSource, XorShift};
use hds_io::{BlockSource, ImageFile, Protected, SourceInfo, SourceLocation, Verdict};

use super::*;
use crate::scan::tests::{mem, meta_for, put, rich_file, scan_into, spread_image, CHUNK, MIB};
use crate::scan::{ScanOptions, ScanStatus, ScanTuning};
use crate::store::ScanStore;
use crate::test_build::*;

/// A scratch directory below `HDS_CORE_TEST_DIR` (else the system's
/// temporary directory), removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let base =
            std::env::var_os("HDS_CORE_TEST_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let path = base.join(format!(
            "hds-core-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(std::fs::canonicalize(&path).unwrap())
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn guard_for<'a>(src: &'a dyn BlockSource, protected: &'a [Protected]) -> SessionGuard<'a> {
    SessionGuard {
        source: src,
        protected,
        allow_same_disk: false,
        allow_unknown_disk: false,
    }
}

/// A test image with every kind of finding: records (one torn, one crossing a
/// chunk end), index blocks with entries, a boot sector, carve hits, and a bad
/// sector.
fn rich_image() -> Vec<u8> {
    let mut img = spread_image();
    let mut torn = rich_file(2000).build();
    tear(&mut torn, 1);
    put(&mut img, 0x9000, &torn);
    put(&mut img, CHUNK - 512, &rich_file(2001).build());
    let dir = Rec::new(2002)
        .flags(3)
        .si([1, 2, 3, 4], 0)
        .name(fref(5, 5), "Folder", 1, 0x1000_0000)
        .index_root(&[index_entry(
            fref(2100, 1),
            fref(2002, 1),
            "Kid",
            1,
            0x1000_0000,
            [1, 2, 3, 4],
        )])
        .index_allocation(0, 0, 4096, &runlist(&[(Some(77), 1)]))
        .build();
    put(&mut img, 2 * CHUNK + 0x2000, &dir);
    put(
        &mut img,
        2 * CHUNK + 0x4000,
        &indx_block(
            0,
            5,
            4096,
            &[
                index_entry(
                    fref(2200, 1),
                    fref(2002, 1),
                    "\u{4e2d}\u{6587}",
                    1,
                    0x1000_0000,
                    [1, 2, 3, 4],
                ),
                index_entry(fref(2201, 1), fref(2002, 1), "a.txt", 1, 0x20, [1, 2, 3, 4]),
            ],
        ),
    );
    put(
        &mut img,
        0x20_0000,
        &boot_sector(512, 8, 100_000, 4, 2, -10, 1, 0),
    );
    put(&mut img, 0x30_0200, &png_head());
    img
}

fn rich_source() -> (Arc<FaultySource>, Arc<dyn BlockSource>) {
    let faulty = Arc::new(FaultySource::new(mem(rich_image())));
    faulty.bad_sectors(0x40_0000..0x40_0400);
    let dynsrc: Arc<dyn BlockSource> = faulty.clone();
    (faulty, dynsrc)
}

/// A completed in-memory session of the rich image.
fn scanned() -> (Session, Arc<dyn BlockSource>) {
    let (_, src) = rich_source();
    let range = 0..src.size();
    let mut session = Session::new(meta_for(
        src.as_ref(),
        range.clone(),
        ScanOptions::default(),
    ));
    let (summary, _) = scan_into(
        src.clone(),
        range,
        ScanOptions::default(),
        &ScanTuning::default(),
        &mut session,
        &AtomicBool::new(false),
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    let c = session.store().counters();
    assert!(c.records >= 6 && c.index_entries >= 2 && c.index_blocks >= 1 && c.boot_hits >= 1);
    assert!(c.carve_hits >= 2 && c.bad_bytes == 1024);
    (session, src)
}

#[test]
fn a_saved_session_loads_back_field_by_field() {
    let dir = TempDir::new("roundtrip");
    let (mut session, src) = scanned();
    session
        .append_opaque(OpaqueKind::Live, b"snapshot 1".to_vec())
        .unwrap();
    session.append_opaque(OpaqueKind::Carved, vec![]).unwrap();
    let protected = [Protected::session(0..src.size())];
    let path = dir.join("a.hds");
    let report = session
        .save_as(&path, &guard_for(src.as_ref(), &protected))
        .unwrap();
    assert_eq!(report.path, path);
    assert_eq!(report.bytes, std::fs::metadata(&path).unwrap().len());
    assert_eq!(session.file_path(), Some(path.as_path()));
    let (loaded, report) = Session::load(&path).unwrap();
    assert_eq!(report.ignored_bytes, 0);
    assert_eq!(report.checkpoints, 2);
    assert_eq!(loaded.meta(), session.meta());
    assert_eq!(loaded.store(), session.store());
    assert_eq!(loaded.store().counters(), session.store().counters());
    assert_eq!(loaded.store().warnings(), session.store().warnings());
    assert_eq!(loaded.opaque(OpaqueKind::Live), &[b"snapshot 1".to_vec()]);
    assert_eq!(loaded.opaque(OpaqueKind::Carved), &[Vec::<u8>::new()]);
    assert!(loaded.opaque(OpaqueKind::Heads).is_empty());
    // Field by field through the read API as well.
    let (a, b) = (session.store(), loaded.store());
    for (&ia, &ib) in a.records_by_offset().iter().zip(b.records_by_offset()) {
        let (ra, rb) = (a.record(ia).unwrap(), b.record(ib).unwrap());
        assert_eq!(ra.disk_offset(), rb.disk_offset());
        assert_eq!(ra.record_number(), rb.record_number());
        assert_eq!(ra.fixup(), rb.fixup());
        assert_eq!(ra.standard_information(), rb.standard_information());
        assert_eq!(
            a.names(ia).collect::<Vec<_>>(),
            b.names(ib).collect::<Vec<_>>()
        );
        assert_eq!(
            a.data_extents(ia).collect::<Vec<_>>(),
            b.data_extents(ib).collect::<Vec<_>>()
        );
        assert_eq!(
            a.index_allocation_extents(ia).collect::<Vec<_>>(),
            b.index_allocation_extents(ib).collect::<Vec<_>>()
        );
    }
    // Two compacted copies of equal stores are byte-identical.
    let path2 = dir.join("b.hds");
    let mut again = loaded;
    again
        .save_as(&path2, &guard_for(src.as_ref(), &protected))
        .unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        std::fs::read(&path2).unwrap()
    );
}

/// The blocks of a session file: (offset, type, end).
fn blocks_of(bytes: &[u8]) -> Vec<(usize, u32, usize)> {
    let mut out = Vec::new();
    let mut pos = 8;
    while pos + 20 <= bytes.len() {
        let ty = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
        let len = u64::from_le_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize;
        let end = pos + 20 + len;
        if end > bytes.len() {
            break;
        }
        out.push((pos, ty, end));
        pos = end;
    }
    out
}

/// A session file written in several runs with a snapshot of the store and
/// the opaque lists after each checkpoint, in file order.
struct History {
    _dir: TempDir,
    path: PathBuf,
    bytes: Vec<u8>,
    snapshots: Vec<(ScanStore, Vec<Vec<u8>>)>,
}

fn history() -> History {
    let dir = TempDir::new("history");
    let (faulty, src) = rich_source();
    let range = 0..src.size();
    let protected = [Protected::session(range.clone())];
    let guard = guard_for(src.as_ref(), &protected);
    let path = dir.join("h.hds");
    let mut session = Session::create(
        meta_for(src.as_ref(), range.clone(), ScanOptions::default()),
        &path,
        &guard,
    )
    .unwrap();
    let snap = |s: &Session| (s.store().clone(), s.opaque(OpaqueKind::Live).to_vec());
    // `create` writes two checkpoints (identity first, then the empty state).
    let mut snapshots = vec![snap(&session), snap(&session)];
    let cancel = Arc::new(AtomicBool::new(false));
    for run in 0..4 {
        faulty.reset_counters();
        cancel.store(false, Ordering::SeqCst);
        if run < 3 {
            faulty.cancel_after_reads(6, cancel.clone());
        }
        let (summary, _) = scan_into(
            src.clone(),
            range.clone(),
            ScanOptions::default(),
            &ScanTuning::default(),
            &mut session,
            &cancel,
        );
        assert!(summary.checkpoint_error.is_none());
        snapshots.push(snap(&session));
        session
            .append_opaque(OpaqueKind::Live, format!("live {run}").into_bytes())
            .unwrap();
        session.checkpoint().unwrap();
        snapshots.push(snap(&session));
    }
    assert!(session.is_complete());
    let bytes = std::fs::read(&path).unwrap();
    History {
        _dir: dir,
        path,
        bytes,
        snapshots,
    }
}

/// Loads `bytes` from a file next to the history and compares with the
/// snapshot of the last checkpoint that ends at or before `valid_until`.
fn check_prefix_state(h: &History, bytes: &[u8], valid_until: usize, tag: &str) {
    let path = h.path.with_file_name(format!("cut-{tag}.hds"));
    std::fs::write(&path, bytes).unwrap();
    let checkpoints: Vec<usize> = blocks_of(&h.bytes)
        .into_iter()
        .filter(|&(_, ty, _)| ty == 11)
        .map(|(_, _, end)| end)
        .collect();
    assert_eq!(checkpoints.len(), h.snapshots.len());
    let expected = checkpoints.iter().rposition(|&end| end <= valid_until);
    match (Session::load(&path), expected) {
        (Err(e), None) => assert!(
            matches!(e, SessionError::Corrupt(_) | SessionError::Unsupported(_)),
            "{tag}: {e}"
        ),
        (Ok((s, report)), Some(k)) => {
            assert_eq!(
                s.store(),
                &h.snapshots[k].0,
                "{tag}: state of checkpoint {k}"
            );
            assert_eq!(s.opaque(OpaqueKind::Live), &h.snapshots[k].1[..], "{tag}");
            assert_eq!(report.valid_end as usize, checkpoints[k], "{tag}");
        }
        (Ok(_), None) => panic!("{tag}: loaded a file without a valid checkpoint"),
        (Err(e), Some(k)) => panic!("{tag}: refused ({e}), expected checkpoint {k}"),
    }
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_cut_file_loads_to_its_last_valid_checkpoint() {
    let h = history();
    let blocks = blocks_of(&h.bytes);
    assert_eq!(blocks.last().unwrap().2, h.bytes.len());
    let mut cuts: Vec<usize> = vec![0, 4, 8];
    for &(start, _, end) in &blocks {
        cuts.extend([start, start + 1, start + 19, start + 20, end - 1, end]);
    }
    let mut rng = XorShift::new(17);
    for _ in 0..200 {
        cuts.push(rng.below(h.bytes.len() as u64) as usize);
    }
    cuts.sort_unstable();
    cuts.dedup();
    for cut in cuts {
        check_prefix_state(&h, &h.bytes[..cut], cut, &format!("cut{cut}"));
    }
}

#[test]
fn flipped_bytes_lose_at_most_what_follows_them() {
    let h = history();
    let blocks = blocks_of(&h.bytes);
    let mut rng = XorShift::new(99);
    for i in 0..300 {
        let mut bytes = h.bytes.clone();
        let at = rng.below(bytes.len() as u64) as usize;
        bytes[at] ^= 1 << rng.below(8);
        // The state ends before the block that holds the flipped byte.
        let valid_until = if at < 8 {
            0
        } else {
            blocks
                .iter()
                .find(|&&(start, _, end)| (start..end).contains(&at))
                .unwrap()
                .0
        };
        check_prefix_state(&h, &bytes, valid_until, &format!("flip{i}"));
    }
}

#[test]
fn appending_after_a_cut_truncates_first() {
    let h = history();
    let blocks = blocks_of(&h.bytes);
    // Cut in the middle of the data blocks that follow the third checkpoint.
    let checkpoints: Vec<usize> = blocks.iter().filter(|b| b.1 == 11).map(|b| b.2).collect();
    let after = blocks.iter().find(|b| b.0 == checkpoints[2]).unwrap();
    assert_ne!(after.1, 11, "data follows that checkpoint");
    let cut = after.0 + (after.2 - after.0) / 2;
    std::fs::write(&h.path, &h.bytes[..cut]).unwrap();
    let (mut session, report) = Session::load(&h.path).unwrap();
    assert_eq!(report.valid_end as usize, checkpoints[2]);
    assert_eq!(report.ignored_bytes as usize, cut - checkpoints[2]);
    assert!(report.ignored_reason.is_some());
    assert_eq!(session.store(), &h.snapshots[2].0);
    let src = MemSource::new(vec![0; 512]);
    let protected = [];
    session
        .attach_file(&h.path, &guard_for(&src, &protected))
        .unwrap();
    session
        .append_opaque(OpaqueKind::Heads, b"heads".to_vec())
        .unwrap();
    session.checkpoint().unwrap();
    let bytes = std::fs::read(&h.path).unwrap();
    let new_blocks = blocks_of(&bytes);
    assert_eq!(new_blocks.last().unwrap().2, bytes.len());
    // Nothing of the torn tail is left: the new blocks start at the old valid end.
    assert!(new_blocks.iter().any(|b| b.0 == checkpoints[2]));
    assert_eq!(&bytes[..checkpoints[2]], &h.bytes[..checkpoints[2]]);
    let (loaded, report) = Session::load(&h.path).unwrap();
    assert_eq!(report.ignored_bytes, 0);
    assert_eq!(loaded.store(), &h.snapshots[2].0);
    assert_eq!(loaded.opaque(OpaqueKind::Heads), &[b"heads".to_vec()]);
}

#[test]
fn hostile_files_are_refused_cleanly() {
    let dir = TempDir::new("hostile");
    let path = dir.join("x.hds");
    // A block that claims 2^62 bytes: refused at once, nothing allocated by it.
    let mut bytes = b"HDSC".to_vec();
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&11u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&(1u64 << 62).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&[0; 64]);
    std::fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        Session::load(&path),
        Err(SessionError::Corrupt(_))
    ));
    // A newer format version.
    let mut newer = b"HDSC".to_vec();
    let next = file::FORMAT_VERSION + 1;
    newer.extend_from_slice(&next.to_le_bytes());
    std::fs::write(&path, &newer).unwrap();
    assert!(matches!(
        Session::load(&path),
        Err(SessionError::Unsupported(v)) if v == next
    ));
    assert_eq!(Session::load(&path).unwrap_err().code(), "unsupported");
    // An older version than any this code reads.
    let mut older = b"HDSC".to_vec();
    older.extend_from_slice(&0u32.to_le_bytes());
    std::fs::write(&path, &older).unwrap();
    assert!(matches!(
        Session::load(&path),
        Err(SessionError::Corrupt(_))
    ));
    // Not a session file, an empty file, a directory.
    std::fs::write(&path, b"hello world").unwrap();
    assert!(matches!(
        Session::load(&path),
        Err(SessionError::Corrupt(_))
    ));
    std::fs::write(&path, b"").unwrap();
    assert!(matches!(
        Session::load(&path),
        Err(SessionError::Corrupt(_))
    ));
    assert!(Session::load(&dir.0).is_err());
    // Random bytes behind a valid header, and valid blocks with random payloads
    // and correct checksums: refused or loaded, never a panic.
    let mut rng = XorShift::new(5);
    for round in 0..300 {
        let mut bytes = b"HDSC".to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        for _ in 0..1 + rng.below(4) {
            let ty = 1 + rng.below(12) as u32;
            let mut payload = vec![0u8; rng.below(600) as usize];
            rng.fill(&mut payload);
            if round % 3 == 0 && !payload.is_empty() {
                // Plausible small counts make the decoders go further.
                payload[0] = rng.below(4) as u8;
                for b in payload.iter_mut().skip(1).take(3) {
                    *b = 0;
                }
            }
            bytes.extend_from_slice(&ty.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
            bytes.extend_from_slice(&crc::crc32(&payload).to_le_bytes());
            bytes.extend_from_slice(&payload);
        }
        std::fs::write(&path, &bytes).unwrap();
        let _ = Session::load(&path);
    }
}

#[test]
fn mutated_valid_payloads_never_panic_and_are_refused_or_consistent() {
    // Valid blocks with one field changed and the checksum fixed up: the
    // decoders must validate, not trust the CRC.
    let dir = TempDir::new("mutate");
    let (mut session, src) = scanned();
    let protected = [Protected::session(0..src.size())];
    let path = dir.join("m.hds");
    session
        .save_as(&path, &guard_for(src.as_ref(), &protected))
        .unwrap();
    let original = std::fs::read(&path).unwrap();
    let blocks = blocks_of(&original);
    let mut rng = XorShift::new(1234);
    let target = dir.join("mutant.hds");
    for _ in 0..1500 {
        let mut bytes = original.clone();
        let &(start, _, end) = &blocks[rng.below(blocks.len() as u64) as usize];
        if end - start <= 20 {
            continue;
        }
        let at = start + 20 + rng.below((end - start - 20) as u64) as usize;
        bytes[at] = rng.next_u64() as u8;
        let crc = crc::crc32(&bytes[start + 20..end]);
        bytes[start + 16..start + 20].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(&target, &bytes).unwrap();
        if let Ok((loaded, _)) = Session::load(&target) {
            // Whatever was accepted is internally consistent: every record
            // decodes through the API without surprises.
            let store = loaded.store();
            for (id, r) in store.records() {
                assert!(r.record_size() >= 1024);
                for e in store.data_extents(id) {
                    if let crate::store::DataExtent::NonResident(e) = e {
                        let _ = e.runs().count();
                    }
                }
                for n in store.names(id) {
                    let _ = n.name.to_string_lossy();
                }
            }
        }
    }
}

/// Whatever a scan stores from damaged or hostile structures, its session
/// file loads back to the same store: the loader's checks never refuse the
/// scanner's own output.
#[test]
fn a_session_of_mutated_structures_always_loads_back() {
    let dir = TempDir::new("mutated-scan");
    let templates = [
        rich_file(10).build(),
        rich_file(11).size(4096).build(),
        Rec::new(12)
            .flags(3)
            .si([1, 2, 3, 4], 0)
            .name(fref(5, 5), "Dir", 1, 0x1000_0000)
            .index_root(&[index_entry(
                fref(20, 1),
                fref(12, 1),
                "Sub",
                1,
                0x1000_0000,
                [1, 2, 3, 4],
            )])
            .index_allocation(0, 0, 4096, &runlist(&[(Some(77), 1)]))
            .build(),
        Rec::new(13)
            .attr_list(&[
                (0x80, "", 0, fref(13, 1), 2),
                (0xA0, "$I30", 0, fref(14, 1), 0),
            ])
            .build(),
        indx_block(
            0,
            5,
            4096,
            &[index_entry(
                fref(30, 1),
                fref(12, 1),
                "Kid",
                1,
                0x1000_0000,
                [1, 2, 3, 4],
            )],
        ),
        boot_sector(512, 8, 100_000, 4, 2, -10, 1, 0),
    ];
    let mut rng = XorShift::new(2024);
    for round in 0..12 {
        let mut img = vec![0u8; 4 * MIB];
        let mut at = 0x1000;
        while at + 4096 < img.len() {
            let mut piece = templates[rng.below(templates.len() as u64) as usize].clone();
            for _ in 0..1 + rng.below(6) {
                let i = rng.below(piece.len() as u64) as usize;
                piece[i] = rng.next_u64() as u8;
            }
            put(&mut img, at, &piece);
            at += 512 * (2 + rng.below(9) as usize);
        }
        let src = mem(img);
        let range = 0..src.size();
        let mut session = Session::new(meta_for(
            src.as_ref(),
            range.clone(),
            ScanOptions::default(),
        ));
        let (summary, _) = scan_into(
            src.clone(),
            range.clone(),
            ScanOptions::default(),
            &ScanTuning::default(),
            &mut session,
            &AtomicBool::new(false),
        );
        assert_eq!(summary.status, ScanStatus::Completed);
        let protected = [Protected::session(range)];
        let path = dir.join(&format!("m{round}.hds"));
        session
            .save_as(&path, &guard_for(src.as_ref(), &protected))
            .unwrap();
        let (loaded, report) = Session::load(&path)
            .unwrap_or_else(|e| panic!("round {round}: the scanner's output was refused: {e}"));
        assert_eq!(report.ignored_bytes, 0);
        assert_eq!(loaded.store(), session.store(), "round {round}");
        assert!(session.store().record_count() > 100, "round {round}");
    }
}

#[test]
fn the_destination_guard_decides_where_a_session_may_go() {
    let dir = TempDir::new("guard");
    // A disk source: on this system its location cannot be resolved, so every
    // path is `unknown_disk` and needs that consent.
    let disk = MemSource::new(vec![0; 4 * MIB]).at_location(SourceLocation::Disk { number: 3 });
    let meta = meta_for(&disk, 0..4 * MIB as u64, ScanOptions::default());
    let protected = [Protected::session(0..4 * MIB as u64)];
    let path = dir.join("s.hds");
    // On Windows "disk 3" is resolved against the real disks of the machine: a
    // temporary directory on another disk is simply allowed. Only Unix, where a
    // disk number names nothing, is certain to answer `unknown_disk`.
    let refused = match Session::create(meta.clone(), &path, &guard_for(&disk, &protected)) {
        Err(refused) => refused,
        Ok(session) if cfg!(windows) => {
            drop(session);
            std::fs::remove_file(&path).unwrap();
            return;
        }
        Ok(_) => panic!("a disk that cannot be located must need consent"),
    };
    match &refused {
        SessionError::DestinationRefused(check) => {
            if !cfg!(windows) {
                assert_eq!(check.verdict, Verdict::UnknownDisk);
            }
        }
        other => panic!("{other}"),
    }
    assert_eq!(refused.code(), "destination_refused");
    assert!(!path.exists(), "nothing was written");
    if !cfg!(windows) {
        let consent = SessionGuard {
            allow_unknown_disk: true,
            ..guard_for(&disk, &protected)
        };
        Session::create(meta, &path, &consent).unwrap();
        assert!(path.exists());
    }
    // An image source: its own file is off limits whatever the consents.
    let image_path = dir.join("disk.img");
    std::fs::write(&image_path, vec![0u8; MIB]).unwrap();
    let image = ImageFile::open(&image_path, None).unwrap();
    let meta = meta_for(&image, 0..MIB as u64, ScanOptions::default());
    let protected = [Protected::session(0..MIB as u64)];
    let consent = SessionGuard {
        allow_same_disk: true,
        allow_unknown_disk: true,
        ..guard_for(&image, &protected)
    };
    match Session::create(meta.clone(), &image_path, &consent) {
        Err(SessionError::DestinationRefused(check)) => {
            assert_eq!(check.verdict, Verdict::OverlapsSource)
        }
        other => panic!("{other:?}"),
    }
    // Next to it is fine.
    let ok = dir.join("disk.hds");
    Session::create(meta, &ok, &consent).unwrap();
    drop(image);
    assert_eq!(std::fs::read(&image_path).unwrap(), vec![0u8; MIB]);
}

#[test]
fn a_file_that_becomes_protected_is_let_go() {
    let dir = TempDir::new("recheck");
    let disk = MemSource::new(vec![0; 4 * MIB]).at_location(SourceLocation::Disk { number: 1 });
    let meta = meta_for(&disk, 0..4 * MIB as u64, ScanOptions::default());
    let protected = [Protected::session(0..4 * MIB as u64)];
    let consent = SessionGuard {
        allow_unknown_disk: true,
        ..guard_for(&disk, &protected)
    };
    let path = dir.join("s.hds");
    let mut session = Session::create(meta, &path, &consent).unwrap();
    assert_eq!(session.last_file(), Some(path.as_path()));
    session.recheck_file(&consent).unwrap();
    assert!(session.has_file());
    // Without the consent (as after a catalog found the location at risk) the
    // file is detached; the session goes on in memory.
    let strict = guard_for(&disk, &protected);
    if !cfg!(windows) {
        assert!(matches!(
            session.recheck_file(&strict),
            Err(SessionError::DestinationRefused(_))
        ));
        assert!(!session.has_file());
        assert_eq!(session.last_file(), Some(path.as_path()));
        session.checkpoint().unwrap();
    }
}

#[test]
fn a_session_file_is_only_replaced_by_one_of_the_same_source_and_range() {
    let dir = TempDir::new("replace");
    let (mut session, src) = scanned();
    let protected = [Protected::session(0..src.size())];
    let guard = guard_for(src.as_ref(), &protected);
    // A file that is not a session file.
    let plain = dir.join("notes.txt");
    std::fs::write(&plain, b"my notes").unwrap();
    assert!(matches!(
        session.save_as(&plain, &guard),
        Err(SessionError::Occupied(_))
    ));
    assert_eq!(std::fs::read(&plain).unwrap(), b"my notes");
    // A session of another range of the same source.
    let other_path = dir.join("other.hds");
    let other_meta = meta_for(src.as_ref(), 0..CHUNK as u64, ScanOptions::default());
    Session::create(other_meta, &other_path, &guard).unwrap();
    let before = std::fs::read(&other_path).unwrap();
    let refused = session.save_as(&other_path, &guard).unwrap_err();
    assert!(matches!(refused, SessionError::Occupied(_)));
    assert_eq!(refused.code(), "exists");
    assert_eq!(std::fs::read(&other_path).unwrap(), before);
    // A session of another source with the same range.
    let foreign = MemSource::new(rich_image()).named("other-disk");
    let foreign_meta = meta_for(
        &foreign,
        session.meta().range.clone(),
        ScanOptions::default(),
    );
    let foreign_path = dir.join("foreign.hds");
    Session::create(foreign_meta, &foreign_path, &guard).unwrap();
    assert!(matches!(
        session.save_as(&foreign_path, &guard),
        Err(SessionError::Occupied(_))
    ));
    // A directory or a link under the name.
    // (The guard already refuses a directory where a session file should go.)
    std::fs::create_dir(dir.join("d.hds")).unwrap();
    assert!(matches!(
        session.save_as(&dir.join("d.hds"), &guard),
        Err(SessionError::Occupied(_) | SessionError::DestinationRefused(_))
    ));
    assert!(dir.join("d.hds").is_dir());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&plain, dir.join("link.hds")).unwrap();
        assert!(matches!(
            session.save_as(&dir.join("link.hds"), &guard),
            Err(SessionError::Occupied(_))
        ));
        assert_eq!(std::fs::read(&plain).unwrap(), b"my notes");
    }
    // A session of the same source and range is replaced, also our own file.
    let same = dir.join("same.hds");
    let mut older = Session::new(session.meta().clone());
    older.save_as(&same, &guard).unwrap();
    drop(older);
    session.save_as(&same, &guard).unwrap();
    session.save_as(&same, &guard).unwrap();
    let (loaded, _) = Session::load(&same).unwrap();
    assert_eq!(loaded.store(), session.store());
    // No temporary file is left behind.
    let names: Vec<String> = std::fs::read_dir(&dir.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.starts_with("~hds-")), "{names:?}");
}

#[test]
fn attaching_requires_the_file_the_session_came_from() {
    let dir = TempDir::new("attach");
    let (mut session, src) = scanned();
    let protected = [Protected::session(0..src.size())];
    let guard = guard_for(src.as_ref(), &protected);
    let path = dir.join("s.hds");
    session.save_as(&path, &guard).unwrap();
    let (mut loaded, _) = Session::load(&path).unwrap();
    // A fresh session has no file to continue.
    let mut fresh = Session::new(session.meta().clone());
    assert!(matches!(
        fresh.attach_file(&path, &guard),
        Err(SessionError::NoFile)
    ));
    // Another session file under that name is refused.
    let mut other = Session::new(session.meta().clone());
    other
        .append_opaque(OpaqueKind::Overrides, b"x".to_vec())
        .unwrap();
    let other_path = dir.join("o.hds");
    other.save_as(&other_path, &guard).unwrap();
    assert!(matches!(
        loaded.attach_file(&other_path, &guard),
        Err(SessionError::Occupied(_))
    ));
    // The right file is accepted and appended to (once the session that wrote it
    // has let go of it: on Windows a second writer gets a sharing violation).
    session.detach_file();
    loaded.attach_file(&path, &guard).unwrap();
    loaded
        .append_opaque(OpaqueKind::Overrides, b"geometry".to_vec())
        .unwrap();
    loaded.checkpoint().unwrap();
    let (again, report) = Session::load(&path).unwrap();
    assert_eq!(report.checkpoints, 3);
    assert_eq!(again.opaque(OpaqueKind::Overrides), &[b"geometry".to_vec()]);
    assert_eq!(again.store(), session.store());
    // Once the file has been changed behind its back, the session refuses it.
    #[cfg(unix)]
    {
        std::fs::write(&path, b"HDSC").unwrap();
        let mut stale = again;
        assert!(stale.attach_file(&path, &guard).is_err());
        loaded
            .append_opaque(OpaqueKind::Overrides, b"more".to_vec())
            .unwrap();
        assert!(
            loaded.checkpoint().is_err(),
            "the file is shorter than its last checkpoint"
        );
    }
}

#[test]
fn opaque_payloads_append_replace_and_survive_compaction() {
    let dir = TempDir::new("opaque");
    let src = MemSource::new(vec![0; MIB]);
    let protected = [];
    let guard = guard_for(&src, &protected);
    let meta = meta_for(&src, 0..MIB as u64, ScanOptions::default());
    let path = dir.join("o.hds");
    let mut s = Session::create(meta, &path, &guard).unwrap();
    s.append_opaque(OpaqueKind::Live, b"v1".to_vec()).unwrap();
    s.append_opaque(OpaqueKind::Heads, b"h1".to_vec()).unwrap();
    s.checkpoint().unwrap();
    s.append_opaque(OpaqueKind::Live, b"v2".to_vec()).unwrap();
    s.replace_opaque(OpaqueKind::Heads, vec![b"h2".to_vec(), b"h3".to_vec()])
        .unwrap();
    s.checkpoint().unwrap();
    let (l, _) = Session::load(&path).unwrap();
    assert_eq!(
        l.opaque(OpaqueKind::Live),
        &[b"v1".to_vec(), b"v2".to_vec()]
    );
    assert_eq!(
        l.opaque(OpaqueKind::Heads),
        &[b"h2".to_vec(), b"h3".to_vec()]
    );
    s.replace_opaque(OpaqueKind::Live, vec![]).unwrap();
    s.append_opaque(OpaqueKind::Overrides, b"o".to_vec())
        .unwrap();
    // Not yet written: the file still has the old lists.
    let (l, _) = Session::load(&path).unwrap();
    assert_eq!(l.opaque(OpaqueKind::Live).len(), 2);
    s.checkpoint().unwrap();
    let (l, _) = Session::load(&path).unwrap();
    assert!(l.opaque(OpaqueKind::Live).is_empty());
    assert_eq!(l.opaque(OpaqueKind::Overrides), &[b"o".to_vec()]);
    // A compacted copy holds the same lists.
    let copy = dir.join("c.hds");
    s.save_as(&copy, &guard).unwrap();
    let (c, report) = Session::load(&copy).unwrap();
    assert_eq!(report.checkpoints, 2);
    for kind in OpaqueKind::ALL {
        assert_eq!(c.opaque(kind), s.opaque(kind), "{kind:?}");
    }
    // The checkpoint after save_as appends to the copy, not to the old file.
    let old_len = std::fs::metadata(&path).unwrap().len();
    s.append_opaque(OpaqueKind::Carved, b"carved".to_vec())
        .unwrap();
    s.checkpoint().unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), old_len);
    let (c, _) = Session::load(&copy).unwrap();
    assert_eq!(c.opaque(OpaqueKind::Carved), &[b"carved".to_vec()]);
}

#[test]
fn checkpoints_during_a_scan_hold_everything_and_the_scan_resumes_from_the_file() {
    let dir = TempDir::new("resume");
    let (faulty, src) = rich_source();
    let range = 0..src.size();
    let protected = [Protected::session(range.clone())];
    let guard = guard_for(src.as_ref(), &protected);
    let path = dir.join("r.hds");
    let mut session = Session::create(
        meta_for(src.as_ref(), range.clone(), ScanOptions::default()),
        &path,
        &guard,
    )
    .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    faulty.cancel_after_reads(7, cancel.clone());
    let tuning = ScanTuning {
        checkpoint_interval: std::time::Duration::ZERO,
        ..ScanTuning::default()
    };
    let (summary, _) = scan_into(
        src.clone(),
        range.clone(),
        ScanOptions::default(),
        &tuning,
        &mut session,
        &cancel,
    );
    assert_eq!(summary.status, ScanStatus::Stopped);
    let stopped = session.store().clone();
    drop(session);
    // What the file holds is exactly what was committed.
    let (mut loaded, report) = Session::load(&path).unwrap();
    assert_eq!(loaded.store(), &stopped);
    assert!(report.checkpoints >= 3);
    // Resume from the file.
    faulty.reset_counters();
    cancel.store(false, Ordering::SeqCst);
    loaded.attach_file(&path, &guard).unwrap();
    let (summary, _) = scan_into(
        src.clone(),
        range.clone(),
        ScanOptions::default(),
        &tuning,
        &mut loaded,
        &cancel,
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    let (fresh, _) = scanned();
    assert_eq!(loaded.store(), fresh.store());
    let (reloaded, _) = Session::load(&path).unwrap();
    assert_eq!(reloaded.store(), fresh.store());
}

#[test]
fn source_lost_still_saves_the_session() {
    let dir = TempDir::new("lost");
    let (faulty, src) = rich_source();
    let range = 0..src.size();
    let protected = [Protected::session(range.clone())];
    let path = dir.join("l.hds");
    let mut session = Session::create(
        meta_for(src.as_ref(), range.clone(), ScanOptions::default()),
        &path,
        &guard_for(src.as_ref(), &protected),
    )
    .unwrap();
    // Chunk 0 is read last; the chunks before it are kept.
    faulty.fail_range(0x1000..0x1200, hds_io::ReadFailure::DeviceLost);
    let (summary, _) = scan_into(
        src,
        range,
        ScanOptions::default(),
        &ScanTuning::default(),
        &mut session,
        &AtomicBool::new(false),
    );
    assert!(matches!(
        summary.status,
        ScanStatus::Failed(crate::scan::ScanFailure::SourceLost(_))
    ));
    assert!(summary.checkpoint_error.is_none());
    let (loaded, _) = Session::load(&path).unwrap();
    assert_eq!(loaded.store(), session.store());
    assert!(!loaded.store().done().is_empty());
}

/// A source reporting a model and serial.
struct Labelled {
    inner: MemSource,
    model: Option<String>,
    serial: Option<String>,
}

impl BlockSource for Labelled {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }
    fn physical_sector_size(&self) -> u32 {
        self.inner.physical_sector_size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn info(&self) -> SourceInfo {
        SourceInfo {
            kind: hds_io::SourceKind::Disk,
            model: self.model.clone(),
            serial: self.serial.clone(),
            ..self.inner.info()
        }
    }
    fn is_alive(&self) -> bool {
        true
    }
}

/// An image with `n` records at known offsets (one per 64 KiB).
fn records_image(n: usize) -> Vec<u8> {
    let mut img = vec![0u8; 16 * MIB];
    for i in 0..n {
        put(
            &mut img,
            0x1_0000 * (i + 1),
            &rich_file(i as u32 + 40).build(),
        );
    }
    img
}

fn session_of(img: &[u8]) -> Session {
    let src = mem(img.to_vec());
    let range = 0..img.len() as u64;
    let mut session = Session::new(meta_for(
        src.as_ref(),
        range.clone(),
        ScanOptions::default(),
    ));
    let (summary, _) = scan_into(
        src,
        range,
        ScanOptions::default(),
        &ScanTuning::default(),
        &mut session,
        &AtomicBool::new(false),
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    session
}

#[test]
fn the_identity_check_follows_b4() {
    let img = records_image(100);
    let session = session_of(&img);
    let none = |_: crate::store::RecordId, _: &crate::store::RawRecord| false;
    let cancel = AtomicBool::new(false);
    // The same bytes match, with 64 samples spread over the range.
    let check = check_identity(&session, mem(img.clone()), &none, false, &cancel).unwrap();
    assert!(check.matches, "{check:?}");
    assert_eq!(
        (check.samples, check.matched, check.mismatched),
        (64, 64, 0)
    );
    assert!(check.warnings.is_empty());
    // Another disk: nothing matches.
    let other = records_image(0);
    let check = check_identity(&session, mem(other), &none, true, &cancel).unwrap();
    assert!(!check.matches);
    assert_eq!(check.failed, vec!["records"]);
    // A modified disk: 10 % of the records changed still pass, 20 % do not.
    let changed = |fraction: usize| {
        let mut img = img.clone();
        for i in (0..100).filter(|i| i % fraction == 0) {
            img[0x1_0000 * (i + 1) + 0x200] ^= 0xFF;
        }
        img
    };
    let check = check_identity(&session, mem(changed(20)), &none, false, &cancel).unwrap();
    assert!(check.matches, "{check:?}");
    assert!(check.mismatched > 0);
    let check = check_identity(&session, mem(changed(3)), &none, true, &cancel).unwrap();
    assert!(!check.matches, "{check:?}");
    // A source too small for the range.
    let check =
        check_identity(&session, mem(img[..8 * MIB].to_vec()), &none, true, &cancel).unwrap();
    assert!(!check.matches && !check.size_ok);
    assert_eq!(check.failed, vec!["size"]);
    // Records the caller excludes are not sampled.
    let even = |_: crate::store::RecordId, r: &crate::store::RawRecord| {
        r.record_number().unwrap().is_multiple_of(2)
    };
    let check = check_identity(&session, mem(img.clone()), &even, false, &cancel).unwrap();
    assert_eq!(check.samples, 50);
    // Unreadable samples count neither way.
    let faulty = Arc::new(FaultySource::new(mem(img.clone())));
    faulty.bad_sectors(0x1_0000..0x1_0200);
    let check = check_identity(&session, faulty, &none, false, &cancel).unwrap();
    assert!(check.matches);
    assert_eq!(check.unreadable, 1);
    // A lost device is an error, not a mismatch.
    let faulty = Arc::new(FaultySource::new(mem(img.clone())));
    faulty.lose_after_reads(0);
    assert!(matches!(
        check_identity(&session, faulty, &none, false, &cancel),
        Err(IdentityError::SourceLost(_))
    ));
}

#[test]
fn force_waives_only_the_minimum_number_of_samples() {
    let img = records_image(5);
    let session = session_of(&img);
    let none = |_: crate::store::RecordId, _: &crate::store::RawRecord| false;
    let cancel = AtomicBool::new(false);
    let check = check_identity(&session, mem(img.clone()), &none, false, &cancel).unwrap();
    assert!(!check.matches);
    assert_eq!(check.failed, vec!["too_few_samples"]);
    let check = check_identity(&session, mem(img.clone()), &none, true, &cancel).unwrap();
    assert!(check.matches);
    // Mismatching samples are refused even with force.
    let mut changed = img.clone();
    changed[0x1_0000 + 0x200] ^= 1;
    let check = check_identity(&session, mem(changed), &none, true, &cancel).unwrap();
    assert!(!check.matches);
    assert_eq!(check.failed, vec!["records"]);
}

#[test]
fn model_and_serial_differences_are_warnings() {
    let img = records_image(20);
    let labelled = |serial: &str| Labelled {
        inner: MemSource::new(img.clone()),
        model: Some("WDC WD40".into()),
        serial: Some(serial.into()),
    };
    let src = labelled("WD-1");
    let range = 0..img.len() as u64;
    let mut session = Session::new(meta_for(&src, range.clone(), ScanOptions::default()));
    let src: Arc<dyn BlockSource> = Arc::new(src);
    let (summary, _) = scan_into(
        src.clone(),
        range,
        ScanOptions::default(),
        &ScanTuning::default(),
        &mut session,
        &AtomicBool::new(false),
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    let none = |_: crate::store::RecordId, _: &crate::store::RawRecord| false;
    let cancel = AtomicBool::new(false);
    let check = check_identity(&session, src, &none, false, &cancel).unwrap();
    assert!(check.matches && check.warnings.is_empty());
    let check =
        check_identity(&session, Arc::new(labelled("WD-2")), &none, false, &cancel).unwrap();
    assert!(check.matches);
    assert_eq!(check.warnings.len(), 1);
    assert_eq!(check.warnings[0].code, WARN_IDENTITY_DIFFERS);
    // The new identity can be recorded; kind and size cannot change.
    let mut identity = session.meta().identity.clone();
    identity.serial = Some("WD-2".into());
    identity.path = r"\\.\PhysicalDrive7".into();
    session.set_identity(identity.clone()).unwrap();
    assert_eq!(session.meta().identity, identity);
    identity.size += 512;
    assert!(session.set_identity(identity).is_err());
}

#[test]
fn session_meta_is_validated() {
    let src = MemSource::new(vec![0; 4096]);
    let id = SourceIdentity::from(&src.info());
    let ok = ScanOptions::default();
    assert!(SessionMeta::new(id.clone(), 0..4096, ok).is_ok());
    assert!(SessionMeta::new(id.clone(), 0..0, ok).is_err());
    assert!(SessionMeta::new(id.clone(), 0..8192, ok).is_err());
    assert!(SessionMeta::new(id.clone(), 100..4096, ok).is_err());
    let bad = ScanOptions {
        max_record_size: 3000,
        ..ok
    };
    assert!(SessionMeta::new(id.clone(), 0..4096, bad).is_err());
    let mut odd = id;
    odd.sector_size = 1000;
    assert!(SessionMeta::new(odd, 0..4000, ok).is_err());
}

/// Turns a session file of the current format into one of format version 1:
/// the header says 1 and every record loses its `$FILE_NAME` time.
fn downgrade_to_v1(bytes: &[u8]) -> Vec<u8> {
    let mut out = b"HDSC".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    for (pos, ty, end) in blocks_of(bytes) {
        let header = &bytes[pos..pos + 20];
        let payload = &bytes[pos + 20..end];
        let payload = if ty == 1 {
            let n = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
            let mut p = payload[..4].to_vec();
            for i in 0..n {
                let rec = &payload[4 + i * codec::RECORD_WIRE..4 + (i + 1) * codec::RECORD_WIRE];
                // Offset, LSN, hash, base and the four $SI times come first.
                p.extend_from_slice(&rec[..64]);
                p.extend_from_slice(&rec[72..]);
            }
            p.extend_from_slice(&payload[4 + n * codec::RECORD_WIRE..]);
            p
        } else {
            payload.to_vec()
        };
        out.extend_from_slice(&header[..8]);
        out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        out.extend_from_slice(&super::crc::crc32(&payload).to_le_bytes());
        out.extend_from_slice(&payload);
    }
    out
}

#[test]
fn a_version_1_file_loads_and_is_rewritten_before_it_is_appended_to() {
    let dir = TempDir::new("v1");
    let (mut session, src) = scanned();
    let protected = [Protected::session(0..src.size())];
    let guard = guard_for(src.as_ref(), &protected);
    let path = dir.join("v2.hds");
    session.save_as(&path, &guard).unwrap();
    session.detach_file();
    let v1 = dir.join("v1.hds");
    std::fs::write(&v1, downgrade_to_v1(&std::fs::read(&path).unwrap())).unwrap();
    let (mut loaded, report) = Session::load(&v1).unwrap();
    assert_eq!(report.ignored_bytes, 0);
    // Everything but the $FILE_NAME times of the records is there; the
    // record time falls back to the $STANDARD_INFORMATION MFT-change time.
    let (a, b) = (session.store(), loaded.store());
    assert_eq!(a.record_count(), b.record_count());
    assert_eq!(a.index_entry_count(), b.index_entry_count());
    let mut with_names = 0;
    for (&ia, &ib) in a.records_by_offset().iter().zip(b.records_by_offset()) {
        let (ra, rb) = (a.record(ia).unwrap(), b.record(ib).unwrap());
        assert_eq!(ra.disk_offset(), rb.disk_offset());
        assert_eq!(ra.standard_information(), rb.standard_information());
        assert_eq!(rb.latest_name_time(), None);
        assert_eq!(rb.record_time(), rb.mft_changed());
        if ra.latest_name_time().is_some() {
            with_names += 1;
        }
    }
    assert!(with_names > 0);
    // The approximate times are announced, and only for the old format.
    let has_warning = |s: &Session| {
        s.store()
            .warnings()
            .iter()
            .any(|w| w.code == WARN_OLD_SESSION_FORMAT)
    };
    assert!(has_warning(&loaded));
    assert!(!has_warning(&session));
    assert!(!has_warning(&Session::load(&path).unwrap().0));
    // Going on in the file rewrites it in the current format first.
    loaded.attach_file(&v1, &guard).unwrap();
    loaded
        .append_opaque(OpaqueKind::Overrides, b"geometry".to_vec())
        .unwrap();
    loaded.checkpoint().unwrap();
    let bytes = std::fs::read(&v1).unwrap();
    assert_eq!(&bytes[4..8], &file::FORMAT_VERSION.to_le_bytes());
    // The file in the earlier format is kept next to it.
    let kept = std::fs::read(dir.join("v1.hds.v1.bak")).unwrap();
    assert_eq!(&kept[4..8], &1_u32.to_le_bytes());
    let (again, _) = Session::load(&v1).unwrap();
    assert_eq!(again.opaque(OpaqueKind::Overrides), &[b"geometry".to_vec()]);
    assert_eq!(again.store(), loaded.store());
    // The rewritten file still says its times came from format 1.
    assert!(has_warning(&again));
    assert_eq!(records_with_name_times(&again), 0);
}

#[test]
fn the_rewrite_of_a_version_1_file_never_leaves_no_valid_session_file() {
    use super::{FaultMode, RewriteStep, SimulatedCrash, REWRITE_FAULTS};
    use FaultMode::{Crash, Error};
    use RewriteStep::{CreateTemp, KeepOld, NameNew, Reopen, RestoreOld, WriteCopy};

    let work = TempDir::new("v1-fault-src");
    let (mut session, src) = scanned();
    let protected = [Protected::session(0..src.size())];
    let guard = guard_for(src.as_ref(), &protected);
    let v2 = work.join("v2.hds");
    session.save_as(&v2, &guard).unwrap();
    session.detach_file();
    let v1_bytes = downgrade_to_v1(&std::fs::read(&v2).unwrap());
    let expected = (
        session.store().record_count(),
        session.store().index_entry_count(),
    );
    let version_of = |bytes: &[u8]| u32::from_le_bytes(bytes[4..8].try_into().unwrap());

    let mut cases: Vec<Vec<(RewriteStep, FaultMode)>> = Vec::new();
    for step in [CreateTemp, WriteCopy, KeepOld, NameNew, Reopen] {
        for mode in [Error, Crash] {
            cases.push(vec![(step, mode)]);
        }
    }
    cases.push(vec![(NameNew, Error), (RestoreOld, Error)]);
    cases.push(vec![(NameNew, Error), (RestoreOld, Crash)]);

    for faults in cases {
        let dir = TempDir::new("v1-fault");
        let path = dir.join("s.hds");
        std::fs::write(&path, &v1_bytes).unwrap();
        let (mut loaded, _) = Session::load(&path).unwrap();
        REWRITE_FAULTS.with(|f| *f.borrow_mut() = faults.clone());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            loaded.attach_file(&path, &guard)
        }));
        let unused = REWRITE_FAULTS.with(|f| std::mem::take(&mut *f.borrow_mut()));
        assert!(
            unused.is_empty(),
            "{faults:?}: step not reached: {unused:?}"
        );
        let crashed = match result {
            Err(payload) => {
                assert!(payload.is::<SimulatedCrash>(), "{faults:?}: a real panic");
                true
            }
            Ok(result) => {
                assert!(result.is_err(), "{faults:?}: the failure was not reported");
                false
            }
        };

        // Every complete session file in the folder, with its version.
        let mut names = Vec::new();
        let mut valid = Vec::new();
        for entry in std::fs::read_dir(&dir.0).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            names.push(name.clone());
            if let Ok((s, report)) = Session::load(&entry.path()) {
                let counts = (s.store().record_count(), s.store().index_entry_count());
                if counts == expected && report.ignored_bytes == 0 {
                    let bytes = std::fs::read(entry.path()).unwrap();
                    valid.push((name, version_of(&bytes)));
                }
            }
        }
        names.sort();
        assert!(
            !valid.is_empty(),
            "{faults:?}: no valid session file left in {names:?}"
        );
        // The file in the earlier format survives unchanged under one name
        // or the other.
        let original = ["s.hds", "s.hds.v1.bak"]
            .iter()
            .any(|n| std::fs::read(dir.join(n)).is_ok_and(|b| b == v1_bytes));
        assert!(
            original,
            "{faults:?}: the version-1 file is gone: {names:?}"
        );

        let first = faults[0];
        if crashed {
            // A later run finds either the old file under its own name, or
            // (crash between the renames) the old file kept and the copy.
            if first.0 == NameNew || first.0 == RestoreOld || faults.len() == 2 {
                assert!(valid.iter().any(|(n, v)| n == "s.hds.v1.bak" && *v == 1));
                assert!(valid
                    .iter()
                    .any(|(n, v)| n.starts_with("~hds-session-") && *v == file::FORMAT_VERSION));
            } else if first.0 == Reopen {
                assert!(valid.contains(&("s.hds".to_string(), file::FORMAT_VERSION)));
            } else {
                assert!(valid.contains(&("s.hds".to_string(), 1)));
            }
            continue;
        }
        match (first.0, faults.len()) {
            (Reopen, _) => {
                // Rewritten and renamed; only reopening failed: attaching
                // again continues the new file.
                assert_eq!(names, ["s.hds", "s.hds.v1.bak"]);
                assert!(valid.contains(&("s.hds".to_string(), file::FORMAT_VERSION)));
                assert!(!loaded.has_file());
                loaded.attach_file(&path, &guard).unwrap();
                loaded.checkpoint().unwrap();
                assert_eq!(Session::load(&path).unwrap().0.store(), loaded.store());
            }
            (NameNew, 2) => {
                // Neither rename worked: the session goes on in the copy
                // under its temporary name.
                assert_eq!(names.len(), 2, "{names:?}");
                let temp = loaded.file_path().unwrap().to_path_buf();
                assert!(temp
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("~hds-session-"));
                loaded.checkpoint().unwrap();
                assert_eq!(Session::load(&temp).unwrap().0.store(), loaded.store());
            }
            _ => {
                // Everything was put back as it was.
                assert_eq!(names, ["s.hds"], "{faults:?}");
                assert_eq!(std::fs::read(&path).unwrap(), v1_bytes);
                assert!(!loaded.has_file());
                loaded.attach_file(&path, &guard).unwrap();
                assert!(loaded.has_file());
            }
        }
    }

    // Without faults, a taken backup name is not replaced.
    let dir = TempDir::new("v1-fault");
    let path = dir.join("s.hds");
    std::fs::write(&path, &v1_bytes).unwrap();
    std::fs::write(dir.join("s.hds.v1.bak"), b"someone else's").unwrap();
    let (mut loaded, _) = Session::load(&path).unwrap();
    loaded.attach_file(&path, &guard).unwrap();
    assert_eq!(
        std::fs::read(dir.join("s.hds.v1.bak")).unwrap(),
        b"someone else's"
    );
    assert_eq!(std::fs::read(dir.join("s.hds.v1-2.bak")).unwrap(), v1_bytes);
    assert_eq!(
        version_of(&std::fs::read(&path).unwrap()),
        file::FORMAT_VERSION
    );
    let warning = loaded
        .store()
        .warnings()
        .iter()
        .find(|w| w.code == WARN_OLD_SESSION_FORMAT)
        .unwrap();
    assert!(
        warning.message.contains("s.hds.v1-2.bak"),
        "{}",
        warning.message
    );
}

/// Records of `s` that carry a `$FILE_NAME` time (none after a version-1
/// file: the rewrite cannot invent them).
fn records_with_name_times(s: &Session) -> usize {
    let store = s.store();
    store
        .records_by_offset()
        .iter()
        .filter(|&&id| store.record(id).unwrap().latest_name_time().is_some())
        .count()
}

#[test]
fn wire_sizes_match_the_encoders() {
    let (session, _) = scanned();
    let store = session.store();
    let one = store.records().next().unwrap().0;
    let mut enc = codec::RecordsEncoder::default();
    let region = store.aux_region(one);
    enc.push(store.record(one).unwrap(), region);
    assert_eq!(
        enc.finish().len(),
        4 + codec::RECORD_WIRE + 8 + region.len()
    );
    let e = codec::encode_entries(store, std::iter::once(crate::store::EntryId(0)));
    let name = store.entry_raw(crate::store::EntryId(0)).unwrap().1.len();
    assert_eq!(e.len(), 4 + 47 + 8 + name);
    assert_eq!(
        codec::encode_blocks(store.index_blocks().iter().take(1)).len(),
        4 + 39
    );
    assert_eq!(
        codec::encode_boots(store.boot_hits().iter().take(1)).len(),
        4 + 64
    );
}

/// The production code of this crate creates, writes, renames and deletes
/// files only through `hds_io::out` (S6).
#[test]
fn no_file_is_written_outside_hds_io_out() {
    fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let forbidden = [
        "File::create",
        "OpenOptions",
        "fs::write",
        "fs::rename",
        "fs::remove",
        "fs::create_dir",
        "fs::copy",
        "fs::hard_link",
        "set_permissions",
        "temp_dir",
    ];
    for file in files {
        let name = file.to_string_lossy().replace('\\', "/");
        if name.ends_with("/tests.rs") || name.ends_with("/test_build.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for word in forbidden {
            assert!(!text.contains(word), "{name} uses {word}");
        }
    }
}
