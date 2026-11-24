//! The jobs that need a catalog or a source besides the scan: recovery
//! (ALGORITHMS H), carving (I) and imaging (J).
//!
//! Every `*_start` decides on the server thread everything that can be
//! refused, so that the refusal is the answer to the request:
//!
//! * `recover_start` (H1-H3): a fresh current-volume snapshot re-evaluates
//!   `overwritten` (H1, [`Work::fresh_overwritten`] and
//!   `Catalog::plan(.., Some(&set))`), the plan is built from the selection
//!   (H2, H4 through `hds_core::query`), the destination's free space is
//!   compared with the bytes that will be written (`no_space`), and
//!   [`Recovery::prepare`] runs the destination guard on the path and again
//!   on the created root with the protected ranges of this moment (session
//!   range first, then the lost volumes: [`Work::protected`]).
//! * `carve_start` (I1, I2): the candidates are chosen from the stored hits
//!   with the `main` instance's geometry, the protected files of I2 (a)
//!   (`Catalog::carve_protected`), the overwritten set and the
//!   live-allocated ranges of the layout.
//! * `image_start` (J): [`Imaging::prepare`] with the protected ranges the
//!   server checked (the session's, or the whole source before a session
//!   exists, plus the imaged range).
//!
//! The run functions hold no lock while they read or write: the recovery
//! locks the state for a moment per finished file to add what the read
//! found to the catalog (G1: `signature_ok` / `signature_mismatch`,
//! `bad_sectors`); carving locks it once at the end to store the run in
//! the session and the catalog (I5). Neither the session nor the catalog
//! can be replaced while a job runs: the server answers `busy` to every
//! command that would.

use std::path::Path;
use std::sync::Arc;

use hds_core::carve::{store_run, CarveGeometry, CarveInput, CarveStatus, Carver, KindSet};
use hds_core::catalog::NodeId;
use hds_core::instances::InstanceKind;
use hds_core::query::Selection;
use hds_io::{destination_check, DestinationCheck, Protected, Purpose, RangedSource};
use hds_recover::{
    rate, FileOutcome, ImageJob as Imaging, ImageObserver, ImageOptions, ImageStartError,
    ImageSummary, JobStatus as RecoverStatus, OnExists as CoreOnExists, Rating, RecoverObserver,
    RecoverOptions, RecoverSummary, Recovery, StartError,
};

use super::convert::{self, display_path};
use super::{no_catalog, no_source, Shared, State, Work};
use crate::backend::{
    CarveJob, CarveRequest, ImageJob, ImageRequest, JobContext, JobStart, RecoverJob,
    RecoverRequest,
};
use crate::output::log_warn;
use crate::protocol::{
    CarveDone, CarveProgress, CarveStartResult, CheckDestinationResult, ErrorCode, ImageDone,
    ImageProgress, ImageStartResult, JobKind, JobStatus, LogLevel, OnExists, ProtocolError,
    RecoverDone, RecoverProgress, RecoverStartResult,
};

/// `destination_refused` with the check (and the catalog's files inside
/// the location's volume) as `data`, as the server answers it.
fn refused(state: &State, check: &DestinationCheck) -> ProtocolError {
    let (lost_files, lost_bytes) = state
        .work
        .as_ref()
        .and_then(|w| w.built.as_ref())
        .filter(|_| !check.source_extents.is_empty())
        .map_or((0, 0), |b| {
            b.catalog.files_in_extents(&check.source_extents)
        });
    let result = CheckDestinationResult {
        check: check.clone(),
        lost_files,
        lost_bytes,
    };
    let message = if check.message.is_empty() {
        "the destination cannot be used".to_string()
    } else {
        check.message.clone()
    };
    let error = ProtocolError::new(ErrorCode::DestinationRefused, message);
    match serde_json::to_value(&result) {
        Ok(data) => error.with_data(data),
        Err(_) => error,
    }
}

