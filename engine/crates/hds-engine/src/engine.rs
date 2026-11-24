//! The protocol server's core: the state machine, the error-by-state table,
//! argument validation and the job manager (PROTOCOL.md sections 1 and 3).
//!
//! An [`Engine`] runs on one thread and takes [`Input`]s from one channel:
//! request lines (from stdin, or from the command-line interface), the end
//! of input, and the completion of its job. Handling them one at a time from
//! one queue is what gives the ordering rules:
//!
//! * requests are answered one at a time, in arrival order;
//! * the response to `X_start` is written before the job's thread is allowed
//!   to run, so before any event of the job;
//! * the job's `X_done` event is the value its run function returns; the
//!   engine writes it when it takes the completion from the queue, after
//!   freeing the job slot and putting the resulting state in place.
//!
//! # States
//!
//! The letters of PROTOCOL.md are derived from four facts: is a source open
//! (attached), is there a session, does it have a catalog, which job runs.
//! **N** no source, no session · **O** source, no catalog · **S** scan
//! running · **C** catalog and source · **B** catalog, no source · **J**
//! carve, recover or image running. Two situations the table does not name
//! are handled by the facts: a session without catalog (after a failed
//! scan) behaves like N or O for catalog commands but has a session for
//! `session_save`, `session_attach_source` and a resumed `scan_start`; an
//! image job started without a catalog is J for the job commands and has no
//! catalog for the queries.
//!
//! # Errors by state
//!
//! | command | check, in this order |
//! |---|---|
//! | `hello`, `list_disks`, `job_cancel`, `shutdown` | always answered |
//! | `open_source`, `close_source`, `session_load` | S, J `busy` |
//! | `session_save` | S, J `busy` · no session `no_session` · guard `destination_refused` |
//! | `session_attach_source` | S, J `busy` · no session `no_session` · open errors · `source_mismatch` (backend) |
//! | `geometry_set`, `instance_set` | S, J `busy` · no catalog `no_catalog` · unknown instance `not_found` · bad values / `strays` `bad_request` |
//! | `scan_start` | S, J `busy` · no source `no_source` · resume without session `no_session` · bad range `bad_request` · guard `destination_refused` |
//! | `catalog_info` | no catalog (N, O, S) `no_catalog` |
//! | `list_children`, `node_info`, `search`, `selection_summary` | no catalog `no_catalog` · wrong epoch `stale_catalog` · bad arguments `bad_request` · unknown id `not_found` (backend) |
//! | `check_destination` | no source (N, B) `no_source` |
//! | `recover_start`, `carve_start` | S `no_catalog` · J `busy` · no catalog `no_catalog` · B `no_source` · wrong epoch `stale_catalog` · empty selection, bad kinds `bad_request` · guard `destination_refused` · carve without collected hits `unsupported` |
//! | `image_start` | S, J `busy` · no source `no_source` · bad range `bad_request` · guard `destination_refused` |
//! | `read_file` | no catalog `no_catalog` · B `no_source` · wrong epoch `stale_catalog` · `length` over 8 MiB `bad_request` |
//!
//! A request whose arguments do not have the documented shape is answered
//! `bad_request` before its state is looked at.
//!
//! # Protected ranges (S2)
//!
//! The destination guard is given, in this order: the session range (the
//! whole source while there is no session), then every `lost_volume` range
//! of the catalog; `image_start` adds the imaged range. The session range is
//! taken from the engine's own record, never only from the backend's
//! `CatalogInfo.protected`.

use std::any::Any;
use std::io;
use std::ops::Range;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map, Value};

use hds_io::{
    annotate_partitions, destination_check, list_disks, open_source, parse_partition_table,
    source_warnings, BlockSource, DiskInfo, PartitionInfo, Protected, ProtectedReason, Purpose,
    SourceInfo, SourceKind, TableKind,
};

use crate::backend::{
    Backend, CarveRequest, Destination, ImageRequest, JobContext, JobStart, RecoverRequest,
    ScanRequest, TaskContext,
};
use crate::output::{log_error, log_info, log_warn, Output};
use crate::protocol::{
    parse_request, CarveDone, CarveStartArgs, CatalogInfo, CheckDestinationArgs,
    CheckDestinationResult, Command, Empty, ErrorCode, EventData, GeometrySetArgs, HelloResult,
    ImageDone, ImageStartArgs, InstanceKind, InstanceSetArgs, JobKind, JobStatus, ListChildrenArgs,
    ListDisksResult, NodeInfoArgs, OpenSourceArgs, OpenSourceResult, ProtocolError, ReadFileArgs,
    ReadFileResult, RecoverDone, RecoverStartArgs, ScanDone, ScanStartArgs, ScanStatus, SearchArgs,
    SelectionArgs, SessionAttachArgs, SessionLoadArgs, SessionSaveArgs, TaskName, PROTOCOL_VERSION,
};
use crate::validate;

/// Longest request line accepted (16 MiB); longer lines are answered
/// `bad_request` with `"id": null` and skipped.
pub const MAX_LINE_BYTES: usize = 16 << 20;

/// Requests and completions waiting for the engine. Bounded, so that a
/// client that writes faster than the engine answers is slowed down.
const INBOX_CAPACITY: usize = 64;

/// Why a request line could not be read as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineError {
    /// Longer than [`MAX_LINE_BYTES`].
    TooLong,
    /// Not valid UTF-8.
    NotUtf8,
}

