//! Scans a disk or volume image the way the engine does and prints what the
//! store holds, how fast it went and how much memory it takes; optionally
//! writes the session to a file and loads it back.
//!
//! ```text
//! cargo run --release -p hds-core --example scanimg -- IMAGE [--start N] [--end N]
//!     [--no-carve] [--session FILE] [--dump N]
//! ```

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use hds_core::scan::{scan, ScanObserver, ScanOptions, ScanProgress};
use hds_core::session::{Session, SessionGuard, SessionMeta, SourceIdentity};
use hds_core::store::DataExtent;
use hds_io::{BlockSource, ImageFile, Protected, RangedSource};

struct Printer {
    events: u64,
}

impl ScanObserver for Printer {
    fn progress(&mut self, p: &ScanProgress) {
        self.events += 1;
        eprintln!(
            "{:?} {:>6.1}% records {} files {} dirs {} blocks {} hits {} bad {} {} MB/s",
            p.phase,
            p.bytes_done as f64 * 100.0 / p.bytes_total.max(1) as f64,
            p.records,
            p.files,
            p.dirs,
            p.index_blocks,
            p.carve_hits,
            p.bad_bytes,
            p.speed_bps / 1_000_000
        );
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(image) = args.next() else {
        eprintln!(
            "usage: scanimg IMAGE [--start N] [--end N] [--no-carve] [--session FILE] [--dump N]"
        );
        std::process::exit(2);
    };
    let mut start = None;
    let mut end = None;
    let mut options = ScanOptions::default();
    let mut session_path: Option<PathBuf> = None;
    let mut dump = 0usize;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--start" => start = args.next().and_then(|v| v.parse().ok()),
            "--end" => end = args.next().and_then(|v| v.parse().ok()),
            "--no-carve" => options.collect_carve_hits = false,
            "--session" => session_path = args.next().map(PathBuf::from),
            "--dump" => dump = args.next().and_then(|v| v.parse().ok()).unwrap_or(20),
            other => {
                eprintln!("unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    let source: Arc<dyn BlockSource> =
        Arc::new(ImageFile::open(image.as_ref(), None).expect("open image"));
    let range = start.unwrap_or(0)..end.unwrap_or(source.size());
    let ranged = RangedSource::new(source.clone(), range.clone()).expect("range");
    let meta = SessionMeta::new(SourceIdentity::from(&source.info()), range.clone(), options)
        .expect("meta");
    let protected = [Protected::session(range.clone())];
    let guard = SessionGuard {
        source: source.as_ref(),
        protected: &protected,
        allow_same_disk: false,
        allow_unknown_disk: true,
    };
    let mut session = match &session_path {
        Some(path) => Session::create(meta, path, &guard).expect("create session file"),
        None => Session::new(meta),
    };
    let cancel = AtomicBool::new(false);
    let mut printer = Printer { events: 0 };
    let t0 = Instant::now();
    let summary = scan(&ranged, &options, &mut session, &mut printer, &cancel);
    let seconds = t0.elapsed().as_secs_f64();
    let store = session.store();
    let c = summary.counters;
    println!(
        "status {:?} in {seconds:.3} s, {} progress events",
        summary.status, printer.events
    );
    println!(
        "read {} bytes: {:.0} MB/s; {} records ({:.0} records/s)",
        summary.bytes_read,
        summary.bytes_read as f64 / seconds / 1e6,
        c.records,
        c.records as f64 / seconds
    );
    println!("counters {c:#?}");
    let memory = store.memory_bytes();
    println!(
        "store memory {} bytes ({:.1} per record); records alone {:.1} bytes per record",
        memory,
        memory as f64 / c.records.max(1) as f64,
        store.record_bytes() as f64 / c.records.max(1) as f64
    );
    println!("warnings {:?}", summary.warnings);
    if let Some(err) = &summary.checkpoint_error {
        println!("checkpoint error: {err}");
    }
    for &id in store.records_by_offset().iter().take(dump) {
        let r = store.record(id).unwrap();
        let names: Vec<String> = store
            .names(id)
            .map(|n| format!("{}<-{}", n.name.to_string_lossy(), n.parent))
            .collect();
        let data: Vec<String> = store
            .data_extents(id)
            .map(|d| match d {
                DataExtent::Resident(r) => format!("res {}+{}", r.value_offset, r.value_len),
                DataExtent::NonResident(e) => format!(
                    "vcn {} runs {:?}",
                    e.start_vcn,
                    e.runs().map(|r| (r.lcn, r.length)).collect::<Vec<_>>()
                ),
            })
            .collect();
        println!(
            "{:#012x} #{:?} seq {} flags {:#x} fixup {:?} names {names:?} data {data:?}",
            r.disk_offset(),
            r.record_number(),
            r.sequence(),
            r.header_flags(),
            r.fixup()
        );
    }
    if let Some(path) = session_path {
        drop(session);
        let t0 = Instant::now();
        let (loaded, report) = Session::load(&path).expect("load");
        println!(
            "session file {} bytes, loaded in {:.3} s ({} checkpoints, {} records)",
            report.file_len,
            t0.elapsed().as_secs_f64(),
            report.checkpoints,
            loaded.store().record_count()
        );
        println!("load report {report:?}");
    }
}
