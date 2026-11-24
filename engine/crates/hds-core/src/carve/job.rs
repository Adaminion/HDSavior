//! The carving job (ALGORITHMS I1 to I3).

use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hds_carve::{try_measure, FileKind, Limits, MeasureError};
use hds_io::{RangeSet, RangedSource};
use hds_recover::{CarveFailure as ReadFailure, CarveReader};
use serde::Serialize;

use super::file::CarvedFile;
use super::set::CarveRun;
use super::{CarveGeometry, KindSet};
use crate::store::CarveHit;

/// Progress events are at most this frequent (4 per second, as for the
/// other jobs).
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Everything the carving job needs from the session and the catalog.
///
/// The fields are owned, so that the job can move to a worker thread and
/// the session and catalog stay free for queries while it runs.
#[derive(Clone, Debug)]
pub struct CarveInput {
    /// The kinds to carve (`carve_start`'s `kinds`; `null` there means
    /// [`KindSet::all`]). Hits of other kinds are ignored, and the carved
    /// files of other kinds stay as they are.
    pub kinds: KindSet,
    /// The `main` instance's geometry (I1): candidates must lie on its
    /// cluster grid.
    pub geometry: CarveGeometry,
    /// The scan's carve hits: `ScanStore::carve_hits_sorted()` (any order
    /// is accepted; the job sorts).
    pub hits: Vec<CarveHit>,
    /// The scan's done set (`ScanStore::done()`): a candidate must lie in
    /// the scanned part of the range (I2).
    pub done: RangeSet,
    /// I2 (a): the absolute byte ranges of the data extents - whole clusters
    /// of every non-sparse run of the unnamed `$DATA`, mapped with the
    /// `main` geometry - of every in-use file of the `main` instance that
    /// is neither `overwritten` nor `out_of_bounds`. Candidates inside them
    /// are skipped, and a candidate's measured length never reaches into
    /// the next one (I3).
    pub protected_files: RangeSet,
    /// I2 (b): the overwritten set (C5): live-allocated ranges and the
    /// extents of `post_format` records. Candidates inside it are skipped.
    pub overwritten: RangeSet,
    /// The live-allocated ranges of the current volumes (C2): candidates
    /// inside them are skipped (they are part of the overwritten set too)
    /// and a measured length never reaches into the next one (I3).
    pub live_allocated: RangeSet,
}

/// Receives the job's progress.
pub trait CarveObserver {
    /// At most every [`PROGRESS_INTERVAL`] (and once at the end).
    fn progress(&mut self, progress: &CarveProgress);
    /// Every file, as it is found.
    fn found(&mut self, _file: &CarvedFile) {}
}

/// An observer that ignores everything.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCarveObserver;

impl CarveObserver for NoCarveObserver {
    fn progress(&mut self, _progress: &CarveProgress) {}
}

/// The `carve_progress` event (PROTOCOL `carve_start`), member for member.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CarveProgress {
    /// Candidates handled (measured, or skipped inside a carved file).
    pub candidates_done: u64,
    /// All candidates.
    pub candidates_total: u64,
    /// Files found.
    pub found: u64,
    /// Their bytes.
    pub bytes_found: u64,
    /// Remaining seconds at the rate so far; `None` before anything is
    /// known.
    pub eta_seconds: Option<u64>,
}

/// How the job ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CarveStatus {
    /// Every candidate was handled.
    Completed,
    /// The cancel flag was raised; the files found so far are in the run.
    Cancelled,
    /// The job could not go on; the files found so far are in the run.
    Failed(CarveFailure),
}

impl CarveStatus {
    /// The protocol's `status`: `"completed"`, `"cancelled"`, `"failed"`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            CarveStatus::Completed => "completed",
            CarveStatus::Cancelled => "cancelled",
            CarveStatus::Failed(_) => "failed",
        }
    }
}

/// Why a job failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CarveFailure {
    /// The source stopped answering (S7): protocol error `source_lost`.
    SourceLost(String),
    /// The source given to [`Carver::run`] is not the one the candidates
    /// were chosen for: protocol error `internal`.
    Internal(String),
}

impl CarveFailure {
    /// The protocol error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            CarveFailure::SourceLost(_) => "source_lost",
            CarveFailure::Internal(_) => "internal",
        }
    }

    /// The message.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            CarveFailure::SourceLost(m) | CarveFailure::Internal(m) => m,
        }
    }
}

impl fmt::Display for CarveFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

