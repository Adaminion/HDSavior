//! The catalog (ALGORITHMS E) and health (G) on the real-driver fixtures of
//! `scripts/linux/make-fixtures.sh`, against their manifests.
//!
//! `HDS_FIXTURES` names the directory of images and manifests; unset, the
//! test says SKIPPED and passes; set but an image missing, it fails. Each
//! fixture is scanned over its manifest's scan range, analysed and
//! catalogued. For every entry of the lost file system whose record
//! survived (`after.record_intact`):
//!
//! * it has a node in the `main` instance, named by one of its
//!   `$FILE_NAME`s (deleted entries: the last path component);
//! * its path is the manifest path when every ancestor is known
//!   (`after.ancestry.path_reconstructible`, deleted entries included),
//!   otherwise it ends with the surviving suffix below a lost or virtual
//!   folder; the entries E6 leaves below a nameless lost folder although
//!   their path is reconstructible are counted and must match `UNNAMED`;
//! * files whose content survived have the manifest size;
//! * the rating: intact, in use, not compressed and nothing current in its
//!   clusters → `good` (the geometry is validated) or `uncertain` (not
//!   validated); compressed → `unsupported`; current allocation in its
//!   clusters → `damaged`; deleted → at most `uncertain`.
//!
//! The recovery plan of the whole main instance holds every file node,
//! and a search for each tested name finds it.

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

use hds_core::catalog::{Catalog, CatalogInput, NodeKind};
use hds_core::geometry::Heads;
use hds_core::instances::{analyze, AnalysisInput, InstanceKind, Overrides};
use hds_core::live::{take_snapshot, LiveState};
use hds_core::query::{Filter, SearchArgs, Selection};
use hds_core::scan::{scan, NoObserver, ScanOptions, ScanStatus};
use hds_core::session::{Session, SessionMeta, SourceIdentity};
use hds_io::{BlockSource, ImageFile, RangedSource};
use hds_recover::Rating;
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