/// The protected ranges of this moment: the backend's own (session range
/// first, then the lost volumes) and whatever else the server checked
/// against.
fn merged_protected(work: &Work, server: &[Protected]) -> Vec<Protected> {
    let mut out = work.protected();
    for p in server {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

fn job_error(code: &str, message: Option<&str>) -> ProtocolError {
    let message = message.map_or_else(|| code.replace('_', " "), str::to_string);
    ProtocolError::new(convert::error_code(code), message)
}

fn job_status(status: RecoverStatus) -> JobStatus {
    match status {
        RecoverStatus::Completed => JobStatus::Completed,
        RecoverStatus::Cancelled => JobStatus::Cancelled,
        RecoverStatus::Failed => JobStatus::Failed,
    }
}

// ------------------------------------------------------------------------------------------
// Recovery
// ------------------------------------------------------------------------------------------

/// Forwards progress and feeds what each read found back into the catalog.
struct RecoverEvents<'a> {
    ctx: &'a JobContext<RecoverProgress>,
    shared: &'a Shared,
}

impl RecoverObserver for RecoverEvents<'_> {
    fn progress(&mut self, p: &hds_recover::RecoverProgress) {
        self.ctx.progress(&RecoverProgress {
            files_done: p.files_done,
            files_total: p.files_total,
            bytes_done: p.bytes_done,
            bytes_total: p.bytes_total,
            current: (!p.current.is_empty()).then(|| p.current.clone()),
            speed_bps: p.speed_bps,
            eta_seconds: p.eta_seconds,
            elapsed_seconds: p.elapsed_seconds,
            ok: p.ok,
            warnings: p.warnings,
            failed: p.failed,
            skipped: p.skipped,
        });
    }

    fn file_done(&mut self, outcome: &FileOutcome) {
        // G1: the signature check and unreadable sectors of the read join
        // the file's flags, so later rows show them.
        let Ok(id) = u32::try_from(outcome.node) else {
            return;
        };
        let st = self.shared.lock();
        if let Some(built) = st.work.as_ref().and_then(|w| w.built.as_ref()) {
            built.catalog.observe(NodeId(id), outcome.flags);
        }
    }
}

fn recover_done(summary: &RecoverSummary) -> RecoverDone {
    RecoverDone {
        status: job_status(summary.status),
        error: summary
            .error
            .map(|e| job_error(e.name(), summary.message.as_deref())),
        files_total: summary.files_total,
        ok: summary.ok,
        warnings: summary.warnings,
        failed: summary.failed,
        skipped: summary.skipped,
        bytes_written: summary.bytes_written,
        dest: summary.dest.clone(),
        report_path: (!summary.report_path.is_empty()).then(|| summary.report_path.clone()),
    }
}

/// Allocation unit assumed for the destination: NTFS's default cluster
/// and the page size of memory file systems. Every file takes whole units.
const ALLOCATION_UNIT: u64 = 4096;
/// Per file: its record or directory entry and its row in the report.
const PER_FILE: u64 = 1024;
/// Per folder: one allocation unit for its index.
const PER_FOLDER: u64 = ALLOCATION_UNIT;

/// What files of `sizes` and `folders` folders take on the destination:
/// each file rounded up to whole allocation units, plus per-file and
/// per-folder overhead. An estimate (`recover_start`'s `no_space`): a
/// destination with larger clusters needs more, one that stores small
/// files inside their records less.
pub(crate) fn space_need(sizes: impl Iterator<Item = u64>, folders: usize) -> u64 {
    let files = sizes.fold(0u64, |sum, size| {
        let units = size.div_ceil(ALLOCATION_UNIT);
        sum.saturating_add(units.saturating_mul(ALLOCATION_UNIT))
            .saturating_add(PER_FILE)
    });
    files.saturating_add((folders as u64).saturating_mul(PER_FOLDER))
}

