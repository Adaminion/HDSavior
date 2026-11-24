//! Scale measurement of the real engine through the protocol (opt-in).
//!
//! Drives `hdsavior-engine serve` as a child process over JSON lines on a
//! large image (the million-record volume of
//! `hds_testkit::scenarios::scale::million_records`) and prints, for every
//! step, the wall time, the peak resident memory of the engine process and
//! how long a `catalog_info` sent while the step runs waits for its answer
//! (whether the request thread, or the backend lock, is blocked). A second
//! part runs the same engine in this process with a counting allocator to
//! measure the heap a recovery plan holds and `files_in_extents`, which the
//! protocol reaches only for a destination on a physical source disk.
//!
//! Nothing here is part of the normal test run: the test is ignored and,
//! even with `--ignored`, does nothing unless `HDS_SCALE_IMAGE` is set.
//!
//! ```text
//! # once: the million-record image (5 GiB sparse, about 1.2 GiB written)
//! #   hds_testkit::scenarios::scale::million_records(path, 7)
//! cd engine
//! HDS_SCALE_IMAGE=/path/scale-million.img \
//!   cargo test --release -p hds-engine --test scale -- --ignored --nocapture
//! ```
//!
//! Build in release (`--release`): the child is the `hdsavior-engine`
//! binary of the same profile. Variables:
//!
//! * `HDS_SCALE_IMAGE` - the image to open (required).
//! * `HDS_SCALE_WORK` - scratch directory for the session files and the
//!   recovered files (default: a new directory below the system temporary
//!   directory). Everything the test creates there is removed at the end
//!   unless `HDS_SCALE_KEEP=1`.
//! * `HDS_SCALE_RECOVER_FILES` - the whole-root recovery is cancelled once
//!   this many files are done (default 20000); the 10k-file folder always
//!   runs to its end.
//! * `HDS_ENGINE_BIN` - another engine binary to drive.
//! * `HDS_SCALE_REPORT` - also write the step table as JSON lines to this
//!   file.
//!
//! Peak memory is the engine's `VmHWM`, reset before every step by writing
//! `5` to `/proc/<pid>/clear_refs`, and the largest `VmRSS` sampled every
//! 5 ms (Linux only; elsewhere the columns read 0). Probe latency: a
//! `catalog_info` is sent right after the step's request and then every
//! 250 ms while the step runs; the table shows the largest wait. For a
//! plain request the first probe necessarily waits for the request itself
//! (requests are answered one at a time, PROTOCOL section 1); for a job it
//! shows whether queries are answered while the job runs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hds_engine::backend::build_backend;
use hds_engine::protocol::OutputLine;
use hds_engine::server::{response_result, InProcess};
use serde_json::{json, Value};

// ------------------------------------------------------------------------------------------
// Counting allocator (in-process part)
// ------------------------------------------------------------------------------------------

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

impl Counting {
    fn added(size: usize) {
        let now = LIVE.fetch_add(size, Ordering::Relaxed) + size;
        PEAK.fetch_max(now, Ordering::Relaxed);
    }
}

// SAFETY: every call is forwarded to the system allocator unchanged; the
// counters only observe sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract is passed on.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            Counting::added(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract is passed on.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            Counting::added(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is passed on.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's contract is passed on.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            Counting::added(new_size);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn heap_live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

fn heap_peak_reset() -> usize {
    let live = heap_live();
    PEAK.store(live, Ordering::Relaxed);
    live
}

fn heap_peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

// ------------------------------------------------------------------------------------------
// Memory of the child
// ------------------------------------------------------------------------------------------

fn status_kib(pid: u32, key: &str) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        })
        .unwrap_or(0)
}

/// Samples `VmRSS` of `pid` every 5 ms; `max` holds the largest value since
/// the last reset.
struct RssSampler {
    max: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pid: u32,
}

impl RssSampler {
    fn start(pid: u32) -> RssSampler {
        let max = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (m, s) = (max.clone(), stop.clone());
        let thread = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                m.fetch_max(status_kib(pid, "VmRSS:"), Ordering::Relaxed);
                thread::sleep(Duration::from_millis(5));
            }
        });
        RssSampler {
            max,
            stop,
            thread: Some(thread),
            pid,
        }
    }

    /// Starts a new step: resets `VmHWM` (Linux 4.0+) and the sampled max.
    fn reset(&self) {
        let _ = std::fs::write(format!("/proc/{}/clear_refs", self.pid), "5");
        self.max
            .store(status_kib(self.pid, "VmRSS:"), Ordering::Relaxed);
    }

    /// The step's peak in KiB.
    fn peak(&self) -> u64 {
        self.max
            .load(Ordering::Relaxed)
            .max(status_kib(self.pid, "VmHWM:"))
    }

    fn now(&self) -> u64 {
        status_kib(self.pid, "VmRSS:")
    }
}

