//! The shared objects of PROTOCOL.md section 2.
//!
//! `SourceInfo`, `Partition`, `Disk`, `Warning` and the guard's result are
//! the serde types of `hds-io`, re-exported from [`crate::protocol`]; this
//! module defines the rest. Every `Option` member is written as `null`, never
//! left out; members the engine does not know are ignored on input.

use serde::{Deserialize, Serialize};

use hds_io::{ProtectedReason, Warning};

/// Where a geometry's cluster size came from. Wire form: `"attr"`, `"boot"`,
/// `"default"`, `"manual"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ClusterSource {
    /// The most frequent ratio of allocated size to clusters (ALGORITHMS F1).
    Attr,
    /// A boot sector.
    Boot,
    /// Nothing qualified: 4096.
    Default,
    /// Set by the user (`geometry_set`).
    Manual,
}

/// Where a geometry's volume offset came from. Wire form: snake case.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum OffsetSource {
    /// The MFT map of a surviving record 0.
    Mft,
    /// Votes of index blocks.
    Index,
    /// A primary boot sector.
    Boot,
    /// A backup boot sector.
    BackupBoot,
    /// Signature anchors.
    Signature,
    /// A partition start or the range start.
    Default,
    /// Taken over from the main instance.
    Inherited,
    /// Set by the user (`geometry_set`).
    Manual,
}

/// The `Geometry` object: where an instance's clusters are.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Geometry {
    /// Cluster size in bytes: a power of two from 512 to 2 MiB.
    pub cluster_size: u32,
    /// Absolute byte offset on the source of the volume's cluster 0.
    pub volume_offset: u64,
    /// Where the cluster size came from.
    pub cluster_source: ClusterSource,
    /// Where the offset came from.
    pub offset_source: OffsetSource,
    /// Distinct directories whose index blocks voted for this offset.
    pub index_dirs: u64,
    /// Index blocks that voted for this offset.
    pub index_blocks: u64,
    /// Files whose first sector was checked (ALGORITHMS F3).
    pub signature_checked: u64,
    /// Of those, the files whose signature matched their extension.
    pub signature_ok: u64,
    /// Validated by ALGORITHMS F6.
    pub validated: bool,
    /// Size of the volume in clusters (ALGORITHMS F5).
    pub total_clusters: u64,
}

/// What an instance is. Wire form: `"main"`, `"older"`, `"other"`,
/// `"embedded"`, `"strays"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum InstanceKind {
    /// The file system that was lost.
    Main,
    /// A file system from before the previous format.
    Older,
    /// Another volume.
    Other,
    /// Inside a disk-image file; browse only.
    Embedded,
    /// Records that belong to no file system.
    Strays,
}

/// Node ids of an instance's virtual folders; `null` when the folder does
/// not exist.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtualFolders {
    /// `[Lost folders]`.
    pub lost: Option<u64>,
    /// `[Orphaned files]`.
    pub orphans: Option<u64>,
    /// `[Carved files]`.
    pub carved: Option<u64>,
}

/// The `Instance` object: one file system found by the scan.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// Instance id; `instances[0]` of the catalog is the main one.
    pub id: u32,
    /// What it is.
    pub kind: InstanceKind,
    /// True when the user set the kind (`instance_set`).
    pub manual: bool,
    /// Display name of its root (`""` for main).
    pub label: String,
    /// Node id of its root.
    pub root: u64,
    /// Records that belong to it.
    pub records: u64,
    /// Non-deleted files.
    pub files: u64,
    /// Non-deleted folders.
    pub dirs: u64,
    /// Deleted files.
    pub deleted_files: u64,
    /// Lost folders.
    pub lost_dirs: u64,
    /// Bytes of the non-deleted files.
    pub bytes: u64,
    /// Oldest MFT-change time, Unix milliseconds; `null` if unknown.
    pub time_from: Option<i64>,
    /// Newest MFT-change time, Unix milliseconds; `null` if unknown.
    pub time_to: Option<i64>,
    /// Display path of the containing file of an embedded instance.
    pub embedded_in: Option<String>,
    /// The chosen geometry.
    pub geometry: Geometry,
    /// All candidates considered.
    pub geometry_candidates: Vec<Geometry>,
    /// Its virtual folders.
    #[serde(rename = "virtual")]
    pub virtual_folders: VirtualFolders,
}

