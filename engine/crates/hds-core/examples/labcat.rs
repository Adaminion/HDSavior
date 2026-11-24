//! Scans a range of an image in memory, runs current volumes (ALGORITHMS
//! C), instances (D) and geometry (F), and writes the result as JSON in the
//! shape of `scripts/linux/instances/hdsi/compare.py` (`catalog_from_model`)
//! for the fields these steps own: per record its class, instance and D2
//! mark; per instance kind, records, label, geometry and container; the
//! main instance and the warnings. `parent`, `flags` and `lost` belong to
//! the catalog step and are left out.
//!
//! ```text
//! cargo run --release -p hds-core --example labcat -- IMAGE START END OUT.json
//!     [--scan-time FILETIME] [--resume-check]
//! ```
//!
//! `--resume-check` also saves the session to a temporary file next to
//! OUT, loads it back without a source and checks that the analysis from
//! the stored `LIVE`, `HEADS` and `OVERRIDES` blocks gives the same JSON.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use hds_core::geometry::Heads;
use hds_core::instances::{analyze, AnalysisInput, Layout, Overrides};
use hds_core::live::{take_snapshot, LiveState, RecordClass};
use hds_core::scan::{scan, NoObserver, ScanOptions, ScanStatus};
use hds_core::session::{Session, SessionGuard, SessionMeta, SourceIdentity};
use hds_core::store::ScanStore;
use hds_io::{BlockSource, ImageFile, Protected, RangedSource};
use serde_json::{json, Map, Value};

fn now_filetime() -> u64 {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (unix + 11_644_473_600) * 10_000_000
}

