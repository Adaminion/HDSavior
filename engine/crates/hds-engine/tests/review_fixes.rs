//! Regression tests for the findings of the protocol, safety and robustness
//! reviews of the engine's integration with hds-core and hds-recover. Each
//! test names the finding it pins down and fails without its fix.
//!
//! The tests on the real-driver fixtures (`HDS_FIXTURES`) print SKIPPED and
//! pass without it.

mod common;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use common::{Harness, TempDir};
use hds_engine::backend::{Backend, Destination, JobContext, ScanRequest, TaskContext};
use hds_engine::output::Output;
use hds_engine::protocol::{ErrorCode, GeometrySetArgs, ReadFileArgs, SearchArgs, TaskName};
use hds_engine::real_backend::{RealBackend, READ_FILE_DEADLINE};
use hds_io::testing::{FaultySource, MemSource};
use hds_io::{
    destination_check, BlockSource, ImageFile, Protected, Purpose, SourceInfo, SourceKind,
    SourceLocation,
};
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

/// Opens fixture `name` and scans its scan range into `session`; returns
/// `scan_done`.
fn scan_fixture(h: &mut Harness, dir: &Path, name: &str, session: Option<&str>) -> Value {
    let m = manifest(dir, name);
    let start = u(&m["scan_range"]["offset"]);
    let end = start + u(&m["scan_range"]["length"]);
    h.ok(
        "open_source",
        json!({"kind": "image", "path": image_path(dir, name),
               "sector_size": m["image"]["sector_size"]}),
    );
    h.ok(
        "scan_start",
        json!({"start": start, "end": end, "session_path": session}),
    );
    let done = h.wait_event_for("scan_done", JOB_TIMEOUT);
    assert_eq!(done["status"], "completed", "{done}");
    done
}

fn context<P: hds_engine::protocol::EventData>() -> (JobContext<P>, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::channel::<String>();
    (
        JobContext::new(
            Output::new(tx),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ),
        rx,
    )
}

fn task(name: TaskName) -> TaskContext {
    let (tx, _rx) = mpsc::channel::<String>();
    TaskContext::new(Output::new(tx), name)
}

/// A fixture read into memory that says it is a physical disk with a serial
/// number no connected disk reports (it lives nowhere: `Detached`).
struct DiskLike(MemSource);

impl BlockSource for DiskLike {
    fn size(&self) -> u64 {
        self.0.size()
    }
    fn sector_size(&self) -> u32 {
        self.0.sector_size()
    }
    fn physical_sector_size(&self) -> u32 {
        self.0.physical_sector_size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.0.read_at(offset, buf)
    }
    fn info(&self) -> SourceInfo {
        SourceInfo {
            kind: SourceKind::Disk,
            path: r"\\.\PhysicalDrive97".to_string(),
            size: self.0.size(),
            sector_size: self.0.sector_size(),
            physical_sector_size: self.0.physical_sector_size(),
            model: Some("HDSavior test disk".to_string()),
            serial: Some("HDS-NOT-CONNECTED-0001".to_string()),
            disk_number: Some(97),
        }
    }
    fn is_alive(&self) -> bool {
        true
    }
    fn location(&self) -> SourceLocation {
        SourceLocation::Detached
    }
}

// ----------------------------------------------------------------------------------------------
// protocol M1 / safety F3: in state B the guard runs against the recorded source.
// ----------------------------------------------------------------------------------------------

#[test]
fn state_b_session_save_is_guarded_against_the_recorded_source() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("review-state-b");
    let mut h = Harness::real();
    let session = scratch.join("s.hds");
    scan_fixture(&mut h, &dir, "basic", Some(&session));
    h.ok("close_source", json!({}));
    // Onto the source image: an overlap, which no consent unlocks (S2).
    let image = image_path(&dir, "basic");
    let e = h.err(
        "session_save",
        json!({"path": image, "allow_same_disk": true, "allow_unknown_disk": true}),
    );
    assert_eq!(e.code, ErrorCode::DestinationRefused, "{e}");
    let data = e.data.expect("the check as data");
    assert_eq!(data["verdict"], "overlaps_source", "{data}");
    assert_eq!(data["reason"], "source_image", "{data}");
    // Anywhere else no consent is needed: the source is known not to be there.
    let saved = h.ok(
        "session_save",
        json!({"path": scratch.join("elsewhere.hds")}),
    );
    assert!(u(&saved["bytes"]) > 0);
    h.ok("shutdown", json!({}));
}