/// `recover_start` (see the module documentation).
pub(super) fn recover_start(
    shared: &Arc<Shared>,
    request: RecoverRequest,
) -> Result<RecoverJob, ProtocolError> {
    let RecoverRequest { args, dest } = request;
    let st = shared.lock();
    let source = st.source.clone().ok_or_else(no_source)?;
    let work = st.work.as_ref().ok_or_else(no_catalog)?;
    let built = work.built.as_ref().ok_or_else(no_catalog)?;
    // H1: the current volumes may have written since the scan.
    let fresh = work.fresh_overwritten(&source, &super::never())?;
    let selection = Selection {
        include: args.include.clone(),
        exclude: args.exclude.clone(),
        filter: args.filter.as_ref().map(convert::core_filter),
        include_deleted: args.include_deleted,
        skip_damaged: args.skip_damaged,
    };
    let store = work.session.store();
    let plan = built
        .catalog
        .plan(store, &selection, Some(&fresh))
        .map_err(|e| convert::query_error(&e))?;
    if plan.files.is_empty() {
        return Err(ProtocolError::bad_request(
            "the selection holds no file to recover",
        ));
    }
    let ranged = work.ranged(&source)?;
    let protected = merged_protected(work, &dest.protected);
    // `no_space`: an estimate of what the files that will be written take
    // on the destination (H3), with allocation overhead.
    let need = space_need(
        plan.files
            .iter()
            .filter(|f| match rate(f.flags, f.context) {
                Rating::Unsupported => false,
                Rating::Damaged => !args.skip_damaged,
                Rating::Good | Rating::Uncertain => true,
            })
            .map(|f| f.size),
        plan.folders.len(),
    );
    let check = destination_check(
        Path::new(&dest.path),
        Purpose::Recover,
        &ranged,
        &protected,
        Some(need),
    );
    if !check.is_allowed(dest.allow_same_disk, dest.allow_unknown_disk) {
        return Err(refused(&st, &check));
    }
    if check.enough_space == Some(false) {
        return Err(ProtocolError::new(
            ErrorCode::NoSpace,
            format!(
                "the destination has {} bytes free; the selected files need {need}",
                check.free_bytes.unwrap_or(0)
            ),
        )
        .with_data(serde_json::json!({
            "free_bytes": check.free_bytes,
            "need_bytes": need,
        })));
    }
    let options = RecoverOptions {
        on_exists: match args.on_exists {
            OnExists::Rename => CoreOnExists::Rename,
            OnExists::Skip => CoreOnExists::Skip,
            OnExists::Overwrite => CoreOnExists::Overwrite,
        },
        skip_damaged: args.skip_damaged,
        allow_same_disk: dest.allow_same_disk,
        allow_unknown_disk: dest.allow_unknown_disk,
        protected,
    };
    // The guard again on the path and on the created root (H3).
    let job =
        Recovery::prepare(ranged, plan, Path::new(&dest.path), options).map_err(|e| match e {
            StartError::DestinationRefused(check) => refused(&st, &check),
            StartError::NoSpace(e) => {
                ProtocolError::new(ErrorCode::NoSpace, format!("the destination is full: {e}"))
            }
            StartError::Io(e) => ProtocolError::from_io("the destination cannot be prepared", &e),
            StartError::InvalidPlan(why) => {
                ProtocolError::internal(format!("invalid recovery plan: {why}"))
            }
        })?;
    drop(st);
    let result = RecoverStartResult {
        job: JobKind::Recover,
        files: job.files_total(),
        bytes: job.bytes_total(),
    };
    let shared = shared.clone();
    Ok(JobStart {
        result,
        run: Box::new(move |ctx: &JobContext<RecoverProgress>| {
            let mut events = RecoverEvents {
                ctx,
                shared: &shared,
            };
            let summary = job.run(&mut events, ctx.cancel_flag());
            if let Some(message) = &summary.message {
                ctx.log(LogLevel::Warn, message.clone());
            }
            recover_done(&summary)
        }),
    })
}

// ------------------------------------------------------------------------------------------
// Carving
// ------------------------------------------------------------------------------------------

struct CarveEvents<'a> {
    ctx: &'a JobContext<CarveProgress>,
}

impl hds_core::carve::CarveObserver for CarveEvents<'_> {
    fn progress(&mut self, p: &hds_core::carve::CarveProgress) {
        self.ctx.progress(&CarveProgress {
            candidates_done: p.candidates_done,
            candidates_total: p.candidates_total,
            found: p.found,
            bytes_found: p.bytes_found,
            eta_seconds: p.eta_seconds,
        });
    }
}

fn unsupported(message: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::Unsupported, message)
}