/// A current volume (ALGORITHMS C): an element of `CatalogInfo.live_volumes`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LiveVolume {
    /// Absolute byte offset of the volume.
    pub offset: u64,
    /// Size in bytes.
    pub size: u64,
    /// When it was formatted, Unix milliseconds; `null` if unknown.
    pub format_time: Option<i64>,
    /// When its snapshot was taken, Unix milliseconds; `null` if unknown.
    pub snapshot_time: Option<i64>,
}

/// An absolute byte range `[start, end)` of the source.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// First byte.
    pub start: u64,
    /// One past the last byte.
    pub end: u64,
}

impl ByteRange {
    /// As a Rust range.
    pub fn range(&self) -> std::ops::Range<u64> {
        self.start..self.end
    }
}

impl From<std::ops::Range<u64>> for ByteRange {
    fn from(range: std::ops::Range<u64>) -> Self {
        ByteRange {
            start: range.start,
            end: range.end,
        }
    }
}

/// A range no destination may touch (S2): an element of
/// `CatalogInfo.protected`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtectedRange {
    /// First byte.
    pub start: u64,
    /// One past the last byte.
    pub end: u64,
    /// `"session_range"` or `"lost_volume"`.
    pub reason: ProtectedReason,
}

impl ProtectedRange {
    /// As the guard's input.
    pub fn to_guard(&self) -> hds_io::Protected {
        hds_io::Protected {
            range: self.start..self.end,
            reason: self.reason,
        }
    }
}

/// The `CatalogInfo` object.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CatalogInfo {
    /// The catalog epoch node ids belong to. The engine assigns it: whatever
    /// a [`Backend`](crate::backend::Backend) puts here is overwritten.
    pub epoch: u64,
    /// The file systems found; `instances[0]` is the main one.
    pub instances: Vec<Instance>,
    /// Nodes of all roots.
    pub nodes: u64,
    /// Records of the current file system (not shown).
    pub live_records: u64,
    /// Files created and deleted again on the current file system (not shown).
    pub post_format_records: u64,
    /// The current volumes.
    pub live_volumes: Vec<LiveVolume>,
    /// The session range.
    pub range: ByteRange,
    /// Ranges no destination may touch: the session range, then one
    /// `lost_volume` entry per lost file system with validated geometry.
    pub protected: Vec<ProtectedRange>,
    /// Bytes scanned.
    pub scanned_bytes: u64,
    /// True when the whole range was scanned.
    pub scan_complete: bool,
    /// Unreadable bytes.
    pub bad_bytes: u64,
    /// Files with data outside the session range.
    pub out_of_range_files: u64,
    /// Their bytes.
    pub out_of_range_bytes: u64,
    /// Stored signature hits.
    pub carve_hits: u64,
    /// Carved files.
    pub carved: u64,
    /// True when the source is attached.
    pub source_attached: bool,
    /// Warnings.
    pub warnings: Vec<Warning>,
}

/// Kind of a node. Wire form: `"dir"`, `"file"`, `"lost_dir"`, `"virtual"`, `"carved"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A folder.
    Dir,
    /// A file.
    File,
    /// A lost folder.
    LostDir,
    /// A virtual folder (`[Lost folders]`, ...) or an instance root.
    Virtual,
    /// A carved file.
    Carved,
}

impl NodeKind {
    /// True for the kinds that hold children.
    pub fn is_folder(self) -> bool {
        matches!(self, NodeKind::Dir | NodeKind::LostDir | NodeKind::Virtual)
    }
}

