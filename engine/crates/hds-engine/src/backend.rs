//! The boundary between the protocol server and the crates that scan and
//! recover (`hds-core`, `hds-recover`).
//!
//! The server ([`crate::engine`]) owns everything that is protocol: the
//! state machine N/O/S/C/B/J and the error-by-state table, argument
//! validation, the open source, catalog epochs, the destination guard's
//! pre-check, the job slot, ordering of responses and events. A
//! [`Backend`] gets only requests that passed all of that, and answers at
//! the protocol level: protocol types in, protocol results or a
//! [`ProtocolError`] out, events through the contexts below (which cannot
//! break the ordering rules: a job's `*_done` event is the value its run
//! function returns, written by the server after the job slot is free).
//!
//! # What the server guarantees to a backend
//!
//! * Calls come from the server thread one at a time, except that the
//!   query methods (`catalog_info`, `list_children`, `node_info`, `search`,
//!   `selection_summary`, `read_file`, `files_in_extents`) may run while a
//!   job's run function runs on the worker thread. A backend must therefore
//!   be `Sync` and synchronize its own state.
//! * Node commands reach the backend only when a catalog exists and the
//!   request's `epoch` is the current one; the backend need not look at
//!   `epoch` at all. Commands that need a source get one.
//! * Ranges are sector-aligned, non-empty and inside the source; node lists,
//!   filters, limits and kinds are validated (see [`crate::engine`]).
//! * Destinations were checked with `hds_io::destination_check` against the
//!   protected ranges of that moment and are allowed with the consents
//!   given; [`Destination`] carries the inputs so that the backend can run
//!   the guard again on the created root (ALGORITHMS H3).
//!
//! # What a backend promises
//!
//! * A method that returns an error leaves the backend's state unchanged.
//! * `scan_start` with `resume: false` replaces the session and drops the
//!   catalog only once it returns `Ok`; while the scan runs, no catalog
//!   method is called.
//! * The `epoch` of every [`CatalogInfo`] it returns is ignored: the server
//!   numbers epochs (a new one for every `scan_done` with a catalog,
//!   `session_load`, `session_attach_source` and `instance_set`, the same
//!   one for `catalog_info`, `geometry_set` and `carve_done`).
//! * Run functions poll [`JobContext::cancel_flag`] and, when it is set,
//!   end with the documented status: `stopped` for a scan (saving the
//!   session; when [`JobContext::is_shutting_down`] also holds, without
//!   building a catalog), `cancelled` for the others (removing the current
//!   part file). A run function that panics is reported as `failed` with
//!   `internal`; so is a query method that panics.

use std::marker::PhantomData;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use hds_carve::FileKind;
use hds_io::{BlockSource, DestinationCheck, PartitionInfo, Protected};

use crate::output::Output;
use crate::protocol::{
    CarveDone, CarveProgress, CarveStartResult, CatalogInfo, ErrorCode, EventData, GeometrySetArgs,
    ImageDone, ImageProgress, ImageStartResult, InstanceSetArgs, ListChildrenArgs, LogEvent,
    LogLevel, NodeInfo, NodeInfoArgs, ProtocolError, ReadFileArgs, RecoverDone, RecoverProgress,
    RecoverStartArgs, RecoverStartResult, RowsPage, ScanDone, ScanProgress, ScanStartResult,
    SearchArgs, SearchPage, SelectionArgs, SelectionSummary, SessionAttachResult, SessionLoadArgs,
    SessionLoadResult, SessionSaveResult, TaskName, TaskProgress,
};

/// What a job's run function gets: its events, its cancel flag.
pub struct JobContext<P> {
    out: Output,
    cancel: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    _progress: PhantomData<fn(&P)>,
}

impl<P: EventData> JobContext<P> {
    /// A context writing to `out`. The server makes one per job; backends
    /// can make their own to test run functions in isolation.
    pub fn new(out: Output, cancel: Arc<AtomicBool>, shutdown: Arc<AtomicBool>) -> JobContext<P> {
        JobContext {
            out,
            cancel,
            shutdown,
            _progress: PhantomData,
        }
    }