/// What the engine reacts to.
pub enum Input {
    /// One request line, without its terminator.
    Line(Result<String, LineError>),
    /// The end of input: like `shutdown`, without a response.
    Eof,
    /// The engine's job ended (sent by the job's thread).
    JobFinished(Box<JobFinished>),
}

/// The end of a job, as its thread reports it.
pub struct JobFinished {
    id: u64,
    outcome: Result<Outcome, ProtocolError>,
}

/// The `*_done` event a job produced.
enum Outcome {
    Scan(ScanDone),
    Recover(RecoverDone),
    Carve(CarveDone),
    Image(ImageDone),
}

/// The open (attached) source.
struct OpenedSource {
    source: Arc<dyn BlockSource>,
    partitions: Vec<PartitionInfo>,
}

impl OpenedSource {
    fn new(source: Arc<dyn BlockSource>) -> (OpenedSource, TableKind, Vec<hds_io::Warning>) {
        let mut table = parse_partition_table(&*source);
        annotate_partitions(&mut table, &*source);
        let warnings = source_warnings(&*source, 0..source.size());
        (
            OpenedSource {
                source,
                partitions: table.partitions,
            },
            table.table,
            warnings,
        )
    }
}

/// What the engine knows about the backend's catalog.
struct CatalogView {
    epoch: u64,
    lost_volumes: Vec<Range<u64>>,
    instances: Vec<(u32, InstanceKind)>,
}

impl CatalogView {
    fn of(catalog: &CatalogInfo) -> CatalogView {
        CatalogView {
            epoch: catalog.epoch,
            lost_volumes: catalog
                .protected
                .iter()
                .filter(|p| p.reason == ProtectedReason::LostVolume && p.start < p.end)
                .map(|p| p.start..p.end)
                .collect(),
            instances: catalog.instances.iter().map(|i| (i.id, i.kind)).collect(),
        }
    }
}

/// What the engine knows about the backend's session.
struct SessionView {
    range: Range<u64>,
    collect_carve_hits: bool,
    /// The file it was loaded from or last saved to (default of a resume).
    path: Option<String>,
    /// Size of the session's source, for `geometry_set`.
    source_size: u64,
    catalog: Option<CatalogView>,
}

struct RunningJob {
    id: u64,
    kind: JobKind,
    cancel: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    /// The session path (scan) or destination (recover, image), for a
    /// `*_done` the engine has to make up after a panic.
    label: Option<String>,
}

/// Whether the engine goes on after a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Exit,
}

/// A job's body with its types erased: output, cancel flag, shutdown flag.
type ErasedRun = Box<dyn FnOnce(&Output, &Arc<AtomicBool>, &Arc<AtomicBool>) -> Outcome + Send>;

/// A started job before its thread exists.
struct PendingJob {
    kind: JobKind,
    label: Option<String>,
    run: ErasedRun,
}

/// What a command handler produced.
enum Reply {
    Value(Value),
    Job(Value, PendingJob),
    Shutdown,
}

type Handled = Result<Reply, ProtocolError>;

fn reply<T: Serialize>(value: &T) -> Handled {
    serde_json::to_value(value)
        .map(Reply::Value)
        .map_err(|e| ProtocolError::internal(format!("cannot serialize the result: {e}")))
}

fn parse_args<T: DeserializeOwned>(args: Map<String, Value>) -> Result<T, ProtocolError> {
    serde_json::from_value(Value::Object(args))
        .map_err(|e| ProtocolError::bad_request(format!("invalid arguments: {e}")))
}

fn error(code: ErrorCode, message: &str) -> ProtocolError {
    ProtocolError::new(code, message)
}

fn panic_text(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic without message".to_string()
    }
}

/// Wraps a backend job so that the engine can run it without knowing its types.
fn erase<R, P, D>(
    kind: JobKind,
    label: Option<String>,
    job: JobStart<R, P, D>,
    wrap: fn(D) -> Outcome,
) -> (R, PendingJob)
where
    P: EventData + 'static,
    D: 'static,
{
    let run = job.run;
    let pending = PendingJob {
        kind,
        label,
        run: Box::new(move |out, cancel, shutdown| {
            let context = JobContext::<P>::new(out.clone(), cancel.clone(), shutdown.clone());
            wrap(run(&context))
        }),
    };
    (job.result, pending)
}

/// A source that is not attached: the guard learns nothing about where it
/// lives and treats every destination as being on an unknown disk.
struct Unattached;

impl BlockSource for Unattached {
    fn size(&self) -> u64 {
        0
    }
    fn sector_size(&self) -> u32 {
        512
    }
    fn physical_sector_size(&self) -> u32 {
        512
    }
    fn read_at(&self, _: u64, _: &mut [u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no source is attached",
        ))
    }
    fn info(&self) -> SourceInfo {
        SourceInfo {
            kind: SourceKind::Image,
            path: String::new(),
            size: 0,
            sector_size: 512,
            physical_sector_size: 512,
            model: None,
            serial: None,
            disk_number: None,
        }
    }
    fn is_alive(&self) -> bool {
        false
    }
}

/// The protocol server's state machine. See the module documentation.
pub struct Engine {
    backend: Arc<dyn Backend>,
    out: Output,
    inbox_tx: SyncSender<Input>,
    inbox_rx: Receiver<Input>,
    source: Option<OpenedSource>,
    session: Option<SessionView>,
    last_epoch: u64,
    job: Option<RunningJob>,
    last_job_id: u64,
}