/// Health rating of a file (ALGORITHMS G3). Wire form: snake case.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Rating {
    /// Nothing speaks against the file.
    Good,
    /// Something might be wrong.
    Uncertain,
    /// Something is wrong.
    Damaged,
    /// The content is not stored as plain clusters.
    Unsupported,
}

/// A health flag of a file (ALGORITHMS G1). Wire form: snake case.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    /// The file was deleted before the accident.
    Deleted,
    /// A record of the file failed its fixup check.
    TornRecord,
    /// Parts of the runlist are missing.
    MissingFragments,
    /// The size was estimated.
    SizeEstimated,
    /// Data lies outside the volume or the session range.
    OutOfBounds,
    /// Data was overwritten.
    Overwritten,
    /// Another file claims the same clusters.
    Overlap,
    /// NTFS- or WOF-compressed.
    Compressed,
    /// EFS-encrypted.
    Encrypted,
    /// A cloud or deduplication placeholder.
    Placeholder,
    /// The runlist has holes.
    Sparse,
    /// The file has no data.
    NoData,
    /// Unreadable sectors.
    BadSectors,
    /// Belongs to an older file system.
    OlderGeneration,
    /// Inside a disk-image file.
    Embedded,
    /// A stray record.
    Stray,
    /// The first bytes match the extension.
    SignatureOk,
    /// The first bytes contradict the extension.
    SignatureMismatch,
}

/// The `Row` object: one node in a listing.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Node id.
    pub id: u64,
    /// Parent node id; `null` for an instance root.
    pub parent: Option<u64>,
    /// Display name (UTF-8; unpaired surrogates as U+FFFD, see [`wire_name`]).
    pub name: String,
    /// Kind of node.
    pub kind: NodeKind,
    /// Lower-case extension without the dot; `""` if none.
    pub ext: String,
    /// Files: real size; folders: total size of the non-deleted files below.
    pub size: u64,
    /// Modification time, Unix milliseconds.
    pub modified: Option<i64>,
    /// Creation time, Unix milliseconds.
    pub created: Option<i64>,
    /// Deleted before the accident.
    pub deleted: bool,
    /// Rating; `null` for folders.
    pub rating: Option<Rating>,
    /// Health flags.
    pub flags: Vec<Flag>,
    /// Folders: non-deleted files below (recursive); files: 0.
    pub files: u64,
    /// Folders: deleted files below (recursive); files: 0.
    pub deleted_files: u64,
    /// Folders: bytes of the deleted files below; files: 0.
    pub deleted_size: u64,
    /// Folders: at least one child folder a `dirs_only` listing would return.
    pub has_children: bool,
}

/// The `Filter` object. All members optional; `null` means "any", an empty
/// list is `bad_request`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct Filter {
    /// Case-insensitive substring of the name.
    pub query: Option<String>,
    /// Extensions (lower case, no dot; `""` for none).
    pub exts: Option<Vec<String>>,
    /// Smallest size in bytes.
    pub min_size: Option<u64>,
    /// Largest size in bytes.
    pub max_size: Option<u64>,
    /// Ratings.
    pub ratings: Option<Vec<Rating>>,
}

/// Converts an NTFS name (UTF-16 code units) for the wire: an unpaired
/// surrogate becomes U+FFFD (PROTOCOL.md, "Conventions").
pub fn wire_name(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpaired_surrogates_become_replacement_characters() {
        // "a", lone high surrogate, "b", a valid pair (U+1F600), lone low surrogate.
        let units = [0x61, 0xD800, 0x62, 0xD83D, 0xDE00, 0xDC00];
        let name = wire_name(&units);
        assert_eq!(name, "a\u{FFFD}b\u{1F600}\u{FFFD}");
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(serde_json::from_str::<String>(&json).unwrap(), name);
    }

    #[test]
    fn options_are_written_as_null() {
        let v = serde_json::to_value(VirtualFolders::default()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"lost": null, "orphans": null, "carved": null})
        );
    }
}
