//! A simulated NTFS volume: the model behind an image.
//!
//! A [`Volume`] is a description of a file system (geometry, cluster
//! allocation, MFT slots, files and folders with their attributes) that can
//! be *rendered* into an image any number of times. Operations such as
//! [`Volume::add_file`] or [`Volume::delete`] change the model the way the
//! NTFS driver changes a volume: deleting clears the in-use flag, bumps the
//! sequence number and leaves everything else in the record; a reused slot
//! starts with the bumped number; freed clusters become available again.
//! Nothing is written until the volume is rendered through
//! [`crate::disk::Disk::render`].
//!
//! Faults that no driver produces (torn records, lying record numbers,
//! parent cycles, stale references) are expressed through [`spec::Quirks`],
//! raw records ([`Volume::put_raw_record`]) and plain writes to the image
//! after rendering.

pub mod alloc;
mod assemble;
pub mod params;
mod render;
pub mod spec;
mod system;

use std::collections::BTreeMap;

use crate::content::ContentSpec;
use crate::ondisk::index::Tree;
use crate::ondisk::record::{AttrModel, RecordModel};
use crate::ondisk::runlist::{total_clusters, Run};
use crate::ondisk::{next_seq, FileRef};
use crate::rng::{derive, derive_n, Rng};
use crate::time;

use alloc::Allocator;
use params::{Flavor, NamePolicy, VolumeParams};
use spec::{
    DirSpec, FileSpec, MultiSpec, Name, NameSpec, Piece, Placement, Quirks, StorageSpec, Times,
};

pub(crate) use render::{RenderSink, Rendered};

/// Handle of a file or folder inside its [`Volume`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(crate) usize);

/// What a name's parent reference points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ParentRef {
    /// A folder of this volume; the reference is its record and sequence.
    Node(NodeId),
    /// A literal reference (stale or dangling ones).
    Raw(FileRef),
}

/// One `$FILE_NAME` of a node.
#[derive(Clone, Debug)]
pub(crate) struct NameLink {
    pub parent: ParentRef,
    pub name: Vec<u16>,
    pub namespace: u8,
    /// Times of the copy in the record (written at creation, then stale).
    pub times: [u64; 4],
    /// Listed in the parent's index while the node is in use.
    pub indexed: bool,
}

/// Data the builder generates itself for system files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemData {
    /// `$MFT:$DATA`: the records are rendered slot by slot.
    Mft,
    /// `$MFT:$BITMAP`.
    MftBitmap,
    /// `$MFTMirr:$DATA`: copies of the first records.
    MftMirr,
    /// `$Bitmap:$DATA`.
    Bitmap,
    /// `$Boot:$DATA`.
    Boot,
    /// `$UpCase:$DATA`.
    UpCase,
    /// `$AttrDef:$DATA`.
    AttrDef,
    /// One repeated byte (`$LogFile` is 0xFF after a format).
    Fill(u8),
    /// Recognisable filler with this four-character tag.
    Filler([u8; 4]),
}

/// Where the bytes of a stream come from.
#[derive(Clone, Debug)]
pub(crate) enum Source {
    /// A content recipe (user files).
    Spec(ContentSpec),
    /// Builder-generated system data.
    System(SystemData),
    /// The clusters are allocated but never written.
    Unwritten,
}

/// One `$DATA`-like attribute of a node.
#[derive(Clone, Debug)]
pub(crate) struct Stream {
    pub type_code: u32,
    pub name: Vec<u16>,
    pub real_size: u64,
    pub initialized: u64,
    /// `None` = resident.
    pub runs: Option<Vec<Run>>,
    pub flags: u16,
    pub wide_header: bool,
    pub source: Source,
    /// The clusters hold bytes that are not the content (compressed and
    /// encrypted files).
    pub garbage: bool,
    pub stale_seed: u64,
}

impl Stream {
    pub(crate) fn is_unnamed_data(&self) -> bool {
        self.type_code == crate::ondisk::record::attr_type::DATA && self.name.is_empty()
    }
}

/// Index state of a folder.
#[derive(Clone, Debug)]
pub(crate) struct DirState {
    /// Clusters of the index allocation, in VCN order.
    pub runs: Vec<Run>,
    pub force_blocks: bool,
    pub fill_percent: u32,
    pub fragment: bool,
    pub lsn: u64,
    pub usn: u16,
}

/// One extension record of a node.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExtRecord {
    pub record: u64,
    /// Sequence number while in use.
    pub seq: u16,
    /// The slot was taken over by another node.
    pub superseded: bool,
}

/// A file, folder or system file of a volume.
#[derive(Clone, Debug)]
pub(crate) struct Node {
    pub record: u64,
    /// Sequence number while in use: what references to the node carry.
    pub seq: u16,
    pub in_use: bool,
    /// The base record's slot was taken over by another node.
    pub superseded: bool,
    pub is_dir: bool,
    /// Created by the formatter or the first mount.
    pub system: bool,
    /// Extra header flags (0x4, 0x8).
    pub header_flags: u16,
    pub names: Vec<NameLink>,
    /// `$STANDARD_INFORMATION` times.
    pub times: [u64; 4],
    pub file_attributes: u32,
    pub has_si: bool,
    pub si_long: bool,
    pub security_id: u32,
    pub security_descriptor: bool,
    pub streams: Vec<Stream>,
    /// Resident attributes written as they are.
    pub extra_attrs: Vec<AttrModel>,
    pub reparse: Option<(u32, Vec<u8>)>,
    pub efs: bool,
    pub dir: Option<DirState>,
    pub multi: Option<MultiSpec>,
    pub extensions: Vec<ExtRecord>,
    /// Clusters of a non-resident attribute list.
    pub list_runs: Vec<Run>,
    pub quirks: Quirks,
    pub lsn: u64,
    pub usn: u16,
    pub tags: Vec<String>,
}