// ----------------------------------------------------------------------------------------------
// safety F4: in state B a change of the protected ranges re-checks the session file.
// ----------------------------------------------------------------------------------------------

#[test]
fn state_b_changes_run_the_guard_on_the_session_file_before_writing() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("review-state-b-file");
    let data = std::fs::read(image_path(&dir, "basic")).unwrap();
    let size = data.len() as u64;
    let source: Arc<dyn BlockSource> = Arc::new(DiskLike(MemSource::new(data)));
    let backend = RealBackend::new();
    let session = scratch.join("s.hds");
    let protected = vec![Protected::session(0..size)];
    let check = destination_check(
        Path::new(&session),
        Purpose::Session,
        &*source,
        &protected,
        None,
    );
    assert!(check.is_allowed(false, false), "{check:?}");
    let job = backend
        .scan_start(ScanRequest {
            source: source.clone(),
            partitions: Vec::new(),
            range: 0..size,
            session: Some(Destination {
                path: session.clone(),
                allow_same_disk: false,
                allow_unknown_disk: false,
                protected,
                check,
            }),
            resume: false,
            collect_carve_hits: false,
        })
        .unwrap();
    let (ctx, _rx) = context();
    let done = (job.run)(&ctx);
    let catalog = done.catalog.expect("a catalog");
    let g = catalog.instances[0].geometry.clone();
    // close_source: state B. The disk with that serial is not connected, so
    // where it is cannot be told; the file was placed without the
    // `unknown_disk` consent and must not be written to any more.
    backend.detach_source();
    let before = std::fs::metadata(&session).unwrap().len();
    let info = backend
        .geometry_set(
            &GeometrySetArgs {
                epoch: 0,
                instance: 0,
                cluster_size: u64::from(g.cluster_size),
                volume_offset: g.volume_offset,
            },
            &task(TaskName::GeometrySet),
        )
        .unwrap();
    assert_eq!(
        std::fs::metadata(&session).unwrap().len(),
        before,
        "the session file was written without a guard run"
    );
    assert!(
        info.warnings
            .iter()
            .any(|w| w.code == hds_engine::real_backend::WARN_SESSION_FILE_AT_RISK),
        "{:?}",
        info.warnings
    );
}

// ----------------------------------------------------------------------------------------------
// protocol M2: geometry_set to the geometry in effect keeps the carved files.
// ----------------------------------------------------------------------------------------------

#[test]
fn geometry_set_to_the_current_geometry_keeps_the_carved_files() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("review-carved-geometry");
    let mut h = Harness::real();
    let session = scratch.join("w.hds");
    let done = scan_fixture(&mut h, &dir, "wiped", Some(&session));
    let epoch = u(&done["catalog"]["epoch"]);
    h.ok("carve_start", json!({"kinds": null}));
    let carved = h.wait_event_for("carve_done", JOB_TIMEOUT);
    assert_eq!(carved["status"], "completed", "{carved}");
    let info = carved["catalog"].clone();
    let count = u(&info["carved"]);
    assert!(count > 0, "{info}");
    let folder = u(&info["instances"][0]["virtual"]["carved"]);
    let g = info["instances"][0]["geometry"].clone();
    let same = h.ok(
        "geometry_set",
        json!({"epoch": epoch, "instance": 0, "cluster_size": g["cluster_size"],
               "volume_offset": g["volume_offset"]}),
    );
    assert_eq!(u(&same["carved"]), count, "{same}");
    assert_eq!(u(&same["instances"][0]["virtual"]["carved"]), folder);
    h.ok("list_children", json!({"epoch": epoch, "node": folder}));
    // Another volume offset changes main: the carved files go.
    let moved = h.ok(
        "geometry_set",
        json!({"epoch": epoch, "instance": 0, "cluster_size": g["cluster_size"],
               "volume_offset": u(&g["volume_offset"]) + 4096}),
    );
    assert_eq!(u(&moved["carved"]), 0, "{moved}");
    assert_eq!(
        h.code("list_children", json!({"epoch": epoch, "node": folder})),
        ErrorCode::NotFound
    );
    h.ok("shutdown", json!({}));
}

