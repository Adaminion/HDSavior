//! The jobs of the real backend (recover, carve, image) on the real-driver
//! fixtures (`HDS_FIXTURES`, scripts/linux/make-fixtures.sh): through the
//! protocol server, through the command-line interface, and - where a
//! fault must strike in the middle of a job - through the backend with a
//! `FaultySource`.
//!
//! Without `HDS_FIXTURES` every test prints SKIPPED and passes.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use common::{sha256_hex, Harness, TempDir};
use hds_engine::backend::{
    Backend, CarveRequest, Destination, ImageRequest, JobContext, RecoverRequest, ScanRequest,
};
use hds_engine::output::Output;
use hds_engine::protocol::{ErrorCode, JobStatus, OnExists, RecoverStartArgs, ScanStatus};
use hds_engine::real_backend::RealBackend;
use hds_io::testing::FaultySource;
use hds_io::{destination_check, BlockSource, ImageFile, Protected, Purpose};
use serde_json::{json, Value};

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

fn scan_range(m: &Value) -> (u64, u64) {
    let start = u(&m["scan_range"]["offset"]);
    (start, start + u(&m["scan_range"]["length"]))
}

/// Opens `image` and scans `range`; returns `scan_done`.
fn scan(
    h: &mut Harness,
    image: &str,
    sector: Option<u64>,
    range: (u64, u64),
    session: &str,
) -> Value {
    h.ok(
        "open_source",
        json!({"kind": "image", "path": image, "sector_size": sector}),
    );
    h.ok(
        "scan_start",
        json!({"start": range.0, "end": range.1, "session_path": session}),
    );
    let done = h.wait_event_for("scan_done", JOB_TIMEOUT);
    assert_eq!(done["status"], "completed", "{done}");
    done
}

fn scan_fixture(h: &mut Harness, dir: &Path, name: &str, m: &Value, session: &str) -> Value {
    scan(
        h,
        &image_path(dir, name),
        m["image"]["sector_size"].as_u64(),
        scan_range(m),
        session,
    )
}

/// One row of a recovery report.
#[derive(Debug, Clone)]
struct ReportRow {
    status: String,
    flags: String,
    output_path: String,
    original_path: String,
    record_offset: Option<u64>,
    first_data_offset: Option<u64>,
}

/// Parses an RFC 4180 report (BOM, CRLF, quoted cells, apostrophe guard).
fn read_report(path: &Path) -> Vec<ReportRow> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(
        bytes.starts_with(b"\xEF\xBB\xBF"),
        "the report starts with a BOM"
    );
    let text = String::from_utf8(bytes[3..].to_vec()).expect("UTF-8 report");
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut cell = String::new();
    let mut record: Vec<String> = Vec::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                chars.next();
                cell.push('"');
            }
            (true, '"') => quoted = false,
            (true, c) => cell.push(c),
            (false, '"') => quoted = true,
            (false, ',') => record.push(std::mem::take(&mut cell)),
            (false, '\r') => {
                assert_eq!(chars.next(), Some('\n'), "CRLF line ends");
                record.push(std::mem::take(&mut cell));
                records.push(std::mem::take(&mut record));
            }
            (false, c) => cell.push(c),
        }
    }
    assert!(
        cell.is_empty() && record.is_empty(),
        "the last line ends with CRLF"
    );
    let header = records.remove(0);
    assert_eq!(
        header,
        [
            "status",
            "flags",
            "size",
            "bytes_written",
            "output_path",
            "original_path",
            "mft_record",
            "record_offset",
            "first_data_offset",
            "modified_utc",
            "detail"
        ]
    );
    let unguard = |s: &str| -> String {
        match s.strip_prefix('\'') {
            Some(rest) if rest.starts_with(['=', '+', '-', '@', '\t']) => rest.to_string(),
            _ => s.to_string(),
        }
    };
    records
        .into_iter()
        .map(|r| {
            assert_eq!(r.len(), 11, "{r:?}");
            ReportRow {
                status: r[0].clone(),
                flags: r[1].clone(),
                output_path: unguard(&r[4]),
                original_path: unguard(&r[5]),
                record_offset: r[7].parse().ok(),
                first_data_offset: r[8].parse().ok(),
            }
        })
        .collect()
}

fn written(status: &str) -> bool {
    status == "ok" || status == "ok_unverified" || status.starts_with("warning")
}

/// Every `~hds-*.part` file below `dir`.
fn part_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(p);
            } else if name.starts_with("~hds-") && name.ends_with(".part") {
                out.push(p);
            }
        }
    }
    out
}

/// Recovers `include` to `dest` and waits for `recover_done`; checks the
/// counters and returns (start result, done, report rows).
fn recover(
    h: &mut Harness,
    epoch: u64,
    include: &[u64],
    dest: &str,
    include_deleted: bool,
) -> (Value, Value, Vec<ReportRow>) {
    let started = h.ok(
        "recover_start",
        json!({"epoch": epoch, "include": include, "dest": dest,
               "include_deleted": include_deleted}),
    );
    assert_eq!(started["job"], "recover");
    let done = h.wait_event_for("recover_done", JOB_TIMEOUT);
    assert_eq!(done["status"], "completed", "{done}");
    assert!(done["error"].is_null());
    assert_eq!(done["files_total"], started["files"]);
    assert_eq!(
        u(&done["ok"]) + u(&done["warnings"]) + u(&done["failed"]) + u(&done["skipped"]),
        u(&done["files_total"]),
        "{done}"
    );
    let report = PathBuf::from(done["report_path"].as_str().expect("report path"));
    assert!(
        report.starts_with(dest),
        "the report is in dest: {}",
        report.display()
    );
    let rows = read_report(&report);
    assert_eq!(rows.len() as u64, u(&done["files_total"]));
    assert!(part_files(Path::new(dest)).is_empty());
    (started, done, rows)
}

