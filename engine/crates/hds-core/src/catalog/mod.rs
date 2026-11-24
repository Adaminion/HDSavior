//! The catalog (ALGORITHMS E): one tree per instance, with node ids, names,
//! parents, lost and virtual folders; its health (ALGORITHMS G,
//! [`crate::health`]) and its queries ([`crate::query`]).
//!
//! # Build
//!
//! ```text
//! let layout = analyze(&input, &mut heads, &overrides, &cancel)?;      // C, D, F
//! let catalog = Catalog::build(&CatalogInput::new(store, &layout, &heads, &[]))?;
//! ```
//!
//! [`Catalog::build`] reads nothing from the source: it works from the
//! scan store, the [`Layout`] and the stored heads (signature flags, the
//! sniffed kind of nameless orphans). The same inputs give the same
//! catalog, node ids included.
//!
//! * E1/E2 (module `groups`): base and extension records, merged `$DATA`
//!   extents, sizes and the flags the records alone decide.
//! * E3-E9 (module `tree`): directories, names, parents, lost folders (E6,
//!   module `lost`), orphans, cycles and system files.
//! * E10 aggregates and G1-G3 ([`crate::health`]).
//! * E11: node ids are dense, assigned in depth-first preorder, instance by
//!   instance (`main` first): the subtree of a node is the id range
//!   `[id, end)`, which makes search and selection a scan of a range.
//!
//! # Immutability and epochs
//!
//! A [`Catalog`] never changes once built: the engine numbers it with an
//! epoch, shares it (`Arc`), and a query against an old epoch is refused
//! by the engine before it reaches the catalog. What changes inside an
//! epoch produces a *new* `Catalog` value that keeps every node id:
//!
//! * `geometry_set` ([`Catalog::with_health`]): the same tree, health
//!   recomputed under the layout's new geometry (F7: "the tree does not
//!   change"); carved nodes are kept or discarded by the caller (I5).
//! * carving ([`Catalog::with_carved`]): the carved section is replaced;
//!   carved ids continue after every id used before in this epoch, so an
//!   id of a discarded carved node answers `not_found` and is never
//!   reused (E11).
//!
//! What reads add to a file's flags (`signature_ok` / `signature_mismatch`
//! from `read_file` and recovery, `bad_sectors`, and `overwritten` from
//! the refreshed snapshot of H1) goes to an overlay shared by those
//! values ([`Catalog::observe`]); ratings follow it.
//!
//! `instance_set` starts a new epoch: [`instance_set`] re-runs D8/D9 from
//! stored state and returns a fresh catalog.
//!
//! # Carved nodes (extension point)
//!
//! Carving (ALGORITHMS I, [`crate::carve`]) hands its results over as
//! [`CarvedEntry`] values (offset, length, extension, verified end):
//! [`Catalog::with_carved`] places them as
//! `[Carved files]/<ext>/f<sector, 10 digits>.<ext>` under the main
//! instance, rates them (I3) and applies I4. They are listed, searched,
//! selected and recovered like any file (one run).

#![deny(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_used
)]

pub(crate) mod groups;
pub(crate) mod lost;
pub(crate) mod tree;

#[cfg(test)]
pub(crate) mod tests;

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use hds_recover::{Flag, FlagSet};
use serde::Serialize;

use crate::geometry::{ClusterSource, Geometry, Heads, OffsetSource};
use crate::health::Health;
use crate::instances::{AnalysisError, InstanceId, InstanceKind, KindChoice, Layout, Overrides};
use crate::query::QueryCache;
use crate::session::{Session, SessionError};
use crate::store::{EntryId, RecordId, ScanStore};

pub(crate) const NONE: u32 = u32::MAX;

/// Name of the virtual folder of nameless lost folders.
pub const LOST_FOLDERS: &str = "[Lost folders]";
/// Name of the virtual folder of orphaned files.
pub const ORPHANED_FILES: &str = "[Orphaned files]";
/// Name of the virtual folder of carved files.
pub const CARVED_FILES: &str = "[Carved files]";

/// A node of a catalog: valid for the catalog's epoch only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct NodeId(pub u32);

impl NodeId {
    /// As an index.
    pub fn index(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }

    /// The wire form (`u64`).
    pub fn wire(self) -> u64 {
        u64::from(self.0)
    }

    /// The node of a wire id; `None` when it cannot name a node.
    pub fn from_wire(id: u64) -> Option<NodeId> {
        u32::try_from(id).ok().filter(|&v| v != NONE).map(NodeId)
    }
}

