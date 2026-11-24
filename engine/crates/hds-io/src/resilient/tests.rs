use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::*;
use crate::rangeset::RangeSet;
use crate::source::RangedSource;
use crate::testing::{FaultySource, MemSource, XorShift};

const MIB: usize = 1 << 20;

fn random_data(len: usize, seed: u64) -> Vec<u8> {
    let mut data = vec![0u8; len];
    XorShift::new(seed).fill(&mut data);
    // No sector may be all zeros by accident: zero means "unreadable" in these tests.
    for sector in data.chunks_mut(512) {
        sector[0] |= 1;
    }
    data
}

fn faulty(data: &[u8], logical: u32, physical: u32) -> FaultySource {
    FaultySource::new(Arc::new(MemSource::with_sector_sizes(
        data.to_vec(),
        logical,
        physical,
    )))
}

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn expect_data(outcome: ReadOutcome) -> Vec<Range<u64>> {
    match outcome {
        ReadOutcome::Data { bad } => bad,
        other => panic!("expected data, got {other:?}"),
    }
}

/// Checks a buffer against the truth: zeros exactly in the bad ranges, the
/// source's bytes everywhere else.
fn assert_buffer(buf: &[u8], offset: u64, data: &[u8], bad: &[Range<u64>]) {
    let mut bad_set = RangeSet::new();
    bad_set.extend(bad.iter().cloned());
    let end = offset + buf.len() as u64;
    for r in bad_set.intersection(offset..end) {
        let slice = &buf[(r.start - offset) as usize..(r.end - offset) as usize];
        assert!(
            slice.iter().all(|&b| b == 0),
            "bad range {r:?} is not zero-filled"
        );
    }
    for r in bad_set.gaps(offset..end) {
        let (a, b) = ((r.start - offset) as usize, (r.end - offset) as usize);
        assert!(
            buf[a..b] == data[r.start as usize..r.end as usize],
            "good range {r:?} differs from the source"
        );
    }
}

#[test]
fn a_clean_read_is_one_request() {
    let data = random_data(8 * MIB, 1);
    let src = faulty(&data, 512, 512);
    let mut buf = AlignedBuf::new(8 * MIB);
    let outcome = read_resilient(&src, 0, &mut buf, &no_cancel());
    assert!(outcome.is_clean());
    assert_eq!(outcome.bad_ranges(), Some(&[][..]));
    assert_eq!(&buf[..], &data[..]);
    assert_eq!(src.read_log(), vec![(0, 8 * MIB)]);
    assert_eq!(
        src.liveness_checks(),
        0,
        "a healthy read needs no liveness check"
    );
    // An empty request touches nothing.
    assert!(read_resilient(&src, 4096, &mut [], &no_cancel()).is_clean());
    assert_eq!(src.read_count(), 1);
}

#[test]
fn scattered_bad_sectors_become_exact_bad_ranges() {
    let data = random_data(8 * MIB, 2);
    let src = faulty(&data, 512, 512);
    // A single sector, two neighbours, the first and the last sector of the chunk,
    // sectors on both sides of a 64 KiB boundary, seven in a row.
    let injected = [
        0u64..512,
        (4 * 65536 - 512)..(4 * 65536 + 512),
        1_000_448..1_000_960,
        2_000_384..2_001_408,
        5_000_192..5_003_776,
        (8 * MIB as u64 - 512)..(8 * MIB as u64),
    ];
    for r in &injected {
        src.bad_sectors(r.clone());
    }
    let mut buf = AlignedBuf::new(8 * MIB);
    buf.fill(0x77);
    let bad = expect_data(read_resilient(&src, 0, &mut buf, &no_cancel()));
    assert_eq!(bad, injected.to_vec());
    assert_buffer(&buf, 0, &data, &bad);
    // Every bad sector was confirmed against a living device before it was declared.
    let bad_sectors: u64 = injected.iter().map(|r| (r.end - r.start) / 512).sum();
    assert!(
        src.liveness_checks() >= bad_sectors,
        "{} checks for {bad_sectors} bad sectors",
        src.liveness_checks()
    );
    // Nothing outside the request was read.
    assert_eq!(src.touched().span(), Some(0..8 * MIB as u64));
}