impl Node {
    pub(crate) fn data(&self) -> Option<&Stream> {
        self.streams.iter().find(|s| s.is_unnamed_data())
    }

    pub(crate) fn reference(&self) -> FileRef {
        FileRef::new(self.record, self.seq)
    }

    /// Sequence number in the record header right now.
    pub(crate) fn header_seq(&self) -> u16 {
        if self.in_use {
            self.seq
        } else {
            next_seq(self.seq)
        }
    }
}

/// Who owns an MFT slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    Base(NodeId),
    Ext(NodeId, usize),
    Raw,
}

/// A record placed by hand.
#[derive(Clone, Debug)]
pub(crate) struct RawRecord {
    pub model: RecordModel,
    pub in_bitmap: bool,
}

/// One contiguous piece of the `$MFT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MftExtent {
    /// First cluster.
    pub lcn: u64,
    /// Number of clusters.
    pub clusters: u64,
    /// Number of the first record in this piece.
    pub first_record: u64,
    /// Number of records in this piece.
    pub records: u64,
}

/// Where the formatter put the system structures.
#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub total_clusters: u64,
    pub mft: Vec<MftExtent>,
    pub mftmirr: Run,
    pub mirror_records: u64,
    pub logfile: Run,
    pub bitmap: Run,
    pub mft_bitmap: Run,
    pub boot: Run,
    pub root_index: Run,
}

/// A simulated NTFS volume. See the module documentation.
pub struct Volume {
    pub(crate) params: VolumeParams,
    pub(crate) seed: u64,
    pub(crate) serial: u64,
    pub(crate) plan: Plan,
    pub(crate) alloc: Allocator,
    pub(crate) nodes: Vec<Node>,
    pub(crate) slots: BTreeMap<u64, Slot>,
    /// Sequence number currently in the header of every slot ever used.
    pub(crate) slot_seq: BTreeMap<u64, u16>,
    pub(crate) raw: BTreeMap<u64, RawRecord>,
    /// Slots marked in the MFT bitmap although nothing of this volume is in them.
    pub(crate) extra_bitmap_slots: std::collections::BTreeSet<u64>,
    pub(crate) trees: BTreeMap<usize, Tree>,
    pub(crate) clock: u64,
    pub(crate) lsn: u64,
    pub(crate) next_record: u64,
    pub(crate) first_user_record: u64,
    pub(crate) rng: Rng,
    pub(crate) rendered: Option<Rendered>,
    /// The model changed since the last render.
    pub(crate) dirty: bool,
}

const ROOT_RECORD: u64 = 5;

impl Volume {
    // ------------------------------------------------------------ geometry

    /// The parameters the volume was formatted with.
    pub fn params(&self) -> &VolumeParams {
        &self.params
    }

    /// Cluster size in bytes.
    pub fn cluster_size(&self) -> u64 {
        self.params.cluster_size as u64
    }

    /// MFT record size in bytes.
    pub fn record_size(&self) -> u64 {
        self.params.record_size as u64
    }

    /// Byte offset of the volume in the image.
    pub fn offset(&self) -> u64 {
        self.params.offset
    }

    /// Clusters of the volume.
    pub fn total_clusters(&self) -> u64 {
        self.plan.total_clusters
    }

    /// The volume serial number.
    pub fn serial(&self) -> u64 {
        self.serial
    }

    /// The pieces of the `$MFT`.
    pub fn mft_extents(&self) -> &[MftExtent] {
        &self.plan.mft
    }

    /// Number of MFT record slots.
    pub fn mft_records(&self) -> u64 {
        self.params.mft_records()
    }

    /// Slots covered by the `$MFT`'s initialized size.
    pub fn mft_initialized_records(&self) -> u64 {
        self.params
            .mft_initialized_records
            .unwrap_or_else(|| self.mft_records())
            .min(self.mft_records())
    }

    /// Absolute byte offset of cluster `lcn`.
    pub fn lcn_to_offset(&self, lcn: u64) -> u64 {
        self.params.offset + lcn * self.cluster_size()
    }

    /// The cluster that holds the absolute byte `offset` (which must lie
    /// inside the volume).
    pub fn offset_to_lcn(&self, offset: u64) -> u64 {
        assert!(offset >= self.params.offset, "offset before the volume");
        let lcn = (offset - self.params.offset) / self.cluster_size();
        assert!(lcn < self.plan.total_clusters, "offset behind the volume");
        lcn
    }

    /// Absolute byte offset of MFT record `number`.
    pub fn record_offset(&self, number: u64) -> u64 {
        let extent = self
            .plan
            .mft
            .iter()
            .find(|e| (e.first_record..e.first_record + e.records).contains(&number))
            .unwrap_or_else(|| {
                panic!(
                    "record {number} is outside the MFT ({} records)",
                    self.mft_records()
                )
            });
        self.lcn_to_offset(extent.lcn) + (number - extent.first_record) * self.record_size()
    }

    /// The first record number given to user files.
    pub fn first_user_record(&self) -> u64 {
        self.first_user_record
    }

    /// Makes automatic record numbers start at `number` or later: for
    /// records that must survive a format that rewrites the slots below it,
    /// while lower slots stay free for the scenario. (Ignored while
    /// `VolumeParams::reuse_records` is on, which always takes the lowest
    /// free slot.)
    pub fn number_from(&mut self, number: u64) {
        self.next_record = self.next_record.max(number);
    }

    /// The cluster allocator, for scenarios that place things by hand.
    pub fn allocator(&mut self) -> &mut Allocator {
        self.dirty = true;
        &mut self.alloc
    }

    /// Read access to the cluster allocator.
    pub fn allocation(&self) -> &Allocator {
        &self.alloc
    }

    // --------------------------------------------------------------- clock

