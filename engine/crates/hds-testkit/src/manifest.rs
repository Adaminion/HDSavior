//! The ground truth of an image.
//!
//! A [`Manifest`] says what was put on the disk, what is still there, and
//! what a correct scan must conclude from it. It has three layers:
//!
//! * **Structures** (`records`, `index_blocks`, `boot_sectors`): every `FILE`
//!   record, `INDX` block and NTFS boot sector whose signature can still be
//!   found in the image, described field by field. This is what the scanner
//!   (ALGORITHMS A3–A5) has to find, no more and no less.
//! * **Truth** (`Entry::truth`): for every file and folder of every file
//!   system that ever was on the disk, whether its records and its content
//!   survived, established by comparing the finished image with what was
//!   written (never by re-deriving the engine's rules).
//! * **Expectations** (`Entry::expect`, `Instance::expected_kind`): what a
//!   correct catalog shows. Only what follows unambiguously from
//!   ALGORITHMS.md is stated; a flag that is in neither `flags_set` nor
//!   `flags_clear` is left open, and so is a `None` path or kind.
//!
//! All offsets are absolute byte offsets in the image. All times are
//! FILETIME values. Everything serializes to JSON with serde.

use serde::{Deserialize, Serialize};

use crate::content::ContentSpec;
use crate::facts::{BootFact, IndexBlockFact, RecordFact};
use crate::ondisk::FileRef;
use crate::volume::params::Flavor;

/// Health flags of ALGORITHMS G1, with the names PROTOCOL.md uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Flag {
    Deleted,
    TornRecord,
    MissingFragments,
    SizeEstimated,
    OutOfBounds,
    Overwritten,
    Overlap,
    Compressed,
    Encrypted,
    Placeholder,
    Sparse,
    NoData,
    BadSectors,
    OlderGeneration,
    Embedded,
    Stray,
    SignatureOk,
    SignatureMismatch,
}

/// Instance kinds of ALGORITHMS D9.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum InstanceKind {
    Main,
    Older,
    Other,
    Embedded,
    Strays,
}

/// What a file system on the disk is, physically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceRole {
    /// The file system that exists there now (ALGORITHMS C): its records
    /// are `live` or `post_format` and never shown.
    Current,
    /// A file system that something was formatted over, or whose partition
    /// is gone: the thing to rescue.
    Lost,
    /// A file system inside a file of another one.
    Embedded,
}

/// Geometry of one file system.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    /// Absolute byte offset of cluster 0.
    pub volume_offset: u64,
    /// Cluster size in bytes.
    pub cluster_size: u32,
    /// Bytes per sector.
    pub bytes_per_sector: u32,
    /// MFT record size.
    pub record_size: u32,
    /// Index block size.
    pub index_block_size: u32,
    /// Sectors of the volume (the partition has one more).
    pub total_sectors: u64,
    /// Whole clusters of the volume.
    pub total_clusters: u64,
    /// Bytes of the partition the volume was formatted into.
    pub partition_bytes: u64,
    /// Hidden sectors of the boot sector.
    pub hidden_sectors: u32,
    /// Volume serial number.
    pub serial: u64,
    /// The pieces of the `$MFT`.
    pub mft_extents: Vec<MftExtentFact>,
    /// First cluster of `$MFTMirr`.
    pub mftmirr_lcn: u64,
    /// Number of MFT record slots.
    pub mft_records: u64,
    /// Slots below the `$MFT`'s initialized size.
    pub mft_initialized_records: u64,
}

/// One piece of an `$MFT`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MftExtentFact {
    /// First cluster.
    pub lcn: u64,
    /// Clusters.
    pub clusters: u64,
    /// Number of the first record in the piece.
    pub first_record: u64,
    /// Records in the piece.
    pub records: u64,
    /// Absolute byte offset of the piece.
    pub offset: u64,
}

