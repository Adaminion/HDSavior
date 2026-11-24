//! A test double of [`Backend`]: a tiny fixed catalog, jobs that report
//! progress and end (or wait for a cancel), and a few trigger values that
//! make it fail or panic. It reads no disk except to open a source named in
//! `session_load`, and writes no file.
//!
//! The catalog: instance 0 (`main`, root node 0) holds folder 1 `Photos`
//! with file 2 `IMG_0001.JPG` (1000 bytes, a JPEG header) and file 3
//! `packed.bin` (`unsupported`); instance 1 (`strays`, root 4); instance 2
//! (`other`, root 5, empty).
//!
//! Triggers:
//!
//! * node 666 anywhere (a query's node, `include`) - the handler or the job
//!   panics;
//! * a scan whose session path contains `lost` - ends `failed` with
//!   `source_lost` and no catalog;
//! * a scan whose session path contains `late-shutdown` - waits for the
//!   shutdown flag, then ends `completed` with a catalog (a scan that saw
//!   the shutdown too late);
//! * a `session_load` path containing `missing` - `not_found`; containing
//!   `nohits` - a session without signature hits;
//! * a source path containing `mismatch` - `session_attach_source` answers
//!   `source_mismatch`;
//! * a `session_save` path containing `io-fail` - `io_error`;
//! * a `session_save` path containing `slow-save` - takes 2 s;
//! * an `image_start` destination containing `exists` - `exists`.

use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use hds_io::{BlockSource, ProtectedReason};

use crate::backend::{
    Backend, CarveJob, CarveRequest, Destination, FileBytes, ImageJob, ImageRequest, JobStart,
    LoadedSession, RecoverJob, RecoverRequest, ScanJob, ScanRequest, TaskContext,
};
use crate::protocol::*;

/// Node id that makes the fake panic.
pub const PANIC_NODE: u64 = 666;

/// Node id of a file whose `read_file` reads until it is stopped (or for
/// the real backend's time limit, about 5 s).
pub const SLOW_NODE: u64 = 777;

/// Size of the fake file 2.
pub const FILE_SIZE: u64 = 1000;

/// How the fake's jobs end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FakeJobs {
    /// Report progress, then end.
    #[default]
    Finish,
    /// Report progress, then wait until cancelled (or shut down).
    UntilCancelled,
}

/// Configuration of a [`FakeBackend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FakeConfig {
    /// How jobs end.
    pub jobs: FakeJobs,
    /// Progress events each job writes before it ends or waits.
    pub progress_events: u32,
}

impl Default for FakeConfig {
    fn default() -> Self {
        FakeConfig {
            jobs: FakeJobs::Finish,
            progress_events: 3,
        }
    }
}

#[derive(Default)]
struct FakeSession {
    range: Range<u64>,
    source: Option<Arc<dyn BlockSource>>,
    catalog: bool,
    collect: bool,
    carved: u64,
}

#[derive(Default)]
struct FakeState {
    session: Option<FakeSession>,
    calls: Vec<String>,
}

/// The test double. See the module documentation.
#[derive(Clone, Default)]
pub struct FakeBackend {
    config: FakeConfig,
    state: Arc<Mutex<FakeState>>,
}

fn lock(state: &Mutex<FakeState>) -> MutexGuard<'_, FakeState> {
    state.lock().unwrap_or_else(|p| p.into_inner())
}

fn not_found(what: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::NotFound, format!("{what} does not exist"))
}

fn geometry() -> Geometry {
    Geometry {
        cluster_size: 4096,
        volume_offset: 0,
        cluster_source: ClusterSource::Attr,
        offset_source: OffsetSource::Default,
        index_dirs: 0,
        index_blocks: 0,
        signature_checked: 0,
        signature_ok: 0,
        validated: true,
        total_clusters: 256,
    }
}

fn instance(id: u32, kind: InstanceKind, root: u64) -> Instance {
    Instance {
        id,
        kind,
        manual: false,
        label: String::new(),
        root,
        records: 3,
        files: 2,
        dirs: 1,
        deleted_files: 0,
        lost_dirs: 0,
        bytes: FILE_SIZE,
        time_from: None,
        time_to: None,
        embedded_in: None,
        geometry: geometry(),
        geometry_candidates: vec![geometry()],
        virtual_folders: VirtualFolders::default(),
    }
}