impl Drop for RssSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ------------------------------------------------------------------------------------------
// The child engine
// ------------------------------------------------------------------------------------------

struct Engine {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<(Instant, String)>,
    next_id: i64,
    rss: RssSampler,
    /// A second request sent like the `catalog_info` probe during the next
    /// step only (for example a search, to see queries during a job).
    extra_probe: Option<(String, Value)>,
}

/// What ends a step.
enum Until<'a> {
    /// The response to the request.
    Response,
    /// The job's `*_done` event; `cancel` is asked with every progress
    /// event and sends `job_cancel` the first time it answers `true`.
    Done {
        event: &'static str,
        cancel: Option<&'a mut dyn FnMut(&Value) -> bool>,
    },
}

/// One measured step.
#[derive(Debug, Default)]
struct Step {
    name: String,
    /// Request to response.
    response: Duration,
    /// Request to the end of the step (`*_done` for jobs).
    wall: Duration,
    peak_kib: u64,
    after_kib: u64,
    probes: Vec<Duration>,
    /// Latencies of the `catalog_info` probes sent after the response (for
    /// a job: while it runs).
    job_probes: Vec<Duration>,
    /// Latencies of the step's extra probe ([`Engine::extra_probe`]).
    extra: Vec<Duration>,
    note: String,
}

impl Step {
    fn max_probe(&self) -> Duration {
        self.probes.iter().copied().max().unwrap_or_default()
    }

    fn json(&self) -> Value {
        json!({
            "step": self.name,
            "response_ms": ms(self.response),
            "wall_ms": ms(self.wall),
            "peak_rss_mib": self.peak_kib / 1024,
            "rss_after_mib": self.after_kib / 1024,
            "probes": self.probes.len(),
            "probe_max_ms": ms(self.max_probe()),
            "job_probe_max_ms": ms(self.job_probes.iter().copied().max().unwrap_or_default()),
            "extra_probes": self.extra.len(),
            "extra_probe_max_ms": ms(self.extra.iter().copied().max().unwrap_or_default()),
            "note": self.note,
        })
    }
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 10_000.0).round() / 10.0
}

const PROBE_EVERY: Duration = Duration::from_millis(250);

impl Engine {
    fn start(bin: &Path) -> Engine {
        let mut child = Command::new(bin)
            .arg("serve")
            .env_remove("HDS_ENGINE_TEST_BACKEND")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the engine");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send((Instant::now(), line)).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take().expect("stdin");
        let rss = RssSampler::start(child.id());
        Engine {
            child,
            stdin,
            lines,
            next_id: 0,
            rss,
            extra_probe: None,
        }
    }

    fn send(&mut self, cmd: &str, args: &Value) -> (i64, Instant) {
        self.next_id += 1;
        let line = json!({"id": self.next_id, "cmd": cmd, "args": args}).to_string();
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .expect("write to the engine");
        self.stdin.flush().expect("flush");
        (self.next_id, Instant::now())
    }

    /// Runs one step; returns its result (or error object), the done
    /// event's data for jobs, and the measurements.
    fn step(
        &mut self,
        name: &str,
        cmd: &str,
        args: Value,
        mut until: Until<'_>,
    ) -> (Result<Value, Value>, Option<Value>, Step) {
        self.rss.reset();
        let (id, t0) = self.send(cmd, &args);
        let mut probe = Some(self.send("catalog_info", &json!({})));
        let extra_probe = self.extra_probe.take();
        let mut extra = None;
        let mut last_probe = Instant::now();
        let mut step = Step {
            name: name.to_string(),
            ..Step::default()
        };
        let mut result: Option<Result<Value, Value>> = None;
        let mut answered: Option<Instant> = None;
        let mut done: Option<Value> = None;
        let mut finished = false;
        let mut cancelled = false;
        while !(finished && probe.is_none() && extra.is_none()) {
            if !finished
                && probe.is_none()
                && extra.is_none()
                && last_probe.elapsed() >= PROBE_EVERY
            {
                probe = Some(self.send("catalog_info", &json!({})));
                if let Some((cmd, args)) = &extra_probe {
                    extra = Some(self.send(cmd, args));
                }
                last_probe = Instant::now();
            }
            let (at, line) = match self.lines.recv_timeout(Duration::from_millis(20)) {
                Ok(x) => x,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => panic!("the engine ended during {name}"),
            };
            let value: Value = serde_json::from_str(&line).expect("a JSON line");
            if let Some(rid) = value.get("id").and_then(Value::as_i64) {
                if rid == id {
                    step.response = at - t0;
                    answered = Some(at);
                    let r = if value["ok"] == json!(true) {
                        Ok(value["result"].clone())
                    } else {
                        Err(value["error"].clone())
                    };
                    let failed = r.is_err();
                    result = Some(r);
                    if matches!(until, Until::Response) || failed {
                        step.wall = at - t0;
                        finished = true;
                    }
                } else if probe.is_some_and(|(pid, _)| pid == rid) {
                    if let Some((_, sent)) = probe.take() {
                        step.probes.push(at - sent);
                        if answered.is_some_and(|a| sent > a) {
                            step.job_probes.push(at - sent);
                        }
                    }
                } else if extra.is_some_and(|(pid, _)| pid == rid) {
                    if let Some((_, sent)) = extra.take() {
                        step.extra.push(at - sent);
                    }
                }
                continue;
            }
            let Some(event) = value.get("event").and_then(Value::as_str) else {
                continue;
            };
            if let Until::Done {
                event: want,
                cancel,
            } = &mut until
            {
                if event == *want {
                    step.wall = at - t0;
                    done = Some(value["data"].clone());
                    finished = true;
                } else if event.ends_with("_progress") && !cancelled {
                    if let Some(cancel) = cancel {
                        if cancel(&value["data"]) {
                            let _ = self.send("job_cancel", &json!({}));
                            cancelled = true;
                            step.note = "cancelled".into();
                        }
                    }
                }
            }
        }
        step.peak_kib = self.rss.peak();
        step.after_kib = self.rss.now();
        let result = result.expect("a response");
        (result, done, step)
    }