impl Engine {
    /// An engine that writes to `out` and hands everything that needs the
    /// scanner or the recovery engine to `backend`.
    pub fn new(backend: Arc<dyn Backend>, out: Output) -> Engine {
        let (inbox_tx, inbox_rx) = mpsc::sync_channel(INBOX_CAPACITY);
        Engine {
            backend,
            out,
            inbox_tx,
            inbox_rx,
            source: None,
            session: None,
            last_epoch: 0,
            job: None,
            last_job_id: 0,
        }
    }

    /// Where to send [`Input`]s. Whoever feeds the engine must end with
    /// [`Input::Eof`] (or a `shutdown` request): the engine holds a sender
    /// itself, so it never sees the channel close.
    pub fn sender(&self) -> SyncSender<Input> {
        self.inbox_tx.clone()
    }

    /// Runs until `shutdown` or the end of input, then ends the running job
    /// as PROTOCOL.md "End of input" says (a scan stops and saves its session
    /// without building a catalog, other jobs are cancelled), writes its
    /// `*_done` event and returns. Requests that arrive while the job ends
    /// are not answered.
    pub fn run(mut self) {
        while let Ok(input) = self.inbox_rx.recv() {
            match input {
                Input::Line(line) => {
                    if self.handle_line(line) == Flow::Exit {
                        break;
                    }
                }
                Input::Eof => {
                    log_info("end of input");
                    break;
                }
                Input::JobFinished(finished) => self.finish_job(*finished),
            }
        }
        self.stop_job_and_wait();
        log_info("exiting");
    }

    /// Ends the running job for shutdown or end of input: sets its shutdown
    /// and cancel flags (a scan then saves its session without building a
    /// catalog; other jobs remove their part file), waits for it and writes
    /// its `*_done` event. Requests that arrive meanwhile are not answered.
    fn stop_job_and_wait(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        log_info("stopping the running job before exit");
        job.shutdown.store(true, Ordering::SeqCst);
        job.cancel.store(true, Ordering::SeqCst);
        while self.job.is_some() {
            match self.inbox_rx.recv() {
                Ok(Input::JobFinished(finished)) => self.finish_job(*finished),
                Ok(Input::Line(_)) => log_warn("a request after shutdown was ignored"),
                Ok(Input::Eof) => {}
                Err(_) => break,
            }
        }
    }

    // --------------------------------------------------------------------------------------
    // Requests
    // --------------------------------------------------------------------------------------

    fn handle_line(&mut self, line: Result<String, LineError>) -> Flow {
        let text = match line {
            Ok(text) => text,
            Err(LineError::TooLong) => {
                self.out.respond::<Value>(
                    None,
                    Err(&ProtocolError::bad_request(format!(
                        "the line is longer than {MAX_LINE_BYTES} bytes"
                    ))),
                );
                return Flow::Continue;
            }
            Err(LineError::NotUtf8) => {
                self.out.respond::<Value>(
                    None,
                    Err(&ProtocolError::bad_request("the line is not valid UTF-8")),
                );
                return Flow::Continue;
            }
        };
        let request = match parse_request(&text) {
            Ok(request) => request,
            Err(refused) => {
                self.out.respond::<Value>(refused.id, Err(&refused.error));
                return Flow::Continue;
            }
        };
        let id = request.id;
        let Some(command) = Command::from_name(&request.cmd) else {
            self.out.respond::<Value>(
                Some(id),
                Err(&ProtocolError::new(
                    ErrorCode::UnknownCommand,
                    format!("unknown command \"{}\"", request.cmd),
                )),
            );
            return Flow::Continue;
        };
        let handled = catch_unwind(AssertUnwindSafe(|| self.dispatch(command, request.args)));
        let handled = handled.unwrap_or_else(|payload| {
            let text = panic_text(&*payload);
            log_error(&format!(
                "the handler of {} panicked: {text}",
                command.name()
            ));
            Err(ProtocolError::internal(format!(
                "the engine failed while handling {}: {text}",
                command.name()
            )))
        });
        match handled {
            Ok(Reply::Value(value)) => {
                self.out.respond(Some(id), Ok(&value));
                Flow::Continue
            }
            Ok(Reply::Job(value, pending)) => {
                self.start_job(id, value, pending);
                Flow::Continue
            }
            Ok(Reply::Shutdown) => {
                self.out.respond(Some(id), Ok(&Empty {}));
                Flow::Exit
            }
            Err(error) => {
                self.out.respond::<Value>(Some(id), Err(&error));
                Flow::Continue
            }
        }
    }

