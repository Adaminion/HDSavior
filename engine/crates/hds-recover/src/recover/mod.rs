//! Recovery of a [`Plan`] into a destination directory (ALGORITHMS H3 to
//! H12).
//!
//! A job has two steps, so that the protocol's `recover_start` can answer
//! before any event of the job is sent:
//!
//! 1. [`Recovery::prepare`] checks the plan, runs the destination guard on
//!    the path, creates the destination directory, runs the guard again on
//!    the created directory, removes leftover part files below it, reads the
//!    limits of its file system, assigns every output name (ALGORITHMS H4)
//!    and creates the report (H10). A refusal is returned as
//!    [`StartError`].
//! 2. [`Recovery::run`] creates the folders, writes the files in ascending
//!    order of their first data byte (H5) through part files (H6), reading
//!    them by the rules of H7, sets file times on the writing handle and
//!    folder times bottom-up at the end (H8), verifies the first bytes (H9),
//!    appends one report row per file (H10), and ends early once on a full
//!    destination, a lost source or a cancel (H12).
//!
//! [`recover`] does both.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use hds_io::out::{classify_error, FsLimits, OnExists, OutDir, OutErrorKind};
use hds_io::{
    destination_check, ComponentLimit, DestinationCheck, Protected, Purpose, RangedSource,
};
use serde::Serialize;

use crate::health::{rating_reasons, Flag, FlagSet, Rating, RatingContext};
use crate::plan::{LazyPlan, Plan, PlanFiles, PlanFolder};
use crate::report::ReportFile;

mod job;
mod tree;

#[cfg(test)]
mod tests;

use tree::{Tree, TreeError};

/// How a recovery job writes and what it may write to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverOptions {
    /// What to do when a final name exists and was not produced by this run.
    pub on_exists: OnExists,
    /// Files rated `damaged` before anything is read are skipped
    /// (`skipped:damaged`).
    pub skip_damaged: bool,
    /// Consent to a destination on the source disk outside the protected
    /// ranges (S3).
    pub allow_same_disk: bool,
    /// Consent to a destination whose disk cannot be determined (S3).
    pub allow_unknown_disk: bool,
    /// The byte ranges of the source no output may touch (S2): the session
    /// range first, then the lost-volume ranges. The range of the source
    /// given to the job is put first if the list does not start with it.
    pub protected: Vec<Protected>,
}

impl RecoverOptions {
    /// The protocol's defaults (`on_exists: "rename"`, no skipping, no
    /// consents) with the given protected ranges.
    pub fn new(protected: Vec<Protected>) -> RecoverOptions {
        RecoverOptions {
            on_exists: OnExists::Rename,
            skip_damaged: false,
            allow_same_disk: false,
            allow_unknown_disk: false,
            protected,
        }
    }
}

/// Why a recovery job did not start.
#[derive(Debug)]
pub enum StartError {
    /// The destination guard does not allow the location (protocol
    /// `destination_refused`, with this check as `data`).
    DestinationRefused(Box<DestinationCheck>),
    /// The plan contradicts itself: a folder id used twice, a parent or
    /// folder that does not exist, folders whose parents form a cycle (an
    /// internal error of the caller).
    InvalidPlan(String),
    /// The destination is full: the destination directory or the report
    /// could not be created for lack of space (protocol `no_space`).
    NoSpace(io::Error),
    /// The destination directory or the report could not be created, or the
    /// destination could not be read (protocol `io_error`).
    Io(io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::DestinationRefused(check) => {
                write!(f, "the destination is refused: {}", check.message)
            }
            StartError::InvalidPlan(why) => write!(f, "invalid recovery plan: {why}"),
            StartError::NoSpace(e) => write!(f, "the destination is full: {e}"),
            StartError::Io(e) => write!(f, "the destination cannot be prepared: {e}"),
        }
    }
}

impl std::error::Error for StartError {}

fn start_io_error(e: io::Error) -> StartError {
    if classify_error(&e) == OutErrorKind::NoSpace {
        StartError::NoSpace(e)
    } else {
        StartError::Io(e)
    }
}

