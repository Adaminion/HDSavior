//! The real [`Backend`]: the protocol commands on top of `hds-core` (scan,
//! session, current volumes, instances, geometry, catalog, health, queries)
//! and `hds-recover` (the content reader of `read_file`).
//!
//! # Why here
//!
//! ARCHITECTURE 5.5 makes `hds-core`'s query types mirror the protocol
//! objects "member by member" and leaves the conversion to the engine
//! (`hds_core::query` module documentation); 5.6 makes the engine the
//! only crate that knows the protocol. So the adapter lives in the engine
//! crate: `convert` turns `hds-core` values into protocol values, this
//! module keeps the state and runs the pipelines, and `hds-core` stays free
//! of protocol types.
//!
//! # State
//!
//! One [`Mutex`] holds everything ([`State`]): the attached source and the
//! [`Work`] of the session (the [`Session`] with its store and file, the
//! `LIVE` state, the stored heads, the overrides, the carved set, the
//! warnings the engine adds, and the [`Built`] catalog with its layout).
//! Queries lock it for their duration; the server calls them one at a
//! time, and only jobs run beside them.
//!
//! A scan moves the [`Work`] into its job (the server asks no catalog
//! question during a scan and refuses every command that changes the
//! session) and puts it back, with its catalog, before the run function
//! returns `scan_done`. If the run function panics, the work goes back
//! without catalog (`WorkSlot`).
//!
//! # Pipelines
//!
//! * Catalog build (`Work::build`, ALGORITHMS C2, D, F, E, G): with a
//!   source attached a fresh current-volume snapshot is taken (C2: "taken
//!   whenever a catalog is built with a source attached"), then
//!   `instances::analyze` (heads read through the source and stored,
//!   F3), then `Catalog::build` with the stored carved files. Only when
//!   all of it succeeded are `LIVE`, `HEADS` and the catalog replaced; the
//!   session file is then checked again by the guard with the new
//!   protected ranges (B3, `session_file_at_risk`) and checkpointed.
//! * Scan (module `scan`): ALGORITHMS A through `hds_core::scan`, which
//!   writes the B3 checkpoints every 5 minutes and at every end itself;
//!   the job records the scan time in `LIVE` (see below), builds the
//!   catalog unless the engine is shutting down, and checkpoints again.
//! * `session_load` (B4): loads the file, finds the source (the one given,
//!   else the recorded path, else for disks every disk of `list_disks`)
//!   whose identity matches, builds the catalog and, with the source
//!   attached, continues writing to the file.
//!
//! # The `LIVE` scan time
//!
//! `hds_core::live::LiveState::scan_time` is "when the scan last ran", the
//! reference of D2's "not more than a day after the scan". Every scan run
//! (new or resumed, whatever its end) sets it to the time the run ended,
//! before the catalog is built and before the session is checkpointed; a
//! session loaded from a file keeps the stored value.
//!
//! # Jobs
//!
//! `recover_start`, `carve_start` and `image_start` are in module `jobs`
//! (ALGORITHMS H, I, J). Their run functions capture `Arc<Shared>`
//! ([`RealBackend::shared`]) and lock it only briefly: queries run beside
//! them. Recovery refreshes the overwritten set first (H1,
//! [`Work::fresh_overwritten`]), builds its plan from the catalog and runs
//! with the protected ranges of [`Work::protected`]; what its reads find
//! joins the catalog (G1). Carving stores its run in the session, the
//! carved set and the catalog (I5) and keeps the epoch.

mod convert;
mod deadline;
mod jobs;
mod scan;

use std::ops::Range;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use hds_core::carve::{discard_carved, CarvedSet};
use hds_core::catalog::{self, CarvedEntry, Catalog, CatalogInput, InstanceSetError, NodeId};
use hds_core::geometry::Heads;
use hds_core::instances::{analyze, AnalysisError, AnalysisInput, KindChoice, Layout, Overrides};
use hds_core::live::{take_snapshot, LiveError, LiveIndex, LiveSnapshot, LiveState};
use hds_core::query::{ListArgs, SearchArgs as CoreSearchArgs, Selection};
use hds_core::session::{
    check_identity, IdentityError, Session, SessionError, SessionGuard, SourceIdentity,
    WARN_IDENTITY_DIFFERS,
};
use hds_io::{
    open_source, BlockSource, Protected, ProtectedReason, RangeSet, RangedSource, SourceInfo,
    SourceKind, Verdict, Warning,
};
use hds_recover::{read_preview, PreviewError, ReadError};

use crate::backend::{
    Backend, CarveJob, CarveRequest, Destination, FileBytes, ImageJob, ImageRequest, LoadedSession,
    RecoverJob, RecoverRequest, ScanJob, ScanRequest, TaskContext,
};
use crate::output::{log_info, log_warn};
use crate::protocol::{
    CatalogInfo, ErrorCode, GeometrySetArgs, InstanceChoice, InstanceSetArgs, ListChildrenArgs,
    NodeInfo, NodeInfoArgs, ProtocolError, ReadFileArgs, RowsPage, SearchArgs, SearchPage,
    SelectionArgs, SelectionSummary, SessionAttachResult, SessionLoadArgs, SessionLoadResult,
    SessionSaveResult, SourceRef,
};
use crate::recorded::Recorded;

pub use convert::display_path;
pub use deadline::{Deadline, READ_FILE_DEADLINE};