/// One file system that is or was on the disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    /// Index into `Manifest::instances`.
    pub id: u32,
    /// The scenario's name for it ("old", "new", "inner", …).
    pub label: String,
    /// What it is physically.
    pub role: InstanceRole,
    /// The kind a correct scan assigns (`None`: not stated; always `None`
    /// for current volumes, which are not shown as instances).
    pub expected_kind: Option<InstanceKind>,
    /// How many file systems were formatted over it afterwards: 0 for the
    /// newest at its place.
    pub generation: u32,
    /// The formatter it imitates.
    pub flavor: Flavor,
    /// Its geometry: the *expected geometry* of ALGORITHMS F for lost
    /// instances.
    pub geometry: Geometry,
    /// Time of its format (creation time of its `$Volume` record).
    pub format_time: u64,
    /// The manifest entry of the file that contains it (embedded only).
    pub embedded_in: Option<u32>,
    /// Embedded only: byte offset of the volume inside that file. Cluster
    /// `n` of the volume is at this offset plus `n` clusters of the *file*;
    /// where that is on the disk follows from the file's extents.
    /// `geometry.volume_offset` is the absolute position of cluster 0 alone
    /// and describes the whole volume only while the file is unfragmented.
    pub offset_in_host: Option<u64>,
    /// Current volumes: the clusters its `$Bitmap` marks as allocated, as
    /// absolute byte ranges `(start, length)` (ALGORITHMS C2).
    pub live_allocated: Vec<(u64, u64)>,
    /// Current volumes: the MFT slots its `$MFT:$BITMAP` marks as in use,
    /// as `(first, count)` ranges.
    pub live_slots: Vec<(u64, u64)>,
    /// Lowest and highest MFT change time among its user files and folders.
    pub time_range: Option<(u64, u64)>,
}

/// One partition of a partition table written by the scenario.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionFact {
    /// 1-based index in offset order.
    pub index: u32,
    /// Absolute byte offset.
    pub offset: u64,
    /// Length in bytes.
    pub size: u64,
    /// `"mbr"` or `"gpt"`.
    pub table: String,
    /// MBR type byte as two hex digits, or the GPT type GUID.
    pub type_id: String,
}

/// File or folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum EntryKind {
    File,
    Dir,
}

/// One name of an entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryName {
    /// The name with unpaired surrogates replaced by U+FFFD.
    pub name: String,
    /// The exact UTF-16 units.
    pub name_utf16: Vec<u16>,
    /// 0 POSIX, 1 Win32, 2 DOS, 3 Win32 + DOS.
    pub namespace: u8,
    /// Parent directory reference as stored.
    pub parent: FileRef,
    /// The record that holds this `$FILE_NAME` (base or extension).
    pub holder_record: u64,
    /// Attribute id of this `$FILE_NAME` in that record. Usually ascending
    /// in record order; ALGORITHMS E4 breaks ties by the lowest id.
    pub attr_id: u16,
}

/// One run of an entry's unnamed data stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extent {
    /// First VCN of the run.
    pub vcn: u64,
    /// Number of clusters.
    pub clusters: u64,
    /// First LCN; `None` for a hole.
    pub lcn: Option<u64>,
    /// Absolute byte offset of the run; `None` for a hole.
    pub offset: Option<u64>,
    /// Length in bytes.
    pub bytes: u64,
}

/// The four times of `$STANDARD_INFORMATION`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryTimes {
    /// Creation time.
    pub created: u64,
    /// Modification time.
    pub modified: u64,
    /// MFT change time.
    pub mft_changed: u64,
    /// Access time.
    pub accessed: u64,
}

/// What became of an entry's base record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordState {
    /// Byte-identical to what the file system last wrote.
    Intact,
    /// Overwritten by something that happened after the file system was
    /// last written (a format, new files, deliberate damage).
    Destroyed,
    /// Its slot was given to another file by the same file system (the
    /// entry had been deleted): the record never reached the accident.
    Reused,
}

/// Facts about an entry in the finished image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Truth {
    /// State of the base record.
    pub record_state: RecordState,
    /// Base record and every extension record are intact.
    pub record_intact: bool,
    /// Record numbers of extension records that are not intact.
    pub lost_extension_records: Vec<u64>,
    /// Files: reading the content from where the records put it (resident
    /// value, or runs with the initialized-size rule) out of the finished
    /// image gives `sha256`. `None` for folders and for files whose content
    /// is not stored plainly (compressed, encrypted).
    pub content_intact: Option<bool>,
    /// Every folder on the way to the root has an intact base record (the
    /// root itself does not count: ALGORITHMS E5).
    pub ancestors_intact: bool,
    /// The base record lies inside the manifest's scan range.
    pub record_in_range: bool,
    /// Every allocated run lies inside the scan range.
    pub data_in_range: bool,
    /// Index entries that still name this entry: those in intact `INDX`
    /// blocks and in the `$INDEX_ROOT` of intact records, stale copies
    /// included.
    pub surviving_index_entries: u32,
}

