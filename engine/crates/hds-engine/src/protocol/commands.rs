//! Arguments and results of the commands of PROTOCOL.md section 3.
//!
//! # Defaults
//!
//! PROTOCOL.md shows each command's arguments as one JSON example; members
//! that identify what to work on have no default and are required (`epoch`,
//! `node`, `path`, `dest`, `kind`, `include`, `instance`, `cluster_size`,
//! `volume_offset`, `purpose`), every other member takes the value of the
//! example when it is absent. Where "absent" must be told apart from a value
//! (the `start`, `end` and `collect_carve_hits` of a resumed scan), the member
//! is an `Option` and the documented default is applied by the engine
//! (`ScanStartArgs::start_or_default`, ...). `null` is accepted wherever a
//! member is an `Option`, and means the same as an absent member.

use serde::{Deserialize, Serialize};

use hds_io::{
    DestinationCheck, DiskInfo, PartitionInfo, Purpose, SourceInfo, SourceKind, TableKind, Warning,
};

use super::objects::{CatalogInfo, Filter, Flag, Rating, Row};
use super::wire::ProtocolError;

/// Every command of the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Command {
    /// `hello`
    Hello,
    /// `list_disks`
    ListDisks,
    /// `open_source`
    OpenSource,
    /// `close_source`
    CloseSource,
    /// `scan_start`
    ScanStart,
    /// `job_cancel`
    JobCancel,
    /// `session_save`
    SessionSave,
    /// `session_load`
    SessionLoad,
    /// `session_attach_source`
    SessionAttachSource,
    /// `catalog_info`
    CatalogInfo,
    /// `geometry_set`
    GeometrySet,
    /// `instance_set`
    InstanceSet,
    /// `list_children`
    ListChildren,
    /// `node_info`
    NodeInfo,
    /// `search`
    Search,
    /// `selection_summary`
    SelectionSummary,
    /// `check_destination`
    CheckDestination,
    /// `recover_start`
    RecoverStart,
    /// `carve_start`
    CarveStart,
    /// `image_start`
    ImageStart,
    /// `read_file`
    ReadFile,
    /// `shutdown`
    Shutdown,
}

impl Command {
    /// Every command, in the order of PROTOCOL.md.
    pub const ALL: [Command; 22] = [
        Command::Hello,
        Command::ListDisks,
        Command::OpenSource,
        Command::CloseSource,
        Command::ScanStart,
        Command::JobCancel,
        Command::SessionSave,
        Command::SessionLoad,
        Command::SessionAttachSource,
        Command::CatalogInfo,
        Command::GeometrySet,
        Command::InstanceSet,
        Command::ListChildren,
        Command::NodeInfo,
        Command::Search,
        Command::SelectionSummary,
        Command::CheckDestination,
        Command::RecoverStart,
        Command::CarveStart,
        Command::ImageStart,
        Command::ReadFile,
        Command::Shutdown,
    ];

    /// The wire name.
    pub fn name(self) -> &'static str {
        match self {
            Command::Hello => "hello",
            Command::ListDisks => "list_disks",
            Command::OpenSource => "open_source",
            Command::CloseSource => "close_source",
            Command::ScanStart => "scan_start",
            Command::JobCancel => "job_cancel",
            Command::SessionSave => "session_save",
            Command::SessionLoad => "session_load",
            Command::SessionAttachSource => "session_attach_source",
            Command::CatalogInfo => "catalog_info",
            Command::GeometrySet => "geometry_set",
            Command::InstanceSet => "instance_set",
            Command::ListChildren => "list_children",
            Command::NodeInfo => "node_info",
            Command::Search => "search",
            Command::SelectionSummary => "selection_summary",
            Command::CheckDestination => "check_destination",
            Command::RecoverStart => "recover_start",
            Command::CarveStart => "carve_start",
            Command::ImageStart => "image_start",
            Command::ReadFile => "read_file",
            Command::Shutdown => "shutdown",
        }
    }

    /// The command with this wire name.
    pub fn from_name(name: &str) -> Option<Command> {
        Command::ALL.iter().copied().find(|c| c.name() == name)
    }
}