/// Kind of a node (PROTOCOL `Row.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A directory with a record.
    Dir,
    /// A file with a record.
    File,
    /// A lost folder (E6).
    LostDir,
    /// An instance root or a virtual folder.
    Virtual,
    /// A carved file (I).
    Carved,
}

impl NodeKind {
    /// The wire name.
    pub fn name(self) -> &'static str {
        match self {
            NodeKind::Dir => "dir",
            NodeKind::File => "file",
            NodeKind::LostDir => "lost_dir",
            NodeKind::Virtual => "virtual",
            NodeKind::Carved => "carved",
        }
    }

    /// Whether nodes of this kind hold children.
    pub fn is_folder(self) -> bool {
        matches!(self, NodeKind::Dir | NodeKind::LostDir | NodeKind::Virtual)
    }

    /// Whether nodes of this kind are files (`file`, `carved`).
    pub fn is_file(self) -> bool {
        !self.is_folder()
    }
}

/// Where a node's name comes from (names are not copied out of the store).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameSrc {
    /// The `idx`-th `$FILE_NAME` of record `rec`.
    Record(RecordId, u16),
    /// An index entry (a lost folder named by E6 a).
    Entry(EntryId),
    /// A text of the catalog (labels, virtual folders, `Folder #P`).
    Text(u32),
}

impl NameSrc {
    pub(crate) fn encode(self) -> u64 {
        match self {
            NameSrc::Record(r, i) => (u64::from(r.0) << 16) | u64::from(i),
            NameSrc::Entry(e) => (1 << 62) | u64::from(e.0),
            NameSrc::Text(t) => (2 << 62) | u64::from(t),
        }
    }

    pub(crate) fn decode(v: u64) -> NameSrc {
        let low = u32::try_from(v & 0xFFFF_FFFF).unwrap_or(0);
        match v >> 62 {
            0 => NameSrc::Record(
                RecordId(u32::try_from((v >> 16) & 0xFFFF_FFFF).unwrap_or(0)),
                u16::try_from(v & 0xFFFF).unwrap_or(0),
            ),
            1 => NameSrc::Entry(EntryId(low)),
            _ => NameSrc::Text(low),
        }
    }
}

/// Node bits.
pub(crate) mod bits {
    /// The file or folder was deleted (its base record is not in use).
    pub const DELETED: u8 = 1;
}

/// One node as stored.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NodeRec {
    /// Parent node ([`NONE`] for an instance root).
    pub parent: u32,
    /// One past the last node of its subtree (preorder).
    pub end: u32,
    /// Base record (or first record of an orphan group); [`NONE`] for
    /// virtual, lost and carved nodes.
    pub rec: u32,
    /// Offset of the folded name in the search arena.
    pub fold: u32,
    /// Name source ([`NameSrc::encode`]).
    pub name: u64,
    /// Catalog instance index.
    pub inst: u16,
    /// Kind.
    pub kind: NodeKind,
    /// [`bits`].
    pub bits: u8,
}

impl NodeRec {
    pub(crate) fn deleted(&self) -> bool {
        self.bits & bits::DELETED != 0
    }
}

/// One instance as the catalog reports it (PROTOCOL `Instance`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogInstance {
    /// Its position in [`Catalog::instances`] (the protocol's instance id;
    /// 0 is `main` when there is one).
    pub id: u32,
    /// The instance of the [`Layout`].
    pub layout_id: InstanceId,
    /// Its kind.
    pub kind: InstanceKind,
    /// The kind was set by the user.
    pub manual: bool,
    /// Display name of its root (`""` for main).
    pub label: String,
    /// Its root node.
    pub root: NodeId,
    /// Records that belong to it.
    pub records: u64,
    /// The FILETIME range of its timed MFT-change times.
    pub times: Option<(u64, u64)>,
    /// Display path of the file that holds it (embedded instances).
    pub embedded_in: Option<String>,
    /// The geometry in effect; for an instance without one, the fallback
    /// the catalog maps its runs with ([`fallback_geometry`]).
    pub geometry: Geometry,
    /// The candidates of its own geometry (F3).
    pub candidates: Vec<Geometry>,
    /// `[Lost folders]`.
    pub lost: Option<NodeId>,
    /// `[Orphaned files]`.
    pub orphans: Option<NodeId>,
    /// `[Carved files]` (main only, once carving found something).
    pub carved: Option<NodeId>,
}

