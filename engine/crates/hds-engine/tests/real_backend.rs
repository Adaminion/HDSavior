//! The engine with its real backend (`build_backend()`: hds-core and
//! hds-recover) on the real-driver fixtures (`HDS_FIXTURES`, made by
//! scripts/linux/make-fixtures.sh), through the protocol server, compared
//! with the fixtures' manifests.
//!
//! Without `HDS_FIXTURES` every test prints SKIPPED and passes; with it set,
//! a missing image fails.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Harness, TempDir};
use hds_engine::protocol::ErrorCode;
use serde_json::{json, Value};

/// Every fixture of scripts/linux/make-fixtures.sh.
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

/// Entries with a reconstructible path that E6 leaves below a nameless lost
/// folder (the same exception as hds-core's catalog fixture test).
const UNNAMED: [(&str, usize); 1] = [("embedded", 32)];

const JOB_TIMEOUT: Duration = Duration::from_secs(600);

fn fixtures_dir() -> Option<PathBuf> {
    let Some(dir) = std::env::var_os("HDS_FIXTURES") else {
        eprintln!("SKIPPED: HDS_FIXTURES is not set");
        return None;
    };
    Some(std::path::absolute(PathBuf::from(dir)).expect("absolute fixture directory"))
}

fn manifest(dir: &Path, name: &str) -> Value {
    let path = dir.join(format!("{name}.manifest.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).expect("manifest JSON")
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or_else(|| panic!("not a number: {v}"))
}

fn image_path(dir: &Path, name: &str) -> String {
    let path = dir.join(format!("{name}.img"));
    assert!(path.is_file(), "{} is missing", path.display());
    path.to_string_lossy().into_owned()
}

/// The scan range of a fixture.
fn scan_range(m: &Value) -> (u64, u64) {
    let start = u(&m["scan_range"]["offset"]);
    (start, start + u(&m["scan_range"]["length"]))
}

/// Opens a fixture and scans its manifest's range into `session`; returns
/// `scan_done`.
fn scan_fixture(h: &mut Harness, dir: &Path, name: &str, m: &Value, session: &str) -> Value {
    let sector = m["image"]["sector_size"].as_u64();
    h.ok(
        "open_source",
        json!({"kind": "image", "path": image_path(dir, name), "sector_size": sector}),
    );
    let (start, end) = scan_range(m);
    let started = h.ok(
        "scan_start",
        json!({"start": start, "end": end, "session_path": session,
               "allow_same_disk": true, "allow_unknown_disk": true}),
    );
    assert_eq!(started["job"], "scan");
    assert_eq!(u(&started["bytes_total"]), end - start);
    h.wait_event_for("scan_done", JOB_TIMEOUT)
}

/// Every row below `root` by display path (root excluded), listing with
/// deleted items, page by page.
fn walk(h: &mut Harness, epoch: u64, root: u64) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let mut stack: Vec<(u64, String)> = vec![(root, String::new())];
    while let Some((node, prefix)) = stack.pop() {
        let mut offset = 0u64;
        loop {
            let page = h.ok(
                "list_children",
                json!({"epoch": epoch, "node": node, "offset": offset, "limit": 1000,
                       "show_deleted": true}),
            );
            let rows = page["rows"].as_array().unwrap().clone();
            for row in &rows {
                let name = row["name"].as_str().unwrap();
                let path = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}/{name}")
                };
                if row["kind"] == "dir" || row["kind"] == "lost_dir" || row["kind"] == "virtual" {
                    stack.push((u(&row["id"]), path.clone()));
                }
                out.push((path, row.clone()));
            }
            offset += rows.len() as u64;
            if rows.is_empty() || offset >= u(&page["total"]) {
                break;
            }
        }
    }
    out
}

/// The whole content of a file through `read_file`, in pieces.
fn read_all(h: &mut Harness, epoch: u64, node: u64) -> Vec<u8> {
    let mut data = Vec::new();
    loop {
        let r = h.ok(
            "read_file",
            json!({"epoch": epoch, "node": node, "offset": data.len(), "length": 1 << 20}),
        );
        let chunk = hds_engine::base64::decode(r["data_b64"].as_str().unwrap()).unwrap();
        data.extend_from_slice(&chunk);
        if r["eof"] == true || chunk.is_empty() {
            assert_eq!(data.len() as u64, u(&r["size"]));
            return data;
        }
    }
}

