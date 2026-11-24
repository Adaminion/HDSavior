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
//! * **Expectations**: what a correct scan and catalog make of the image,
//!   by the rules of ALGORITHMS.md applied to the structures above:
//!   - per record (`RecordFact::expect`): its class (A3 dropped, C3 live or
//!     `post_format`, or old), its catalog instance, whether D2 marks it
//!     `older_generation`;
//!   - per MFT fragment (`fragments`): the contradictions and kept cuts of
//!     D2 with their boundary times (D3) and the instance of each segment;
//!   - the catalog (`catalog`): the instances of D4–D9 with kind, anchor,
//!     container, the geometry each must find with its F6 verdict and
//!     evidence counts, the lost-volume range of F5 and whether S2 protects
//!     it;
//!   - the overwritten set of C5 (`overwritten_set`) and the catalog
//!     warnings that can be decided (`warnings`);
//!   - per entry (`Entry::expect`): presence, catalog instance, path and
//!     health flags (`Instance::expected_kind` summarises the kind per
//!     physical file system).
//!
//!   Only what follows unambiguously from ALGORITHMS.md is stated: a flag in
//!   neither `flags_set` nor `flags_clear` is open, and so is a `None` path,
//!   instance, verdict or range. Where the builder's scenarios rely on D4
//!   joining pieces of one file system, the scenario tests check the
//!   evidence (see `crate::expect`).
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

/// What a file system on the disk is for a scan of the manifest's range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceRole {
    /// A current volume of ALGORITHMS C1: its own boot sector is at the
    /// range start or at a partition start of the table inside the range,
    /// and its record 0 is where that boot sector points. Its records are
    /// `live` or `post_format` and never shown.
    Current,
    /// A file system that something was formatted over, whose partition is
    /// gone, or whose boot sector is anywhere else: the thing to rescue.
    Lost,
    /// A file system inside a file of another one.
    Embedded,
    /// A file system whose partition lies wholly outside the scan range: a
    /// correct scan never sees it (a healthy neighbour partition, for
    /// example).
    Unscanned,
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
    /// Clusters of `$MFTMirr` (one run). Records inside them belong to the
    /// volume's `$MFTMirr`, which ALGORITHMS C3 calls `live` on a current
    /// volume.
    pub mftmirr_clusters: u64,
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
    /// The kind of the catalog instance that holds its shown entries, when
    /// they all lie in one (`None` otherwise, and for current volumes, which
    /// are not shown). Derived from `Manifest::catalog`.
    pub expected_kind: Option<InstanceKind>,
    /// How many file systems were formatted over it afterwards: 0 for the
    /// newest at its place.
    pub generation: u32,
    /// The formatter it imitates.
    pub flavor: Flavor,
    /// Its geometry as it was formatted. What a correct catalog finds for
    /// the instance that holds its records is `CatalogInstance::geometry`.
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
    /// The catalog instance (`Manifest::catalog`) whose tree holds the node;
    /// `None` when the entry is not shown or the manifest does not say.
    pub instance: Option<u32>,
    /// Path from the root of its catalog instance, when it is certain (all
    /// ancestors survived, a surviving index entry or stale copy names the
    /// lost ones, or the scenario states it).
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
        self.content
            .as_ref()
            .filter(|c| c.is_available())
            .map(ContentSpec::to_vec)
    }

    /// `true` when a recovery of this file must be bit-identical to
    /// `sha256`: its records and its content are intact.
    pub fn must_recover_exactly(&self) -> bool {
        self.kind == EntryKind::File
            && self.truth.record_intact
            && self.truth.content_intact == Some(true)
    }
}

/// How a correct scan classifies a `FILE` record (ALGORITHMS A3, C3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordClass {
    /// Counted and dropped at scan time (A3): never used (not in use, no
    /// attributes), or every 512-byte block fails its fixup check.
    Dropped,
    /// A record of a current volume in a slot its `$MFT:$BITMAP` marks as
    /// in use, or a copy inside its `$MFTMirr` (C3): hidden, counted.
    Live,
    /// A record in a free slot of a current volume's `$MFT` that the current
    /// volume wrote after the format (C3): hidden; its non-resident extents,
    /// mapped with the current geometry, are part of the overwritten set.
    PostFormat,
    /// Everything else: the input of instance grouping (D).
    Old,
}