/// The geometry used for an instance that has none (no own evidence and no
/// main to inherit from): 4096-byte clusters at the range start, not
/// validated, so every file of it is rated at most `uncertain`.
pub fn fallback_geometry(range: &Range<u64>) -> Geometry {
    Geometry {
        cluster_size: 4096,
        volume_offset: range.start,
        cluster_source: ClusterSource::Default,
        offset_source: OffsetSource::Default,
        index_dirs: 0,
        index_blocks: 0,
        mft_agree: 0,
        signature_votes: 0,
        signature_checked: 0,
        signature_ok: 0,
        validated: false,
        total_clusters: 0,
        end_by_files: 0,
        end_of_all: 0,
        lost_volume: range.start..range.start,
    }
}

/// A lost folder's key (E6): instance and parent reference `(P, s)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LostKey {
    /// Catalog instance index.
    pub instance: u32,
    /// Record number `P`.
    pub number: u64,
    /// Sequence `s`.
    pub sequence: u16,
}

/// The static part of a catalog: nodes, names, groups.
#[derive(Debug, Default)]
pub(crate) struct Tree {
    pub nodes: Vec<NodeRec>,
    /// Folded names, each followed by `\0`.
    pub fold: String,
    /// Texts of [`NameSrc::Text`].
    pub texts: Vec<String>,
    /// Extension records of groups: (node, record), sorted.
    pub exts: Vec<(u32, RecordId)>,
    /// Lost folders: (node, key), sorted by node.
    pub lost: Vec<(u32, LostKey)>,
    /// Catalog instance index of each layout instance (by layout index).
    pub inst_of_layout: Vec<u32>,
    /// Session range.
    pub range: Range<u64>,
    /// Node of each base record (by record index; [`NONE`] if none).
    pub node_of_rec: Vec<u32>,
    /// Base records whose header has no number (pre-3.1 headers) and
    /// their positional number (D1): (record index, number), sorted.
    pub positional: Vec<(u32, u32)>,
}

impl Tree {
    /// Fills [`Tree::node_of_rec`].
    pub(crate) fn index_records(&mut self, records: usize) {
        self.node_of_rec = vec![NONE; records];
        for (i, n) in self.nodes.iter().enumerate() {
            if n.rec == NONE {
                continue;
            }
            if let Some(slot) = self
                .node_of_rec
                .get_mut(usize::try_from(n.rec).unwrap_or(usize::MAX))
            {
                *slot = u32::try_from(i).unwrap_or(NONE);
            }
        }
    }

    /// The effective number of a node's base record (D1): the number in
    /// its header, else its positional number; `None` when neither is
    /// known (an unnumbered stray).
    pub(crate) fn record_number(&self, store: &ScanStore, r: RecordId) -> Option<u32> {
        if let Some(n) = store.record(r).and_then(|x| x.record_number()) {
            return Some(n);
        }
        let k = self.positional.partition_point(|&(x, _)| x < r.0);
        self.positional
            .get(k)
            .filter(|&&(x, _)| x == r.0)
            .map(|&(_, n)| n)
    }

    /// The node of a base record.
    pub(crate) fn node_of_record(&self, r: RecordId) -> Option<u32> {
        self.node_of_rec
            .get(r.index())
            .copied()
            .filter(|&n| n != NONE)
    }

    /// The display name of a node.
    pub(crate) fn display_name(&self, store: &ScanStore, node: &NodeRec) -> String {
        tree::display_name(store, &self.texts, NameSrc::decode(node.name))
    }