/// Why a file was not written (`skipped:<reason>`).
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Stored compressed.
    Compressed,
    /// Encrypted.
    Encrypted,
    /// A cloud placeholder or deduplication stub.
    Placeholder,
    /// Inside a disk image on the drive (browse only).
    Embedded,
    /// Rated `damaged` and `skip_damaged` was asked for.
    Damaged,
    /// The final name exists and `on_exists` is `skip`.
    Exists,
    /// Larger than the destination file system can hold.
    TooLarge,
    /// The output path would be longer than 32,767 UTF-16 units.
    PathTooLong,
}

impl SkipReason {
    /// The wire name.
    pub const fn name(self) -> &'static str {
        match self {
            SkipReason::Compressed => "compressed",
            SkipReason::Encrypted => "encrypted",
            SkipReason::Placeholder => "placeholder",
            SkipReason::Embedded => "embedded",
            SkipReason::Damaged => "damaged",
            SkipReason::Exists => "exists",
            SkipReason::TooLarge => "too_large",
            SkipReason::PathTooLong => "path_too_long",
        }
    }
}

/// Why a file failed (`failed:<code>`): nothing of it is left at the
/// destination.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    /// Resident content whose record changed since the scan.
    RecordChanged,
    /// The destination refused a write, a rename or a folder.
    WriteError,
    /// The content could not be read for a reason that is neither a bad
    /// sector nor a lost device (or its record could not be verified).
    IoError,
}

impl FailReason {
    /// The wire name.
    pub const fn name(self) -> &'static str {
        match self {
            FailReason::RecordChanged => "record_changed",
            FailReason::WriteError => "write_error",
            FailReason::IoError => "io_error",
        }
    }
}

/// The status of one file after the attempt (ALGORITHMS H11). Shown as
/// `ok`, `ok_unverified`, `warning:<flags>`, `skipped:<reason>`,
/// `failed:<code>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileStatus {
    /// Written, rating `good`, first bytes match the extension.
    Ok,
    /// Written, rating `good`, no opinion about the extension (or empty).
    OkUnverified,
    /// Written, rating after the read is not `good`. Lists what keeps it
    /// from `good` ([`rating_reasons`]).
    Warning {
        /// All flags of the file after the read.
        flags: FlagSet,
        /// The conditions of the rating that are not flags.
        context: RatingContext,
    },
    /// Not written.
    Skipped(SkipReason),
    /// Not written, nothing left at the destination.
    Failed(FailReason),
}

impl FileStatus {
    /// Whether this is one of the two `ok` statuses.
    pub fn is_ok(&self) -> bool {
        matches!(self, FileStatus::Ok | FileStatus::OkUnverified)
    }

    /// Whether the file was written (`ok`, `ok_unverified`, `warning`).
    pub fn is_written(&self) -> bool {
        matches!(
            self,
            FileStatus::Ok | FileStatus::OkUnverified | FileStatus::Warning { .. }
        )
    }
}

impl fmt::Display for FileStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileStatus::Ok => f.write_str("ok"),
            FileStatus::OkUnverified => f.write_str("ok_unverified"),
            FileStatus::Warning { flags, context } => {
                write!(f, "warning:{}", rating_reasons(*flags, *context).join(";"))
            }
            FileStatus::Skipped(reason) => write!(f, "skipped:{}", reason.name()),
            FileStatus::Failed(reason) => write!(f, "failed:{}", reason.name()),
        }
    }
}

impl Serialize for FileStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// What happened to one file; handed to [`RecoverObserver::file_done`] in
/// the order the files are processed, after its report row is written.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct FileOutcome {
    /// The plan's node id.
    pub node: u64,
    /// The status.
    pub status: FileStatus,
    /// All flags after the attempt: the plan's flags, what the read met, and
    /// the signature check of the bytes written (for files not read: the
    /// plan's flags).
    pub flags: FlagSet,
    /// The rating of `flags` in the file's rating context.
    pub rating: Rating,
    /// Path of the output file relative to the destination, with the
    /// platform's separators: where the file is (written files), where it
    /// would have gone (others).
    pub output_path: String,
    /// Bytes of the output file.
    pub bytes_written: u64,
    /// The report row's `detail`.
    pub detail: String,
}

