//! Catalog queries: what the catalog commands of PROTOCOL.md answer
//! (`list_children`, `node_info`, `search`, `selection_summary`,
//! `catalog_info`, `check_destination`'s `lost_files` / `lost_bytes`) and
//! the recovery plans of ALGORITHMS H2 ([`Catalog::plan`]).
//!
//! The types mirror the protocol objects member by member (`Row`,
//! `NodeInfo`, `SearchRow`, `SelectionSummary`, `CatalogInfo`): the
//! engine converts them field by field. Flags and ratings are
//! `hds_recover`'s [`Flag`] / [`Rating`], whose wire names are the
//! protocol's.
//!
//! # Cost
//!
//! * Result lists (the filtered children of a folder, the matches of a
//!   search) are cached per (node, sort, filters), a few entries, least
//!   recently used first out. A list of up to 8192 entries is sorted whole
//!   on first use; a longer one only as far as a page reaches: the first
//!   1024 by a bounded heap (each sort key computed once), then a sorted
//!   prefix that at least doubles each time it is extended. Further pages
//!   cost `O(limit)` on average. Measured on ten million records (release
//!   build, this VM): a folder of 100 000 entries lists its first page in
//!   about 6 ms and the next ones in under 1 ms.
//! * `search`: node ids are a preorder, so the subtree of `under` is one
//!   id range and its folded names one contiguous stretch of the name
//!   arena: one substring search over that stretch finds the candidates
//!   (ten million names: about 70 ms). A query that matches nearly
//!   everything costs about a second for its first page.
//! * Paths and ancestor lists are bounded to [`MAX_PATH_DEPTH`] levels
//!   (hostile parent chains can be millions deep); the root is always
//!   named first, elided levels are shown as `…`.

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

mod info;
mod select;
#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::fmt;
use std::sync::{Arc, Mutex};

use hds_recover::{Flag, FlagSet, Rating};

use crate::catalog::{Catalog, NameSrc, NodeId, NodeKind, NodeRec, NONE};
use crate::health::{child_bits, ext_of};
use crate::store::ScanStore;

pub use info::{
    CatalogSummary, InstanceSummary, LiveVolumeSummary, ProtectedSummary, POST_FORMAT_TEXT,
    WARN_OUT_OF_RANGE_FILES, WARN_SCAN_INCOMPLETE,
};
pub use select::{Selection, SelectionSummary};

/// Largest page of `list_children` and `search`.
pub const MAX_PAGE: u32 = 1000;
/// Paths and ancestor lists name at most this many levels.
pub const MAX_PATH_DEPTH: usize = 1024;
/// Cached listings and searches.
const CACHE_ENTRIES: usize = 8;
/// Result lists up to this length are sorted whole on first use.
const FULL_SORT: usize = 8192;
/// The first partial sort of a longer list covers at least this many.
const MIN_CHUNK: usize = 1024;

/// Converts a FILETIME to Unix milliseconds (PROTOCOL conventions): `None`
/// for 0 and for values of 2^63 and above.
pub fn filetime_to_unix_ms(ft: u64) -> Option<i64> {
    if ft == 0 || ft >= 1 << 63 {
        return None;
    }
    let ft = i64::try_from(ft).ok()?;
    let rel = ft.checked_sub(116_444_736_000_000_000)?;
    Some(rel.div_euclid(10_000))
}

/// Why a query failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// No node with this id (`not_found`).
    NotFound(u64),
    /// The arguments do not fit the node or each other (`bad_request`).
    BadRequest(String),
    /// The file's content is not stored as plain clusters, or it is
    /// browse-only (`unsupported`, `read_file`).
    Unsupported(String),
}

impl QueryError {
    /// The protocol error code.
    pub fn code(&self) -> &'static str {
        match self {
            QueryError::NotFound(_) => "not_found",
            QueryError::BadRequest(_) => "bad_request",
            QueryError::Unsupported(_) => "unsupported",
        }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::NotFound(id) => write!(f, "node {id} does not exist"),
            QueryError::BadRequest(m) | QueryError::Unsupported(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for QueryError {}

/// Sort key of listings and searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SortKey {
    /// Case-insensitive name.
    #[default]
    Name,
    /// Size.
    Size,
    /// Modification time.
    Modified,
    /// Rating.
    Rating,
    /// Extension.
    Ext,
}

/// One node in a listing (PROTOCOL `Row`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Node id.
    pub id: NodeId,
    /// Parent; `None` for an instance root.
    pub parent: Option<NodeId>,
    /// Display name (unpaired surrogates as U+FFFD).
    pub name: String,
    /// Kind.
    pub kind: NodeKind,
    /// Lower-case extension without the dot (`""` for folders and names
    /// without one).
    pub ext: String,
    /// Files: real size; folders: total size of the non-deleted files below.
    pub size: u64,
    /// Modification time, Unix ms.
    pub modified: Option<i64>,
    /// Creation time, Unix ms.
    pub created: Option<i64>,
    /// Deleted.
    pub deleted: bool,
    /// Rating; `None` for folders.
    pub rating: Option<Rating>,
    /// Flags.
    pub flags: FlagSet,
    /// Folders: non-deleted files below.
    pub files: u64,
    /// Folders: deleted files below.
    pub deleted_files: u64,
    /// Folders: bytes of the deleted files below.
    pub deleted_size: u64,
    /// Folders: a child folder that a `dirs_only` listing returns.
    pub has_children: bool,
}

