//! The scanner (ALGORITHMS A1 to A8).
//!
//! [`scan`] reads the session range of a [`RangedSource`] in 8 MiB
//! sector-aligned chunks through `hds_io::read_resilient`: first the probe
//! regions (where an `$MFT` is expected), then everything not yet done in
//! ascending order. At every 512-byte offset of a chunk it looks for a
//! `FILE` record, an `INDX` block, an NTFS boot sector and (optionally) the
//! start of a file of a known type, and appends what it finds to the
//! session's [`ScanStore`]. Structures that cross a chunk end are read on
//! their own.
//!
//! # Threads
//!
//! The caller's thread parses; one reader thread, started for the
//! duration of the call, reads ahead into a small pool of aligned buffers
//! (three by default). Chunks are committed strictly in the order they were
//! planned, and the planner decides on committed state only, so the result
//! does not depend on thread timing.
//!
//! # A chunk is committed whole or not at all
//!
//! Only after a chunk has been read and parsed completely - including the
//! separate reads of structures crossing its end - are its findings kept,
//! its range added to the done set and its unreadable sectors to the bad
//! ranges. A device that stops answering (safety invariant S7) or a
//! cancellation in the middle of a chunk leaves no trace of that chunk: a
//! resumed scan reads it again and ends up with the same store as a scan
//! that was never interrupted.
//!
//! # Session files
//!
//! When the session has a file, a checkpoint is written every 5 minutes
//! and at the end, whatever the end is (completed, stopped, source lost,
//! failed). A checkpoint that cannot be written does not stop the scan;
//! the error is reported to the observer and in the summary.

#![deny(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_used
)]

mod parse;
mod plan;
mod progress;

#[cfg(test)]
pub(crate) mod tests;

use std::collections::VecDeque;
use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use hds_io::{read_resilient, AlignedBuf, RangedSource, ReadOutcome, Warning};
use hds_ntfs::{DEFAULT_MAX_RECORD_SIZE, MAX_RECORD_SIZE, MIN_RECORD_SIZE};
use serde::Serialize;

use crate::session::{Session, SessionError};
use crate::store::{ScanCounters, ScanStore, ZeroCheck};

pub use plan::CHUNK_SIZE;

/// Warning code: the first gigabyte read held no non-zero byte (A7).
pub const WARN_SOURCE_READS_ZERO: &str = "source_reads_zero";
/// Warning code: the cap on file-signature hits was reached (A3.4).
pub const WARN_CARVE_HITS_CAPPED: &str = "carve_hits_capped";

/// At most this many file-signature hits are kept (A3.4).
pub const CARVE_HIT_CAP: usize = 64_000_000;

/// The dead-source check looks at the first this many bytes read (A7).
pub const ZERO_CHECK_BYTES: u64 = 1 << 30;

const READS_ZERO_MESSAGE: &str = "The first gigabyte read from this disk contains nothing but \
     zeros. The disk probably discarded its data when it was formatted (TRIM, common on SSDs and \
     many SMR hard disks), or it was fully formatted; little or nothing may be recoverable.";
const CARVE_CAPPED_MESSAGE: &str = "More than 64 million file signatures were found. Further \
     ones are not kept, so searching for files by signature may miss files in the rest of the \
     disk.";

/// The options of `scan_start` that concern the scan. They are part of the
/// session and cannot change when it is resumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScanOptions {
    /// Collect file-signature hits for carving (`collect_carve_hits`).
    pub collect_carve_hits: bool,
    /// Largest `FILE` record accepted: a power of two from 1024 to 65536,
    /// 4096 by default (ARCHITECTURE 5.1).
    pub max_record_size: u32,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            collect_carve_hits: true,
            max_record_size: DEFAULT_MAX_RECORD_SIZE,
        }
    }
}

impl ScanOptions {
    /// Whether the values are acceptable.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.max_record_size.is_power_of_two()
            && (MIN_RECORD_SIZE..=MAX_RECORD_SIZE).contains(&self.max_record_size)
    }
}