    /// The display path of a node: the names from its root down, `/`
    /// separated, empty names (the main root) left out.
    pub(crate) fn path(&self, store: &ScanStore, node: u32) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut x = node;
        let mut guard = 0usize;
        while let Some(n) = self.node(x) {
            let name = self.display_name(store, n);
            if !name.is_empty() {
                parts.push(name);
            }
            x = n.parent;
            guard = guard.saturating_add(1);
            if guard > self.nodes.len() {
                break;
            }
        }
        parts.reverse();
        parts.join("/")
    }

    pub(crate) fn node(&self, id: u32) -> Option<&NodeRec> {
        self.nodes.get(usize::try_from(id).ok()?)
    }

    pub(crate) fn len(&self) -> u32 {
        u32::try_from(self.nodes.len()).unwrap_or(NONE)
    }

    /// The extension records of a node's group.
    pub(crate) fn exts_of(&self, node: u32) -> Vec<RecordId> {
        let i = self.exts.partition_point(|&(n, _)| n < node);
        self.exts
            .get(i..)
            .unwrap_or(&[])
            .iter()
            .take_while(|&&(n, _)| n == node)
            .map(|&(_, r)| r)
            .collect()
    }

    /// The lost-folder key of a node.
    pub(crate) fn lost_key(&self, node: u32) -> Option<LostKey> {
        let i = self.lost.binary_search_by_key(&node, |&(n, _)| n).ok()?;
        self.lost.get(i).map(|&(_, k)| k)
    }

    /// The folded name of a node.
    pub(crate) fn folded(&self, node: &NodeRec) -> &str {
        let start = usize::try_from(node.fold).unwrap_or(usize::MAX);
        let rest = self.fold.get(start..).unwrap_or("");
        rest.split('\0').next().unwrap_or("")
    }

    /// The folded name of node `id` in O(1): names are stored in node
    /// order, each followed by `\0`, so a name ends one byte before the
    /// next node's starts.
    pub(crate) fn folded_at(&self, id: u32) -> &str {
        let i = usize::try_from(id).unwrap_or(usize::MAX);
        let Some(n) = self.nodes.get(i) else {
            return "";
        };
        let a = usize::try_from(n.fold).unwrap_or(usize::MAX);
        let b = self
            .nodes
            .get(i.saturating_add(1))
            .map_or(self.fold.len(), |m| usize::try_from(m.fold).unwrap_or(0));
        self.fold.get(a..b.saturating_sub(1).max(a)).unwrap_or("")
    }
}

/// A carved file handed to the catalog (I3). The carving step converts its
/// own results to these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarvedEntry {
    /// Absolute offset of the first byte.
    pub offset: u64,
    /// Length in bytes.
    pub len: u64,
    /// Extension (lower case, no dot).
    pub ext: String,
    /// The end was verified by the format's structure.
    pub end_verified: bool,
}

impl CarvedEntry {
    /// The node name: `f<sector number, 10 digits>.<ext>`.
    pub fn name(&self) -> String {
        format!("f{:010}.{}", self.offset / 512, self.ext)
    }
}

/// The carved section: `[Carved files]`, one folder per extension, the
/// files; ids `[base, base + nodes.len())`.
#[derive(Debug, Default)]
pub(crate) struct CarvedSection {
    pub base: u32,
    pub nodes: Vec<NodeRec>,
    /// Per node: the entry (files) or `None` (folders).
    pub entries: Vec<Option<CarvedEntry>>,
    pub texts: Vec<String>,
    pub fold: String,
    /// Per node: flags, size and aggregates (as in [`Health`]).
    pub flags: Vec<u32>,
    pub size: Vec<u64>,
    pub files: Vec<u32>,
    pub deleted_files: Vec<u32>,
    pub deleted_size: Vec<u64>,
    /// Deleted files of the tree that carving makes `overwritten` (I4).
    pub overwrites: Vec<u32>,
}

/// Flags that reads added after the catalog was built.
#[derive(Debug, Default)]
pub(crate) struct Observed {
    pub map: Mutex<(u64, HashMap<u32, u32>)>,
}

/// Why a catalog could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// More nodes than the id space holds.
    TooManyNodes,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::TooManyNodes => {
                f.write_str("the catalog would hold more than 2^32 - 2 nodes")
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// What a catalog is built from.
#[derive(Clone, Copy)]
pub struct CatalogInput<'a> {
    /// The scan store.
    pub store: &'a ScanStore,
    /// C, D and F.
    pub layout: &'a Layout,
    /// The stored heads (nothing is read: [`Heads::stored`] only).
    pub heads: &'a Heads,
    /// Carved files (I), placed under the main instance.
    pub carved: &'a [CarvedEntry],
}

impl<'a> CatalogInput<'a> {
    /// The input of a catalog.
    pub fn new(
        store: &'a ScanStore,
        layout: &'a Layout,
        heads: &'a Heads,
        carved: &'a [CarvedEntry],
    ) -> CatalogInput<'a> {
        CatalogInput {
            store,
            layout,
            heads,
            carved,
        }
    }
}

/// A catalog: the trees of all instances with their health. Immutable;
/// see the module documentation.
pub struct Catalog {
    pub(crate) tree: Arc<Tree>,
    pub(crate) instances: Arc<Vec<CatalogInstance>>,
    pub(crate) health: Arc<Health>,
    pub(crate) carved: Arc<CarvedSection>,
    pub(crate) observed: Arc<Observed>,
    pub(crate) cache: Arc<QueryCache>,
    /// The next id a carved node may take (never reused in an epoch).
    pub(crate) next_id: u32,
}