/// Warning code: the session file lies inside a lost volume (ALGORITHMS B3).
pub const WARN_SESSION_FILE_AT_RISK: &str = "session_file_at_risk";

/// Warning code: the last checkpoint of the session file failed (disk
/// full, disk gone): the file does not hold the session as it is. Cleared
/// by the next checkpoint that succeeds or by `session_save`.
pub const WARN_SESSION_FILE_NOT_SAVED: &str = "session_file_not_saved";

/// Warning code of `session_load`: the file ends early (bytes after its
/// last complete checkpoint were ignored: a crash or a full disk) or holds
/// no scanned data at all.
pub const WARN_SESSION_INCOMPLETE: &str = "session_incomplete";

/// The `data` member of an `instance_set` / `geometry_set` error after
/// which the backend rebuilt its catalog from the session (the previous
/// layout could not be restored): `{"catalog_reloaded": true}`. The
/// server then starts a new epoch.
pub const CATALOG_RELOADED: &str = "catalog_reloaded";

/// The current time as a FILETIME.
pub(crate) fn now_filetime() -> u64 {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    unix.saturating_add(11_644_473_600)
        .saturating_mul(10_000_000)
}

/// A flag nobody sets: plain requests are not cancellable, and the
/// analysis phase of a scan ignores `job_cancel` (PROTOCOL `scan_progress`).
fn never() -> AtomicBool {
    AtomicBool::new(false)
}

fn no_session() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::NoSession,
        "there is no session: scan or load one first",
    )
}

fn no_catalog() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::NoCatalog,
        "there is no catalog: scan or load a session first",
    )
}

fn no_source() -> ProtocolError {
    ProtocolError::new(ErrorCode::NoSource, "the session's source is not attached")
}

fn source_lost(e: &std::io::Error) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::SourceLost,
        format!("the source stopped answering: {e}"),
    )
}

fn analysis_error(e: &AnalysisError) -> ProtocolError {
    match e {
        AnalysisError::SourceLost(e) => source_lost(e),
        AnalysisError::Cancelled => ProtocolError::new(ErrorCode::Cancelled, "cancelled"),
    }
}

/// The consents a session file location was allowed with (S3); the guard
/// runs again with them when the protected ranges grow (B3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Consent {
    /// `allow_same_disk`.
    pub same_disk: bool,
    /// `allow_unknown_disk`.
    pub unknown_disk: bool,
}

impl Consent {
    fn of(dest: &Destination) -> Consent {
        Consent {
            same_disk: dest.allow_same_disk,
            unknown_disk: dest.allow_unknown_disk,
        }
    }
}

/// A built catalog with the layout it was built from.
pub struct Built {
    /// Current volumes, instances, geometries (C, D, F).
    pub layout: Layout,
    /// The trees and their health (E, G).
    pub catalog: Catalog,
}

/// A session and everything derived from it.
pub struct Work {
    /// The session: facts, store, file.
    pub session: Session,
    /// The `LIVE` block: scan time and the latest snapshot.
    pub live: LiveState,
    /// The heads read for geometry checks (F3), stored in `HEADS`.
    pub heads: Heads,
    /// The user's overrides (`instance_set`, `geometry_set`).
    pub overrides: Overrides,
    /// The carved files (I5), stored in `CARVED`.
    pub carved: CarvedSet,
    /// The consents of the session file's location.
    pub consent: Consent,
    /// Warnings the engine adds to the catalog's (`identity_differs`,
    /// `session_file_at_risk`).
    pub warnings: Vec<Warning>,
    /// The catalog, once built.
    pub built: Option<Built>,
}

impl Work {
    fn new(session: Session) -> Work {
        Work {
            session,
            live: LiveState::default(),
            heads: Heads::new(),
            overrides: Overrides::new(),
            carved: CarvedSet::new(),
            consent: Consent::default(),
            warnings: Vec::new(),
            built: None,
        }
    }

    /// A loaded session with its stored blocks decoded.
    fn loaded(session: Session) -> Result<Work, ProtocolError> {
        let corrupt = |what: &str, e: &dyn std::fmt::Display| {
            ProtocolError::new(
                ErrorCode::IoError,
                format!("the session file cannot be used: its {what} block is damaged: {e}"),
            )
        };
        let live = LiveState::load(&session).map_err(|e| corrupt("LIVE", &e))?;
        let heads = Heads::load(&session).map_err(|e| corrupt("HEADS", &e))?;
        let overrides = Overrides::load(&session).map_err(|e| corrupt("OVERRIDES", &e))?;
        let carved = CarvedSet::from_session(&session).map_err(|e| corrupt("CARVED", &e))?;
        Ok(Work {
            live,
            heads,
            overrides,
            carved,
            ..Work::new(session)
        })
    }

    /// The source wrapped in the session range (S2a).
    pub fn ranged(&self, source: &Arc<dyn BlockSource>) -> Result<RangedSource, ProtocolError> {
        let range = self.session.meta().range.clone();
        RangedSource::new(source.clone(), range).map_err(|e| {
            ProtocolError::new(
                ErrorCode::SourceMismatch,
                format!("the source does not hold the session range: {e}"),
            )
        })
    }

    /// The protected ranges of this moment (S2): the session range, then
    /// the lost-volume ranges of the catalog.
    pub fn protected(&self) -> Vec<Protected> {
        let mut out = vec![Protected::session(self.session.meta().range.clone())];
        if let Some(built) = &self.built {
            out.extend(
                built
                    .layout
                    .protected()
                    .iter()
                    .filter(|p| p.reason == ProtectedReason::LostVolume)
                    .filter(|p| p.range.start < p.range.end)
                    .cloned(),
            );
        }
        out
    }