fn row(id: u64, parent: Option<u64>, name: &str, kind: NodeKind) -> Row {
    let file = !kind.is_folder();
    Row {
        id,
        parent,
        name: name.to_string(),
        kind,
        ext: match id {
            2 => "jpg".into(),
            3 => "bin".into(),
            _ => String::new(),
        },
        size: if id == 3 { 5000 } else { FILE_SIZE },
        modified: Some(1_714_566_896_000),
        created: None,
        deleted: false,
        rating: match (file, id) {
            (false, _) => None,
            (true, 3) => Some(Rating::Unsupported),
            (true, _) => Some(Rating::Good),
        },
        flags: if id == 3 {
            vec![Flag::Compressed]
        } else {
            vec![]
        },
        files: if file { 0 } else { 2 },
        deleted_files: 0,
        deleted_size: 0,
        has_children: id == 0,
    }
}

/// The fake's nodes.
fn node(id: u64) -> Option<Row> {
    Some(match id {
        0 => row(0, None, "", NodeKind::Virtual),
        1 => row(1, Some(0), "Photos", NodeKind::Dir),
        2 => row(2, Some(1), "IMG_0001.JPG", NodeKind::File),
        3 => row(3, Some(1), "packed.bin", NodeKind::File),
        4 => row(4, None, "[Stray records]", NodeKind::Virtual),
        5 => row(5, None, "[File system 2]", NodeKind::Virtual),
        _ => return None,
    })
}

fn children(id: u64) -> Vec<u64> {
    match id {
        0 => vec![1],
        1 => vec![2, 3],
        _ => vec![],
    }
}

fn check_node(id: u64) -> Result<Row, ProtocolError> {
    if id == PANIC_NODE {
        panic!("fake backend: node {PANIC_NODE} was asked for");
    }
    node(id).ok_or_else(|| not_found(&format!("node {id}")))
}

/// The content of file 2: a JPEG header, then a counting pattern.
pub fn file_content() -> Vec<u8> {
    let mut data = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00,
    ];
    let mut i = 0u8;
    while data.len() < FILE_SIZE as usize {
        data.push(i);
        i = i.wrapping_add(1);
    }
    data
}

impl FakeBackend {
    /// A fake with `config`.
    pub fn new(config: FakeConfig) -> FakeBackend {
        FakeBackend {
            config,
            state: Arc::default(),
        }
    }

    /// The backend methods called so far, by name, in order.
    pub fn calls(&self) -> Vec<String> {
        lock(&self.state).calls.clone()
    }