/// The end of a job: what `carve_done` reports (without the catalog) and
/// a little more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarveSummary {
    /// How it ended.
    pub status: CarveStatus,
    /// Files found (`carve_done.found`).
    pub found: u64,
    /// Their bytes (`carve_done.bytes_found`).
    pub bytes_found: u64,
    /// Candidates (`carve_start.candidates`).
    pub candidates_total: u64,
    /// Candidates handled before the end.
    pub candidates_done: u64,
    /// Candidates skipped because a file carved by this run covers them
    /// (I2 c).
    pub inside_carved: u64,
    /// Candidates that are not a complete, measurable file of their kind
    /// within their bound (or exceeded the work limits).
    pub not_measurable: u64,
    /// Candidates given up because their bytes could not be read.
    pub unreadable: u64,
}

/// A prepared carving job: the candidates of I2, ready to be measured.
#[derive(Debug)]
pub struct Carver {
    kinds: KindSet,
    geometry: CarveGeometry,
    range: Range<u64>,
    /// The candidates, ascending by offset (I2 without (c)).
    candidates: Vec<CarveHit>,
    protected_files: RangeSet,
    live_allocated: RangeSet,
    progress_interval: Duration,
}

impl Carver {
    /// Chooses the candidates of I2 among `input.hits` for a session whose
    /// range is `range`: hits of the requested kinds, inside the range and
    /// its done part, on the geometry's cluster grid, outside the protected
    /// files, the overwritten set and the live-allocated ranges. Costs
    /// `O(hits × log ranges)`; the hit vector is reused for the candidates.
    #[must_use]
    pub fn new(input: CarveInput, range: Range<u64>) -> Carver {
        let CarveInput {
            kinds,
            geometry,
            mut hits,
            done,
            protected_files,
            overwritten,
            live_allocated,
        } = input;
        hits.retain(|hit| {
            let offset = hit.disk_offset();
            kinds.contains(hit.kind())
                && range.contains(&offset)
                && geometry.on_grid(offset)
                && done.contains(offset)
                && !protected_files.contains(offset)
                && !overwritten.contains(offset)
                && !live_allocated.contains(offset)
        });
        hits.sort_unstable();
        // one sector, one sniffed kind; a hostile session could repeat one
        hits.dedup_by_key(|hit| hit.disk_offset());
        hits.shrink_to_fit();
        Carver {
            kinds,
            geometry,
            range,
            candidates: hits,
            protected_files,
            live_allocated,
            progress_interval: PROGRESS_INTERVAL,
        }
    }

    /// Number of candidates (`carve_start`'s `candidates`).
    #[must_use]
    pub fn candidates(&self) -> u64 {
        u64::try_from(self.candidates.len()).unwrap_or(u64::MAX)
    }

    /// The candidates, ascending by offset.
    #[must_use]
    pub fn candidate_hits(&self) -> &[CarveHit] {
        &self.candidates
    }

    /// Changes the minimum time between progress events (tests use 0).
    #[must_use]
    pub fn with_progress_interval(mut self, interval: Duration) -> Carver {
        self.progress_interval = interval;
        self
    }

    /// The furthest a candidate at `offset` can extend (I3): to the next
    /// protected file, the next live-allocated range or the range end,
    /// whichever comes first.
    fn bound(&self, offset: u64) -> u64 {
        let next = |set: &RangeSet| {
            set.intersection(offset..self.range.end)
                .next()
                .map_or(self.range.end, |r| r.start)
        };
        next(&self.protected_files)
            .min(next(&self.live_allocated))
            .min(self.range.end)
    }

    /// Measures the candidates in ascending offset order (I3) and returns
    /// the run with every file found, whatever the status. Reads only
    /// through `source`, which must cover the range given to [`new`]
    /// (otherwise nothing is read and the status is `Failed(Internal)`).
    /// Polls `cancel` before every candidate and between the retries of a
    /// failing read.
    ///
    /// [`new`]: Carver::new
    pub fn run(
        self,
        source: &RangedSource,
        observer: &mut dyn CarveObserver,
        cancel: &AtomicBool,
    ) -> (CarveRun, CarveSummary) {
        let started = Instant::now();
        let mut files: Vec<CarvedFile> = Vec::new();
        let mut summary = CarveSummary {
            status: CarveStatus::Completed,
            found: 0,
            bytes_found: 0,
            candidates_total: self.candidates(),
            candidates_done: 0,
            inside_carved: 0,
            not_measurable: 0,
            unreadable: 0,
        };
        if source.range() != self.range {
            summary.status = CarveStatus::Failed(CarveFailure::Internal(format!(
                "the source covers {:?}, the candidates were chosen for {:?}",
                source.range(),
                self.range
            )));
            return (self.into_run(files), summary);
        }

        let mut last_progress: Option<Instant> = None;
        let mut carved_until = 0_u64;
        for hit in &self.candidates {
            if cancel.load(Ordering::Relaxed) {
                summary.status = CarveStatus::Cancelled;
                break;
            }
            let due = last_progress.is_none_or(|t| t.elapsed() >= self.progress_interval);
            if due {
                observer.progress(&progress(&summary, started));
                last_progress = Some(Instant::now());
            }
            let offset = hit.disk_offset();
            if offset < carved_until {
                // I2 (c): inside a file this run carved
                summary.inside_carved = summary.inside_carved.saturating_add(1);
                summary.candidates_done = summary.candidates_done.saturating_add(1);
                continue;
            }
            match self.measure(source, hit.kind(), offset, cancel) {
                Outcome::File(file) => {
                    carved_until = file.end();
                    summary.found = summary.found.saturating_add(1);
                    summary.bytes_found = summary.bytes_found.saturating_add(file.len());
                    observer.found(&file);
                    files.push(file);
                }
                Outcome::NotMeasurable => {
                    summary.not_measurable = summary.not_measurable.saturating_add(1);
                }
                Outcome::Unreadable => {
                    summary.unreadable = summary.unreadable.saturating_add(1);
                }
                Outcome::Cancelled => {
                    summary.status = CarveStatus::Cancelled;
                    break;
                }
                Outcome::SourceLost(message) => {
                    summary.status = CarveStatus::Failed(CarveFailure::SourceLost(message));
                    break;
                }
            }
            summary.candidates_done = summary.candidates_done.saturating_add(1);
        }
        observer.progress(&progress(&summary, started));
        (self.into_run(files), summary)
    }