impl fmt::Debug for Catalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Catalog")
            .field("nodes", &self.node_count())
            .field("instances", &self.instances.len())
            .finish()
    }
}

impl Catalog {
    /// Builds the catalog of a layout (E1-E11, G1-G3).
    pub fn build(input: &CatalogInput<'_>) -> Result<Catalog, BuildError> {
        let (tree, instances) = tree::build(input)?;
        let tree = Arc::new(tree);
        let health = Arc::new(Health::compute(
            input.store,
            input.layout,
            &tree,
            &instances,
            input.heads,
        ));
        let next_id = tree.len();
        let mut catalog = Catalog {
            tree,
            instances: Arc::new(instances),
            health,
            carved: Arc::new(CarvedSection {
                base: next_id,
                ..CarvedSection::default()
            }),
            observed: Arc::new(Observed::default()),
            cache: Arc::new(QueryCache::default()),
            next_id,
        };
        if !input.carved.is_empty() {
            catalog = catalog.with_carved(input.store, input.carved)?;
        }
        Ok(catalog)
    }

    /// The same tree with health recomputed under `layout`'s geometries
    /// (`geometry_set`, F7); carved nodes are kept (the caller discards
    /// them with [`Catalog::with_carved`] and an empty list when the main
    /// geometry changed, I5). The layout must be the one the tree was
    /// built from, re-applied (same instances).
    pub fn with_health(&self, store: &ScanStore, layout: &Layout, heads: &Heads) -> Catalog {
        let mut instances = (*self.instances).clone();
        for ci in &mut instances {
            if let Some(li) = layout.instance(ci.layout_id) {
                ci.geometry = li
                    .geometry
                    .clone()
                    .unwrap_or_else(|| fallback_geometry(layout.range()));
                ci.candidates = li.candidates.clone();
            }
        }
        let health = Arc::new(Health::compute(
            store, layout, &self.tree, &instances, heads,
        ));
        let carved =
            crate::health::rehealth_carved(&self.carved, &self.tree, &instances, &health, store);
        Catalog {
            tree: self.tree.clone(),
            instances: Arc::new(instances),
            health,
            carved: Arc::new(carved),
            observed: Arc::new(Observed::default()),
            cache: Arc::new(QueryCache::default()),
            next_id: self.next_id,
        }
    }

    /// The same catalog with its carved files replaced by `entries`
    /// (`carve_start`, I5). Ids of the new carved nodes follow every id
    /// used before in this epoch.
    pub fn with_carved(
        &self,
        store: &ScanStore,
        entries: &[CarvedEntry],
    ) -> Result<Catalog, BuildError> {
        let base = self.next_id;
        let carved = crate::health::carved_section(
            &self.tree,
            &self.instances,
            &self.health,
            store,
            base,
            entries,
        )?;
        let used = u32::try_from(carved.nodes.len()).map_err(|_| BuildError::TooManyNodes)?;
        let next_id = base
            .checked_add(used)
            .filter(|&n| n != NONE)
            .ok_or(BuildError::TooManyNodes)?;
        let root = (!carved.nodes.is_empty()).then_some(NodeId(base));
        let mut instances = (*self.instances).clone();
        for ci in &mut instances {
            ci.carved = if ci.kind == InstanceKind::Main {
                root
            } else {
                None
            };
        }
        Ok(Catalog {
            tree: self.tree.clone(),
            instances: Arc::new(instances),
            health: self.health.clone(),
            carved: Arc::new(carved),
            observed: self.observed.clone(),
            cache: Arc::new(QueryCache::default()),
            next_id,
        })
    }

    /// The instances, `main` first.
    pub fn instances(&self) -> &[CatalogInstance] {
        &self.instances
    }

    /// The catalog instance of a layout instance.
    pub fn instance_of_layout(&self, id: InstanceId) -> Option<&CatalogInstance> {
        let i = *self.tree.inst_of_layout.get(id.index())?;
        self.instances.get(usize::try_from(i).ok()?)
    }

    /// Number of nodes (all roots, carved nodes included).
    pub fn node_count(&self) -> u64 {
        u64::from(self.tree.len())
            .saturating_add(u64::try_from(self.carved.nodes.len()).unwrap_or(0))
    }