/// Whether a correct catalog has a node for an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// There must be a node for it (somewhere in the tree of its instance).
    Shown,
    /// There must be none: it belongs to the current file system, is a
    /// system file, or lives below `$Extend`.
    Hidden,
    /// Its record is gone; it cannot appear as itself (its content may
    /// still come back through carving or an orphan group).
    Gone,
    /// Not stated.
    Unspecified,
}

/// What a correct catalog says about an entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expect {
    /// Whether the entry has a node.
    pub presence: Presence,
    /// Path from the root of its instance, when it is certain (all
    /// ancestors survived, or the scenario states it).
    pub path: Option<Vec<String>>,
    /// Flags the node must have.
    pub flags_set: Vec<Flag>,
    /// Flags the node must not have.
    pub flags_clear: Vec<Flag>,
}

impl Expect {
    /// Requires `flag` to be set.
    pub fn set(&mut self, flag: Flag) {
        self.flags_clear.retain(|f| *f != flag);
        if !self.flags_set.contains(&flag) {
            self.flags_set.push(flag);
            self.flags_set.sort();
        }
    }

    /// Requires `flag` to be clear.
    pub fn clear(&mut self, flag: Flag) {
        self.flags_set.retain(|f| *f != flag);
        if !self.flags_clear.contains(&flag) {
            self.flags_clear.push(flag);
            self.flags_clear.sort();
        }
    }

    /// Leaves `flag` open.
    pub fn open(&mut self, flag: Flag) {
        self.flags_set.retain(|f| *f != flag);
        self.flags_clear.retain(|f| *f != flag);
    }
}

/// One file or folder of one file system.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Index into `Manifest::entries`.
    pub id: u32,
    /// The instance it belongs to.
    pub instance: u32,
    /// File or folder.
    pub kind: EntryKind,
    /// Created by the formatter or the first mount (system files, `$Extend`
    /// children, `System Volume Information`).
    pub system: bool,
    /// Display path from the root, joined with `/`.
    pub path: String,
    /// The names on that path (a name can itself contain `/` or `\`).
    pub components: Vec<String>,
    /// Base record number.
    pub record: u64,
    /// Sequence number that references to the entry carry. A deleted
    /// entry's record header holds the next one.
    pub sequence: u16,
    /// Entry of the parent folder (`None` for a root and for raw parents).
    pub parent: Option<u32>,
    /// Every `$FILE_NAME`, in record order.
    pub names: Vec<EntryName>,
    /// Absolute offset of the base record as last written; `None` when the
    /// slot had been reused.
    pub record_offset: Option<u64>,
    /// Extension records: `(record number, absolute offset)`.
    pub extension_records: Vec<(u64, u64)>,
    /// Real size of the unnamed stream (0 for folders).
    pub size: u64,
    /// Allocated size of the unnamed stream.
    pub allocated_size: u64,
    /// Initialized size of the unnamed stream.
    pub initialized_size: u64,
    /// The unnamed stream is resident.
    pub resident: bool,
    /// Resident data: absolute offset of the record that holds it, offset of
    /// the value inside that record, length.
    pub resident_at: Option<(u64, u32, u32)>,
    /// Runs of the unnamed stream in VCN order.
    pub extents: Vec<Extent>,
    /// Attribute flags of the unnamed stream.
    pub data_flags: u16,
    /// Names of the named streams.
    pub streams: Vec<String>,
    /// Reparse tag.
    pub reparse_tag: Option<u32>,
    /// File attributes of `$STANDARD_INFORMATION`.
    pub file_attributes: u32,
    /// Times of `$STANDARD_INFORMATION`.
    pub times: EntryTimes,
    /// `$LogFile` sequence number of the base record.
    pub lsn: u64,
    /// Recipe of the logical content (files with generated content).
    pub content: Option<ContentSpec>,
    /// SHA-256 of the logical content.
    pub sha256: Option<String>,
    /// Deleted before the image was finished.
    pub deleted: bool,
    /// What is physically left.
    pub truth: Truth,
    /// What a correct catalog shows.
    pub expect: Expect,
    /// Free-form tags set by the scenario.
    pub tags: Vec<String>,
}