/// What the catalog must say about one manifest entry (see hds-core's
/// catalog fixture test for the reasoning).
fn wanted_ratings(e: &Value, validated: bool) -> Option<Vec<&'static str>> {
    let after = &e["after"];
    let flags: Vec<&str> = e["ondisk"]["data_flags"]
        .as_array()
        .map(|l| l.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let resident = e["ondisk"]["resident"] == true;
    let unsupported = !resident && (flags.contains(&"compressed") || flags.contains(&"encrypted"));
    let current = after["overlaps_new_allocation"] == true;
    let outside = after["data_inside_scan_range"] == false;
    let deleted = e["deleted"] == true;
    let intact = after["content_intact"] == true;
    if unsupported {
        Some(vec!["unsupported"])
    } else if current || outside {
        Some(vec!["damaged"])
    } else if deleted {
        Some(vec!["uncertain", "damaged"])
    } else if intact {
        Some(vec![if validated { "good" } else { "uncertain" }])
    } else {
        None
    }
}

#[test]
fn fixtures_scan_browse_search_and_read_like_the_manifests() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real");
    let mut failures: Vec<String> = Vec::new();
    for name in FIXTURES {
        let m = manifest(&dir, name);
        let mut h = Harness::real();
        let session = scratch.join(&format!("{name}.hds"));
        let done = scan_fixture(&mut h, &dir, name, &m, &session);
        assert_eq!(done["status"], "completed", "{name}: {done}");
        assert!(done["error"].is_null());
        assert!(Path::new(done["session_path"].as_str().unwrap()).is_file());
        let catalog = done["catalog"].clone();
        let epoch = u(&catalog["epoch"]);
        assert_eq!(catalog["scan_complete"], true, "{name}");
        assert_eq!(catalog["source_attached"], true, "{name}");
        let (start, end) = scan_range(&m);
        assert_eq!(catalog["range"], json!({"start": start, "end": end}));
        assert_eq!(
            catalog["protected"][0],
            json!({"start": start, "end": end, "reason": "session_range"})
        );
        let main = &catalog["instances"][0];
        assert_eq!(main["kind"], "main", "{name}");
        let validated = main["geometry"]["validated"] == true;
        // The scan_done catalog is the one catalog_info answers.
        assert_eq!(h.ok("catalog_info", json!({})), catalog, "{name}");
        let tree = walk(&mut h, epoch, u(&main["root"]));
        // Every file and folder with a record, by the record's disk offset.
        let mut by_record: HashMap<u64, (String, &Value)> = HashMap::new();
        for (path, row) in &tree {
            if row["kind"] != "file" && row["kind"] != "dir" {
                continue;
            }
            let info = h.ok("node_info", json!({"epoch": epoch, "node": row["id"]}));
            assert_eq!(&info["path"], path, "{name}: node_info and the listing agree");
            assert_eq!(&info["row"]["id"], &row["id"]);
            if let Some(offset) = info["record"]["disk_offset"].as_u64() {
                by_record.insert(offset, (path.clone(), row));
            }
        }
        let (mut checked, mut paths, mut sizes, mut ratings, mut unnamed, mut read) =
            (0, 0, 0, 0, 0, 0);
        let mut fail = |msg: String| {
            if failures.len() < 80 {
                failures.push(format!("{name}: {msg}"));
            }
        };
        for e in m["entries"].as_array().unwrap() {
            let after = &e["after"];
            if after["record_intact"] != true || u(&e["mft_record"]) < 24 {
                continue;
            }
            checked += 1;
            let path = e["path"].as_str().unwrap();
            let last = path.rsplit('/').next().unwrap();
            let offset = u(&e["ondisk"]["record_offset"]);
            let Some((got, row)) = by_record.get(&offset) else {
                fail(format!("{path}: no node for the record at {offset}"));
                continue;
            };
            let (got, row) = (got.as_str(), *row);
            if after["ancestry"]["path_reconstructible"] == true {
                if got == path {
                    paths += 1;
                } else if got.starts_with("[Lost folders]/Folder #") && got.ends_with(last) {
                    unnamed += 1;
                } else {
                    fail(format!("{path}: at {got:?}"));
                }
            } else {
                let suffix = after["ancestry"]["surviving_suffix"].as_str().unwrap_or(last);
                let lost =
                    got.starts_with("[Lost folders]/") || got.starts_with("[Orphaned files]/");
                if got.ends_with(suffix) && (lost || got == suffix) {
                    paths += 1;
                } else {
                    fail(format!("{path}: at {got:?}, not below a lost folder with {suffix:?}"));
                }
            }
            if e["type"] != "file" {
                if row["kind"] != "dir" {
                    fail(format!("{path}: kind {}", row["kind"]));
                }
                continue;
            }
            if row["kind"] != "file" {
                fail(format!("{path}: kind {}", row["kind"]));
                continue;
            }
            if row["deleted"] != e["deleted"] {
                fail(format!("{path}: deleted {}", row["deleted"]));
            }
            let intact = after["content_intact"] == true;
            if intact {
                if row["size"] == e["size"] {
                    sizes += 1;
                } else {
                    fail(format!("{path}: size {} != {}", row["size"], e["size"]));
                }
                if e["deleted"] != true && row["modified"] != e["modified_unix_ms"] {
                    fail(format!(
                        "{path}: modified {} != {}",
                        row["modified"], e["modified_unix_ms"]
                    ));
                }
            }
            if let Some(want) = wanted_ratings(e, validated) {
                if want.iter().any(|r| row["rating"] == *r) {
                    ratings += 1;
                } else {
                    fail(format!(
                        "{path}: rated {} {}, expected {want:?}",
                        row["rating"], row["flags"]
                    ));
                }
            }
            // A sample of good files: the content read through read_file
            // is the manifest's (SHA-256), and node_info names the record.
            let node = u(&row["id"]);
            if row["rating"] == "good" && u(&e["size"]) <= 4 << 20 && read < 12 {
                read += 1;
                let data = read_all(&mut h, epoch, node);
                if sha256_hex(&data) != e["sha256"].as_str().unwrap_or("") {
                    fail(format!("{path}: read_file content differs"));
                }
                let info = h.ok("node_info", json!({"epoch": epoch, "node": node}));
                if info["path"] != got || info["record"]["disk_offset"] != e["ondisk"]["record_offset"]
                {
                    fail(format!(
                        "{path}: node_info {} at {}",
                        info["path"], info["record"]["disk_offset"]
                    ));
                }
                // read_file at offset 0 checked the signature (G1).
                let flags = info["row"]["flags"].as_array().unwrap();
                if data.len() >= 16
                    && hds_carve::matches_extension(&data, row["ext"].as_str().unwrap()).is_some()
                    && !flags.iter().any(|f| f == "signature_ok")
                {
                    fail(format!("{path}: no signature flag after read_file: {flags:?}"));
                }
                // Search finds it by its name, with its path.
                let found = h.ok(
                    "search",
                    json!({"epoch": epoch, "filter": {"query": last.to_uppercase()},
                           "show_deleted": true, "limit": 1000}),
                );
                if !found["rows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["id"] == node && r["path"] == got)
                {
                    fail(format!("{path}: search for {last:?} does not find it"));
                }
            }
        }
        // The selection of the whole main instance holds every file.
        let files = tree
            .iter()
            .map(|(_, r)| r)
            .filter(|r| r["kind"] == "file" || r["kind"] == "carved")
            .count() as u64;
        let summary = h.ok(
            "selection_summary",
            json!({"epoch": epoch, "include": [u(&main["root"])], "include_deleted": true}),
        );
        assert_eq!(u(&summary["files"]), files, "{name}");
        assert_eq!(
            u(&summary["good"])
                + u(&summary["uncertain"])
                + u(&summary["damaged"])
                + u(&summary["unsupported"]),
            files,
            "{name}"
        );
        let want_unnamed = UNNAMED
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(0, |&(_, k)| k);
        if unnamed != want_unnamed {
            failures.push(format!(
                "{name}: {unnamed} entries below a nameless lost folder, expected {want_unnamed}"
            ));
        }
        eprintln!(
            "{name}: {checked} entries, {paths} paths, {sizes} sizes, {ratings} ratings, \
             {read} files read; {} nodes, validated {validated}",
            catalog["nodes"]
        );
        assert!(checked > 0, "{name}");
        // Without a validated geometry nothing is rated good (F6) and no
        // sample is read.
        assert!(read > 0 || !validated, "{name}");
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The rows of the main root, without their (volatile) flags of reads.
fn root_listing(h: &mut Harness) -> (Value, Value) {
    let catalog = h.ok("catalog_info", json!({}));
    let epoch = catalog["epoch"].clone();
    let root = catalog["instances"][0]["root"].clone();
    let rows = h.ok(
        "list_children",
        json!({"epoch": epoch, "node": root, "limit": 1000, "show_deleted": true}),
    );
    (catalog, rows)
}

/// A catalog without what differs between equal catalogs: the epoch and
/// the time of the current-volume snapshot.
fn without_epoch(mut catalog: Value) -> Value {
    catalog["epoch"] = json!(0);
    if let Some(list) = catalog["live_volumes"].as_array_mut() {
        for v in list {
            v["snapshot_time"] = json!(null);
        }
    }
    catalog
}

#[test]
fn sessions_save_load_close_attach_and_identity() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-session");
    let name = "offset-gpt";
    let m = manifest(&dir, name);
    let session = scratch.join("s.hds");
    let mut h = Harness::real();
    let done = scan_fixture(&mut h, &dir, name, &m, &session);
    assert_eq!(done["status"], "completed", "{done}");
    let (catalog, rows) = root_listing(&mut h);
    let epoch = u(&catalog["epoch"]);

    // session_save writes a compacted copy and continues in it.
    let copy = scratch.join("copy.hds");
    let saved = h.ok(
        "session_save",
        json!({"path": copy, "allow_unknown_disk": true, "allow_same_disk": true}),
    );
    assert!(u(&saved["bytes"]) > 0);
    assert!(Path::new(saved["path"].as_str().unwrap()).is_file());

    // close_source: the catalog stays (state B), reads need the source.
    h.ok("close_source", json!({}));
    let b = h.ok("catalog_info", json!({}));
    assert_eq!(b["source_attached"], false);
    assert_eq!(u(&b["epoch"]), epoch);
    let file = rows["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "file")
        .map(|r| u(&r["id"]));
    if let Some(file) = file {
        let e = h.err("read_file", json!({"epoch": epoch, "node": file}));
        assert_eq!(e.code, ErrorCode::NoSource);
    }
    let same = h.ok(
        "list_children",
        json!({"epoch": epoch, "node": catalog["instances"][0]["root"], "limit": 1000,
               "show_deleted": true}),
    );
    assert_eq!(same, rows);
    let e = h.err("scan_start", json!({"resume": true}));
    assert_eq!(e.code, ErrorCode::NoSource);

    // A different image is refused; the right one is attached again with a
    // new epoch and the same catalog.
    let other = image_path(&dir, "offset-mbr");
    let e = h.err(
        "session_attach_source",
        json!({"kind": "image", "path": other}),
    );
    assert_eq!(e.code, ErrorCode::SourceMismatch, "{e:?}");
    assert!(e.data.unwrap()["failed"].is_array());
    let attached = h.ok(
        "session_attach_source",
        json!({"kind": "image", "path": image_path(&dir, name)}),
    );
    assert_eq!(attached["catalog"]["source_attached"], true);
    let epoch2 = u(&attached["catalog"]["epoch"]);
    assert!(epoch2 > epoch);
    assert_eq!(
        without_epoch(attached["catalog"].clone()),
        without_epoch(catalog.clone())
    );
    let e = h.err(
        "list_children",
        json!({"epoch": epoch, "node": catalog["instances"][0]["root"]}),
    );
    assert_eq!(e.code, ErrorCode::StaleCatalog);
    h.ok("shutdown", json!({}));

    // session_load in a new engine finds the recorded source by its path.
    for path in [&session, &copy] {
        let mut h = Harness::real();
        let loaded = h.ok("session_load", json!({"path": path}));
        assert_eq!(loaded["source_attached"], true, "{loaded}");
        assert!(loaded["source_error"].is_null());
        assert_eq!(loaded["source"]["path"], image_path(&dir, name));
        assert_eq!(
            without_epoch(loaded["catalog"].clone()),
            without_epoch(catalog.clone())
        );
        let (_, rows2) = root_listing(&mut h);
        assert_eq!(rows2, rows);
        // task_progress went with the load.
        assert!(h.events.iter().any(|e| e.event == "task_progress"));
        h.ok("shutdown", json!({}));
    }

    // Given another source, the session loads without one and says why.
    let mut h = Harness::real();
    let loaded = h.ok(
        "session_load",
        json!({"path": session, "source": {"kind": "image", "path": other}}),
    );
    assert_eq!(loaded["source_attached"], false);
    assert_eq!(loaded["source_error"]["code"], "source_mismatch");
    assert_eq!(loaded["catalog"]["source_attached"], false);
    assert_eq!(loaded["source"]["path"], image_path(&dir, name));
    let (_, rows3) = root_listing(&mut h);
    assert_eq!(rows3, rows);
    h.ok("shutdown", json!({}));

    // A file that is not a session.
    let mut h = Harness::real();
    let junk = scratch.join("junk.hds");
    std::fs::write(&junk, b"not a session").unwrap();
    let e = h.err("session_load", json!({"path": junk}));
    assert_eq!(e.code, ErrorCode::IoError, "{e:?}");
    let e = h.err("session_load", json!({"path": scratch.join("missing.hds")}));
    assert_eq!(e.code, ErrorCode::NotFound, "{e:?}");
}

#[test]
fn stopped_and_shut_down_scans_resume_to_the_same_catalog() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-resume");
    let name = "basic";
    let m = manifest(&dir, name);
    let (start, end) = scan_range(&m);
    let image = image_path(&dir, name);

    // Reference: one uninterrupted scan.
    let mut h = Harness::real();
    let done = scan_fixture(&mut h, &dir, name, &m, &scratch.join("full.hds"));
    let full = without_epoch(done["catalog"].clone());
    let (_, full_rows) = root_listing(&mut h);
    h.ok("shutdown", json!({}));

    // Stopped at once: a catalog of what was read, then resumed.
    let mut h = Harness::real();
    h.ok("open_source", json!({"kind": "image", "path": image}));
    let session = scratch.join("stopped.hds");
    h.ok(
        "scan_start",
        json!({"start": start, "end": end, "session_path": session,
               "allow_unknown_disk": true, "allow_same_disk": true}),
    );
    h.ok("job_cancel", json!({}));
    let done = h.wait_event_for("scan_done", JOB_TIMEOUT);
    assert!(
        done["status"] == "stopped" || done["status"] == "completed",
        "{done}"
    );
    assert!(done["catalog"].is_object(), "a stopped scan has a catalog");
    if done["status"] == "stopped" {
        assert_eq!(done["catalog"]["scan_complete"], false);
    }
    // A resume keeps the range: another one is refused.
    let e = h.err("scan_start", json!({"resume": true, "start": start + 512}));
    assert_eq!(e.code, ErrorCode::BadRequest);
    h.ok("scan_start", json!({"resume": true}));
    let done = h.wait_event_for("scan_done", JOB_TIMEOUT);
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["session_path"], session);
    assert_eq!(without_epoch(done["catalog"].clone()), full);
    let (_, rows) = root_listing(&mut h);
    assert_eq!(rows, full_rows);
    // Resuming a complete session just ends.
    h.ok("scan_start", json!({"resume": true}));
    let done = h.wait_event_for("scan_done", JOB_TIMEOUT);
    assert_eq!(done["status"], "completed");
    h.ok("shutdown", json!({}));

    // Shut down during the scan: the session is saved without a catalog;
    // loading it and resuming completes it.
    let mut h = Harness::real();
    h.ok("open_source", json!({"kind": "image", "path": image}));
    let session = scratch.join("shutdown.hds");
    h.ok(
        "scan_start",
        json!({"start": start, "end": end, "session_path": session,
               "allow_unknown_disk": true, "allow_same_disk": true}),
    );
    h.ok("shutdown", json!({}));
    let done = h.wait_event_for("scan_done", JOB_TIMEOUT);
    assert!(done["catalog"].is_null(), "{done}");
    assert!(
        done["status"] == "stopped" || done["status"] == "completed",
        "{done}"
    );
    let mut h = Harness::real();
    let loaded = h.ok("session_load", json!({"path": session}));
    if loaded["source_attached"] != true {
        // Stopped before 8 records were stored: B4 cannot prove the
        // identity without `force`.
        assert_eq!(loaded["source_error"]["code"], "source_mismatch", "{loaded}");
        assert_eq!(
            loaded["source_error"]["data"]["failed"],
            json!(["too_few_samples"])
        );
        let e = h.err("scan_start", json!({"resume": true}));
        assert_eq!(e.code, ErrorCode::NoSource);
        h.ok(
            "session_attach_source",
            json!({"kind": "image", "path": image, "force": true}),
        );
    }
    h.ok("scan_start", json!({"resume": true}));
    let done = h.wait_event_for("scan_done", JOB_TIMEOUT);
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(without_epoch(done["catalog"].clone()), full);
    h.ok("shutdown", json!({}));
}

