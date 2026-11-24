use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use hds_io::testing::{FaultySource, MemSource, XorShift};
use hds_io::{BlockSource, RangedSource, ReadFailure};

use super::*;
use crate::testutil::pattern;

fn ranged(inner: Arc<dyn BlockSource>, range: Range<u64>) -> RangedSource {
    RangedSource::new(inner, range).unwrap()
}

fn faulty(data: Vec<u8>) -> Arc<FaultySource> {
    Arc::new(FaultySource::new(Arc::new(MemSource::new(data))))
}

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn read_all(reader: &mut ContentReader<'_>) -> Result<Vec<u8>, ReadError> {
    let mut out = vec![0xAA; reader.size() as usize];
    let n = reader.read_at(0, &mut out)?;
    assert_eq!(n, out.len());
    Ok(out)
}

fn extents(runs: Vec<Run>, real_size: u64, initialized_size: u64) -> FileSource {
    FileSource::Extents {
        runs,
        real_size,
        initialized_size,
    }
}

fn data(offset: u64, len: u64) -> Run {
    Run::Data { offset, len }
}

fn hole(len: u64) -> Run {
    Run::Hole { len }
}

/// The reads a source was asked for must all lie inside the session range (S2a).
fn assert_reads_inside(disk: &FaultySource, range: Range<u64>) {
    for touched in disk.touched().iter() {
        assert!(
            touched.start >= range.start && touched.end <= range.end,
            "read {touched:?} outside the session range {range:?}"
        );
    }
}

#[test]
fn holes_and_runs_come_back_in_file_order() {
    let disk = pattern(64 * 1024);
    let source = ranged(Arc::new(MemSource::new(disk.clone())), 0..65536);
    let file = extents(
        vec![
            data(8192, 4096),
            hole(8192),
            data(512, 1024),
            data(30000, 3000),
        ],
        4096 + 8192 + 1024 + 3000,
        u64::MAX,
    );
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    let out = read_all(&mut reader).unwrap();
    let mut expected = disk[8192..12288].to_vec();
    expected.extend(std::iter::repeat_n(0u8, 8192));
    expected.extend_from_slice(&disk[512..1536]);
    expected.extend_from_slice(&disk[30000..33000]);
    assert_eq!(out, expected);
    assert!(reader.flags().is_empty(), "{:?}", reader.flags());
    assert_eq!(reader.report().out_of_bounds_bytes(), 0);
    assert_eq!(reader.report().missing_bytes(), 0);
}

#[test]
fn reads_at_any_offset_and_length_agree_with_one_whole_read() {
    let disk = pattern(40_000);
    let source = ranged(Arc::new(MemSource::new(disk)), 0..40_448);
    let file = extents(
        vec![
            data(1000, 777),
            hole(333),
            data(20_001, 5000),
            data(3, 2000),
        ],
        7000,
        6500,
    );
    let cancel = no_cancel();
    let whole = read_all(&mut ContentReader::new(&source, &file, &cancel)).unwrap();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    for (offset, len) in [
        (0, 1),
        (776, 3),
        (777, 333),
        (1109, 1),
        (6499, 10),
        (6999, 5),
        (7000, 9),
        (123, 6000),
    ] {
        let mut out = vec![0xAA; len];
        let n = reader.read_at(offset, &mut out).unwrap();
        let end = (offset as usize + len).min(7000);
        let expect = whole.get(offset as usize..end).unwrap_or(&[]);
        assert_eq!(&out[..n], expect, "offset {offset} len {len}");
        assert!(
            out[n..].iter().all(|&b| b == 0xAA),
            "nothing beyond what was returned is touched"
        );
    }
    assert_eq!(reader.read_at(u64::MAX, &mut [0u8; 8]).unwrap(), 0);
    assert_eq!(reader.read_at(0, &mut []).unwrap(), 0);
}