/// Knobs with the contract's values as defaults. Tests shorten them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanTuning {
    /// Interval between checkpoints of the session file: 5 minutes.
    pub checkpoint_interval: Duration,
    /// Cap on file-signature hits: [`CARVE_HIT_CAP`].
    pub carve_hit_cap: usize,
    /// Interval between progress events: 250 ms (never less).
    pub progress_interval: Duration,
    /// Chunks read ahead of the parser: 3 (1 to 8).
    pub read_ahead: usize,
}

impl Default for ScanTuning {
    fn default() -> Self {
        ScanTuning {
            checkpoint_interval: Duration::from_secs(300),
            carve_hit_cap: CARVE_HIT_CAP,
            progress_interval: progress::MIN_INTERVAL,
            read_ahead: 3,
        }
    }
}

/// Phase of a scan, as reported in `scan_progress`. Wire form:
/// `"mft_probe"`, `"sweep"` (the engine adds `"analyzing"` for the catalog
/// build).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanPhase {
    /// Reading the regions where an `$MFT` is expected.
    MftProbe,
    /// Reading everything else in ascending order.
    Sweep,
}

/// The data of a `scan_progress` event (member names as in PROTOCOL.md).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScanProgress {
    /// Current phase.
    pub phase: ScanPhase,
    /// Size of the done set (so a resumed scan starts where it stopped).
    pub bytes_done: u64,
    /// Length of the session range.
    pub bytes_total: u64,
    /// Absolute offset of the last chunk read.
    pub position: u64,
    /// Bytes per second averaged over the last 5 s.
    pub speed_bps: u64,
    /// Remaining bytes over speed, rounded up; `None` at speed 0.
    pub eta_seconds: Option<u64>,
    /// Whole seconds of this run.
    pub elapsed_seconds: u64,
    /// Stored records.
    pub records: u64,
    /// In-use base records without the directory flag.
    pub files: u64,
    /// In-use base records with the directory flag.
    pub dirs: u64,
    /// Stored `INDX` blocks.
    pub index_blocks: u64,
    /// Stored file-signature hits.
    pub carve_hits: u64,
    /// Bytes in bad ranges (a multiple of the logical sector size).
    pub bad_bytes: u64,
    /// Every warning raised so far.
    pub warnings: Vec<Warning>,
}

/// Receives what a scan has to tell while it runs. Called on the scan's
/// thread.
pub trait ScanObserver {
    /// A progress event (at most 4 per second).
    fn progress(&mut self, progress: &ScanProgress);

    /// A warning was raised for the first time.
    fn warning(&mut self, warning: &Warning) {
        let _ = warning;
    }

    /// A checkpoint of the session file failed; the scan goes on and tries
    /// again at the next checkpoint.
    fn checkpoint_failed(&mut self, error: &SessionError) {
        let _ = error;
    }
}

/// An observer that ignores everything.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoObserver;

impl ScanObserver for NoObserver {
    fn progress(&mut self, _progress: &ScanProgress) {}
}

/// Why a scan failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanFailure {
    /// The source stopped answering (S7). The chunk being read was recorded
    /// neither as done nor as bad; the session can be resumed after the
    /// source is attached again. Protocol error `source_lost`.
    SourceLost(String),
    /// A read was refused for a reason that is neither the medium nor the
    /// device (a bug), or the session state is inconsistent. Protocol error
    /// `internal`.
    Internal(String),
    /// The source's range or the options differ from the session's.
    /// Protocol error `bad_request`.
    Mismatch(String),
}

impl ScanFailure {
    /// The protocol error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ScanFailure::SourceLost(_) => "source_lost",
            ScanFailure::Internal(_) => "internal",
            ScanFailure::Mismatch(_) => "bad_request",
        }
    }

    /// Plain-language explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            ScanFailure::SourceLost(m) | ScanFailure::Internal(m) | ScanFailure::Mismatch(m) => m,
        }
    }
}