    /// The time the next operation will be stamped with.
    pub fn clock(&self) -> u64 {
        self.clock
    }

    /// Sets the time of the next operation.
    pub fn set_clock(&mut self, filetime: u64) {
        self.clock = filetime;
    }

    /// Moves the clock forward.
    pub fn advance(&mut self, ticks: u64) {
        self.clock += ticks;
    }

    fn tick(&mut self) -> u64 {
        self.dirty = true;
        let now = self.clock;
        self.clock += 20 * time::SECOND + self.rng.below(10 * time::MINUTE);
        now
    }

    pub(crate) fn next_lsn(&mut self) -> u64 {
        if self.params.flavor == Flavor::Mkntfs {
            return 0;
        }
        self.lsn += 0x60 + 8 * self.rng.below(0x200);
        self.lsn
    }

    // --------------------------------------------------------------- nodes

    /// The root directory (record 5).
    pub fn root(&self) -> NodeId {
        match self.slots.get(&ROOT_RECORD) {
            Some(Slot::Base(id)) => *id,
            _ => panic!("the volume has no root directory"),
        }
    }

    /// The node whose base record is `number`, if any.
    pub fn node_at(&self, number: u64) -> Option<NodeId> {
        match self.slots.get(&number) {
            Some(Slot::Base(id)) => Some(*id),
            _ => None,
        }
    }