// ----------------------------------------------------------------------------------------------
// Recovery through the protocol
// ----------------------------------------------------------------------------------------------

/// Checks a recovery of a whole main tree against the manifest: every file
/// whose record and content survived comes back bit-identical (deleted,
/// fragmented and sparse ones included), and nothing reported `ok` differs
/// from the manifest (S5). Returns (files compared, ok rows).
fn check_against_manifest(name: &str, m: &Value, dest: &Path, rows: &[ReportRow]) -> (u64, u64) {
    let by_record: HashMap<u64, &ReportRow> = rows
        .iter()
        .filter_map(|r| Some((r.record_offset?, r)))
        .collect();
    let mut failures = Vec::new();
    let (mut compared, mut ok_rows, mut deleted, mut sparse, mut fragmented) = (0, 0, 0, 0, 0);
    for e in m["entries"].as_array().unwrap() {
        if e["type"] != "file" || e["after"]["record_intact"] != true {
            continue;
        }
        let Some(offset) = e["ondisk"]["record_offset"].as_u64() else {
            continue;
        };
        let path = e["path"].as_str().unwrap();
        let flags: Vec<&str> = e["ondisk"]["data_flags"]
            .as_array()
            .map(|l| l.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let resident = e["ondisk"]["resident"] == true;
        let unsupported =
            !resident && (flags.contains(&"compressed") || flags.contains(&"encrypted"));
        let row = by_record.get(&offset);
        if let Some(row) = row {
            if row.status == "ok" || row.status == "ok_unverified" {
                ok_rows += 1;
                let data = std::fs::read(dest.join(&row.output_path)).unwrap_or_default();
                if e["after"]["content_intact"] != true || sha256_hex(&data) != e["sha256"] {
                    failures.push(format!("{path}: reported {} but differs (S5)", row.status));
                }
            }
        }
        let expected = e["after"]["content_intact"] == true
            && e["after"]["overlaps_new_allocation"] != true
            && e["after"]["data_inside_scan_range"] != false
            && !unsupported;
        if !expected {
            continue;
        }
        let Some(row) = row else {
            failures.push(format!("{path}: not in the report"));
            continue;
        };
        if !written(&row.status) {
            // Under Wine a name of more than 255 UTF-8 bytes fails on the
            // Linux host with an error that does not lead to the fallback
            // name (hds-recover's report); NTFS takes such names.
            let host_limit = cfg!(windows)
                && row.status == "failed:write_error"
                && path.rsplit('/').next().is_some_and(|n| n.len() > 255);
            if host_limit {
                eprintln!("{name}: {path}: {} (the host's name limit)", row.status);
            } else {
                failures.push(format!("{path}: {} {}", row.status, row.flags));
            }
            continue;
        }
        let data = std::fs::read(dest.join(&row.output_path))
            .unwrap_or_else(|e| panic!("{name}: {}: {e}", row.output_path));
        if sha256_hex(&data) != e["sha256"] {
            failures.push(format!(
                "{path}: recovered as {} ({}) with other content",
                row.output_path, row.status
            ));
            continue;
        }
        if e["after"]["ancestry"]["path_reconstructible"] == true
            && e["deleted"] != true
            && row.original_path != path
        {
            failures.push(format!("{path}: original_path {}", row.original_path));
        }
        compared += 1;
        if e["deleted"] == true {
            deleted += 1;
        }
        if flags.contains(&"sparse") {
            sparse += 1;
        }
        if e["ondisk"]["fragments"].as_u64().unwrap_or(0) > 1 {
            fragmented += 1;
        }
    }
    assert!(
        failures.is_empty(),
        "{name}: {} failures:\n{}",
        failures.len(),
        failures[..failures.len().min(60)].join("\n")
    );
    eprintln!(
        "{name}: {compared} files bit-identical ({deleted} deleted, {sparse} sparse, \
         {fragmented} fragmented), {ok_rows} ok rows"
    );
    assert!(compared > 0, "{name}");
    (compared, ok_rows)
}

#[test]
fn recovery_brings_back_fixture_files_bit_identical() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-recover");
    for name in [
        "basic",
        "fragmented",
        "offset-mbr",
        "offset-mbr63",
        "offset-gpt",
        "4k-records",
        "big-clusters",
    ] {
        let m = manifest(&dir, name);
        let mut h = Harness::real();
        let session = scratch.join(&format!("{name}.hds"));
        let done = scan_fixture(&mut h, &dir, name, &m, &session);
        let catalog = &done["catalog"];
        let epoch = u(&catalog["epoch"]);
        let main = u(&catalog["instances"][0]["root"]);
        let summary = h.ok(
            "selection_summary",
            json!({"epoch": epoch, "include": [main], "include_deleted": true}),
        );
        let dest = scratch.join(&format!("{name}-out"));
        let (started, rdone, rows) = recover(&mut h, epoch, &[main], &dest, true);
        assert_eq!(started["files"], summary["files"], "{name}");
        assert_eq!(rdone["dest"], dest, "{name}");
        // Unsupported files are skipped, never written.
        assert!(rows
            .iter()
            .filter(|r| r.flags.contains("compressed") || r.flags.contains("encrypted"))
            .all(|r| r.status.starts_with("skipped:")));
        let (_, ok_rows) = check_against_manifest(name, &m, Path::new(&dest), &rows);
        assert!(ok_rows > 0 || catalog["instances"][0]["geometry"]["validated"] != true);
        // H5: files are written in ascending order of their first data byte.
        let firsts: Vec<u64> = rows.iter().filter_map(|r| r.first_data_offset).collect();
        assert!(firsts.windows(2).all(|w| w[0] <= w[1]), "{name}: H5 order");

        // G1: what the recovery read shows in later rows.
        let ok = rows
            .iter()
            .find(|r| r.status == "ok" && r.record_offset.is_some())
            .cloned();
        if let Some(ok) = ok {
            let found = h.ok(
                "search",
                json!({"epoch": epoch, "filter": {"query": Path::new(&ok.original_path)
                    .file_name().unwrap().to_string_lossy()}, "show_deleted": true, "limit": 1000}),
            );
            let row = found["rows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["path"] == ok.original_path)
                .unwrap_or_else(|| panic!("{name}: {} not found", ok.original_path));
            assert!(
                row["flags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f == "signature_ok"),
                "{name}: {row}"
            );
        }

        // A second run into the same destination: on_exists skip.
        let again = h.ok(
            "recover_start",
            json!({"epoch": epoch, "include": [main], "dest": dest, "include_deleted": true,
                   "on_exists": "skip"}),
        );
        let second = h.wait_event_for("recover_done", JOB_TIMEOUT);
        assert_eq!(second["status"], "completed");
        assert_eq!(second["files_total"], again["files"]);
        assert_ne!(second["report_path"], rdone["report_path"]);
        let rows2 = read_report(Path::new(second["report_path"].as_str().unwrap()));
        let second: HashMap<(String, Option<u64>), &ReportRow> = rows2
            .iter()
            .map(|r| ((r.original_path.clone(), r.record_offset), r))
            .collect();
        let not_skipped: Vec<String> = rows
            .iter()
            .filter(|r| written(&r.status))
            // Wine on a Linux host: see check_against_manifest.
            .filter(|r| {
                !(cfg!(windows)
                    && r.original_path
                        .rsplit('/')
                        .next()
                        .is_some_and(|n| n.len() > 255))
            })
            .filter_map(|r| {
                let again = second.get(&(r.original_path.clone(), r.record_offset))?;
                (again.status != "skipped:exists").then(|| {
                    format!(
                        "{} -> {} ({})",
                        r.output_path, again.status, again.output_path
                    )
                })
            })
            .collect();
        assert!(
            not_skipped.is_empty(),
            "{name}: every written file exists now: {not_skipped:?}"
        );
        h.ok("shutdown", json!({}));
    }
}

#[test]
fn recovery_refuses_the_source_and_bad_selections() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-refuse");
    let m = manifest(&dir, "basic");
    let image = image_path(&dir, "basic");
    let mut h = Harness::real();
    // State O: the image file itself is the source, also for an image.
    h.ok("open_source", json!({"kind": "image", "path": image}));
    let e = h.err("image_start", json!({"dest": image}));
    assert_eq!(e.code, ErrorCode::DestinationRefused);
    assert_eq!(e.data.as_ref().unwrap()["verdict"], "overlaps_source");
    assert_eq!(e.data.as_ref().unwrap()["reason"], "source_image");

    let session = scratch.join("basic.hds");
    let done = scan_fixture(&mut h, &dir, "basic", &m, &session);
    let epoch = u(&done["catalog"]["epoch"]);
    let main = u(&done["catalog"]["instances"][0]["root"]);
    // The image that is the source, and a folder "inside" it.
    for dest in [image.clone(), format!("{image}/out")] {
        let e = h.err(
            "recover_start",
            json!({"epoch": epoch, "include": [main], "dest": dest,
                   "allow_same_disk": true, "allow_unknown_disk": true}),
        );
        assert_eq!(e.code, ErrorCode::DestinationRefused, "{dest}: {e}");
        assert!(e.data.is_some(), "{dest}: the check is the data");
    }
    let e = h.err(
        "image_start",
        json!({"dest": image, "allow_same_disk": true, "allow_unknown_disk": true}),
    );
    assert_eq!(e.code, ErrorCode::DestinationRefused);
    assert_eq!(e.data.as_ref().unwrap()["reason"], "source_image");
    let e = h.err("session_save", json!({"path": image}));
    assert_eq!(e.code, ErrorCode::DestinationRefused);
    // Nothing was written next to the image.
    assert!(!Path::new(&format!("{image}.hdsimg")).exists());
    // A selection without files.
    let folder = m["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "dir" && e["after"]["record_intact"] == true)
        .map(|e| e["path"].as_str().unwrap().to_string());
    let _ = folder;
    assert_eq!(
        h.code(
            "recover_start",
            json!({"epoch": epoch, "include": [], "dest": scratch.join("x")})
        ),
        ErrorCode::BadRequest
    );
    assert_eq!(
        h.code(
            "recover_start",
            json!({"epoch": epoch, "include": [main], "exclude": [main], "dest": scratch.join("y")})
        ),
        ErrorCode::BadRequest
    );
    assert!(!Path::new(&scratch.join("y")).exists());
    assert_eq!(
        h.code(
            "recover_start",
            json!({"epoch": epoch + 7, "include": [main], "dest": scratch.join("z")})
        ),
        ErrorCode::StaleCatalog
    );
    // Browse only (state B): no_source for every job.
    h.ok("close_source", json!({}));
    assert_eq!(
        h.code(
            "recover_start",
            json!({"epoch": epoch, "include": [main], "dest": scratch.join("b")})
        ),
        ErrorCode::NoSource
    );
    assert_eq!(h.code("carve_start", json!({})), ErrorCode::NoSource);
    assert_eq!(
        h.code("image_start", json!({"dest": scratch.join("b.img")})),
        ErrorCode::NoSource
    );
    h.ok("shutdown", json!({}));
}

// ----------------------------------------------------------------------------------------------
// Carving
// ----------------------------------------------------------------------------------------------

#[test]
fn carving_the_wiped_fixture_finds_files_whose_record_is_gone() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-carve");
    let name = "wiped";
    let m = manifest(&dir, name);
    let mut h = Harness::real();
    let session = scratch.join("wiped.hds");
    let done = scan_fixture(&mut h, &dir, name, &m, &session);
    let catalog = done["catalog"].clone();
    let epoch = u(&catalog["epoch"]);
    let main = &catalog["instances"][0];
    let root = u(&main["root"]);
    let validated = main["geometry"]["validated"] == true;
    let cluster = u(&m["old_volume"]["cluster_size"]);
    let volume = u(&m["old_volume"]["offset"]);
    // A node of the epoch before carving.
    let before = h.ok("list_children", json!({"epoch": epoch, "node": root}));
    let some_node = before["rows"][0]["id"].clone();

    let started = h.ok("carve_start", json!({"kinds": null}));
    assert_eq!(started["job"], "carve");
    assert!(u(&started["candidates"]) > 0);
    let carved = h.wait_event_for("carve_done", JOB_TIMEOUT);
    assert_eq!(carved["status"], "completed", "{carved}");
    assert!(u(&carved["found"]) > 0);
    let info = carved["catalog"].clone();
    assert_eq!(u(&info["epoch"]), epoch, "carving keeps the epoch");
    assert_eq!(h.ok("catalog_info", json!({})), info);
    // Ids of the epoch stay valid.
    h.ok("node_info", json!({"epoch": epoch, "node": some_node}));
    let carved_root = u(&info["instances"][0]["virtual"]["carved"]);

    // The targets: files whose record is gone and whose single run survived.
    struct Target {
        path: String,
        offset: u64,
        size: u64,
        sha: String,
    }
    let mut targets = Vec::new();
    for e in m["entries"].as_array().unwrap() {
        let runs = e["ondisk"]["runs"].as_array().cloned().unwrap_or_default();
        if e["type"] == "file"
            && e["after"]["record_intact"] == false
            && e["after"]["content_intact"] == true
            && runs.len() == 1
            && runs[0][0].is_u64()
            && u(&e["size"]) > 0
        {
            targets.push(Target {
                path: e["path"].as_str().unwrap().to_string(),
                offset: volume + u(&runs[0][0]) * cluster,
                size: u(&e["size"]),
                sha: e["sha256"].as_str().unwrap().to_string(),
            });
        }
    }
    // Every carved file, by its offset (the name holds the sector).
    let mut carved_nodes: HashMap<u64, Value> = HashMap::new();
    let folders = h.ok(
        "list_children",
        json!({"epoch": epoch, "node": carved_root, "limit": 1000}),
    );
    for folder in folders["rows"].as_array().unwrap() {
        assert_eq!(folder["kind"], "virtual");
        let mut rows: Vec<Value> = Vec::new();
        loop {
            let page = h.ok(
                "list_children",
                json!({"epoch": epoch, "node": folder["id"], "offset": rows.len(), "limit": 1000}),
            );
            let got = page["rows"].as_array().unwrap().clone();
            let done = got.is_empty() || rows.len() + got.len() >= u(&page["total"]) as usize;
            rows.extend(got);
            if done {
                break;
            }
        }
        for row in &rows {
            assert_eq!(row["kind"], "carved");
            let file_name = row["name"].as_str().unwrap();
            let sector: u64 = file_name[1..11].parse().unwrap();
            assert!(["good", "uncertain", "damaged"].contains(&row["rating"].as_str().unwrap()));
            if !validated {
                assert_ne!(
                    row["rating"], "good",
                    "F6: no good carved file without a validated geometry"
                );
            }
            carved_nodes.insert(sector * 512, row.clone());
        }
    }
    assert_eq!(carved_nodes.len() as u64, u(&carved["found"]));
    // I2 (a): nothing is carved inside an in-use file whose record survived.
    for e in m["entries"].as_array().unwrap() {
        if e["type"] != "file" || e["deleted"] == true || e["after"]["record_intact"] != true {
            continue;
        }
        for run in e["ondisk"]["runs"].as_array().into_iter().flatten() {
            let Some(lcn) = run[0].as_u64() else { continue };
            let (a, b) = (
                volume + lcn * cluster,
                volume + (lcn + u(&run[1])) * cluster,
            );
            for (&offset, row) in &carved_nodes {
                let end = offset + u(&row["size"]);
                assert!(
                    end <= a || offset >= b,
                    "{}: carved {} inside it",
                    e["path"],
                    row["name"]
                );
            }
        }
    }
    // The targets the scanner sniffed are carved; the exact ones come back
    // bit-identical through recovery.
    let mut exact = Vec::new();
    for t in &targets {
        if let Some(row) = carved_nodes.get(&t.offset) {
            if u(&row["size"]) == t.size {
                exact.push((t, row.clone()));
            }
        }
    }
    eprintln!(
        "{name}: {} candidates, {} carved, {} targets, {} carved exactly",
        started["candidates"],
        carved_nodes.len(),
        targets.len(),
        exact.len()
    );
    assert!(
        exact.len() >= 20,
        "only {} targets carved exactly",
        exact.len()
    );
    let ids: Vec<Value> = exact.iter().map(|(_, r)| r["id"].clone()).collect();
    let dest = scratch.join("carved-out");
    let started = h.ok(
        "recover_start",
        json!({"epoch": epoch, "include": ids, "dest": dest}),
    );
    assert_eq!(u(&started["files"]), exact.len() as u64);
    let rdone = h.wait_event_for("recover_done", JOB_TIMEOUT);
    assert_eq!(rdone["status"], "completed");
    let rows = read_report(Path::new(rdone["report_path"].as_str().unwrap()));
    let by_first: HashMap<u64, &ReportRow> = rows
        .iter()
        .filter_map(|r| Some((r.first_data_offset?, r)))
        .collect();
    for (t, _) in &exact {
        let row = by_first
            .get(&t.offset)
            .unwrap_or_else(|| panic!("{}: not recovered", t.path));
        assert!(written(&row.status), "{}: {}", t.path, row.status);
        assert!(
            row.original_path.starts_with("[Carved files]/"),
            "{}",
            row.original_path
        );
        let data = std::fs::read(Path::new(&dest).join(&row.output_path)).unwrap();
        assert_eq!(sha256_hex(&data), t.sha, "{}", t.path);
    }

    // I5: carving one kind replaces only that kind.
    let jpegs = carved_nodes
        .values()
        .filter(|r| r["ext"] == "jpg" || r["ext"] == "jpeg")
        .count() as u64;
    h.ok("carve_start", json!({"kinds": ["png"]}));
    let png = h.wait_event_for("carve_done", JOB_TIMEOUT);
    assert_eq!(png["status"], "completed");
    assert_eq!(u(&png["catalog"]["epoch"]), epoch);
    let all = h.ok(
        "search",
        json!({"epoch": epoch, "under": png["catalog"]["instances"][0]["virtual"]["carved"],
               "filter": {"exts": ["jpg", "jpeg"]}, "limit": 1000}),
    );
    assert_eq!(u(&all["total"]), jpegs, "the jpeg files stay");
    // The carved files are stored in the session.
    h.ok("session_load", json!({"path": session}));
    let reloaded = h.ok("catalog_info", json!({}));
    assert_eq!(reloaded["nodes"], png["catalog"]["nodes"]);
    assert!(reloaded["instances"][0]["virtual"]["carved"].is_u64());
    // Any scan discards them (I5), also resuming a complete session.
    h.ok("scan_start", json!({"resume": true}));
    let rescanned = h.wait_event_for("scan_done", JOB_TIMEOUT);
    assert_eq!(rescanned["status"], "completed");
    assert!(rescanned["catalog"]["instances"][0]["virtual"]["carved"].is_null());
    h.ok("shutdown", json!({}));
}

// ----------------------------------------------------------------------------------------------
// Imaging
// ----------------------------------------------------------------------------------------------

fn same_content(a: &Path, b: &Path, start: u64, len: u64) {
    use std::io::{Read, Seek, SeekFrom};
    let mut fa = std::fs::File::open(a).unwrap();
    let mut fb = std::fs::File::open(b).unwrap();
    fa.seek(SeekFrom::Start(start)).unwrap();
    assert_eq!(fb.metadata().unwrap().len(), len, "{}", b.display());
    let (mut x, mut y) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    let mut left = len;
    while left > 0 {
        let n = left.min(1 << 20) as usize;
        fa.read_exact(&mut x[..n]).unwrap();
        fb.read_exact(&mut y[..n]).unwrap();
        assert!(x[..n] == y[..n], "content differs near {}", len - left);
        left -= n as u64;
    }
}

#[test]
fn an_image_of_a_fixture_is_scanned_and_recovered_like_the_fixture() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-image");
    let name = "offset-mbr";
    let m = manifest(&dir, name);
    let image = image_path(&dir, name);
    let (start, end) = scan_range(&m);
    let mut h = Harness::real();
    // State O: the partition is imaged before anything else is read.
    h.ok("open_source", json!({"kind": "image", "path": image}));
    let copy = scratch.join("partition.img");
    let started = h.ok(
        "image_start",
        json!({"start": start, "end": end, "dest": copy}),
    );
    assert_eq!(started["job"], "image");
    assert_eq!(u(&started["bytes"]), end - start);
    let done = h.wait_event_for("image_done", JOB_TIMEOUT);
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(u(&done["bytes_written"]), end - start);
    assert_eq!(done["bad_bytes"], 0);
    assert!(done["bad_map_path"].is_null());
    assert_eq!(done["path"], copy);
    same_content(Path::new(&image), Path::new(&copy), start, end - start);
    // An existing image is not replaced.
    assert_eq!(
        h.code(
            "image_start",
            json!({"start": start, "end": end, "dest": copy})
        ),
        ErrorCode::Exists
    );
    // Resuming a complete image just ends.
    h.ok(
        "image_start",
        json!({"start": start, "end": end, "dest": copy, "resume": true}),
    );
    let again = h.wait_event_for("image_done", JOB_TIMEOUT);
    assert_eq!(again["status"], "completed");
    assert_eq!(u(&again["bytes_written"]), end - start);
    // Another range does not continue it.
    assert_eq!(
        h.code(
            "image_start",
            json!({"start": start, "end": end - 512, "dest": copy, "resume": true})
        ),
        ErrorCode::SourceMismatch
    );

    // The copy is a source like the partition: the same files come back.
    let session = scratch.join("copy.hds");
    let len = end - start;
    let done = scan(&mut h, &copy, None, (0, len), &session);
    let catalog = &done["catalog"];
    let epoch = u(&catalog["epoch"]);
    let main = u(&catalog["instances"][0]["root"]);
    let dest = scratch.join("from-copy");
    let (_, _, rows) = recover(&mut h, epoch, &[main], &dest, true);
    // The manifest's offsets are those of the disk; the copy starts at the
    // partition.
    let mut shifted = m.clone();
    for e in shifted["entries"].as_array_mut().unwrap() {
        if let Some(o) = e["ondisk"]["record_offset"].as_u64() {
            e["ondisk"]["record_offset"] = json!(o - start);
        }
    }
    check_against_manifest("offset-mbr image", &shifted, Path::new(&dest), &rows);
    h.ok("shutdown", json!({}));
}