/// The empty object `{}`: the result of `close_source`, `job_cancel` and
/// `shutdown`, and the arguments of commands that take none.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Empty {}

/// Kind of a job. Wire form: `"scan"`, `"recover"`, `"carve"`, `"image"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// `scan_start`
    Scan,
    /// `recover_start`
    Recover,
    /// `carve_start`
    Carve,
    /// `image_start`
    Image,
}

// ------------------------------------------------------------------------------------------
// hello, list_disks, open_source
// ------------------------------------------------------------------------------------------

/// Result of `hello`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HelloResult {
    /// Version of the engine.
    pub engine_version: String,
    /// Protocol version: 1.
    pub protocol: u32,
    /// `std::env::consts::OS`: `"windows"`, `"linux"`, ...
    pub os: String,
    /// Advisory: whether the engine runs elevated.
    pub elevated: bool,
    /// Whether physical disks can be opened on this system.
    pub raw_disk_support: bool,
    /// The kinds `carve_start` accepts (`hds_carve::FileKind::name`).
    pub carve_kinds: Vec<String>,
}

/// Result of `list_disks`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ListDisksResult {
    /// The disks (empty on systems other than Windows).
    pub disks: Vec<DiskInfo>,
}

/// Arguments of `open_source`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OpenSourceArgs {
    /// Disk or image.
    pub kind: SourceKind,
    /// `\\.\PhysicalDrive<N>` or the image file's native absolute path.
    pub path: String,
    /// Images only: the logical sector size; `null` = detected, else 512.
    #[serde(default)]
    pub sector_size: Option<u32>,
}

/// Result of `open_source`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OpenSourceResult {
    /// The source.
    pub source: SourceInfo,
    /// Kind of partition table.
    pub table: TableKind,
    /// The partitions, sniffed from the source itself.
    pub partitions: Vec<PartitionInfo>,
    /// Read-only computed warnings about the whole source.
    pub warnings: Vec<Warning>,
}

// ------------------------------------------------------------------------------------------
// scan
// ------------------------------------------------------------------------------------------

/// Arguments of `scan_start`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanStartArgs {
    /// First byte of the range, sector-aligned; absent = 0 (resume: the session's).
    #[serde(default)]
    pub start: Option<u64>,
    /// End of the range, sector-aligned; absent or `null` = the end of the
    /// source (resume: the session's).
    #[serde(default)]
    pub end: Option<u64>,
    /// Where the session is saved; `null` = not saved automatically (resume:
    /// the file the session was loaded from or last saved to).
    #[serde(default)]
    pub session_path: Option<String>,
    /// Continue the loaded session.
    #[serde(default)]
    pub resume: bool,
    /// Collect signature hits for carving; absent = true (resume: the session's).
    #[serde(default)]
    pub collect_carve_hits: Option<bool>,
    /// Consent for a session file on the source disk (S3).
    #[serde(default)]
    pub allow_same_disk: bool,
    /// Consent for a session file whose disk cannot be determined (S3).
    #[serde(default)]
    pub allow_unknown_disk: bool,
}

impl ScanStartArgs {
    /// `start` with its default for a new scan.
    pub fn start_or_default(&self) -> u64 {
        self.start.unwrap_or(0)
    }

    /// `collect_carve_hits` with its default for a new scan.
    pub fn collect_carve_hits_or_default(&self) -> bool {
        self.collect_carve_hits.unwrap_or(true)
    }
}

/// Result of `scan_start`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ScanStartResult {
    /// Always [`JobKind::Scan`].
    pub job: JobKind,
    /// Bytes the scan will read.
    pub bytes_total: u64,
}

// ------------------------------------------------------------------------------------------
// sessions
// ------------------------------------------------------------------------------------------

/// Arguments of `session_save`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionSaveArgs {
    /// Native absolute path of the new session file.
    pub path: String,
    /// Consent for the source disk (S3).
    #[serde(default)]
    pub allow_same_disk: bool,
    /// Consent for an unknown disk (S3).
    #[serde(default)]
    pub allow_unknown_disk: bool,
}

