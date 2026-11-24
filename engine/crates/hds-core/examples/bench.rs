//! Measures the scanner and the session file on a large synthetic image.
//!
//! ```text
//! cargo run --release -p hds-core --example bench -- DIR FIXTURE_IMAGE [RECORDS]
//! ```
//!
//! Builds `DIR/bench.img` once (4 GiB, sparse, written through
//! `hds_io::out`): an NTFS boot sector naming an `$MFT` at 1 GiB that holds
//! `RECORDS` records (default 1,000,000) copied from the in-use records of
//! `FIXTURE_IMAGE` and renumbered; 20,000 `INDX` blocks behind it; 1 GiB of
//! incompressible data from 2.5 GiB on with a JPEG or PNG header every
//! 256 KiB; zeros elsewhere. Then scans the regions one by one and the
//! whole image, with and without file-signature hits, and saves and loads
//! the session. Run it twice: the first run fills the page cache.

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use hds_core::scan::{scan, NoObserver, ScanOptions, ScanStatus};
use hds_core::session::{Session, SessionGuard, SessionMeta, SourceIdentity};
use hds_io::out::OutDir;
use hds_io::testing::XorShift;
use hds_io::{BlockSource, ImageFile, Protected, RangedSource};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
const IMAGE: u64 = 4 * GIB;
const MFT: u64 = GIB;
const RANDOM: std::ops::Range<u64> = (5 * GIB / 2)..(7 * GIB / 2);

fn peak_rss() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM"))
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Raw bytes of the in-use records and of the INDX blocks of a fixture.
fn samples(fixture: &Path) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let image = std::fs::read(fixture).expect("read fixture");
    let mut records = Vec::new();
    let mut blocks = Vec::new();
    for at in (0..image.len() - 4096).step_by(512) {
        let sector = &image[at..at + 512];
        if let Some(p) = hds_ntfs::probe_file_record(sector, 4096) {
            let raw = image[at..at + p.record_size as usize].to_vec();
            let mut copy = raw.clone();
            if hds_ntfs::parse_file_record(&mut copy)
                .is_ok_and(|r| r.is_in_use() && r.allocated_size() == 1024)
            {
                records.push(raw);
            }
        } else if let Some(p) = hds_ntfs::probe_index_block(sector) {
            if p.block_size == 4096 {
                blocks.push(image[at..at + 4096].to_vec());
            }
        }
    }
    (records, blocks)
}