#[test]
fn instance_and_geometry_changes_follow_the_epoch_rules() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-instances");
    // `generations` holds an older file system besides main.
    let name = "generations";
    let m = manifest(&dir, name);
    let session = scratch.join("g.hds");
    let mut h = Harness::real();
    let done = scan_fixture(&mut h, &dir, name, &m, &session);
    assert_eq!(done["status"], "completed");
    let catalog = done["catalog"].clone();
    let epoch = u(&catalog["epoch"]);
    let instances = catalog["instances"].as_array().unwrap().clone();
    let kinds: Vec<&str> = instances
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    eprintln!("{name}: instances {kinds:?}");
    assert_eq!(kinds[0], "main");

    // geometry_set to the geometry in effect keeps the epoch and the tree.
    let g = &instances[0]["geometry"];
    let set = h.ok(
        "geometry_set",
        json!({"instance": 0, "cluster_size": g["cluster_size"], "volume_offset": g["volume_offset"]}),
    );
    assert_eq!(u(&set["epoch"]), epoch);
    assert_eq!(set["instances"][0]["geometry"]["cluster_source"], "manual");
    assert_eq!(set["instances"][0]["geometry"]["offset_source"], "manual");
    assert_eq!(set["nodes"], catalog["nodes"]);
    assert!(h.events.iter().any(|e| e.event == "task_progress"));
    // An instance that does not exist, and strays, are refused.
    let e = h.err(
        "geometry_set",
        json!({"instance": 99, "cluster_size": 4096, "volume_offset": 0}),
    );
    assert_eq!(e.code, ErrorCode::NotFound);
    if let Some(strays) = kinds.iter().position(|k| *k == "strays") {
        let e = h.err("instance_set", json!({"instance": strays, "kind": "main"}));
        assert_eq!(e.code, ErrorCode::BadRequest);
    }

    // instance_set: a new epoch; the old one is stale.
    if let Some(other) = kinds
        .iter()
        .position(|k| *k == "older" || *k == "other" || *k == "embedded")
    {
        let changed = h.ok("instance_set", json!({"instance": other, "kind": "main"}));
        let epoch2 = u(&changed["epoch"]);
        assert!(epoch2 > epoch);
        assert_eq!(changed["instances"][0]["kind"], "main");
        assert_eq!(changed["instances"][0]["manual"], true);
        let e = h.err(
            "list_children",
            json!({"epoch": epoch, "node": catalog["instances"][0]["root"]}),
        );
        assert_eq!(e.code, ErrorCode::StaleCatalog);
        // The choice is stored in the session.
        h.ok("shutdown", json!({}));
        let mut h2 = Harness::real();
        let loaded = h2.ok("session_load", json!({"path": session}));
        assert_eq!(
            without_epoch(loaded["catalog"].clone()),
            without_epoch(changed.clone())
        );
        // auto withdraws it: the engine's own verdict again.
        let new_main = changed["instances"]
            .as_array()
            .unwrap()
            .iter()
            .position(|i| i["manual"] == true)
            .unwrap();
        let back = h2.ok("instance_set", json!({"instance": new_main, "kind": "auto"}));
        let mut a = without_epoch(back.clone());
        let mut b = without_epoch(catalog.clone());
        // geometry_set above stored a manual geometry for main.
        for v in [&mut a, &mut b] {
            for i in v["instances"].as_array_mut().unwrap() {
                i["geometry"]["cluster_source"] = json!(null);
                i["geometry"]["offset_source"] = json!(null);
                i["geometry"]["signature_checked"] = json!(null);
                i["geometry"]["signature_ok"] = json!(null);
            }
        }
        assert_eq!(a["instances"].as_array().unwrap().len(), kinds.len());
        let back_kinds: Vec<&str> = a["instances"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["kind"].as_str().unwrap())
            .collect();
        assert_eq!(back_kinds, kinds);
        h2.ok("shutdown", json!({}));
    } else {
        eprintln!("{name}: no second instance; instance_set not exercised");
    }
}