/// A page of rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<R> {
    /// Rows after filters.
    pub total: u64,
    /// This page.
    pub rows: Vec<R>,
}

/// `list_children` arguments (epoch checked by the engine).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListArgs {
    /// The folder.
    pub node: u64,
    /// Rows to skip.
    pub offset: u64,
    /// Rows to return (at most [`MAX_PAGE`]).
    pub limit: u32,
    /// Sort key.
    pub sort: SortKey,
    /// Descending.
    pub desc: bool,
    /// Folders only.
    pub dirs_only: bool,
    /// Show deleted files and folders.
    pub show_deleted: bool,
    /// Extensions of the files to list.
    pub exts: Option<Vec<String>>,
    /// Ratings of the files to list.
    pub ratings: Option<Vec<Rating>>,
}

/// The `Filter` object.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct Filter {
    /// Case-insensitive substring of the name.
    pub query: Option<String>,
    /// Extensions.
    pub exts: Option<Vec<String>>,
    /// Smallest size.
    pub min_size: Option<u64>,
    /// Largest size.
    pub max_size: Option<u64>,
    /// Ratings.
    pub ratings: Option<Vec<Rating>>,
}

/// `search` arguments.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchArgs {
    /// Subtree; `None` = the main root.
    pub under: Option<u64>,
    /// The filter.
    pub filter: Filter,
    /// Include deleted files.
    pub show_deleted: bool,
    /// Sort key.
    pub sort: SortKey,
    /// Descending.
    pub desc: bool,
    /// Rows to skip.
    pub offset: u64,
    /// Rows to return.
    pub limit: u32,
}

/// A search result (PROTOCOL: `Row` plus `path` and `ancestor_ids`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRow {
    /// The row.
    pub row: Row,
    /// Display path.
    pub path: String,
    /// Ancestors, root first.
    pub ancestor_ids: Vec<NodeId>,
}

/// `node_info.record`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordDetails {
    /// Record number: the effective one of D1 (the header's, else the
    /// positional one); `None` when neither is known (an unnumbered stray
    /// record).
    pub number: Option<u64>,
    /// Sequence number.
    pub seq: u32,
    /// Absolute offset of the base record.
    pub disk_offset: u64,
    /// Extension records.
    pub extension_records: u32,
}

/// `node_info.data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDetails {
    /// Resident.
    pub resident: bool,
    /// Real size.
    pub real_size: u64,
    /// Initialized size.
    pub initialized_size: u64,
    /// Allocated size.
    pub allocated_size: u64,
    /// Runs.
    pub runs: u64,
    /// First LCN.
    pub first_lcn: Option<u64>,
    /// Absolute offset of the first data byte.
    pub first_offset: Option<u64>,
}

/// `node_info` (PROTOCOL `NodeInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeDetails {
    /// The row.
    pub row: Row,
    /// Ancestors, root first, with names.
    pub ancestors: Vec<(NodeId, String)>,
    /// Display path.
    pub path: String,
    /// Catalog instance id.
    pub instance: u32,
    /// Access time, Unix ms.
    pub accessed: Option<i64>,
    /// Other hard links (names other than the shown one; DOS aliases
    /// left out).
    pub other_names: Vec<String>,
    /// The record (`None` for virtual, lost and carved nodes).
    pub record: Option<RecordDetails>,
    /// The data (`None` for folders).
    pub data: Option<DataDetails>,
    /// One explanation per flag.
    pub flag_details: Vec<(Flag, &'static str)>,
}

/// The cache of sorted listings and searches.
#[derive(Debug)]
pub(crate) struct QueryCache {
    entries: Mutex<Vec<(CacheKey, Arc<Mutex<Sorted>>)>>,
    /// [`FULL_SORT`] and [`MIN_CHUNK`] (smaller in tests that exercise the
    /// partial sorts).
    tuning: Mutex<(usize, usize)>,
    /// Per catalog instance: files, dirs, deleted files, lost dirs, bytes
    /// (`catalog_info`). They depend only on the tree and the health, which
    /// a catalog value never changes (a new one gets a new cache).
    pub(crate) counts: std::sync::OnceLock<Vec<[u64; 5]>>,
}