    /// The overwritten set (C5) of a fresh snapshot, taken now and not
    /// stored (ALGORITHMS H1 for `recover_start`).
    pub fn fresh_overwritten(
        &self,
        source: &Arc<dyn BlockSource>,
        cancel: &AtomicBool,
    ) -> Result<RangeSet, ProtocolError> {
        let ranged = self.ranged(source)?;
        let snapshot = take_snapshot(&ranged, now_filetime(), cancel).map_err(|e| match e {
            LiveError::SourceLost(e) => source_lost(&e),
            LiveError::Cancelled => ProtocolError::new(ErrorCode::Cancelled, "cancelled"),
        })?;
        Ok(LiveIndex::build(self.session.store(), &snapshot)
            .overwritten()
            .clone())
    }

    /// The carved files as the catalog takes them.
    fn carved_entries(&self) -> Vec<CarvedEntry> {
        carved_entries(&self.carved)
    }

    /// Builds the catalog (see the module documentation). `progress` gets
    /// `(step, steps)`. On an error nothing but `heads` changed.
    fn build(
        &mut self,
        source: Option<&Arc<dyn BlockSource>>,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), ProtocolError> {
        const STEPS: u64 = 3;
        let ranged = source.map(|s| self.ranged(s)).transpose()?;
        let mut live = self.live.clone();
        progress(0, STEPS);
        if let Some(r) = &ranged {
            let snapshot = take_snapshot(r, now_filetime(), cancel).map_err(|e| match e {
                LiveError::SourceLost(e) => source_lost(&e),
                LiveError::Cancelled => ProtocolError::new(ErrorCode::Cancelled, "cancelled"),
            })?;
            live.snapshot = Some(snapshot);
        }
        progress(1, STEPS);
        self.heads.attach(ranged);
        let input = AnalysisInput::new(self.session.store(), self.session.meta(), &live);
        let layout = analyze(&input, &mut self.heads, &self.overrides, cancel)
            .map_err(|e| analysis_error(&e))?;
        progress(2, STEPS);
        let entries = self.carved_entries();
        let catalog = Catalog::build(&CatalogInput::new(
            self.session.store(),
            &layout,
            &self.heads,
            &entries,
        ))
        .map_err(|e| ProtocolError::internal(e.to_string()))?;
        progress(3, STEPS);
        if self.heads.misses() > 0 {
            log_info(&format!(
                "{} geometry checks used no head (no source attached); attach the source for the full analysis",
                self.heads.misses()
            ));
        }
        if live != self.live {
            if let Err(e) = live.save(&mut self.session) {
                log_warn(&format!("the LIVE block cannot be stored: {e}"));
            }
            self.live = live;
        }
        self.store_heads();
        self.built = Some(Built { layout, catalog });
        self.recheck_file(source.map(|s| &**s));
        self.checkpoint();
        Ok(())
    }

    fn store_heads(&mut self) {
        if let Err(e) = self.heads.save_new(&mut self.session) {
            log_warn(&format!("the HEADS block cannot be stored: {e}"));
        }
    }

    /// Runs the guard again on the session file with the protected ranges
    /// of now (B3): a file inside a lost volume is detached and the
    /// warning `session_file_at_risk` raised. Without an attached source
    /// (state B) the guard runs against the recorded source located again
    /// ([`Recorded`]): the protected ranges change in B too
    /// (`instance_set`, `geometry_set`).
    fn recheck_file(&mut self, source: Option<&dyn BlockSource>) {
        if !self.session.has_file() {
            return;
        }
        let recorded;
        let source = match source {
            Some(source) => source,
            None => {
                recorded = self.recorded();
                &recorded
            }
        };
        let protected = self.protected();
        let guard = SessionGuard {
            source,
            protected: &protected,
            allow_same_disk: self.consent.same_disk,
            allow_unknown_disk: self.consent.unknown_disk,
        };
        let path = self.session.file_path().map(display_path);
        if let Err(e) = self.session.recheck_file(&guard) {
            let message = match &e {
                SessionError::DestinationRefused(check)
                    if check.verdict == Verdict::OverlapsSource =>
                {
                    "The session file lies inside a lost file system on the disk being rescued. \
                     HDSavior stopped writing to it; save the session to another disk."
                        .to_string()
                }
                _ => format!(
                    "The session file can no longer be written where it is ({e}); save the session elsewhere."
                ),
            };
            log_warn(&format!(
                "session file {} detached: {message}",
                path.unwrap_or_default()
            ));
            self.set_warning(WARN_SESSION_FILE_AT_RISK, message);
        }
    }

    /// The session's source as recorded, located again (state B).
    fn recorded(&self) -> Recorded {
        Recorded::locate(&convert::recorded_source(&self.session.meta().identity))
    }

    fn set_warning(&mut self, code: &str, message: String) {
        self.warnings.retain(|w| w.code != code);
        self.warnings.push(Warning {
            code: code.to_string(),
            message,
        });
    }

    /// Appends what changed to the session file, if one is attached. A
    /// failure raises `session_file_not_saved`; a success clears it.
    pub fn checkpoint(&mut self) {
        if !self.session.has_file() {
            return;
        }
        match self.session.checkpoint() {
            Ok(()) => self
                .warnings
                .retain(|w| w.code != WARN_SESSION_FILE_NOT_SAVED),
            Err(e) => {
                log_warn(&format!("the session file cannot be updated: {e}"));
                self.not_saved(&e.to_string());
            }
        }
    }