/// How a scan ended (`scan_done.status`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanStatus {
    /// The whole range is done.
    Completed,
    /// Cancelled: the store holds what was committed before.
    Stopped,
    /// Ended by an error.
    Failed(ScanFailure),
}

/// The outcome of [`scan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanSummary {
    /// How the scan ended.
    pub status: ScanStatus,
    /// Size of the done set.
    pub bytes_done: u64,
    /// Length of the session range.
    pub bytes_total: u64,
    /// Bytes this run read and committed.
    pub bytes_read: u64,
    /// Whole seconds of this run.
    pub elapsed_seconds: u64,
    /// The store's counters at the end.
    pub counters: ScanCounters,
    /// Every warning of the store.
    pub warnings: Vec<Warning>,
    /// The last checkpoint that failed, if the session file could not be
    /// brought up to date at the end (`None` when the final checkpoint
    /// succeeded or the session has no file).
    pub checkpoint_error: Option<String>,
}

/// Why the work on a chunk stopped.
#[derive(Debug)]
pub(crate) enum Abort {
    SourceLost(io::Error),
    Cancelled,
    Internal(io::Error),
}

impl Abort {
    fn into_status(self) -> ScanStatus {
        match self {
            Abort::SourceLost(e) => ScanStatus::Failed(ScanFailure::SourceLost(format!(
                "The source stopped answering: {e}"
            ))),
            Abort::Cancelled => ScanStatus::Stopped,
            Abort::Internal(e) => {
                ScanStatus::Failed(ScanFailure::Internal(format!("A read failed: {e}")))
            }
        }
    }
}

struct ReadRequest {
    range: Range<u64>,
    buf: AlignedBuf,
}

struct ReadResult {
    range: Range<u64>,
    buf: AlignedBuf,
    outcome: ReadOutcome,
}

/// The reader thread: reads every requested chunk with `read_resilient`.
fn reader(
    source: &RangedSource,
    cancel: &AtomicBool,
    requests: Receiver<ReadRequest>,
    results: SyncSender<ReadResult>,
) {
    for ReadRequest { range, mut buf } in requests {
        let len = usize::try_from(range.end.saturating_sub(range.start)).unwrap_or(usize::MAX);
        let outcome = match buf.get_mut(..len) {
            Some(slice) => read_resilient(source, range.start, slice, cancel),
            None => ReadOutcome::Internal(io::Error::other("chunk larger than its buffer")),
        };
        if results
            .send(ReadResult {
                range,
                buf,
                outcome,
            })
            .is_err()
        {
            break;
        }
    }
}

/// Scans the session range of `source` into `session` with the default
/// [`ScanTuning`]. See [`scan_with`].
pub fn scan(
    source: &RangedSource,
    options: &ScanOptions,
    session: &mut Session,
    observer: &mut dyn ScanObserver,
    cancel: &AtomicBool,
) -> ScanSummary {
    scan_with(
        source,
        options,
        session,
        observer,
        cancel,
        &ScanTuning::default(),
    )
}