/// Entries with a reconstructible path that E6 leaves below a nameless
/// lost folder, per fixture (every other fixture: none). In `embedded`,
/// the 16 folders `Projects/p01..p16` (32 entries below them) stay
/// `Folder #100..#115`: their owner `Projects` (99) is lost and the
/// embedded guest holds number 99, so E6's owner-number clause fails.
const UNNAMED: [(&str, usize); 1] = [("embedded", 32)];

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
fn fixtures_catalog_names_paths_sizes_and_ratings() {
    let Some(dir) = std::env::var_os("HDS_FIXTURES").map(PathBuf::from) else {
        eprintln!("SKIPPED: HDS_FIXTURES is not set");
        return;
    };
    let cancel = AtomicBool::new(false);
    let mut failures: Vec<String> = Vec::new();
    for name in FIXTURES {
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{name}.manifest.json"))).expect("manifest"),
        )
        .unwrap();
        let sector = manifest["image"]["sector_size"].as_u64().map(|s| s as u32);
        let image = ImageFile::open(&dir.join(format!("{name}.img")), sector).expect("image");
        let source: Arc<dyn BlockSource> = Arc::new(image);
        let start = u(&manifest["scan_range"]["offset"]);
        let range = start..start + u(&manifest["scan_range"]["length"]);
        let ranged = RangedSource::new(source.clone(), range.clone()).unwrap();
        let options = ScanOptions::default();
        let meta =
            SessionMeta::new(SourceIdentity::from(&source.info()), range.clone(), options).unwrap();
        let mut session = Session::new(meta);
        let summary = scan(&ranged, &options, &mut session, &mut NoObserver, &cancel);
        assert_eq!(summary.status, ScanStatus::Completed, "{name}");
        let live = LiveState {
            scan_time: Some(now_filetime()),
            snapshot: Some(take_snapshot(&ranged, now_filetime(), &cancel).unwrap()),
        };
        let mut heads = Heads::new();
        heads.attach(Some(ranged.clone()));
        let input = AnalysisInput::new(session.store(), session.meta(), &live);
        let layout = analyze(&input, &mut heads, &Overrides::new(), &cancel).unwrap();
        let store = session.store();
        let cat = Catalog::build(&CatalogInput::new(store, &layout, &heads, &[])).unwrap();
        let main = &cat.instances()[0];
        assert_eq!(main.kind, InstanceKind::Main, "{name}");
        let validated = main.geometry.validated;
        let (mut checked, mut paths, mut sizes, mut ratings, mut unnamed) = (0, 0, 0, 0, 0);
        let mut fail = |m: String| {
            if failures.len() < 60 {
                failures.push(format!("{name}: {m}"));
            }
        };
        for e in manifest["entries"].as_array().unwrap() {
            let after = &e["after"];
            if after["record_intact"] != Value::Bool(true) {
                continue;
            }
            let path = e["path"].as_str().unwrap();
            let off = u(&e["ondisk"]["record_offset"]);
            let number = u(&e["mft_record"]);
            if number < 24 {
                continue;
            }
            let Some(rec) = store.record_at(off) else {
                fail(format!("{path}: record at {off} not scanned"));
                continue;
            };
            let Some(node) = cat.node_of_record(rec) else {
                fail(format!("{path}: no node"));
                continue;
            };
            checked += 1;
            if cat.instance_kind(node) != Some(InstanceKind::Main) {
                fail(format!(
                    "{path}: in a {:?} instance",
                    cat.instance_kind(node)
                ));
                continue;
            }
            // Name.
            let shown = cat.name(store, node);
            let names: Vec<String> = match e["ondisk"]["names"].as_array() {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|n| n["name"].as_str().unwrap().to_string())
                    .collect(),
                _ => vec![path.rsplit('/').next().unwrap().to_string()],
            };
            if !names.contains(&shown) {
                fail(format!(
                    "{path}: named {shown:?}, expected one of {names:?}"
                ));
            }
            // Path.
            let got = cat.path(store, node);
            let anc = &after["ancestry"];
            let deleted = e["deleted"] == Value::Bool(true);
            let last = path.rsplit('/').next().unwrap_or(path);
            if anc["path_reconstructible"] == Value::Bool(true) && got == path {
                paths += 1;
            } else if anc["path_reconstructible"] == Value::Bool(true) {
                // E6 may leave a folder nameless where the manifest's
                // usability test (C4 only) names it: then the entry must
                // sit below a nameless lost folder, never elsewhere, and
                // only where that is expected (UNNAMED).
                let nameless = got.starts_with("[Lost folders]/Folder #") && got.ends_with(last);
                if nameless {
                    unnamed += 1;
                } else {
                    fail(format!(
                        "path {got:?}, expected {path:?}{}",
                        if deleted { " (deleted)" } else { "" }
                    ));
                }
            } else if let Some(suffix) = anc["surviving_suffix"].as_str() {
                let lost =
                    got.starts_with("[Lost folders]/") || got.starts_with("[Orphaned files]/");
                if got.ends_with(suffix) && (lost || got == suffix) {
                    paths += 1;
                } else {
                    fail(format!(
                        "path {got:?} does not end with {suffix:?} ({path})"
                    ));
                }
            }
            if e["type"] != "file" {
                if cat.kind(node) != Some(NodeKind::Dir) {
                    fail(format!("{path}: kind {:?}", cat.kind(node)));
                }
                continue;
            }
            let row = cat.row(store, node, true).unwrap();
            let intact = after["content_intact"] == Value::Bool(true);
            if intact {
                if Some(row.size) != e["size"].as_u64() {
                    fail(format!("{path}: size {}, expected {}", row.size, e["size"]));
                } else {
                    sizes += 1;
                }
            }
            // Rating.
            let flags: Vec<&str> = e["ondisk"]["data_flags"]
                .as_array()
                .map(|l| l.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let current = after["overlaps_new_allocation"] == Value::Bool(true);
            let resident = e["ondisk"]["resident"] == Value::Bool(true);
            let outside = after["data_inside_scan_range"] == Value::Bool(false);
            // G1: resident data is always raw, whatever its flags say.
            let unsupported =
                !resident && (flags.contains(&"compressed") || flags.contains(&"encrypted"));
            let want: Option<Vec<Rating>> = if unsupported {
                Some(vec![Rating::Unsupported])
            } else if current || outside {
                Some(vec![Rating::Damaged])
            } else if deleted {
                Some(vec![Rating::Uncertain, Rating::Damaged])
            } else if intact {
                Some(vec![if validated {
                    Rating::Good
                } else {
                    Rating::Uncertain
                }])
            } else {
                None
            };
            if outside && !row.flags.contains(hds_recover::Flag::OutOfBounds) {
                fail(format!("{path}: data outside the range, not out_of_bounds"));
            }
            if let Some(want) = want {
                if row.rating.is_some_and(|r| want.contains(&r)) {
                    ratings += 1;
                } else {
                    fail(format!(
                        "{path}: rated {:?} {:?}, expected {want:?}",
                        row.rating, row.flags
                    ));
                }
            }
        }
        // Every file is in the plan of the whole instance; search finds names.
        let plan = cat
            .plan(
                store,
                &Selection {
                    include: vec![main.root.wire()],
                    include_deleted: true,
                    ..Selection::default()
                },
                None,
            )
            .unwrap();
        let files = cat
            .node_ids()
            .filter(|&i| {
                cat.kind(i).is_some_and(|k| k.is_file())
                    && cat.instance_kind(i) == Some(InstanceKind::Main)
            })
            .count();
        assert_eq!(plan.files.len(), files, "{name}");
        let some = cat
            .node_ids()
            .find(|&i| cat.kind(i) == Some(NodeKind::File))
            .unwrap();
        let q = cat.name(store, some).to_uppercase();
        let hits = cat
            .search(
                store,
                &SearchArgs {
                    filter: Filter {
                        query: Some(q),
                        ..Filter::default()
                    },
                    show_deleted: true,
                    limit: 1000,
                    ..SearchArgs::default()
                },
            )
            .unwrap();
        assert!(hits.rows.iter().any(|r| r.row.id == some), "{name}: search");
        eprintln!(
            "{name}: {checked} entries checked, {paths} paths ({unnamed} more below a nameless \
             lost folder), {sizes} sizes, {ratings} ratings; {} nodes, main validated {validated}",
            cat.node_count()
        );
        assert!(checked > 0, "{name}");
        let want_unnamed = UNNAMED
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(0, |&(_, k)| k);
        if unnamed != want_unnamed {
            failures.push(format!(
                "{name}: {unnamed} entries below a nameless lost folder, expected {want_unnamed}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