    /// A request outside the measured steps.
    fn call(&mut self, cmd: &str, args: Value) -> Value {
        let (id, _) = self.send(cmd, &args);
        loop {
            let (_, line) = self.lines.recv().expect("the engine ended");
            let value: Value = serde_json::from_str(&line).expect("a JSON line");
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                assert_eq!(value["ok"], json!(true), "{cmd} {args}: {value}");
                return value["result"].clone();
            }
        }
    }
}

fn ok(r: Result<Value, Value>, what: &str) -> Value {
    r.unwrap_or_else(|e| panic!("{what} failed: {e}"))
}

// ------------------------------------------------------------------------------------------
// The measurement
// ------------------------------------------------------------------------------------------

struct Work {
    dir: PathBuf,
    keep: bool,
}

impl Work {
    fn new() -> Work {
        let base = std::env::var_os("HDS_SCALE_WORK")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("hds-scale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the work directory");
        Work {
            dir: std::path::absolute(&dir).expect("absolute"),
            keep: std::env::var("HDS_SCALE_KEEP").is_ok_and(|v| v == "1"),
        }
    }

    fn path(&self, name: &str) -> String {
        self.dir.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn dir_stats(path: &Path) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else {
            continue;
        };
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) => {
                    files += 1;
                    bytes += m.len();
                }
                Err(_) => {}
            }
        }
    }
    (files, bytes)
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