/// Scans the session range of `source` into `session`.
///
/// `source.range()` and `options` must be the session's; the scan skips
/// whatever the session's done set already holds, so calling this on a
/// loaded session resumes it, and on a complete session returns
/// [`ScanStatus::Completed`] at once. `cancel` is polled before every chunk
/// and between the retries of a failing read; the scan then ends with
/// [`ScanStatus::Stopped`]. Synchronous: the engine runs it on a worker
/// thread.
pub fn scan_with(
    source: &RangedSource,
    options: &ScanOptions,
    session: &mut Session,
    observer: &mut dyn ScanObserver,
    cancel: &AtomicBool,
    tuning: &ScanTuning,
) -> ScanSummary {
    let started = Instant::now();
    let range = source.range();
    let mismatch = if !options.is_valid() {
        Some("the maximum record size must be a power of two from 1024 to 65536".to_string())
    } else if session.meta().range != range {
        Some(format!(
            "the source range {}..{} is not the session range {}..{}",
            range.start,
            range.end,
            session.meta().range.start,
            session.meta().range.end
        ))
    } else if session.meta().options != *options {
        Some("the scan options differ from the session's".to_string())
    } else {
        None
    };
    let mut run = Run {
        source,
        options: *options,
        tuning: tuning.clone(),
        cancel,
        meter: progress::Meter::new(started, tuning.progress_interval),
        phase: ScanPhase::MftProbe,
        position: range.start,
        bytes_read: 0,
        last_checkpoint: started,
        checkpoint_error: None,
    };
    let refused = mismatch.is_some();
    let status = match mismatch {
        Some(message) => ScanStatus::Failed(ScanFailure::Mismatch(message)),
        None => run.execute(session, observer),
    };
    // A final checkpoint whatever the end (A6, B3) - unless the request was
    // refused before anything was read.
    if session.has_file() && !refused {
        match session.checkpoint() {
            Ok(()) => run.checkpoint_error = None,
            Err(e) => {
                observer.checkpoint_failed(&e);
                run.checkpoint_error = Some(e.to_string());
            }
        }
    }
    let now = Instant::now();
    let store = session.store();
    ScanSummary {
        status,
        bytes_done: store.done().total_len(),
        bytes_total: range.end.saturating_sub(range.start),
        bytes_read: run.bytes_read,
        elapsed_seconds: now.saturating_duration_since(started).as_secs(),
        counters: store.counters(),
        warnings: store.warnings().to_vec(),
        checkpoint_error: run.checkpoint_error,
    }
}

/// The state of one call of [`scan_with`].
struct Run<'a> {
    source: &'a RangedSource,
    options: ScanOptions,
    tuning: ScanTuning,
    cancel: &'a AtomicBool,
    meter: progress::Meter,
    phase: ScanPhase,
    position: u64,
    bytes_read: u64,
    last_checkpoint: Instant,
    checkpoint_error: Option<String>,
}