// ----------------------------------------------------------------------------------------------
// protocol M3: instance ids belong to an epoch; instance_set and geometry_set check it.
// ----------------------------------------------------------------------------------------------

#[test]
fn a_repeated_instance_set_answers_stale_catalog() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("review-instance-epoch");
    let mut h = Harness::real();
    let done = scan_fixture(&mut h, &dir, "generations", Some(&scratch.join("g.hds")));
    let catalog = done["catalog"].clone();
    let epoch = u(&catalog["epoch"]);
    let other = catalog["instances"]
        .as_array()
        .unwrap()
        .iter()
        .position(|i| i["kind"] == "older" || i["kind"] == "other")
        .expect("generations holds an older file system");
    let request = json!({"epoch": epoch, "instance": other, "kind": "main"});
    let changed = h.ok("instance_set", request.clone());
    let epoch2 = u(&changed["epoch"]);
    assert!(epoch2 > epoch);
    // The same request again (a double click, a retry) must not toggle main back.
    assert_eq!(h.code("instance_set", request), ErrorCode::StaleCatalog);
    assert_eq!(h.epoch(), epoch2, "the refused request changed nothing");
    let g = &changed["instances"][0]["geometry"];
    assert_eq!(
        h.code(
            "geometry_set",
            json!({"epoch": epoch, "instance": 0, "cluster_size": g["cluster_size"],
                   "volume_offset": g["volume_offset"]})
        ),
        ErrorCode::StaleCatalog
    );
    h.ok("shutdown", json!({}));
}

// ----------------------------------------------------------------------------------------------
// protocol M4 and m7: the command-line interface.
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

/// A session stopped by `shutdown` right after `scan_start`: too young to
/// be recognised by its records (B4, fewer than 8).
fn young_session(image: &str, session: &str) {
    let mut h = Harness::real();
    h.ok("open_source", json!({"kind": "image", "path": image}));
    h.ok("scan_start", json!({"session_path": session}));
    h.ok("shutdown", json!({}));
    h.engine.finish();
    assert!(Path::new(session).is_file());
}

#[test]
fn cli_resumes_a_young_session_of_the_source_named_on_the_command_line() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("review-cli-young");
    let image = image_path(&dir, "basic");
    let session = scratch.join("early.hds");
    young_session(&image, &session);
    let (code, lines, stderr) = cli(&["scan", &image, "--session", &session, "--resume"]);
    assert_eq!(code, 0, "{stderr}");
    let done = lines.last().unwrap();
    assert_eq!(done["event"], "scan_done", "{done}");
    assert_eq!(done["data"]["status"], "completed");
    // Another image is still refused (its records differ, or it has none
    // the session could compare: it is never forced unless the session
    // itself is the young one).
    let other = image_path(&dir, "fragmented");
    let (code, _, _) = cli(&["scan", &other, "--session", &session, "--resume"]);
    assert_eq!(code, 1);
}

#[test]
fn cli_scan_does_not_replace_a_session_without_force() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("review-cli-replace");
    let image = image_path(&dir, "basic");
    let session = scratch.join("s.hds");
    let (code, _, stderr) = cli(&["scan", &image, "--session", &session]);
    assert_eq!(code, 0, "{stderr}");
    let before = std::fs::read(&session).unwrap();
    let (code, _, stderr) = cli(&["scan", &image, "--session", &session]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("--force"), "{stderr}");
    assert_eq!(
        std::fs::read(&session).unwrap(),
        before,
        "the session is kept"
    );
    let (code, _, stderr) = cli(&["scan", &image, "--session", &session, "--force"]);
    assert_eq!(code, 0, "{stderr}");
}

