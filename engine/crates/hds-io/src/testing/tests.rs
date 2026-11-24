use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::resilient::classify_read_error;

fn disk(sectors: usize) -> Arc<MemSource> {
    let mut data = vec![0u8; sectors * 512];
    for (i, sector) in data.chunks_mut(512).enumerate() {
        sector.fill(i as u8);
    }
    Arc::new(MemSource::new(data))
}

#[test]
fn xorshift_is_deterministic_and_spreads() {
    let mut a = XorShift::new(42);
    let mut b = XorShift::new(42);
    let mut c = XorShift::new(43);
    let seq_a: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
    let seq_b: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
    let seq_c: Vec<u64> = (0..16).map(|_| c.next_u64()).collect();
    assert_eq!(seq_a, seq_b);
    assert_ne!(seq_a, seq_c);
    assert_eq!(a.below(0), 0);
    assert!((0..1000).all(|_| a.below(7) < 7));
    let mut seen = [false; 7];
    (0..200).for_each(|_| seen[a.below(7) as usize] = true);
    assert!(seen.iter().all(|&s| s));
    let mut buf = [0u8; 37];
    XorShift::new(0).fill(&mut buf);
    assert!(buf.iter().any(|&b| b != 0));
}

#[test]
fn mem_source_reads_and_validates() {
    let src = MemSource::new(vec![9u8; 1000]);
    assert_eq!(src.size(), 1024, "padded to whole sectors");
    assert_eq!(&src.data()[1000..], &[0u8; 24]);
    let mut buf = [0u8; 512];
    src.read_at(512, &mut buf).unwrap();
    assert_eq!(&buf[..488], &[9u8; 488]);
    assert_eq!(&buf[488..], &[0u8; 24]);
    for (offset, len) in [(1u64, 512usize), (0, 100), (1024, 512), (512, 1024)] {
        let mut buf = vec![0u8; len];
        let err = src.read_at(offset, &mut buf).unwrap_err();
        assert_eq!(classify_read_error(&err), ReadFailure::Caller);
    }
    let info = src.info();
    assert_eq!(
        (info.kind, info.path.as_str(), info.size),
        (SourceKind::Image, "mem:test", 1024)
    );
    assert_eq!(src.location(), SourceLocation::Detached);
    assert!(src.is_alive());

    let four_k = MemSource::with_sector_sizes(vec![1u8; 5000], 4096, 512);
    assert_eq!(
        (
            four_k.size(),
            four_k.sector_size(),
            four_k.physical_sector_size()
        ),
        (8192, 4096, 4096)
    );
    let odd = MemSource::with_sector_sizes(vec![1u8; 10], 0, 0);
    assert_eq!(
        (odd.size(), odd.sector_size(), odd.physical_sector_size()),
        (512, 512, 512)
    );
    let as_disk = MemSource::new(vec![0; 512])
        .at_location(SourceLocation::Disk { number: 3 })
        .named("d");
    assert_eq!(as_disk.info().kind, SourceKind::Disk);
    assert_eq!(as_disk.info().disk_number, Some(3));
    assert_eq!(as_disk.info().path, "mem:d");
}

#[test]
fn faulty_source_fails_reads_that_touch_a_bad_range() {
    let src = FaultySource::new(disk(64));
    src.bad_sectors(1024..1536);
    let mut buf = vec![0u8; 512];
    src.read_at(512, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 1));
    src.read_at(1536, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 3));
    // Exactly the bad sector, and every request that includes it.
    for (offset, len) in [(1024u64, 512usize), (0, 2048), (512, 1024), (1024, 4096)] {
        let mut buf = vec![0u8; len];
        let err = src.read_at(offset, &mut buf).unwrap_err();
        assert_eq!(classify_read_error(&err), ReadFailure::Media);
        assert!(
            buf.iter().all(|&b| b == 0xEE),
            "a failed read leaves garbage, not data"
        );
    }
    assert!(src.is_alive());
    src.clear_faults();
    src.read_at(1024, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 2));
}

#[test]
fn faulty_source_injects_every_class() {
    let src = FaultySource::new(disk(64));
    src.fail_range(0..512, ReadFailure::Caller);
    src.fail_range(512..1024, ReadFailure::Resource);
    src.fail_range(1024..1536, ReadFailure::DeviceLost);
    src.fail_range(1536..2048, ReadFailure::Cancelled);
    src.fail_range(2048..2560, ReadFailure::Unknown);
    #[allow(clippy::reversed_empty_ranges)]
    src.fail_range(9000..8000, ReadFailure::Media); // ignored
    let mut buf = vec![0u8; 512];
    for (offset, class) in [
        (0u64, ReadFailure::Caller),
        (512, ReadFailure::Resource),
        (1024, ReadFailure::DeviceLost),
        (1536, ReadFailure::Cancelled),
        (2048, ReadFailure::Unknown),
    ] {
        assert_eq!(
            classify_read_error(&src.read_at(offset, &mut buf).unwrap_err()),
            class
        );
    }
    src.read_at(8192, &mut buf).unwrap();
    // The earlier range wins where two overlap.
    src.fail_range(0..4096, ReadFailure::Media);
    assert_eq!(
        classify_read_error(&src.read_at(0, &mut buf).unwrap_err()),
        ReadFailure::Caller
    );
    assert_eq!(
        classify_read_error(&src.read_at(3072, &mut buf).unwrap_err()),
        ReadFailure::Media
    );
    // A request that breaks the read contract is a caller error whatever is injected.
    assert_eq!(
        classify_read_error(&src.read_at(3073, &mut buf).unwrap_err()),
        ReadFailure::Caller
    );
}