    /// Every node id of this catalog in ascending order: the tree's, then
    /// the carved section's (which follows any carved ids discarded
    /// earlier in the epoch, so the ids are not always contiguous).
    pub fn node_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        let carved = u32::try_from(self.carved.nodes.len()).unwrap_or(0);
        (0..self.tree.len())
            .chain(self.carved.base..self.carved.base.saturating_add(carved))
            .map(NodeId)
    }

    /// Whether `id` names a node of this catalog.
    pub fn contains(&self, id: NodeId) -> bool {
        self.rec(id.0).is_some()
    }

    /// The carved entries currently in the catalog.
    pub fn carved_entries(&self) -> Vec<CarvedEntry> {
        self.carved.entries.iter().flatten().cloned().collect()
    }

    /// Records flags that a read found (`signature_ok` or
    /// `signature_mismatch`, `bad_sectors`, `overwritten`). A signature
    /// flag replaces the other one. Ignored for folders and unknown ids.
    pub fn observe(&self, node: NodeId, flags: FlagSet) {
        let keep = FlagSet::of(&[
            Flag::SignatureOk,
            Flag::SignatureMismatch,
            Flag::BadSectors,
            Flag::Overwritten,
        ]);
        let add = flags.intersection(keep);
        if add.is_empty() || !self.rec(node.0).is_some_and(|n| n.kind.is_file()) {
            return;
        }
        if let Ok(mut g) = self.observed.map.lock() {
            let (gen, map) = &mut *g;
            let slot = map.entry(node.0).or_insert(0);
            let mut cur = FlagSet::from_bits(*slot).unwrap_or(FlagSet::EMPTY);
            if add.contains(Flag::SignatureOk) || add.contains(Flag::SignatureMismatch) {
                cur = cur
                    .without(Flag::SignatureOk)
                    .without(Flag::SignatureMismatch);
                cur.insert(if add.contains(Flag::SignatureMismatch) {
                    Flag::SignatureMismatch
                } else {
                    Flag::SignatureOk
                });
            }
            cur = cur.union(
                add.without(Flag::SignatureOk)
                    .without(Flag::SignatureMismatch),
            );
            if cur.bits() != *slot {
                *slot = cur.bits();
                *gen = gen.saturating_add(1);
            }
        }
    }

    /// H1: marks `overwritten` every file whose data intersects `set` (the
    /// overwritten set of a fresh snapshot). Returns how many files were
    /// newly marked.
    pub fn refresh_overwritten(&self, set: &hds_io::RangeSet) -> u64 {
        let mut n = 0u64;
        for node in self.health.files_intersecting(set) {
            let before = self.flags(NodeId(node));
            if !before.contains(Flag::Overwritten) {
                self.observe(NodeId(node), FlagSet::of(&[Flag::Overwritten]));
                n = n.saturating_add(1);
            }
        }
        n
    }

    /// The node record of an id (static or carved).
    pub(crate) fn rec(&self, id: u32) -> Option<&NodeRec> {
        if id < self.tree.len() {
            return self.tree.node(id);
        }
        let k = id.checked_sub(self.carved.base)?;
        self.carved.nodes.get(usize::try_from(k).ok()?)
    }

    /// The observation generation (changes whenever a read adds a flag).
    pub(crate) fn observed_gen(&self) -> u64 {
        self.observed.map.lock().map(|g| g.0).unwrap_or(0)
    }

    /// The flags of a node: what the catalog knows plus what reads found.
    pub fn flags(&self, id: NodeId) -> FlagSet {
        let base = self.base_flags(id.0);
        let extra = self
            .observed
            .map
            .lock()
            .ok()
            .and_then(|g| g.1.get(&id.0).copied())
            .and_then(FlagSet::from_bits)
            .unwrap_or(FlagSet::EMPTY);
        if extra.is_empty() {
            return base;
        }
        let mut f = base;
        if extra.contains(Flag::SignatureOk) || extra.contains(Flag::SignatureMismatch) {
            f = f
                .without(Flag::SignatureOk)
                .without(Flag::SignatureMismatch);
        }
        f.union(extra)
    }

    pub(crate) fn base_flags(&self, id: u32) -> FlagSet {
        let bits = if id < self.tree.len() {
            self.health
                .flags
                .get(usize::try_from(id).unwrap_or(usize::MAX))
                .copied()
        } else {
            id.checked_sub(self.carved.base)
                .and_then(|k| self.carved.flags.get(usize::try_from(k).ok()?).copied())
        };
        let f = bits.and_then(FlagSet::from_bits).unwrap_or(FlagSet::EMPTY);
        // I4: a carved file inside a deleted file's extents.
        if self.carved.overwrites.binary_search(&id).is_ok() {
            return f.with(Flag::Overwritten);
        }
        f
    }

    /// The session range.
    pub fn range(&self) -> &Range<u64> {
        &self.tree.range
    }

    /// The lost-folder key of a lost folder.
    pub fn lost_key(&self, id: NodeId) -> Option<LostKey> {
        self.tree.lost_key(id.0)
    }

    /// The base record of a node (files and directories with a record).
    pub fn record_of(&self, id: NodeId) -> Option<RecordId> {
        self.rec(id.0)
            .map(|n| n.rec)
            .filter(|&r| r != NONE)
            .map(RecordId)
    }

    /// The node of a base record (files and directories; `None` for
    /// records without a node: system files, records below `$Extend`,
    /// extension records, current-volume records).
    pub fn node_of_record(&self, r: RecordId) -> Option<NodeId> {
        self.tree.node_of_record(r).map(NodeId)
    }

    /// Whether a lost folder got a real name (E6) rather than `Folder #P`.
    pub fn lost_folder_named(&self, id: NodeId) -> bool {
        self.rec(id.0).is_some_and(|n| {
            n.kind == NodeKind::LostDir && !matches!(NameSrc::decode(n.name), NameSrc::Text(_))
        })
    }

    /// The parent of a node (`None` for a root).
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.rec(id.0)
            .map(|n| n.parent)
            .filter(|&p| p != NONE)
            .map(NodeId)
    }

    /// The kind of a node.
    pub fn kind(&self, id: NodeId) -> Option<NodeKind> {
        self.rec(id.0).map(|n| n.kind)
    }

    /// The catalog instance of a node.
    pub fn instance_index(&self, id: NodeId) -> Option<u32> {
        self.rec(id.0).map(|n| u32::from(n.inst))
    }
}

