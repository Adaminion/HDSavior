//! Tests of the scanner with hand-built images and misbehaving sources.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hds_io::testing::{FaultySource, MemSource};
use hds_io::{
    check_read_request, BlockSource, RangeSet, RangedSource, ReadFailure, SourceInfo, SourceKind,
    Warning,
};
use hds_ntfs::{FileRef, FixupStatus, Namespace, RunlistError, RunlistStatus};

use super::*;
use crate::session::{Session, SessionMeta, SourceIdentity};
use crate::store::{
    AttrListName, AttributeListState, DataExtent, EntrySourceKind, RecordId, Reparse, ScanStore,
};
use crate::test_build::*;

pub(crate) const MIB: usize = 1 << 20;
pub(crate) const CHUNK: usize = CHUNK_SIZE as usize;

pub(crate) fn put(img: &mut [u8], at: usize, bytes: &[u8]) {
    img[at..at + bytes.len()].copy_from_slice(bytes);
}

/// Records progress events with their time, warnings and checkpoint failures.
#[derive(Default)]
pub(crate) struct Recorder {
    pub events: Vec<(Instant, ScanProgress)>,
    pub warnings: Vec<Warning>,
    pub checkpoint_failures: usize,
}

impl ScanObserver for Recorder {
    fn progress(&mut self, progress: &ScanProgress) {
        self.events.push((Instant::now(), progress.clone()));
    }
    fn warning(&mut self, warning: &Warning) {
        self.warnings.push(warning.clone());
    }
    fn checkpoint_failed(&mut self, _error: &crate::session::SessionError) {
        self.checkpoint_failures += 1;
    }
}

pub(crate) fn meta_for(
    src: &dyn BlockSource,
    range: Range<u64>,
    options: ScanOptions,
) -> SessionMeta {
    SessionMeta::new(SourceIdentity::from(&src.info()), range, options).unwrap()
}

/// Scans `range` of `src` into a fresh in-memory session.
pub(crate) fn scan_fresh(
    src: Arc<dyn BlockSource>,
    range: Range<u64>,
    options: ScanOptions,
    tuning: &ScanTuning,
) -> (Session, ScanSummary, Recorder) {
    let mut session = Session::new(meta_for(src.as_ref(), range.clone(), options));
    let (summary, recorder) = scan_into(
        src,
        range,
        options,
        tuning,
        &mut session,
        &AtomicBool::new(false),
    );
    (session, summary, recorder)
}

pub(crate) fn scan_into(
    src: Arc<dyn BlockSource>,
    range: Range<u64>,
    options: ScanOptions,
    tuning: &ScanTuning,
    session: &mut Session,
    cancel: &AtomicBool,
) -> (ScanSummary, Recorder) {
    let ranged = RangedSource::new(src, range).unwrap();
    let mut recorder = Recorder::default();
    let summary = scan_with(&ranged, &options, session, &mut recorder, cancel, tuning);
    (summary, recorder)
}

fn whole(img: &[u8]) -> Range<u64> {
    0..img.len() as u64
}

pub(crate) fn mem(img: Vec<u8>) -> Arc<dyn BlockSource> {
    Arc::new(MemSource::new(img))
}

fn quick() -> ScanTuning {
    ScanTuning::default()
}

/// A source of zeros of any size.
struct ZeroSource {
    size: u64,
}