    fn dispatch(&mut self, command: Command, args: Map<String, Value>) -> Handled {
        match command {
            Command::Hello => {
                parse_args::<Empty>(args)?;
                self.hello()
            }
            Command::ListDisks => {
                parse_args::<Empty>(args)?;
                self.list_disks()
            }
            Command::OpenSource => self.open_source(parse_args(args)?),
            Command::CloseSource => {
                parse_args::<Empty>(args)?;
                self.close_source()
            }
            Command::ScanStart => self.scan_start(parse_args(args)?),
            Command::JobCancel => {
                parse_args::<Empty>(args)?;
                if let Some(job) = &self.job {
                    job.cancel.store(true, Ordering::SeqCst);
                }
                reply(&Empty {})
            }
            Command::SessionSave => self.session_save(parse_args(args)?),
            Command::SessionLoad => self.session_load(parse_args(args)?),
            Command::SessionAttachSource => self.session_attach(parse_args(args)?),
            Command::CatalogInfo => {
                parse_args::<Empty>(args)?;
                self.catalog_info()
            }
            Command::GeometrySet => self.geometry_set(parse_args(args)?),
            Command::InstanceSet => self.instance_set(parse_args(args)?),
            Command::ListChildren => self.list_children(parse_args(args)?),
            Command::NodeInfo => self.node_info(parse_args(args)?),
            Command::Search => self.search(parse_args(args)?),
            Command::SelectionSummary => self.selection_summary(parse_args(args)?),
            Command::CheckDestination => self.check_destination(parse_args(args)?),
            Command::RecoverStart => self.recover_start(parse_args(args)?),
            Command::CarveStart => self.carve_start(parse_args(args)?),
            Command::ImageStart => self.image_start(parse_args(args)?),
            Command::ReadFile => self.read_file(parse_args(args)?),
            Command::Shutdown => {
                parse_args::<Empty>(args)?;
                Ok(Reply::Shutdown)
            }
        }
    }

    // --------------------------------------------------------------------------------------
    // State checks
    // --------------------------------------------------------------------------------------

    fn job_kind(&self) -> Option<JobKind> {
        self.job.as_ref().map(|job| job.kind)
    }

    fn catalog(&self) -> Option<&CatalogView> {
        self.session.as_ref().and_then(|s| s.catalog.as_ref())
    }

    /// `busy` in S and J.
    fn not_busy(&self) -> Result<(), ProtocolError> {
        match self.job_kind() {
            Some(kind) => Err(ProtocolError::new(
                ErrorCode::Busy,
                format!(
                    "a {} job is running; wait for it or cancel it",
                    serde_json::to_value(kind)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default()
                ),
            )),
            None => Ok(()),
        }
    }

    fn source(&self) -> Result<&OpenedSource, ProtocolError> {
        self.source.as_ref().ok_or_else(|| {
            error(
                ErrorCode::NoSource,
                "no source is open (or the session's source is not attached)",
            )
        })
    }

    fn session(&self) -> Result<&SessionView, ProtocolError> {
        self.session.as_ref().ok_or_else(|| {
            error(
                ErrorCode::NoSession,
                "there is no session: scan or load one first",
            )
        })
    }

    /// `no_catalog` without a catalog (N, O, S).
    fn require_catalog(&self) -> Result<&CatalogView, ProtocolError> {
        if self.job_kind() == Some(JobKind::Scan) {
            return Err(error(
                ErrorCode::NoCatalog,
                "a scan is running; the catalog is built when it ends",
            ));
        }
        self.catalog().ok_or_else(|| {
            error(
                ErrorCode::NoCatalog,
                "there is no catalog: scan or load a session first",
            )
        })
    }

    /// `no_catalog`, then `stale_catalog`.
    fn require_epoch(&self, epoch: u64) -> Result<(), ProtocolError> {
        let current = self.require_catalog()?.epoch;
        if epoch != current {
            return Err(ProtocolError::new(
                ErrorCode::StaleCatalog,
                format!("epoch {epoch} is over; the current catalog epoch is {current}"),
            ));
        }
        Ok(())
    }