    /// Raises `session_file_not_saved` for the attached file.
    pub fn not_saved(&mut self, why: &str) {
        let path = self
            .session
            .file_path()
            .map(display_path)
            .unwrap_or_default();
        self.set_warning(
            WARN_SESSION_FILE_NOT_SAVED,
            format!(
                "The session could not be written to its file {path} ({why}). The file does not \
                 hold the latest state: save the session to another location, or it is lost when \
                 HDSavior closes."
            ),
        );
    }

    /// Whether the last checkpoint of the attached file failed.
    pub fn file_behind(&self) -> bool {
        self.warnings
            .iter()
            .any(|w| w.code == WARN_SESSION_FILE_NOT_SAVED)
    }

    /// Forgets the file warnings after the session was written anew.
    fn file_written(&mut self) {
        self.warnings.retain(|w| {
            w.code != WARN_SESSION_FILE_AT_RISK && w.code != WARN_SESSION_FILE_NOT_SAVED
        });
    }

    /// The `CatalogInfo` of the built catalog (`None` without one).
    pub fn info(&self, source_attached: bool) -> Option<CatalogInfo> {
        let built = self.built.as_ref()?;
        let summary = built.catalog.summary(&self.session, &built.layout);
        Some(convert::catalog_info(
            &summary,
            source_attached,
            &self.warnings,
        ))
    }

    fn built(&self) -> Result<&Built, ProtocolError> {
        self.built.as_ref().ok_or_else(no_catalog)
    }

    /// Writes to the session file at `path` from now on: continues the
    /// file this session was loaded from or last written to, else writes
    /// a compacted copy there (`save_as`, which replaces only a session of
    /// the same source and range).
    fn use_file(
        &mut self,
        path: &Path,
        source: &dyn BlockSource,
        dest: &Destination,
    ) -> Result<(), ProtocolError> {
        let guard = SessionGuard {
            source,
            protected: &dest.protected,
            allow_same_disk: dest.allow_same_disk,
            allow_unknown_disk: dest.allow_unknown_disk,
        };
        match self.session.attach_file(path, &guard) {
            Ok(()) => {}
            Err(e @ SessionError::DestinationRefused(_)) => return Err(convert::session_error(&e)),
            Err(_) => {
                self.session
                    .save_as(path, &guard)
                    .map_err(|e| convert::session_error(&e))?;
            }
        }
        self.consent = Consent::of(dest);
        self.file_written();
        Ok(())
    }

    /// The identity check of B4 against `source`; current-volume records
    /// of the stored snapshot are not sampled (they change while the disk
    /// is in use).
    fn identity(
        &self,
        source: &Arc<dyn BlockSource>,
        force: bool,
    ) -> Result<hds_core::session::IdentityCheck, ProtocolError> {
        let store = self.session.store();
        let empty = LiveSnapshot::default();
        let snapshot = self.live.snapshot.as_ref().unwrap_or(&empty);
        let index = LiveIndex::build(store, snapshot);
        let exclude = |id, _: &hds_core::store::RawRecord| !index.is_old(id);
        check_identity(&self.session, source.clone(), &exclude, force, &never()).map_err(
            |e| match e {
                IdentityError::SourceLost(e) => source_lost(&e),
                IdentityError::Cancelled => ProtocolError::new(ErrorCode::Cancelled, "cancelled"),
                IdentityError::Internal(e) => {
                    ProtocolError::internal(format!("the identity check failed: {e}"))
                }
            },
        )
    }

    /// Checks `source` (B4) and, when it matches, records its path, model
    /// and serial and the `identity_differs` warning.
    fn accept_source(
        &mut self,
        source: &Arc<dyn BlockSource>,
        force: bool,
    ) -> Result<(), ProtocolError> {
        let check = self.identity(source, force)?;
        if !check.matches {
            let data = serde_json::json!({
                "failed": check.failed,
                "samples": check.samples,
                "matched": check.matched,
                "mismatched": check.mismatched,
                "unreadable": check.unreadable,
            });
            let what = if check.failed.contains(&"size") {
                "it is too small for the session range".to_string()
            } else if check.failed.contains(&"too_few_samples") {
                format!(
                    "only {} stored records could be compared (force accepts this)",
                    check.matched
                )
            } else {
                format!(
                    "{} of {} compared records differ",
                    check.mismatched,
                    check.matched.saturating_add(check.mismatched)
                )
            };
            return Err(ProtocolError::new(
                ErrorCode::SourceMismatch,
                format!(
                    "{} is not the source of this session: {what}",
                    source.info().path
                ),
            )
            .with_data(data));
        }
        let info = source.info();
        if info.path != self.session.meta().identity.path
            || info.model != self.session.meta().identity.model
            || info.serial != self.session.meta().identity.serial
        {
            if let Err(e) = self.session.set_identity(SourceIdentity::from(&info)) {
                log_warn(&format!(
                    "the session keeps its recorded source identity: {e}"
                ));
            }
        }
        self.warnings.retain(|w| w.code != WARN_IDENTITY_DIFFERS);
        self.warnings.extend(check.warnings);
        Ok(())
    }

