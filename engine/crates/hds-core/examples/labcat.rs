//! Scans a range of an image in memory, runs current volumes (ALGORITHMS
//! C), instances (D), geometry (F), the catalog (E) and health (G), and
//! writes the result as JSON in the shape of
//! `scripts/linux/instances/hdsi/compare.py` (`catalog_from_model`): per
//! record its class, instance and D2 mark, and for base records with a
//! catalog node their `parent` and `flags`; per instance kind, records,
//! label, geometry and container; per lost folder `(instance, P, s)` its
//! name and parent (`lost`, a list of `[instance, P, s, name, parent]`
//! because JSON keys are strings); the main instance and the warnings.
//!
//! Parents: `["root"]`, `["dir", OFFSET]` (the directory's base record),
//! `["lost", INSTANCE, P, s]`, `["extend"]` (omitted below `$Extend`),
//! `["lostroot"]` (`[Lost folders]`, and the strays root that holds the
//! strays' lost folders). Files in `[Orphaned files]` and system files
//! carry no parent, as in the model.
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
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use hds_core::catalog::{Catalog, CatalogInput, NodeId, NodeKind};
use hds_core::geometry::Heads;
use hds_core::instances::{analyze, AnalysisInput, Layout, Overrides};
use hds_core::live::{take_snapshot, LiveState, RecordClass};
use hds_core::scan::{scan, NoObserver, ScanOptions, ScanStatus};
use hds_core::session::{Session, SessionGuard, SessionMeta, SourceIdentity};
use hds_core::store::{RecordId, ScanStore};
use hds_io::out::OutDir;
use hds_io::{BlockSource, ImageFile, Protected, RangedSource};
use serde_json::{json, Map, Value};

/// Deletes the regular file `name` in `dir` when it exists.
fn remove_if_present(dir: &OutDir, name: &str) {
    if dir.entry(name).expect("output entry").is_some() {
        dir.remove_file(name).expect("remove old output");
    }
}

/// Writes `bytes` as `name` in `dir`, replacing an earlier output of this
/// tool (a regular file only).
fn replace_file(dir: &OutDir, name: &str, bytes: &[u8]) {
    remove_if_present(dir, name);
    let mut f = dir.create_new(name).expect("create output");
    f.write_all(bytes).expect("write output");
    f.sync().expect("sync output");
}

fn now_filetime() -> u64 {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (unix + 11_644_473_600) * 10_000_000
}

/// The compare.py form of a node as a parent.
fn parent_value(store: &ScanStore, layout: &Layout, cat: &Catalog, p: NodeId) -> Option<Value> {
    let kind = cat.kind(p)?;
    let inst = cat
        .instance_index(p)
        .and_then(|i| cat.instances().get(i as usize))?;
    if cat.parent(p).is_none() {
        // An instance root: the strays root holds the strays' lost folders.
        return Some(if inst.kind == hds_core::instances::InstanceKind::Strays {
            json!(["lostroot"])
        } else {
            json!(["root"])
        });
    }
    match kind {
        NodeKind::Dir => {
            let r = cat.record_of(p)?;
            Some(json!(["dir", store.record(r)?.disk_offset()]))
        }
        NodeKind::LostDir => {
            let k = cat.lost_key(p)?;
            let _ = layout;
            Some(json!(["lost", inst.layout_id.0, k.number, k.sequence]))
        }
        NodeKind::Virtual if Some(p) == inst.lost => Some(json!(["lostroot"])),
        _ => None,
    }
}

fn export(store: &ScanStore, layout: &Layout, cat: &Catalog) -> Value {
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
        let mut v = json!({"class": class.name(), "inst": inst,
                   "older_gen_mark": layout.is_older_generation(id)});
        if let Some(node) = cat.node_of_record(id) {
            let parent = cat
                .parent(node)
                .and_then(|p| parent_value(store, layout, cat, p));
            if let Some(p) = parent {
                v["parent"] = p;
                let flags: Vec<&str> = cat.flags(node).iter().map(|f| f.name()).collect();
                v["flags"] = json!(flags);
            }
        } else if class == RecordClass::Old && !rec.is_extension() && extend_child(store, id) {
            v["parent"] = json!(["extend"]);
        }
        records.insert(rec.disk_offset().to_string(), v);
    }
    let mut lost: Vec<Value> = Vec::new();
    for id in 0..cat.node_count() as u32 {
        let node = NodeId(id);
        let Some(k) = cat.lost_key(node) else {
            continue;
        };
        let Some(ci) = cat.instances().get(k.instance as usize) else {
            continue;
        };
        let name = cat.lost_folder_named(node).then(|| cat.name(store, node));
        let parent = cat
            .parent(node)
            .and_then(|p| parent_value(store, layout, cat, p))
            .unwrap_or(Value::Null);
        lost.push(json!([ci.layout_id.0, k.number, k.sequence, name, parent]));
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
        "lost": lost,
        "nodes": cat.node_count(),
    })
}

/// Whether the best-placed name of a record names `$Extend` (11) as its
/// parent: E5 omits it and everything below.
fn extend_child(store: &ScanStore, id: RecordId) -> bool {
    store.names(id).any(|n| n.parent.number() == 11)
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
    let t2 = Instant::now();
    let catalog =
        Catalog::build(&CatalogInput::new(session.store(), &layout, &heads, &[])).expect("catalog");
    let t_cat = t2.elapsed();
    let value = export(session.store(), &layout, &catalog);
    // All file writes go through hds_io::out (S6), also in this tool.
    let (out_dir, out_name) = hds_io::out::split_file_path(&out).expect("output path");
    let out_dir = OutDir::open_root(&out_dir).expect("output directory");
    replace_file(
        &out_dir,
        &out_name,
        &serde_json::to_vec(&value).expect("json"),
    );
    eprintln!(
        "{}: {} records, {} instances, main {:?}; scan {:.2}s, analysis {:.2}s, catalog {:.2}s, {} head reads",
        image.display(),
        session.store().record_count(),
        layout.instances().len(),
        layout.main(),
        t_scan.as_secs_f64(),
        t_an.as_secs_f64(),
        t_cat.as_secs_f64(),
        heads.reads()
    );
    for l in &layout.log {
        eprintln!("  {l}");
    }

    if resume_check {
        live.save(&mut session).expect("LIVE");
        heads.save_new(&mut session).expect("HEADS");
        overrides.save(&mut session).expect("OVERRIDES");
        let session_name = format!("{out_name}.labcat-session");
        let path = out_dir.path().join(&session_name);
        remove_if_present(&out_dir, &session_name);
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
        let catalog2 = Catalog::build(&CatalogInput::new(loaded.store(), &layout2, &heads2, &[]))
            .expect("catalog");
        let v2 = export(loaded.store(), &layout2, &catalog2);
        drop(loaded);
        remove_if_present(&out_dir, &session_name);
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