#[test]
fn cli_carve_saves_the_carved_files_into_the_session() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("review-cli-carve");
    let image = image_path(&dir, "wiped");
    let session = scratch.join("w.hds");
    let (code, _, stderr) = cli(&["scan", &image, "--session", &session]);
    assert_eq!(code, 0, "{stderr}");
    // The loaded file is not always continued in place (a location that
    // needs consents is not taken up again by a load), so the carve run
    // saves the session back to --session itself.
    let copy = session.clone();
    let (code, lines, stderr) = cli(&["carve", "--session", &copy]);
    assert_eq!(code, 0, "{stderr}");
    let found = lines
        .iter()
        .find(|l| l["event"] == "carve_done")
        .expect("carve_done")["data"]["catalog"]["carved"]
        .clone();
    assert!(u(&found) > 0);
    assert_eq!(lines.last().unwrap()["event"], "carve_done");
    let (code, lines, stderr) = cli(&["info", "--session", &copy]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(lines.last().unwrap()["catalog"]["carved"], found);
}

#[test]
fn cli_info_fails_when_the_named_source_is_not_the_sessions() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("review-cli-info");
    let session = scratch.join("s.hds");
    let (code, _, stderr) = cli(&["scan", &image_path(&dir, "basic"), "--session", &session]);
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = cli(&[
        "info",
        "--session",
        &session,
        "--source",
        &image_path(&dir, "fragmented"),
    ]);
    assert_eq!(code, 1, "{stderr}");
    let (code, _, stderr) = cli(&[
        "info",
        "--session",
        &session,
        "--source",
        &image_path(&dir, "basic"),
    ]);
    assert_eq!(code, 0, "{stderr}");
}

// ----------------------------------------------------------------------------------------------
// robustness 1: read_file on a slow, failing disk.
// ----------------------------------------------------------------------------------------------

/// Scans fixture `name` through a `FaultySource` with the backend directly.
fn faulty_backend(dir: &Path, name: &str) -> (RealBackend, Arc<FaultySource>) {
    let file: Arc<dyn BlockSource> =
        Arc::new(ImageFile::open(Path::new(&image_path(dir, name)), None).unwrap());
    let faulty = Arc::new(FaultySource::new(file));
    let source: Arc<dyn BlockSource> = faulty.clone();
    let size = source.size();
    let backend = RealBackend::new();
    let job = backend
        .scan_start(ScanRequest {
            source,
            partitions: Vec::new(),
            range: 0..size,
            session: None,
            resume: false,
            collect_carve_hits: false,
        })
        .unwrap();
    let (ctx, _rx) = context();
    assert!((job.run)(&ctx).catalog.is_some());
    (backend, faulty)
}

#[test]
fn read_file_on_a_slow_bad_disk_neither_holds_the_lock_nor_reads_for_long() {
    let Some(dir) = fixtures_dir() else { return };
    let (backend, faulty) = faulty_backend(&dir, "fragmented");
    let search: SearchArgs = serde_json::from_value(
        json!({"epoch": 0, "sort": "size", "desc": true, "limit": 1, "filter": null}),
    )
    .unwrap();
    let node = backend.search(&search).unwrap().rows[0].row.id;
    // Every read fails as a bad sector and takes 5 ms: the content reader
    // retries per sector, thousands of reads for this file.
    faulty.bad_sectors(0..u64::MAX / 2);
    faulty.set_delay(Duration::from_millis(5));
    let reader = backend.clone();
    let started = Instant::now();
    let read = std::thread::spawn(move || {
        let args: ReadFileArgs =
            serde_json::from_value(json!({"epoch": 0, "node": node, "length": 8388608})).unwrap();
        let result = reader.read_file(&args);
        (started.elapsed(), result.map(|f| f.data.len()))
    });
    std::thread::sleep(Duration::from_millis(300));
    let asked = Instant::now();
    backend.catalog_info().unwrap();
    let waited = asked.elapsed();
    let (took, result) = read.join().unwrap();
    assert!(
        waited < Duration::from_secs(1),
        "catalog_info waited {waited:?} for the read's lock"
    );
    assert!(
        took < READ_FILE_DEADLINE + Duration::from_secs(3),
        "read_file read for {took:?}"
    );
    let e = result.expect_err("the read was stopped");
    assert_eq!(e.code, ErrorCode::IoError, "{e}");
}