    /// Continues writing to the file this session was loaded from or last
    /// written to, when the guard allows it without consents (best effort:
    /// otherwise changes stay in memory until `session_save`).
    fn reattach_file(&mut self, source: &dyn BlockSource) {
        if self.session.has_file() {
            return;
        }
        let Some(path) = self.session.last_file().map(Path::to_path_buf) else {
            return;
        };
        let protected = self.protected();
        let guard = SessionGuard {
            source,
            protected: &protected,
            allow_same_disk: false,
            allow_unknown_disk: false,
        };
        match self.session.attach_file(&path, &guard) {
            Ok(()) => {
                self.consent = Consent::default();
                self.checkpoint();
            }
            Err(SessionError::DestinationRefused(check))
                if check.verdict == Verdict::OverlapsSource =>
            {
                // B3: a session file inside a lost volume is not written to.
                log_warn(&format!(
                    "the session file {} lies inside the rescued area and is not written to",
                    path.display()
                ));
                self.set_warning(
                    WARN_SESSION_FILE_AT_RISK,
                    "The session file lies inside a lost file system on the disk being rescued. \
                     HDSavior does not write to it; save the session to another disk."
                        .to_string(),
                );
            }
            Err(e) => log_info(&format!(
                "the session file {} is not written to ({e}); session_save keeps the changes",
                path.display()
            )),
        }
    }
}

/// The files of a carved set as the catalog takes them.
fn carved_entries(set: &CarvedSet) -> Vec<CarvedEntry> {
    set.files()
        .iter()
        .map(|f| CarvedEntry {
            offset: f.offset(),
            len: f.len(),
            ext: f.ext().to_string(),
            end_verified: f.end_verified(),
        })
        .collect()
}

/// What the backend holds.
#[derive(Default)]
pub struct State {
    /// The attached source.
    pub source: Option<Arc<dyn BlockSource>>,
    /// The session (`None` without one, and while a scan runs).
    pub work: Option<Work>,
}

impl State {
    fn work(&self) -> Result<&Work, ProtocolError> {
        self.work.as_ref().ok_or_else(no_session)
    }

    fn work_mut(&mut self) -> Result<&mut Work, ProtocolError> {
        self.work.as_mut().ok_or_else(no_session)
    }
}

/// The state shared with job threads.
#[derive(Default)]
pub struct Shared {
    state: Mutex<State>,
}

impl Shared {
    /// Locks the state (a poisoned lock is taken over: every change is
    /// made so that a panic leaves a usable state).
    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The backend of `hds-core` and `hds-recover`.
#[derive(Default, Clone)]
pub struct RealBackend {
    shared: Arc<Shared>,
}

impl RealBackend {
    /// A backend without source or session.
    pub fn new() -> RealBackend {
        RealBackend::default()
    }

    /// The state shared with jobs.
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Locks the state.
    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.lock()
    }

    fn with_built<T>(
        &self,
        f: impl FnOnce(&Session, &Built) -> Result<T, ProtocolError>,
    ) -> Result<T, ProtocolError> {
        let st = self.lock();
        let work = st.work.as_ref().ok_or_else(no_catalog)?;
        let built = work.built()?;
        f(&work.session, built)
    }