/// Result of `session_save`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionSaveResult {
    /// The path written.
    pub path: String,
    /// Its size in bytes.
    pub bytes: u64,
}

/// A source named by kind and path (`session_load.source`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SourceRef {
    /// Disk or image.
    pub kind: SourceKind,
    /// Native path.
    pub path: String,
}

/// Arguments of `session_load`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionLoadArgs {
    /// Native absolute path of the session file.
    pub path: String,
    /// A source to open instead of the recorded one; `null` = the recorded one.
    #[serde(default)]
    pub source: Option<SourceRef>,
}

/// Result of `session_load`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionLoadResult {
    /// The attached source, or the recorded one when none matched.
    pub source: SourceInfo,
    /// True when a matching source is attached.
    pub source_attached: bool,
    /// Why no source is attached; `null` when one is.
    pub source_error: Option<ProtocolError>,
    /// The catalog built from the session.
    pub catalog: CatalogInfo,
}

/// Arguments of `session_attach_source`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionAttachArgs {
    /// Disk or image.
    pub kind: SourceKind,
    /// Native path.
    pub path: String,
    /// Accept a match by fewer than 8 samples (ALGORITHMS B4).
    #[serde(default)]
    pub force: bool,
}

/// Result of `session_attach_source`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionAttachResult {
    /// The attached source.
    pub source: SourceInfo,
    /// The rebuilt catalog (new epoch).
    pub catalog: CatalogInfo,
}

// ------------------------------------------------------------------------------------------
// catalog changes
// ------------------------------------------------------------------------------------------

/// Arguments of `geometry_set`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GeometrySetArgs {
    /// Instance id.
    pub instance: u32,
    /// A power of two from 512 to 2 MiB.
    pub cluster_size: u64,
    /// A multiple of 512 inside the source.
    pub volume_offset: u64,
}

/// What `instance_set` makes of an instance. Wire form: `"main"`, `"other"`, `"auto"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum InstanceChoice {
    /// Make it the main instance.
    Main,
    /// Make an embedded or older instance a volume of its own.
    Other,
    /// Withdraw the override.
    Auto,
}

/// Arguments of `instance_set`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct InstanceSetArgs {
    /// Instance id.
    pub instance: u32,
    /// The new kind.
    pub kind: InstanceChoice,
}

// ------------------------------------------------------------------------------------------
// queries
// ------------------------------------------------------------------------------------------

/// Sort key of listings. Wire form: `"name"`, `"size"`, `"modified"`, `"rating"`, `"ext"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    /// By name (case-insensitive).
    #[default]
    Name,
    /// By size.
    Size,
    /// By modification time.
    Modified,
    /// By rating.
    Rating,
    /// By extension.
    Ext,
}

/// Largest `limit` of `list_children` and `search`.
pub const MAX_PAGE: u32 = 1000;

/// Largest `length` of `read_file`.
pub const MAX_READ_LENGTH: u64 = 8_388_608;

fn default_limit() -> u32 {
    200
}

fn default_read_length() -> u64 {
    65_536
}

/// Arguments of `list_children`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ListChildrenArgs {
    /// Catalog epoch.
    pub epoch: u64,
    /// The folder.
    pub node: u64,
    /// Rows to skip.
    #[serde(default)]
    pub offset: u64,
    /// Rows to return, at most [`MAX_PAGE`].
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Sort key.
    #[serde(default)]
    pub sort: SortKey,
    /// Descending.
    #[serde(default)]
    pub desc: bool,
    /// Folders only.
    #[serde(default)]
    pub dirs_only: bool,
    /// Show deleted files and folders.
    #[serde(default)]
    pub show_deleted: bool,
    /// Extensions of the files to list; `null` = any.
    #[serde(default)]
    pub exts: Option<Vec<String>>,
    /// Ratings of the files to list; `null` = any.
    #[serde(default)]
    pub ratings: Option<Vec<Rating>>,
}

/// Result of `list_children`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RowsPage {
    /// Rows after filters.
    pub total: u64,
    /// This page.
    pub rows: Vec<Row>,
}

/// Arguments of `node_info`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NodeInfoArgs {
    /// Catalog epoch.
    pub epoch: u64,
    /// The node.
    pub node: u64,
}