#[test]
fn retries_use_64k_blocks_then_physical_sectors() {
    let data = random_data(MIB, 3);
    // 512e: a damaged physical sector takes all eight logical sectors with it.
    let src = faulty(&data, 512, 4096);
    src.bad_sectors(200_704..201_216); // one logical sector inside block 3 (196608..262144)
    let mut buf = AlignedBuf::new(MIB);
    let bad = expect_data(read_resilient(&src, 0, &mut buf, &no_cancel()));
    assert_eq!(
        bad,
        vec![200_704..204_800],
        "the whole physical sector is reported"
    );
    assert_buffer(&buf, 0, &data, &bad);

    let log = src.read_log();
    assert_eq!(log[0], (0, MIB), "first the whole request");
    // Then sixteen 64 KiB blocks; the failing one is followed by its sixteen physical sectors.
    let blocks: Vec<_> = log[1..].iter().filter(|(_, len)| *len == 65536).collect();
    assert_eq!(blocks.len(), 16);
    assert!(blocks
        .iter()
        .enumerate()
        .all(|(i, (offset, _))| *offset == i as u64 * 65536));
    let units: Vec<_> = log[1..]
        .iter()
        .filter(|(_, len)| *len == 4096)
        .cloned()
        .collect();
    assert_eq!(
        units,
        (0..16)
            .map(|i| (196_608 + i * 4096, 4096usize))
            .collect::<Vec<_>>()
    );
    assert_eq!(log.len(), 1 + 16 + 16);
}

#[test]
fn blocks_and_units_follow_the_source_grid_at_odd_offsets() {
    let data = random_data(MIB, 4);
    let src = faulty(&data, 512, 4096);
    // A partition that starts at sector 63: nothing is aligned to 4 KiB.
    let start = 63 * 512u64;
    let len = 300 * 512usize;
    src.bad_sectors(40_960..41_472);
    let mut buf = AlignedBuf::new(len);
    let bad = expect_data(read_resilient(&src, start, &mut buf, &no_cancel()));
    // The physical sector 40960..45056 lies completely inside the request.
    assert_eq!(bad, vec![40_960..45_056]);
    assert_buffer(&buf, start, &data, &bad);
    let log = src.read_log();
    // Whole request, then the piece up to the first 64 KiB boundary, in which the
    // failure lies, then physical sectors clipped to that piece.
    assert_eq!(log[0], (start, len));
    assert_eq!(log[1], (start, 65536 - start as usize));
    assert_eq!(
        log[2],
        (start, 512),
        "from sector 63 to the 4 KiB boundary at 32768"
    );
    assert_eq!(log[3], (32768, 4096));
    assert_eq!(
        src.touched().span(),
        Some(start..start + len as u64),
        "no read leaves the request"
    );

    // A bad physical sector that straddles the start of the request is clipped to it.
    let src = faulty(&data, 512, 4096);
    src.bad_sectors(28_672..29_184); // logical sector 56, in the physical sector 28672..32768
    let mut buf = AlignedBuf::new(len);
    let bad = expect_data(read_resilient(&src, start, &mut buf, &no_cancel()));
    assert!(
        bad.is_empty(),
        "the bad sector is before the request: {bad:?}"
    );
    src.bad_sectors(32_256..32_768); // logical sector 63 itself
    let bad = expect_data(read_resilient(&src, start, &mut buf, &no_cancel()));
    assert_eq!(bad, vec![32_256..32_768]);
    assert_buffer(&buf, start, &data, &bad);
}