#[test]
fn parts_of_runs_outside_the_session_range_are_zeros_and_out_of_bounds() {
    let disk = faulty(pattern(64 * 1024));
    let range = 16_384..32_768u64;
    let source = ranged(disk.clone(), range.clone());
    let file = extents(
        vec![
            data(15_872, 1024),      // straddles the start: 512 outside, 512 inside
            data(32_256, 1024),      // straddles the end: 512 inside, 512 outside
            data(40_000, 100),       // entirely behind the range
            data(1_000, 100),        // entirely before it
            data(u64::MAX - 10, 50), // runs off the end of the address space
            data(20_000, 100),       // fine
        ],
        2 * 1024 + 100 + 100 + 50 + 100,
        u64::MAX,
    );
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    let out = read_all(&mut reader).unwrap();
    let all = pattern(64 * 1024);
    assert!(out[..512].iter().all(|&b| b == 0));
    assert_eq!(&out[512..1024], &all[16_384..16_896]);
    assert_eq!(&out[1024..1536], &all[32_256..32_768]);
    assert!(out[1536..2048 + 100 + 100 + 50].iter().all(|&b| b == 0));
    assert_eq!(&out[2298..2398], &all[20_000..20_100]);
    assert!(reader.flags().contains(Flag::OutOfBounds));
    assert!(
        !reader.flags().contains(Flag::BadSectors),
        "never a bad sector"
    );
    assert_eq!(
        reader.report().out_of_bounds_bytes(),
        512 + 512 + 100 + 100 + 50
    );
    assert_eq!(
        reader.report().out_of_bounds_ranges().collect::<Vec<_>>(),
        [0..512, 1536..2298]
    );
    assert_reads_inside(&disk, range);
}

#[test]
fn bytes_at_and_beyond_the_initialized_size_are_zeros_and_not_read() {
    let disk = faulty(pattern(64 * 1024));
    // Stale bytes behind the initialized size sit in unreadable sectors: neither read nor flagged.
    disk.bad_sectors(12_288..16_384);
    let source = ranged(disk.clone(), 0..65_536);
    let file = extents(vec![data(8192, 8192)], 7000, 3000);
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    let out = read_all(&mut reader).unwrap();
    assert_eq!(out.len(), 7000);
    assert_eq!(&out[..3000], &pattern(64 * 1024)[8192..11_192]);
    assert!(out[3000..].iter().all(|&b| b == 0));
    assert!(reader.flags().is_empty(), "{:?}", reader.flags());
    let touched = disk.touched();
    assert!(!touched.intersects(11_264..65_536), "{touched:?}");
}

#[test]
fn nothing_beyond_the_real_size_is_returned() {
    // 3 clusters of 4 KiB allocated, 2.5 clusters of data.
    let disk = pattern(64 * 1024);
    let source = ranged(Arc::new(MemSource::new(disk.clone())), 0..65_536);
    let file = extents(vec![data(4096, 12_288)], 10_240, 10_240);
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    let mut out = vec![0xAA; 20_000];
    assert_eq!(reader.read_at(0, &mut out).unwrap(), 10_240);
    assert_eq!(&out[..10_240], &disk[4096..14_336]);
    assert!(out[10_240..].iter().all(|&b| b == 0xAA));
    assert_eq!(reader.read_at(10_240, &mut out).unwrap(), 0);
}

#[test]
fn runs_that_end_before_the_size_leave_missing_fragments() {
    let disk = pattern(16_384);
    let source = ranged(Arc::new(MemSource::new(disk.clone())), 0..16_384);
    let file = extents(vec![data(0, 1000)], 3000, 2500);
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    let out = read_all(&mut reader).unwrap();
    assert_eq!(&out[..1000], &disk[..1000]);
    assert!(out[1000..].iter().all(|&b| b == 0));
    assert!(reader.flags().contains(Flag::MissingFragments));
    assert_eq!(
        reader.report().missing_bytes(),
        1500,
        "only up to the initialized size"
    );
    // No runs at all.
    let empty = extents(Vec::new(), 100, 100);
    let mut reader = ContentReader::new(&source, &empty, &cancel);
    assert_eq!(read_all(&mut reader).unwrap(), vec![0u8; 100]);
    assert_eq!(reader.flags(), Flag::MissingFragments.into());
    // Covered exactly: nothing is missing.
    let exact = extents(vec![data(0, 100)], 100, 100);
    let mut reader = ContentReader::new(&source, &exact, &cancel);
    read_all(&mut reader).unwrap();
    assert!(reader.flags().is_empty());
}