/// `recover_start` of the whole root taken apart with `hds-core` and
/// `hds-recover` directly, in the order `real_backend::jobs::recover_start`
/// runs it on the request thread with the backend lock held: H1's fresh
/// snapshot, `Catalog::plan`, `Recovery::prepare` (tree, names, report
/// file). Returns printable lines.
fn recover_start_breakdown(saved: &Path, image: &Path, dest: &Path) -> Vec<String> {
    use hds_core::catalog::{Catalog, CatalogInput};
    use hds_core::geometry::Heads;
    use hds_core::instances::{analyze, AnalysisInput, Overrides};
    use hds_core::live::{take_snapshot, LiveIndex, LiveState};
    use hds_core::query::Selection;
    use hds_core::session::Session;
    use hds_io::{open_source, Protected, RangedSource, SourceKind};
    use hds_recover::{PlanFile, PlanFolder, RecoverOptions, Recovery};

    let mut out = Vec::new();
    let never = AtomicBool::new(false);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| (d.as_secs() + 11_644_473_600) * 10_000_000);
    let (session, _) = Session::load(saved).expect("load the saved session");
    let source = open_source(SourceKind::Image, image, None).expect("open the image");
    let range = session.meta().range.clone();
    let ranged = RangedSource::new(source, range.clone()).expect("ranged");
    let store = session.store();
    let mut heads = Heads::load(&session).expect("heads");
    heads.attach(Some(ranged.clone()));
    let mut live = LiveState::load(&session).expect("live");
    live.snapshot = Some(take_snapshot(&ranged, now, &never).expect("snapshot"));
    let layout = analyze(
        &AnalysisInput::new(store, session.meta(), &live),
        &mut heads,
        &Overrides::new(),
        &never,
    )
    .expect("analysis");
    let catalog = Catalog::build(&CatalogInput::new(store, &layout, &heads, &[])).expect("catalog");
    let root = catalog.instances()[0].root.wire();

    let base = heap_peak_reset();
    let t = Instant::now();
    let snapshot = take_snapshot(&ranged, now, &never).expect("snapshot");
    let fresh = LiveIndex::build(store, &snapshot).overwritten().clone();
    drop(snapshot);
    out.push(format!(
        "H1 fresh snapshot + LiveIndex: {:.1} ms, peak +{} MiB",
        ms(t.elapsed()),
        heap_peak().saturating_sub(base) >> 20
    ));

    let base = heap_peak_reset();
    let t = Instant::now();
    let sel = Selection {
        include: vec![root],
        include_deleted: true,
        ..Selection::default()
    };
    let plan = catalog.plan(store, &sel, Some(&fresh)).expect("plan");
    let plan_time = t.elapsed();
    let held = heap_live().saturating_sub(base);
    let files = plan.files.len().max(1);
    let heap_strings: usize = plan
        .files
        .iter()
        .map(|f| f.name.capacity() * 2 + f.ext.capacity() + f.original_path.capacity())
        .sum();
    out.push(format!(
        "Catalog::plan: {:.1} ms; {} files, {} folders; holds {} MiB = {:.0} B per file \
         (PlanFile {} B inline, PlanFolder {} B, name+ext+original_path {:.0} B per file), \
         peak +{} MiB",
        ms(plan_time),
        plan.files.len(),
        plan.folders.len(),
        held >> 20,
        held as f64 / files as f64,
        std::mem::size_of::<PlanFile>(),
        std::mem::size_of::<PlanFolder>(),
        heap_strings as f64 / files as f64,
        heap_peak().saturating_sub(base) >> 20
    ));

    let base = heap_peak_reset();
    let t = Instant::now();
    let mut protected = vec![Protected::session(range.clone())];
    protected.extend(layout.protected().iter().cloned());
    let mut options = RecoverOptions::new(protected);
    options.allow_same_disk = true;
    options.allow_unknown_disk = true;
    let job = Recovery::prepare(ranged, plan, dest, options).expect("prepare");
    out.push(format!(
        "Recovery::prepare (tree, names, report file): {:.1} ms, adds {} MiB = {:.0} B per \
         file, peak +{} MiB",
        ms(t.elapsed()),
        heap_live().saturating_sub(base) >> 20,
        heap_live().saturating_sub(base) as f64 / files as f64,
        heap_peak().saturating_sub(base) >> 20
    ));
    drop(job);
    let _ = std::fs::remove_dir_all(dest);

    // What the engine does now: the selection with 12 bytes per file under
    // the lock, the destination prepared, and the plan ordered and named
    // by the job itself (measured up to its first written file).
    let base = heap_peak_reset();
    let t = Instant::now();
    let planned = catalog
        .plan_selection(store, &sel, Some(&fresh))
        .expect("plan_selection");
    let files = planned.file_count().max(1);
    let folders = planned.folders().len();
    out.push(format!(
        "Catalog::plan_selection: {:.1} ms; holds {} MiB = {:.0} B per file, peak +{} MiB",
        ms(t.elapsed()),
        heap_live().saturating_sub(base) >> 20,
        heap_live().saturating_sub(base) as f64 / files as f64,
        heap_peak().saturating_sub(base) >> 20
    ));
    let t = Instant::now();
    let mut protected = vec![Protected::session(range.clone())];
    protected.extend(layout.protected().iter().cloned());
    let mut options = RecoverOptions::new(protected);
    options.allow_same_disk = true;
    options.allow_unknown_disk = true;
    let ranged = RangedSource::new(
        open_source(SourceKind::Image, image, None).expect("open the image"),
        range,
    )
    .expect("ranged");
    let job = Recovery::prepare_lazy(
        ranged,
        planned.into_lazy(session.shared_store()),
        dest,
        options,
    )
    .expect("prepare_lazy");
    out.push(format!(
        "Recovery::prepare_lazy (destination, report file): {:.1} ms; recover_start answers \
         after this",
        ms(t.elapsed())
    ));
    // The first progress event comes after the first folder is created,
    // that is right after the order and the names.
    struct FirstFile<'a>(&'a AtomicBool, Option<Instant>);
    impl hds_recover::RecoverObserver for FirstFile<'_> {
        fn progress(&mut self, _: &hds_recover::RecoverProgress) {
            self.1.get_or_insert_with(Instant::now);
        }
        fn file_done(&mut self, _: &hds_recover::FileOutcome) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    let mut observer = FirstFile(&cancel, None);
    let summary = job.run(&mut observer, &cancel);
    out.push(format!(
        "job: order + names {:.1} ms; + {} folders created + first file: {:.1} ms ({:?}), \
         peak +{} MiB = {:.0} B per file above the selection's base",
        observer.1.map_or(0.0, |at| ms(at.duration_since(t))),
        folders,
        ms(t.elapsed()),
        summary.status,
        heap_peak().saturating_sub(base) >> 20,
        heap_peak().saturating_sub(base) as f64 / files as f64
    ));
    let _ = std::fs::remove_dir_all(dest);
    out
}