/// An element of `node_info.ancestors`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Ancestor {
    /// Node id.
    pub id: u64,
    /// Display name.
    pub name: String,
}

/// `node_info.record`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RecordInfo {
    /// MFT record number.
    pub number: u64,
    /// Sequence number.
    pub seq: u32,
    /// Absolute byte offset of the record on the source.
    pub disk_offset: u64,
    /// Number of extension records.
    pub extension_records: u32,
}

/// `node_info.data`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DataInfo {
    /// True when the data lives inside the record.
    pub resident: bool,
    /// Real size.
    pub real_size: u64,
    /// Initialized size.
    pub initialized_size: u64,
    /// Allocated size.
    pub allocated_size: u64,
    /// Number of runs.
    pub runs: u64,
    /// First cluster; `null` when resident or unknown.
    pub first_lcn: Option<u64>,
    /// Absolute byte offset of the first data byte; `null` when unknown.
    pub first_offset: Option<u64>,
}

/// An element of `node_info.flag_details`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FlagDetail {
    /// The flag.
    pub flag: Flag,
    /// Its explanation in plain language.
    pub text: String,
}

/// Result of `node_info`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    /// The node's row.
    pub row: Row,
    /// Root first.
    pub ancestors: Vec<Ancestor>,
    /// Display path.
    pub path: String,
    /// Instance id.
    pub instance: u32,
    /// Access time, Unix milliseconds.
    pub accessed: Option<i64>,
    /// Other hard links.
    pub other_names: Vec<String>,
    /// `null` for virtual, lost and carved nodes.
    pub record: Option<RecordInfo>,
    /// `null` for folders.
    pub data: Option<DataInfo>,
    /// One explanation per flag of the row.
    pub flag_details: Vec<FlagDetail>,
}

/// Arguments of `search`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SearchArgs {
    /// Catalog epoch.
    pub epoch: u64,
    /// Subtree to search; `null` = the main root.
    #[serde(default)]
    pub under: Option<u64>,
    /// The filter.
    #[serde(default)]
    pub filter: Filter,
    /// Include deleted files.
    #[serde(default)]
    pub show_deleted: bool,
    /// Sort key.
    #[serde(default)]
    pub sort: SortKey,
    /// Descending.
    #[serde(default)]
    pub desc: bool,
    /// Rows to skip.
    #[serde(default)]
    pub offset: u64,
    /// Rows to return, at most [`MAX_PAGE`].
    #[serde(default = "default_limit")]
    pub limit: u32,
}

/// A row of `search`: a [`Row`] plus its path.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SearchRow {
    /// The row.
    #[serde(flatten)]
    pub row: Row,
    /// Display path.
    pub path: String,
    /// Ancestor ids, root first.
    pub ancestor_ids: Vec<u64>,
}

/// Result of `search`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SearchPage {
    /// Matches.
    pub total: u64,
    /// This page.
    pub rows: Vec<SearchRow>,
}

/// Arguments of `selection_summary`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SelectionArgs {
    /// Catalog epoch.
    pub epoch: u64,
    /// Selected nodes.
    pub include: Vec<u64>,
    /// Deselected nodes.
    #[serde(default)]
    pub exclude: Vec<u64>,
    /// Applied to every selected file; `null` = none.
    #[serde(default)]
    pub filter: Option<Filter>,
    /// Include deleted files.
    #[serde(default)]
    pub include_deleted: bool,
    /// Leave damaged files out.
    #[serde(default)]
    pub skip_damaged: bool,
}

/// Result of `selection_summary`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectionSummary {
    /// Selected files after filters.
    pub files: u64,
    /// Bytes that will be attempted.
    pub bytes: u64,
    /// Directories that will be created.
    pub dirs: u64,
    /// Selected files rated good.
    pub good: u64,
    /// ... uncertain.
    pub uncertain: u64,
    /// ... damaged.
    pub damaged: u64,
    /// ... unsupported.
    pub unsupported: u64,
}

// ------------------------------------------------------------------------------------------
// destinations, jobs
// ------------------------------------------------------------------------------------------