    fn into_run(self, files: Vec<CarvedFile>) -> CarveRun {
        CarveRun {
            kinds: self.kinds,
            geometry: self.geometry,
            files,
        }
    }

    /// Measures one candidate.
    fn measure(
        &self,
        source: &RangedSource,
        kind: FileKind,
        offset: u64,
        cancel: &AtomicBool,
    ) -> Outcome {
        let max_len = self.bound(offset).saturating_sub(offset);
        let mut limits = Limits::DEFAULT;
        let mut retried = false;
        loop {
            let reader = CarveReader::new(source, offset, max_len, cancel);
            match try_measure(kind, &reader, max_len, &limits) {
                Ok(measured) => {
                    return match CarvedFile::from_measured(offset, kind, &measured) {
                        Some(file) if file.end() <= offset.saturating_add(max_len) => {
                            Outcome::File(file)
                        }
                        _ => Outcome::NotMeasurable,
                    };
                }
                Err(MeasureError::LimitReached) if kind == FileKind::Zip && !retried => {
                    // I3: a ZIP that hits the read limit (streamed entries
                    // must be read to find their data descriptors) is tried
                    // once more with the limit raised to its bound
                    retried = true;
                    limits = raised_limits(max_len);
                }
                Err(MeasureError::LimitReached | MeasureError::NotMeasurable) => {
                    return Outcome::NotMeasurable
                }
                Err(MeasureError::Io(error)) => {
                    return match reader.failure() {
                        Some(ReadFailure::SourceLost) => Outcome::SourceLost(error.to_string()),
                        Some(ReadFailure::Cancelled) => Outcome::Cancelled,
                        // unreadable sectors drop the candidate (the file
                        // would contain zeros where data was), and so does
                        // anything else a read reported
                        Some(ReadFailure::BadSectors | ReadFailure::Other) | None => {
                            if cancel.load(Ordering::Relaxed) {
                                Outcome::Cancelled
                            } else {
                                Outcome::Unreadable
                            }
                        }
                    };
                }
            }
        }
    }
}

/// The limits of the ZIP retry: as many bytes as the bound allows, and
/// reads to match (the smallest block the measurer reads is 16 KiB).
fn raised_limits(max_len: u64) -> Limits {
    Limits {
        max_read_bytes: max_len.max(Limits::DEFAULT.max_read_bytes),
        max_reads: (max_len / (16 << 10))
            .saturating_add(1)
            .max(Limits::DEFAULT.max_reads),
    }
}

enum Outcome {
    File(CarvedFile),
    NotMeasurable,
    Unreadable,
    Cancelled,
    SourceLost(String),
}

fn progress(summary: &CarveSummary, started: Instant) -> CarveProgress {
    let done = summary.candidates_done;
    let remaining = summary.candidates_total.saturating_sub(done);
    let elapsed = started.elapsed();
    // remaining × elapsed / done, in whole seconds
    let eta_seconds = elapsed
        .as_millis()
        .checked_mul(u128::from(remaining))
        .and_then(|t| t.checked_div(u128::from(done)))
        .and_then(|ms| u64::try_from(ms / 1000).ok());
    CarveProgress {
        candidates_done: done,
        candidates_total: summary.candidates_total,
        found: summary.found,
        bytes_found: summary.bytes_found,
        eta_seconds,
    }
}