impl Default for QueryCache {
    fn default() -> Self {
        QueryCache {
            entries: Mutex::new(Vec::new()),
            tuning: Mutex::new((FULL_SORT, MIN_CHUNK)),
            counts: std::sync::OnceLock::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CacheKey {
    List {
        node: u32,
        sort: SortKey,
        desc: bool,
        dirs_only: bool,
        show_deleted: bool,
        exts: Option<Vec<String>>,
        ratings: Option<Vec<Rating>>,
        gen: u64,
    },
    Search {
        under: u32,
        filter: Filter,
        show_deleted: bool,
        sort: SortKey,
        desc: bool,
        gen: u64,
    },
}

/// A result list sorted up to `sorted` (the rest is unordered).
#[derive(Debug, Default)]
struct Sorted {
    ids: Vec<u32>,
    sorted: usize,
}

impl QueryCache {
    fn get_or(&self, key: CacheKey, make: impl FnOnce() -> Vec<u32>) -> Arc<Mutex<Sorted>> {
        if let Ok(mut e) = self.entries.lock() {
            if let Some(i) = e.iter().position(|(k, _)| *k == key) {
                let hit = e.remove(i);
                let v = hit.1.clone();
                e.push(hit);
                return v;
            }
        }
        let v = Arc::new(Mutex::new(Sorted {
            ids: make(),
            sorted: 0,
        }));
        if let Ok(mut e) = self.entries.lock() {
            if e.len() >= CACHE_ENTRIES {
                e.remove(0);
            }
            e.push((key, v.clone()));
        }
        v
    }
}

/// What a sort compares, per node.
#[derive(Debug, Clone)]
struct SortVal<'a> {
    folder: bool,
    key: KeyVal<'a>,
    name: &'a str,
    id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum KeyVal<'a> {
    Num(Option<u64>),
    Time(Option<i64>),
    Rating(Option<Rating>),
    Text(&'a str),
}

fn cmp_vals(a: &SortVal<'_>, b: &SortVal<'_>, desc: bool) -> Ordering {
    // Folders first, nulls last, whatever the direction; ties by name,
    // then id, ascending.
    b.folder
        .cmp(&a.folder)
        .then_with(|| {
            let (na, nb) = (a.key.is_null(), b.key.is_null());
            na.cmp(&nb).then_with(|| {
                let o = a.key.cmp(&b.key);
                if desc {
                    o.reverse()
                } else {
                    o
                }
            })
        })
        .then_with(|| a.name.cmp(b.name))
        .then_with(|| a.id.cmp(&b.id))
}

/// A sort key in the bounded heap of [`Catalog::sort_upto`].
struct HeapItem<'a> {
    v: SortVal<'a>,
    idx: usize,
    desc: bool,
}

impl PartialEq for HeapItem<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for HeapItem<'_> {}

impl PartialOrd for HeapItem<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapItem<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        cmp_vals(&self.v, &other.v, self.desc)
    }
}

impl KeyVal<'_> {
    fn is_null(&self) -> bool {
        matches!(
            self,
            KeyVal::Num(None) | KeyVal::Time(None) | KeyVal::Rating(None)
        )
    }
}

/// Whether `ext` is in a filter's extension list (already lower case,
/// see [`lower_exts`]).
fn ext_matches(exts: &Option<Vec<String>>, ext: &str) -> bool {
    exts.as_ref().is_none_or(|l| l.iter().any(|e| e == ext))
}

/// A filter's extensions in lower case (the protocol sends them so; this
/// makes the comparison exact without allocating per node).
fn lower_exts(exts: &Option<Vec<String>>) -> Option<Vec<String>> {
    exts.as_ref()
        .map(|l| l.iter().map(|e| e.to_lowercase()).collect())
}

impl Catalog {
    // ------------------------------------------------------------ nodes

    /// The folded (lower-case) name of a node.
    pub(crate) fn folded(&self, id: u32) -> &str {
        if id < self.tree.len() {
            return self.tree.folded_at(id);
        }
        let Some(n) = self.rec(id) else { return "" };
        let start = usize::try_from(n.fold).unwrap_or(usize::MAX);
        self.carved
            .fold
            .get(start..)
            .unwrap_or("")
            .split('\0')
            .next()
            .unwrap_or("")
    }