/// Arguments of `check_destination`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CheckDestinationArgs {
    /// Native absolute path.
    pub path: String,
    /// What it is for.
    pub purpose: Purpose,
    /// Space needed, for `enough_space`.
    #[serde(default)]
    pub need_bytes: Option<u64>,
}

/// Result of `check_destination`: the guard's result plus what the catalog
/// says about the location.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CheckDestinationResult {
    /// The guard's result (`hds_io::destination_check`).
    #[serde(flatten)]
    pub check: DestinationCheck,
    /// Catalog files stored in the part of the source disk that the
    /// location's volume occupies (0 without a catalog).
    pub lost_files: u64,
    /// Their bytes.
    pub lost_bytes: u64,
}

/// What happens to a file that existed before this run. Wire form:
/// `"rename"`, `"skip"`, `"overwrite"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnExists {
    /// Take the next free ` (n)` name.
    #[default]
    Rename,
    /// Skip the file.
    Skip,
    /// Replace a regular file (never a directory or link).
    Overwrite,
}

/// Arguments of `recover_start`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RecoverStartArgs {
    /// Catalog epoch.
    pub epoch: u64,
    /// Selected nodes; must not be empty.
    pub include: Vec<u64>,
    /// Deselected nodes.
    #[serde(default)]
    pub exclude: Vec<u64>,
    /// Applied to every selected file.
    #[serde(default)]
    pub filter: Option<Filter>,
    /// Destination directory.
    pub dest: String,
    /// Include deleted files.
    #[serde(default)]
    pub include_deleted: bool,
    /// Leave damaged files out.
    #[serde(default)]
    pub skip_damaged: bool,
    /// Consent for the source disk (S3).
    #[serde(default)]
    pub allow_same_disk: bool,
    /// Consent for an unknown disk (S3).
    #[serde(default)]
    pub allow_unknown_disk: bool,
    /// Files that existed before this run.
    #[serde(default)]
    pub on_exists: OnExists,
}

/// Result of `recover_start`: the numbers of `selection_summary`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RecoverStartResult {
    /// Always [`JobKind::Recover`].
    pub job: JobKind,
    /// Files.
    pub files: u64,
    /// Bytes.
    pub bytes: u64,
}

/// Arguments of `carve_start`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct CarveStartArgs {
    /// Kinds (`hello.carve_kinds`); `null` = all.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
}

/// Result of `carve_start`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CarveStartResult {
    /// Always [`JobKind::Carve`].
    pub job: JobKind,
    /// Candidates to measure.
    pub candidates: u64,
}

/// Arguments of `image_start`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ImageStartArgs {
    /// First byte, sector-aligned.
    #[serde(default)]
    pub start: u64,
    /// End, sector-aligned; `null` = the end of the source.
    #[serde(default)]
    pub end: Option<u64>,
    /// Native absolute path of the image file.
    pub dest: String,
    /// Continue an earlier image.
    #[serde(default)]
    pub resume: bool,
    /// Consent for the source disk (S3).
    #[serde(default)]
    pub allow_same_disk: bool,
    /// Consent for an unknown disk (S3).
    #[serde(default)]
    pub allow_unknown_disk: bool,
}

/// Result of `image_start`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ImageStartResult {
    /// Always [`JobKind::Image`].
    pub job: JobKind,
    /// Bytes to image.
    pub bytes: u64,
}

/// Arguments of `read_file`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReadFileArgs {
    /// Catalog epoch.
    pub epoch: u64,
    /// The file.
    pub node: u64,
    /// Byte offset in the file.
    #[serde(default)]
    pub offset: u64,
    /// Bytes to read, at most [`MAX_READ_LENGTH`].
    #[serde(default = "default_read_length")]
    pub length: u64,
}

/// Result of `read_file`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReadFileResult {
    /// The bytes, base64 (standard alphabet, padded).
    pub data_b64: String,
    /// Size of the file.
    pub size: u64,
    /// True when the bytes reach the end of the file.
    pub eof: bool,
    /// Sniffed kind of the returned bytes when `offset == 0`, else `null`.
    pub kind: Option<String>,
}