#[test]
fn eight_consecutive_bad_sectors_give_up_the_rest_of_the_block() {
    let data = random_data(MIB, 5);
    let src = faulty(&data, 512, 512);
    // A long run: 300 KiB starting 10 sectors into block 2.
    let run = (2 * 65536 + 5120)..(2 * 65536 + 5120 + 300 * 1024);
    src.bad_sectors(run.clone());
    let mut buf = AlignedBuf::new(MIB);
    let bad = expect_data(read_resilient(&src, 0, &mut buf, &no_cancel()));
    // Block 2 is given up after its eighth bad sector; blocks 3 to 5 are all bad;
    // the run ends inside block 6 (at 436224), whose rest is given up as well.
    assert_eq!(bad, vec![run.start..7 * 65536]);
    assert_buffer(&buf, 0, &data, &bad);

    let log = src.read_log();
    let reads_in = |block: u64| {
        log[1..]
            .iter()
            .filter(|(o, _)| (o / 65536) == block)
            .count()
    };
    // Block 2: the block, ten good sectors, eight bad ones. Blocks 3-6: the block and eight sectors.
    assert_eq!(reads_in(2), 1 + 10 + 8);
    for block in 3..=6 {
        assert_eq!(reads_in(block), 1 + 8, "block {block}");
    }
    for block in (0..2).chain(7..16) {
        assert_eq!(reads_in(block), 1, "block {block} is healthy");
    }
}

#[test]
fn seven_bad_sectors_do_not_trigger_the_rule() {
    let data = random_data(MIB, 6);
    let src = faulty(&data, 512, 512);
    // Seven bad, one good, seven bad: the counter starts over after the good sector.
    src.bad_sectors(65536..65536 + 7 * 512);
    src.bad_sectors(65536 + 8 * 512..65536 + 15 * 512);
    let mut buf = AlignedBuf::new(MIB);
    let bad = expect_data(read_resilient(&src, 0, &mut buf, &no_cancel()));
    assert_eq!(
        bad,
        vec![65536..65536 + 7 * 512, 65536 + 8 * 512..65536 + 15 * 512]
    );
    assert_buffer(&buf, 0, &data, &bad);

    // Exactly eight: the readable rest of the block is given up, by the rule.
    let src = faulty(&data, 512, 512);
    src.bad_sectors(65536..65536 + 8 * 512);
    let bad = expect_data(read_resilient(&src, 0, &mut buf, &no_cancel()));
    assert_eq!(bad, vec![65536..2 * 65536]);
    assert_buffer(&buf, 0, &data, &bad);

    // Eight bad sectors at the very end of a block give up nothing extra.
    let src = faulty(&data, 512, 512);
    src.bad_sectors(2 * 65536 - 8 * 512..2 * 65536);
    let bad = expect_data(read_resilient(&src, 0, &mut buf, &no_cancel()));
    assert_eq!(bad, vec![2 * 65536 - 8 * 512..2 * 65536]);
}

#[test]
fn a_lost_device_is_never_a_bad_sector() {
    let data = random_data(MIB, 7);
    // Lost before the first request.
    let src = faulty(&data, 512, 512);
    src.lose_after_reads(0);
    let mut buf = AlignedBuf::new(MIB);
    assert!(matches!(
        read_resilient(&src, 0, &mut buf, &no_cancel()),
        ReadOutcome::SourceLost(_)
    ));
    assert_eq!(
        src.read_count(),
        1,
        "no retries against a device that is gone"
    );

    // Lost in the middle of a chunk, while bad sectors are being worked around.
    for reads_before_loss in 1..40 {
        let src = faulty(&data, 512, 512);
        src.bad_sectors(100_000 / 512 * 512..100_000 / 512 * 512 + 4096);
        src.lose_after_reads(reads_before_loss);
        let outcome = read_resilient(&src, 0, &mut buf, &no_cancel());
        // Whole request (1), blocks 0 and 1 (2), 67 good sectors of block 1, 8 bad
        // ones, 14 more blocks: the device is always gone before the call is done.
        assert!(
            matches!(outcome, ReadOutcome::SourceLost(_)),
            "after {reads_before_loss} reads: {outcome:?}"
        );
    }
}