    /// The display name of a node.
    pub fn name(&self, store: &ScanStore, id: NodeId) -> String {
        let Some(n) = self.rec(id.0) else {
            return String::new();
        };
        if id.0 < self.tree.len() {
            return self.tree.display_name(store, n);
        }
        match NameSrc::decode(n.name) {
            NameSrc::Text(t) => self
                .carved
                .texts
                .get(usize::try_from(t).unwrap_or(usize::MAX))
                .cloned()
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// The exact UTF-16 name of a node (for recovery).
    pub(crate) fn name_units(&self, store: &ScanStore, id: u32) -> Vec<u16> {
        let Some(n) = self.rec(id) else {
            return Vec::new();
        };
        if id < self.tree.len() {
            return crate::catalog::tree::name_units(
                store,
                &self.tree.texts,
                NameSrc::decode(n.name),
            );
        }
        self.name(store, NodeId(id)).encode_utf16().collect()
    }

    fn carved_root(&self) -> Option<u32> {
        (!self.carved.nodes.is_empty()).then_some(self.carved.base)
    }

    fn is_main_root(&self, id: u32) -> bool {
        self.instances
            .iter()
            .any(|i| i.root.0 == id && i.kind == crate::instances::InstanceKind::Main)
    }

    /// Sets the sort thresholds (tests of the partial sorts).
    #[cfg(test)]
    pub(crate) fn set_sort_tuning(&self, full_sort: usize, min_chunk: usize) {
        if let Ok(mut t) = self.cache.tuning.lock() {
            *t = (full_sort, min_chunk);
        }
    }

    /// The children of a folder, in id order.
    pub(crate) fn children(&self, id: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let Some(n) = self.rec(id) else { return out };
        let mut c = id.saturating_add(1);
        while c < n.end {
            out.push(c);
            let Some(cn) = self.rec(c) else { break };
            if cn.end <= c {
                break;
            }
            c = cn.end;
        }
        if self.is_main_root(id) {
            out.extend(self.carved_root());
        }
        out
    }

    /// The id ranges of the subtree of a node (the carved section joins
    /// the main root's).
    pub(crate) fn subtree(&self, id: u32) -> Vec<std::ops::Range<u32>> {
        let Some(n) = self.rec(id) else {
            return Vec::new();
        };
        let mut v = Vec::with_capacity(2);
        v.push(id..n.end.max(id));
        if self.is_main_root(id) {
            if let Some(c) = self.carved_root() {
                let end = self.rec(c).map_or(c, |x| x.end);
                v.push(c..end);
            }
        }
        v
    }

    /// Size and aggregates of a node: (size, files, deleted files,
    /// deleted size, child bits).
    fn numbers(&self, id: u32) -> (u64, u64, u64, u64, u8) {
        let h = &self.health;
        let (mut s, mut f, mut df, mut ds, mut cb) = if id < self.tree.len() {
            let i = usize::try_from(id).unwrap_or(usize::MAX);
            (
                h.size.get(i).copied().unwrap_or(0),
                u64::from(h.files.get(i).copied().unwrap_or(0)),
                u64::from(h.deleted_files.get(i).copied().unwrap_or(0)),
                h.deleted_size.get(i).copied().unwrap_or(0),
                h.child_dirs.get(i).copied().unwrap_or(0),
            )
        } else {
            let k = id
                .checked_sub(self.carved.base)
                .and_then(|k| usize::try_from(k).ok())
                .unwrap_or(usize::MAX);
            let sec = &self.carved;
            let is_folder = self.rec(id).is_some_and(|n| n.kind.is_folder());
            let has_sub = sec
                .nodes
                .get(k.saturating_add(1))
                .is_some_and(|n| n.kind.is_folder() && n.parent == id);
            (
                sec.size.get(k).copied().unwrap_or(0),
                u64::from(sec.files.get(k).copied().unwrap_or(0)),
                0,
                0,
                if is_folder && has_sub {
                    child_bits::ANY | child_bits::VISIBLE
                } else {
                    0
                },
            )
        };
        if self.is_main_root(id) {
            if let Some(c) = self.carved_root() {
                let (cs, cf, _, _, _) = self.numbers(c);
                s = s.saturating_add(cs);
                f = f.saturating_add(cf);
                cb |= child_bits::ANY | child_bits::VISIBLE;
                df = df.saturating_add(0);
                ds = ds.saturating_add(0);
            }
        }
        (s, f, df, ds, cb)
    }

    fn end_verified(&self, id: u32) -> bool {
        id.checked_sub(self.carved.base)
            .and_then(|k| self.carved.entries.get(usize::try_from(k).ok()?))
            .and_then(|e| e.as_ref())
            .is_some_and(|e| e.end_verified)
    }

    /// The rating of a node (`None` for folders).
    pub fn rating(&self, id: NodeId) -> Option<Rating> {
        let n = self.rec(id.0)?;
        crate::health::rating(
            n.kind,
            self.flags(id),
            self.health.validated(n.inst),
            self.end_verified(id.0),
        )
    }

    fn si_times(&self, store: &ScanStore, n: &NodeRec) -> Option<[u64; 4]> {
        if n.rec == NONE {
            return None;
        }
        store
            .record(crate::store::RecordId(n.rec))
            .and_then(|r| r.standard_information())
            .map(|si| {
                [
                    si.times.created,
                    si.times.modified,
                    si.times.mft_changed,
                    si.times.accessed,
                ]
            })
    }

    /// The row of a node. `show_deleted` decides `has_children`.
    pub fn row(&self, store: &ScanStore, id: NodeId, show_deleted: bool) -> Option<Row> {
        let n = *self.rec(id.0)?;
        let (size, files, deleted_files, deleted_size, cb) = self.numbers(id.0);
        let folder = n.kind.is_folder();
        let times = self.si_times(store, &n);
        let flags = self.flags(id);
        Some(Row {
            id,
            parent: (n.parent != NONE).then_some(NodeId(n.parent)),
            name: self.name(store, id),
            kind: n.kind,
            ext: if folder {
                String::new()
            } else {
                ext_of(self.folded(id.0)).to_string()
            },
            size,
            modified: times.and_then(|t| filetime_to_unix_ms(t[1])),
            created: times.and_then(|t| filetime_to_unix_ms(t[0])),
            deleted: n.deleted(),
            rating: self.rating(id),
            flags,
            files: if folder { files } else { 0 },
            deleted_files: if folder { deleted_files } else { 0 },
            deleted_size: if folder { deleted_size } else { 0 },
            has_children: folder
                && cb
                    & if show_deleted {
                        child_bits::ANY
                    } else {
                        child_bits::VISIBLE
                    }
                    != 0,
        })
    }

    fn sort_val<'a>(&'a self, store: &ScanStore, id: u32, sort: SortKey) -> SortVal<'a> {
        let n = self.rec(id);
        let folder = n.is_some_and(|n| n.kind.is_folder());
        let key = match sort {
            SortKey::Name => KeyVal::Text(self.folded(id)),
            SortKey::Size => KeyVal::Num(Some(self.numbers(id).0)),
            SortKey::Modified => KeyVal::Time(
                n.and_then(|n| self.si_times(store, n))
                    .and_then(|t| filetime_to_unix_ms(t[1])),
            ),
            SortKey::Rating => KeyVal::Rating(self.rating(NodeId(id))),
            SortKey::Ext => KeyVal::Text(if folder { "" } else { ext_of(self.folded(id)) }),
        };
        SortVal {
            folder,
            key,
            name: self.folded(id),
            id,
        }
    }