// ----------------------------------------------------------------------------------------------
// Faults in the middle of a job: the backend with a FaultySource.
// ----------------------------------------------------------------------------------------------

fn context<P: hds_engine::protocol::EventData>(
    cancel: &Arc<AtomicBool>,
) -> (JobContext<P>, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::channel::<String>();
    (
        JobContext::new(
            Output::new(tx),
            cancel.clone(),
            Arc::new(AtomicBool::new(false)),
        ),
        rx,
    )
}

fn destination(
    path: &str,
    purpose: Purpose,
    source: &dyn BlockSource,
    protected: Vec<Protected>,
) -> Destination {
    let check = destination_check(Path::new(path), purpose, source, &protected, None);
    assert!(check.is_allowed(false, false), "{check:?}");
    Destination {
        path: path.to_string(),
        allow_same_disk: false,
        allow_unknown_disk: false,
        protected,
        check,
    }
}

#[test]
fn an_image_cancelled_in_the_middle_is_resumed_to_the_same_bytes() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-image-resume");
    let image = image_path(&dir, "basic");
    let file: Arc<dyn BlockSource> = Arc::new(ImageFile::open(Path::new(&image), None).unwrap());
    let faulty = Arc::new(FaultySource::new(file));
    let source: Arc<dyn BlockSource> = faulty.clone();
    let size = source.size();
    let backend = RealBackend::new();
    let dest_path = scratch.join("basic-copy.img");
    let request = |resume: bool| ImageRequest {
        source: source.clone(),
        range: 0..size,
        dest: destination(
            &dest_path,
            Purpose::Image,
            &*source,
            vec![Protected::session(0..size)],
        ),
        resume,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let job = backend.image_start(request(false)).unwrap();
    assert_eq!(job.result.bytes, size);
    faulty.cancel_after_reads(3, cancel.clone());
    let (ctx, _rx) = context(&cancel);
    let done = (job.run)(&ctx);
    assert_eq!(done.status, JobStatus::Cancelled, "{done:?}");
    assert!(done.bytes_written < size, "{done:?}");
    assert!(Path::new(&format!("{dest_path}.hdsimg")).is_file());
    // Without resume the unfinished image is not replaced.
    assert_eq!(
        backend.image_start(request(false)).err().unwrap().code,
        ErrorCode::Exists
    );
    // A source that is lost while resuming leaves the confirmed part.
    let cancel = Arc::new(AtomicBool::new(false));
    let job = backend.image_start(request(true)).unwrap();
    faulty.lose_after_reads(2);
    let (ctx, _rx) = context(&cancel);
    let lost = (job.run)(&ctx);
    assert_eq!(lost.status, JobStatus::Failed, "{lost:?}");
    assert_eq!(lost.error.as_ref().unwrap().code, ErrorCode::SourceLost);
    assert!(lost.bytes_written >= done.bytes_written);
    faulty.revive();
    // Resumed: the image is the source, byte for byte.
    let cancel = Arc::new(AtomicBool::new(false));
    let job = backend.image_start(request(true)).unwrap();
    let (ctx, rx) = context(&cancel);
    let resumed = (job.run)(&ctx);
    assert_eq!(resumed.status, JobStatus::Completed, "{resumed:?}");
    assert_eq!(resumed.bytes_written, size);
    assert!(rx.try_iter().any(|l| l.contains("image_progress")));
    same_content(Path::new(&image), Path::new(&dest_path), 0, size);
}