#[test]
fn a_device_that_drops_off_with_io_errors_is_caught_by_the_liveness_check() {
    let data = random_data(MIB, 8);
    // The bus is going down: requests fail like bad sectors, but the device no
    // longer answers. One real bad sector makes sure the call has work to do.
    for reads_before_loss in 0..40 {
        let src = faulty(&data, 512, 512);
        src.bad_sectors(99_840..100_352);
        src.lose_silently_after_reads(reads_before_loss);
        let mut buf = AlignedBuf::new(MIB);
        let outcome = read_resilient(&src, 0, &mut buf, &no_cancel());
        match outcome {
            ReadOutcome::SourceLost(e) => assert_eq!(classify_read_error(&e), ReadFailure::Media),
            other => panic!("after {reads_before_loss} reads: {other:?}"),
        }
        assert!(src.liveness_checks() >= 1);
    }

    // A device that dies after it has served the whole request did serve it.
    let src = faulty(&data, 512, 512);
    src.lose_silently_after_reads(1);
    let mut buf = AlignedBuf::new(MIB);
    assert!(read_resilient(&src, 0, &mut buf, &no_cancel()).is_clean());
    assert_eq!(&buf[..], &data[..]);

    // The device dies right after the last bad sector was confirmed: the bad ranges
    // collected so far are not trusted either.
    let src = faulty(&data, 512, 512);
    src.bad_sectors(MIB as u64 - 512..MIB as u64);
    let bad = expect_data(read_resilient(&src, 0, &mut buf, &no_cancel()));
    assert_eq!(bad, vec![MIB as u64 - 512..MIB as u64]);
    assert_eq!(
        src.liveness_checks(),
        2,
        "once for the sector, once before the result is handed out"
    );
    src.reset_counters();
    src.lose_after_liveness_checks(1);
    let outcome = read_resilient(&src, 0, &mut buf, &no_cancel());
    assert!(matches!(outcome, ReadOutcome::SourceLost(_)), "{outcome:?}");
    assert_eq!(src.liveness_checks(), 2);
}

#[test]
fn cancellation_is_honoured_before_and_between_retries() {
    let data = random_data(MIB, 9);
    let src = faulty(&data, 512, 512);
    let mut buf = AlignedBuf::new(MIB);
    let cancelled = AtomicBool::new(true);
    assert!(matches!(
        read_resilient(&src, 0, &mut buf, &cancelled),
        ReadOutcome::Cancelled
    ));
    assert_eq!(src.read_count(), 0, "a cancelled job starts no request");

    // Raised during the sector-by-sector retries of a long bad run.
    for raise_at in [1u64, 2, 3, 5, 9, 20] {
        let src = faulty(&data, 512, 512);
        src.bad_sectors(0..MIB as u64 / 2);
        let flag = Arc::new(AtomicBool::new(false));
        src.cancel_after_reads(raise_at, flag.clone());
        let outcome = read_resilient(&src, 0, &mut buf, &flag);
        assert!(
            matches!(outcome, ReadOutcome::Cancelled),
            "raised at read {raise_at}: {outcome:?}"
        );
        assert_eq!(
            src.read_count(),
            raise_at,
            "the flag is polled before every retry"
        );
    }

    // Raised by the last request of a call that then succeeds: the data is delivered.
    let src = faulty(&data, 512, 512);
    let flag = Arc::new(AtomicBool::new(false));
    src.cancel_after_reads(1, flag.clone());
    assert!(read_resilient(&src, 0, &mut buf, &flag).is_clean());
    assert!(flag.load(Ordering::SeqCst));

    // A request cancelled by the system (CancelSynchronousIo) ends the call the same way.
    let src = faulty(&data, 512, 512);
    src.fail_range(4096..8192, ReadFailure::Cancelled);
    assert!(matches!(
        read_resilient(&src, 0, &mut buf, &no_cancel()),
        ReadOutcome::Cancelled
    ));
}