/// Sends a request to the in-process engine and returns its result; other
/// lines are kept in `seen`.
fn inproc_call(engine: &mut InProcess, seen: &mut Vec<String>, cmd: &str, args: &Value) -> Value {
    let id = engine.send(cmd, args);
    loop {
        let line = engine
            .next_line(Duration::from_secs(600))
            .expect("the in-process engine answers");
        match OutputLine::parse(&line) {
            Some(OutputLine::Response(resp)) if resp.id == Some(id) => {
                return response_result(resp).unwrap_or_else(|err| panic!("{cmd} failed: {err}"));
            }
            _ => seen.push(line),
        }
    }
}

#[test]
#[ignore = "scale measurement: set HDS_SCALE_IMAGE, run in release with --ignored --nocapture"]
fn scale_through_the_protocol() {
    let Some(image) = std::env::var_os("HDS_SCALE_IMAGE") else {
        eprintln!("SKIPPED: HDS_SCALE_IMAGE is not set");
        return;
    };
    let image = std::path::absolute(PathBuf::from(image)).expect("absolute image path");
    if cfg!(debug_assertions) {
        eprintln!("WARNING: debug build; the numbers mean little (use --release)");
    }
    let bin = std::env::var_os("HDS_ENGINE_BIN").map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_hdsavior-engine")),
        PathBuf::from,
    );
    let recover_files: u64 = std::env::var("HDS_SCALE_RECOVER_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let work = Work::new();
    let mut steps: Vec<Step> = Vec::new();

    // ---------------------------------------------------------------- child engine
    let mut e = Engine::start(&bin);
    let (_, _, s) = e.step("hello", "hello", json!({}), Until::Response);
    steps.push(s);
    let (r, _, s) = e.step(
        "open_source",
        "open_source",
        json!({"kind": "image", "path": image.to_string_lossy()}),
        Until::Response,
    );
    ok(r, "open_source");
    steps.push(s);

    let session = work.path("scan.hds");
    let (r, done, mut s) = e.step(
        "scan_start .. scan_done",
        "scan_start",
        json!({"session_path": session}),
        Until::Done {
            event: "scan_done",
            cancel: None,
        },
    );
    ok(r, "scan_start");
    let done = done.expect("scan_done");
    assert_eq!(done["status"], json!("completed"), "{done}");
    let catalog = done["catalog"].clone();
    let session_bytes = std::fs::metadata(&session).map_or(0, |m| m.len());
    s.note = format!(
        "{} nodes, session file {} MiB",
        catalog["nodes"],
        session_bytes >> 20
    );
    steps.push(s);
    let epoch = u(&catalog["epoch"]);
    let main = &catalog["instances"][0];
    let root = u(&main["root"]);
    let all_files = u(&main["files"]) + u(&main["deleted_files"]);

    let (r, _, mut s) = e.step("catalog_info", "catalog_info", json!({}), Until::Response);
    s.note = format!("{} bytes of JSON", ok(r, "catalog_info").to_string().len());
    steps.push(s);

    // The biggest folder by direct children: best-first by recursive file
    // count; a folder's direct files are its files minus its subfolders'.
    let t = Instant::now();
    let mut calls = 0u32;
    let mut best = (root, 0u64, String::new());
    let mut queue: Vec<(u64, u64, String)> = vec![(root, all_files, "/".into())];
    let mut explored = 0;
    while let Some(i) = queue
        .iter()
        .enumerate()
        .max_by_key(|(_, q)| q.1)
        .map(|(i, _)| i)
    {
        let (node, files, name) = queue.swap_remove(i);
        if explored >= 400 || t.elapsed() > Duration::from_secs(120) {
            break;
        }
        explored += 1;
        let mut offset = 0;
        let mut below = 0u64;
        loop {
            let page = e.call(
                "list_children",
                json!({"epoch": epoch, "node": node, "offset": offset, "limit": 1000,
                       "dirs_only": true, "show_deleted": true}),
            );
            calls += 1;
            let rows = page["rows"].as_array().cloned().unwrap_or_default();
            for row in &rows {
                let f = u(&row["files"]) + u(&row["deleted_files"]);
                below += f;
                let id = u(&row["id"]);
                let rname = row["name"].as_str().unwrap_or("").to_string();
                if row["has_children"] == json!(true) {
                    queue.push((id, f, rname));
                } else if f > best.1 {
                    best = (id, f, rname);
                }
            }
            offset += rows.len() as u64;
            if rows.is_empty() || offset >= u(&page["total"]) {
                break;
            }
        }
        let direct = files.saturating_sub(below);
        if direct > best.1 {
            best = (node, direct, name);
        }
    }
    let walk = t.elapsed();
    let (big, _, big_name) = best;
    let probe = e.call(
        "list_children",
        json!({"epoch": epoch, "node": big, "limit": 1, "show_deleted": true}),
    );
    let big_total = u(&probe["total"]);
    eprintln!(
        "biggest folder: {big_name:?} (node {big}), {big_total} children; found with {calls} \
         list_children calls in {:.1} s",
        walk.as_secs_f64()
    );

    let list = |offset: u64, sort: &str, desc: bool| {
        json!({"epoch": epoch, "node": big, "offset": offset, "limit": 1000,
               "sort": sort, "desc": desc, "show_deleted": true})
    };
    let (r, _, mut s) = e.step(
        "list_children biggest, page 1 by name",
        "list_children",
        list(0, "name", false),
        Until::Response,
    );
    let first_page = ok(r, "list_children");
    s.note = format!("total {}", first_page["total"]);
    steps.push(s);
    let (_, _, s) = e.step(
        "list_children biggest, middle page",
        "list_children",
        list(big_total / 2, "name", false),
        Until::Response,
    );
    steps.push(s);
    let (_, _, s) = e.step(
        "list_children biggest, last page by size desc",
        "list_children",
        list(big_total.saturating_sub(1000), "size", true),
        Until::Response,
    );
    steps.push(s);
    let (r, _, mut s) = e.step(
        "list_children root",
        "list_children",
        json!({"epoch": epoch, "node": root, "limit": 1000, "show_deleted": true}),
        Until::Response,
    );
    let root_page = ok(r, "list_children root");
    s.note = format!("total {}", root_page["total"]);
    steps.push(s);

    // search: one name (few hits), and everything.
    let a_file = first_page["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["kind"] == json!("file")))
        .cloned()
        .expect("a file in the biggest folder");
    let a_name = a_file["name"].as_str().unwrap_or("").to_string();
    let (r, _, mut s) = e.step(
        "search one name",
        "search",
        json!({"epoch": epoch, "under": root, "filter": {"query": a_name},
               "show_deleted": true, "limit": 200}),
        Until::Response,
    );
    s.note = format!("{a_name:?}: {} hits", ok(r, "search")["total"]);
    steps.push(s);
    let (r, _, mut s) = e.step(
        "search all files, page 1 by name",
        "search",
        json!({"epoch": epoch, "under": root, "filter": null, "show_deleted": true,
               "limit": 1000}),
        Until::Response,
    );
    let total_hits = u(&ok(r, "search")["total"]);
    s.note = format!("{total_hits} hits");
    steps.push(s);
    let (_, _, mut s) = e.step(
        "search all files, page 1 again (cached)",
        "search",
        json!({"epoch": epoch, "under": root, "filter": null, "show_deleted": true,
               "limit": 1000}),
        Until::Response,
    );
    s.note = "the same request: its sorted list is cached".into();
    steps.push(s);
    let (_, _, s) = e.step(
        "search all files, last page",
        "search",
        json!({"epoch": epoch, "under": root, "filter": null, "show_deleted": true,
               "offset": total_hits.saturating_sub(1000), "limit": 1000}),
        Until::Response,
    );
    steps.push(s);
    let (r, _, mut s) = e.step(
        "search all files, page 1 by size desc",
        "search",
        json!({"epoch": epoch, "under": root, "filter": null, "show_deleted": true,
               "sort": "size", "desc": true, "limit": 1000}),
        Until::Response,
    );
    let largest = ok(r, "search")["rows"][0].clone();
    s.note = format!("largest file {} bytes", largest["size"]);
    steps.push(s);

    let (r, _, mut s) = e.step(
        "selection_summary root",
        "selection_summary",
        json!({"epoch": epoch, "include": [root], "include_deleted": true}),
        Until::Response,
    );
    let summary = ok(r, "selection_summary");
    s.note = format!("{} files, {} bytes", summary["files"], summary["bytes"]);
    steps.push(s);

    let (_, _, s) = e.step(
        "node_info of a file",
        "node_info",
        json!({"epoch": epoch, "node": a_file["id"]}),
        Until::Response,
    );
    steps.push(s);
    let (r, _, mut s) = e.step(
        "read_file largest, 8 MiB",
        "read_file",
        json!({"epoch": epoch, "node": largest["id"], "offset": 0, "length": 8 << 20}),
        Until::Response,
    );
    s.note = match r {
        Ok(v) => format!(
            "{} bytes base64",
            v["data_b64"].as_str().map_or(0, str::len)
        ),
        Err(err) => format!("error {err}"),
    };
    steps.push(s);

    let rec_a = work.path("recover-folder");
    let rec_b = work.path("recover-root");
    let (r, _, mut s) = e.step(
        "check_destination",
        "check_destination",
        json!({"path": work.dir.to_string_lossy(), "purpose": "recover",
               "need_bytes": summary["bytes"]}),
        Until::Response,
    );
    let check = ok(r, "check_destination");
    s.note = format!("verdict {}", check["verdict"]);
    steps.push(s);
    let consent = check["verdict"] != json!("ok");

    // (a) one folder of about 10 000 files: the root child closest to it.
    let folder = root_page["rows"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .filter(|r| r["kind"] != json!("file"))
                .min_by_key(|r| (u(&r["files"]) + u(&r["deleted_files"])).abs_diff(10_000))
        })
        .cloned()
        .expect("a folder below the root");
    let (r, done, mut s) = e.step(
        "recover folder (~10k files)",
        "recover_start",
        json!({"epoch": epoch, "include": [folder["id"]], "dest": rec_a,
               "include_deleted": true, "allow_same_disk": consent,
               "allow_unknown_disk": consent}),
        Until::Done {
            event: "recover_done",
            cancel: None,
        },
    );
    let started = ok(r, "recover_start folder");
    let done = done.expect("recover_done");
    let (n, bytes) = dir_stats(Path::new(&rec_a));
    s.note = format!(
        "{:?}: {} files planned, start answered after {:.0} ms; {} ok; {n} files / {} MiB on disk",
        folder["name"],
        started["files"],
        ms(s.response),
        done["ok"],
        bytes >> 20
    );
    steps.push(s);

    // (b) the whole root, cancelled after `recover_files` files. The extra
    // probe is a broad search: its cached list is dropped whenever a read
    // adds a flag (`Catalog::observe` bumps the generation).
    e.extra_probe = Some((
        "search".into(),
        json!({"epoch": epoch, "under": root, "filter": null, "show_deleted": true,
               "limit": 200}),
    ));
    let started_at = Instant::now();
    let mut cancel = |p: &Value| {
        u(&p["files_done"]) >= recover_files || started_at.elapsed() > Duration::from_secs(300)
    };
    let (r, done, mut s) = e.step(
        "recover whole root",
        "recover_start",
        json!({"epoch": epoch, "include": [root], "dest": rec_b, "include_deleted": true,
               "allow_same_disk": consent, "allow_unknown_disk": consent}),
        Until::Done {
            event: "recover_done",
            cancel: Some(&mut cancel),
        },
    );
    let started = ok(r, "recover_start root");
    let done = done.expect("recover_done");
    let files_per_s = (u(&done["ok"]) + u(&done["warnings"])) as f64
        / s.wall.saturating_sub(s.response).as_secs_f64().max(1e-3);
    s.note = format!(
        "{} files planned, start answered after {:.0} ms; {} {} ok ({files_per_s:.0} files/s)",
        started["files"],
        ms(s.response),
        done["status"],
        done["ok"],
    );
    steps.push(s);
    let _ = std::fs::remove_dir_all(&rec_a);
    let _ = std::fs::remove_dir_all(&rec_b);

    let (r, done, mut s) = e.step(
        "carve_start .. carve_done",
        "carve_start",
        json!({}),
        Until::Done {
            event: "carve_done",
            cancel: None,
        },
    );
    s.note = match (r, done) {
        (Ok(v), Some(d)) => format!("{} candidates, {} found", v["candidates"], d["found"]),
        (r, d) => format!("{r:?} {d:?}"),
    };
    steps.push(s);

    let saved = work.path("saved.hds");
    let (r, _, mut s) = e.step(
        "session_save",
        "session_save",
        json!({"path": saved}),
        Until::Response,
    );
    s.note = format!("{} MiB", u(&ok(r, "session_save")["bytes"]) >> 20);
    steps.push(s);
    let (r, _, mut s) = e.step(
        "session_load",
        "session_load",
        json!({"path": saved}),
        Until::Response,
    );
    s.note = format!(
        "source_attached {}",
        ok(r, "session_load")["source_attached"]
    );
    steps.push(s);
    let _ = e.send("shutdown", &json!({}));
    let _ = e.child.wait();
    drop(e);

    // ---------------------------------------------------------------- in process
    // The heap a recovery plan of the whole root holds, and files_in_extents.
    let backend = build_backend();
    let mut engine = InProcess::start(backend.clone());
    let mut seen: Vec<String> = Vec::new();
    let mut call =
        |engine: &mut InProcess, cmd: &str, args: Value| inproc_call(engine, &mut seen, cmd, &args);
    let before = heap_peak_reset();
    let t = Instant::now();
    let loaded = call(&mut engine, "session_load", json!({"path": saved}));
    let load_time = t.elapsed();
    let after_load = heap_live();
    let load_peak = heap_peak();
    let epoch = u(&loaded["catalog"]["epoch"]);
    let root = u(&loaded["catalog"]["instances"][0]["root"]);
    let nodes = u(&loaded["catalog"]["nodes"]);

    let base = heap_peak_reset();
    let t = Instant::now();
    let started = call(
        &mut engine,
        "recover_start",
        json!({"epoch": epoch, "include": [root], "dest": work.path("recover-inproc"),
               "include_deleted": true, "allow_same_disk": consent,
               "allow_unknown_disk": consent}),
    );
    let plan_time = t.elapsed();
    let answer_held = heap_live().saturating_sub(base);
    let plan_files = u(&started["files"]);
    // The job orders and names the files before it writes the first one:
    // its memory is measured once it has written some.
    let mut running_files = 0;
    while running_files < 1000 {
        let line = engine
            .next_line(Duration::from_secs(600))
            .expect("recover_progress");
        if line.contains("\"recover_done\"") {
            seen.push(line);
            break;
        }
        if line.contains("\"recover_progress\"") {
            let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            running_files = u(&v["data"]["files_done"]);
        }
    }
    let held = heap_live().saturating_sub(base);
    let plan_peak = heap_peak().saturating_sub(base);
    let mut call =
        |engine: &mut InProcess, cmd: &str, args: Value| inproc_call(engine, &mut seen, cmd, &args);
    call(&mut engine, "job_cancel", json!({}));
    let mut done = seen.iter().any(|l| l.contains("\"recover_done\""));
    while !done {
        let line = engine
            .next_line(Duration::from_secs(600))
            .expect("recover_done");
        done = line.contains("\"recover_done\"");
    }
    let _ = std::fs::remove_dir_all(work.path("recover-inproc"));
    let image_size = std::fs::metadata(&image).map_or(0, |m| m.len());
    let t = Instant::now();
    let whole = 0..image_size;
    let (lost_files, lost_bytes) = backend.files_in_extents(std::slice::from_ref(&whole));
    let fie_time = t.elapsed();
    drop(engine);
    let breakdown = recover_start_breakdown(
        Path::new(&saved),
        &image,
        Path::new(&work.path("recover-breakdown")),
    );

    // ---------------------------------------------------------------- report
    eprintln!();
    eprintln!(
        "{:<48} {:>10} {:>10} {:>9} {:>9} {:>7} {:>10}  note",
        "step", "resp ms", "wall ms", "peak MiB", "after MiB", "probes", "probe max"
    );
    let extra_note = |s: &Step| {
        let mut note = String::new();
        if !s.job_probes.is_empty() && s.wall > s.response + PROBE_EVERY {
            note = format!(
                "; catalog_info while the job ran: {} probes, max {:.1} ms",
                s.job_probes.len(),
                ms(s.job_probes.iter().copied().max().unwrap_or_default())
            );
        }
        if !s.extra.is_empty() {
            let mut sorted = s.extra.clone();
            sorted.sort_unstable();
            note.push_str(&format!(
                "; search probes: {} (median {:.0} ms, max {:.0} ms)",
                sorted.len(),
                ms(sorted[sorted.len() / 2]),
                ms(sorted[sorted.len() - 1])
            ));
        }
        note
    };
    for s in &steps {
        eprintln!(
            "{:<48} {:>10.1} {:>10.1} {:>9} {:>9} {:>7} {:>10.1}  {}",
            s.name,
            ms(s.response),
            ms(s.wall),
            s.peak_kib / 1024,
            s.after_kib / 1024,
            s.probes.len(),
            ms(s.max_probe()),
            s.note,
        );
        let extra = extra_note(s);
        if !extra.is_empty() {
            eprintln!("{:<48} {extra}", "");
        }
    }
    eprintln!();
    eprintln!(
        "in process: session_load {:.1} s, heap {} -> {} MiB (peak {} MiB), {} nodes, \
         {:.0} heap bytes per node",
        load_time.as_secs_f64(),
        before >> 20,
        after_load >> 20,
        load_peak >> 20,
        nodes,
        after_load.saturating_sub(before) as f64 / nodes.max(1) as f64
    );
    eprintln!(
        "in process: recover_start of the root answered after {:.2} s ({} MiB held then); {} \
         files; after {running_files} files written the job holds {} MiB of heap ({:.0} B per \
         file), peak {} MiB above the base ({:.0} B per file)",
        plan_time.as_secs_f64(),
        answer_held >> 20,
        plan_files,
        held >> 20,
        held as f64 / plan_files.max(1) as f64,
        plan_peak >> 20,
        plan_peak as f64 / plan_files.max(1) as f64,
    );
    eprintln!(
        "in process: files_in_extents of the whole image: {:.1} ms ({lost_files} files, \
         {lost_bytes} bytes)",
        ms(fie_time)
    );
    for line in &breakdown {
        eprintln!("recover_start breakdown: {line}");
    }
    if let Some(path) = std::env::var_os("HDS_SCALE_REPORT") {
        let mut out = String::new();
        for s in &steps {
            out.push_str(&s.json().to_string());
            out.push('\n');
        }
        out.push_str(
            &json!({
                "step": "in_process",
                "session_load_ms": ms(load_time),
                "heap_after_load_mib": after_load >> 20,
                "heap_per_node": after_load.saturating_sub(before) / nodes.max(1) as usize,
                "plan_ms": ms(plan_time),
                "plan_files": plan_files,
                "plan_held_bytes": held,
                "plan_peak_bytes": plan_peak,
                "files_in_extents_ms": ms(fie_time),
            })
            .to_string(),
        );
        out.push('\n');
        std::fs::write(path, out).expect("write the report");
    }
}