#[test]
fn recovery_and_carving_stop_cleanly_on_cancel_and_on_a_lost_source() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-faults");
    let image = image_path(&dir, "wiped");
    let file: Arc<dyn BlockSource> = Arc::new(ImageFile::open(Path::new(&image), None).unwrap());
    let faulty = Arc::new(FaultySource::new(file));
    let source: Arc<dyn BlockSource> = faulty.clone();
    let size = source.size();
    let backend = RealBackend::new();
    let never = Arc::new(AtomicBool::new(false));
    let job = backend
        .scan_start(ScanRequest {
            source: source.clone(),
            partitions: Vec::new(),
            range: 0..size,
            session: None,
            resume: false,
            collect_carve_hits: true,
        })
        .unwrap();
    let (ctx, _rx) = context(&never);
    let scanned = (job.run)(&ctx);
    assert_eq!(scanned.status, ScanStatus::Completed);
    let catalog = scanned.catalog.unwrap();
    let root = catalog.instances[0].root;
    let protected = vec![Protected::session(0..size)];
    let args = |dest: &str| RecoverStartArgs {
        epoch: 0,
        include: vec![root],
        exclude: Vec::new(),
        filter: None,
        dest: dest.to_string(),
        include_deleted: true,
        skip_damaged: false,
        allow_same_disk: false,
        allow_unknown_disk: false,
        on_exists: OnExists::Rename,
    };

    // Cancel in the middle of a recovery.
    let dest = scratch.join("cancelled");
    let job = backend
        .recover_start(RecoverRequest {
            args: args(&dest),
            dest: destination(&dest, Purpose::Recover, &*source, protected.clone()),
        })
        .unwrap();
    let total = job.result.files;
    let cancel = Arc::new(AtomicBool::new(false));
    faulty.cancel_after_reads(40, cancel.clone());
    let (ctx, _rx) = context(&cancel);
    let done = (job.run)(&ctx);
    assert_eq!(done.status, JobStatus::Cancelled, "{done:?}");
    let handled = done.ok + done.warnings + done.failed + done.skipped;
    assert!(handled < total, "{done:?}");
    assert!(
        part_files(Path::new(&dest)).is_empty(),
        "the part file is removed"
    );
    let rows = read_report(Path::new(done.report_path.as_deref().unwrap()));
    assert_eq!(
        rows.len() as u64,
        handled,
        "files not reached are not listed"
    );

    // A source lost in the middle of a recovery (S7).
    let dest = scratch.join("lost");
    let job = backend
        .recover_start(RecoverRequest {
            args: args(&dest),
            dest: destination(&dest, Purpose::Recover, &*source, protected.clone()),
        })
        .unwrap();
    faulty.lose_after_reads(40);
    let (ctx, _rx) = context(&never);
    let done = (job.run)(&ctx);
    assert_eq!(done.status, JobStatus::Failed, "{done:?}");
    assert_eq!(done.error.as_ref().unwrap().code, ErrorCode::SourceLost);
    assert!(part_files(Path::new(&dest)).is_empty());
    // H1 needs the source: recover_start says so.
    let dest = scratch.join("refused");
    let e = backend
        .recover_start(RecoverRequest {
            args: args(&dest),
            dest: destination(&dest, Purpose::Recover, &*source, protected.clone()),
        })
        .err()
        .unwrap();
    assert_eq!(e.code, ErrorCode::SourceLost);
    faulty.revive();

    // Cancelled carving keeps what it found (I5).
    let job = backend
        .carve_start(CarveRequest {
            kinds: hds_carve::FileKind::all().to_vec(),
        })
        .unwrap();
    let candidates = job.result.candidates;
    let cancel = Arc::new(AtomicBool::new(false));
    faulty.cancel_after_reads(30, cancel.clone());
    let (ctx, _rx) = context(&cancel);
    let done = (job.run)(&ctx);
    assert_eq!(done.status, JobStatus::Cancelled, "{done:?}");
    assert!(done.found > 0, "{done:?}");
    let info = done.catalog.unwrap();
    let kept = backend.catalog_info().unwrap();
    assert_eq!(info.nodes, kept.nodes);
    assert!(kept.instances[0].virtual_folders.carved.is_some());
    // A complete run finds at least as much.
    let job = backend
        .carve_start(CarveRequest {
            kinds: hds_carve::FileKind::all().to_vec(),
        })
        .unwrap();
    assert_eq!(job.result.candidates, candidates);
    let (ctx, _rx) = context(&never);
    let full = (job.run)(&ctx);
    assert_eq!(full.status, JobStatus::Completed);
    assert!(full.found >= done.found);
}