    /// Writes a progress event of this job.
    pub fn progress(&self, progress: &P) {
        self.out.event(progress);
    }

    /// Writes a `log` event.
    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        self.out.event(&LogEvent {
            level,
            message: message.into(),
        });
    }

    /// Set by `job_cancel`, by `shutdown` and at the end of input. This is
    /// the flag to hand to `hds_io::read_resilient`.
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }

    /// True once the job should stop.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// True when the engine is shutting down (`shutdown` or end of input):
    /// a scan then saves its session but builds no catalog.
    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }
}

/// What a plain request that takes long (`session_load`, `geometry_set`)
/// gets: a way to report `task_progress`.
pub struct TaskContext {
    out: Output,
    task: TaskName,
}

impl TaskContext {
    /// A context writing `task_progress` events for `task` to `out`.
    pub fn new(out: Output, task: TaskName) -> TaskContext {
        TaskContext { out, task }
    }

    /// Writes `task_progress` with `fraction` clamped to 0..=1 (NaN as 0).
    pub fn progress(&self, fraction: f64) {
        let fraction = if fraction.is_nan() {
            0.0
        } else {
            fraction.clamp(0.0, 1.0)
        };
        self.out.event(&TaskProgress {
            task: self.task,
            fraction,
        });
    }
}

/// A job's body: runs on the worker thread and returns the job's `*_done`
/// event, which the server writes once the job slot is free.
pub type JobRun<P, D> = Box<dyn FnOnce(&JobContext<P>) -> D + Send + 'static>;

/// What a successful `*_start` returns: the response, and the job.
pub struct JobStart<R, P, D> {
    /// The result of the `*_start` request, written before any event of the job.
    pub result: R,
    /// The job.
    pub run: JobRun<P, D>,
}

/// A started scan.
pub type ScanJob = JobStart<ScanStartResult, ScanProgress, ScanDone>;
/// A started recovery.
pub type RecoverJob = JobStart<RecoverStartResult, RecoverProgress, RecoverDone>;
/// A started carving run.
pub type CarveJob = JobStart<CarveStartResult, CarveProgress, CarveDone>;
/// A started imaging run.
pub type ImageJob = JobStart<ImageStartResult, ImageProgress, ImageDone>;

/// A destination that passed the server's guard check, with everything
/// needed to check it again where it is created.
#[derive(Debug, Clone)]
pub struct Destination {
    /// Native absolute path as the client sent it.
    pub path: String,
    /// Consent for the source disk.
    pub allow_same_disk: bool,
    /// Consent for an unknown disk.
    pub allow_unknown_disk: bool,
    /// The protected ranges it was checked against, in the order the guard
    /// takes them (session range first).
    pub protected: Vec<Protected>,
    /// The server's check.
    pub check: DestinationCheck,
}

/// A validated `scan_start`.
pub struct ScanRequest {
    /// The whole source; the scan wraps it in a `RangedSource` of `range`.
    pub source: Arc<dyn BlockSource>,
    /// Its partitions as `open_source` sniffed them.
    pub partitions: Vec<PartitionInfo>,
    /// The session range: sector-aligned, non-empty, inside the source. For
    /// a resumed scan, the session's.
    pub range: Range<u64>,
    /// Where the session is saved; `None` = not saved automatically. For a
    /// resumed scan the default (the file the session was loaded from or
    /// last saved to) is already applied.
    pub session: Option<Destination>,
    /// Continue the loaded session.
    pub resume: bool,
    /// Collect signature hits (for a resumed scan, the session's).
    pub collect_carve_hits: bool,
}

/// A loaded session.
pub struct LoadedSession {
    /// The response.
    pub result: SessionLoadResult,
    /// The attached source, when `result.source_attached`.
    pub source: Option<Arc<dyn BlockSource>>,
    /// Whether the session's scan collects signature hits (needed to answer
    /// `carve_start` and a resumed `scan_start`).
    pub collect_carve_hits: bool,
}