/// A `recover_progress` event.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct RecoverProgress {
    /// Files finished (in any status).
    pub files_done: u64,
    /// Files in the plan.
    pub files_total: u64,
    /// Bytes of the files to be attempted that are done.
    pub bytes_done: u64,
    /// Bytes of the files to be attempted: not unsupported, not damaged when
    /// `skip_damaged` (the numbers of `selection_summary`).
    pub bytes_total: u64,
    /// Display path of the file being worked on.
    pub current: String,
    /// Bytes per second over the last five seconds.
    pub speed_bps: u64,
    /// Remaining bytes / speed, rounded up; `None` at speed 0.
    pub eta_seconds: Option<u64>,
    /// Whole seconds since the job started.
    pub elapsed_seconds: u64,
    /// Files `ok` or `ok_unverified`.
    pub ok: u64,
    /// Files with a warning.
    pub warnings: u64,
    /// Files failed.
    pub failed: u64,
    /// Files skipped.
    pub skipped: u64,
}

/// How a job ended. Wire form: `"completed"`, `"cancelled"`, `"failed"`.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Every file was attempted.
    Completed,
    /// The job was cancelled.
    Cancelled,
    /// The job ended early; `error` says why.
    Failed,
}

/// Why a job failed: protocol error codes.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum JobError {
    /// The destination is full.
    NoSpace,
    /// The source stopped answering (S7).
    SourceLost,
    /// The destination failed in a way that ends the job (the report or
    /// the image could not be written).
    IoError,
    /// An internal error.
    Internal,
}

impl JobError {
    /// The wire name.
    pub const fn name(self) -> &'static str {
        match self {
            JobError::NoSpace => "no_space",
            JobError::SourceLost => "source_lost",
            JobError::IoError => "io_error",
            JobError::Internal => "internal",
        }
    }
}

/// The `recover_done` event.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct RecoverSummary {
    /// How the job ended.
    pub status: JobStatus,
    /// Why it failed (`status == Failed`).
    pub error: Option<JobError>,
    /// Files in the plan.
    pub files_total: u64,
    /// Files `ok` or `ok_unverified`.
    pub ok: u64,
    /// Files with a warning.
    pub warnings: u64,
    /// Files failed.
    pub failed: u64,
    /// Files skipped. `ok + warnings + failed + skipped = files_total` when
    /// the job completed.
    pub skipped: u64,
    /// Bytes of all files written.
    pub bytes_written: u64,
    /// The destination as the caller named it.
    pub dest: String,
    /// The report, as a display path.
    pub report_path: String,
    /// A sentence about the error, for the log; not part of the event.
    #[serde(skip)]
    pub message: Option<String>,
}

/// Receives the events of a recovery job.
pub trait RecoverObserver {
    /// A progress event (at most four per second).
    fn progress(&mut self, progress: &RecoverProgress);

    /// One file is finished and its report row written. The default does
    /// nothing; `hds-core` can update its catalog from it (the signature
    /// flags and bad sectors found by the read, ALGORITHMS G1).
    fn file_done(&mut self, outcome: &FileOutcome) {
        let _ = outcome;
    }
}

/// An observer that ignores everything.
impl RecoverObserver for () {
    fn progress(&mut self, _: &RecoverProgress) {}
}

/// `path` without the `\\?\` prefix Windows paths of the output primitives
/// carry, for display.
pub(crate) fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.into_owned()
    }
}

/// The first flag of the `unsupported` group, in the order of H11's
/// reasons, as a skip reason.
fn unsupported_reason(flags: FlagSet) -> Option<SkipReason> {
    [
        (Flag::Compressed, SkipReason::Compressed),
        (Flag::Encrypted, SkipReason::Encrypted),
        (Flag::Placeholder, SkipReason::Placeholder),
        (Flag::Embedded, SkipReason::Embedded),
    ]
    .into_iter()
    .find_map(|(flag, reason)| flags.contains(flag).then_some(reason))
}

/// Whether the job will try to write a file with these flags: the files
/// `bytes_total` counts.
fn will_attempt(flags: FlagSet, context: RatingContext, skip_damaged: bool) -> bool {
    match crate::health::rate(flags, context) {
        Rating::Unsupported => false,
        Rating::Damaged => !skip_damaged,
        Rating::Good | Rating::Uncertain => true,
    }
}