/// `instance_set` (PROTOCOL, ALGORITHMS D8, I5): sets the kind of the
/// catalog instance `instance` (`None` = `auto`), stores the overrides in
/// the session (`OVERRIDES`), re-runs D8, D9, labels, F4 and S2 from stored
/// state ([`Layout::apply`]) and builds the new catalog (a new epoch).
///
/// Carved nodes are discarded (I5): the new catalog has none, and the
/// caller must also drop them from the session
/// ([`crate::carve::discard_carved`]) before it saves.
///
/// Fails with [`InstanceSetError::Strays`] for the strays pseudo-instance
/// and [`InstanceSetError::NotFound`] for an unknown id. On any error the
/// session and `overrides` are unchanged and `layout` is put back as it
/// was (except after [`InstanceSetError::Restore`]); `heads` may keep
/// heads read meanwhile (they are true reads of the source).
#[allow(clippy::too_many_arguments)]
pub fn instance_set(
    session: &mut Session,
    layout: &mut Layout,
    overrides: &mut Overrides,
    heads: &mut Heads,
    catalog: &Catalog,
    instance: u32,
    choice: Option<KindChoice>,
    cancel: &AtomicBool,
) -> Result<Catalog, InstanceSetError> {
    let ci = catalog
        .instances()
        .get(usize::try_from(instance).unwrap_or(usize::MAX))
        .ok_or(InstanceSetError::NotFound)?;
    if ci.kind == InstanceKind::Strays {
        return Err(InstanceSetError::Strays);
    }
    let mut next = overrides.clone();
    next.set_kind(session.store(), layout, ci.layout_id, choice);
    apply_or_restore(session.store(), layout, overrides, &next, heads, cancel)?;
    let built = Catalog::build(&CatalogInput::new(session.store(), layout, heads, &[]))
        .map_err(InstanceSetError::Build);
    let catalog = match built {
        Ok(c) => c,
        Err(e) => {
            restore(session.store(), layout, overrides, heads)?;
            return Err(e);
        }
    };
    commit(session, layout, overrides, next, heads)?;
    Ok(catalog)
}