/// `carve_start` (see the module documentation).
pub(super) fn carve_start(
    shared: &Arc<Shared>,
    request: CarveRequest,
) -> Result<CarveJob, ProtocolError> {
    let st = shared.lock();
    let source = st.source.clone().ok_or_else(no_source)?;
    let work = st.work.as_ref().ok_or_else(no_catalog)?;
    let built = work.built.as_ref().ok_or_else(no_catalog)?;
    if !work.session.meta().options.collect_carve_hits {
        return Err(unsupported(
            "the scan did not collect signature hits; scan again with collect_carve_hits to carve",
        ));
    }
    // I1: the main instance's geometry only.
    let main = built
        .catalog
        .instances()
        .iter()
        .find(|i| i.kind == InstanceKind::Main)
        .ok_or_else(|| {
            unsupported("there is no main file system whose clusters carving could follow")
        })?;
    let geometry = CarveGeometry::new(
        u64::from(main.geometry.cluster_size),
        main.geometry.volume_offset,
    )
    .ok_or_else(|| unsupported("the main file system's geometry cannot be used for carving"))?;
    let mut kinds = KindSet::default();
    for kind in &request.kinds {
        kinds.insert(*kind);
    }
    let store = work.session.store();
    let live = built.layout.live();
    let input = CarveInput {
        kinds,
        geometry,
        hits: store.carve_hits_sorted(),
        done: store.done().clone(),
        protected_files: built.catalog.carve_protected(),
        overwritten: live.overwritten().clone(),
        live_allocated: live.live_allocated().clone(),
    };
    let range = work.session.meta().range.clone();
    let carver = Carver::new(input, range);
    let ranged = work.ranged(&source)?;
    drop(st);
    let result = CarveStartResult {
        job: JobKind::Carve,
        candidates: carver.candidates(),
    };
    let shared = shared.clone();
    Ok(JobStart {
        result,
        run: Box::new(move |ctx: &JobContext<CarveProgress>| {
            let mut events = CarveEvents { ctx };
            let (run, summary) = carver.run(&ranged, &mut events, ctx.cancel_flag());
            let (status, mut error) = match &summary.status {
                CarveStatus::Completed => (JobStatus::Completed, None),
                CarveStatus::Cancelled => (JobStatus::Cancelled, None),
                CarveStatus::Failed(f) => (
                    JobStatus::Failed,
                    Some(job_error(f.code(), Some(f.message()))),
                ),
            };
            // I5: the run replaces the carved files of its kinds, also when
            // it was cancelled or lost the source (what was found is kept).
            let mut st = shared.lock();
            let attached = st.source.is_some();
            let catalog = match st.work.as_mut() {
                Some(work) => store_carve_run(work, &run).map_or_else(
                    |e| {
                        error.get_or_insert(e);
                        None
                    },
                    |()| work.info(attached),
                ),
                None => None,
            };
            let status = if error.is_some() {
                JobStatus::Failed
            } else {
                status
            };
            CarveDone {
                status,
                error,
                found: summary.found,
                bytes_found: summary.bytes_found,
                catalog,
            }
        }),
    })
}

/// Stores a carving run in the set, the session and the catalog (I5); on
/// an error nothing changed.
fn store_carve_run(work: &mut Work, run: &hds_core::carve::CarveRun) -> Result<(), ProtocolError> {
    let mut carved = work.carved.clone();
    carved.apply(run);
    let entries = super::carved_entries(&carved);
    let built = work.built.as_mut().ok_or_else(no_catalog)?;
    let catalog = built
        .catalog
        .with_carved(work.session.store(), &entries)
        .map_err(|e| ProtocolError::internal(e.to_string()))?;
    if let Err(e) = store_run(&mut work.session, run) {
        log_warn(&format!(
            "the carved files cannot be stored in the session: {e}"
        ));
    }
    built.catalog = catalog;
    work.carved = carved;
    work.checkpoint();
    Ok(())
}

// ------------------------------------------------------------------------------------------
// Imaging
// ------------------------------------------------------------------------------------------

struct ImageEvents<'a> {
    ctx: &'a JobContext<ImageProgress>,
}

impl ImageObserver for ImageEvents<'_> {
    fn progress(&mut self, p: &hds_recover::ImageProgress) {
        self.ctx.progress(&ImageProgress {
            bytes_done: p.bytes_done,
            bytes_total: p.bytes_total,
            speed_bps: p.speed_bps,
            eta_seconds: p.eta_seconds,
            elapsed_seconds: p.elapsed_seconds,
            bad_bytes: p.bad_bytes,
        });
    }
}