/// The limits assumed when the destination does not report its own: 255
/// UTF-16 units per name on Windows, 255 bytes of UTF-8 elsewhere, no
/// practical file-size limit.
pub(crate) fn fallback_limits() -> FsLimits {
    FsLimits {
        max_component: if cfg!(windows) {
            ComponentLimit::Utf16Units(255)
        } else {
            ComponentLimit::Utf8Bytes(255)
        },
        max_file_bytes: None,
        fs_name: None,
    }
}

/// Limits of the destination that tests replace.
#[derive(Debug, Clone, Copy, Default)]
struct Overrides {
    /// The component limit names are fitted to (the destination still
    /// enforces its own).
    component: Option<ComponentLimit>,
    /// The largest file the destination is said to hold.
    max_file_bytes: Option<u64>,
}

/// A recovery job (ALGORITHMS H3 to H12), in two steps so that the
/// protocol's `recover_start` can answer before any event of the job is
/// sent:
///
/// 1. [`Recovery::prepare`] (or [`Recovery::prepare_lazy`]) checks the
///    destination: runs the destination guard on the path, creates the
///    destination directory, runs the guard again on the created directory,
///    removes leftover part files below it, reads the limits of its file
///    system and creates the report (H10). [`Recovery::prepare`] also checks
///    the plan and assigns every output name (H4) at this point. A refusal
///    is a [`StartError`]; nothing is read from the source.
/// 2. [`Recovery::run`] (for a lazy plan: first checks the plan, orders the
///    files and assigns the names, one output directory at a time; a plan
///    that contradicts itself then ends the job as failed with `internal`)
///    creates the folders, writes the files in ascending order of their
///    first data byte (H5) through part files (H6), reading them by the
///    rules of H7, sets file times on the writing handle and folder times
///    bottom-up at the end (H8), checks the first bytes (H9), appends one
///    report row per file (H10), and ends early once on a full destination,
///    a lost source or a cancel (H12).
///
/// A prepared job owns everything it needs, so it can be moved to a worker
/// thread. [`recover`] does both steps.
pub struct Recovery {
    source: RangedSource,
    folders: Vec<PlanFolder>,
    files: Box<dyn PlanFiles>,
    options: RecoverOptions,
    dest_display: String,
    root: OutDir,
    limits: FsLimits,
    limit: ComponentLimit,
    check: DestinationCheck,
    /// `None` until the names are assigned (a lazy plan: in `run`).
    tree: Option<Tree>,
    report: ReportFile,
    bytes_total: u64,
    files_total: u64,
}

impl fmt::Debug for Recovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recovery")
            .field("dest", &self.dest_display)
            .field("folders", &self.folders.len())
            .field("files", &self.files_total)
            .field("bytes", &self.bytes_total)
            .finish()
    }
}

impl Recovery {
    /// Checks the plan and the destination and prepares the job (step 1
    /// above). `dest` is the destination directory as the user gave it: an
    /// absolute path, created if missing. Nothing is read from the source.
    pub fn prepare(
        source: RangedSource,
        plan: Plan,
        dest: &Path,
        options: RecoverOptions,
    ) -> Result<Recovery, StartError> {
        Self::prepare_with(source, plan, dest, options, Overrides::default())
    }

    /// Checks the destination and prepares the job for a plan whose files
    /// are described when they are needed: the plan is checked, the files
    /// ordered and the names assigned when [`run`](Self::run) starts, so
    /// that this step costs O(files) only for the totals
    /// ([`PlanFiles::facts`]).
    pub fn prepare_lazy(
        source: RangedSource,
        plan: LazyPlan,
        dest: &Path,
        options: RecoverOptions,
    ) -> Result<Recovery, StartError> {
        Self::prepare_parts(source, plan, false, dest, options, Overrides::default())
    }

    /// [`prepare`](Self::prepare) with limits of the destination replaced:
    /// tests reach the fallback name and the size limit this way.
    fn prepare_with(
        source: RangedSource,
        plan: Plan,
        dest: &Path,
        options: RecoverOptions,
        overrides: Overrides,
    ) -> Result<Recovery, StartError> {
        Self::prepare_parts(source, plan.into(), true, dest, options, overrides)
    }