#[test]
fn files_on_the_source_count_as_lost_files_for_check_destination() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-dest");
    let name = "basic";
    let m = manifest(&dir, name);
    let mut h = Harness::real();
    let done = scan_fixture(&mut h, &dir, name, &m, &scratch.join("b.hds"));
    assert_eq!(done["status"], "completed");
    // The source image itself: overlaps the source; no catalog file "lies"
    // inside a regular file's extents on Linux (no disk extents), so the
    // counts are simply present.
    let check = h.ok(
        "check_destination",
        json!({"path": image_path(&dir, name), "purpose": "image"}),
    );
    assert_eq!(check["verdict"], "overlaps_source");
    assert!(check["lost_files"].is_u64() && check["lost_bytes"].is_u64());
    // Jobs of part B are not connected yet.
    let epoch = u(&done["catalog"]["epoch"]);
    let root = &done["catalog"]["instances"][0]["root"];
    let e = h.err(
        "recover_start",
        json!({"epoch": epoch, "include": [root], "dest": scratch.join("out"),
               "allow_unknown_disk": true}),
    );
    assert_eq!(e.code, ErrorCode::Unsupported);
    let e = h.err("carve_start", json!({}));
    assert_eq!(e.code, ErrorCode::Unsupported);
    // read_file on a folder.
    let e = h.err("read_file", json!({"epoch": epoch, "node": root}));
    assert_eq!(e.code, ErrorCode::BadRequest);
    let e = h.err("node_info", json!({"epoch": epoch, "node": 4_000_000_000u64}));
    assert_eq!(e.code, ErrorCode::NotFound);
    // basic holds one file system: "other" on it would leave no main.
    let kinds: Vec<Value> = done["catalog"]["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["kind"].clone())
        .collect();
    if kinds == [json!("main")] {
        let e = h.err("instance_set", json!({"instance": 0, "kind": "other"}));
        assert_eq!(e.code, ErrorCode::BadRequest);
        assert_eq!(h.epoch(), epoch, "a refused change keeps the epoch");
        // "main" on the main instance only marks it as the user's choice.
        let set = h.ok("instance_set", json!({"instance": 0, "kind": "main"}));
        assert_eq!(set["instances"][0]["manual"], true);
        assert!(u(&set["epoch"]) > epoch);
    }
    h.ok("shutdown", json!({}));
}

