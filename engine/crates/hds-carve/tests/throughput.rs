//! How fast is `sniff` at what the scanner does: one call per 512-byte
//! sector of a large buffer?
//!
//! The number that matters is the release-mode one:
//!
//! ```text
//! cargo test --release -p hds-carve --test throughput -- --nocapture
//! ```

mod common;

use std::hint::black_box;
use std::time::Instant;

use common::{samples, Rng};
use hds_carve::sniff;

const SECTOR: usize = 512;

/// Scans `data` like the scanner: sniff at every sector. Returns
/// (hits, nanoseconds per call).
fn scan(data: &[u8], passes: usize) -> (usize, f64) {
    let calls = data.len() / SECTOR * passes;
    let started = Instant::now();
    let mut hits = 0usize;
    for _ in 0..passes {
        for sector in data.chunks_exact(SECTOR) {
            hits += usize::from(sniff(black_box(sector)).is_some());
        }
    }
    (
        hits / passes,
        started.elapsed().as_nanos() as f64 / calls as f64,
    )
}

#[test]
fn sniff_throughput() {
    const SIZE: usize = 128 << 20;
    let mut rng = Rng::new(0x5CA9);

    // what a used disk looks like: mostly compressed media (noise to us),
    // some empty space, some text, and every megabyte or so a file start
    let mut mixed = rng.bytes(SIZE);
    for chunk in mixed.chunks_mut(1 << 20).skip(1).step_by(4) {
        chunk.fill(0);
    }
    for chunk in mixed.chunks_mut(1 << 20).skip(3).step_by(8) {
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = b"The quick brown fox jumps over the lazy dog.\r\n"[i % 46];
        }
    }
    let all = samples();
    let mut starts = 0;
    for (i, at) in (0..SIZE - (1 << 20)).step_by(1 << 20).enumerate() {
        let s = &all[i % all.len()].data;
        mixed[at..at + s.len()].copy_from_slice(s);
        starts += 1;
    }

    // The large buffers do not fit in any cache: there the time per sector is
    // the cache miss on the sector's first bytes, which the scanner pays
    // anyway. The small ones show what sniff() itself costs.
    let random = rng.bytes(32 << 20);
    let cases: [(&str, &[u8], usize); 6] = [
        ("mixed disk content", &mixed, 3),
        ("random", &random, 3),
        ("0xFF", &vec![0xFFu8; 32 << 20], 3),
        ("random, in cache", &random[..128 << 10], 4000),
        ("0xFF, in cache", &[0xFFu8; 128 << 10], 4000),
        ("zeros, in cache", &[0u8; 128 << 10], 4000),
    ];
    println!(
        "sniff() at every {SECTOR}-byte sector ({} build):",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    let mut worst = 0f64;
    for (what, data, passes) in cases {
        // warm up, then measure
        scan(data, 1);
        let (hits, ns) = scan(data, passes);
        let sectors_per_s = 1e9 / ns;
        println!(
            "  {what:<20} {ns:>6.2} ns/sector  = {:>7.1} GiB of disk per second, {hits} hits in {} KiB",
            sectors_per_s * SECTOR as f64 / (1u64 << 30) as f64,
            data.len() >> 10
        );
        worst = worst.max(ns);
        if what == "mixed disk content" {
            // every planted file is found (some samples hold a second
            // candidate at a sector boundary inside them)
            assert!(
                hits >= starts && hits <= starts + 8,
                "{hits} hits for {starts} files"
            );
        } else {
            assert_eq!(hits, 0, "{what}");
        }
    }
    // A hard disk delivers some 400 000 sectors per second. Even an
    // unoptimised build must be orders of magnitude faster than that; the
    // bound is deliberately loose so that a loaded CI machine does not trip it.
    assert!(worst < 250.0, "sniff takes {worst:.1} ns per sector");
}