#[test]
fn caller_errors_are_internal_errors_not_bad_sectors() {
    let data = random_data(MIB, 10);
    let src = faulty(&data, 512, 512);
    let mut buf = AlignedBuf::new(8192);
    for (offset, len) in [
        (1u64, 512usize),
        (0, 100),
        (MIB as u64, 512),
        (MIB as u64 - 512, 1024),
        (u64::MAX - 511, 512),
    ] {
        let outcome = read_resilient(&src, offset, &mut buf[..len], &no_cancel());
        assert!(
            matches!(outcome, ReadOutcome::Internal(_)),
            "{offset}+{len}: {outcome:?}"
        );
    }
    assert_eq!(
        src.read_count(),
        0,
        "invalid requests never reach the device"
    );

    // The device itself says "invalid parameter".
    src.fail_range(4096..8192, ReadFailure::Caller);
    let outcome = read_resilient(&src, 0, &mut buf, &no_cancel());
    assert!(matches!(outcome, ReadOutcome::Internal(_)), "{outcome:?}");
    assert_eq!(src.liveness_checks(), 0);

    // A read outside the session range.
    let inner: Arc<dyn BlockSource> = Arc::new(MemSource::new(data));
    let ranged = RangedSource::new(inner, 4096..65536).unwrap();
    let outcome = read_resilient(&ranged, 0, &mut buf, &no_cancel());
    match outcome {
        ReadOutcome::Internal(e) => assert!(crate::OutOfRange::from_io(&e).is_some()),
        other => panic!("{other:?}"),
    }
    assert!(read_resilient(&ranged, 4096, &mut buf, &no_cancel()).is_clean());
}

#[test]
fn resource_errors_shrink_the_transfer() {
    let data = random_data(MIB, 11);
    let src = faulty(&data, 512, 512);
    let mut buf = AlignedBuf::new(MIB);
    // The driver stack cannot take 1 MiB at once, but 64 KiB is fine.
    src.set_max_transfer(Some(65536));
    assert!(read_resilient(&src, 0, &mut buf, &no_cancel()).is_clean());
    assert_eq!(&buf[..], &data[..]);
    assert_eq!(src.read_count(), 1 + 16);

    // Not even 64 KiB: sector by sector, still without any bad range.
    src.reset_counters();
    src.set_max_transfer(Some(4096));
    buf.fill(0);
    assert!(read_resilient(&src, 0, &mut buf, &no_cancel()).is_clean());
    assert_eq!(&buf[..], &data[..]);
    assert_eq!(
        src.liveness_checks(),
        0,
        "a lack of resources is not probed as a bad sector"
    );

    // Not even one sector: an internal error after a bounded number of attempts.
    src.reset_counters();
    src.set_max_transfer(Some(256));
    let outcome = read_resilient(&src, 0, &mut buf[..65536], &no_cancel());
    match outcome {
        ReadOutcome::Internal(e) => assert_eq!(classify_read_error(&e), ReadFailure::Resource),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        src.read_count(),
        1 + 3,
        "the request, then three tries of the first sector"
    );

    // A single-sector request: the failure of the request is the first try.
    src.reset_counters();
    let outcome = read_resilient(&src, 0, &mut buf[..512], &no_cancel());
    assert!(matches!(outcome, ReadOutcome::Internal(_)), "{outcome:?}");
    assert_eq!(src.read_count(), 3);
}

#[test]
fn unknown_errors_count_as_media_errors_on_a_living_device() {
    let data = random_data(MIB, 12);
    let src = faulty(&data, 512, 512);
    src.fail_range(8192..8704, ReadFailure::Unknown);
    let mut buf = AlignedBuf::new(65536);
    let bad = expect_data(read_resilient(&src, 0, &mut buf, &no_cancel()));
    assert_eq!(bad, vec![8192..8704]);
    assert_buffer(&buf, 0, &data, &bad);
    assert!(src.liveness_checks() >= 1);
}