/// A validated `recover_start`.
pub struct RecoverRequest {
    /// The arguments (epoch already checked).
    pub args: RecoverStartArgs,
    /// The destination directory.
    pub dest: Destination,
}

/// A validated `carve_start`.
pub struct CarveRequest {
    /// The kinds to carve; never empty (`null` became every kind).
    pub kinds: Vec<FileKind>,
}

/// A validated `image_start`.
pub struct ImageRequest {
    /// The whole source.
    pub source: Arc<dyn BlockSource>,
    /// The range to image: sector-aligned, non-empty, inside the source.
    pub range: Range<u64>,
    /// The image file; its protected ranges include `range`.
    pub dest: Destination,
    /// Continue an earlier image.
    pub resume: bool,
}

/// Bytes of a file for `read_file`; the server encodes and sniffs them.
pub struct FileBytes {
    /// The bytes read, at most the requested length.
    pub data: Vec<u8>,
    /// Size of the file.
    pub size: u64,
    /// True when `data` reaches the end of the file.
    pub eof: bool,
}

/// Everything of the protocol that needs `hds-core` or `hds-recover`.
pub trait Backend: Send + Sync {
    /// `scan_start` (see [`ScanRequest`]). On `Ok` the backend has replaced
    /// (or, resuming, kept) its session and has no catalog until the run
    /// function returns a `scan_done` with one.
    fn scan_start(&self, request: ScanRequest) -> Result<ScanJob, ProtocolError>;

    /// `session_save`: writes a compacted copy of the session to `dest.path`.
    fn session_save(&self, dest: Destination) -> Result<SessionSaveResult, ProtocolError>;

    /// `session_load`: replaces the session and catalog. Opens the recorded
    /// source or `args.source` and checks its identity (ALGORITHMS B4).
    fn session_load(
        &self,
        args: &SessionLoadArgs,
        task: &TaskContext,
    ) -> Result<LoadedSession, ProtocolError>;

    /// `session_attach_source` with a source the server opened: checks its
    /// identity (`source_mismatch`) and rebuilds the catalog.
    fn session_attach_source(
        &self,
        source: Arc<dyn BlockSource>,
        force: bool,
    ) -> Result<SessionAttachResult, ProtocolError>;

    /// Drops the session and catalog (`open_source` of another source).
    fn close_session(&self);

    /// Drops every handle to the source (`close_source`); the catalog stays
    /// browsable.
    fn detach_source(&self);

    /// `catalog_info`.
    fn catalog_info(&self) -> Result<CatalogInfo, ProtocolError>;

    /// `geometry_set` (instance exists, values validated).
    fn geometry_set(
        &self,
        args: &GeometrySetArgs,
        task: &TaskContext,
    ) -> Result<CatalogInfo, ProtocolError>;

    /// `instance_set` (instance exists and is not `strays`).
    fn instance_set(&self, args: &InstanceSetArgs) -> Result<CatalogInfo, ProtocolError>;

    /// `list_children`; `not_found` for an unknown node.
    fn list_children(&self, args: &ListChildrenArgs) -> Result<RowsPage, ProtocolError>;

    /// `node_info`; `not_found` for an unknown node.
    fn node_info(&self, args: &NodeInfoArgs) -> Result<NodeInfo, ProtocolError>;

    /// `search`; `not_found` for an unknown `under`.
    fn search(&self, args: &SearchArgs) -> Result<SearchPage, ProtocolError>;

    /// `selection_summary`; `not_found` for an unknown node in the lists.
    fn selection_summary(&self, args: &SelectionArgs) -> Result<SelectionSummary, ProtocolError>;

    /// Catalog files, and their bytes, stored inside `extents` (absolute
    /// byte ranges of the source): `check_destination.lost_files` and
    /// `lost_bytes`. Called only while a catalog exists.
    fn files_in_extents(&self, extents: &[Range<u64>]) -> (u64, u64);

    /// `recover_start`: `bad_request` for a selection without files,
    /// `no_space`, `destination_refused` when the guard on the created root
    /// refuses.
    fn recover_start(&self, request: RecoverRequest) -> Result<RecoverJob, ProtocolError>;