impl Run<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn execute(&mut self, session: &mut Session, observer: &mut dyn ScanObserver) -> ScanStatus {
        let range = self.source.range();
        if session.store().done().contains_range(range.clone()) {
            return ScanStatus::Completed;
        }
        if self.cancelled() {
            return ScanStatus::Stopped;
        }
        let probes = match plan::probe_offsets(self.source, self.cancel) {
            Ok(probes) => probes,
            Err(abort) => return abort.into_status(),
        };
        let mut planner = plan::Planner::new(range, session.store().done(), &probes);
        let depth = self.tuning.read_ahead.clamp(1, 8);
        let source = self.source;
        let cancel = self.cancel;
        std::thread::scope(|scope| {
            let (request_tx, request_rx) = mpsc::sync_channel::<ReadRequest>(depth);
            let (result_tx, result_rx) = mpsc::sync_channel::<ReadResult>(depth);
            scope.spawn(move || reader(source, cancel, request_rx, result_tx));
            let status = self.pump(
                session,
                observer,
                &mut planner,
                depth,
                &request_tx,
                &result_rx,
            );
            // Ending both channels lets the reader finish its current read
            // and stop; the scope then joins it.
            drop(request_tx);
            drop(result_rx);
            status
        })
    }

    /// Keeps the reader busy and commits the chunks it returns, in order.
    fn pump(
        &mut self,
        session: &mut Session,
        observer: &mut dyn ScanObserver,
        planner: &mut plan::Planner,
        depth: usize,
        requests: &SyncSender<ReadRequest>,
        results: &Receiver<ReadResult>,
    ) -> ScanStatus {
        let chunk_len = usize::try_from(CHUNK_SIZE).unwrap_or(usize::MAX);
        let mut free: Vec<AlignedBuf> = (0..depth).map(|_| AlignedBuf::new(chunk_len)).collect();
        let mut in_flight: VecDeque<plan::Planned> = VecDeque::new();
        let mut parser = parse::ChunkParser::new(
            self.source,
            self.cancel,
            self.options.max_record_size,
            self.options.collect_carve_hits,
            self.tuning.carve_hit_cap,
        );
        loop {
            // Plan and request as far ahead as the buffers allow.
            while !self.cancelled() {
                let Some(buf) = free.pop() else { break };
                let Some(planned) = planner.next() else {
                    free.push(buf);
                    break;
                };
                if requests
                    .send(ReadRequest {
                        range: planned.range.clone(),
                        buf,
                    })
                    .is_err()
                {
                    return ScanStatus::Failed(ScanFailure::Internal(
                        "the reader thread ended unexpectedly".to_string(),
                    ));
                }
                in_flight.push_back(planned);
            }
            if self.cancelled() {
                return ScanStatus::Stopped;
            }
            let Some(planned) = in_flight.pop_front() else {
                return ScanStatus::Completed;
            };
            let result = loop {
                match results.recv_timeout(self.meter_interval()) {
                    Ok(result) => break result,
                    Err(RecvTimeoutError::Timeout) => self.report(session.store(), observer),
                    Err(RecvTimeoutError::Disconnected) => {
                        return ScanStatus::Failed(ScanFailure::Internal(
                            "the reader thread ended unexpectedly".to_string(),
                        ))
                    }
                }
            };
            if result.range != planned.range {
                return ScanStatus::Failed(ScanFailure::Internal(
                    "the reader returned chunks out of order".to_string(),
                ));
            }
            let ReadResult {
                range,
                buf,
                outcome,
            } = result;
            let bad = match outcome {
                ReadOutcome::Data { bad } => bad,
                ReadOutcome::SourceLost(e) => return Abort::SourceLost(e).into_status(),
                ReadOutcome::Cancelled => return ScanStatus::Stopped,
                ReadOutcome::Internal(e) => return Abort::Internal(e).into_status(),
            };
            let len = usize::try_from(range.end.saturating_sub(range.start)).unwrap_or(usize::MAX);
            let data = buf.get(..len).unwrap_or(&[]);
            if let Err(abort) = self.commit(
                session,
                observer,
                planner,
                &mut parser,
                &planned,
                data,
                &bad,
            ) {
                return abort.into_status();
            }
            free.push(buf);
            if self.cancelled() {
                return ScanStatus::Stopped;
            }
            self.maybe_checkpoint(session, observer);
            self.report(session.store(), observer);
        }
    }

    fn meter_interval(&self) -> Duration {
        self.tuning.progress_interval.max(progress::MIN_INTERVAL)
    }

    /// Parses a chunk and keeps its findings, or nothing of it on an error.
    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        session: &mut Session,
        observer: &mut dyn ScanObserver,
        planner: &mut plan::Planner,
        parser: &mut parse::ChunkParser<'_>,
        planned: &plan::Planned,
        data: &[u8],
        bad: &[Range<u64>],
    ) -> Result<(), Abort> {
        let range = planned.range.clone();
        let store = session.store_mut();
        let mark = store.mark();
        let outcome = match parser.parse(store, range.start, data) {
            Ok(outcome) => outcome,
            Err(abort) => {
                store.rollback(mark);
                return Err(abort);
            }
        };
        // From here on the chunk counts.
        store.drops.add(&outcome.drops);
        for r in bad {
            store.add_bad(r.clone());
        }
        store.done.insert(range.clone());
        store.invalidate();
        let mut raised = Vec::new();
        if outcome.carve_capped && store.raise_warning(WARN_CARVE_HITS_CAPPED, CARVE_CAPPED_MESSAGE)
        {
            raised.push(WARN_CARVE_HITS_CAPPED);
        }
        if zero_check(store, self.source.range(), range.clone(), data, bad) {
            raised.push(WARN_SOURCE_READS_ZERO);
        }
        for code in raised {
            if let Some(warning) = store.warnings().iter().find(|w| w.code == code) {
                observer.warning(warning);
            }
        }
        if let Some(probe) = planned.probe {
            if outcome.drops.file_hits > 0 {
                planner.probe_hit(probe, range.end);
            }
        }
        self.phase = planned.phase;
        self.position = range.start;
        self.bytes_read = self
            .bytes_read
            .saturating_add(range.end.saturating_sub(range.start));
        self.meter.sample(Instant::now(), self.bytes_read);
        Ok(())
    }

    fn maybe_checkpoint(&mut self, session: &mut Session, observer: &mut dyn ScanObserver) {
        if !session.has_file() {
            return;
        }
        let now = Instant::now();
        if now.saturating_duration_since(self.last_checkpoint) < self.tuning.checkpoint_interval {
            return;
        }
        self.last_checkpoint = now;
        match session.checkpoint() {
            Ok(()) => self.checkpoint_error = None,
            Err(e) => {
                observer.checkpoint_failed(&e);
                self.checkpoint_error = Some(e.to_string());
            }
        }
    }

    /// Sends a progress event if one is due.
    fn report(&mut self, store: &ScanStore, observer: &mut dyn ScanObserver) {
        let now = Instant::now();
        if !self.meter.due(now) {
            return;
        }
        self.meter.emitted(now);
        let range = self.source.range();
        let total = range.end.saturating_sub(range.start);
        let done = store.done().total_len();
        let speed = self.meter.speed(now);
        let counters = store.counters();
        observer.progress(&ScanProgress {
            phase: self.phase,
            bytes_done: done,
            bytes_total: total,
            position: self.position,
            speed_bps: speed,
            eta_seconds: progress::eta_seconds(total.saturating_sub(done), speed),
            elapsed_seconds: self.meter.elapsed_seconds(now),
            records: counters.records,
            files: counters.files,
            dirs: counters.dirs,
            index_blocks: counters.index_blocks,
            carve_hits: counters.carve_hits,
            bad_bytes: counters.bad_bytes,
            warnings: store.warnings().to_vec(),
        });
    }
}