    pub(crate) fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0]
    }

    pub(crate) fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.0]
    }

    /// The reference other records use for `node`: its base record number
    /// and the sequence number it had while in use.
    pub fn reference(&self, node: NodeId) -> FileRef {
        self.node(node).reference()
    }

    /// Record numbers of the extension records of `node`.
    pub fn extension_records(&self, node: NodeId) -> Vec<u64> {
        self.node(node)
            .extensions
            .iter()
            .map(|e| e.record)
            .collect()
    }

    /// `true` until the node is deleted.
    pub fn is_in_use(&self, node: NodeId) -> bool {
        self.node(node).in_use
    }

    /// The runs of the unnamed `$DATA` of `node` (empty when resident or
    /// absent).
    pub fn runs(&self, node: NodeId) -> Vec<Run> {
        self.node(node)
            .data()
            .and_then(|s| s.runs.clone())
            .unwrap_or_default()
    }

    /// The runs of the index allocation of a folder, as of the last render.
    pub fn index_runs(&self, node: NodeId) -> Vec<Run> {
        self.node(node)
            .dir
            .as_ref()
            .map(|d| d.runs.clone())
            .unwrap_or_default()
    }

    /// Absolute byte ranges `(offset, length)` of the allocated runs of the
    /// unnamed `$DATA` of `node`.
    pub fn byte_ranges(&self, node: NodeId) -> Vec<(u64, u64)> {
        self.runs(node)
            .iter()
            .filter_map(|r| {
                r.lcn
                    .map(|lcn| (self.lcn_to_offset(lcn), r.len * self.cluster_size()))
            })
            .collect()
    }

    /// The oddities of a node, for changing them after creation.
    pub fn quirks_mut(&mut self, node: NodeId) -> &mut Quirks {
        self.touch_record(node);
        &mut self.node_mut(node).quirks
    }

    /// Attaches a free-form tag that ends up in the manifest entry.
    pub fn tag(&mut self, node: NodeId, tag: &str) {
        self.node_mut(node).tags.push(tag.to_string());
    }

    /// All user files and folders (not the system files), in creation order.
    pub fn user_nodes(&self) -> Vec<NodeId> {
        (0..self.nodes.len())
            .filter(|&i| !self.nodes[i].system)
            .map(NodeId)
            .collect()
    }

    /// The children of a folder that are in use, in creation order.
    pub fn children(&self, dir: NodeId) -> Vec<NodeId> {
        (0..self.nodes.len())
            .filter(|&i| {
                let n = &self.nodes[i];
                n.in_use && i != dir.0 && n.names.iter().any(|l| l.parent == ParentRef::Node(dir))
            })
            .map(NodeId)
            .collect()
    }

    pub(crate) fn parent_ref(&self, parent: &ParentRef) -> FileRef {
        match parent {
            ParentRef::Node(id) => self.node(*id).reference(),
            ParentRef::Raw(r) => *r,
        }
    }

    fn touch_record(&mut self, node: NodeId) {
        self.dirty = true;
        let lsn = self.next_lsn();
        let n = self.node_mut(node);
        n.lsn = lsn;
        n.usn = bump_usn(n.usn);
    }

    /// Records a change of the folder's content at time `now`.
    fn touch_dir(&mut self, dir: NodeId, now: u64) {
        let lsn = self.next_lsn();
        let index_lsn = self.next_lsn();
        let n = self.node_mut(dir);
        n.times[1] = now;
        n.times[2] = now;
        n.times[3] = now;
        n.lsn = lsn;
        n.usn = bump_usn(n.usn);
        if let Some(d) = n.dir.as_mut() {
            d.lsn = index_lsn;
            d.usn = bump_usn(d.usn);
        }
    }

    // ------------------------------------------------------- record slots

    fn slot_is_free(&self, number: u64) -> bool {
        match self.slots.get(&number) {
            None => true,
            Some(Slot::Raw) => false,
            Some(Slot::Base(id)) => !self.node(*id).in_use,
            Some(Slot::Ext(id, _)) => !self.node(*id).in_use,
        }
    }

    /// Takes a record slot for a new record and returns its number and the
    /// sequence number the new record starts with.
    pub(crate) fn take_record(&mut self, explicit: Option<u64>) -> (u64, u16) {
        let capacity = self.mft_records();
        let number = match explicit {
            Some(n) => {
                assert!(
                    n < capacity,
                    "record {n} is outside the MFT ({capacity} records)"
                );
                assert!(self.slot_is_free(n), "record {n} is in use");
                n
            }
            None if self.params.reuse_records => (self.first_user_record..capacity)
                .find(|&n| self.slot_is_free(n))
                .unwrap_or_else(|| panic!("the MFT is full ({capacity} records)")),
            None => {
                let n = (self.next_record..capacity)
                    .find(|n| !self.slots.contains_key(n))
                    .unwrap_or_else(|| {
                        panic!("the MFT is full ({capacity} records); raise its capacity")
                    });
                self.next_record = n + 1;
                n
            }
        };
        // whoever had the slot before loses its record
        match self.slots.get(&number).copied() {
            Some(Slot::Base(old)) => self.node_mut(old).superseded = true,
            Some(Slot::Ext(old, k)) => self.node_mut(old).extensions[k].superseded = true,
            Some(Slot::Raw) | None => {}
        }
        let seq = self.slot_seq.get(&number).copied().unwrap_or(1);
        self.slot_seq.insert(number, seq);
        (number, seq)
    }

    fn take_extensions(&mut self, node: NodeId, count: usize, explicit: &[Option<u64>]) {
        for k in self.node(node).extensions.len()..count {
            let (record, seq) = self.take_record(explicit.get(k).copied().flatten());
            self.slots.insert(record, Slot::Ext(node, k));
            self.node_mut(node).extensions.push(ExtRecord {
                record,
                seq,
                superseded: false,
            });
        }
    }

    // --------------------------------------------------------------- names

    fn default_names(&self, parent: NodeId, name: &Name) -> Vec<(Vec<u16>, u8)> {
        match self.params.effective_name_policy() {
            NamePolicy::Posix => vec![(name.0.clone(), 0)],
            NamePolicy::Win32 => vec![(name.0.clone(), 1)],
            NamePolicy::Win32AndDos => {
                if is_dos_name(&name.0) {
                    vec![(name.0.clone(), 3)]
                } else {
                    let alias = self.dos_alias(parent, &name.0);
                    // the DOS alias comes first in a Windows record
                    vec![(alias, 2), (name.0.clone(), 1)]
                }
            }
        }
    }

    fn dos_alias(&self, parent: NodeId, long: &[u16]) -> Vec<u16> {
        let taken = |candidate: &[u16]| {
            self.nodes.iter().any(|n| {
                n.in_use
                    && n.names.iter().any(|l| {
                        l.parent == ParentRef::Node(parent)
                            && l.name.len() == candidate.len()
                            && l.name
                                .iter()
                                .zip(candidate)
                                .all(|(a, b)| ascii_upper(*a) == ascii_upper(*b))
                    })
            })
        };
        (1..10_000u32)
            .map(|n| dos_alias_candidate(long, n))
            .find(|c| !taken(c))
            .expect("some alias is free")
    }

    fn make_links(
        &self,
        parent: NodeId,
        name: &Name,
        explicit: &Option<Vec<NameSpec>>,
        created: u64,
    ) -> Vec<NameLink> {
        let names: Vec<(Vec<u16>, u8)> = match explicit {
            Some(list) => {
                assert!(!list.is_empty(), "an explicit name list must not be empty");
                list.iter()
                    .map(|n| (n.name.0.clone(), n.namespace))
                    .collect()
            }
            None => self.default_names(parent, name),
        };
        names
            .into_iter()
            .map(|(name, namespace)| {
                assert!(
                    (1..=255).contains(&name.len()),
                    "a name has 1 to 255 UTF-16 units"
                );
                assert!(namespace <= 3, "namespace {namespace} does not exist");
                NameLink {
                    parent: ParentRef::Node(parent),
                    name,
                    namespace,
                    times: [created; 4],
                    indexed: true,
                }
            })
            .collect()
    }

    fn default_times(&mut self, now: u64) -> [u64; 4] {
        // A third of the files look copied: their modification time is
        // older than their creation on this volume. The MFT change time is
        // the one that cannot be back-dated.
        let modified = if self.rng.chance(1, 3) {
            now.saturating_sub(self.rng.range(time::DAY, 700 * time::DAY))
        } else {
            now + self.rng.below(5 * time::SECOND)
        };
        let changed = now + self.rng.range(1, 20) * time::SECOND / 10;
        let accessed = changed + self.rng.below(2 * time::HOUR);
        [now, modified, changed, accessed]
    }

    fn new_node(&mut self, record: u64, seq: u16, is_dir: bool) -> Node {
        let lsn = self.next_lsn();
        let windows = self.params.flavor != Flavor::Mkntfs;
        Node {
            record,
            seq,
            in_use: true,
            superseded: false,
            is_dir,
            system: false,
            header_flags: 0,
            names: Vec::new(),
            times: [0; 4],
            file_attributes: 0,
            has_si: true,
            si_long: windows,
            security_id: if windows { 0x107 } else { 0 },
            security_descriptor: !windows,
            streams: Vec::new(),
            extra_attrs: Vec::new(),
            reparse: None,
            efs: false,
            dir: None,
            multi: None,
            extensions: Vec::new(),
            list_runs: Vec::new(),
            quirks: Quirks::default(),
            lsn,
            usn: 1,
            tags: Vec::new(),
        }
    }

    fn push_node(&mut self, node: Node) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.slots.insert(node.record, Slot::Base(id));
        self.nodes.push(node);
        id
    }

    fn assert_dir(&self, parent: NodeId) {
        let p = self.node(parent);
        assert!(p.is_dir, "record {} is not a folder", p.record);
        assert!(p.in_use, "folder in record {} was deleted", p.record);
    }

    // ------------------------------------------------------------- folders

    /// Creates a folder named `name` in `parent`.
    pub fn mkdir(&mut self, parent: NodeId, name: impl Into<Name>) -> NodeId {
        self.mkdir_with(parent, name, DirSpec::new())
    }

    /// Creates a folder with explicit settings.
    pub fn mkdir_with(&mut self, parent: NodeId, name: impl Into<Name>, spec: DirSpec) -> NodeId {
        self.assert_dir(parent);
        let name = name.into();
        let now = self.tick();
        let (record, seq) = self.take_record(spec.record);
        let mut node = self.new_node(record, seq, true);
        node.times = spec.times.map(Times::array).unwrap_or([now; 4]);
        node.file_attributes = spec.file_attributes;
        node.names = self.make_links(parent, &name, &spec.names, node.times[0]);
        let mut runs = Vec::new();
        for run in &spec.index_runs {
            let lcn = run.lcn.expect("index runs cannot be holes");
            runs.push(self.alloc.alloc_at(lcn, run.len));
        }
        let index_lsn = self.next_lsn();
        node.dir = Some(DirState {
            runs,
            force_blocks: spec.force_index_blocks,
            fill_percent: spec.index_fill_percent,
            fragment: spec.fragment_index,
            lsn: index_lsn,
            usn: 1,
        });
        node.quirks = spec.quirks;
        let extension_count = spec.multi.as_ref().map_or(0, MultiSpec::highest_holder);
        let explicit = spec
            .multi
            .as_ref()
            .map(|m| m.extension_records.clone())
            .unwrap_or_default();
        node.multi = spec.multi;
        let id = self.push_node(node);
        self.take_extensions(id, extension_count, &explicit);
        self.ensure_list_allocated(id);
        self.touch_dir(parent, now);
        id
    }

    // --------------------------------------------------------------- files

    /// Adds a file named `name` to `parent`.
    pub fn add_file(&mut self, parent: NodeId, name: impl Into<Name>, spec: FileSpec) -> NodeId {
        self.assert_dir(parent);
        let name = name.into();
        let now = self.tick();
        let (record, seq) = self.take_record(spec.record);
        let mut node = self.new_node(record, seq, false);
        node.times = match spec.times {
            Some(t) => t.array(),
            None => self.default_times(now),
        };
        node.file_attributes = spec.file_attributes;
        node.names = self.make_links(parent, &name, &spec.names, node.times[0]);
        node.reparse = spec.reparse.clone();
        node.efs = spec.efs_stream;
        node.quirks = spec.quirks.clone();
        let stale_seed = derive_n(self.seed, 0x5354_414C_4500 ^ record);

        let cs = self.cluster_size();
        let mut resident_auto = false;
        if !spec.no_data_attribute {
            let len = spec.content.len;
            let plain = spec.placement == Placement::Auto
                && spec.initialized.is_none()
                && !spec.garbage_on_disk
                && spec.extra_clusters == 0;
            let resident = match spec.storage {
                StorageSpec::Resident => true,
                StorageSpec::NonResident => false,
                StorageSpec::Auto => plain && len < self.record_size(),
            };
            resident_auto = resident && spec.storage == StorageSpec::Auto;
            let stream = if resident {
                Stream {
                    type_code: crate::ondisk::record::attr_type::DATA,
                    name: Vec::new(),
                    real_size: len,
                    initialized: len,
                    runs: None,
                    flags: spec.attr_flags,
                    wide_header: false,
                    source: Source::Spec(spec.content.clone()),
                    garbage: false,
                    stale_seed,
                }
            } else {
                self.non_resident_stream(&spec, stale_seed)
            };
            node.streams.push(stream);
        }
        for s in &spec.streams {
            let resident = !s.non_resident && s.content.len <= 256;
            let runs = if resident {
                None
            } else {
                let need = s.content.len.div_ceil(cs);
                Some(if need > 0 {
                    vec![self.alloc.alloc(need)]
                } else {
                    Vec::new()
                })
            };
            node.streams.push(Stream {
                type_code: crate::ondisk::record::attr_type::DATA,
                name: s.name.0.clone(),
                real_size: s.content.len,
                initialized: s.content.len,
                runs,
                flags: 0,
                wide_header: false,
                source: Source::Spec(s.content.clone()),
                garbage: false,
                stale_seed: derive_n(stale_seed, node.streams.len() as u64),
            });
        }

        let extension_count = spec.multi.as_ref().map_or(0, MultiSpec::highest_holder);
        let explicit = spec
            .multi
            .as_ref()
            .map(|m| m.extension_records.clone())
            .unwrap_or_default();
        node.multi = spec.multi.clone();
        let id = self.push_node(node);
        self.take_extensions(id, extension_count, &explicit);

        // A file that was to be resident "if it fits" becomes non-resident
        // when its record overflows, as NTFS does.
        if resident_auto && self.assemble(id, None).is_err() {
            let stream = self.non_resident_stream(&spec, stale_seed);
            let node = self.node_mut(id);
            let at = node
                .streams
                .iter()
                .position(Stream::is_unnamed_data)
                .expect("just added");
            node.streams[at] = stream;
        }
        self.fit_or_split(id);
        self.ensure_list_allocated(id);
        self.touch_dir(parent, now);
        id
    }

    /// Adds a file of `len` bytes of tagged content whose seed derives from
    /// the volume seed and the record number.
    pub fn add_tagged(&mut self, parent: NodeId, name: impl Into<Name>, len: u64) -> NodeId {
        let seed = derive_n(self.seed, 0x4649_4C45_0000 + self.nodes.len() as u64);
        self.add_file(parent, name, FileSpec::tagged(seed, len))
    }

    fn non_resident_stream(&mut self, spec: &FileSpec, stale_seed: u64) -> Stream {
        let cs = self.cluster_size();
        let len = spec.content.len;
        let need = len.div_ceil(cs) + spec.extra_clusters;
        let compressed = spec.attr_flags & 0x00FF != 0;
        let mut runs: Vec<Run> = match &spec.placement {
            Placement::Auto if compressed => {
                // 16-cluster compression units: some data clusters, then a
                // hole that makes up the unit (notes section 4).
                let mut runs = Vec::new();
                let mut left = need;
                let mut unit = 0u64;
                while left > 0 {
                    let unit_len = left.min(16);
                    if unit_len == 16 {
                        let stored = 3 + derive_n(stale_seed, unit) % 10;
                        runs.push(self.alloc.alloc(stored));
                        runs.push(Run::hole(16 - stored));
                    } else {
                        runs.push(self.alloc.alloc(unit_len));
                    }
                    left -= unit_len;
                    unit += 1;
                }
                runs
            }
            Placement::Auto => {
                if need > 0 {
                    vec![self.alloc.alloc(need)]
                } else {
                    Vec::new()
                }
            }
            Placement::Fragments(pieces) => {
                assert!(need > 0, "an empty file has no fragments");
                self.alloc.alloc_fragmented(need, (*pieces).min(need))
            }
            Placement::Runs(runs) => {
                for run in runs {
                    if let Some(lcn) = run.lcn {
                        if spec.force_runs {
                            self.alloc.mark(lcn, run.len);
                        } else {
                            self.alloc.alloc_at(lcn, run.len);
                        }
                    }
                }
                runs.clone()
            }
        };
        // A sparse stream whose runs stop short is completed with a hole
        // (a placeholder is one hole from start to end).
        let mapped = total_clusters(&runs);
        if mapped < need && spec.attr_flags & crate::ondisk::record::ATTR_SPARSE != 0 {
            runs.push(Run::hole(need - mapped));
        }
        let mut content = spec.content.clone();
        let initialized = spec.initialized.unwrap_or(len).min(len);
        if !spec.garbage_on_disk {
            let mut vcn = 0;
            for run in &runs {
                if run.lcn.is_none() {
                    content = content.with_zero_range(
                        vcn * cs,
                        (run.len * cs).min(len.saturating_sub(vcn * cs)),
                    );
                }
                vcn += run.len;
            }
            if initialized < len {
                content = content.with_zero_range(initialized, len - initialized);
            }
        }
        Stream {
            type_code: crate::ondisk::record::attr_type::DATA,
            name: Vec::new(),
            real_size: len,
            initialized,
            runs: Some(runs),
            flags: spec.attr_flags,
            wide_header: spec.wide_header,
            source: Source::Spec(content),
            garbage: spec.garbage_on_disk,
            stale_seed,
        }
    }

    /// Makes sure the records of `node` can be encoded. A file or folder
    /// whose runlist overflows its record gets a resident attribute list
    /// and extension records the way Windows splits (mapping pairs move to
    /// child records); any other overflow is a mistake of the scenario.
    pub(crate) fn fit_or_split(&mut self, node: NodeId) {
        let Err(overflow) = self.assemble(node, self.trees.get(&node.0)) else {
            return;
        };
        let n = self.node(node);
        assert!(
            n.multi.is_none(),
            "record {} of the file in record {} overflows ({overflow}); adjust its MultiSpec",
            overflow.slot,
            n.record
        );
        let per_piece = ((self.record_size() as usize) - 0x38 - 0x50 - 8) / 9;
        let data_runs = n.data().and_then(|s| s.runs.as_ref()).map_or(0, Vec::len);
        let index_runs = n.dir.as_ref().map_or(0, |d| d.runs.len());
        let mut multi = MultiSpec::new(params::ListForm::Resident);
        let mut holder = 0;
        let split = |runs: usize, holder: &mut usize| -> Vec<Piece> {
            let mut pieces = Vec::new();
            let mut left = runs;
            while left > 0 {
                *holder += 1;
                let take = left.min(per_piece);
                pieces.push(Piece {
                    holder: *holder,
                    runs: take,
                });
                left -= take;
            }
            pieces
        };
        if data_runs > 4 {
            multi.data = split(data_runs, &mut holder);
        }
        if index_runs > 4 {
            multi.index_allocation = split(index_runs, &mut holder);
        }
        assert!(
            holder > 0,
            "the record of the file in record {} overflows ({overflow}) and has no long runlist to move out",
            n.record
        );
        self.node_mut(node).multi = Some(multi);
        self.take_extensions(node, holder, &[]);
        if let Err(overflow) = self.assemble(node, self.trees.get(&node.0)) {
            panic!(
                "record {} still overflows after splitting: {overflow}",
                overflow.slot
            );
        }
    }

    // ------------------------------------------------------- modifications

    /// Adds another name (hard link) for `node` in `parent`.
    pub fn hard_link(
        &mut self,
        node: NodeId,
        parent: NodeId,
        name: impl Into<Name>,
        namespace: u8,
    ) {
        self.assert_dir(parent);
        let now = self.tick();
        let name = name.into();
        assert!(
            (1..=255).contains(&name.0.len()) && namespace <= 3,
            "bad link name"
        );
        self.node_mut(node).names.push(NameLink {
            parent: ParentRef::Node(parent),
            name: name.0,
            namespace,
            times: [now; 4],
            indexed: true,
        });
        self.node_mut(node).times[2] = now;
        self.touch_record(node);
        self.fit_or_split(node);
        self.touch_dir(parent, now);
    }

    /// Renames `node`: its first name gets the new name (a DOS alias, if the
    /// node has one, is dropped).
    pub fn rename(&mut self, node: NodeId, new_name: impl Into<Name>) {
        let now = self.tick();
        let new_name = new_name.into();
        let n = self.node_mut(node);
        n.names.retain(|l| l.namespace != 2);
        let link = n.names.first_mut().expect("the node has a name");
        link.name = new_name.0;
        if link.namespace == 3 {
            link.namespace = 1;
        }
        link.times = [now; 4];
        n.times[2] = now;
        let parent = link.parent;
        self.touch_record(node);
        if let ParentRef::Node(p) = parent {
            self.touch_dir(p, now);
        }
    }

    /// Moves `node` into another folder (every name of it). No check is made
    /// that the result is a tree: moving a folder below one of its own
    /// descendants builds a parent cycle.
    pub fn set_parent(&mut self, node: NodeId, new_parent: NodeId) {
        let now = self.tick();
        let mut old_parents = Vec::new();
        for link in &mut self.node_mut(node).names {
            if let ParentRef::Node(old) = link.parent {
                old_parents.push(old);
            }
            link.parent = ParentRef::Node(new_parent);
        }
        self.node_mut(node).times[2] = now;
        self.touch_record(node);
        old_parents.sort_unstable();
        old_parents.dedup();
        for old in old_parents {
            if old != new_parent && self.node(old).in_use {
                self.touch_dir(old, now);
            }
        }
        self.touch_dir(new_parent, now);
    }

    /// Replaces the parent reference of every name of `node` by a literal
    /// one; the node disappears from its folder's index.
    pub fn set_raw_parent(&mut self, node: NodeId, parent: FileRef) {
        for link in &mut self.node_mut(node).names {
            link.parent = ParentRef::Raw(parent);
            link.indexed = false;
        }
        self.touch_record(node);
    }

    /// Sets the `$STANDARD_INFORMATION` times of `node`.
    pub fn set_times(&mut self, node: NodeId, times: Times) {
        self.node_mut(node).times = times.array();
        self.touch_record(node);
    }

    /// Makes an existing file or folder a reparse point.
    pub fn node_reparse(&mut self, node: NodeId, tag: u32, data: Vec<u8>) {
        let n = self.node_mut(node);
        n.reparse = Some((tag, data));
        n.file_attributes |= 0x400;
        self.touch_record(node);
    }

    /// Sets the `$LogFile` sequence number of the records of `node`.
    pub fn set_lsn(&mut self, node: NodeId, lsn: u64) {
        self.dirty = true;
        self.node_mut(node).lsn = lsn;
    }

    /// The `$LogFile` sequence number of the records of `node`.
    pub fn lsn(&self, node: NodeId) -> u64 {
        self.node(node).lsn
    }

    /// The `$STANDARD_INFORMATION` times of `node`.
    pub fn times(&self, node: NodeId) -> Times {
        let t = self.node(node).times;
        Times {
            created: t[0],
            modified: t[1],
            mft_changed: t[2],
            accessed: t[3],
        }
    }

    /// Deletes a file or an empty folder: the record stays, not in use, with
    /// its sequence number bumped; clusters and the slot become free.
    pub fn delete(&mut self, node: NodeId) {
        assert!(
            self.children(node).is_empty(),
            "the folder is not empty; use delete_tree"
        );
        let now = self.tick();
        let n = self.node(node);
        assert!(n.in_use, "record {} is already deleted", n.record);
        assert!(
            !n.system || n.record >= 24,
            "system files cannot be deleted"
        );
        let mut freed: Vec<Run> = n.list_runs.clone();
        for s in &n.streams {
            freed.extend(s.runs.iter().flatten().copied());
        }
        if let Some(d) = &n.dir {
            freed.extend(d.runs.iter().copied());
        }
        let slots: Vec<(u64, u16)> = std::iter::once((n.record, n.seq))
            .chain(
                n.extensions
                    .iter()
                    .filter(|e| !e.superseded)
                    .map(|e| (e.record, e.seq)),
            )
            .collect();
        let parents: Vec<NodeId> = n
            .names
            .iter()
            .filter_map(|l| match l.parent {
                ParentRef::Node(p) => Some(p),
                ParentRef::Raw(_) => None,
            })
            .collect();
        self.alloc.free_runs(&freed);
        for (record, seq) in slots {
            self.slot_seq.insert(record, next_seq(seq));
        }
        self.trees.remove(&node.0);
        self.node_mut(node).in_use = false;
        self.touch_record(node);
        for p in parents {
            if self.node(p).in_use {
                self.touch_dir(p, now);
            }
        }
    }

    /// Deletes a folder with everything below it, children first.
    pub fn delete_tree(&mut self, node: NodeId) {
        for child in self.children(node) {
            self.delete_tree(child);
        }
        self.delete(node);
    }

    /// Places a hand-made record in slot `number`. With `in_bitmap` the
    /// slot counts as in use for the MFT bitmap.
    pub fn put_raw_record(&mut self, number: u64, model: RecordModel, in_bitmap: bool) {
        assert!(
            number < self.mft_records(),
            "record {number} is outside the MFT"
        );
        assert!(self.slot_is_free(number), "record {number} is in use");
        assert_eq!(
            model.record_size as u64,
            self.record_size(),
            "wrong record size"
        );
        match self.slots.get(&number).copied() {
            Some(Slot::Base(old)) => self.node_mut(old).superseded = true,
            Some(Slot::Ext(old, k)) => self.node_mut(old).extensions[k].superseded = true,
            _ => {}
        }
        self.slots.insert(number, Slot::Raw);
        self.raw.insert(number, RawRecord { model, in_bitmap });
        self.dirty = true;
    }

    /// Sets the bit of `slot` in the MFT bitmap without putting a record
    /// there: the state between "bit set" and "record written" that a crash
    /// leaves, and the only way a bitmap can claim a slot beyond the
    /// initialized part of the `$MFT`.
    pub fn mark_slot_in_bitmap(&mut self, slot: u64) {
        assert!(slot < self.mft_records(), "slot {slot} is outside the MFT");
        self.extra_bitmap_slots.insert(slot);
        self.dirty = true;
    }

    /// A record model as the builder would write it for slot `number`
    /// (a base record, an extension or a raw record), for scenarios that
    /// want to derive a hand-made record from a real one.
    pub fn record_model(&mut self, number: u64) -> RecordModel {
        self.finalize();
        match self.slots.get(&number).copied() {
            Some(Slot::Raw) => self.raw[&number].model.clone(),
            Some(Slot::Base(id)) | Some(Slot::Ext(id, _)) => self
                .assemble(id, self.trees.get(&id.0))
                .unwrap_or_else(|e| panic!("record {} overflows: {e}", e.slot))
                .into_iter()
                .find(|a| a.slot == number)
                .map(|a| a.model)
                .expect("the node owns the slot"),
            None => panic!("record {number} was never used"),
        }
    }

    /// The seed all content of this volume derives from.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// A seed for a file's content derived from the volume seed and `label`.
    pub fn content_seed(&self, label: &str) -> u64 {
        derive(self.seed, label)
    }

    /// The display path of `node` from the root, components joined by `/`.
    pub fn path(&self, node: NodeId) -> String {
        self.path_components(node).join("/")
    }

    /// The names from the root to `node`, following each node's first
    /// non-DOS name. A chain that does not reach the root (cycles, raw
    /// parents) ends where it stops making progress.
    pub fn path_components(&self, node: NodeId) -> Vec<String> {
        let mut parts = Vec::new();
        let mut at = node;
        let mut seen = std::collections::BTreeSet::new();
        while seen.insert(at) {
            let n = self.node(at);
            if n.record == ROOT_RECORD {
                break;
            }
            let Some(link) = n
                .names
                .iter()
                .find(|l| l.namespace != 2)
                .or(n.names.first())
            else {
                break;
            };
            parts.push(String::from_utf16_lossy(&link.name));
            match link.parent {
                ParentRef::Node(p) => at = p,
                ParentRef::Raw(_) => break,
            }
        }
        parts.reverse();
        parts
    }
}