// ----------------------------------------------------------------------------------------------
// SHA-256 (FIPS 180-4), for comparing contents with the manifests.
// ----------------------------------------------------------------------------------------------

fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[test]
fn sha256_matches_the_standard_vectors() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn other_turns_an_embedded_file_system_into_a_volume_of_its_own() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-embedded");
    let name = "embedded";
    let m = manifest(&dir, name);
    let mut h = Harness::real();
    let done = scan_fixture(&mut h, &dir, name, &m, &scratch.join("e.hds"));
    assert_eq!(done["status"], "completed");
    let catalog = done["catalog"].clone();
    let instances = catalog["instances"].as_array().unwrap().clone();
    let Some(k) = instances.iter().position(|i| i["kind"] == "embedded") else {
        panic!("{name}: no embedded instance: {catalog}");
    };
    assert!(instances[k]["embedded_in"].is_string());
    let epoch = u(&catalog["epoch"]);
    // Its files are browse-only: read_file refuses them.
    let root = u(&instances[k]["root"]);
    let found = h.ok(
        "search",
        json!({"epoch": epoch, "under": root, "show_deleted": true, "limit": 5}),
    );
    if let Some(row) = found["rows"].as_array().unwrap().first() {
        assert_eq!(row["rating"], "unsupported");
        let e = h.err("read_file", json!({"epoch": epoch, "node": row["id"]}));
        assert_eq!(e.code, ErrorCode::Unsupported);
    }
    let changed = h.ok("instance_set", json!({"instance": k, "kind": "other"}));
    assert!(u(&changed["epoch"]) > epoch);
    let now = changed["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["manual"] == true)
        .expect("the instance the user set")
        .clone();
    assert_eq!(now["kind"], "other");
    assert!(now["embedded_in"].is_null());
    assert_eq!(changed["instances"][0]["kind"], "main");
    h.ok("shutdown", json!({}));
}