fn boot_sector() -> Vec<u8> {
    let mut s = vec![0u8; 512];
    s[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    s[3..11].copy_from_slice(b"NTFS    ");
    s[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
    s[0x0D] = 8;
    s[0x15] = 0xF8;
    s[0x28..0x30].copy_from_slice(&(IMAGE / 512 - 1).to_le_bytes());
    s[0x30..0x38].copy_from_slice(&(MFT / 4096).to_le_bytes());
    s[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
    s[0x40] = 0xF6;
    s[0x44] = 1;
    s[0x1FE] = 0x55;
    s[0x1FF] = 0xAA;
    s
}

fn build(dir: &Path, fixture: &Path, count: u64) {
    let out = OutDir::open_root(dir).expect("open directory");
    if out.entry("bench.img").expect("look").is_some() {
        return;
    }
    let (records, blocks) = samples(fixture);
    assert!(!records.is_empty() && !blocks.is_empty());
    let t0 = Instant::now();
    let mut file = out.create_new("bench.img").expect("create image");
    file.set_len(IMAGE).expect("size");
    file.write_all(&boot_sector()).unwrap();
    let mut w = std::io::BufWriter::with_capacity(8 << 20, &mut file);
    w.seek(SeekFrom::Start(MFT)).unwrap();
    for n in 0..count {
        let mut r = records[(n % records.len() as u64) as usize].clone();
        r[0x2C..0x30].copy_from_slice(&(n as u32).to_le_bytes());
        w.write_all(&r).unwrap();
    }
    for i in 0..20_000usize {
        w.write_all(&blocks[i % blocks.len()]).unwrap();
    }
    w.seek(SeekFrom::Start(RANDOM.start)).unwrap();
    let mut rng = XorShift::new(42);
    let mut piece = vec![0u8; 256 * 1024];
    let jpeg = [
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00,
        0x48, 0x00, 0x48, 0x00, 0x00,
    ];
    let png = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H', b'D', b'R', 0,
        0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0, 0x3A, 0x7E, 0x9B, 0x55,
    ];
    let mut k = 0usize;
    let mut at = RANDOM.start;
    while at < RANDOM.end {
        rng.fill(&mut piece);
        if k.is_multiple_of(2) {
            piece[..jpeg.len()].copy_from_slice(&jpeg);
        } else {
            piece[..png.len()].copy_from_slice(&png);
        }
        k += 1;
        w.write_all(&piece).unwrap();
        at += piece.len() as u64;
    }
    w.flush().unwrap();
    drop(w);
    file.sync().unwrap();
    println!(
        "built {} in {:.1} s",
        out.path().join("bench.img").display(),
        t0.elapsed().as_secs_f64()
    );
}

fn run(source: &Arc<dyn BlockSource>, range: std::ops::Range<u64>, carve: bool) -> Session {
    let options = ScanOptions {
        collect_carve_hits: carve,
        ..ScanOptions::default()
    };
    let meta =
        SessionMeta::new(SourceIdentity::from(&source.info()), range.clone(), options).unwrap();
    let mut session = Session::new(meta);
    let ranged = RangedSource::new(source.clone(), range.clone()).unwrap();
    let t0 = Instant::now();
    let summary = scan(
        &ranged,
        &options,
        &mut session,
        &mut NoObserver,
        &AtomicBool::new(false),
    );
    let s = t0.elapsed().as_secs_f64();
    assert_eq!(summary.status, ScanStatus::Completed);
    let c = summary.counters;
    let len = range.end - range.start;
    println!(
        "  {:>5} MiB carve={:<5} {:>7.3} s {:>7.0} MB/s  records {:>8} ({:>9.0}/s) blocks {:>6} hits {:>6}",
        len / MIB,
        carve,
        s,
        len as f64 / s / 1e6,
        c.records,
        c.records as f64 / s,
        c.index_blocks,
        c.carve_hits
    );
    session
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(dir), Some(fixture)) = (args.next(), args.next()) else {
        eprintln!("usage: bench DIR FIXTURE_IMAGE [RECORDS]");
        std::process::exit(2);
    };
    let count: u64 = args
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let dir = Path::new(&dir);
    build(dir, Path::new(&fixture), count);
    let source: Arc<dyn BlockSource> =
        Arc::new(ImageFile::open(&dir.join("bench.img"), None).unwrap());
    // Warm the page cache.
    run(&source, 0..IMAGE, false);
    println!("regions:");
    let mft_end = MFT + count * 1024 + 20_000 * 4096;
    let mft_end = mft_end.div_ceil(4096) * 4096;
    for (name, range) in [
        ("MFT and INDX", MFT..mft_end),
        ("random data with signatures", RANDOM),
        ("zeros", 3 * GIB + GIB / 2..IMAGE),
    ] {
        println!(" {name}:");
        run(&source, range.clone(), false);
        run(&source, range, true);
    }
    println!("whole image:");
    run(&source, 0..IMAGE, false);
    let session = run(&source, 0..IMAGE, true);
    let store = session.store();
    println!(
        "store: {} records, {} bytes total ({:.1} per record), records alone {:.1} bytes per record; {}",
        store.record_count(),
        store.memory_bytes(),
        store.memory_bytes() as f64 / store.record_count() as f64,
        store.record_bytes() as f64 / store.record_count() as f64,
        peak_rss()
    );
    // Session file: a compacted copy, then a load.
    let mut session = session;
    let protected = [Protected::session(0..IMAGE)];
    let guard = SessionGuard {
        source: source.as_ref(),
        protected: &protected,
        allow_same_disk: false,
        allow_unknown_disk: true,
    };
    let path = dir.join("bench.hds");
    if dir.join("bench.hds").exists() {
        let out = OutDir::open_root(dir).unwrap();
        out.remove_file("bench.hds").unwrap();
    }
    let t0 = Instant::now();
    let report = session.save_as(&path, &guard).unwrap();
    println!(
        "session save: {} bytes in {:.3} s",
        report.bytes,
        t0.elapsed().as_secs_f64()
    );
    drop(session);
    let t0 = Instant::now();
    let (loaded, _) = Session::load(&path).unwrap();
    let load = t0.elapsed().as_secs_f64();
    println!(
        "session load: {} records in {:.3} s ({:.0} records/s)",
        loaded.store().record_count(),
        load,
        loaded.store().record_count() as f64 / load
    );
    let t0 = Instant::now();
    let index = loaded.store().fragment_index(|_, _| true);
    println!(
        "fragment index: {} fragments over {} records in {:.3} s",
        index.fragment_count(),
        index.numbered().len(),
        t0.elapsed().as_secs_f64()
    );
}