// ----------------------------------------------------------------------------------------------
// Shutdown while a job runs
// ----------------------------------------------------------------------------------------------

#[test]
fn shutdown_cancels_a_running_recovery_without_leaving_part_files() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-shutdown");
    let m = manifest(&dir, "basic");
    let mut h = Harness::real();
    let session = scratch.join("basic.hds");
    let done = scan_fixture(&mut h, &dir, "basic", &m, &session);
    let epoch = u(&done["catalog"]["epoch"]);
    let main = u(&done["catalog"]["instances"][0]["root"]);
    let dest = scratch.join("out");
    h.ok(
        "recover_start",
        json!({"epoch": epoch, "include": [main], "dest": dest, "include_deleted": true}),
    );
    h.ok("shutdown", json!({}));
    let done = h.wait_event_for("recover_done", JOB_TIMEOUT);
    assert!(
        done["status"] == "cancelled" || done["status"] == "completed",
        "{done}"
    );
    assert!(part_files(Path::new(&dest)).is_empty());
}

// ----------------------------------------------------------------------------------------------
// The command-line interface
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
fn cli_recover_carve_and_image() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-cli-jobs");
    let m = manifest(&dir, "basic");
    let image = image_path(&dir, "basic");
    let session = scratch.join("basic.hds");
    let (code, _, stderr) = cli(&["scan", &image, "--session", &session]);
    assert_eq!(code, 0, "{stderr}");

    let dest = scratch.join("out");
    let (code, lines, stderr) = cli(&[
        "recover",
        "--session",
        &session,
        "--dest",
        &dest,
        "--all",
        "--include-deleted",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let done = lines.last().unwrap();
    assert_eq!(done["event"], "recover_done");
    assert_eq!(done["data"]["status"], "completed");
    let rows = read_report(Path::new(done["data"]["report_path"].as_str().unwrap()));
    check_against_manifest("basic (cli)", &m, Path::new(&dest), &rows);
    // A destination that is the source: exit code 1, refused.
    let (code, lines, _) = cli(&["recover", "--session", &session, "--dest", &image, "--all"]);
    assert_eq!(code, 1);
    assert!(lines
        .iter()
        .any(|l| l["error"]["code"] == "destination_refused"));

    let (code, lines, stderr) = cli(&["carve", "--session", &session, "--kinds", "jpeg,png"]);
    assert_eq!(code, 0, "{stderr}");
    let done = lines.last().unwrap();
    assert_eq!(done["event"], "carve_done");
    assert_eq!(done["data"]["status"], "completed");
    assert!(done["data"]["catalog"].is_object());

    let copy = scratch.join("head.img");
    let (code, lines, stderr) = cli(&["image", &image, "--dest", &copy, "--end", "4194304"]);
    assert_eq!(code, 0, "{stderr}");
    let done = lines.last().unwrap();
    assert_eq!(done["event"], "image_done");
    assert_eq!(done["data"]["bytes_written"], 4194304);
    same_content(Path::new(&image), Path::new(&copy), 0, 4194304);
    // Again: exists, exit code 1; with --resume it just ends.
    let (code, _, _) = cli(&["image", &image, "--dest", &copy, "--end", "4194304"]);
    assert_eq!(code, 1);
    let (code, _, stderr) = cli(&[
        "image", &image, "--dest", &copy, "--end", "4194304", "--resume",
    ]);
    assert_eq!(code, 0, "{stderr}");
}

#[test]
fn the_backend_runs_the_guard_again_for_a_disk_source() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("real-disk-guard");
    let data = std::fs::read(image_path(&dir, "basic")).unwrap();
    let size = data.len() as u64;
    // A source that says it is physical disk 7: on this system no
    // destination can be placed on a disk, so every one is `unknown_disk`
    // (on Windows the same check resolves the volume's disk extents).
    let source: Arc<dyn BlockSource> = Arc::new(
        hds_io::testing::MemSource::new(data)
            .at_location(hds_io::SourceLocation::Disk { number: 7 }),
    );
    let backend = RealBackend::new();
    let never = Arc::new(AtomicBool::new(false));
    let job = backend
        .scan_start(ScanRequest {
            source: source.clone(),
            partitions: Vec::new(),
            range: 0..size,
            session: None,
            resume: false,
            collect_carve_hits: false,
        })
        .unwrap();
    let (ctx, _rx) = context(&never);
    let scanned = (job.run)(&ctx);
    let root = scanned.catalog.unwrap().instances[0].root;
    let dest = scratch.join("out");
    let protected = vec![Protected::session(0..size)];
    let check = destination_check(
        Path::new(&dest),
        Purpose::Recover,
        &*source,
        &protected,
        None,
    );
    assert_eq!(check.verdict, hds_io::Verdict::UnknownDisk);
    let request = |consent: bool| RecoverRequest {
        args: RecoverStartArgs {
            epoch: 0,
            include: vec![root],
            exclude: Vec::new(),
            filter: None,
            dest: dest.clone(),
            include_deleted: false,
            skip_damaged: false,
            allow_same_disk: false,
            allow_unknown_disk: consent,
            on_exists: OnExists::Rename,
        },
        dest: Destination {
            path: dest.clone(),
            allow_same_disk: false,
            allow_unknown_disk: consent,
            protected: protected.clone(),
            check: check.clone(),
        },
    };
    // Without the consent the backend refuses on its own, and creates nothing.
    let e = backend.recover_start(request(false)).err().unwrap();
    assert_eq!(e.code, ErrorCode::DestinationRefused);
    assert_eq!(e.data.as_ref().unwrap()["verdict"], "unknown_disk");
    assert!(!Path::new(&dest).exists());
    // The image job too.
    let image = scratch.join("copy.img");
    let e = backend
        .image_start(ImageRequest {
            source: source.clone(),
            range: 0..size,
            dest: Destination {
                path: image.clone(),
                allow_same_disk: false,
                allow_unknown_disk: false,
                protected: protected.clone(),
                check: check.clone(),
            },
            resume: false,
        })
        .err()
        .unwrap();
    assert_eq!(e.code, ErrorCode::DestinationRefused);
    assert!(!Path::new(&image).exists());
    // With it, the files come back.
    let job = backend.recover_start(request(true)).unwrap();
    let (ctx, _rx) = context(&never);
    let done = (job.run)(&ctx);
    assert_eq!(done.status, JobStatus::Completed);
    assert!(done.ok > 0);
}