    fn record(&self, call: &str) -> MutexGuard<'_, FakeState> {
        let mut state = lock(&self.state);
        state.calls.push(call.to_string());
        state
    }

    fn catalog_of(session: &FakeSession) -> CatalogInfo {
        let range = session.range.clone();
        CatalogInfo {
            epoch: 999, // the engine overwrites it
            instances: vec![
                instance(0, InstanceKind::Main, 0),
                instance(1, InstanceKind::Strays, 4),
                instance(2, InstanceKind::Other, 5),
            ],
            nodes: 6,
            live_records: 0,
            post_format_records: 0,
            live_volumes: vec![],
            range: range.clone().into(),
            protected: vec![
                ProtectedRange {
                    start: range.start,
                    end: range.end,
                    reason: ProtectedReason::SessionRange,
                },
                ProtectedRange {
                    start: range.start,
                    end: range.end,
                    reason: ProtectedReason::LostVolume,
                },
            ],
            scanned_bytes: range.end - range.start,
            scan_complete: true,
            bad_bytes: 0,
            out_of_range_files: 0,
            out_of_range_bytes: 0,
            carve_hits: if session.collect { 3 } else { 0 },
            carved: session.carved,
            source_attached: session.source.is_some(),
            warnings: vec![],
        }
    }

    fn catalog(&self) -> Result<CatalogInfo, ProtocolError> {
        let state = lock(&self.state);
        match &state.session {
            Some(session) if session.catalog => Ok(Self::catalog_of(session)),
            _ => Err(ProtocolError::new(ErrorCode::NoCatalog, "fake: no catalog")),
        }
    }

    /// Reports progress, then waits for a cancel when configured so.
    fn work<P: EventData>(
        config: FakeConfig,
        ctx: &crate::backend::JobContext<P>,
        mut progress: impl FnMut(u64) -> P,
    ) {
        for i in 0..config.progress_events {
            ctx.progress(&progress(u64::from(i)));
        }
        if config.jobs == FakeJobs::UntilCancelled {
            while !ctx.is_cancelled() {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

impl Backend for FakeBackend {
    fn scan_start(&self, request: ScanRequest) -> Result<ScanJob, ProtocolError> {
        let mut state = self.record("scan_start");
        let resumed = request.resume && state.session.is_some();
        let session = state.session.get_or_insert_with(FakeSession::default);
        if !resumed {
            session.carved = 0;
        }
        session.range = request.range.clone();
        session.source = Some(request.source.clone());
        session.catalog = false;
        session.collect = request.collect_carve_hits;
        drop(state);
        let bytes_total = request.range.end - request.range.start;
        let session_path = request.session.map(|d| d.path);
        let config = self.config;
        let shared = self.state.clone();
        Ok(JobStart {
            result: ScanStartResult {
                job: JobKind::Scan,
                bytes_total,
            },
            run: Box::new(move |ctx| {
                FakeBackend::work(config, ctx, |i| ScanProgress {
                    bytes_done: i,
                    bytes_total,
                    ..ScanProgress::default()
                });
                if session_path.as_deref().is_some_and(|p| p.contains("lost")) {
                    return ScanDone {
                        status: ScanStatus::Failed,
                        error: Some(ProtocolError::new(
                            ErrorCode::SourceLost,
                            "fake: the source went away",
                        )),
                        session_path,
                        catalog: None,
                    };
                }
                if session_path
                    .as_deref()
                    .is_some_and(|p| p.contains("late-shutdown"))
                {
                    // A scan that decided `completed` (and built its
                    // catalog) just before a shutdown arrived.
                    while !ctx.is_shutting_down() {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    let mut state = lock(&shared);
                    let catalog = state.session.as_mut().map(|session| {
                        session.catalog = true;
                        FakeBackend::catalog_of(session)
                    });
                    return ScanDone {
                        status: ScanStatus::Completed,
                        error: None,
                        session_path,
                        catalog,
                    };
                }
                // A shutdown also sets the cancel flag: `stopped`.
                let status = if ctx.is_cancelled() || ctx.is_shutting_down() {
                    ScanStatus::Stopped
                } else {
                    ScanStatus::Completed
                };
                if ctx.is_shutting_down() {
                    return ScanDone {
                        status,
                        error: None,
                        session_path,
                        catalog: None,
                    };
                }
                let mut state = lock(&shared);
                let catalog = state.session.as_mut().map(|session| {
                    session.catalog = true;
                    FakeBackend::catalog_of(session)
                });
                ScanDone {
                    status,
                    error: None,
                    session_path,
                    catalog,
                }
            }),
        })
    }

    fn session_save(&self, dest: Destination) -> Result<SessionSaveResult, ProtocolError> {
        drop(self.record("session_save"));
        if dest.path.contains("slow-save") {
            std::thread::sleep(Duration::from_secs(2));
        }
        if dest.path.contains("io-fail") {
            return Err(ProtocolError::new(ErrorCode::IoError, "fake: cannot write"));
        }
        Ok(SessionSaveResult {
            path: dest.path,
            bytes: 4096,
        })
    }

    fn session_load(
        &self,
        args: &SessionLoadArgs,
        task: &TaskContext,
    ) -> Result<LoadedSession, ProtocolError> {
        drop(self.record("session_load"));
        task.progress(0.5);
        if args.path.contains("missing") {
            return Err(not_found(&format!("session file {}", args.path)));
        }
        let source = match &args.source {
            Some(source) => Some(
                hds_io::open_source(source.kind, Path::new(&source.path), None)
                    .map_err(|e| ProtocolError::from_io("cannot open the source", &e))?,
            ),
            None => None,
        };
        task.progress(1.0);
        let size = source.as_ref().map_or(1 << 20, |s| s.size());
        let session = FakeSession {
            range: 0..size,
            source: source.clone(),
            catalog: true,
            collect: !args.path.contains("nohits"),
            carved: 0,
        };
        let catalog = Self::catalog_of(&session);
        let info = match &source {
            Some(s) => s.info(),
            None => SourceInfo {
                kind: SourceKind::Image,
                path: "recorded.img".into(),
                size,
                sector_size: 512,
                physical_sector_size: 512,
                model: None,
                serial: None,
                disk_number: None,
            },
        };
        lock(&self.state).session = Some(session);
        Ok(LoadedSession {
            result: SessionLoadResult {
                source: info,
                source_attached: source.is_some(),
                source_error: None,
                catalog,
            },
            collect_carve_hits: !args.path.contains("nohits"),
            source,
        })
    }

    fn session_attach_source(
        &self,
        source: Arc<dyn BlockSource>,
        _force: bool,
    ) -> Result<SessionAttachResult, ProtocolError> {
        let mut state = self.record("session_attach_source");
        let info = source.info();
        if info.path.contains("mismatch") {
            return Err(
                ProtocolError::new(ErrorCode::SourceMismatch, "fake: different disk")
                    .with_data(serde_json::json!({"checks": ["record_hashes"]})),
            );
        }
        let session = state
            .session
            .as_mut()
            .ok_or_else(|| ProtocolError::new(ErrorCode::NoSession, "fake: no session"))?;
        session.source = Some(source);
        session.catalog = true;
        Ok(SessionAttachResult {
            source: info,
            catalog: Self::catalog_of(session),
        })
    }

    fn close_session(&self) {
        self.record("close_session").session = None;
    }

    fn detach_source(&self) {
        if let Some(session) = &mut self.record("detach_source").session {
            session.source = None;
        }
    }

    fn catalog_info(&self) -> Result<CatalogInfo, ProtocolError> {
        drop(self.record("catalog_info"));
        self.catalog()
    }

    fn geometry_set(
        &self,
        _: &GeometrySetArgs,
        task: &TaskContext,
    ) -> Result<CatalogInfo, ProtocolError> {
        drop(self.record("geometry_set"));
        task.progress(1.0);
        self.catalog()
    }

    fn instance_set(&self, _: &InstanceSetArgs) -> Result<CatalogInfo, ProtocolError> {
        drop(self.record("instance_set"));
        self.catalog()
    }

    fn list_children(&self, args: &ListChildrenArgs) -> Result<RowsPage, ProtocolError> {
        drop(self.record("list_children"));
        let parent = check_node(args.node)?;
        if !parent.kind.is_folder() {
            return Err(ProtocolError::bad_request(format!(
                "node {} is not a folder",
                args.node
            )));
        }
        let rows: Vec<Row> = children(args.node)
            .into_iter()
            .filter_map(node)
            .filter(|r| !args.dirs_only || r.kind.is_folder())
            .collect();
        let total = rows.len() as u64;
        let rows = rows
            .into_iter()
            .skip(usize::try_from(args.offset).unwrap_or(usize::MAX))
            .take(args.limit as usize)
            .collect();
        Ok(RowsPage { total, rows })
    }

    fn node_info(&self, args: &NodeInfoArgs) -> Result<NodeInfo, ProtocolError> {
        drop(self.record("node_info"));
        let row = check_node(args.node)?;
        let file = !row.kind.is_folder();
        Ok(NodeInfo {
            ancestors: vec![],
            path: row.name.clone(),
            instance: 0,
            accessed: None,
            other_names: vec![],
            record: None,
            data: file.then_some(DataInfo {
                resident: false,
                real_size: row.size,
                initialized_size: row.size,
                allocated_size: 4096,
                runs: 1,
                first_lcn: Some(10),
                first_offset: Some(40960),
            }),
            flag_details: vec![],
            row,
        })
    }

    fn search(&self, args: &SearchArgs) -> Result<SearchPage, ProtocolError> {
        drop(self.record("search"));
        if let Some(under) = args.under {
            check_node(under)?;
        }
        let query = args.filter.query.clone().unwrap_or_default().to_lowercase();
        let rows: Vec<SearchRow> = [2, 3]
            .into_iter()
            .filter_map(node)
            .filter(|r| r.name.to_lowercase().contains(&query))
            .map(|row| SearchRow {
                path: format!("Photos/{}", row.name),
                ancestor_ids: vec![0, 1],
                row,
            })
            .collect();
        Ok(SearchPage {
            total: rows.len() as u64,
            rows,
        })
    }

    fn selection_summary(&self, args: &SelectionArgs) -> Result<SelectionSummary, ProtocolError> {
        drop(self.record("selection_summary"));
        for &id in args.include.iter().chain(&args.exclude) {
            check_node(id)?;
        }
        Ok(SelectionSummary {
            files: 2,
            bytes: FILE_SIZE,
            dirs: 1,
            good: 1,
            unsupported: 1,
            ..SelectionSummary::default()
        })
    }

    fn files_in_extents(&self, extents: &[Range<u64>]) -> (u64, u64) {
        drop(self.record("files_in_extents"));
        (extents.len() as u64, FILE_SIZE * extents.len() as u64)
    }

    fn recover_start(&self, request: RecoverRequest) -> Result<RecoverJob, ProtocolError> {
        drop(self.record("recover_start"));
        let panics = request.args.include.contains(&PANIC_NODE);
        for &id in request.args.include.iter().chain(&request.args.exclude) {
            if id != PANIC_NODE {
                check_node(id)?;
            }
        }
        let config = self.config;
        let dest = request.dest.path;
        Ok(JobStart {
            result: RecoverStartResult {
                job: JobKind::Recover,
                files: 1,
                bytes: FILE_SIZE,
            },
            run: Box::new(move |ctx| {
                if panics {
                    panic!("fake backend: recovery of node {PANIC_NODE}");
                }
                FakeBackend::work(config, ctx, |i| RecoverProgress {
                    files_done: i,
                    files_total: 1,
                    ..RecoverProgress::default()
                });
                let cancelled = ctx.is_cancelled();
                RecoverDone {
                    status: if cancelled {
                        JobStatus::Cancelled
                    } else {
                        JobStatus::Completed
                    },
                    error: None,
                    files_total: 1,
                    ok: u64::from(!cancelled),
                    bytes_written: if cancelled { 0 } else { FILE_SIZE },
                    report_path: Some(format!("{dest}/HDSavior-report.csv")),
                    dest,
                    ..RecoverDone::default()
                }
            }),
        })
    }

    fn carve_start(&self, request: CarveRequest) -> Result<CarveJob, ProtocolError> {
        drop(self.record("carve_start"));
        let config = self.config;
        let shared = self.state.clone();
        let candidates = request.kinds.len() as u64;
        Ok(JobStart {
            result: CarveStartResult {
                job: JobKind::Carve,
                candidates,
            },
            run: Box::new(move |ctx| {
                FakeBackend::work(config, ctx, |i| CarveProgress {
                    candidates_done: i,
                    candidates_total: candidates,
                    ..CarveProgress::default()
                });
                let mut state = lock(&shared);
                let catalog = state.session.as_mut().map(|s| {
                    s.carved = 1;
                    FakeBackend::catalog_of(s)
                });
                CarveDone {
                    status: if ctx.is_cancelled() {
                        JobStatus::Cancelled
                    } else {
                        JobStatus::Completed
                    },
                    error: None,
                    found: 1,
                    bytes_found: 512,
                    catalog,
                }
            }),
        })
    }

    fn image_start(&self, request: ImageRequest) -> Result<ImageJob, ProtocolError> {
        drop(self.record("image_start"));
        if request.dest.path.contains("exists") {
            return Err(ProtocolError::new(
                ErrorCode::Exists,
                "fake: the image file exists",
            ));
        }
        let config = self.config;
        let total = request.range.end - request.range.start;
        let path = request.dest.path;
        Ok(JobStart {
            result: ImageStartResult {
                job: JobKind::Image,
                bytes: total,
            },
            run: Box::new(move |ctx| {
                FakeBackend::work(config, ctx, |i| ImageProgress {
                    bytes_done: i,
                    bytes_total: total,
                    ..ImageProgress::default()
                });
                let cancelled = ctx.is_cancelled();
                ImageDone {
                    status: if cancelled {
                        JobStatus::Cancelled
                    } else {
                        JobStatus::Completed
                    },
                    error: None,
                    bytes_written: if cancelled { 0 } else { total },
                    bad_bytes: 0,
                    path,
                    bad_map_path: None,
                }
            }),
        })
    }

    fn read_file(
        &self,
        args: &ReadFileArgs,
        stop: &Arc<AtomicBool>,
    ) -> Result<FileBytes, ProtocolError> {
        drop(self.record("read_file"));
        if args.node == SLOW_NODE {
            // A file on a failing disk: reads until stopped or until the
            // time limit of the real backend.
            let started = std::time::Instant::now();
            while !stop.load(Ordering::SeqCst) {
                if started.elapsed() >= crate::real_backend::READ_FILE_DEADLINE {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            return Err(ProtocolError::new(
                ErrorCode::IoError,
                "fake: reading stopped",
            ));
        }
        let row = check_node(args.node)?;
        if row.kind.is_folder() {
            return Err(ProtocolError::bad_request(format!(
                "node {} is a folder",
                args.node
            )));
        }
        if row.rating == Some(Rating::Unsupported) {
            return Err(ProtocolError::new(
                ErrorCode::Unsupported,
                "fake: compressed",
            ));
        }
        let content = file_content();
        let start = args.offset.min(FILE_SIZE);
        let end = start.saturating_add(args.length).min(FILE_SIZE);
        Ok(FileBytes {
            data: content[start as usize..end as usize].to_vec(),
            size: FILE_SIZE,
            eof: end == FILE_SIZE,
        })
    }
}