impl BlockSource for ZeroSource {
    fn size(&self) -> u64 {
        self.size
    }
    fn sector_size(&self) -> u32 {
        512
    }
    fn physical_sector_size(&self) -> u32 {
        512
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        check_read_request(self.size, 512, offset, buf.len())?;
        buf.fill(0);
        Ok(())
    }
    fn info(&self) -> SourceInfo {
        SourceInfo {
            kind: SourceKind::Image,
            path: "zeros".into(),
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

fn record_ids(store: &ScanStore) -> Vec<(u64, Option<u32>)> {
    store
        .records_by_offset()
        .iter()
        .map(|&id| {
            let r = store.record(id).unwrap();
            (r.disk_offset(), r.record_number())
        })
        .collect()
}

// ------------------------------------------------------------------ records

/// A file record with everything A4 stores.
pub(crate) fn rich_file(number: u32) -> Rec {
    Rec::new(number)
        .seq(3)
        .lsn(0x1234_5678)
        .links(2)
        .si([10, 20, 30, 40], 0x20)
        .name(fref(5, 5), "PHOTO~1.JPG", 2, 0x20)
        .name(fref(5, 5), "Photo.jpg", 1, 0x20)
        .data_extent(
            0,
            9,
            (40960, 40000, 39000),
            &runlist(&[(Some(1000), 4), (None, 2), (Some(2000), 4)]),
            0x8000,
        )
        .named_data("WofCompressedData", b"compressed bytes")
        .reparse(0x8000_0017)
}

#[test]
fn every_field_of_a4_is_stored() {
    let mut img = vec![0u8; 2 * MIB];
    let file = rich_file(100).build();
    put(&mut img, 0x10_0000, &file);
    let dir = Rec::new(101)
        .flags(3)
        .si([1, 2, 3, 4], 0)
        .name(fref(5, 5), "Caf\u{e9}", 1, 0x1000_0000)
        .name(fref(5, 5), "\u{65e5}\u{672c}", 0, 0x1000_0000)
        .index_root(&[
            index_entry(
                fref(200, 2),
                fref(101, 1),
                "sub",
                1,
                0x1000_0000,
                [5, 6, 7, 8],
            ),
            index_entry(
                fref(201, 1),
                fref(101, 1),
                "file.txt",
                1,
                0x20,
                [5, 6, 7, 8],
            ),
        ])
        .index_allocation(0, 3, 16384, &runlist(&[(Some(500), 4)]))
        .build();
    put(&mut img, 0x10_0400, &dir);
    let ext = Rec::new(102)
        .base(fref(100, 3))
        .links(0)
        .data_extent(10, 19, (0, 0, 0), &runlist(&[(Some(3000), 10)]), 0)
        .build();
    put(&mut img, 0x10_0800, &ext);
    let listed = Rec::new(103)
        .si([1, 1, 1, 1], 0)
        .attr_list(&[
            (0x30, "", 0, fref(103, 1), 1),
            (0x80, "", 0, fref(103, 1), 2),
            (0x80, "", 10, fref(104, 1), 0),
            (0x80, "ads", 0, fref(103, 1), 5),
            (0xA0, "$I30", 0, fref(103, 1), 3),
        ])
        .resident_data(b"hello")
        .build();
    put(&mut img, 0x10_0C00, &listed);
    let old = Rec::new(0)
        .unnumbered()
        .si([9, 9, 9, 9], 0)
        .resident_data(b"x")
        .build();
    put(&mut img, 0x10_1000 + 512, &old);

    let (session, summary, _) = scan_fresh(
        mem(img),
        0..2 * MIB as u64,
        ScanOptions::default(),
        &quick(),
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    let store = session.store();
    assert_eq!(
        record_ids(store),
        vec![
            (0x10_0000, Some(100)),
            (0x10_0400, Some(101)),
            (0x10_0800, Some(102)),
            (0x10_0C00, Some(103)),
            (0x10_1200, None)
        ]
    );
    let id = store.record_at(0x10_0000).unwrap();
    let r = store.record(id).unwrap();
    assert_eq!(r.record_size(), 1024);
    assert_eq!(r.sequence(), 3);
    assert_eq!(r.header_flags(), 1);
    assert!(r.is_in_use() && !r.is_directory() && !r.is_extension());
    assert_eq!(r.link_count(), 2);
    assert_eq!(r.lsn(), 0x1234_5678);
    assert_eq!(r.base(), FileRef::ZERO);
    assert_eq!(r.hash(), hds_ntfs::fnv1a64(&file));
    assert_eq!(r.fixup(), FixupStatus::Ok);
    let si = r.standard_information().unwrap();
    assert_eq!(
        (
            si.times.created,
            si.times.modified,
            si.times.mft_changed,
            si.times.accessed
        ),
        (10, 20, 30, 40)
    );
    assert_eq!(si.flags, 0x20);
    assert_eq!(r.mft_changed(), Some(30));
    assert!(r.has_wof_stream());
    assert_eq!(r.reparse(), Reparse::Tag(0x8000_0017));
    assert_eq!(r.attribute_list(), AttributeListState::Absent);
    assert!(!r.damage().any());
    let names: Vec<_> = store.names(id).collect();
    assert_eq!(names.len(), 2);
    assert!(names[0].name.eq_str("PHOTO~1.JPG"));
    assert_eq!(names[0].namespace, Namespace::Dos);
    assert_eq!(names[1].name.to_string_lossy(), "Photo.jpg");
    assert_eq!(names[1].namespace, Namespace::Win32);
    assert_eq!(names[1].parent, FileRef::new(5, 5));
    assert_eq!(names[1].flags, 0x20);
    assert_eq!(names[0].attr_id, 1);
    assert_eq!(names[1].attr_id, 2);
    let data: Vec<_> = store.data_extents(id).collect();
    assert_eq!(data.len(), 1);
    let DataExtent::NonResident(extent) = data[0] else {
        panic!("{data:?}")
    };
    assert_eq!(extent.start_vcn, 0);
    assert_eq!(extent.last_vcn, 9);
    let sizes = extent.sizes.unwrap();
    assert_eq!(
        (sizes.allocated, sizes.real, sizes.initialized),
        (40960, 40000, 39000)
    );
    assert_eq!(extent.flags, 0x8000);
    assert!(extent.has_sparse_flag() && !extent.is_compressed() && !extent.is_encrypted());
    assert_eq!(extent.attr_id, 3);
    assert_eq!(extent.runlist_status, RunlistStatus::Complete);
    let runs: Vec<_> = extent.runs().map(|r| (r.lcn, r.length)).collect();
    assert_eq!(runs, vec![(Some(1000), 4), (None, 2), (Some(2000), 4)]);
    assert_eq!(extent.clusters(), 10);
    assert_eq!(store.index_allocation_extents(id).count(), 0);

    // The directory: names in one-byte and in UTF-16 form, IA extent, root entries.
    let id = store.record_at(0x10_0400).unwrap();
    let r = store.record(id).unwrap();
    assert!(r.is_directory());
    let names: Vec<_> = store.names(id).collect();
    assert_eq!(names[0].name.to_string_lossy(), "Caf\u{e9}");
    assert_eq!(names[1].name.to_string_lossy(), "\u{65e5}\u{672c}");
    assert!(names.iter().all(|n| n.is_directory()));
    let ia: Vec<_> = store.index_allocation_extents(id).collect();
    assert_eq!(ia.len(), 1);
    assert_eq!(
        ia[0].runs().map(|r| (r.lcn, r.length)).collect::<Vec<_>>(),
        vec![(Some(500), 4)]
    );
    assert_eq!(ia[0].sizes.unwrap().allocated, 16384);
    // Only the directory entry of the root is stored, with the record as source.
    let entries: Vec<_> = store.index_entries().map(|(_, e)| e).collect();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.child, FileRef::new(200, 2));
    assert_eq!(e.parent, FileRef::new(101, 1));
    assert!(e.name.eq_str("sub"));
    assert_eq!(e.namespace, Namespace::Win32);
    assert_eq!(e.mft_changed, 7);
    assert_eq!(e.source.offset, 0x10_0400);
    assert_eq!(e.source.lsn, r.lsn());
    assert_eq!(e.source.kind, EntrySourceKind::RecordRoot);
    assert!(!e.source.torn);

    // The extension record: a later extent without sizes.
    let id = store.record_at(0x10_0800).unwrap();
    let r = store.record(id).unwrap();
    assert!(r.is_extension());
    assert_eq!(r.base(), FileRef::new(100, 3));
    assert_eq!(r.standard_information(), None);
    assert_eq!(r.mft_changed(), None);
    let DataExtent::NonResident(extent) = store.data_extents(id).next().unwrap() else {
        panic!()
    };
    assert_eq!(
        (extent.start_vcn, extent.last_vcn, extent.sizes),
        (10, 19, None)
    );

    // The resident attribute list: only the 0x80 and 0xA0 entries.
    let id = store.record_at(0x10_0C00).unwrap();
    let r = store.record(id).unwrap();
    assert_eq!(
        r.attribute_list(),
        AttributeListState::Resident { complete: true }
    );
    let al: Vec<_> = store.attribute_list_entries(id).collect();
    assert_eq!(
        al.iter()
            .map(|e| (e.type_code, e.name, e.start_vcn, e.holder, e.attr_id))
            .collect::<Vec<_>>(),
        vec![
            (0x80, AttrListName::Unnamed, 0, FileRef::new(103, 1), 2),
            (0x80, AttrListName::Unnamed, 10, FileRef::new(104, 1), 0),
            (0x80, AttrListName::Other, 0, FileRef::new(103, 1), 5),
            (0xA0, AttrListName::I30, 0, FileRef::new(103, 1), 3),
        ]
    );
    let DataExtent::Resident(res) = store.data_extents(id).next().unwrap() else {
        panic!()
    };
    assert_eq!(res.value_len, 5);
    assert_eq!(
        &listed[res.value_offset as usize..res.value_offset as usize + 5],
        b"hello"
    );

    // The record without a number (pre-3.1 header).
    let id = store.record_at(0x10_1200).unwrap();
    assert_eq!(store.record(id).unwrap().record_number(), None);
    assert_eq!(store.record(id).unwrap().fragment_key(), None);

    let c = store.counters();
    assert_eq!((c.records, c.files, c.dirs, c.index_entries), (5, 3, 1, 1));
}

#[test]
fn damaged_attributes_are_flagged_not_dropped() {
    let mut img = vec![0u8; MIB];
    // A runlist whose second header announces a 9-byte field.
    let mut faulty = runlist(&[(Some(7000), 3)]);
    faulty.pop();
    faulty.extend_from_slice(&[0x19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0]);
    let r1 = Rec::new(300)
        .data_extent(0, 2, (12288, 12288, 12288), &faulty, 0)
        .build();
    put(&mut img, 0x8000, &r1);
    // A non-resident $DATA whose runlist offset points outside the attribute.
    let mut bad = nonresident_attr(DATA, "", 0, 0, 0, 0, (0, 0, 0), &[0]);
    bad[0x20..0x22].copy_from_slice(&0x2000u16.to_le_bytes());
    // A non-resident $REPARSE_POINT, a $FILE_NAME that does not decode.
    let reparse = nonresident_attr(
        REPARSE_POINT,
        "",
        1,
        0,
        0,
        0,
        (4096, 20, 20),
        &runlist(&[(Some(5), 1)]),
    );
    let broken_name = resident_attr(FILE_NAME, "", 2, 0, &[0u8; 10]);
    let r2 = Rec::new(301)
        .raw_attr(bad)
        .raw_attr(reparse)
        .raw_attr(broken_name)
        .build();
    put(&mut img, 0x8400, &r2);
    // An attribute chain cut short: a length field of 4 after the first attribute.
    let mut r3 = Rec::new(302)
        .si([1, 2, 3, 4], 0)
        .name(fref(5, 5), "cut", 0, 0)
        .build();
    // Overwrite the end marker of the walk by a bogus attribute header.
    let first = u16::from_le_bytes([r3[0x14], r3[0x15]]) as usize;
    let si_len = u32::from_le_bytes(r3[first + 4..first + 8].try_into().unwrap()) as usize;
    r3[first + si_len..first + si_len + 8].copy_from_slice(&[0x30, 0, 0, 0, 4, 0, 0, 0]);
    put(&mut img, 0x8800, &r3);

    let (session, _, _) = scan_fresh(mem(img), 0..MIB as u64, ScanOptions::default(), &quick());
    let store = session.store();
    let id = store.record_at(0x8000).unwrap();
    let DataExtent::NonResident(e) = store.data_extents(id).next().unwrap() else {
        panic!()
    };
    assert_eq!(
        e.runlist_status,
        RunlistStatus::Invalid(RunlistError::FieldTooLarge)
    );
    assert_eq!(
        e.runs().map(|r| (r.lcn, r.length)).collect::<Vec<_>>(),
        vec![(Some(7000), 3)]
    );
    let id = store.record_at(0x8400).unwrap();
    let r = store.record(id).unwrap();
    let d = r.damage();
    assert!(d.data_malformed && d.name_malformed && !d.attributes_cut);
    assert_eq!(
        store.data_extents(id).count(),
        0,
        "content location unknown, not a resident value"
    );
    assert_eq!(r.reparse(), Reparse::Unknown);
    let id = store.record_at(0x8800).unwrap();
    let r = store.record(id).unwrap();
    assert!(r.damage().attributes_cut);
    assert!(r.standard_information().is_some());
    assert_eq!(store.names(id).count(), 0);
}

#[test]
fn torn_never_used_and_log_copies() {
    let mut img = vec![0u8; MIB];
    let mut torn = rich_file(400).build();
    tear(&mut torn, 1);
    put(&mut img, 0x4000, &torn);
    let mut log_copy = rich_file(401).build();
    tear(&mut log_copy, 0);
    tear(&mut log_copy, 1);
    put(&mut img, 0x4400, &log_copy);
    put(&mut img, 0x4800, &never_used(402));
    // A 4 KiB record with all eight blocks torn is a log copy too.
    let mut big = rich_file(403).size(4096).build();
    for b in 0..8 {
        tear(&mut big, b);
    }
    put(&mut img, 0x5000, &big);
    // A never-used record with sequence 0 (Windows XP) and one in use without attributes.
    put(&mut img, 0x6000, &Rec::new(404).seq(0).flags(0).build());
    put(&mut img, 0x6400, &Rec::new(405).build());

    let (session, _, _) = scan_fresh(mem(img), 0..MIB as u64, ScanOptions::default(), &quick());
    let store = session.store();
    assert_eq!(
        record_ids(store),
        vec![(0x4000, Some(400)), (0x6400, Some(405))]
    );
    let r = store.record(store.record_at(0x4000).unwrap()).unwrap();
    assert_eq!(
        r.fixup(),
        FixupStatus::Torn {
            first_bad_block: 1,
            bad_blocks: 1
        }
    );
    // The hash is of the bytes as read.
    assert_eq!(r.hash(), hds_ntfs::fnv1a64(&torn));
    let r = store.record(store.record_at(0x6400).unwrap()).unwrap();
    assert!(!r.has_attributes());
    let c = store.counters();
    assert_eq!(
        (c.records, c.never_used, c.log_copies, c.file_hits),
        (2, 2, 2, 6)
    );
}

#[test]
fn four_kib_records_and_the_record_size_limit() {
    let mut img = vec![0u8; MIB];
    put(&mut img, 0x2000, &rich_file(10).size(4096).build());
    put(&mut img, 0x4000, &rich_file(11).size(8192).build());
    let src = mem(img);
    let (session, _, _) = scan_fresh(src.clone(), 0..MIB as u64, ScanOptions::default(), &quick());
    assert_eq!(record_ids(session.store()), vec![(0x2000, Some(10))]);
    let r = session.store().record(RecordId(0)).unwrap();
    assert_eq!(r.record_size(), 4096);
    assert_eq!(session.store().counters().file_refused, 1);
    let wide = ScanOptions {
        max_record_size: 65536,
        ..ScanOptions::default()
    };
    let (session, _, _) = scan_fresh(src, 0..MIB as u64, wide, &quick());
    assert_eq!(
        record_ids(session.store()),
        vec![(0x2000, Some(10)), (0x4000, Some(11))]
    );
}

#[test]
fn structures_crossing_chunk_ends_are_read_on_their_own() {
    let mut img = vec![0u8; 3 * CHUNK];
    // A record whose second half lies in the next chunk, at an odd sector.
    let rec = rich_file(500).build();
    put(&mut img, CHUNK - 512, &rec);
    // A 4 KiB INDX block crossing the second chunk end.
    let block = indx_block(
        8,
        77,
        4096,
        &[
            index_entry(
                fref(600, 1),
                fref(500, 3),
                "Dir A",
                1,
                0x1000_0000,
                [1, 2, 3, 4],
            ),
            index_entry(fref(601, 1), fref(500, 3), "file", 1, 0x20, [1, 2, 3, 4]),
        ],
    );
    put(&mut img, 2 * CHUNK - 1024, &block);
    let faulty = Arc::new(FaultySource::new(mem(img)));
    let (session, _, _) = scan_fresh(
        faulty.clone(),
        0..3 * CHUNK as u64,
        ScanOptions::default(),
        &quick(),
    );
    let store = session.store();
    let id = store.record_at(CHUNK as u64 - 512).unwrap();
    assert_eq!(store.record(id).unwrap().hash(), hds_ntfs::fnv1a64(&rec));
    assert_eq!(store.names(id).count(), 2);
    let b = store.index_block_at(2 * CHUNK as u64 - 1024).unwrap();
    assert_eq!(
        (b.vcn, b.size, b.lsn, b.owner),
        (8, 4096, 77, Some(FileRef::new(500, 3)))
    );
    assert_eq!(b.fixup, FixupStatus::Ok);
    let entries: Vec<_> = store.index_entries().map(|(_, e)| e).collect();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].name.eq_str("Dir A"));
    assert_eq!(entries[0].source.kind, EntrySourceKind::Block);
    assert_eq!(entries[0].source.lsn, 77);
    // Both were read on their own.
    let log = faulty.read_log();
    assert!(log.contains(&(CHUNK as u64 - 512, 1024)), "{log:?}");
    assert!(log.contains(&(2 * CHUNK as u64 - 1024, 4096)), "{log:?}");
}

#[test]
fn a_structure_crossing_the_range_end_is_not_read() {
    let mut img = vec![0u8; 2 * CHUNK];
    put(&mut img, CHUNK - 512, &rich_file(7).build());
    let faulty = Arc::new(FaultySource::new(mem(img)));
    let range = 0..CHUNK as u64;
    let (session, _, _) = scan_fresh(
        faulty.clone(),
        range.clone(),
        ScanOptions::default(),
        &quick(),
    );
    assert_eq!(session.store().record_count(), 0);
    assert_eq!(session.store().counters().truncated, 1);
    // Nothing outside the session range was asked for (S2a).
    let touched = faulty.touched();
    assert!(touched.iter().all(|r| r.end <= range.end), "{touched:?}");
}

#[test]
fn boot_sectors_and_carve_hits_at_any_sector() {
    let mut img = vec![0u8; 2 * CHUNK];
    let boot = boot_sector(512, 8, 1_000_000, 4, 2, -10, 1, 63);
    put(&mut img, 512 * 12_345, &boot);
    put(&mut img, 512 * 12_347, &png_head());
    put(&mut img, CHUNK + 512 * 3, &png_head());
    // Not a boot sector: the signatures are there but the fields are invalid.
    let mut bogus = boot.clone();
    bogus[0x0D] = 3;
    put(&mut img, 512 * 20_001, &bogus);
    let src = mem(img);
    let (session, _, _) = scan_fresh(
        src.clone(),
        0..2 * CHUNK as u64,
        ScanOptions::default(),
        &quick(),
    );
    let store = session.store();
    let boots = store.boot_hits();
    assert_eq!(boots.len(), 1);
    assert_eq!(boots[0].disk_offset, 512 * 12_345);
    assert_eq!(boots[0].boot.cluster_size, 4096);
    assert_eq!(boots[0].boot.hidden_sectors, 63);
    assert_eq!(store.counters().boot_refused, 1);
    let hits = store.carve_hits_sorted();
    assert_eq!(
        hits.iter()
            .map(|h| (h.disk_offset(), h.kind()))
            .collect::<Vec<_>>(),
        vec![
            (512 * 12_347, hds_carve::FileKind::Png),
            (CHUNK as u64 + 1536, hds_carve::FileKind::Png)
        ]
    );
    assert_eq!(hits[0].raw(), (12_347 << 8) | 2);
    // Without collection no hit is kept.
    let off = ScanOptions {
        collect_carve_hits: false,
        ..ScanOptions::default()
    };
    let (session, _, _) = scan_fresh(src, 0..2 * CHUNK as u64, off, &quick());
    assert_eq!(session.store().carve_hit_count(), 0);
}

#[test]
fn the_carve_hit_cap_drops_and_warns_once() {
    let mut img = vec![0u8; 2 * CHUNK];
    for i in 0..5 {
        put(&mut img, 512 * (100 + 7 * i), &png_head());
    }
    put(&mut img, CHUNK + 512, &png_head());
    let tuning = ScanTuning {
        carve_hit_cap: 3,
        ..ScanTuning::default()
    };
    let (session, summary, recorder) = scan_fresh(
        mem(img),
        0..2 * CHUNK as u64,
        ScanOptions::default(),
        &tuning,
    );
    let c = session.store().counters();
    assert_eq!((c.carve_hits, c.carve_dropped), (3, 3));
    assert_eq!(recorder.warnings.len(), 1);
    assert_eq!(recorder.warnings[0].code, WARN_CARVE_HITS_CAPPED);
    assert_eq!(
        summary
            .warnings
            .iter()
            .filter(|w| w.code == WARN_CARVE_HITS_CAPPED)
            .count(),
        1
    );
}

#[test]
fn a_partition_at_sector_63_puts_records_at_odd_sectors_and_is_probed_first() {
    // 40 MiB disk; NTFS partition at sector 63 whose boot sector puts the MFT at LCN 6400
    // (25 MiB + 63 sectors into the disk).
    let mut img = vec![0u8; 40 * MIB];
    let part_sectors = (40 * MIB / 512 - 63) as u32;
    put(&mut img, 0, &mbr(&[(63, part_sectors, 0x07)]));
    let p = 63 * 512;
    put(
        &mut img,
        p,
        &boot_sector(512, 8, u64::from(part_sectors) - 1, 6400, 2, -10, 1, 63),
    );
    let mft = p + 6400 * 4096;
    for n in 0..8u32 {
        put(&mut img, mft + n as usize * 1024, &rich_file(n).build());
    }
    let faulty = Arc::new(FaultySource::new(mem(img)));
    let (session, summary, recorder) = scan_fresh(
        faulty.clone(),
        0..40 * MIB as u64,
        ScanOptions::default(),
        &quick(),
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    let store = session.store();
    let found = record_ids(store);
    assert_eq!(found.len(), 8);
    assert!(
        found.iter().all(|(o, _)| (o / 512) % 2 == 1),
        "odd sectors: {found:?}"
    );
    // The first chunk read is the one holding the MFT the boot sector names.
    let first_chunk = faulty
        .read_log()
        .into_iter()
        .find(|&(_, len)| len == CHUNK)
        .unwrap();
    assert_eq!(first_chunk.0, (mft / CHUNK * CHUNK) as u64);
    assert_eq!(
        recorder.events.first().map(|(_, p)| p.phase),
        Some(ScanPhase::MftProbe)
    );
    // One fragment: origin = MFT start.
    let index = store.fragment_index(|_, _| true);
    assert_eq!(index.fragment_count(), 1);
    let f = index.fragments().next().unwrap();
    assert_eq!(
        (f.record_size, f.origin, f.records.len()),
        (1024, i64::try_from(mft).unwrap(), 8)
    );
    // The boot sector at the partition start is a hit as well.
    assert_eq!(store.boot_hits()[0].disk_offset, p as u64);
}

#[test]
fn probe_offsets_follow_a2() {
    // No partition table: the range start and range length; a boot sector at the range
    // start naming its MFT.
    let mut img = vec![0u8; 64 * MIB];
    put(
        &mut img,
        0,
        &boot_sector(512, 8, (64 * MIB / 512 - 1) as u64, 1000, 2, -10, 1, 0),
    );
    let src = mem(img);
    let ranged = RangedSource::new(src.clone(), 0..64 * MIB as u64).unwrap();
    let probes = plan::probe_offsets(&ranged, &AtomicBool::new(false)).unwrap();
    let third = ((64 * MIB / 3) & !4095) as u64;
    assert_eq!(probes, vec![1000 * 4096, third]);
    // A sub-range: only its own start counts; sector 0 is never read.
    let faulty = Arc::new(FaultySource::new(src));
    let sub = RangedSource::new(faulty.clone(), 8 * MIB as u64..64 * MIB as u64).unwrap();
    let probes = plan::probe_offsets(&sub, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        probes,
        vec![8 * MIB as u64 + ((56 * MIB / 3) & !4095) as u64]
    );
    assert!(faulty.touched().iter().all(|r| r.start >= 8 * MIB as u64));
}

#[test]
fn a_4096_byte_sector_source_reads_crossing_structures_unaligned() {
    let mut img = vec![0u8; 2 * CHUNK];
    let rec = rich_file(9).build();
    put(&mut img, CHUNK - 512, &rec);
    let src: Arc<dyn BlockSource> = Arc::new(MemSource::with_sector_sizes(img, 4096, 4096));
    let (session, summary, _) =
        scan_fresh(src, 0..2 * CHUNK as u64, ScanOptions::default(), &quick());
    assert_eq!(summary.status, ScanStatus::Completed);
    let id = session.store().record_at(CHUNK as u64 - 512).unwrap();
    assert_eq!(
        session.store().record(id).unwrap().hash(),
        hds_ntfs::fnv1a64(&rec)
    );
}

// --------------------------------------------------------------- read errors

/// An image with records spread over three chunks.
pub(crate) fn spread_image() -> Vec<u8> {
    let mut img = vec![0u8; 3 * CHUNK];
    for (i, at) in [
        0x1000usize,
        CHUNK + 0x3000,
        CHUNK + 0x10_0000,
        2 * CHUNK + 0x8000,
    ]
    .iter()
    .enumerate()
    {
        put(&mut img, *at, &rich_file(1000 + i as u32).build());
    }
    put(&mut img, 2 * CHUNK + 0x2_0000, &png_head());
    img
}

#[test]
fn scattered_bad_sectors_become_exact_bad_ranges() {
    let img = spread_image();
    let faulty = Arc::new(FaultySource::new(mem(img)));
    let bad = [
        CHUNK as u64 + 0x3000..CHUNK as u64 + 0x3200, // inside the record at that offset
        CHUNK as u64 + 0x5_0000..CHUNK as u64 + 0x5_0200,
        2 * CHUNK as u64 + 0x7_0400..2 * CHUNK as u64 + 0x7_0800,
    ];
    for b in &bad {
        faulty.bad_sectors(b.clone());
    }
    let range = 0..3 * CHUNK as u64;
    let (session, summary, _) = scan_fresh(faulty, range.clone(), ScanOptions::default(), &quick());
    assert_eq!(summary.status, ScanStatus::Completed);
    let store = session.store();
    let expected: RangeSet = bad.iter().cloned().collect();
    assert_eq!(store.bad_ranges(), &expected);
    assert_eq!(store.done().iter().collect::<Vec<_>>(), vec![range]);
    assert_eq!(store.counters().bad_bytes, 0x200 + 0x200 + 0x400);
    // The record whose first sector is unreadable (zero-filled) is gone.
    assert!(store.record_at(CHUNK as u64 + 0x3000).is_none());
    assert_eq!(store.record_count(), 3);
}

#[test]
fn a_long_bad_run_gives_up_the_rest_of_its_64_kib_block() {
    let img = spread_image();
    let faulty = Arc::new(FaultySource::new(mem(img)));
    let block = CHUNK as u64 + 0x4_0000;
    faulty.bad_sectors(block + 2048..block + 2048 + 20 * 512);
    let (session, _, _) = scan_fresh(
        faulty,
        0..3 * CHUNK as u64,
        ScanOptions::default(),
        &quick(),
    );
    let expected: RangeSet = std::iter::once(block + 2048..block + 0x1_0000).collect();
    assert_eq!(session.store().bad_ranges(), &expected);
}

/// The store of an uninterrupted scan of `img`.
fn reference(img: &[u8]) -> ScanStore {
    let (session, summary, _) = scan_fresh(
        mem(img.to_vec()),
        whole(img),
        ScanOptions::default(),
        &quick(),
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    session.store().clone()
}

#[test]
fn a_device_lost_in_a_chunk_records_nothing_of_it_and_a_resume_completes() {
    let img = spread_image();
    let faulty = Arc::new(FaultySource::new(mem(img.clone())));
    // Chunk 0 is read last (the probe starts at a third of the range).
    faulty.fail_range(0x2_0000..0x2_0200, ReadFailure::DeviceLost);
    let src: Arc<dyn BlockSource> = faulty.clone();
    let range = whole(&img);
    let mut session = Session::new(meta_for(
        src.as_ref(),
        range.clone(),
        ScanOptions::default(),
    ));
    let cancel = AtomicBool::new(false);
    let (summary, _) = scan_into(
        src.clone(),
        range.clone(),
        ScanOptions::default(),
        &quick(),
        &mut session,
        &cancel,
    );
    assert!(
        matches!(
            summary.status,
            ScanStatus::Failed(ScanFailure::SourceLost(_))
        ),
        "{:?}",
        summary.status
    );
    assert_eq!(summary.status.clone(), summary.status);
    let store = session.store();
    // The failing chunk is neither done nor bad, and none of its records is kept;
    // the chunks read before it are.
    let failing = 0..CHUNK as u64;
    assert_eq!(
        store.done().iter().collect::<Vec<_>>(),
        vec![CHUNK as u64..3 * CHUNK as u64]
    );
    assert!(!store.done().intersects(failing.clone()));
    assert!(store.bad_ranges().is_empty());
    assert!(store
        .records()
        .all(|(_, r)| !failing.contains(&r.disk_offset())));
    // Resume after the device is back: same store as a scan that was never interrupted.
    faulty.clear_faults();
    let (summary, _) = scan_into(
        src,
        range,
        ScanOptions::default(),
        &quick(),
        &mut session,
        &cancel,
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    assert_eq!(session.store(), &reference(&img));
}

#[test]
fn a_device_lost_during_retries_records_nothing_of_the_chunk() {
    let img = spread_image();
    let faulty = Arc::new(FaultySource::new(mem(img.clone())));
    faulty.bad_sectors(2 * CHUNK as u64 + 0x10_0000..2 * CHUNK as u64 + 0x10_0400);
    // The first liveness check (for the first bad sector) answers, then the device is gone.
    faulty.lose_after_liveness_checks(1);
    let src: Arc<dyn BlockSource> = faulty.clone();
    let range = whole(&img);
    let mut session = Session::new(meta_for(
        src.as_ref(),
        range.clone(),
        ScanOptions::default(),
    ));
    let cancel = AtomicBool::new(false);
    let (summary, _) = scan_into(
        src.clone(),
        range.clone(),
        ScanOptions::default(),
        &quick(),
        &mut session,
        &cancel,
    );
    assert!(
        matches!(
            summary.status,
            ScanStatus::Failed(ScanFailure::SourceLost(_))
        ),
        "{:?}",
        summary.status
    );
    assert!(session.store().bad_ranges().is_empty());
    assert!(!session
        .store()
        .done()
        .intersects(2 * CHUNK as u64..3 * CHUNK as u64));
    faulty.revive();
    let (summary, _) = scan_into(
        src,
        range,
        ScanOptions::default(),
        &quick(),
        &mut session,
        &cancel,
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    let expected: RangeSet =
        std::iter::once(2 * CHUNK as u64 + 0x10_0000..2 * CHUNK as u64 + 0x10_0400).collect();
    assert_eq!(session.store().bad_ranges(), &expected);
}

#[test]
fn a_device_that_drops_off_with_media_errors_is_lost_not_bad() {
    let img = spread_image();
    let faulty = Arc::new(FaultySource::new(mem(img.clone())));
    faulty.lose_silently_after_reads(5);
    let (session, summary, _) = scan_fresh(faulty, whole(&img), ScanOptions::default(), &quick());
    assert!(
        matches!(
            summary.status,
            ScanStatus::Failed(ScanFailure::SourceLost(_))
        ),
        "{:?}",
        summary.status
    );
    assert!(session.store().bad_ranges().is_empty());
}

#[test]
fn cancellation_stops_between_chunks_and_a_resume_completes() {
    let img = spread_image();
    let faulty = Arc::new(FaultySource::new(mem(img.clone())));
    let cancel = Arc::new(AtomicBool::new(false));
    faulty.cancel_after_reads(6, cancel.clone());
    let src: Arc<dyn BlockSource> = faulty.clone();
    let range = whole(&img);
    let mut session = Session::new(meta_for(
        src.as_ref(),
        range.clone(),
        ScanOptions::default(),
    ));
    let (summary, _) = scan_into(
        src.clone(),
        range.clone(),
        ScanOptions::default(),
        &quick(),
        &mut session,
        &cancel,
    );
    assert_eq!(summary.status, ScanStatus::Stopped);
    // The done set is made of whole grid chunks.
    for r in session.store().done().iter() {
        assert!(
            r.start % CHUNK_SIZE == 0 && (r.end % CHUNK_SIZE == 0 || r.end == range.end),
            "{r:?}"
        );
    }
    assert!(!session.is_complete());
    // FaultySource raises the flag again on every read after the chosen one until
    // its counters are reset.
    faulty.reset_counters();
    cancel.store(false, Ordering::SeqCst);
    let (summary, _) = scan_into(
        src,
        range,
        ScanOptions::default(),
        &quick(),
        &mut session,
        &cancel,
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    assert_eq!(session.store(), &reference(&img));
}

#[test]
fn cancellation_is_polled_between_retries_of_a_failing_read() {
    let img = vec![0u8; 2 * CHUNK];
    let faulty = Arc::new(FaultySource::new(mem(img)));
    // Every sector of the first chunk fails, each read taking 20 ms: the retries
    // alone would take minutes.
    faulty.bad_sectors(0..CHUNK as u64);
    faulty.set_delay(Duration::from_millis(20));
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        flag.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let src: Arc<dyn BlockSource> = faulty;
    let range = 0..2 * CHUNK as u64;
    let mut session = Session::new(meta_for(
        src.as_ref(),
        range.clone(),
        ScanOptions::default(),
    ));
    let (summary, _) = scan_into(
        src,
        range,
        ScanOptions::default(),
        &quick(),
        &mut session,
        &cancel,
    );
    stopper.join().unwrap();
    assert_eq!(summary.status, ScanStatus::Stopped);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(session.store().bad_ranges().is_empty());
    assert!(session.store().done().is_empty());
}

#[test]
fn the_result_does_not_depend_on_thread_timing() {
    let mut img = spread_image();
    // Structures crossing chunk ends make the parser read on its own while the reader works.
    put(&mut img, CHUNK - 512, &rich_file(77).build());
    put(&mut img, 2 * CHUNK - 512, &indx_block(0, 1, 4096, &[]));
    let fast = Arc::new(FaultySource::new(mem(img.clone())));
    let slow = Arc::new(FaultySource::new(mem(img.clone())));
    slow.set_delay(Duration::from_millis(15));
    let (a, _, _) = scan_fresh(fast.clone(), whole(&img), ScanOptions::default(), &quick());
    let tuning = ScanTuning {
        read_ahead: 1,
        ..ScanTuning::default()
    };
    let (b, _, _) = scan_fresh(slow.clone(), whole(&img), ScanOptions::default(), &tuning);
    assert_eq!(a.store(), b.store());
    let chunks = |f: &FaultySource| -> Vec<u64> {
        f.read_log()
            .into_iter()
            .filter(|&(_, len)| len >= MIB)
            .map(|(o, _)| o)
            .collect()
    };
    assert_eq!(chunks(&fast), chunks(&slow));
}

#[test]
fn a_source_that_reads_zero_raises_the_warning_once() {
    let size = (1u64 << 30) + 16 * MIB as u64;
    let src: Arc<dyn BlockSource> = Arc::new(ZeroSource { size });
    let (session, summary, recorder) = scan_fresh(src, 0..size, ScanOptions::default(), &quick());
    assert_eq!(summary.status, ScanStatus::Completed);
    assert_eq!(
        recorder
            .warnings
            .iter()
            .filter(|w| w.code == WARN_SOURCE_READS_ZERO)
            .count(),
        1
    );
    assert_eq!(session.store().warnings().len(), 1);
    // Data anywhere in the first gigabyte: no warning.
    let mut img = vec![0u8; 64 * MIB];
    img[40 * MIB + 3] = 1;
    let (_, summary, _) = scan_fresh(
        mem(img),
        0..64 * MIB as u64,
        ScanOptions::default(),
        &quick(),
    );
    assert!(summary.warnings.is_empty());
    // A small range of zeros is reported once all of it is read.
    let (_, summary, _) = scan_fresh(
        mem(vec![0u8; 3 * MIB]),
        0..3 * MIB as u64,
        ScanOptions::default(),
        &quick(),
    );
    assert_eq!(summary.warnings.len(), 1);
    // Unreadable sectors are not zeros that were read: an image that is all bad
    // sectors raises nothing.
    let faulty = Arc::new(FaultySource::new(mem(vec![0u8; 2 * MIB])));
    faulty.bad_sectors(0..2 * MIB as u64);
    let (session, summary, _) =
        scan_fresh(faulty, 0..2 * MIB as u64, ScanOptions::default(), &quick());
    assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);
    assert_eq!(session.store().counters().bad_bytes, 2 * MIB as u64);
}

#[test]
fn progress_is_paced_and_reports_the_phases() {
    // 24 chunks; the boot sector names an MFT in chunk 16, the heuristic probe is at
    // chunk 8; the sweep then reads chunks 0 to 7.
    let mut img = vec![0u8; 24 * CHUNK];
    put(
        &mut img,
        0,
        &boot_sector(512, 8, (24 * CHUNK / 512 - 1) as u64, 32768, 2, -10, 1, 0),
    );
    put(&mut img, 16 * CHUNK, &rich_file(1).build());
    let faulty = Arc::new(FaultySource::new(mem(img.clone())));
    faulty.set_delay(Duration::from_millis(60));
    let (session, summary, recorder) =
        scan_fresh(faulty, whole(&img), ScanOptions::default(), &quick());
    assert_eq!(summary.status, ScanStatus::Completed);
    let events = &recorder.events;
    assert!(events.len() >= 3, "{}", events.len());
    for pair in events.windows(2) {
        let gap = pair[1].0.duration_since(pair[0].0);
        assert!(gap >= Duration::from_millis(249), "{gap:?}");
    }
    let phases: Vec<ScanPhase> = events.iter().map(|(_, p)| p.phase).collect();
    assert_eq!(phases.first(), Some(&ScanPhase::MftProbe));
    assert_eq!(phases.last(), Some(&ScanPhase::Sweep));
    for (_, p) in events {
        assert_eq!(p.bytes_total, img.len() as u64);
        assert!(p.bytes_done <= p.bytes_total);
        assert!(p.bytes_done % CHUNK_SIZE == 0);
    }
    // bytes_done never goes back and ends at what the store says.
    assert!(events
        .windows(2)
        .all(|w| w[0].1.bytes_done <= w[1].1.bytes_done));
    assert_eq!(session.store().done().total_len(), img.len() as u64);
    let json = serde_json::to_value(&events[0].1).unwrap();
    assert_eq!(json["phase"], "mft_probe");
    for member in [
        "bytes_done",
        "bytes_total",
        "position",
        "speed_bps",
        "eta_seconds",
        "elapsed_seconds",
        "records",
        "files",
        "dirs",
        "index_blocks",
        "carve_hits",
        "bad_bytes",
        "warnings",
    ] {
        assert!(json.get(member).is_some(), "{member}");
    }
}

#[test]
fn mismatched_range_or_options_are_refused_and_a_complete_session_returns_at_once() {
    let img = spread_image();
    let src = Arc::new(FaultySource::new(mem(img.clone())));
    let dynsrc: Arc<dyn BlockSource> = src.clone();
    let range = whole(&img);
    let mut session = Session::new(meta_for(
        dynsrc.as_ref(),
        range.clone(),
        ScanOptions::default(),
    ));
    let cancel = AtomicBool::new(false);
    let (summary, _) = scan_into(
        dynsrc.clone(),
        0..CHUNK as u64,
        ScanOptions::default(),
        &quick(),
        &mut session,
        &cancel,
    );
    assert!(matches!(
        summary.status,
        ScanStatus::Failed(ScanFailure::Mismatch(_))
    ));
    let other = ScanOptions {
        collect_carve_hits: false,
        ..ScanOptions::default()
    };
    let (summary, _) = scan_into(
        dynsrc.clone(),
        range.clone(),
        other,
        &quick(),
        &mut session,
        &cancel,
    );
    assert!(
        matches!(summary.status, ScanStatus::Failed(ScanFailure::Mismatch(ref m)) if m.contains("options"))
    );
    assert_eq!(src.read_count(), 0);
    let (summary, _) = scan_into(
        dynsrc.clone(),
        range.clone(),
        ScanOptions::default(),
        &quick(),
        &mut session,
        &cancel,
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    src.reset_counters();
    let (summary, _) = scan_into(
        dynsrc,
        range,
        ScanOptions::default(),
        &quick(),
        &mut session,
        &cancel,
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    assert_eq!(src.read_count(), 0, "a complete session reads nothing");
    assert_eq!(summary.bytes_read, 0);
}

/// A large source of zeros that holds no memory, for probe arithmetic on
/// volume sizes Windows was measured with.
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
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        buf.fill(0);
        Ok(())
    }
    fn info(&self) -> hds_io::SourceInfo {
        MemSource::new(vec![0; 512]).info()
    }
    fn is_alive(&self) -> bool {
        true
    }
}

#[test]
fn probe_offsets_cover_where_windows_puts_the_mft() {
    const GIB: u64 = 1 << 30;
    // Volume size -> where Windows' quick format was measured to put the $MFT.
    for (size, measured) in [
        (256 * MIB as u64, (256 * MIB as u64 / 3) & !4095),
        (GIB, (GIB / 3) & !4095),
        (4 * GIB, GIB),
        (16 * GIB, 3 * GIB),
        (64 * GIB, 3 * GIB),
    ] {
        let src: Arc<dyn BlockSource> = Arc::new(Zeros(size));
        let ranged = RangedSource::new(src, 0..size).unwrap();
        let probes = plan::probe_offsets(&ranged, &AtomicBool::new(false)).unwrap();
        assert!(
            probes.contains(&measured),
            "{size}: {probes:?} misses {measured}"
        );
    }
}
