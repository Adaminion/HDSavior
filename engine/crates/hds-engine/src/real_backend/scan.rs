//! The scan job (`scan_start`, ALGORITHMS A, B3).
//!
//! `start` prepares the session on the server thread, so that everything
//! that can be refused is refused in the answer to `scan_start`: a new
//! session file is created (`Session::create`: guard, temporary file next
//! to it, rename without replacing), a resumed session continues its file
//! or writes a compacted copy to the new path. The run function then scans
//! (`hds_core::scan` checkpoints every 5 minutes and at every end: done,
//! stopped, `source_lost`, shutdown), records the scan time in `LIVE`,
//! builds the catalog in phase `analyzing` unless the engine is shutting
//! down, checkpoints and puts the session back.

use std::ops::Range;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use hds_core::scan::{scan, ScanObserver, ScanOptions, ScanStatus as CoreStatus};
use hds_core::session::{Session, SessionError, SessionGuard, SessionMeta, SourceIdentity};
use hds_io::{BlockSource, RangedSource};

use super::convert::{self, display_path};
use super::{now_filetime, Consent, Shared, Work};
use crate::backend::{JobContext, JobStart, ScanJob, ScanRequest};
use crate::output::log_warn;
use crate::protocol::{
    ErrorCode, JobKind, LogLevel, ProtocolError, ScanDone, ScanPhase, ScanProgress,
    ScanStartResult, ScanStatus,
};

/// Gives the [`Work`] back to the backend when the job ends, also when its
/// run function panics (then without catalog).
struct WorkSlot {
    shared: Arc<Shared>,
    work: Option<Work>,
}

impl WorkSlot {
    fn work(&mut self) -> Option<&mut Work> {
        self.work.as_mut()
    }

    fn give_back(&mut self) {
        if let Some(mut work) = self.work.take() {
            let mut st = self.shared.lock();
            if st.work.is_none() {
                if std::thread::panicking() {
                    work.built = None;
                }
                st.work = Some(work);
            }
        }
    }
}

impl Drop for WorkSlot {
    fn drop(&mut self) {
        self.give_back();
    }
}

/// Forwards the scanner's progress as `scan_progress` events.
struct Observer<'a> {
    ctx: &'a JobContext<ScanProgress>,
    last: ScanProgress,
}

impl ScanObserver for Observer<'_> {
    fn progress(&mut self, progress: &hds_core::scan::ScanProgress) {
        self.last = convert::scan_progress(progress);
        self.ctx.progress(&self.last);
    }

    fn checkpoint_failed(&mut self, error: &SessionError) {
        self.ctx.log(
            LogLevel::Warn,
            format!("the session file could not be updated: {error}"),
        );
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    let a = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    a == b
}

/// `scan_start`: prepares the session and returns the job.
pub(super) fn start(shared: &Arc<Shared>, request: ScanRequest) -> Result<ScanJob, ProtocolError> {
    let mut st = shared.lock();
    let source = request.source.clone();
    let range = request.range.clone();
    let work = if request.resume {
        let work = st.work_mut()?;
        if work.session.meta().range != range {
            return Err(ProtocolError::bad_request(
                "a resumed scan keeps the session range",
            ));
        }
        if let Some(dest) = &request.session {
            work.use_file(Path::new(&dest.path), &*source, dest)?;
        }
        let mut work = st.work.take().ok_or_else(super::no_session)?;
        work.built = None;
        work
    } else {
        let options = ScanOptions {
            collect_carve_hits: request.collect_carve_hits,
            ..ScanOptions::default()
        };
        let meta = SessionMeta::new(SourceIdentity::from(&source.info()), range.clone(), options)
            .map_err(|e| convert::session_error(&e))?;
        let session = match &request.session {
            Some(dest) => {
                let path = Path::new(&dest.path);
                // A file cannot be replaced while this process still writes
                // to it (Windows): the old session lets go of its file
                // when it is the same one.
                let released = st.work.as_mut().and_then(|w| {
                    let own = w.session.file_path().is_some_and(|f| same_file(f, path));
                    own.then(|| {
                        w.session.detach_file();
                        w.consent
                    })
                });
                let guard = SessionGuard {
                    source: &*source,
                    protected: &dest.protected,
                    allow_same_disk: dest.allow_same_disk,
                    allow_unknown_disk: dest.allow_unknown_disk,
                };
                match Session::create(meta, path, &guard) {
                    Ok(s) => s,
                    Err(e) => {
                        if let (Some(consent), Some(w)) = (released, st.work.as_mut()) {
                            take_back_file(w, path, &*source, consent);
                        }
                        return Err(convert::session_error(&e));
                    }
                }
            }
            None => Session::new(meta),
        };
        let mut work = Work::new(session);
        if let Some(dest) = &request.session {
            work.consent = Consent::of(dest);
        }
        st.work = None;
        work
    };
    st.source = Some(source.clone());
    drop(st);
    let bytes_total = range.end.saturating_sub(range.start);
    let shared = shared.clone();
    let resume = request.resume;
    Ok(JobStart {
        result: ScanStartResult {
            job: JobKind::Scan,
            bytes_total,
        },
        run: Box::new(move |ctx: &JobContext<ScanProgress>| {
            let mut slot = WorkSlot {
                shared,
                work: Some(work),
            };
            run(&mut slot, ctx, &source, range, resume)
        }),
    })
}