// ----------------------------------------------------------------------------------------------
// The command-line interface (ARCHITECTURE 5.6) with the build backend.
// ----------------------------------------------------------------------------------------------

fn cli(args: &[&str]) -> (i32, Vec<Value>, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_hdsavior-engine"))
        .args(args)
        .env_remove("HDS_ENGINE_TEST_BACKEND")
        .output()
        .expect("run the engine");
    let lines = String::from_utf8(out.stdout)
        .expect("UTF-8 stdout")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect();
    (
        out.status.code().unwrap_or(-1),
        lines,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn cli_scan_info_ls_and_find_on_fixtures() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-cli");
    for name in ["basic", "offset-mbr"] {
        let m = manifest(&dir, name);
        let image = image_path(&dir, name);
        let session = scratch.join(&format!("{name}.hds"));
        // offset-mbr: the old volume is the first partition.
        let range: Vec<String> = if m["image"]["partition_table"].is_null() {
            Vec::new()
        } else {
            vec!["--partition".into(), "1".into()]
        };
        let mut args = vec!["scan", image.as_str(), "--session", session.as_str()];
        args.extend(range.iter().map(String::as_str));
        let (code, lines, stderr) = cli(&args);
        assert_eq!(code, 0, "{name}: {stderr}");
        let done = lines.last().unwrap();
        assert_eq!(done["event"], "scan_done", "{name}");
        assert_eq!(done["data"]["status"], "completed");
        let (start, end) = scan_range(&m);
        assert_eq!(
            done["data"]["catalog"]["range"],
            json!({"start": start, "end": end}),
            "{name}: --partition scans the manifest's range"
        );
        let nodes = done["data"]["catalog"]["nodes"].clone();

        let (code, lines, stderr) = cli(&["info", "--session", &session]);
        assert_eq!(code, 0, "{name}: {stderr}");
        let info = lines.last().unwrap();
        assert_eq!(info["source_attached"], true, "{name}: {info}");
        assert_eq!(info["catalog"]["nodes"], nodes);

        // ls --path and find agree with the manifest.
        let entry = m["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e["type"] == "file"
                    && e["deleted"] == false
                    && e["path"].as_str().unwrap().contains('/')
                    && e["after"]["record_intact"] == true
                    && e["after"]["ancestry"]["path_reconstructible"] == true
            })
            .expect("a file in a folder");
        let path = entry["path"].as_str().unwrap();
        let (folder, file) = path.rsplit_once('/').unwrap();
        let (code, lines, stderr) = cli(&["ls", "--session", &session, "--path", folder]);
        assert_eq!(code, 0, "{name}: {stderr}");
        let listing = lines.last().unwrap();
        let row = listing["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == file)
            .unwrap_or_else(|| panic!("{name}: {file} not listed in {folder}: {listing}"));
        assert_eq!(row["size"], entry["size"]);
        let ext = file.rsplit_once('.').map(|(_, e)| e.to_lowercase());
        let mut args = vec!["find", "--session", session.as_str(), file];
        if let Some(ext) = &ext {
            args.extend(["--ext", ext.as_str()]);
        }
        let (code, lines, stderr) = cli(&args);
        assert_eq!(code, 0, "{name}: {stderr}");
        let found = lines.last().unwrap();
        assert!(
            found["rows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["path"] == path && r["id"] == row["id"]),
            "{name}: find {file}: {found}"
        );
        // A path that does not exist fails with exit code 1.
        let (code, _, stderr) = cli(&["ls", "--session", &session, "--path", "no/such/folder"]);
        assert_eq!(code, 1);
        assert!(stderr.contains("no entry named"), "{stderr}");
    }
}