    /// `carve_start`: `unsupported` without stored signature hits.
    fn carve_start(&self, request: CarveRequest) -> Result<CarveJob, ProtocolError>;

    /// `image_start`: `exists`, `no_space`, `destination_refused`.
    fn image_start(&self, request: ImageRequest) -> Result<ImageJob, ProtocolError>;

    /// `read_file`: `not_found`, `bad_request` for a folder, `unsupported`
    /// for a file whose content is not stored as plain clusters.
    fn read_file(&self, args: &ReadFileArgs) -> Result<FileBytes, ProtocolError>;
}

/// The backend this build of the engine serves (`serve`, the command-line
/// interface, the transcript test). Until `hds-core` and `hds-recover` are
/// connected it is [`UnavailableBackend`]; connecting them means returning
/// their implementation of [`Backend`] here.
pub fn build_backend() -> Arc<dyn Backend> {
    Arc::new(UnavailableBackend)
}

/// The backend of an engine built without the scanner and the recovery
/// engine: every command that needs them answers `unsupported`.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableBackend;

impl UnavailableBackend {
    fn refuse<T>(what: &str) -> Result<T, ProtocolError> {
        Err(ProtocolError::new(
            ErrorCode::Unsupported,
            format!("this build of the engine cannot {what}: it has no scanner or recovery engine"),
        ))
    }
}

impl Backend for UnavailableBackend {
    fn scan_start(&self, _: ScanRequest) -> Result<ScanJob, ProtocolError> {
        Self::refuse("scan")
    }
    fn session_save(&self, _: Destination) -> Result<SessionSaveResult, ProtocolError> {
        Self::refuse("save sessions")
    }
    fn session_load(
        &self,
        _: &SessionLoadArgs,
        _: &TaskContext,
    ) -> Result<LoadedSession, ProtocolError> {
        Self::refuse("load sessions")
    }
    fn session_attach_source(
        &self,
        _: Arc<dyn BlockSource>,
        _: bool,
    ) -> Result<SessionAttachResult, ProtocolError> {
        Self::refuse("attach sources")
    }
    fn close_session(&self) {}
    fn detach_source(&self) {}
    fn catalog_info(&self) -> Result<CatalogInfo, ProtocolError> {
        Self::refuse("build catalogs")
    }
    fn geometry_set(
        &self,
        _: &GeometrySetArgs,
        _: &TaskContext,
    ) -> Result<CatalogInfo, ProtocolError> {
        Self::refuse("change geometries")
    }
    fn instance_set(&self, _: &InstanceSetArgs) -> Result<CatalogInfo, ProtocolError> {
        Self::refuse("change instances")
    }
    fn list_children(&self, _: &ListChildrenArgs) -> Result<RowsPage, ProtocolError> {
        Self::refuse("list folders")
    }
    fn node_info(&self, _: &NodeInfoArgs) -> Result<NodeInfo, ProtocolError> {
        Self::refuse("describe nodes")
    }
    fn search(&self, _: &SearchArgs) -> Result<SearchPage, ProtocolError> {
        Self::refuse("search")
    }
    fn selection_summary(&self, _: &SelectionArgs) -> Result<SelectionSummary, ProtocolError> {
        Self::refuse("summarize selections")
    }
    fn files_in_extents(&self, _: &[Range<u64>]) -> (u64, u64) {
        (0, 0)
    }
    fn recover_start(&self, _: RecoverRequest) -> Result<RecoverJob, ProtocolError> {
        Self::refuse("recover files")
    }
    fn carve_start(&self, _: CarveRequest) -> Result<CarveJob, ProtocolError> {
        Self::refuse("carve")
    }
    fn image_start(&self, _: ImageRequest) -> Result<ImageJob, ProtocolError> {
        Self::refuse("image disks")
    }
    fn read_file(&self, _: &ReadFileArgs) -> Result<FileBytes, ProtocolError> {
        Self::refuse("read files")
    }
}
