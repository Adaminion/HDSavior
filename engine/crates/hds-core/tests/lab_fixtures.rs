//! Current volumes, instances and geometry (ALGORITHMS C, D, F) on the
//! real-driver fixtures of `scripts/linux/make-fixtures.sh`.
//!
//! `HDS_FIXTURES` names the directory of images and manifests; unset, the
//! test says SKIPPED and passes; set but an image missing, it fails. Each
//! fixture is scanned over its manifest's scan range and analysed; the
//! current volumes must be the manifest's new volumes (with their format
//! times), the main instance must carry the old volume's geometry, the
//! protected list must start with the session range, nothing may be read
//! outside the range, and the analysis from the stored `LIVE`, `HEADS`
//! and `OVERRIDES` blocks (no source) must equal the first one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hds_core::geometry::Heads;
use hds_core::instances::{analyze, AnalysisInput, InstanceKind, Overrides};
use hds_core::live::{take_snapshot, LiveState};
use hds_core::scan::{scan, NoObserver, ScanOptions, ScanStatus};
use hds_core::session::{Session, SessionMeta, SourceIdentity};
use hds_io::testing::FaultySource;
use hds_io::{BlockSource, ImageFile, Protected, RangedSource};
use serde_json::Value;

const FIXTURES: [&str; 16] = [
    "basic",
    "fragmented",
    "wiped",
    "cluster-change",
    "offset-mbr",
    "offset-mbr63",
    "offset-gpt",
    "post-write",
    "small-clusters",
    "big-clusters",
    "4k-records",
    "embedded",
    "generations",
    "generations-restored",
    "live-collision",
    "repartition",
];

fn now_filetime() -> u64 {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (unix + 11_644_473_600) * 10_000_000
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or_else(|| panic!("not a number: {v}"))
}

#[test]
fn fixtures_current_volumes_instances_and_geometry() {
    let Some(dir) = std::env::var_os("HDS_FIXTURES").map(PathBuf::from) else {
        eprintln!("SKIPPED: HDS_FIXTURES is not set");
        return;
    };
    let cancel = AtomicBool::new(false);
    for name in FIXTURES {
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{name}.manifest.json"))).expect("manifest"),
        )
        .unwrap();
        let image = ImageFile::open(&dir.join(format!("{name}.img")), None).expect("image");
        let faulty = Arc::new(FaultySource::new(Arc::new(image)));
        let source: Arc<dyn BlockSource> = faulty.clone();
        let start = u(&manifest["scan_range"]["offset"]);
        let range = start..start + u(&manifest["scan_range"]["length"]);
        let ranged = RangedSource::new(source.clone(), range.clone()).unwrap();
        let options = ScanOptions::default();
        let meta =
            SessionMeta::new(SourceIdentity::from(&source.info()), range.clone(), options).unwrap();
        let mut session = Session::new(meta);
        let summary = scan(&ranged, &options, &mut session, &mut NoObserver, &cancel);
        assert_eq!(summary.status, ScanStatus::Completed, "{name}");
        let snapshot = take_snapshot(&ranged, now_filetime(), &cancel).unwrap();
        let live = LiveState {
            scan_time: Some(snapshot.taken_at),
            snapshot: Some(snapshot.clone()),
        };
        let mut heads = Heads::new();
        heads.attach(Some(ranged.clone()));
        let input = AnalysisInput::new(session.store(), session.meta(), &live);
        let layout = analyze(&input, &mut heads, &Overrides::new(), &cancel).unwrap();
        for r in faulty.touched().iter() {
            assert!(
                r.start >= range.start && r.end <= range.end,
                "{name}: read outside the range: {r:?}"
            );
        }

        // C1/C2: the new volumes inside the range, with their format times.
        let expect: Vec<(u64, u64)> = manifest["new_volumes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| (u(&v["offset"]), u(&v["format_time"])))
            .filter(|(o, _)| range.contains(o))
            .collect();
        let got: Vec<(u64, u64)> = layout
            .live_volumes()
            .iter()
            .map(|v| (v.offset, v.format_time))
            .collect();
        assert_eq!(got, expect, "{name}: current volumes");
        assert!(!snapshot.incomplete, "{name}: snapshot incomplete");
        assert!(layout.live().live_records() > 0, "{name}");

        // D8/F: main carries the old volume's geometry, validated.
        let main = layout.instance(layout.main().expect("main")).unwrap();
        assert_eq!(main.kind, InstanceKind::Main);
        let g = main.geometry.as_ref().expect("main geometry");
        let old = &manifest["old_volume"];
        assert_eq!(
            (u64::from(g.cluster_size), g.volume_offset),
            (u(&old["cluster_size"]), u(&old["offset"])),
            "{name}: main geometry {g:?}"
        );
        // live-collision: the new volume's own files cover most old
        // clusters, so fewer than 20 files stay checkable and only 2
        // directories vote: F6 does not validate it (the geometry is
        // still the right one).
        let validated = name != "live-collision";
        assert_eq!(g.validated, validated, "{name}: {g:?}");

        // S2: the session range first, then main's lost-volume range
        // (protected also when not validated: main is the anchor).
        let p = layout.protected();
        assert_eq!(p[0], Protected::session(range.clone()), "{name}");
        assert!(
            p.contains(&Protected::lost_volume(g.lost_volume.clone())),
            "{name}"
        );
        assert_eq!(g.lost_volume.start, u(&old["offset"]));

        // Fixture-specific expectations.
        let kinds: Vec<InstanceKind> = layout.instances().iter().map(|i| i.kind).collect();
        match name {
            "embedded" => assert!(kinds.contains(&InstanceKind::Embedded), "{name}: {kinds:?}"),
            "generations" | "generations-restored" => {
                assert!(kinds.contains(&InstanceKind::Older), "{name}: {kinds:?}")
            }
            "post-write" | "live-collision" => {
                assert!(
                    layout.live().post_format_records() > 0 || layout.live().live_records() > 30,
                    "{name}"
                )
            }
            _ => {}
        }

        // Resume: the same analysis from the stored blocks, without a source.
        live.save(&mut session).unwrap();
        heads.save_new(&mut session).unwrap();
        let live2 = LiveState::load(&session).unwrap();
        let mut heads2 = Heads::load(&session).unwrap();
        let overrides2 = Overrides::load(&session).unwrap();
        let input2 = AnalysisInput::new(session.store(), session.meta(), &live2);
        let again = analyze(&input2, &mut heads2, &overrides2, &cancel).unwrap();
        assert_eq!(
            again.instances(),
            layout.instances(),
            "{name}: resume differs"
        );
        assert_eq!(again.protected(), layout.protected(), "{name}");
        assert_eq!(heads2.reads(), 0);
        eprintln!(
            "{name}: {} records, {} instances {:?}, main ({}, {}) {:?} checked {}/{}, {} cuts, warnings {:?}",
            session.store().record_count(),
            layout.instances().len(),
            kinds,
            g.cluster_size,
            g.volume_offset,
            g.offset_source,
            g.signature_ok,
            g.signature_checked,
            layout.cuts().len(),
            layout.warnings()
        );
    }
}