/// `geometry_set` (PROTOCOL, ALGORITHMS F7): stores a manual geometry for
/// the catalog instance `instance`, re-applies the layout and recomputes
/// health on the same tree (same epoch). Returns the new catalog and
/// whether the main instance's geometry changed (then the caller discards
/// carved nodes, I5: [`Catalog::with_carved`] with no entries and
/// [`crate::carve::discard_carved`]).
///
/// Fails with [`InstanceSetError::Strays`] for the strays pseudo-instance
/// (its geometry is always `main`'s, F4; PROTOCOL: `bad_request`) and
/// [`InstanceSetError::NotFound`] for an unknown id. On any error the
/// session, `overrides` and `layout` are as before, as for
/// [`instance_set`].
#[allow(clippy::too_many_arguments)]
pub fn geometry_set(
    session: &mut Session,
    layout: &mut Layout,
    overrides: &mut Overrides,
    heads: &mut Heads,
    catalog: &Catalog,
    instance: u32,
    geometry: (u32, u64),
    cancel: &AtomicBool,
) -> Result<(Catalog, bool), InstanceSetError> {
    let ci = catalog
        .instances()
        .get(usize::try_from(instance).unwrap_or(usize::MAX))
        .ok_or(InstanceSetError::NotFound)?;
    if ci.kind == InstanceKind::Strays {
        return Err(InstanceSetError::Strays);
    }
    let is_main = ci.kind == InstanceKind::Main;
    let mut next = overrides.clone();
    next.set_geometry(session.store(), layout, ci.layout_id, Some(geometry));
    apply_or_restore(session.store(), layout, overrides, &next, heads, cancel)?;
    commit(session, layout, overrides, next, heads)?;
    Ok((catalog.with_health(session.store(), layout, heads), is_main))
}

/// Runs [`Layout::apply`] with `next`. When it fails (cancelled, or the
/// source lost while heads are read for a manual geometry), `apply` may
/// already have rewritten kinds, labels and `main`: the layout is put back
/// by applying `current` again ([`restore`]) and the error is returned.
///
/// The restore reads nothing in practice: the heads `current` needs were
/// read when it was applied and [`Heads`] keeps every head it read. Only
/// if a needed head is unknown and the source is gone does the restore
/// fail too; then the error says so ([`InstanceSetError::Restore`]) and
/// the layout must be rebuilt from the session (`session_load`).
fn apply_or_restore(
    store: &ScanStore,
    layout: &mut Layout,
    current: &Overrides,
    next: &Overrides,
    heads: &mut Heads,
    cancel: &AtomicBool,
) -> Result<(), InstanceSetError> {
    match layout.apply(store, next, heads, cancel) {
        Ok(()) => Ok(()),
        Err(e) => {
            restore(store, layout, current, heads)?;
            Err(InstanceSetError::Analysis(e))
        }
    }
}

/// Applies `current` again, ignoring the caller's cancel flag (the
/// restore must finish).
fn restore(
    store: &ScanStore,
    layout: &mut Layout,
    current: &Overrides,
    heads: &mut Heads,
) -> Result<(), InstanceSetError> {
    layout
        .apply(store, current, heads, &AtomicBool::new(false))
        .map_err(InstanceSetError::Restore)
}

/// Stores `next` in the session (`OVERRIDES`) and in the caller's
/// overrides once the new layout is in place; if the session cannot store
/// it, the layout is restored to `overrides` and the error returned.
fn commit(
    session: &mut Session,
    layout: &mut Layout,
    overrides: &mut Overrides,
    next: Overrides,
    heads: &mut Heads,
) -> Result<(), InstanceSetError> {
    if let Err(e) = next.save(session) {
        restore(session.store(), layout, overrides, heads)?;
        return Err(InstanceSetError::Session(e));
    }
    *overrides = next;
    Ok(())
}

/// Why `instance_set` or `geometry_set` failed.
#[derive(Debug)]
pub enum InstanceSetError {
    /// No such instance (`not_found`).
    NotFound,
    /// The strays pseudo-instance cannot be changed (`bad_request`).
    Strays,
    /// The session could not store the override (`io_error`).
    Session(SessionError),
    /// Heads could not be read for a manual geometry.
    Analysis(AnalysisError),
    /// The catalog could not be built.
    Build(BuildError),
    /// The command failed and the layout could not be put back either
    /// (a head the previous overrides need is unknown and the source is
    /// gone): the layout is inconsistent and must be rebuilt from the
    /// session (`session_load`). The session and the caller's overrides
    /// are unchanged.
    Restore(AnalysisError),
}

impl fmt::Display for InstanceSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstanceSetError::NotFound => f.write_str("no such instance"),
            InstanceSetError::Strays => f.write_str("the stray records cannot be changed"),
            InstanceSetError::Session(e) => write!(f, "the session could not be updated: {e}"),
            InstanceSetError::Analysis(e) => write!(f, "{e}"),
            InstanceSetError::Build(e) => write!(f, "{e}"),
            InstanceSetError::Restore(e) => {
                write!(f, "the previous state could not be restored: {e}")
            }
        }
    }
}

impl std::error::Error for InstanceSetError {}