fn export(store: &ScanStore, layout: &Layout) -> Value {
    let mut records = Map::new();
    for &id in store.records_by_offset() {
        let rec = store.record(id).expect("record");
        let class = layout.class(id);
        let inst = if class == RecordClass::Old {
            layout
                .instance_of(id)
                .map(|i| json!(i.0))
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        records.insert(
            rec.disk_offset().to_string(),
            json!({"class": class.name(), "inst": inst,
                   "older_gen_mark": layout.is_older_generation(id)}),
        );
    }
    let mut instances = Map::new();
    for inst in layout.instances() {
        if inst.records.is_empty() {
            continue;
        }
        let geometry = inst.geometry.as_ref().map(|g| {
            json!({"cluster_size": g.cluster_size, "volume_offset": g.volume_offset,
                   "source": g.offset_source.name(), "validated": g.validated,
                   "checked": g.signature_checked, "matched": g.signature_ok,
                   "dirs": g.index_dirs, "mft_agree": g.mft_agree,
                   "lvr": [g.lost_volume.start, g.lost_volume.end],
                   "total_clusters": g.total_clusters, "end_by_files": g.end_by_files,
                   "protected": inst.protected.is_some()})
        });
        let embedded_in = inst.embedded_in.as_ref().map(|c| {
            json!({"inst": c.instance.0,
                   "container_off": store.record(c.record).map(|r| r.disk_offset()),
                   "name": c.name})
        });
        instances.insert(
            inst.id.0.to_string(),
            json!({"kind": inst.kind.name(), "records": inst.records.len(),
                   "label": inst.label, "geometry": geometry, "embedded_in": embedded_in,
                   "anchor": !inst.anchor_of.is_empty(), "manual": inst.manual}),
        );
    }
    let protected: Vec<Value> = layout
        .protected()
        .iter()
        .map(|p| json!([p.range.start, p.range.end, format!("{:?}", p.reason)]))
        .collect();
    let lv: Vec<Value> = layout
        .live_volumes()
        .iter()
        .map(|v| json!({"offset": v.offset, "size": v.size, "format_time": v.format_time}))
        .collect();
    json!({
        "records": records,
        "instances": instances,
        "main": layout.main().map(|m| m.0),
        "warnings": layout.warnings(),
        "protected": protected,
        "live_volumes": lv,
        "live_records": layout.live().live_records(),
        "post_format_records": layout.live().post_format_records(),
        "contradictions": layout.contradiction_counts().0,
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!("usage: labcat IMAGE START END OUT.json [--scan-time FILETIME] [--resume-check]");
        std::process::exit(2);
    }
    let image = PathBuf::from(&args[0]);
    let start: u64 = args[1].parse().expect("START");
    let end: u64 = args[2].parse().expect("END");
    let out = PathBuf::from(&args[3]);
    let mut scan_time = now_filetime();
    let mut resume_check = false;
    let mut i = 4;
    while i < args.len() {
        match args[i].as_str() {
            "--scan-time" => {
                i += 1;
                scan_time = args[i].parse().expect("FILETIME");
            }
            "--resume-check" => resume_check = true,
            other => panic!("unknown argument {other}"),
        }
        i += 1;
    }

    let t0 = Instant::now();
    let source: Arc<dyn BlockSource> = Arc::new(ImageFile::open(&image, None).expect("open image"));
    let range = start..end;
    let ranged = RangedSource::new(source.clone(), range.clone()).expect("range");
    let options = ScanOptions::default();
    let meta = SessionMeta::new(SourceIdentity::from(&source.info()), range.clone(), options)
        .expect("meta");
    let mut session = Session::new(meta.clone());
    let cancel = AtomicBool::new(false);
    let summary = scan(&ranged, &options, &mut session, &mut NoObserver, &cancel);
    assert_eq!(
        summary.status,
        ScanStatus::Completed,
        "scan did not complete"
    );
    let t_scan = t0.elapsed();

    let t1 = Instant::now();
    let snapshot = take_snapshot(&ranged, scan_time, &cancel).expect("snapshot");
    let live = LiveState {
        scan_time: Some(scan_time),
        snapshot: Some(snapshot),
    };
    let mut heads = Heads::new();
    heads.attach(Some(ranged.clone()));
    let overrides = Overrides::new();
    let input = AnalysisInput::new(session.store(), session.meta(), &live);
    let layout = analyze(&input, &mut heads, &overrides, &cancel).expect("analysis");
    let t_an = t1.elapsed();
    let value = export(session.store(), &layout);
    std::fs::write(&out, serde_json::to_vec(&value).expect("json")).expect("write output");
    eprintln!(
        "{}: {} records, {} instances, main {:?}; scan {:.2}s, analysis {:.2}s, {} head reads",
        image.display(),
        session.store().record_count(),
        layout.instances().len(),
        layout.main(),
        t_scan.as_secs_f64(),
        t_an.as_secs_f64(),
        heads.reads()
    );
    for l in &layout.log {
        eprintln!("  {l}");
    }

    if resume_check {
        live.save(&mut session).expect("LIVE");
        heads.save_new(&mut session).expect("HEADS");
        overrides.save(&mut session).expect("OVERRIDES");
        let dir = out.parent().unwrap_or(Path::new("."));
        let path = dir.join(format!(
            "{}.labcat-session",
            out.file_name().and_then(|n| n.to_str()).unwrap_or("out")
        ));
        let _ = std::fs::remove_file(&path);
        let protected = [Protected::session(range.clone())];
        let guard = SessionGuard {
            source: source.as_ref(),
            protected: &protected,
            allow_same_disk: true,
            allow_unknown_disk: true,
        };
        let mut s2 = session;
        s2.save_as(&path, &guard).expect("save session");
        let (loaded, _) = Session::load(&path).expect("load session");
        let live2 = LiveState::load(&loaded).expect("LIVE");
        let mut heads2 = Heads::load(&loaded).expect("HEADS");
        let overrides2 = Overrides::load(&loaded).expect("OVERRIDES");
        let input2 = AnalysisInput::new(loaded.store(), loaded.meta(), &live2);
        let layout2 = analyze(&input2, &mut heads2, &overrides2, &cancel).expect("analysis");
        let v2 = export(loaded.store(), &layout2);
        let _ = std::fs::remove_file(&path);
        if v2 != value {
            eprintln!("RESUME MISMATCH");
            let a: BTreeMap<String, Value> =
                serde_json::from_value(value["instances"].clone()).unwrap();
            let b: BTreeMap<String, Value> =
                serde_json::from_value(v2["instances"].clone()).unwrap();
            eprintln!("before {a:?}\nafter {b:?}");
            std::process::exit(1);
        }
        eprintln!("resume check: identical (no reads: {})", heads2.reads());
    }
}