#[test]
fn small_requests_do_not_repeat_the_failed_read() {
    let data = random_data(MIB, 13);
    let src = faulty(&data, 4096, 4096);
    src.bad_sectors(8192..12288);
    let mut buf = AlignedBuf::new(4096);
    // One sector: the request, its failure is the sector's failure.
    let bad = expect_data(read_resilient(&src, 8192, &mut buf, &no_cancel()));
    assert_eq!(bad, vec![8192..12288]);
    assert!(buf.iter().all(|&b| b == 0));
    assert_eq!(src.read_count(), 1);
    // One block of several sectors: the request, then the sectors.
    src.reset_counters();
    let mut buf = AlignedBuf::new(16384);
    let bad = expect_data(read_resilient(&src, 0, &mut buf, &no_cancel()));
    assert_eq!(bad, vec![8192..12288]);
    assert_buffer(&buf, 0, &data, &bad);
    assert_eq!(src.read_count(), 1 + 4);
}

#[test]
fn random_faults_never_corrupt_good_data() {
    let data = random_data(4 * MIB, 14);
    for seed in 1..=12u64 {
        let mut rng = XorShift::new(seed);
        let (logical, physical) = [(512, 512), (512, 4096), (4096, 4096)][rng.below(3) as usize];
        let src = faulty(&data, logical, physical);
        let mut injected = RangeSet::new();
        for _ in 0..rng.below(12) {
            let start = rng.below(4 * MIB as u64 / u64::from(logical)) * u64::from(logical);
            let len = (rng.below(6) + 1) * u64::from(logical);
            let end = (start + len).min(4 * MIB as u64);
            src.bad_sectors(start..end);
            injected.insert(start..end);
        }
        let total_sectors = 4 * MIB as u64 / u64::from(logical);
        let first = rng.below(total_sectors - 1);
        let count = rng.below(total_sectors - first) + 1;
        let (offset, len) = (
            first * u64::from(logical),
            (count * u64::from(logical)) as usize,
        );
        let mut buf = AlignedBuf::new(len);
        buf.fill(0x5C);
        let bad = expect_data(read_resilient(&src, offset, &mut buf, &no_cancel()));
        let request = offset..offset + len as u64;

        assert_buffer(&buf, offset, &data, &bad);
        let mut reported = RangeSet::new();
        for (i, r) in bad.iter().enumerate() {
            assert!(
                r.start < r.end && r.start >= request.start && r.end <= request.end,
                "{r:?} outside {request:?}"
            );
            assert!(r.start % u64::from(logical) == 0 && r.end % u64::from(logical) == 0);
            if i > 0 {
                assert!(
                    bad[i - 1].end < r.start,
                    "bad ranges must be ascending and merged"
                );
            }
            reported.insert(r.clone());
        }
        // Every unreadable sector of the request is reported ...
        for r in injected.intersection(request.clone()) {
            assert!(
                reported.contains_range(r.clone()),
                "seed {seed}: {r:?} was not reported"
            );
        }
        // ... and whatever else is given up lies in a 64 KiB block that has a bad sector.
        for r in &bad {
            let mut block = r.start / 65536 * 65536;
            while block < r.end {
                assert!(
                    injected.intersects(block..block + 65536),
                    "seed {seed}: healthy block {block} given up"
                );
                block += 65536;
            }
        }
        assert!(request.start <= src.touched().first().unwrap().start);
        assert!(src.touched().last().unwrap().end <= request.end);
    }
}

#[test]
fn read_unaligned_reads_any_byte_range() {
    let data = random_data(2 * MIB + 4096, 15);
    let mut rng = XorShift::new(99);
    for (logical, physical) in [(512u32, 512u32), (512, 4096), (4096, 4096)] {
        let src = faulty(&data, logical, physical);
        let size = src.size();
        for _ in 0..60 {
            let offset = rng.below(size);
            let len = match rng.below(4) {
                0 => rng.below(64),
                1 => rng.below(5000),
                _ => rng.below(300_000),
            }
            .min(size - offset) as usize;
            let mut buf = vec![0xA5u8; len];
            let bad = expect_data(read_unaligned(&src, offset, &mut buf, &no_cancel()));
            assert!(bad.is_empty());
            assert!(
                buf == data[offset as usize..offset as usize + len],
                "{len} bytes at {offset}"
            );
        }
        // Reads are sector-aligned on the device and stay inside the covering sectors.
        for (offset, len) in src.read_log() {
            assert_eq!(offset % u64::from(logical), 0);
            assert_eq!(len as u64 % u64::from(logical), 0);
        }
    }
}