    /// Sorts `ids` as far as `upto` (the first `upto` in final order).
    fn sort_upto(&self, store: &ScanStore, s: &mut Sorted, upto: usize, sort: SortKey, desc: bool) {
        let upto = upto.min(s.ids.len());
        if s.sorted >= upto {
            return;
        }
        let need = upto.saturating_sub(s.sorted);
        let rest = s.ids.get_mut(s.sorted..).unwrap_or(&mut []);
        if need > 0 && need.saturating_mul(4) < rest.len() {
            // The `need` first of a long list: a bounded heap computes each
            // sort key once and mostly compares it once.
            let mut heap: std::collections::BinaryHeap<HeapItem<'_>> =
                std::collections::BinaryHeap::with_capacity(need.saturating_add(1));
            for (idx, &id) in rest.iter().enumerate() {
                let item = HeapItem {
                    v: self.sort_val(store, id, sort),
                    idx,
                    desc,
                };
                if heap.len() < need {
                    heap.push(item);
                } else if heap.peek().is_some_and(|top| item < *top) {
                    heap.pop();
                    heap.push(item);
                }
            }
            let first = heap.into_sorted_vec();
            let mut taken = vec![false; rest.len()];
            let mut head: Vec<u32> = Vec::with_capacity(first.len());
            for it in &first {
                if let Some(t) = taken.get_mut(it.idx) {
                    *t = true;
                }
                head.push(it.v.id);
            }
            drop(first);
            let mut tail: Vec<u32> = Vec::with_capacity(rest.len().saturating_sub(head.len()));
            for (idx, &id) in rest.iter().enumerate() {
                if !taken.get(idx).copied().unwrap_or(true) {
                    tail.push(id);
                }
            }
            for (slot, id) in rest.iter_mut().zip(head.into_iter().chain(tail)) {
                *slot = id;
            }
        } else {
            let cmp = |a: &u32, b: &u32| {
                cmp_vals(
                    &self.sort_val(store, *a, sort),
                    &self.sort_val(store, *b, sort),
                    desc,
                )
            };
            if need < rest.len() && need > 0 {
                rest.select_nth_unstable_by(need.saturating_sub(1), cmp);
                if let Some(head) = rest.get_mut(..need) {
                    head.sort_unstable_by(cmp);
                }
            } else {
                rest.sort_unstable_by(cmp);
            }
        }
        s.sorted = upto;
    }