    /// Finds the session's source for `session_load`: the one given, else
    /// the recorded path, else (disks) every disk of `list_disks`. Returns
    /// the first that matches (B4), or the reason the first candidate
    /// failed.
    fn find_source(
        work: &mut Work,
        requested: Option<&SourceRef>,
    ) -> (Option<Arc<dyn BlockSource>>, Option<ProtocolError>) {
        let identity = work.session.meta().identity.clone();
        let mut candidates: Vec<(SourceKind, String)> = Vec::new();
        match requested {
            Some(r) => candidates.push((r.kind, r.path.clone())),
            None => {
                candidates.push((identity.kind, identity.path.clone()));
                if identity.kind == SourceKind::Disk {
                    for disk in hds_io::list_disks() {
                        if disk.path != identity.path {
                            candidates.push((SourceKind::Disk, disk.path));
                        }
                    }
                }
            }
        }
        let mut first_error: Option<ProtocolError> = None;
        for (kind, path) in candidates {
            let sector = (kind == SourceKind::Image).then_some(identity.sector_size);
            let opened = match open_source(kind, Path::new(&path), sector) {
                Ok(s) => s,
                Err(e) => {
                    first_error.get_or_insert_with(|| {
                        ProtocolError::from_io(&format!("cannot open the source {path}"), &e)
                    });
                    continue;
                }
            };
            match work.accept_source(&opened, false) {
                Ok(()) => return (Some(opened), None),
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        (
            None,
            Some(first_error.unwrap_or_else(|| {
                ProtocolError::new(ErrorCode::NoSource, "no matching source was found")
            })),
        )
    }

    /// The error of a failed `instance_set` / `geometry_set`. After
    /// [`InstanceSetError::Restore`] the layout is rebuilt from the session
    /// (no reads: the source is gone) and the catalog with it; the error
    /// then carries [`CATALOG_RELOADED`] so that the server starts a new
    /// epoch.
    fn change_error(work: &mut Work, e: InstanceSetError) -> ProtocolError {
        match e {
            InstanceSetError::NotFound => {
                ProtocolError::new(ErrorCode::NotFound, "no such instance")
            }
            InstanceSetError::Strays => {
                ProtocolError::bad_request("the stray records cannot be changed")
            }
            InstanceSetError::Session(e) => convert::session_error(&e),
            InstanceSetError::Analysis(e) => analysis_error(&e),
            InstanceSetError::Build(e) => ProtocolError::internal(e.to_string()),
            InstanceSetError::Restore(e) => {
                let error = analysis_error(&e);
                log_warn(&format!(
                    "the catalog could not be restored ({e}); rebuilding it from the session"
                ));
                let rebuilt = work.build(None, &never(), &mut |_, _| {});
                if let Err(r) = rebuilt {
                    log_warn(&format!("the catalog could not be rebuilt: {}", r.message));
                    work.built = None;
                }
                ProtocolError::new(
                    error.code,
                    format!(
                        "{}; the catalog was rebuilt from the session (new epoch)",
                        error.message
                    ),
                )
                .with_data(serde_json::json!({ CATALOG_RELOADED: true }))
            }
        }
    }

    /// Discards the carved files (I5) in the set and the session.
    fn discard_carved(work: &mut Work) {
        work.carved = CarvedSet::new();
        if let Err(e) = discard_carved(&mut work.session) {
            log_warn(&format!(
                "the carved files cannot be discarded in the session: {e}"
            ));
        }
    }
}

/// `instance_set` with `"other"` on the main instance hands `main` to the
/// instance D8 picks next: an `older` instance, or an `other` one the user
/// did not set. Without such an instance no file system would be `main`
/// (and `instances[0]` would not be the main instance): `bad_request`.
fn refuse_orphaned_main(built: &Built, instance: u32) -> Result<(), ProtocolError> {
    use hds_core::instances::InstanceKind as K;
    let instances = built.catalog.instances();
    let is_main = instances
        .iter()
        .any(|i| i.id == instance && i.kind == K::Main);
    let successor = instances
        .iter()
        .any(|i| i.id != instance && matches!(i.kind, K::Older | K::Other) && !i.manual);
    if is_main && !successor {
        return Err(ProtocolError::bad_request(
            "no other file system can become the main one; choose \"main\" for another \
             instance instead",
        ));
    }
    Ok(())
}

impl Backend for RealBackend {
    fn scan_start(&self, request: ScanRequest) -> Result<ScanJob, ProtocolError> {
        scan::start(&self.shared, request)
    }

    fn session_save(&self, dest: Destination) -> Result<SessionSaveResult, ProtocolError> {
        let mut st = self.lock();
        let source = st.source.clone();
        let work = st.work_mut()?;
        // State B: the recorded source, located again (S2).
        let recorded;
        let located: &dyn BlockSource = match &source {
            Some(source) => &**source,
            None => {
                recorded = work.recorded();
                &recorded
            }
        };
        let guard = SessionGuard {
            source: located,
            protected: &dest.protected,
            allow_same_disk: dest.allow_same_disk,
            allow_unknown_disk: dest.allow_unknown_disk,
        };
        let report = work
            .session
            .save_as(Path::new(&dest.path), &guard)
            .map_err(|e| convert::session_error(&e))?;
        work.consent = Consent::of(&dest);
        work.file_written();
        Ok(SessionSaveResult {
            path: display_path(&report.path),
            bytes: report.bytes,
        })
    }

    fn session_load(
        &self,
        args: &SessionLoadArgs,
        task: &TaskContext,
    ) -> Result<LoadedSession, ProtocolError> {
        task.progress(0.0);
        let path = Path::new(&args.path);
        let (session, report) = Session::load(path).map_err(|e| convert::session_error(&e))?;
        if report.ignored_bytes > 0 {
            log_info(&format!(
                "{}: {} bytes after the last checkpoint are ignored ({})",
                args.path,
                report.ignored_bytes,
                report.ignored_reason.clone().unwrap_or_default()
            ));
        }
        let mut work = Work::loaded(session)?;
        if report.ignored_bytes > 0 {
            work.set_warning(
                WARN_SESSION_INCOMPLETE,
                format!(
                    "The session file ends early: {} bytes after its last complete checkpoint \
                     could not be used ({}). The session holds what was saved until then; a \
                     scan resumed from it reads the rest again.",
                    report.ignored_bytes,
                    report.ignored_reason.clone().unwrap_or_default()
                ),
            );
        }
        task.progress(0.1);
        let (mut source, mut source_error) = Self::find_source(&mut work, args.source.as_ref());
        task.progress(0.3);
        let mut step = |k: u64, n: u64| {
            let f = if n == 0 { 1.0 } else { k as f64 / n as f64 };
            task.progress(0.3 + 0.7 * f);
        };
        if let Err(e) = work.build(source.as_ref(), &never(), &mut step) {
            if source.is_none() {
                return Err(e);
            }
            // The source failed while the catalog was built: browse
            // without it.
            source = None;
            source_error = Some(e);
            work.build(None, &never(), &mut step)?;
        }
        let collect_carve_hits = work.session.meta().options.collect_carve_hits;
        let source_info = source.as_ref().map_or_else(
            || convert::recorded_source(&work.session.meta().identity),
            |s| s.info(),
        );
        let mut st = self.lock();
        // The previous session goes (and lets go of its file) first.
        st.work = None;
        st.source = None;
        if let Some(s) = &source {
            work.reattach_file(&**s);
        }
        let mut catalog = work.info(source.is_some()).ok_or_else(no_catalog)?;
        if catalog.scanned_bytes == 0
            && !work
                .warnings
                .iter()
                .any(|w| w.code == WARN_SESSION_INCOMPLETE)
        {
            work.set_warning(
                WARN_SESSION_INCOMPLETE,
                "The session file holds no scanned data: the scan stopped (or the file was cut \
                 off) before its first checkpoint. Resume the scan to read the source."
                    .to_string(),
            );
            catalog = work.info(source.is_some()).ok_or_else(no_catalog)?;
        }
        st.work = Some(work);
        st.source = source.clone();
        task.progress(1.0);
        Ok(LoadedSession {
            result: SessionLoadResult {
                source: source_info,
                source_attached: source.is_some(),
                source_error,
                catalog,
            },
            source,
            collect_carve_hits,
        })
    }

    fn session_attach_source(
        &self,
        source: Arc<dyn BlockSource>,
        force: bool,
    ) -> Result<SessionAttachResult, ProtocolError> {
        let mut st = self.lock();
        let work = st.work_mut()?;
        work.accept_source(&source, force)?;
        work.build(Some(&source), &never(), &mut |_, _| {})?;
        work.reattach_file(&*source);
        let catalog = work.info(true).ok_or_else(no_catalog)?;
        st.source = Some(source.clone());
        Ok(SessionAttachResult {
            source: source.info(),
            catalog,
        })
    }

    fn close_session(&self) {
        let mut st = self.lock();
        st.work = None;
        st.source = None;
    }

    fn detach_source(&self) {
        let mut st = self.lock();
        st.source = None;
        if let Some(work) = &mut st.work {
            work.heads.attach(None);
        }
    }

    fn catalog_info(&self) -> Result<CatalogInfo, ProtocolError> {
        let st = self.lock();
        let attached = st.source.is_some();
        st.work
            .as_ref()
            .and_then(|w| w.info(attached))
            .ok_or_else(no_catalog)
    }

    fn geometry_set(
        &self,
        args: &GeometrySetArgs,
        task: &TaskContext,
    ) -> Result<CatalogInfo, ProtocolError> {
        task.progress(0.0);
        let mut st = self.lock();
        let source = st.source.clone();
        let work = st.work_mut()?;
        let cluster_size = u32::try_from(args.cluster_size)
            .map_err(|_| ProtocolError::bad_request("cluster_size is too large"))?;
        let ranged = source.as_ref().map(|s| work.ranged(s)).transpose()?;
        work.heads.attach(ranged);
        let Work {
            session,
            heads,
            overrides,
            built,
            ..
        } = work;
        let built_ref = built.as_mut().ok_or_else(no_catalog)?;
        let result = catalog::geometry_set(
            session,
            &mut built_ref.layout,
            overrides,
            heads,
            &built_ref.catalog,
            args.instance,
            (cluster_size, args.volume_offset),
            &never(),
        );
        match result {
            Ok((catalog, main_changed)) => {
                let catalog = if main_changed {
                    Self::discard_carved(work);
                    catalog
                        .with_carved(work.session.store(), &[])
                        .map_err(|e| ProtocolError::internal(e.to_string()))?
                } else {
                    catalog
                };
                if let Some(built) = &mut work.built {
                    built.catalog = catalog;
                }
                work.store_heads();
                work.recheck_file(source.as_deref());
                work.checkpoint();
                task.progress(1.0);
                work.info(source.is_some()).ok_or_else(no_catalog)
            }
            Err(e) => Err(Self::change_error(work, e)),
        }
    }

    fn instance_set(&self, args: &InstanceSetArgs) -> Result<CatalogInfo, ProtocolError> {
        let mut st = self.lock();
        let source = st.source.clone();
        let work = st.work_mut()?;
        let ranged = source.as_ref().map(|s| work.ranged(s)).transpose()?;
        work.heads.attach(ranged);
        let choice = match args.kind {
            InstanceChoice::Main => Some(KindChoice::Main),
            InstanceChoice::Other => Some(KindChoice::Other),
            InstanceChoice::Auto => None,
        };
        if choice == Some(KindChoice::Other) {
            refuse_orphaned_main(work.built()?, args.instance)?;
        }
        let Work {
            session,
            heads,
            overrides,
            built,
            ..
        } = work;
        let built_ref = built.as_mut().ok_or_else(no_catalog)?;
        let result = catalog::instance_set(
            session,
            &mut built_ref.layout,
            overrides,
            heads,
            &built_ref.catalog,
            args.instance,
            choice,
            &never(),
        );
        match result {
            Ok(catalog) => {
                // I5: instance_set discards the carved files (the new
                // catalog has none).
                Self::discard_carved(work);
                if let Some(built) = &mut work.built {
                    built.catalog = catalog;
                }
                work.store_heads();
                work.recheck_file(source.as_deref());
                work.checkpoint();
                work.info(source.is_some()).ok_or_else(no_catalog)
            }
            Err(e) => Err(Self::change_error(work, e)),
        }
    }

    fn list_children(&self, args: &ListChildrenArgs) -> Result<RowsPage, ProtocolError> {
        self.with_built(|session, built| {
            let list = ListArgs {
                node: args.node,
                offset: args.offset,
                limit: args.limit,
                sort: convert::core_sort(args.sort),
                desc: args.desc,
                dirs_only: args.dirs_only,
                show_deleted: args.show_deleted,
                exts: args.exts.clone(),
                ratings: convert::core_ratings(&args.ratings),
            };
            built
                .catalog
                .list_children(session.store(), &list)
                .map(|p| convert::rows_page(&p))
                .map_err(|e| convert::query_error(&e))
        })
    }

    fn node_info(&self, args: &NodeInfoArgs) -> Result<NodeInfo, ProtocolError> {
        self.with_built(|session, built| {
            built
                .catalog
                .node_info(session.store(), args.node)
                .map(|d| convert::node_info(&d))
                .map_err(|e| convert::query_error(&e))
        })
    }

    fn search(&self, args: &SearchArgs) -> Result<SearchPage, ProtocolError> {
        self.with_built(|session, built| {
            let search = CoreSearchArgs {
                under: args.under,
                filter: convert::core_filter(&args.filter),
                show_deleted: args.show_deleted,
                sort: convert::core_sort(args.sort),
                desc: args.desc,
                offset: args.offset,
                limit: args.limit,
            };
            built
                .catalog
                .search(session.store(), &search)
                .map(|p| convert::search_page(&p))
                .map_err(|e| convert::query_error(&e))
        })
    }

    fn selection_summary(&self, args: &SelectionArgs) -> Result<SelectionSummary, ProtocolError> {
        self.with_built(|_, built| {
            let selection = Selection {
                include: args.include.clone(),
                exclude: args.exclude.clone(),
                filter: args.filter.as_ref().map(convert::core_filter),
                include_deleted: args.include_deleted,
                skip_damaged: args.skip_damaged,
            };
            built
                .catalog
                .selection_summary(&selection)
                .map(|s| convert::selection_summary(&s))
                .map_err(|e| convert::query_error(&e))
        })
    }

    fn files_in_extents(&self, extents: &[Range<u64>]) -> (u64, u64) {
        self.with_built(|_, built| Ok(built.catalog.files_in_extents(extents)))
            .unwrap_or((0, 0))
    }

    fn recover_start(&self, request: RecoverRequest) -> Result<RecoverJob, ProtocolError> {
        jobs::recover_start(&self.shared, request)
    }

    fn carve_start(&self, request: CarveRequest) -> Result<CarveJob, ProtocolError> {
        jobs::carve_start(&self.shared, request)
    }

    fn image_start(&self, request: ImageRequest) -> Result<ImageJob, ProtocolError> {
        jobs::image_start(&self.shared, request)
    }

    fn read_file(&self, args: &ReadFileArgs) -> Result<FileBytes, ProtocolError> {
        // What the read needs is taken under the lock; the read itself runs
        // without it, so that a slow or failing disk does not hold up a
        // running recovery (its G1 updates take the lock after every file).
        let stop = Arc::new(AtomicBool::new(false));
        let (catalog, id, file, ext, ranged) = {
            let st = self.lock();
            let source = st.source.clone().ok_or_else(no_source)?;
            let work = st.work()?;
            let built = work.built()?;
            let store = work.session.store();
            let id = NodeId::from_wire(args.node)
                .filter(|&n| built.catalog.contains(n))
                .ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::NotFound,
                        format!("node {} does not exist", args.node),
                    )
                })?;
            built
                .catalog
                .readable(id)
                .map_err(|e| convert::query_error(&e))?;
            let (file, _) = built
                .catalog
                .file_source(store, id.0)
                .map_err(|e| convert::query_error(&e))?;
            let ext = built
                .catalog
                .row(store, id, true)
                .map(|r| r.ext)
                .unwrap_or_default();
            let range = work.session.meta().range.clone();
            let deadline: Arc<dyn BlockSource> =
                Arc::new(Deadline::new(source, READ_FILE_DEADLINE, stop.clone()));
            let ranged = RangedSource::new(deadline, range).map_err(|e| {
                ProtocolError::new(
                    ErrorCode::SourceMismatch,
                    format!("the source does not hold the session range: {e}"),
                )
            })?;
            (built.catalog.clone(), id, file, ext, ranged)
        };
        let length = usize::try_from(args.length).unwrap_or(usize::MAX);
        let preview = read_preview(&ranged, &file, &ext, args.offset, length, &stop).map_err(
            |e| match e {
                PreviewError::TooLong { requested } => ProtocolError::bad_request(format!(
                    "length {requested} is more than read_file returns at once"
                )),
                PreviewError::Read(ReadError::SourceLost(e)) => source_lost(&e),
                PreviewError::Read(ReadError::Cancelled) => ProtocolError::new(
                    ErrorCode::IoError,
                    format!(
                        "reading stopped after {} s: this part of the file lies in an area of \
                         the disk that is very slow to read (bad sectors); recovering the file \
                         reads it completely",
                        READ_FILE_DEADLINE.as_secs()
                    ),
                ),
                PreviewError::Read(ReadError::RecordChanged) => ProtocolError::new(
                    ErrorCode::IoError,
                    "the file's record changed on the disk since the scan",
                ),
                PreviewError::Read(e) => {
                    ProtocolError::new(ErrorCode::IoError, format!("the file cannot be read: {e}"))
                }
            },
        )?;
        // What the read found (signature at offset 0, bad sectors) joins
        // the file's flags (G1); a catalog replaced meanwhile ignores it.
        catalog.observe(id, preview.flags);
        Ok(FileBytes {
            data: preview.data,
            size: preview.size,
            eof: preview.eof,
        })
    }

    fn session_source(&self) -> Option<SourceInfo> {
        let st = self.lock();
        st.work
            .as_ref()
            .map(|w| convert::recorded_source(&w.session.meta().identity))
    }

    fn session_sector_size(&self) -> Option<u32> {
        let st = self.lock();
        st.work
            .as_ref()
            .map(|w| w.session.meta().identity.sector_size)
    }
}