#[test]
fn read_unaligned_handles_large_ranges_bad_sectors_and_bounds() {
    let data = random_data(4 * MIB, 16);
    let src = faulty(&data, 4096, 4096);
    src.bad_sectors(MIB as u64..MIB as u64 + 4096);
    src.bad_sectors(3 * MIB as u64 + 8192..3 * MIB as u64 + 12288);

    // A range of several bounce buffers that starts and ends inside sectors.
    let (offset, len) = (1000u64, 3 * MIB + 777);
    let mut buf = vec![0xA5u8; len];
    let bad = expect_data(read_unaligned(&src, offset, &mut buf, &no_cancel()));
    assert_eq!(bad, vec![MIB as u64..MIB as u64 + 4096]);
    assert_buffer(&buf, offset, &data, &bad);
    assert!(
        src.read_log().iter().all(|&(_, len)| len <= MIB),
        "pieces of at most 1 MiB"
    );

    // Bad ranges are clipped to the request.
    let (offset, len) = (MIB as u64 + 100, 200usize);
    let mut buf = vec![0xA5u8; len];
    let bad = expect_data(read_unaligned(&src, offset, &mut buf, &no_cancel()));
    assert_eq!(bad, vec![offset..offset + 200]);
    assert!(buf.iter().all(|&b| b == 0));
    let (offset, len) = (MIB as u64 - 10, 20usize);
    let mut buf = vec![0xA5u8; len];
    let bad = expect_data(read_unaligned(&src, offset, &mut buf, &no_cancel()));
    assert_eq!(bad, vec![MIB as u64..MIB as u64 + 10]);
    assert_buffer(&buf, offset, &data, &bad);

    // The last bytes of the source, an empty request, and requests beyond the end.
    let mut tail = [0u8; 100];
    assert!(read_unaligned(&src, 4 * MIB as u64 - 100, &mut tail, &no_cancel()).is_clean());
    assert_eq!(&tail[..], &data[4 * MIB - 100..]);
    assert!(read_unaligned(&src, 4 * MIB as u64, &mut [], &no_cancel()).is_clean());
    for (offset, len) in [
        (4 * MIB as u64 - 50, 100usize),
        (4 * MIB as u64, 1),
        (u64::MAX, 1),
        (u64::MAX - 5, 10),
    ] {
        let mut buf = vec![0u8; len];
        let outcome = read_unaligned(&src, offset, &mut buf, &no_cancel());
        assert!(
            matches!(outcome, ReadOutcome::Internal(_)),
            "{offset}+{len}: {outcome:?}"
        );
    }

    // An aligned request into aligned memory goes straight through.
    src.reset_counters();
    let mut aligned = AlignedBuf::new(8192);
    assert!(read_unaligned(&src, 8192, &mut aligned, &no_cancel()).is_clean());
    assert_eq!(src.read_log(), vec![(8192, 8192)]);

    // Loss and cancellation pass through unchanged.
    src.lose_after_reads(0);
    let mut buf = [0u8; 10];
    assert!(matches!(
        read_unaligned(&src, 5, &mut buf, &no_cancel()),
        ReadOutcome::SourceLost(_)
    ));
    assert!(matches!(
        read_unaligned(&src, 5, &mut buf, &AtomicBool::new(true)),
        ReadOutcome::Cancelled
    ));
}