/// Best effort: the old session writes to its file again after a failed
/// `Session::create` at the same path.
fn take_back_file(work: &mut Work, path: &Path, source: &dyn BlockSource, consent: Consent) {
    let protected = work.protected();
    let guard = SessionGuard {
        source,
        protected: &protected,
        allow_same_disk: consent.same_disk,
        allow_unknown_disk: consent.unknown_disk,
    };
    if let Err(e) = work.session.attach_file(path, &guard) {
        log_warn(&format!(
            "the previous session no longer writes to {}: {e}",
            path.display()
        ));
    }
}

fn run(
    slot: &mut WorkSlot,
    ctx: &JobContext<ScanProgress>,
    source: &Arc<dyn BlockSource>,
    range: Range<u64>,
    resume: bool,
) -> ScanDone {
    let started = Instant::now();
    let Some(work) = slot.work() else {
        return failed(ProtocolError::internal("the session is gone"), None);
    };
    if resume {
        // I5: any scan_start discards the carved files.
        super::RealBackend::discard_carved(work);
    }
    let ranged = match RangedSource::new(source.clone(), range) {
        Ok(r) => r,
        Err(e) => {
            return failed(
                ProtocolError::internal(format!("the session range does not fit the source: {e}")),
                work.session.file_path().map(display_path),
            )
        }
    };
    let options = work.session.meta().options;
    let mut observer = Observer {
        ctx,
        last: ScanProgress::default(),
    };
    let summary = scan(
        &ranged,
        &options,
        &mut work.session,
        &mut observer,
        ctx.cancel_flag(),
    );
    if let Some(e) = &summary.checkpoint_error {
        ctx.log(
            LogLevel::Warn,
            format!("the session file is not up to date: {e}"),
        );
    }
    // LIVE: when the scan last ran (the end of this run).
    work.live.scan_time = Some(now_filetime());
    if let Err(e) = work.live.save(&mut work.session) {
        log_warn(&format!("the LIVE block cannot be stored: {e}"));
    }
    let (status, error) = match &summary.status {
        CoreStatus::Completed => (ScanStatus::Completed, None),
        CoreStatus::Stopped => (ScanStatus::Stopped, None),
        CoreStatus::Failed(f) => (
            ScanStatus::Failed,
            Some(ProtocolError::new(
                convert::error_code(f.code()),
                f.message().to_string(),
            )),
        ),
    };
    let mut done = ScanDone {
        status,
        error,
        session_path: None,
        catalog: None,
    };
    if status != ScanStatus::Failed && !ctx.is_shutting_down() {
        // Phase `analyzing`: job_cancel is ignored (PROTOCOL scan_progress).
        let mut progress = observer.last.clone();
        progress.phase = ScanPhase::Analyzing;
        progress.eta_seconds = None;
        progress.speed_bps = 0;
        let mut step = |k: u64, n: u64| {
            progress.bytes_done = k;
            progress.bytes_total = n;
            progress.elapsed_seconds = started.elapsed().as_secs();
            ctx.progress(&progress);
        };
        let never = AtomicBool::new(false);
        if let Err(e) = work.build(Some(source), &never, &mut step) {
            // S7: a source lost while the catalog is built fails the scan;
            // the session stays resumable.
            done.status = ScanStatus::Failed;
            done.error = Some(e);
            work.checkpoint();
        }
    } else {
        work.checkpoint();
    }
    done.session_path = work.session.file_path().map(display_path);
    // The session goes back before scan_done is written, so that the
    // catalog can be queried as soon as the event arrives.
    slot.give_back();
    if done.status != ScanStatus::Failed && !ctx.is_shutting_down() {
        let st = slot.shared.lock();
        let attached = st.source.is_some();
        done.catalog = st.work.as_ref().and_then(|w| w.info(attached));
    }
    if done.status == ScanStatus::Failed && done.error.is_none() {
        done.error = Some(ProtocolError::new(ErrorCode::Internal, "the scan failed"));
    }
    done
}

fn failed(error: ProtocolError, session_path: Option<String>) -> ScanDone {
    ScanDone {
        status: ScanStatus::Failed,
        error: Some(error),
        session_path,
        catalog: None,
    }
}