/// Whether `data` holds a non-zero byte (eight bytes at a time).
fn has_non_zero(data: &[u8]) -> bool {
    let (words, rest) = data.as_chunks::<8>();
    words.iter().any(|w| u64::from_ne_bytes(*w) != 0) || rest.iter().any(|&b| b != 0)
}

/// ALGORITHMS A7 for one committed chunk (already in the done set). Only
/// successfully read bytes count: bad sectors are zero-filled, which says
/// nothing about the medium. The warning is raised once 1 GiB of read bytes
/// held nothing but zeros, or, for a smaller range, once all of it has been
/// read. Returns true when the warning is raised now.
fn zero_check(
    store: &mut ScanStore,
    session_range: Range<u64>,
    chunk: Range<u64>,
    data: &[u8],
    bad: &[Range<u64>],
) -> bool {
    let ZeroCheck::Checking(seen) = store.zero else {
        return false;
    };
    let mut good = 0_u64;
    let mut cursor = chunk.start;
    let mut pieces: Vec<Range<u64>> = Vec::new();
    for b in bad {
        if b.start > cursor {
            pieces.push(cursor..b.start.min(chunk.end));
        }
        cursor = cursor.max(b.end);
    }
    if cursor < chunk.end {
        pieces.push(cursor..chunk.end);
    }
    for piece in pieces {
        let from = usize::try_from(piece.start.saturating_sub(chunk.start)).unwrap_or(usize::MAX);
        let to = usize::try_from(piece.end.saturating_sub(chunk.start)).unwrap_or(usize::MAX);
        if data.get(from..to).is_some_and(has_non_zero) {
            store.zero = ZeroCheck::NonZeroSeen;
            return false;
        }
        good = good.saturating_add(piece.end.saturating_sub(piece.start));
    }
    let seen = seen.saturating_add(good);
    let complete = store.done.contains_range(session_range);
    if seen >= ZERO_CHECK_BYTES || (complete && seen > 0) {
        store.zero = ZeroCheck::Raised;
        return store.raise_warning(WARN_SOURCE_READS_ZERO, READS_ZERO_MESSAGE);
    }
    store.zero = ZeroCheck::Checking(seen);
    false
}