// ----------------------------------------------------------------------------------------------
// robustness 2: a session file that cannot be written is not reported as saved.
// ----------------------------------------------------------------------------------------------

/// `hdsavior-engine serve` with a file-size limit of about 100 KiB (the
/// session file of `basic` is several hundred): every write beyond it fails
/// like a full disk.
#[cfg(unix)]
struct LimitedEngine {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    lines: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
    id: u64,
}

#[cfg(unix)]
impl LimitedEngine {
    fn start() -> LimitedEngine {
        use std::io::BufRead;
        let bin = env!("CARGO_BIN_EXE_hdsavior-engine");
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("trap '' XFSZ; ulimit -f 200 && exec '{bin}' serve"))
            .env_remove("HDS_ENGINE_TEST_BACKEND")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start the engine");
        let stdin = child.stdin.take().unwrap();
        let lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        LimitedEngine {
            child,
            stdin,
            lines,
            id: 0,
        }
    }

    fn send(&mut self, cmd: &str, args: Value) -> Value {
        use std::io::Write;
        self.id += 1;
        let line = json!({"id": self.id, "cmd": cmd, "args": args}).to_string();
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
        loop {
            let v: Value = serde_json::from_str(&self.lines.next().unwrap().unwrap()).unwrap();
            if v["id"] == self.id {
                assert_eq!(v["ok"], true, "{cmd}: {v}");
                return v["result"].clone();
            }
        }
    }

    fn event(&mut self, name: &str) -> Value {
        loop {
            let v: Value = serde_json::from_str(&self.lines.next().unwrap().unwrap()).unwrap();
            if v["event"] == name {
                return v["data"].clone();
            }
        }
    }
}

#[cfg(unix)]
impl Drop for LimitedEngine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
#[test]
fn a_session_file_that_cannot_be_written_is_not_reported_as_saved() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("review-session-full");
    let session = scratch.join("full.hds");
    let mut engine = LimitedEngine::start();
    engine.send(
        "open_source",
        json!({"kind": "image", "path": image_path(&dir, "basic")}),
    );
    engine.send("scan_start", json!({"session_path": session}));
    let done = engine.event("scan_done");
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["session_path"], Value::Null, "{done}");
    let warnings = done["catalog"]["warnings"].as_array().unwrap().clone();
    assert!(
        warnings
            .iter()
            .any(|w| w["code"] == "session_file_not_saved"),
        "{warnings:?}"
    );
    drop(engine);
    // The fragment loads, and says it is one.
    let mut h = Harness::real();
    let loaded = h.ok("session_load", json!({"path": session}));
    let warnings = loaded["catalog"]["warnings"].as_array().unwrap().clone();
    assert!(
        warnings.iter().any(|w| w["code"] == "session_incomplete"),
        "{warnings:?}"
    );
    h.ok("shutdown", json!({}));
}

// ----------------------------------------------------------------------------------------------
// robustness 4: a shutdown during the analysis of a scan stops the catalog build.
// ----------------------------------------------------------------------------------------------

/// Sets `flag` as soon as a `scan_progress` of phase `analyzing` is written.
struct ShutdownOnAnalyzing(Arc<AtomicBool>);

