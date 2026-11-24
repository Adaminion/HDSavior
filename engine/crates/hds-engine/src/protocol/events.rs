//! The events of PROTOCOL.md sections 3 and 4.

use serde::{Deserialize, Serialize};

use hds_io::Warning;

use super::objects::CatalogInfo;
use super::wire::{EventData, ProtocolError};

/// Phase of a scan. Wire form: `"mft_probe"`, `"sweep"`, `"analyzing"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScanPhase {
    /// Reading forward from the probe offsets (ALGORITHMS A2).
    #[default]
    MftProbe,
    /// Reading the rest of the range.
    Sweep,
    /// Building the catalog; `job_cancel` is ignored.
    Analyzing,
}

/// `scan_progress`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanProgress {
    /// Phase.
    pub phase: ScanPhase,
    /// Size of the done set (or analysis steps done).
    pub bytes_done: u64,
    /// Size of the range (or analysis steps).
    pub bytes_total: u64,
    /// Absolute offset of the last chunk read.
    pub position: u64,
    /// Bytes per second over the last 5 s.
    pub speed_bps: u64,
    /// Remaining seconds; `null` while analyzing or at speed 0.
    pub eta_seconds: Option<u64>,
    /// Whole seconds of this run.
    pub elapsed_seconds: u64,
    /// Records stored.
    pub records: u64,
    /// In-use base records without the directory flag.
    pub files: u64,
    /// In-use base records with the directory flag.
    pub dirs: u64,
    /// Index blocks stored.
    pub index_blocks: u64,
    /// Signature hits stored.
    pub carve_hits: u64,
    /// Unreadable bytes (a multiple of the logical sector size).
    pub bad_bytes: u64,
    /// Warnings raised so far.
    pub warnings: Vec<Warning>,
}

impl EventData for ScanProgress {
    const NAME: &'static str = "scan_progress";
}

/// How a scan ended. Wire form: `"completed"`, `"stopped"`, `"failed"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScanStatus {
    /// The whole range was read.
    #[default]
    Completed,
    /// Cancelled by the user (or ended by shutdown).
    Stopped,
    /// Ended by an error.
    Failed,
}

/// `scan_done`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanDone {
    /// How it ended.
    pub status: ScanStatus,
    /// Set when `status` is `failed`.
    pub error: Option<ProtocolError>,
    /// Where the session was saved; `null` when it is not saved.
    pub session_path: Option<String>,
    /// The catalog; `null` when the scan failed or the engine is shutting down.
    pub catalog: Option<CatalogInfo>,
}

impl EventData for ScanDone {
    const NAME: &'static str = "scan_done";
}

/// How a recover, carve or image job ended. Wire form: `"completed"`,
/// `"cancelled"`, `"failed"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Done.
    #[default]
    Completed,
    /// Cancelled.
    Cancelled,
    /// Ended by an error.
    Failed,
}

/// `recover_progress`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoverProgress {
    /// Files processed.
    pub files_done: u64,
    /// Files to process.
    pub files_total: u64,
    /// Bytes processed.
    pub bytes_done: u64,
    /// Bytes to process.
    pub bytes_total: u64,
    /// Display path of the file being written.
    pub current: Option<String>,
    /// Bytes per second over the last 5 s.
    pub speed_bps: u64,
    /// Remaining seconds; `null` at speed 0.
    pub eta_seconds: Option<u64>,
    /// Whole seconds of this run.
    pub elapsed_seconds: u64,
    /// Files with status `ok` or `ok_unverified`.
    pub ok: u64,
    /// Files with status `warning:...`.
    pub warnings: u64,
    /// Files with status `failed:...`.
    pub failed: u64,
    /// Files with status `skipped:...`.
    pub skipped: u64,
}

impl EventData for RecoverProgress {
    const NAME: &'static str = "recover_progress";
}

/// `recover_done`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoverDone {
    /// How it ended.
    pub status: JobStatus,
    /// Set when `status` is `failed` (`no_space`, `source_lost`, ...).
    pub error: Option<ProtocolError>,
    /// Files of the selection.
    pub files_total: u64,
    /// Files `ok`.
    pub ok: u64,
    /// Files with warnings.
    pub warnings: u64,
    /// Files failed.
    pub failed: u64,
    /// Files skipped.
    pub skipped: u64,
    /// Bytes written.
    pub bytes_written: u64,
    /// The destination directory.
    pub dest: String,
    /// The CSV report; `null` when none was created.
    pub report_path: Option<String>,
}

impl EventData for RecoverDone {
    const NAME: &'static str = "recover_done";
}

/// `carve_progress`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct CarveProgress {
    /// Candidates measured.
    pub candidates_done: u64,
    /// Candidates to measure.
    pub candidates_total: u64,
    /// Files found.
    pub found: u64,
    /// Their bytes.
    pub bytes_found: u64,
    /// Remaining seconds; `null` if unknown.
    pub eta_seconds: Option<u64>,
}

impl EventData for CarveProgress {
    const NAME: &'static str = "carve_progress";
}

/// `carve_done`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct CarveDone {
    /// How it ended.
    pub status: JobStatus,
    /// Set when `status` is `failed`.
    pub error: Option<ProtocolError>,
    /// Files found.
    pub found: u64,
    /// Their bytes.
    pub bytes_found: u64,
    /// The catalog with the carved nodes (same epoch); `null` when the job
    /// failed before the catalog could be updated.
    pub catalog: Option<CatalogInfo>,
}

impl EventData for CarveDone {
    const NAME: &'static str = "carve_done";
}

/// `image_progress`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageProgress {
    /// Bytes copied.
    pub bytes_done: u64,
    /// Bytes to copy.
    pub bytes_total: u64,
    /// Bytes per second over the last 5 s.
    pub speed_bps: u64,
    /// Remaining seconds; `null` at speed 0.
    pub eta_seconds: Option<u64>,
    /// Whole seconds of this run.
    pub elapsed_seconds: u64,
    /// Unreadable bytes, zero-filled.
    pub bad_bytes: u64,
}

impl EventData for ImageProgress {
    const NAME: &'static str = "image_progress";
}

/// `image_done`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageDone {
    /// How it ended.
    pub status: JobStatus,
    /// Set when `status` is `failed`.
    pub error: Option<ProtocolError>,
    /// Bytes in the image (confirmed).
    pub bytes_written: u64,
    /// Unreadable bytes.
    pub bad_bytes: u64,
    /// The image file.
    pub path: String,
    /// The list of unreadable ranges; `null` when there were none.
    pub bad_map_path: Option<String>,
}

impl EventData for ImageDone {
    const NAME: &'static str = "image_done";
}

/// Which plain request reports `task_progress`. Wire form: `"session_load"`,
/// `"geometry_set"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskName {
    /// `session_load`
    SessionLoad,
    /// `geometry_set`
    GeometrySet,
}

/// `task_progress`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TaskProgress {
    /// The request at work.
    pub task: TaskName,
    /// Fraction done, 0 to 1.
    pub fraction: f64,
}

impl EventData for TaskProgress {
    const NAME: &'static str = "task_progress";
}

/// Level of a `log` event. Wire form: `"info"`, `"warn"`, `"error"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Information.
    Info,
    /// Something noteworthy.
    Warn,
    /// An error.
    Error,
}

/// `log`: informational.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LogEvent {
    /// Level.
    pub level: LogLevel,
    /// Text.
    pub message: String,
}

impl EventData for LogEvent {
    const NAME: &'static str = "log";
}