impl Entry {
    /// The last path component.
    pub fn name(&self) -> &str {
        self.components.last().map_or("", String::as_str)
    }

    /// The logical content, when it can be regenerated.
    pub fn content_bytes(&self) -> Option<Vec<u8>> {
        self.content.as_ref().filter(|c| c.is_available()).map(ContentSpec::to_vec)
    }

    /// `true` when a recovery of this file must be bit-identical to
    /// `sha256`: its records and its content are intact.
    pub fn must_recover_exactly(&self) -> bool {
        self.kind == EntryKind::File && self.truth.record_intact && self.truth.content_intact == Some(true)
    }
}

/// The ground truth of one image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Version of this structure.
    pub manifest_version: u32,
    /// Scenario name.
    pub scenario: String,
    /// What the image is.
    pub description: String,
    /// The paragraphs of ALGORITHMS.md the image exists to test.
    pub rules: Vec<String>,
    /// The seed everything was generated from.
    pub seed: u64,
    /// Image size in bytes.
    pub image_size: u64,
    /// Logical sector size a block source for the image should report.
    pub sector_size: u32,
    /// The session range `(start, end)` a test should scan. Truth and
    /// expectations are relative to it.
    pub scan_range: (u64, u64),
    /// Partitions of a partition table, if one was written.
    pub partitions: Vec<PartitionFact>,
    /// The file systems.
    pub instances: Vec<Instance>,
    /// Their files and folders.
    pub entries: Vec<Entry>,
    /// Every `FILE` record whose signature is still in the image.
    pub records: Vec<RecordFact>,
    /// Every `INDX` block whose signature is still in the image.
    pub index_blocks: Vec<IndexBlockFact>,
    /// Every NTFS boot sector still in the image.
    pub boot_sectors: Vec<BootFact>,
    /// The overwritten set of ALGORITHMS C5 as sorted, merged absolute byte
    /// ranges `(start, length)`: clusters the current file systems have
    /// allocated plus the extents of files deleted on them.
    pub overwritten_set: Vec<(u64, u64)>,
    /// Remarks.
    pub notes: Vec<String>,
}

impl Manifest {
    /// Serializes to JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a manifest always serializes")
    }

    /// Serializes to indented JSON.
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("a manifest always serializes")
    }

    /// Parses JSON written by [`Manifest::to_json`].
    pub fn from_json(json: &str) -> Result<Manifest, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// The instance with the given label.
    pub fn instance(&self, label: &str) -> &Instance {
        self.instances
            .iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| panic!("the manifest has no instance labelled {label:?}"))
    }

    /// Entries of one instance.
    pub fn entries_of<'a>(&'a self, label: &str) -> impl Iterator<Item = &'a Entry> + 'a {
        let id = self.instance(label).id;
        self.entries.iter().filter(move |e| e.instance == id)
    }

    /// The entry of instance `label` with display path `path`.
    pub fn entry(&self, label: &str, path: &str) -> &Entry {
        self.find_entry(label, path)
            .unwrap_or_else(|| panic!("instance {label:?} has no entry {path:?}"))
    }

    /// Like [`Manifest::entry`], `None` when there is no such entry.
    pub fn find_entry(&self, label: &str, path: &str) -> Option<&Entry> {
        let id = self.instance(label).id;
        self.entries.iter().find(|e| e.instance == id && e.path == path)
    }

    /// Mutable access to an entry, for scenario-specific expectations.
    pub fn entry_mut(&mut self, label: &str, path: &str) -> &mut Entry {
        let id = self.instance(label).id;
        self.entries
            .iter_mut()
            .find(|e| e.instance == id && e.path == path)
            .unwrap_or_else(|| panic!("instance {label:?} has no entry {path:?}"))
    }

    /// The files of all instances that are not system files.
    pub fn user_files(&self) -> impl Iterator<Item = &Entry> + '_ {
        self.entries.iter().filter(|e| !e.system && e.kind == EntryKind::File)
    }

    /// The `FILE` records of one instance that can still be found, damaged
    /// ones and copies included (see `RecordFact::state` and `role`).
    pub fn records_of(&self, instance: u32) -> impl Iterator<Item = &RecordFact> + '_ {
        self.records.iter().filter(move |r| r.instance == instance)
    }
}