/// What a correct catalog makes of one `FILE` record.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordExpect {
    /// Its class; `None` for records outside the scan range (a correct
    /// scan never reads them) and for records whose bytes the manifest does
    /// not describe (`ArtifactState::Damaged`).
    pub class: Option<RecordClass>,
    /// Old records: the catalog instance (`Manifest::catalog`) it belongs
    /// to; `None` when not stated, or when that instance produces no node
    /// and is therefore not reported (ALGORITHMS D9).
    pub instance: Option<u32>,
    /// Old records: whether ALGORITHMS D2 marks it `older_generation` (the
    /// higher record of an unexplained contradiction); `None` when not
    /// stated.
    pub older_generation: Option<bool>,
}

/// Geometry a correct catalog finds for one of its instances (ALGORITHMS F).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedGeometry {
    /// Cluster size in bytes.
    pub cluster_size: u32,
    /// Absolute byte offset of cluster 0.
    pub volume_offset: u64,
    /// Taken from `main` for lack of evidence of its own (F4, source
    /// `inherited`).
    pub inherited: bool,
    /// F6: `Some(true)` or `Some(false)`; `None` where the verdict depends
    /// on a detail the manifest cannot predict (which 200 files the sample
    /// of F3 picks, for example).
    pub validated: Option<bool>,
    /// Distinct directories whose intact, non-current index blocks vote for
    /// this geometry (F2).
    pub voting_directories: u32,
    /// Records besides record 0 that the instance's own `$MFT` runlist
    /// places where they were found (F2 MFT map, F6); 0 without record 0.
    pub mft_map_records: u32,
    /// Checkable files (F3) under this geometry.
    pub signature_checkable: u32,
    /// Of those, files whose first bytes show their extension's signature.
    pub signature_matches: u32,
    /// `total_clusters` of F5; `None` when the rule does not decide it
    /// (several agreeing boot hits with different sizes, or readings of
    /// "files" that disagree).
    pub total_clusters: Option<u64>,
}

/// One instance a correct catalog builds (ALGORITHMS D).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogInstance {
    /// Index into `Manifest::catalog`.
    pub id: u32,
    /// Its kind (D9).
    pub kind: InstanceKind,
    /// Readable name: the label of its physical instance, `+`-joined when
    /// it holds several, `strays` for the strays pseudo-instance.
    pub label: String,
    /// The physical instances (`Manifest::instances`) whose records it holds.
    pub sources: Vec<u32>,
    /// Old records it holds.
    pub records: u32,
    /// The anchor of D8: it holds old records at the origin of a current
    /// volume's `$MFT` (segment 0 of that fragment).
    pub anchor: bool,
    /// Embedded only: the manifest entry of the container file (D7).
    pub container: Option<u32>,
    /// The geometry it must find; `None` when not stated (strays, an
    /// embedded volume in a fragmented container).
    pub geometry: Option<ExpectedGeometry>,
    /// The lost-volume range of F5, `[start, end)` clipped to the image;
    /// `None` when not stated.
    pub lost_volume_range: Option<(u64, u64)>,
    /// Whether S2 protects that range (a `main` or `other` instance with
    /// validated geometry); `None` when the validation is not stated.
    pub protected: Option<bool>,
}

/// Why two records cannot belong to one file system (ALGORITHMS D2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContradictionKind {
    /// (A): both map the same cluster number in an unnamed `$DATA` or
    /// `$INDEX_ALLOCATION:$I30` run.
    SharedCluster,
    /// (B): one names the other's slot as its parent, and the record there
    /// has another sequence number or is not a directory.
    ParentSlot,
    /// (B): one names the other's slot as its base record, and the record
    /// there has another sequence number.
    BaseSlot,
}

/// A contradiction as D2 keeps it: per `hi`, the highest-numbered `lo`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContradictionFact {
    /// The lower record number.
    pub lo: u64,
    /// The higher record number (the stale record).
    pub hi: u64,
    /// The kinds of contradiction found between the two records.
    pub kinds: Vec<ContradictionKind>,
    /// The kept cut inside `(lo, hi]`; `None` = unexplained (`hi` is marked
    /// `older_generation`, warning `mixed_generations`).
    pub explained_by: Option<u64>,
}

/// A kept generation cut (ALGORITHMS D2, D3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutFact {
    /// The record number `p`: records numbered `≥ p` are the older side.
    pub position: u64,
    /// `B_c`: the newest change time of the segment directly above.
    pub older_newest: u64,
    /// `A_c`: the oldest change time of the segment directly below.
    pub newer_oldest: u64,
    /// Distinct `hi` records of the contradictions whose interval holds it.
    pub support: u32,
}