fn bump_usn(usn: u16) -> u16 {
    match usn.wrapping_add(1) {
        0 | 0xFFFF => 1,
        v => v,
    }
}

fn ascii_upper(u: u16) -> u16 {
    if (b'a' as u16..=b'z' as u16).contains(&u) {
        u - 0x20
    } else {
        u
    }
}

fn is_dos_char(u: u16) -> bool {
    u < 0x80 && {
        let c = u as u8;
        c.is_ascii_alphanumeric() || b"!#$%&'()-@^_`{}~".contains(&c)
    }
}

/// `true` when `name` fits the 8.3 scheme (case does not matter: Windows
/// stores `readme.txt` as one Win32 + DOS name).
pub(crate) fn is_dos_name(name: &[u16]) -> bool {
    let dot = b'.' as u16;
    let mut parts = name.split(|&u| u == dot);
    let base = parts.next().unwrap_or(&[]);
    let ext = parts.next();
    if parts.next().is_some() {
        return false;
    }
    (1..=8).contains(&base.len())
        && base.iter().all(|&u| is_dos_char(u))
        && ext.is_none_or(|e| (1..=3).contains(&e.len()) && e.iter().all(|&u| is_dos_char(u)))
}

/// The n-th 8.3 alias candidate of a long name: up to six valid characters
/// of the base, `~n`, and up to three of the extension, upper case.
pub(crate) fn dos_alias_candidate(long: &[u16], n: u32) -> Vec<u16> {
    let dot = b'.' as u16;
    let (base, ext) = match long.iter().rposition(|&u| u == dot) {
        Some(at) if at > 0 => (&long[..at], &long[at + 1..]),
        _ => (long, &long[..0]),
    };
    let clean = |units: &[u16], max: usize| -> Vec<u16> {
        units
            .iter()
            .copied()
            .filter(|&u| is_dos_char(u))
            .map(ascii_upper)
            .take(max)
            .collect()
    };
    let suffix: Vec<u16> = format!("~{n}").encode_utf16().collect();
    let mut alias = clean(base, 8 - suffix.len().min(7));
    if alias.is_empty() {
        alias.push(b'_' as u16);
    }
    alias.extend(suffix);
    let ext = clean(ext, 3);
    if !ext.is_empty() {
        alias.push(dot);
        alias.extend(ext);
    }
    alias
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn dos_names_and_aliases() {
        for valid in [
            "README.TXT",
            "readme.txt",
            "A",
            "12345678.123",
            "$MFT",
            "BOOT.INI",
            "a_b-c~1.x",
        ] {
            assert!(is_dos_name(&u(valid)), "{valid}");
        }
        for invalid in [
            "",
            "123456789",
            "a.b.c",
            "long name.txt",
            "a.html",
            ".hidden",
            "Übung.txt",
            "a+b.txt",
            "name.",
        ] {
            assert!(!is_dos_name(&u(invalid)), "{invalid}");
        }
        assert_eq!(
            dos_alias_candidate(&u("System Volume Information"), 1),
            u("SYSTEM~1")
        );
        assert_eq!(dos_alias_candidate(&u("Program Files"), 2), u("PROGRA~2"));
        assert_eq!(
            dos_alias_candidate(&u("holiday photo.jpeg"), 1),
            u("HOLIDA~1.JPE")
        );
        assert_eq!(dos_alias_candidate(&u("a.b.c"), 1), u("AB~1.C"));
        assert_eq!(dos_alias_candidate(&u("日本語.txt"), 1), u("_~1.TXT"));
        assert_eq!(dos_alias_candidate(&u("longfilename"), 12), u("LONGF~12"));
        for n in [1, 9, 10, 99, 100, 9999] {
            assert!(is_dos_name(&dos_alias_candidate(
                &u("Some long name.extension"),
                n
            )));
        }
    }

    #[test]
    fn usn_never_becomes_zero_or_all_ones() {
        assert_eq!(bump_usn(1), 2);
        assert_eq!(bump_usn(0xFFFE), 1);
        assert_eq!(bump_usn(0xFFFF), 1);
    }
}