#[test]
fn unreadable_sectors_in_the_middle_of_a_run_are_zeros_with_their_ranges() {
    let disk = faulty(pattern(256 * 1024));
    disk.bad_sectors(70_144..71_168); // two sectors
    let source = ranged(disk.clone(), 0..262_144);
    let file = extents(vec![data(65_536, 65_536)], 65_536, 65_536);
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    let out = read_all(&mut reader).unwrap();
    let all = pattern(256 * 1024);
    let bad_in_file = 70_144 - 65_536..71_168 - 65_536;
    assert!(out[bad_in_file.clone()].iter().all(|&b| b == 0));
    assert_eq!(&out[..bad_in_file.start], &all[65_536..70_144]);
    assert_eq!(&out[bad_in_file.end..], &all[71_168..131_072]);
    assert_eq!(reader.flags(), Flag::BadSectors.into());
    assert_eq!(reader.report().bad_bytes(), 1024);
    assert_eq!(
        reader.report().bad_ranges().collect::<Vec<_>>(),
        [4608..5632]
    );
    assert_eq!(
        reader.report().bad_disk_ranges().collect::<Vec<_>>(),
        [70_144..71_168]
    );
}

#[test]
fn a_device_lost_in_the_middle_of_a_file_is_not_a_bad_sector() {
    let size = 3 * READ_CHUNK as u64;
    let disk = faulty(pattern(size as usize));
    let source = ranged(disk.clone(), 0..size);
    let file = extents(vec![data(0, size)], size, size);
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    let mut chunk = vec![0u8; READ_CHUNK];
    assert_eq!(reader.read_at(0, &mut chunk).unwrap(), READ_CHUNK);
    disk.lose_after_reads(0);
    match reader.read_at(READ_CHUNK as u64, &mut chunk) {
        Err(ReadError::SourceLost(_)) => {}
        other => panic!("{other:?}"),
    }
    assert!(!reader.flags().contains(Flag::BadSectors));
    assert_eq!(reader.report().bad_bytes(), 0);

    // Lost while bad sectors are being worked around: still lost.
    disk.revive();
    disk.bad_sectors(READ_CHUNK as u64..READ_CHUNK as u64 + 512);
    disk.lose_silently_after_reads(2);
    let mut reader = ContentReader::new(&source, &file, &cancel);
    match reader.read_at(READ_CHUNK as u64, &mut chunk) {
        Err(ReadError::SourceLost(_)) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(reader.report().bad_bytes(), 0);
}

#[test]
fn a_cancel_stops_the_read() {
    // The flag is raised while the second request runs; the third one sees it.
    let size = 3 * READ_CHUNK as u64;
    let disk = faulty(pattern(size as usize));
    let source = ranged(disk.clone(), 0..size);
    let file = extents(vec![data(0, size)], size, size);
    let cancel = Arc::new(AtomicBool::new(false));
    disk.cancel_after_reads(2, cancel.clone());
    let mut reader = ContentReader::new(&source, &file, &cancel);
    let mut out = vec![0u8; size as usize];
    match reader.read_at(0, &mut out) {
        Err(ReadError::Cancelled) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(disk.read_count(), 2, "the second request was the last one");
    cancel.store(false, Ordering::SeqCst);
    let flag = AtomicBool::new(true);
    let mut reader = ContentReader::new(&source, &file, &flag);
    assert!(matches!(
        reader.read_at(0, &mut out[..10]),
        Err(ReadError::Cancelled)
    ));
}

#[test]
fn requests_are_sector_aligned_and_at_most_one_chunk() {
    let size = 20 << 20;
    let disk = faulty(pattern(size));
    let source = ranged(disk.clone(), 0..size as u64);
    let file = extents(
        vec![data(1000, 18 << 20), data(3, 700)],
        (18 << 20) + 700,
        u64::MAX,
    );
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    let out = read_all(&mut reader).unwrap();
    let all = pattern(size);
    assert_eq!(&out[..18 << 20], &all[1000..1000 + (18 << 20)]);
    assert_eq!(&out[18 << 20..], &all[3..703]);
    for (offset, len) in disk.read_log() {
        assert_eq!(offset % 512, 0);
        assert_eq!(len % 512, 0);
        assert!(len <= READ_CHUNK, "{len}");
    }
}

#[test]
fn runs_that_are_not_sector_aligned_work_on_4k_sectors() {
    let disk = pattern(128 * 1024);
    let source = ranged(
        Arc::new(MemSource::with_sector_sizes(disk.clone(), 4096, 4096)),
        8192..122_880,
    );
    let file = extents(
        vec![data(8704, 5000), data(100_001, 3), data(122_000, 1500)],
        6503,
        6503,
    );
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    let out = read_all(&mut reader).unwrap();
    assert_eq!(&out[..5000], &disk[8704..13_704]);
    assert_eq!(&out[5000..5003], &disk[100_001..100_004]);
    assert_eq!(&out[5003..5883], &disk[122_000..122_880]);
    assert!(out[5883..].iter().all(|&b| b == 0));
    assert_eq!(reader.report().out_of_bounds_bytes(), 620);
}

#[test]
fn the_scratch_buffer_can_be_handed_on() {
    let disk = pattern(65_536);
    let source = ranged(Arc::new(MemSource::new(disk.clone())), 0..65_536);
    let a = extents(vec![data(0, 30_000)], 30_000, 30_000);
    let b = extents(vec![data(512, 100)], 100, 100);
    let cancel = no_cancel();
    let mut first = ContentReader::new(&source, &a, &cancel);
    read_all(&mut first).unwrap();
    let buffer = first.into_buffer();
    assert!(buffer.len() >= 30_000);
    let mut second = ContentReader::with_buffer(&source, &b, &cancel, buffer);
    assert_eq!(read_all(&mut second).unwrap(), &disk[512..612]);
}

// ---------------------------------------------------------------------------------------------
// Resident content
// ---------------------------------------------------------------------------------------------

fn file_record(record_size: usize, value: &[u8], usn: u16) -> Vec<u8> {
    crate::testutil::file_record(record_size, value, usn).0
}

/// Places `record` at `offset` of a patterned disk and describes its value
/// the way the scanner would, through hds-ntfs.
fn resident_setup(disk_len: usize, offset: usize, record: &[u8]) -> (Vec<u8>, FileSource) {
    let mut disk = pattern(disk_len);
    disk[offset..offset + record.len()].copy_from_slice(record);
    let hash = fnv1a64(record);
    let mut copy = record.to_vec();
    let parsed = hds_ntfs::parse_file_record(&mut copy).unwrap();
    let value = parsed
        .attributes()
        .find(|a| a.type_code == hds_ntfs::attr_type::DATA)
        .and_then(|a| a.resident())
        .unwrap();
    let file = FileSource::Resident {
        record_offset: offset as u64,
        record_size: record.len() as u32,
        value_offset: value.offset,
        value_len: value.len(),
        hash,
    };
    (disk, file)
}

#[test]
fn resident_content_crossing_fixup_positions_comes_back_whole() {
    let value: Vec<u8> = (0..700u32).map(|i| (i * 7 + 3) as u8).collect();
    let record = file_record(1024, &value, 0x4242);
    // The value crosses the protected tail of the first block.
    let FileSource::Resident { value_offset, .. } = resident_setup(1 << 16, 8192, &record).1 else {
        unreachable!()
    };
    assert!(value_offset < 510 && value_offset as usize + value.len() > 512);
    for sector in [512u32, 4096] {
        // At an offset that is not sector-aligned for 4 KiB sectors.
        let (disk, file) = resident_setup(1 << 16, 8192 + 1024, &record);
        let source = ranged(
            Arc::new(MemSource::with_sector_sizes(disk, sector, sector)),
            0..1 << 16,
        );
        let cancel = no_cancel();
        let mut reader = ContentReader::new(&source, &file, &cancel);
        assert_eq!(reader.size(), 700);
        assert_eq!(
            read_all(&mut reader).unwrap(),
            value,
            "sector size {sector}"
        );
        assert!(reader.flags().is_empty());
        let mut part = [0u8; 10];
        assert_eq!(reader.read_at(505, &mut part).unwrap(), 10);
        assert_eq!(&part, &value[505..515]);
    }
}

#[test]
fn a_changed_record_is_refused() {
    let record = file_record(1024, b"hello, resident world", 7);
    let (mut disk, file) = resident_setup(16_384, 4096, &record);
    disk[4096 + 900] ^= 0x01; // one bit anywhere in the record, even outside the value
    let source = ranged(Arc::new(MemSource::new(disk)), 0..16_384);
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    assert!(matches!(
        reader.read_at(0, &mut [0u8; 5]),
        Err(ReadError::RecordChanged)
    ));
    assert!(matches!(
        reader.verify_record(),
        Err(ReadError::RecordChanged)
    ));
}

#[test]
fn an_empty_resident_value_is_verified_too() {
    let record = file_record(1024, b"", 9);
    let (mut disk, file) = resident_setup(16_384, 2048, &record);
    assert_eq!(file.size(), 0);
    let source = ranged(Arc::new(MemSource::new(disk.clone())), 0..16_384);
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    reader.verify_record().unwrap();
    disk[2048 + 0x10] = 2; // the sequence number changed: the slot was reused
    let source = ranged(Arc::new(MemSource::new(disk)), 0..16_384);
    let mut reader = ContentReader::new(&source, &file, &cancel);
    assert!(matches!(
        reader.verify_record(),
        Err(ReadError::RecordChanged)
    ));
}

#[test]
fn a_torn_record_is_read_and_flagged() {
    let value = vec![0x5A; 600];
    let mut record = file_record(1024, &value, 0x1111);
    record[1022] = 0x99; // the second block no longer ends with the sequence number
    let (disk, file) = resident_setup(16_384, 4096, &record);
    let source = ranged(Arc::new(MemSource::new(disk)), 0..16_384);
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    assert_eq!(read_all(&mut reader).unwrap(), value);
    assert_eq!(reader.flags(), Flag::TornRecord.into());
}

#[test]
fn an_unreadable_record_cannot_be_verified() {
    let record = file_record(1024, b"data", 3);
    let (disk, file) = resident_setup(16_384, 4096, &record);
    let disk = faulty(disk);
    disk.bad_sectors(4608..5120);
    let source = ranged(disk, 0..16_384);
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    match reader.verify_record() {
        Err(ReadError::RecordUnreadable { bad }) => assert_eq!(bad, [4608..5120]),
        other => panic!("{other:?}"),
    }
    assert!(reader.flags().contains(Flag::BadSectors));
}

#[test]
fn impossible_resident_descriptions_are_refused_without_reading_outside() {
    let record = file_record(1024, b"data", 3);
    let (disk, good) = resident_setup(16_384, 4096, &record);
    let FileSource::Resident {
        value_offset, hash, ..
    } = good
    else {
        unreachable!()
    };
    let disk = faulty(disk);
    let source = ranged(disk.clone(), 2048..8192);
    let cancel = no_cancel();
    let cases = [
        // record partly outside the session range
        FileSource::Resident {
            record_offset: 7680,
            record_size: 1024,
            value_offset,
            value_len: 4,
            hash,
        },
        // value outside the record
        FileSource::Resident {
            record_offset: 4096,
            record_size: 1024,
            value_offset: 1000,
            value_len: 100,
            hash,
        },
        // not a record size
        FileSource::Resident {
            record_offset: 4096,
            record_size: 1000,
            value_offset,
            value_len: 4,
            hash,
        },
        FileSource::Resident {
            record_offset: u64::MAX - 100,
            record_size: 1024,
            value_offset,
            value_len: 4,
            hash,
        },
    ];
    for file in &cases {
        let mut reader = ContentReader::new(&source, file, &cancel);
        assert!(
            matches!(reader.verify_record(), Err(ReadError::Invalid(_))),
            "{file:?}"
        );
    }
    assert_reads_inside(&disk, 2048..8192);
    // A record whose update sequence array does not fit its size, with a matching hash.
    let mut broken = record.clone();
    broken[0x06] = 9;
    let (disk, _) = resident_setup(16_384, 4096, &record);
    let mut disk = disk;
    disk[4096..5120].copy_from_slice(&broken);
    let source = ranged(Arc::new(MemSource::new(disk)), 0..16_384);
    let file = FileSource::Resident {
        record_offset: 4096,
        record_size: 1024,
        value_offset,
        value_len: 4,
        hash: fnv1a64(&broken),
    };
    let mut reader = ContentReader::new(&source, &file, &cancel);
    assert!(matches!(reader.verify_record(), Err(ReadError::Invalid(_))));
}

// ---------------------------------------------------------------------------------------------
// Randomized comparison with a byte-by-byte model of H7
// ---------------------------------------------------------------------------------------------

/// H7 one byte at a time: the byte, and whether it is out of bounds / missing.
fn model_byte(
    disk: &[u8],
    range: &Range<u64>,
    runs: &[Run],
    initialized: u64,
    pos: u64,
) -> (u8, bool, bool) {
    if pos >= initialized {
        return (0, false, false);
    }
    let mut start = 0u64;
    for run in runs {
        let len = run.len();
        if pos >= start && pos - start < len {
            return match *run {
                Run::Hole { .. } => (0, false, false),
                Run::Data { offset, .. } => match offset.checked_add(pos - start) {
                    Some(abs) if range.contains(&abs) => (disk[abs as usize], false, false),
                    _ => (0, true, false),
                },
            };
        }
        start = start.saturating_add(len);
    }
    (0, false, true)
}

#[test]
fn random_files_read_exactly_as_the_model_says() {
    let mut rng = XorShift::new(0x5EED);
    let disk_len = 96 * 1024usize;
    let disk_bytes = pattern(disk_len);
    for round in 0..300 {
        let sector = if round % 3 == 0 { 4096u64 } else { 512 };
        let range_start = rng.below(disk_len as u64 / 2 / sector) * sector;
        let range_end = (range_start
            + sector * (1 + rng.below((disk_len as u64 - range_start) / sector)))
        .min(disk_len as u64);
        let range = range_start..range_end;
        let disk = Arc::new(FaultySource::new(Arc::new(MemSource::with_sector_sizes(
            disk_bytes.clone(),
            sector as u32,
            sector as u32,
        ))));
        let source = ranged(disk.clone(), range.clone());
        let mut runs = Vec::new();
        for _ in 0..rng.below(7) {
            let len = rng.below(6000);
            runs.push(match rng.below(5) {
                0 => hole(len),
                1 => data(u64::MAX - rng.below(5000), len),
                _ => data(rng.below(disk_len as u64 + 4000), len),
            });
        }
        let covered: u64 = runs.iter().map(Run::len).sum();
        let real = rng.below(covered + 3000);
        let initialized = if rng.below(3) == 0 {
            rng.below(real + 1)
        } else {
            u64::MAX
        };
        let file = extents(runs.clone(), real, initialized);
        let cancel = no_cancel();
        let mut reader = ContentReader::new(&source, &file, &cancel);
        let mut expect_oob = false;
        let mut expect_missing = false;
        let mut whole = Vec::with_capacity(real as usize);
        for pos in 0..real {
            let (byte, oob, missing) = model_byte(&disk_bytes, &range, &runs, initialized, pos);
            whole.push(byte);
            expect_oob |= oob;
            expect_missing |= missing;
        }
        for _ in 0..4 {
            let offset = rng.below(real + 100);
            let len = rng.below(real + 100) as usize;
            let mut out = vec![0xAA; len];
            let n = reader.read_at(offset, &mut out).unwrap();
            let end = (offset + len as u64).min(real);
            let expect = whole
                .get(offset as usize..end.max(offset) as usize)
                .unwrap_or(&[]);
            assert_eq!(
                &out[..n],
                expect,
                "round {round}: offset {offset} len {len} file {file:?} range {range:?}"
            );
        }
        // One complete read decides the flags.
        let mut reader = ContentReader::new(&source, &file, &cancel);
        let mut out = vec![0u8; real as usize];
        reader.read_at(0, &mut out).unwrap();
        assert_eq!(out, whole, "round {round}");
        assert_eq!(
            reader.flags().contains(Flag::OutOfBounds),
            expect_oob,
            "round {round} {file:?} {range:?}"
        );
        assert_eq!(
            reader.flags().contains(Flag::MissingFragments),
            expect_missing,
            "round {round}"
        );
        assert!(!reader.flags().contains(Flag::BadSectors));
        assert_reads_inside(&disk, range);
    }
}

// ---------------------------------------------------------------------------------------------
// The carving adapter
// ---------------------------------------------------------------------------------------------

/// A complete 1x1 PNG (from the hds-carve documentation).
const PNG: [u8; 67] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00, 0x00, 0x3A, 0x7E, 0x9B,
    0x55, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xA8, 0x07, 0x00, 0x00,
    0x81, 0x00, 0x80, 0x7E, 0x1C, 0x29, 0xC7, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

#[test]
fn the_carving_adapter_reads_relative_to_the_candidate_and_stops_at_the_range() {
    let mut bytes = pattern(32_768);
    bytes[12_288..12_288 + PNG.len()].copy_from_slice(&PNG);
    let disk = faulty(bytes.clone());
    let source = ranged(disk.clone(), 4096..20_480);
    let cancel = no_cancel();
    let reader = CarveReader::new(&source, 12_288, u64::MAX, &cancel);
    assert_eq!(reader.len(), 20_480 - 12_288);
    let kind = hds_carve::sniff(&PNG).unwrap();
    let measured =
        hds_carve::try_measure(kind, &reader, reader.len(), &hds_carve::Limits::default()).unwrap();
    assert_eq!(measured.len, 67);
    let mut buf = vec![0u8; 10_000];
    assert_eq!(
        reader.read_at(100, &mut buf).unwrap(),
        20_480 - 12_288 - 100
    );
    assert_eq!(&buf[..50], &bytes[12_388..12_438]);
    assert_eq!(reader.read_at(20_480 - 12_288, &mut buf).unwrap(), 0);
    assert_eq!(reader.read_at(u64::MAX, &mut buf).unwrap(), 0);
    let short = CarveReader::new(&source, 12_288, 1000, &cancel);
    assert_eq!(short.read_at(900, &mut buf).unwrap(), 100);
    assert_eq!(reader.failure(), None);
    // A candidate before the range reads nothing.
    let before = CarveReader::new(&source, 0, 100, &cancel);
    assert_eq!(before.read_at(0, &mut buf).unwrap(), 0);
    assert_reads_inside(&disk, 4096..20_480);
}

#[test]
fn the_carving_adapter_tells_bad_sectors_from_a_lost_device() {
    let disk = faulty(pattern(32_768));
    disk.bad_sectors(13_312..13_824);
    let source = ranged(disk.clone(), 0..32_768);
    let cancel = no_cancel();
    let reader = CarveReader::new(&source, 12_288, 8192, &cancel);
    let mut buf = vec![0u8; 4096];
    assert!(reader.read_at(0, &mut buf).is_err());
    assert_eq!(reader.failure(), Some(CarveFailure::BadSectors));
    disk.clear_faults();
    disk.lose_after_reads(0);
    let reader = CarveReader::new(&source, 12_288, 8192, &cancel);
    assert!(reader.read_at(0, &mut buf).is_err());
    assert_eq!(reader.failure(), Some(CarveFailure::SourceLost));
    disk.revive();
    disk.fail_range(0..32_768, ReadFailure::Caller);
    let reader = CarveReader::new(&source, 12_288, 8192, &cancel);
    assert!(reader.read_at(0, &mut buf).is_err());
    assert_eq!(reader.failure(), Some(CarveFailure::Other));
    disk.clear_faults();
    let cancelled = AtomicBool::new(true);
    let reader = CarveReader::new(&source, 12_288, 8192, &cancelled);
    assert!(reader.read_at(0, &mut buf).is_err());
    assert_eq!(reader.failure(), Some(CarveFailure::Cancelled));
}

// ---------------------------------------------------------------------------------------------
// Previews
// ---------------------------------------------------------------------------------------------

#[test]
fn a_preview_returns_bytes_kind_and_signature() {
    let mut bytes = pattern(32_768);
    bytes[8192..8192 + PNG.len()].copy_from_slice(&PNG);
    let source = ranged(Arc::new(MemSource::new(bytes.clone())), 0..32_768);
    let file = extents(vec![data(8192, 4096)], 3000, 3000);
    let cancel = no_cancel();
    let preview = read_preview(&source, &file, "png", 0, 16, &cancel).unwrap();
    assert_eq!(preview.data, &PNG[..16]);
    assert_eq!(preview.size, 3000);
    assert!(!preview.eof);
    assert_eq!(preview.kind, Some(FileKind::Png));
    assert_eq!(preview.flags, Flag::SignatureOk.into());
    let wrong = read_preview(&source, &file, "jpg", 0, 4096, &cancel).unwrap();
    assert_eq!(wrong.data.len(), 3000);
    assert!(wrong.eof);
    assert_eq!(wrong.flags, Flag::SignatureMismatch.into());
    let no_opinion = read_preview(&source, &file, "txt", 0, 100, &cancel).unwrap();
    assert!(no_opinion.flags.is_empty());
    let later = read_preview(&source, &file, "png", 2990, 100, &cancel).unwrap();
    assert_eq!(later.data, &bytes[8192 + 2990..8192 + 3000]);
    assert!(later.eof);
    assert_eq!(later.kind, None);
    assert!(
        later.flags.is_empty(),
        "no signature check away from offset 0"
    );
    let beyond = read_preview(&source, &file, "png", 5000, 100, &cancel).unwrap();
    assert!(beyond.data.is_empty() && beyond.eof);
    assert!(matches!(
        read_preview(&source, &file, "png", 0, MAX_PREVIEW_LEN + 1, &cancel),
        Err(PreviewError::TooLong { .. })
    ));
    read_preview(&source, &file, "png", 0, MAX_PREVIEW_LEN, &cancel).unwrap();
}

#[test]
fn a_short_preview_still_checks_the_first_kilobyte() {
    // A PDF whose header follows 600 bytes of junk: a 10-byte preview must not call it a mismatch.
    let mut bytes = vec![b' '; 16_384];
    bytes[4096 + 600..4096 + 608].copy_from_slice(b"%PDF-1.7");
    let source = ranged(Arc::new(MemSource::new(bytes)), 0..16_384);
    let file = extents(vec![data(4096, 4096)], 4096, 4096);
    let cancel = no_cancel();
    let preview = read_preview(&source, &file, "pdf", 0, 10, &cancel).unwrap();
    assert_eq!(preview.data.len(), 10);
    assert_eq!(preview.flags, Flag::SignatureOk.into());
}

#[test]
fn a_preview_reports_what_the_read_met() {
    let disk = faulty(pattern(32_768));
    disk.bad_sectors(8704..9216);
    let source = ranged(disk, 4096..16_384);
    let file = extents(vec![data(8192, 4096), data(16_000, 1000)], 5096, 5096);
    let cancel = no_cancel();
    let preview = read_preview(&source, &file, "bin", 0, 8192, &cancel).unwrap();
    assert_eq!(
        preview.flags,
        FlagSet::of(&[Flag::BadSectors, Flag::OutOfBounds])
    );
    assert_eq!(preview.report.bad_bytes(), 512);
    assert_eq!(preview.report.out_of_bounds_bytes(), 616);
    assert!(preview.data[512..1024].iter().all(|&b| b == 0));
}

#[test]
fn the_signature_flag_follows_matches_extension() {
    assert_eq!(signature_flag(&PNG, 67, "png"), Some(Flag::SignatureOk));
    assert_eq!(
        signature_flag(&PNG, 67, "jpg"),
        Some(Flag::SignatureMismatch)
    );
    assert_eq!(signature_flag(&PNG, 67, "txt"), None);
    assert_eq!(
        signature_flag(&[], 0, "png"),
        None,
        "an empty file has no opinion"
    );
}

#[test]
fn a_source_with_an_impossible_sector_size_is_an_error_not_a_hang() {
    let huge = 16 << 20;
    let source = ranged(
        Arc::new(MemSource::with_sector_sizes(
            pattern(huge),
            huge as u32,
            huge as u32,
        )),
        0..huge as u64,
    );
    let file = extents(vec![data(100, 100)], 100, 100);
    let cancel = no_cancel();
    let mut reader = ContentReader::new(&source, &file, &cancel);
    assert!(matches!(
        reader.read_at(0, &mut [0u8; 100]),
        Err(ReadError::Io(_))
    ));
}