fn image_done(summary: &ImageSummary) -> ImageDone {
    ImageDone {
        status: job_status(summary.status),
        error: summary
            .error
            .map(|e| job_error(e.name(), summary.message.as_deref())),
        bytes_written: summary.bytes_written,
        bad_bytes: summary.bad_bytes,
        path: summary.path.clone(),
        bad_map_path: summary.bad_map_path.clone(),
    }
}

/// `image_start` (see the module documentation).
pub(super) fn image_start(
    shared: &Arc<Shared>,
    request: ImageRequest,
) -> Result<ImageJob, ProtocolError> {
    let ImageRequest {
        source,
        range,
        dest,
        resume,
    } = request;
    // A source that stopped answering is refused before any file is made
    // (an image job on it would only leave an empty image behind).
    if !source.is_alive() {
        return Err(ProtocolError::new(
            ErrorCode::SourceLost,
            "the source stopped answering; attach it again",
        ));
    }
    let ranged = RangedSource::new(source, range).map_err(|e| {
        ProtocolError::bad_request(format!("the range does not fit the source: {e}"))
    })?;
    let st = shared.lock();
    // The server's list (session range or the whole source, lost volumes,
    // the imaged range) and the backend's own of this moment.
    let protected = match st.work.as_ref() {
        Some(work) => merged_protected(work, &dest.protected),
        None => dest.protected.clone(),
    };
    let options = ImageOptions {
        resume,
        allow_same_disk: dest.allow_same_disk,
        allow_unknown_disk: dest.allow_unknown_disk,
        protected,
    };
    let job = Imaging::prepare(ranged, Path::new(&dest.path), &options).map_err(|e| match e {
        ImageStartError::DestinationRefused(check) => refused(&st, &check),
        ImageStartError::Exists(path) => ProtocolError::new(
            ErrorCode::Exists,
            format!(
                "{} exists already; resume it or choose another file",
                display_path(&path)
            ),
        ),
        ImageStartError::CannotResume(why) => ProtocolError::new(
            ErrorCode::Exists,
            format!("the image cannot be continued ({why}); choose another file"),
        ),
        ImageStartError::NoSpace {
            free_bytes,
            need_bytes,
        } => ProtocolError::new(ErrorCode::NoSpace, e.to_string())
            .with_data(serde_json::json!({"free_bytes": free_bytes, "need_bytes": need_bytes})),
        ImageStartError::TooLarge {
            size,
            max_file_bytes,
        } => ProtocolError::new(ErrorCode::NoSpace, e.to_string())
            .with_data(serde_json::json!({"need_bytes": size, "max_file_bytes": max_file_bytes})),
        ImageStartError::SourceMismatch(ref failed) => {
            ProtocolError::new(ErrorCode::SourceMismatch, e.to_string())
                .with_data(serde_json::json!({ "failed": failed }))
        }
        ImageStartError::SourceLost(ref io) => ProtocolError::new(
            ErrorCode::SourceLost,
            format!("the source stopped answering: {io}"),
        ),
        ImageStartError::Io(ref io) => {
            ProtocolError::from_io("the image file cannot be prepared", io)
        }
    })?;
    drop(st);
    let result = ImageStartResult {
        job: JobKind::Image,
        bytes: job.bytes_total(),
    };
    Ok(JobStart {
        result,
        run: Box::new(move |ctx: &JobContext<ImageProgress>| {
            let mut events = ImageEvents { ctx };
            let summary = job.run(&mut events, ctx.cancel_flag());
            if let Some(message) = &summary.message {
                ctx.log(LogLevel::Warn, message.clone());
            }
            image_done(&summary)
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::space_need;

    #[test]
    fn the_space_need_counts_allocation_overhead() {
        // Review finding: 9,916 small files of 18.2 MB passed a 30 MiB
        // check and filled the destination after 4,604 files.
        let sizes = std::iter::repeat_n(1835_u64, 9916);
        let need = space_need(sizes, 40);
        assert!(need > 30 * 1024 * 1024, "{need}");
        assert_eq!(space_need(std::iter::once(0), 0), 1024);
        assert_eq!(space_need(std::iter::once(4097), 1), 8192 + 1024 + 4096);
        assert_eq!(space_need(std::iter::once(u64::MAX), 0), u64::MAX);
    }
}