#[test]
fn faulty_source_loses_the_device_after_n_reads() {
    let src = FaultySource::new(disk(64));
    let mut buf = vec![0u8; 512];
    src.read_at(0, &mut buf).unwrap();
    src.lose_after_reads(2);
    src.read_at(512, &mut buf).unwrap();
    assert!(src.is_alive());
    src.read_at(1024, &mut buf).unwrap();
    // Gone as soon as the second read has been served.
    assert!(!src.is_alive());
    let err = src.read_at(1536, &mut buf).unwrap_err();
    assert_eq!(classify_read_error(&err), ReadFailure::DeviceLost);
    assert!(!src.is_alive());
    assert_eq!(
        classify_read_error(&src.read_at(0, &mut buf).unwrap_err()),
        ReadFailure::DeviceLost
    );
    src.revive();
    assert!(src.is_alive());
    src.read_at(0, &mut buf).unwrap();

    // The silent kind: reads fail like bad sectors, only the liveness check knows.
    src.lose_silently_after_reads(0);
    assert_eq!(
        classify_read_error(&src.read_at(0, &mut buf).unwrap_err()),
        ReadFailure::Media
    );
    assert!(!src.is_alive());
    assert_eq!(src.liveness_checks(), 5);

    // Unplugged between two liveness checks.
    src.revive();
    src.lose_after_liveness_checks(2);
    assert!(src.is_alive());
    src.read_at(0, &mut buf).unwrap();
    assert!(src.is_alive());
    assert!(!src.is_alive());
    assert_eq!(
        classify_read_error(&src.read_at(0, &mut buf).unwrap_err()),
        ReadFailure::DeviceLost
    );
    src.revive();
    src.lose_after_liveness_checks(0);
    assert!(!src.is_alive());
}

#[test]
fn faulty_source_limits_transfers_delays_and_cancels() {
    let src = FaultySource::new(disk(64));
    src.set_max_transfer(Some(1024));
    let mut big = vec![0u8; 2048];
    assert_eq!(
        classify_read_error(&src.read_at(0, &mut big).unwrap_err()),
        ReadFailure::Resource
    );
    src.read_at(0, &mut big[..1024]).unwrap();
    src.set_max_transfer(None);
    src.read_at(0, &mut big).unwrap();

    src.set_delay(Duration::from_millis(30));
    let started = Instant::now();
    src.read_at(0, &mut big).unwrap();
    assert!(started.elapsed() >= Duration::from_millis(30));
    src.set_delay(Duration::ZERO);

    let flag = Arc::new(AtomicBool::new(false));
    src.cancel_after_reads(2, flag.clone());
    src.read_at(0, &mut big).unwrap();
    assert!(!flag.load(Ordering::SeqCst));
    src.read_at(0, &mut big).unwrap();
    assert!(
        flag.load(Ordering::SeqCst),
        "raised when the second read begins"
    );
}

#[test]
fn faulty_source_records_what_was_asked() {
    let inner = MemSource::new(vec![0; 64 * 512])
        .at_location(SourceLocation::Disk { number: 5 })
        .named("rec");
    let src = FaultySource::new(Arc::new(inner));
    src.bad_sectors(4096..4608);
    let mut buf = vec![0u8; 1024];
    src.read_at(0, &mut buf).unwrap();
    src.read_at(4096, &mut buf).unwrap_err();
    src.read_at(8192, &mut buf).unwrap();
    assert_eq!(src.read_count(), 3);
    assert_eq!(src.read_log(), vec![(0, 1024), (4096, 1024), (8192, 1024)]);
    assert_eq!(
        src.touched().iter().collect::<Vec<_>>(),
        vec![0..1024, 4096..5120, 8192..9216]
    );
    assert_eq!(src.info().path, "mem:rec");
    assert_eq!(src.location(), SourceLocation::Disk { number: 5 });
    assert_eq!(
        (src.size(), src.sector_size(), src.physical_sector_size()),
        (64 * 512, 512, 512)
    );
    src.reset_counters();
    assert_eq!(src.read_count(), 0);
    assert!(src.touched().is_empty());
    assert!(src.read_log().is_empty());
    // Faults survive a counter reset.
    assert!(src.read_at(4096, &mut buf).is_err());
}

#[test]
fn faulty_source_is_usable_from_many_threads() {
    let src = Arc::new(FaultySource::new(disk(256)));
    let threads: Vec<_> = (0..8u64)
        .map(|t| {
            let src = src.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 512];
                for i in 0..50u64 {
                    let sector = (t * 31 + i) % 256;
                    src.read_at(sector * 512, &mut buf).unwrap();
                    assert!(buf.iter().all(|&b| b == sector as u8));
                }
            })
        })
        .collect();
    threads.into_iter().for_each(|t| t.join().unwrap());
    assert_eq!(src.read_count(), 400);
}