#[test]
fn windows_error_codes_are_classified_as_in_the_notes() {
    use ReadFailure::*;
    let table: &[(&[i32], ReadFailure)] = &[
        (
            &[23, 1117, 121, 483, 1126, 1127, 1128, 25, 30, 27, 1111],
            Media,
        ),
        (
            &[21, 1167, 433, 2, 55, 1617, 1110, 1112, 321, 31, 20, 6, 5],
            DeviceLost,
        ),
        (&[87, 1, 50, 38, 998, 1784], Caller),
        (&[1450, 1453, 8, 1816], Resource),
        (&[995], Cancelled),
        (&[0, 3, 4390, 170, 1785, -1, i32::MAX, i32::MIN], Unknown),
    ];
    for (codes, class) in table {
        for &code in *codes {
            assert_eq!(classify_windows_error(code), *class, "error {code}");
        }
    }
}

#[test]
fn ambiguous_windows_loss_codes_need_a_failed_liveness_check() {
    // Codes a present disk can answer for one bad sector (a USB bridge that
    // reports a medium error as STATUS_UNSUCCESSFUL, ...): the device is asked
    // before the job ends.
    for code in [31, 21, 20, 5] {
        assert_eq!(classify_windows_error(code), ReadFailure::DeviceLost);
        assert!(windows_loss_needs_proof(code), "error {code}");
    }
    // Codes no present device gives: the job ends at once.
    for code in [1167, 433, 2, 55, 1617, 1110, 1112, 321, 6] {
        assert_eq!(classify_windows_error(code), ReadFailure::DeviceLost);
        assert!(!windows_loss_needs_proof(code), "error {code}");
    }
    for code in [23, 1117, 87, 995, 0] {
        assert!(!windows_loss_needs_proof(code), "error {code}");
    }
}

#[cfg(unix)]
#[test]
fn unix_error_codes_are_classified() {
    use ReadFailure::*;
    assert_eq!(classify_unix_error(libc::EIO), Media);
    for code in [libc::ENXIO, libc::ENODEV, libc::EBADF, libc::ESTALE] {
        assert_eq!(classify_unix_error(code), DeviceLost);
    }
    assert_eq!(classify_unix_error(libc::EINVAL), Caller);
    assert_eq!(classify_unix_error(libc::ENOMEM), Resource);
    assert_eq!(classify_unix_error(libc::ECANCELED), Cancelled);
    assert_eq!(classify_unix_error(libc::EPERM), Unknown);
    #[cfg(target_os = "linux")]
    assert_eq!(classify_unix_error(libc::ENOMEDIUM), DeviceLost);
    // Through io::Error, the way a failed pread arrives.
    assert_eq!(
        classify_read_error(&io::Error::from_raw_os_error(libc::EIO)),
        Media
    );
}

#[cfg(windows)]
#[test]
fn os_errors_go_through_the_windows_table() {
    assert_eq!(
        classify_read_error(&io::Error::from_raw_os_error(23)),
        ReadFailure::Media
    );
    assert_eq!(
        classify_read_error(&io::Error::from_raw_os_error(1167)),
        ReadFailure::DeviceLost
    );
    assert_eq!(
        classify_read_error(&io::Error::from_raw_os_error(87)),
        ReadFailure::Caller
    );
}

#[test]
fn errors_without_an_os_code_are_classified_by_origin_and_kind() {
    use ReadFailure::*;
    for class in [Media, DeviceLost, Caller, Resource, Cancelled, Unknown] {
        let e = read_error(class, "made here");
        assert_eq!(classify_read_error(&e), class);
        assert_eq!(e.to_string(), "made here");
    }
    assert_eq!(
        classify_read_error(&io::Error::new(io::ErrorKind::UnexpectedEof, "short")),
        Caller
    );
    assert_eq!(
        classify_read_error(&io::Error::new(io::ErrorKind::InvalidInput, "bad")),
        Caller
    );
    assert_eq!(
        classify_read_error(&io::Error::new(io::ErrorKind::OutOfMemory, "oom")),
        Resource
    );
    assert_eq!(classify_read_error(&io::Error::other("who knows")), Unknown);
    assert_eq!(
        classify_read_error(&io::Error::from(io::ErrorKind::TimedOut)),
        Unknown
    );
}