    fn instance(&self, id: u32) -> Result<InstanceKind, ProtocolError> {
        self.require_catalog()?
            .instances
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, kind)| *kind)
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::NotFound, format!("instance {id} does not exist"))
            })
    }

    // --------------------------------------------------------------------------------------
    // Epochs and the catalog mirror
    // --------------------------------------------------------------------------------------

    fn new_epoch(&mut self) -> u64 {
        self.last_epoch = self.last_epoch.saturating_add(1);
        self.last_epoch
    }

    /// Stamps `catalog` with a new epoch and records it.
    fn adopt_new_catalog(&mut self, catalog: &mut CatalogInfo) {
        catalog.epoch = self.new_epoch();
        if let Some(session) = &mut self.session {
            session.catalog = Some(CatalogView::of(catalog));
        }
    }

    /// Stamps `catalog` with the current epoch and records it.
    fn adopt_same_catalog(&mut self, catalog: &mut CatalogInfo) {
        let Some(session) = &mut self.session else {
            return;
        };
        match &session.catalog {
            Some(view) => {
                catalog.epoch = view.epoch;
                session.catalog = Some(CatalogView::of(catalog));
            }
            None => {
                // The backend has a catalog the engine did not know about:
                // that is a new node set.
                self.adopt_new_catalog(catalog);
            }
        }
    }

    // --------------------------------------------------------------------------------------
    // The destination guard
    // --------------------------------------------------------------------------------------

    /// The protected ranges of this moment (S2), session range first.
    fn protected(&self) -> Vec<Protected> {
        let mut protected = Vec::new();
        match &self.session {
            Some(session) => {
                protected.push(Protected::session(session.range.clone()));
                if let Some(catalog) = &session.catalog {
                    protected.extend(
                        catalog
                            .lost_volumes
                            .iter()
                            .cloned()
                            .map(Protected::lost_volume),
                    );
                }
            }
            None => {
                if let Some(source) = &self.source {
                    protected.push(Protected::session(0..source.source.size()));
                }
            }
        }
        protected
    }

    fn check(
        &self,
        path: &str,
        purpose: Purpose,
        protected: &[Protected],
        need_bytes: Option<u64>,
    ) -> CheckDestinationResult {
        let unattached = Unattached;
        let source: &dyn BlockSource = match &self.source {
            Some(opened) => &*opened.source,
            None => &unattached,
        };
        let check = destination_check(Path::new(path), purpose, source, protected, need_bytes);
        let (lost_files, lost_bytes) =
            if self.catalog().is_some() && !check.source_extents.is_empty() {
                self.backend.files_in_extents(&check.source_extents)
            } else {
                (0, 0)
            };
        CheckDestinationResult {
            check,
            lost_files,
            lost_bytes,
        }
    }

    /// Runs the guard and refuses with `destination_refused` (the check as
    /// `data`) unless the location may be written with the consents given.
    fn guard(
        &self,
        path: &str,
        purpose: Purpose,
        allow_same_disk: bool,
        allow_unknown_disk: bool,
        protected: Vec<Protected>,
    ) -> Result<Destination, ProtocolError> {
        let result = self.check(path, purpose, &protected, None);
        if !result.check.is_allowed(allow_same_disk, allow_unknown_disk) {
            let message = if result.check.message.is_empty() {
                format!("the location {path} cannot be used")
            } else {
                result.check.message.clone()
            };
            let data = serde_json::to_value(&result).unwrap_or(Value::Null);
            return Err(ProtocolError::new(ErrorCode::DestinationRefused, message).with_data(data));
        }
        Ok(Destination {
            path: path.to_string(),
            allow_same_disk,
            allow_unknown_disk,
            protected,
            check: result.check,
        })
    }

    // --------------------------------------------------------------------------------------
    // Commands without backend
    // --------------------------------------------------------------------------------------

    fn hello(&self) -> Handled {
        reply(&HelloResult {
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
            os: std::env::consts::OS.to_string(),
            elevated: hds_io::is_elevated(),
            raw_disk_support: cfg!(windows),
            carve_kinds: hds_carve::FileKind::all()
                .iter()
                .map(|k| k.name().to_string())
                .collect(),
        })
    }

    fn list_disks(&self) -> Handled {
        let disks = list_disks().into_iter().map(disk_list_entry).collect();
        reply(&ListDisksResult { disks })
    }

    fn open_source(&mut self, args: OpenSourceArgs) -> Handled {
        self.not_busy()?;
        validate::path("path", &args.path)?;
        let source = open_source(args.kind, Path::new(&args.path), args.sector_size)
            .map_err(|e| ProtocolError::from_io("cannot open the source", &e))?;
        let info = source.info();
        let (opened, table, warnings) = OpenedSource::new(source);
        let partitions = opened.partitions.clone();
        // The new source is open: now the old one, its session and catalog go.
        self.backend.detach_source();
        self.backend.close_session();
        self.session = None;
        self.source = Some(opened);
        log_info(&format!("opened {} ({} bytes)", info.path, info.size));
        reply(&OpenSourceResult {
            source: info,
            table,
            partitions,
            warnings,
        })
    }

    fn close_source(&mut self) -> Handled {
        self.not_busy()?;
        if self.source.take().is_some() {
            self.backend.detach_source();
        }
        reply(&Empty {})
    }

    fn check_destination(&self, args: CheckDestinationArgs) -> Handled {
        self.source()?;
        reply(&self.check(&args.path, args.purpose, &self.protected(), args.need_bytes))
    }

    // --------------------------------------------------------------------------------------
    // Scan and sessions
    // --------------------------------------------------------------------------------------

    fn scan_start(&mut self, args: ScanStartArgs) -> Handled {
        self.not_busy()?;
        let opened = self.source()?;
        let source = opened.source.clone();
        let partitions = opened.partitions.clone();
        let (range, collect, session_path) = if args.resume {
            let session = self.session()?;
            if args.start.is_some_and(|s| s != session.range.start)
                || args.end.is_some_and(|e| e != session.range.end)
            {
                return Err(ProtocolError::bad_request(format!(
                    "a resumed scan keeps the session range {}..{}",
                    session.range.start, session.range.end
                )));
            }
            if args
                .collect_carve_hits
                .is_some_and(|c| c != session.collect_carve_hits)
            {
                return Err(ProtocolError::bad_request(
                    "a resumed scan keeps the session's collect_carve_hits",
                ));
            }
            (
                session.range.clone(),
                session.collect_carve_hits,
                args.session_path.clone().or_else(|| session.path.clone()),
            )
        } else {
            let range = validate::byte_range(
                args.start_or_default(),
                args.end,
                source.size(),
                source.sector_size(),
            )?;
            (
                range,
                args.collect_carve_hits_or_default(),
                args.session_path.clone(),
            )
        };
        // A resumed range must fit the attached source like a new one.
        let range = validate::byte_range(
            range.start,
            Some(range.end),
            source.size(),
            source.sector_size(),
        )?;
        let session_dest = match &session_path {
            Some(path) => {
                validate::path("session_path", path)?;
                // The new range, plus what the current catalog knows of lost volumes.
                let mut protected = vec![Protected::session(range.clone())];
                if let Some(catalog) = self.catalog() {
                    protected.extend(
                        catalog
                            .lost_volumes
                            .iter()
                            .cloned()
                            .map(Protected::lost_volume),
                    );
                }
                Some(self.guard(
                    path,
                    Purpose::Session,
                    args.allow_same_disk,
                    args.allow_unknown_disk,
                    protected,
                )?)
            }
            None => None,
        };
        let request = ScanRequest {
            source: source.clone(),
            partitions,
            range: range.clone(),
            session: session_dest,
            resume: args.resume,
            collect_carve_hits: collect,
        };
        let job = self.backend.scan_start(request)?;
        let (result, pending) = erase(JobKind::Scan, session_path.clone(), job, Outcome::Scan);
        // Any scan_start ends the current epoch at once.
        let previous_size = self.session.as_ref().map(|s| s.source_size);
        self.session = Some(SessionView {
            range,
            collect_carve_hits: collect,
            path: session_path,
            source_size: if args.resume {
                previous_size.unwrap_or(source.size())
            } else {
                source.size()
            },
            catalog: None,
        });
        Ok(Reply::Job(serialize(&result)?, pending))
    }

    fn session_save(&mut self, args: SessionSaveArgs) -> Handled {
        self.not_busy()?;
        self.session()?;
        validate::path("path", &args.path)?;
        let dest = self.guard(
            &args.path,
            Purpose::Session,
            args.allow_same_disk,
            args.allow_unknown_disk,
            self.protected(),
        )?;
        let result = self.backend.session_save(dest)?;
        if let Some(session) = &mut self.session {
            session.path = Some(result.path.clone());
        }
        reply(&result)
    }

    fn session_load(&mut self, args: SessionLoadArgs) -> Handled {
        self.not_busy()?;
        validate::path("path", &args.path)?;
        if let Some(source) = &args.source {
            validate::path("source.path", &source.path)?;
        }
        let task = TaskContext::new(self.out.clone(), TaskName::SessionLoad);
        let loaded = self.backend.session_load(&args, &task)?;
        let mut result = loaded.result;
        let attached = loaded.source.is_some();
        if result.source_attached != attached {
            log_warn(
                "session_load: the backend's source_attached disagrees with the source it returned",
            );
        }
        result.source_attached = attached;
        result.catalog.source_attached = attached;
        if attached {
            result.source_error = None;
        } else if result.source_error.is_none() {
            result.source_error = Some(error(ErrorCode::NoSource, "no matching source was found"));
        }
        // The old source goes; the loaded session's (if any) takes its place.
        self.source = None;
        if let Some(source) = loaded.source {
            result.source = source.info();
            self.source = Some(OpenedSource::new(source).0);
        }
        self.session = Some(SessionView {
            range: result.catalog.range.range(),
            collect_carve_hits: loaded.collect_carve_hits,
            path: Some(args.path.clone()),
            source_size: result.source.size,
            catalog: None,
        });
        self.adopt_new_catalog(&mut result.catalog);
        reply(&result)
    }

    fn session_attach(&mut self, args: SessionAttachArgs) -> Handled {
        self.not_busy()?;
        self.session()?;
        validate::path("path", &args.path)?;
        // An image is read with the sector size the session was scanned
        // with; a disk reports its own.
        let sector_size = match args.kind {
            SourceKind::Image => self.backend.session_sector_size(),
            SourceKind::Disk => None,
        };
        let source = open_source(args.kind, Path::new(&args.path), sector_size)
            .map_err(|e| ProtocolError::from_io("cannot open the source", &e))?;
        let mut result = self
            .backend
            .session_attach_source(source.clone(), args.force)?;
        result.source = source.info();
        result.catalog.source_attached = true;
        self.source = Some(OpenedSource::new(source).0);
        if let Some(session) = &mut self.session {
            session.source_size = result.source.size;
        }
        self.adopt_new_catalog(&mut result.catalog);
        reply(&result)
    }

    // --------------------------------------------------------------------------------------
    // Catalog commands
    // --------------------------------------------------------------------------------------

    fn catalog_info(&mut self) -> Handled {
        self.require_catalog()?;
        let mut catalog = self.backend.catalog_info()?;
        self.adopt_same_catalog(&mut catalog);
        reply(&catalog)
    }

    fn geometry_set(&mut self, args: GeometrySetArgs) -> Handled {
        self.not_busy()?;
        self.require_catalog()?;
        self.instance(args.instance)?;
        let size = self.session()?.source_size;
        validate::geometry(args.cluster_size, args.volume_offset, size)?;
        let task = TaskContext::new(self.out.clone(), TaskName::GeometrySet);
        let mut catalog = self
            .backend
            .geometry_set(&args, &task)
            .map_err(|e| self.after_failed_change(e))?;
        self.adopt_same_catalog(&mut catalog);
        reply(&catalog)
    }

    fn instance_set(&mut self, args: InstanceSetArgs) -> Handled {
        self.not_busy()?;
        self.require_catalog()?;
        if self.instance(args.instance)? == InstanceKind::Strays {
            return Err(ProtocolError::bad_request(
                "the stray records cannot be made a file system",
            ));
        }
        let mut catalog = self
            .backend
            .instance_set(&args)
            .map_err(|e| self.after_failed_change(e))?;
        self.adopt_new_catalog(&mut catalog);
        reply(&catalog)
    }

    /// A failed `geometry_set` or `instance_set` whose error data says
    /// `catalog_reloaded`: the backend could not put its previous catalog
    /// back and rebuilt it from the session, so the node set may differ and
    /// a new epoch starts (PROTOCOL.md "Catalog epochs"). The new epoch is
    /// added to the error's data; without a catalog the session has none.
    fn after_failed_change(&mut self, mut error: ProtocolError) -> ProtocolError {
        let reloaded = error
            .data
            .as_ref()
            .and_then(|d| d.get(crate::real_backend::CATALOG_RELOADED))
            .and_then(Value::as_bool)
            == Some(true);
        if !reloaded {
            return error;
        }
        match self.backend.catalog_info() {
            Ok(mut catalog) => {
                self.adopt_new_catalog(&mut catalog);
                if let Some(Value::Object(data)) = &mut error.data {
                    data.insert("epoch".to_string(), Value::from(catalog.epoch));
                }
            }
            Err(_) => {
                if let Some(session) = &mut self.session {
                    session.catalog = None;
                }
            }
        }
        error
    }

    fn list_children(&self, mut args: ListChildrenArgs) -> Handled {
        self.require_epoch(args.epoch)?;
        validate::limit(args.limit)?;
        validate::exts("exts", &mut args.exts)?;
        validate::non_empty("ratings", &args.ratings)?;
        reply(&self.backend.list_children(&args)?)
    }

    fn node_info(&self, args: NodeInfoArgs) -> Handled {
        self.require_epoch(args.epoch)?;
        reply(&self.backend.node_info(&args)?)
    }

    fn search(&self, mut args: SearchArgs) -> Handled {
        self.require_epoch(args.epoch)?;
        validate::limit(args.limit)?;
        validate::filter("filter", &mut args.filter)?;
        reply(&self.backend.search(&args)?)
    }

    fn selection_summary(&self, mut args: SelectionArgs) -> Handled {
        self.require_epoch(args.epoch)?;
        validate::selection(&args.include, &args.exclude, false)?;
        if let Some(filter) = &mut args.filter {
            validate::filter("filter", filter)?;
        }
        reply(&self.backend.selection_summary(&args)?)
    }

    fn read_file(&self, args: ReadFileArgs) -> Handled {
        self.require_catalog()?;
        self.source()?;
        self.require_epoch(args.epoch)?;
        validate::read_length(args.length)?;
        let mut bytes = self.backend.read_file(&args)?;
        let length = usize::try_from(args.length).unwrap_or(usize::MAX);
        if bytes.data.len() > length {
            log_warn("read_file: the backend returned more bytes than requested");
            bytes.data.truncate(length);
            bytes.eof = false;
        }
        let kind = if args.offset == 0 {
            hds_carve::sniff(&bytes.data[..bytes.data.len().min(hds_carve::SNIFF_LEN)])
                .map(|k| k.name().to_string())
        } else {
            None
        };
        reply(&ReadFileResult {
            data_b64: crate::base64::encode(&bytes.data),
            size: bytes.size,
            eof: bytes.eof,
            kind,
        })
    }

    // --------------------------------------------------------------------------------------
    // Jobs that need a catalog
    // --------------------------------------------------------------------------------------

    /// The state checks of `recover_start` and `carve_start`: S `no_catalog`,
    /// J `busy`, no catalog `no_catalog`, B `no_source`.
    fn catalog_job_allowed(&self) -> Result<(), ProtocolError> {
        match self.job_kind() {
            Some(JobKind::Scan) => self.require_catalog().map(|_| ()),
            Some(_) => self.not_busy(),
            None => {
                self.require_catalog()?;
                self.source()?;
                Ok(())
            }
        }
    }

    fn recover_start(&mut self, mut args: RecoverStartArgs) -> Handled {
        self.catalog_job_allowed()?;
        self.require_epoch(args.epoch)?;
        validate::selection(&args.include, &args.exclude, true)?;
        if let Some(filter) = &mut args.filter {
            validate::filter("filter", filter)?;
        }
        validate::path("dest", &args.dest)?;
        let dest = self.guard(
            &args.dest,
            Purpose::Recover,
            args.allow_same_disk,
            args.allow_unknown_disk,
            self.protected(),
        )?;
        let label = Some(args.dest.clone());
        let job = self.backend.recover_start(RecoverRequest { args, dest })?;
        let (result, pending) = erase(JobKind::Recover, label, job, Outcome::Recover);
        Ok(Reply::Job(serialize(&result)?, pending))
    }

    fn carve_start(&mut self, args: CarveStartArgs) -> Handled {
        self.catalog_job_allowed()?;
        let kinds = validate::carve_kinds(&args.kinds)?;
        if !self.session()?.collect_carve_hits {
            return Err(error(
                ErrorCode::Unsupported,
                "the scan did not collect signature hits; scan again with collect_carve_hits to carve",
            ));
        }
        let job = self.backend.carve_start(CarveRequest { kinds })?;
        let (result, pending) = erase(JobKind::Carve, None, job, Outcome::Carve);
        Ok(Reply::Job(serialize(&result)?, pending))
    }

    fn image_start(&mut self, args: ImageStartArgs) -> Handled {
        self.not_busy()?;
        let source = self.source()?.source.clone();
        let range =
            validate::byte_range(args.start, args.end, source.size(), source.sector_size())?;
        validate::path("dest", &args.dest)?;
        let mut protected = self.protected();
        protected.push(Protected::session(range.clone()));
        let dest = self.guard(
            &args.dest,
            Purpose::Image,
            args.allow_same_disk,
            args.allow_unknown_disk,
            protected,
        )?;
        let label = Some(args.dest.clone());
        let job = self.backend.image_start(ImageRequest {
            source,
            range,
            dest,
            resume: args.resume,
        })?;
        let (result, pending) = erase(JobKind::Image, label, job, Outcome::Image);
        Ok(Reply::Job(serialize(&result)?, pending))
    }

    // --------------------------------------------------------------------------------------
    // The job manager
    // --------------------------------------------------------------------------------------

    /// Starts `pending` on a worker thread that waits until the response is
    /// written, so that no event of the job can precede it.
    fn start_job(&mut self, id: i64, result: Value, pending: PendingJob) {
        self.last_job_id = self.last_job_id.wrapping_add(1);
        let job_id = self.last_job_id;
        let cancel = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (go_tx, go_rx) = mpsc::channel::<bool>();
        let inbox = self.inbox_tx.clone();
        let out = self.out.clone();
        let (thread_cancel, thread_shutdown) = (cancel.clone(), shutdown.clone());
        let run = pending.run;
        let spawned = thread::Builder::new()
            .name(format!("hds-job-{job_id}"))
            .spawn(move || {
                if go_rx.recv() != Ok(true) {
                    return;
                }
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run(&out, &thread_cancel, &thread_shutdown)
                }))
                .map_err(|payload| {
                    let text = panic_text(&*payload);
                    log_error(&format!("job {job_id} panicked: {text}"));
                    ProtocolError::internal(format!("the job failed: {text}"))
                });
                // The engine may be gone only if it is exiting without waiting, which it never does.
                let finished = JobFinished {
                    id: job_id,
                    outcome,
                };
                let _ = inbox.send(Input::JobFinished(Box::new(finished)));
            });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(e) => {
                log_error(&format!("cannot start a job thread: {e}"));
                self.out.respond::<Value>(
                    Some(id),
                    Err(&ProtocolError::internal(format!(
                        "cannot start the job: {e}"
                    ))),
                );
                // A scan has already ended the epoch; the backend has no job.
                return;
            }
        };
        self.job = Some(RunningJob {
            id: job_id,
            kind: pending.kind,
            cancel,
            shutdown,
            thread,
            label: pending.label,
        });
        self.out.respond(Some(id), Ok(&result));
        // The thread waits for this; it cannot have gone.
        let _ = go_tx.send(true);
    }

    /// Frees the job slot, puts the resulting state in place and writes the
    /// job's `*_done` event, in that order.
    fn finish_job(&mut self, finished: JobFinished) {
        let Some(job) = self.job.take_if(|job| job.id == finished.id) else {
            log_warn(&format!(
                "completion of unknown job {} ignored",
                finished.id
            ));
            return;
        };
        if job.thread.join().is_err() {
            log_error("a job thread ended abnormally");
        }
        let cancelled = job.cancel.load(Ordering::SeqCst);
        let outcome = finished
            .outcome
            .unwrap_or_else(|error| failed_outcome(job.kind, error, job.label.clone()));
        match outcome {
            Outcome::Scan(mut done) => {
                if let Some(path) = &done.session_path {
                    if let Some(session) = &mut self.session {
                        session.path = Some(path.clone());
                    }
                }
                if done.status == ScanStatus::Failed {
                    done.catalog = None;
                }
                match &mut done.catalog {
                    Some(catalog) => {
                        catalog.source_attached = self.source.is_some();
                        self.adopt_new_catalog(catalog);
                    }
                    None => {
                        if let Some(session) = &mut self.session {
                            session.catalog = None;
                        }
                    }
                }
                self.out.event(&done);
            }
            Outcome::Carve(mut done) => {
                if let Some(catalog) = &mut done.catalog {
                    self.adopt_same_catalog(catalog);
                }
                self.out.event(&done);
            }
            Outcome::Recover(done) => self.out.event(&done),
            Outcome::Image(done) => self.out.event(&done),
        }
        if cancelled {
            log_info("the cancelled job has ended");
        }
    }
}