    fn page(
        &self,
        store: &ScanStore,
        list: &Arc<Mutex<Sorted>>,
        offset: u64,
        limit: u32,
        sort: SortKey,
        desc: bool,
    ) -> (u64, Vec<u32>) {
        let Ok(mut s) = list.lock() else {
            return (0, Vec::new());
        };
        let total = u64::try_from(s.ids.len()).unwrap_or(u64::MAX);
        let off = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(s.ids.len());
        let lim = usize::try_from(limit.min(MAX_PAGE)).unwrap_or(0);
        let upto = off.saturating_add(lim).min(s.ids.len());
        // Lists up to FULL_SORT are sorted once; longer ones as far as
        // asked, growing the sorted prefix at least twofold each time, so
        // paging through costs O(limit) per page on average.
        let (full_sort, min_chunk) = self
            .cache
            .tuning
            .lock()
            .map(|t| *t)
            .unwrap_or((FULL_SORT, MIN_CHUNK));
        let want = if s.ids.len() <= full_sort {
            s.ids.len()
        } else {
            upto.max(s.sorted.saturating_mul(2)).max(min_chunk)
        };
        if s.sorted < upto {
            self.sort_upto(store, &mut s, want, sort, desc);
        }
        (total, s.ids.get(off..upto).unwrap_or(&[]).to_vec())
    }

    // ------------------------------------------------------------ list_children

    /// `list_children`.
    pub fn list_children(
        &self,
        store: &ScanStore,
        args: &ListArgs,
    ) -> Result<Page<Row>, QueryError> {
        let id = NodeId::from_wire(args.node)
            .filter(|&n| self.contains(n))
            .ok_or(QueryError::NotFound(args.node))?;
        if !self.rec(id.0).is_some_and(|n| n.kind.is_folder()) {
            return Err(QueryError::BadRequest(format!(
                "node {} is not a folder",
                args.node
            )));
        }
        let key = CacheKey::List {
            node: id.0,
            sort: args.sort,
            desc: args.desc,
            dirs_only: args.dirs_only,
            show_deleted: args.show_deleted,
            exts: args.exts.clone(),
            ratings: args.ratings.clone(),
            // What reads find (G1) changes ratings only: a list that
            // neither filters nor sorts by rating stays valid.
            gen: if args.ratings.is_some() || args.sort == SortKey::Rating {
                self.observed_gen()
            } else {
                0
            },
        };
        let list = self.cache.get_or(key, || {
            let args = ListArgs {
                exts: lower_exts(&args.exts),
                ..args.clone()
            };
            self.children(id.0)
                .into_iter()
                .filter(|&c| self.listed(c, &args))
                .collect()
        });
        let (total, ids) = self.page(store, &list, args.offset, args.limit, args.sort, args.desc);
        let rows = ids
            .into_iter()
            .filter_map(|c| self.row(store, NodeId(c), args.show_deleted))
            .collect();
        Ok(Page { total, rows })
    }

    fn listed(&self, c: u32, args: &ListArgs) -> bool {
        let Some(n) = self.rec(c) else { return false };
        if n.kind.is_folder() {
            if !args.show_deleted && n.deleted() && self.numbers(c).1 == 0 {
                return false;
            }
            return true;
        }
        if args.dirs_only || (!args.show_deleted && n.deleted()) {
            return false;
        }
        if !ext_matches(&args.exts, ext_of(self.folded(c))) {
            return false;
        }
        if let Some(r) = &args.ratings {
            if !self.rating(NodeId(c)).is_some_and(|x| r.contains(&x)) {
                return false;
            }
        }
        true
    }

    // ------------------------------------------------------------ paths

    /// The ancestors of a node, root first, at most [`MAX_PATH_DEPTH`]
    /// (the root and the nearest ones; `elided` tells whether levels were
    /// left out between them).
    ///
    /// The walk stops after [`MAX_PATH_DEPTH`] levels: the root of a
    /// deeper node is found from the preorder intervals of the instance
    /// roots, so the cost is bounded whatever the depth of the chain.
    pub(crate) fn ancestors(&self, id: u32) -> (Vec<u32>, bool) {
        let mut near: Vec<u32> = Vec::new();
        let mut x = self.rec(id).map_or(NONE, |n| n.parent);
        let mut root = NONE;
        let mut elided = false;
        while x != NONE {
            let Some(n) = self.rec(x) else { break };
            if n.parent == NONE {
                root = x;
                break;
            }
            if near.len() >= MAX_PATH_DEPTH {
                // `x` and the levels above it, up to the root, are left out.
                elided = true;
                root = self.tree_root_of(x);
                break;
            }
            near.push(x);
            x = n.parent;
        }
        near.reverse();
        let mut out = Vec::with_capacity(near.len().saturating_add(1));
        if root != NONE {
            out.push(root);
        }
        out.extend(near);
        (out, elided)
    }

    /// The root of the tree that holds node `x` (a node of the tree, not
    /// of the carved section): the instance root whose preorder interval
    /// `[root, end)` contains it ([`NONE`] if none does).
    fn tree_root_of(&self, x: u32) -> u32 {
        self.instances
            .iter()
            .map(|i| i.root.0)
            .find(|&r| r <= x && self.rec(r).is_some_and(|n| x < n.end))
            .unwrap_or(NONE)
    }

    /// The display path of a node (names from its root down, `/`
    /// separated, the empty name of the main root left out).
    pub fn path(&self, store: &ScanStore, id: NodeId) -> String {
        let (anc, elided) = self.ancestors(id.0);
        self.path_from(store, id, &anc, elided)
    }