impl hds_engine::output::LineSink for ShutdownOnAnalyzing {
    fn write_line(&mut self, line: &str) -> io::Result<()> {
        if line.contains("\"analyzing\"") {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }
}

#[test]
fn shutdown_during_the_analysis_does_not_wait_for_the_catalog() {
    let Some(dir) = fixtures_dir() else { return };
    let source: Arc<dyn BlockSource> =
        Arc::new(ImageFile::open(Path::new(&image_path(&dir, "basic")), None).unwrap());
    let size = source.size();
    let backend = RealBackend::new();
    let job = backend
        .scan_start(ScanRequest {
            source,
            partitions: Vec::new(),
            range: 0..size,
            session: None,
            resume: false,
            collect_carve_hits: false,
        })
        .unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let ctx = JobContext::new(
        Output::new(ShutdownOnAnalyzing(shutdown.clone())),
        Arc::new(AtomicBool::new(false)),
        shutdown,
    );
    let done = (job.run)(&ctx);
    assert!(done.catalog.is_none());
    // The catalog was not built behind the scenes either.
    assert_eq!(
        backend.catalog_info().unwrap_err().code,
        ErrorCode::NoCatalog
    );
}

// ----------------------------------------------------------------------------------------------
// robustness 6: image_start on a source that is already lost.
// ----------------------------------------------------------------------------------------------

#[test]
fn image_start_on_a_lost_source_answers_source_lost_and_creates_nothing() {
    let scratch = TempDir::new("review-image-lost");
    let image = common::zero_image(&scratch, "src.img", 1 << 20);
    let mut h = Harness::real();
    h.ok("open_source", json!({"kind": "image", "path": image}));
    // The medium goes away (the image is cut short under the engine).
    std::fs::OpenOptions::new()
        .write(true)
        .open(&image)
        .unwrap()
        .set_len(0)
        .unwrap();
    let dest = scratch.join("copy.img");
    let e = h.err("image_start", json!({"dest": dest}));
    assert_eq!(e.code, ErrorCode::SourceLost, "{e}");
    assert!(!Path::new(&dest).exists(), "no image file is left behind");
    assert!(
        std::fs::read_dir(scratch.path()).unwrap().count() == 1,
        "only the source remains"
    );
    h.ok("shutdown", json!({}));
}

// ----------------------------------------------------------------------------------------------
// safety F5: a recovery with on_exists=overwrite does not delete the live session file.
// ----------------------------------------------------------------------------------------------

#[test]
fn overwrite_does_not_delete_the_session_file_being_written() {
    let Some(dir) = fixtures_dir() else { return };
    let scratch = TempDir::new("review-overwrite-session");
    let out = scratch.path().join("out");
    std::fs::create_dir_all(out.join("work")).unwrap();
    // A path the recovery of `basic` produces.
    let session = out.join("work").join("about.txt");
    let session = session.to_string_lossy().into_owned();
    let mut h = Harness::real();
    let done = scan_fixture(&mut h, &dir, "basic", Some(&session));
    let epoch = u(&done["catalog"]["epoch"]);
    let root = u(&done["catalog"]["instances"][0]["root"]);
    let magic = |path: &str| std::fs::read(path).unwrap()[..4].to_vec();
    assert_eq!(magic(&session), b"HDSC");
    h.ok(
        "recover_start",
        json!({"epoch": epoch, "include": [root], "dest": out.to_string_lossy(),
               "on_exists": "overwrite"}),
    );
    let recovered = h.wait_event_for("recover_done", JOB_TIMEOUT);
    assert_eq!(recovered["status"], "completed", "{recovered}");
    assert!(u(&recovered["failed"]) >= 1, "{recovered}");
    assert_eq!(magic(&session), b"HDSC", "the session file was replaced");
    h.ok("shutdown", json!({}));
    h.engine.finish();
    // It is still a session (the writer went on writing to it, not to an
    // unlinked inode).
    let mut h = Harness::real();
    h.ok("session_load", json!({"path": session}));
    h.ok("shutdown", json!({}));
}