fn serialize<T: Serialize>(value: &T) -> Result<Value, ProtocolError> {
    serde_json::to_value(value)
        .map_err(|e| ProtocolError::internal(format!("cannot serialize the result: {e}")))
}

/// The `*_done` of a job whose run function panicked.
fn failed_outcome(kind: JobKind, error: ProtocolError, label: Option<String>) -> Outcome {
    match kind {
        JobKind::Scan => Outcome::Scan(ScanDone {
            status: ScanStatus::Failed,
            error: Some(error),
            session_path: label,
            catalog: None,
        }),
        JobKind::Recover => Outcome::Recover(RecoverDone {
            status: JobStatus::Failed,
            error: Some(error),
            dest: label.unwrap_or_default(),
            ..RecoverDone::default()
        }),
        JobKind::Carve => Outcome::Carve(CarveDone {
            status: JobStatus::Failed,
            error: Some(error),
            ..CarveDone::default()
        }),
        JobKind::Image => Outcome::Image(ImageDone {
            status: JobStatus::Failed,
            error: Some(error),
            path: label.unwrap_or_default(),
            ..ImageDone::default()
        }),
    }
}

/// A `list_disks` entry as PROTOCOL.md describes it ("The disk list is made
/// without reading a single sector"): partitions are the volumes Windows
/// exposes, with empty `type` and `type_id` and no `ntfs` summary, in
/// `index` order.
pub fn disk_list_entry(mut disk: DiskInfo) -> DiskInfo {
    for partition in &mut disk.partitions {
        partition.type_name.clear();
        partition.type_id.clear();
        partition.ntfs = None;
    }
    disk.partitions.sort_by_key(|p| p.index);
    disk
}