/// One MFT fragment of old records (ALGORITHMS D1) and what D2 finds in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentFact {
    /// Record size of its records.
    pub record_size: u32,
    /// `disk offset − number × record size` (signed).
    pub origin: i64,
    /// Old records in it, unnumbered ones that join it included.
    pub records: u32,
    /// Lowest and highest record number.
    pub numbers: (u64, u64),
    /// The contradictions, by ascending `hi`.
    pub contradictions: Vec<ContradictionFact>,
    /// The kept cuts, ascending.
    pub cuts: Vec<CutFact>,
    /// Per segment (0 = lowest record numbers): the catalog instance that
    /// holds it, `None` when not stated.
    pub segment_instances: Vec<Option<u32>>,
}

/// Whether a correct catalog raises a warning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarningExpect {
    /// The PROTOCOL warning code, e.g. `mixed_generations`.
    pub code: String,
    /// `true`: it must be raised; `false`: it must not be.
    pub raised: bool,
}

/// Version of the manifest structure: 2 added the catalog, the record
/// expectations and the generation analysis (ALGORITHMS as rewritten in
/// commit 7a70c4e).
pub const MANIFEST_VERSION: u32 = 2;

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
    /// allocated plus the extents of `post_format` records.
    pub overwritten_set: Vec<(u64, u64)>,
    /// The instances a correct catalog builds (ALGORITHMS D). Current
    /// volumes are not among them.
    pub catalog: Vec<CatalogInstance>,
    /// Every MFT fragment of old records with the generation analysis of
    /// ALGORITHMS D2.
    pub fragments: Vec<FragmentFact>,
    /// Catalog warnings (PROTOCOL `Warning` codes) the manifest can decide:
    /// raised or not raised by a correct catalog. Codes not listed are open.
    pub warnings: Vec<WarningExpect>,
    /// Remarks.
    pub notes: Vec<String>,
}

impl Manifest {
    /// A manifest with nothing in it, for code that fills one in by hand.
    pub(crate) fn blank(scenario: &str, image_size: u64) -> Manifest {
        Manifest {
            manifest_version: MANIFEST_VERSION,
            scenario: scenario.to_string(),
            description: String::new(),
            rules: Vec::new(),
            seed: 0,
            image_size,
            sector_size: 512,
            scan_range: (0, image_size),
            partitions: Vec::new(),
            instances: Vec::new(),
            entries: Vec::new(),
            records: Vec::new(),
            index_blocks: Vec::new(),
            boot_sectors: Vec::new(),
            overwritten_set: Vec::new(),
            catalog: Vec::new(),
            fragments: Vec::new(),
            warnings: Vec::new(),
            notes: Vec::new(),
        }
    }

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
        self.entries
            .iter()
            .find(|e| e.instance == id && e.path == path)
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
        self.entries
            .iter()
            .filter(|e| !e.system && e.kind == EntryKind::File)
    }

    /// The `FILE` records of one instance that can still be found, damaged
    /// ones and copies included (see `RecordFact::state` and `role`).
    pub fn records_of(&self, instance: u32) -> impl Iterator<Item = &RecordFact> + '_ {
        self.records.iter().filter(move |r| r.instance == instance)
    }

    /// The `main` catalog instance, if a correct catalog has one.
    pub fn main_instance(&self) -> Option<&CatalogInstance> {
        self.catalog.iter().find(|c| c.kind == InstanceKind::Main)
    }

    /// The catalog instances of one kind.
    pub fn catalog_of_kind(
        &self,
        kind: InstanceKind,
    ) -> impl Iterator<Item = &CatalogInstance> + '_ {
        self.catalog.iter().filter(move |c| c.kind == kind)
    }

    /// The catalog instance an entry is expected in.
    pub fn catalog_of(&self, e: &Entry) -> Option<&CatalogInstance> {
        e.expect.instance.map(|id| &self.catalog[id as usize])
    }

    /// The catalog instance that holds the shown entries of the physical
    /// instance `label`, when they all lie in one.
    pub fn catalog_for(&self, label: &str) -> Option<&CatalogInstance> {
        let id = self.instance(label).id;
        let mut ids = self
            .entries
            .iter()
            .filter(|e| e.instance == id && e.expect.presence == Presence::Shown)
            .map(|e| e.expect.instance);
        let first = ids.next()??;
        ids.all(|i| i == Some(first))
            .then(|| &self.catalog[first as usize])
    }
}