    fn prepare_parts(
        source: RangedSource,
        plan: LazyPlan,
        eager: bool,
        dest: &Path,
        options: RecoverOptions,
        overrides: Overrides,
    ) -> Result<Recovery, StartError> {
        let LazyPlan { folders, mut files } = plan;
        let never = AtomicBool::new(false);
        let mut tree = if eager {
            Some(
                Tree::build(&folders, &mut *files, &never).map_err(|e| match e {
                    TreeError::Invalid(why) => StartError::InvalidPlan(why),
                    TreeError::Cancelled => StartError::InvalidPlan("cancelled".into()),
                })?,
            )
        } else {
            None
        };
        let count = files.count();
        let bytes_total = (0..count)
            .filter_map(|i| files.facts(i))
            .filter(|f| will_attempt(f.flags, f.context, options.skip_damaged))
            .fold(0u64, |sum, f| sum.saturating_add(f.size));
        let allowed = |check: &DestinationCheck| {
            check.is_allowed(options.allow_same_disk, options.allow_unknown_disk)
        };
        // The session range is always protected (S2), also if the caller's list
        // does not start with it.
        let mut protected = options.protected.clone();
        let session = Protected::session(source.range());
        if protected.first() != Some(&session) {
            protected.insert(0, session);
        }
        // The guard on the path as given, before anything is created ...
        let check = destination_check(
            dest,
            Purpose::Recover,
            &source,
            &protected,
            Some(bytes_total),
        );
        if !allowed(&check) {
            return Err(StartError::DestinationRefused(Box::new(check)));
        }
        let root = OutDir::create_root(dest).map_err(start_io_error)?;
        // ... and again on the created directory, by its canonical path.
        let check = destination_check(
            root.path(),
            Purpose::Recover,
            &source,
            &protected,
            Some(bytes_total),
        );
        if !allowed(&check) {
            return Err(StartError::DestinationRefused(Box::new(check)));
        }
        root.remove_leftover_parts(true).map_err(start_io_error)?;
        // A destination that cannot describe itself is still writable: the
        // usual limits then apply, and the file system has the last word.
        let mut limits = root.limits().unwrap_or_else(|_| fallback_limits());
        if overrides.max_file_bytes.is_some() {
            limits.max_file_bytes = overrides.max_file_bytes;
        }
        let limit = overrides.component.unwrap_or(limits.max_component);
        if let Some(tree) = &mut tree {
            // Names come from the plan alone: nothing to cancel here.
            let _ = tree.assign_names(&folders, &mut *files, limit, &never);
        }
        let report = ReportFile::create(&root, SystemTime::now()).map_err(start_io_error)?;
        Ok(Recovery {
            source,
            folders,
            files,
            options,
            dest_display: dest.display().to_string(),
            root,
            limits,
            limit,
            check,
            tree,
            report,
            bytes_total,
            files_total: u64::try_from(count).unwrap_or(u64::MAX),
        })
    }

    /// Files in the plan (`recover_start.files`).
    pub fn files_total(&self) -> u64 {
        self.files_total
    }

    /// Bytes of the files that will be attempted (`recover_start.bytes`).
    pub fn bytes_total(&self) -> u64 {
        self.bytes_total
    }

    /// The canonical destination directory.
    pub fn root_path(&self) -> &Path {
        self.root.path()
    }

    /// The report file.
    pub fn report_path(&self) -> PathBuf {
        self.root.path().join(self.report.name())
    }

    /// The destination guard's verdict on the created directory (free
    /// space, file system, maximum file size).
    pub fn destination(&self) -> &DestinationCheck {
        &self.check
    }

    /// Runs the job to its end. `cancel` is polled while a lazy plan is
    /// ordered and named, between files, between chunks of a file and
    /// between retries of a failing read.
    pub fn run(self, observer: &mut dyn RecoverObserver, cancel: &AtomicBool) -> RecoverSummary {
        job::run(self, observer, cancel)
    }
}

/// [`Recovery::prepare`] and [`Recovery::run`] in one call.
pub fn recover(
    source: RangedSource,
    plan: Plan,
    dest: &Path,
    options: RecoverOptions,
    observer: &mut dyn RecoverObserver,
    cancel: &AtomicBool,
) -> Result<RecoverSummary, StartError> {
    Ok(Recovery::prepare(source, plan, dest, options)?.run(observer, cancel))
}