    /// [`Catalog::path`] from the node's [`ancestors`](Self::ancestors).
    pub(crate) fn path_from(
        &self,
        store: &ScanStore,
        id: NodeId,
        anc: &[u32],
        elided: bool,
    ) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (k, a) in anc.iter().enumerate() {
            let name = self.name(store, NodeId(*a));
            if !name.is_empty() {
                parts.push(name);
            }
            if k == 0 && elided {
                parts.push("…".to_string());
            }
        }
        parts.push(self.name(store, id));
        parts.retain(|p| !p.is_empty());
        parts.join("/")
    }

    // ------------------------------------------------------------ node_info

    /// `node_info`.
    pub fn node_info(&self, store: &ScanStore, node: u64) -> Result<NodeDetails, QueryError> {
        let id = NodeId::from_wire(node)
            .filter(|&n| self.contains(n))
            .ok_or(QueryError::NotFound(node))?;
        let n = *self.rec(id.0).ok_or(QueryError::NotFound(node))?;
        let row = self
            .row(store, id, false)
            .ok_or(QueryError::NotFound(node))?;
        let (anc, elided) = self.ancestors(id.0);
        let path = self.path_from(store, id, &anc, elided);
        let ancestors = anc
            .iter()
            .map(|&a| (NodeId(a), self.name(store, NodeId(a))))
            .collect();
        let times = self.si_times(store, &n);
        let mut record = None;
        let mut data = None;
        let mut other_names = Vec::new();
        let ci = self.instances.get(usize::from(n.inst));
        if n.rec != NONE {
            let base = crate::store::RecordId(n.rec);
            let exts = self.tree.exts_of(id.0);
            let r = store.record(base);
            record = r.map(|r| RecordDetails {
                number: self.tree.record_number(store, base).map(u64::from),
                seq: u32::from(r.sequence()),
                disk_offset: r.disk_offset(),
                extension_records: u32::try_from(exts.len()).unwrap_or(u32::MAX),
            });
            let shown = NameSrc::decode(n.name);
            for rid in crate::catalog::groups::group_records(base, &exts) {
                for (i, nm) in store.names(rid).enumerate() {
                    let this = NameSrc::Record(rid, u16::try_from(i).unwrap_or(u16::MAX));
                    if this == shown || nm.namespace == hds_ntfs::Namespace::Dos {
                        continue;
                    }
                    let s = nm.name.to_string_lossy();
                    if s != row.name && !other_names.contains(&s) {
                        other_names.push(s);
                    }
                }
            }
            if n.kind == NodeKind::File {
                let g = ci.map(|c| &c.geometry);
                let cs = g.map_or(0, |g| u64::from(g.cluster_size));
                let vo = g.map_or(0, |g| g.volume_offset);
                let m = crate::catalog::groups::merge(store, base, &exts, cs);
                let first_lcn = m.first_lcn();
                data = Some(DataDetails {
                    resident: m.resident.is_some(),
                    real_size: m.size,
                    initialized_size: m.initialized,
                    allocated_size: m.allocated,
                    runs: m.run_count(),
                    first_lcn,
                    first_offset: match m.resident {
                        Some(res) => store
                            .record(res.record)
                            .and_then(|r| r.disk_offset().checked_add(u64::from(res.value_offset))),
                        None => {
                            crate::catalog::lost::run_bytes(first_lcn, 1, cs, vo).map(|r| r.start)
                        }
                    },
                });
            }
        }
        if n.kind == NodeKind::Carved {
            if let Some(e) = self.carved_entry(id.0) {
                data = Some(DataDetails {
                    resident: false,
                    real_size: e.len,
                    initialized_size: e.len,
                    allocated_size: e.len,
                    runs: 1,
                    first_lcn: self.instances.get(usize::from(n.inst)).and_then(|c| {
                        e.offset
                            .checked_sub(c.geometry.volume_offset)?
                            .checked_div(u64::from(c.geometry.cluster_size))
                    }),
                    first_offset: Some(e.offset),
                });
            }
        }
        let flag_details = row.flags.iter().map(|f| (f, f.explanation())).collect();
        Ok(NodeDetails {
            path,
            ancestors,
            instance: u32::from(n.inst),
            accessed: times.and_then(|t| filetime_to_unix_ms(t[3])),
            other_names,
            record,
            data,
            flag_details,
            row,
        })
    }

    pub(crate) fn carved_entry(&self, id: u32) -> Option<&crate::catalog::CarvedEntry> {
        let k = usize::try_from(id.checked_sub(self.carved.base)?).ok()?;
        self.carved.entries.get(k)?.as_ref()
    }

    // ------------------------------------------------------------ search

    /// Whether a file passes a filter (and `show_deleted`).
    pub(crate) fn passes(
        &self,
        id: u32,
        filter: &Filter,
        query: Option<&str>,
        deleted_ok: bool,
    ) -> bool {
        let Some(n) = self.rec(id) else { return false };
        if !n.kind.is_file() || (!deleted_ok && n.deleted()) {
            return false;
        }
        let folded = self.folded(id);
        if let Some(q) = query {
            if !folded.contains(q) {
                return false;
            }
        }
        if !ext_matches(&filter.exts, ext_of(folded)) {
            return false;
        }
        if filter.min_size.is_some() || filter.max_size.is_some() {
            let size = self.numbers(id).0;
            if filter.min_size.is_some_and(|m| size < m)
                || filter.max_size.is_some_and(|m| size > m)
            {
                return false;
            }
        }
        if let Some(r) = &filter.ratings {
            if !self.rating(NodeId(id)).is_some_and(|x| r.contains(&x)) {
                return false;
            }
        }
        true
    }

    /// Candidate nodes of a range whose folded name contains `q`: one
    /// substring search over the range's stretch of the name arena.
    fn name_hits(&self, range: std::ops::Range<u32>, q: &str) -> Vec<u32> {
        let (nodes, arena, base) = if range.start < self.tree.len() {
            (self.tree.nodes.as_slice(), self.tree.fold.as_str(), 0u32)
        } else {
            (
                self.carved.nodes.as_slice(),
                self.carved.fold.as_str(),
                self.carved.base,
            )
        };
        let lo = usize::try_from(range.start.saturating_sub(base)).unwrap_or(0);
        let hi = usize::try_from(range.end.saturating_sub(base))
            .unwrap_or(0)
            .min(nodes.len());
        let Some(slice) = nodes.get(lo..hi) else {
            return Vec::new();
        };
        // Offsets never decrease with the node id.
        let Some(a) = slice.first().map(|n| n.fold) else {
            return Vec::new();
        };
        let b = nodes.get(hi).map_or(arena.len(), |n| {
            usize::try_from(n.fold).unwrap_or(arena.len())
        });
        let a = usize::try_from(a).unwrap_or(arena.len());
        let Some(stretch) = arena.get(a..b) else {
            return Vec::new();
        };
        let mut out: Vec<u32> = Vec::new();
        for (pos, _) in stretch.match_indices(q) {
            let abs = u32::try_from(a.saturating_add(pos)).unwrap_or(NONE);
            let Some(k) = slice.partition_point(|n| n.fold <= abs).checked_sub(1) else {
                continue;
            };
            let id = u32::try_from(lo.saturating_add(k))
                .unwrap_or(NONE)
                .saturating_add(base);
            if out.last() != Some(&id) {
                out.push(id);
            }
        }
        out
    }

    /// `search`.
    pub fn search(
        &self,
        store: &ScanStore,
        args: &SearchArgs,
    ) -> Result<Page<SearchRow>, QueryError> {
        let under = match args.under {
            Some(u) => NodeId::from_wire(u)
                .filter(|&n| self.contains(n))
                .ok_or(QueryError::NotFound(u))?,
            None => self
                .instances
                .first()
                .map(|i| i.root)
                .ok_or(QueryError::NotFound(0))?,
        };
        let key = CacheKey::Search {
            under: under.0,
            filter: args.filter.clone(),
            show_deleted: args.show_deleted,
            sort: args.sort,
            desc: args.desc,
            // As for lists: only ratings depend on what reads find.
            gen: if args.filter.ratings.is_some() || args.sort == SortKey::Rating {
                self.observed_gen()
            } else {
                0
            },
        };
        let q = args.filter.query.as_deref().map(crate::catalog::tree::fold);
        let q = q.filter(|q| !q.is_empty());
        let list = self.cache.get_or(key, || {
            let filter = Filter {
                exts: lower_exts(&args.filter.exts),
                ..args.filter.clone()
            };
            let mut out = Vec::new();
            for r in self.subtree(under.0) {
                match &q {
                    Some(q) => {
                        // The names already matched: no second test.
                        for id in self.name_hits(r, q) {
                            if self.passes(id, &filter, None, args.show_deleted) {
                                out.push(id);
                            }
                        }
                    }
                    None => {
                        for id in r {
                            if self.passes(id, &filter, None, args.show_deleted) {
                                out.push(id);
                            }
                        }
                    }
                }
            }
            out
        });
        let (total, ids) = self.page(store, &list, args.offset, args.limit, args.sort, args.desc);
        let rows = ids
            .into_iter()
            .filter_map(|id| {
                let row = self.row(store, NodeId(id), args.show_deleted)?;
                let (anc, elided) = self.ancestors(id);
                Some(SearchRow {
                    path: self.path_from(store, NodeId(id), &anc, elided),
                    ancestor_ids: anc.into_iter().map(NodeId).collect(),
                    row,
                })
            })
            .collect();
        Ok(Page { total, rows })
    }
}
