//! Selections (PROTOCOL `selection_summary`, `recover_start`), recovery
//! plans (ALGORITHMS H2, H4) and `check_destination`'s catalog part.

use std::collections::BTreeMap;
use std::ops::Range;

use hds_io::out::Timestamps;
use hds_io::RangeSet;
use hds_recover::{FileSource, Plan, PlanFile, PlanFolder, Rating, Run};

use crate::catalog::groups::{self, Merged};
use crate::catalog::{Catalog, NodeId, NodeKind, NONE};
use crate::health::ext_of;
use crate::instances::InstanceKind;
use crate::store::{RecordId, ScanStore};

use super::{Filter, QueryError, MAX_PATH_DEPTH};

/// A selection (H2): `include` / `exclude` node ids, the filter and the
/// switches of `selection_summary` and `recover_start`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Selection {
    /// Selected nodes.
    pub include: Vec<u64>,
    /// Deselected nodes.
    pub exclude: Vec<u64>,
    /// Applied to every selected file.
    pub filter: Option<Filter>,
    /// Deleted files are selected too.
    pub include_deleted: bool,
    /// Damaged files are not attempted.
    pub skip_damaged: bool,
}

/// `selection_summary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelectionSummary {
    /// Selected files after the filter and `include_deleted`.
    pub files: u64,
    /// Bytes of the files that will be attempted.
    pub bytes: u64,
    /// Directories that will be created.
    pub dirs: u64,
    /// Selected files rated `good`.
    pub good: u64,
    /// ... `uncertain`.
    pub uncertain: u64,
    /// ... `damaged`.
    pub damaged: u64,
    /// ... `unsupported`.
    pub unsupported: u64,
}

/// The selected files and folders of a selection.
struct Selected {
    files: Vec<u32>,
    folders: Vec<u32>,
}

impl Catalog {
    fn node_of(&self, id: u64) -> Result<u32, QueryError> {
        NodeId::from_wire(id)
            .filter(|&n| self.contains(n))
            .map(|n| n.0)
            .ok_or(QueryError::NotFound(id))
    }

    /// H2: the selected nodes. A node is selected when its nearest
    /// ancestor-or-self in `include` or `exclude` is in `include`; the
    /// subtree of a node is an id range, so this is a sweep over the
    /// listed nodes.
    fn select(&self, sel: &Selection) -> Result<Selected, QueryError> {
        let mut listed: BTreeMap<u32, bool> = BTreeMap::new();
        for &i in &sel.include {
            listed.insert(self.node_of(i)?, true);
        }
        for &i in &sel.exclude {
            let n = self.node_of(i)?;
            if listed.get(&n) == Some(&true) {
                return Err(QueryError::BadRequest(format!(
                    "node {i} is both included and excluded"
                )));
            }
            listed.insert(n, false);
        }
        let mut filter = sel.filter.clone().unwrap_or_default();
        filter.exts = super::lower_exts(&filter.exts);
        let q = filter.query.as_deref().map(crate::catalog::tree::fold);
        let q = q.filter(|q| !q.is_empty());
        let mut out = Selected {
            files: Vec::new(),
            folders: Vec::new(),
        };
        let take = |id: u32, out: &mut Selected| {
            let Some(n) = self.rec(id) else { return };
            if n.kind.is_folder() {
                if !n.deleted() || sel.include_deleted {
                    out.folders.push(id);
                }
            } else if self.passes(id, &filter, q.as_deref(), sel.include_deleted) {
                out.files.push(id);
            }
        };
        // Static ids: a sweep with a stack of open listed subtrees.
        let tree_len = self.tree.len();
        let mut stack: Vec<(u32, bool)> = Vec::new();
        let mut cursor = 0u32;
        let emit = |a: u32, b: u32, on: bool, out: &mut Selected| {
            if on {
                for id in a..b {
                    take(id, out);
                }
            }
        };
        for (&id, &inc) in listed.range(..tree_len) {
            while let Some(&(end, on)) = stack.last() {
                if end > id {
                    break;
                }
                emit(cursor, end, on, &mut out);
                cursor = cursor.max(end);
                stack.pop();
            }
            if let Some(&(_, on)) = stack.last() {
                emit(cursor, id, on, &mut out);
            }
            cursor = id;
            let end = self.rec(id).map_or(id, |n| n.end);
            stack.push((end.max(id.saturating_add(1)), inc));
        }
        while let Some((end, on)) = stack.pop() {
            emit(cursor, end, on, &mut out);
            cursor = cursor.max(end);
        }
        // Carved nodes: their nearest listed ancestor-or-self (at most
        // three levels below the main root).
        let base = self.carved.base;
        let count = u32::try_from(self.carved.nodes.len()).unwrap_or(0);
        for id in base..base.saturating_add(count) {
            let mut x = id;
            let mut state = None;
            let mut guard = 0u32;
            while x != NONE && guard < 64 {
                if let Some(&inc) = listed.get(&x) {
                    state = Some(inc);
                    break;
                }
                x = self.rec(x).map_or(NONE, |n| n.parent);
                guard = guard.saturating_add(1);
            }
            if state == Some(true) {
                take(id, &mut out);
            }
        }
        Ok(out)
    }

    /// The folders a plan creates for `sel`: the selected folders and the
    /// ancestors of everything selected, below the main root (whose
    /// folder is the destination itself); for other instances their root
    /// is the folder named after the label (H4).
    fn plan_folders(&self, sel: &Selected) -> Vec<u32> {
        let mut set: Vec<u32> = Vec::new();
        let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut add_chain = |start: u32, set: &mut Vec<u32>| {
            let mut x = start;
            while x != NONE {
                if !seen.insert(x) {
                    break;
                }
                if !self.is_main_root(x) {
                    set.push(x);
                }
                x = self.rec(x).map_or(NONE, |n| n.parent);
            }
        };
        for &f in &sel.folders {
            add_chain(f, &mut set);
        }
        for &f in &sel.files {
            let p = self.rec(f).map_or(NONE, |n| n.parent);
            add_chain(p, &mut set);
        }
        set.sort_unstable();
        set
    }

    /// `selection_summary`.
    pub fn selection_summary(&self, sel: &Selection) -> Result<SelectionSummary, QueryError> {
        let s = self.select(sel)?;
        let mut out = SelectionSummary {
            dirs: u64::try_from(self.plan_folders(&s).len()).unwrap_or(u64::MAX),
            ..SelectionSummary::default()
        };
        for &f in &s.files {
            let rating = self.rating(NodeId(f)).unwrap_or(Rating::Uncertain);
            out.files = out.files.saturating_add(1);
            let slot = match rating {
                Rating::Good => &mut out.good,
                Rating::Uncertain => &mut out.uncertain,
                Rating::Damaged => &mut out.damaged,
                Rating::Unsupported => &mut out.unsupported,
            };
            *slot = slot.saturating_add(1);
            let attempted =
                rating != Rating::Unsupported && !(sel.skip_damaged && rating == Rating::Damaged);
            if attempted {
                out.bytes = out.bytes.saturating_add(self.numbers(f).0);
            }
        }
        Ok(out)
    }

    /// H2: the recovery plan of a selection, in `hds_recover`'s types.
    ///
    /// `fresh` is the overwritten set of the snapshot taken at
    /// `recover_start` (H1); files it touches are marked `overwritten`
    /// first ([`Catalog::refresh_overwritten`]). Every selected file is in
    /// the plan, also `unsupported` ones (recovery reports them
    /// `skipped:*`) and damaged ones (`skip_damaged` is a recovery
    /// option). Folders: the selected ones (created even when empty) and
    /// the ancestors of everything selected, from the instance root:
    /// files of other instances go below a folder named after the
    /// instance label (H4); virtual and lost folders are real folders.
    pub fn plan(
        &self,
        store: &ScanStore,
        sel: &Selection,
        fresh: Option<&RangeSet>,
    ) -> Result<Plan, QueryError> {
        if let Some(set) = fresh {
            self.refresh_overwritten(set);
        }
        let s = self.select(sel)?;
        let folder_ids = self.plan_folders(&s);
        let parent_of = |id: u32| -> Option<u64> {
            let p = self.rec(id).map_or(NONE, |n| n.parent);
            (p != NONE && !self.is_main_root(p)).then_some(u64::from(p))
        };
        let mut plan = Plan::default();
        for &d in &folder_ids {
            let Some(n) = self.rec(d) else { continue };
            let times = self.si_times(store, n);
            plan.folders.push(PlanFolder {
                id: u64::from(d),
                parent: parent_of(d),
                name: self.name_units(store, d),
                times: times.map_or_else(Timestamps::default, timestamps),
                in_use: !n.deleted(),
                record_number: self.record_number(store, n.rec),
            });
        }
        // The display path of the last parent seen, when a child's path is
        // that path plus the child's name (no elision involved): files of
        // one folder then cost O(1) each instead of a walk to the root.
        let mut memo: (u32, Option<String>) = (NONE, None);
        for &f in &s.files {
            let Some(n) = self.rec(f).copied() else {
                continue;
            };
            let id = NodeId(f);
            if memo.0 != n.parent {
                let (anc, elided) = self.ancestors(n.parent);
                let composable = !elided && anc.len() < MAX_PATH_DEPTH;
                memo = (
                    n.parent,
                    composable.then(|| self.path_from(store, NodeId(n.parent), &anc, elided)),
                );
            }
            let original_path = match &memo.1 {
                Some(parent) => {
                    let name = self.name(store, id);
                    match (parent.is_empty(), name.is_empty()) {
                        (true, _) => name,
                        (false, true) => parent.clone(),
                        (false, false) => format!("{parent}/{name}"),
                    }
                }
                None => self.path(store, id),
            };
            let flags = self.flags(id);
            let validated = self.health.validated(n.inst);
            let end_verified = self.end_verified(f);
            let (source, record_offset) = self.file_source(store, f)?;
            plan.files.push(PlanFile {
                node: u64::from(f),
                folder: parent_of(f),
                name: self.name_units(store, f),
                ext: ext_of(self.folded(f)).to_string(),
                size: self.numbers(f).0,
                times: self
                    .si_times(store, &n)
                    .map_or_else(Timestamps::default, timestamps),
                flags,
                context: crate::health::context(n.kind, validated, end_verified),
                in_use: !n.deleted(),
                record_number: self.record_number(store, n.rec),
                record_offset,
                first_data_offset: source.first_offset(),
                original_path,
                source,
            });
        }
        Ok(plan)
    }

    fn record_number(&self, store: &ScanStore, rec: u32) -> Option<u64> {
        if rec == NONE {
            return None;
        }
        self.tree.record_number(store, RecordId(rec)).map(u64::from)
    }

    /// Where the content of a file is (`read_file`, plans): the source and
    /// the offset of its base record. `bad_request` for a folder.
    pub fn file_source(
        &self,
        store: &ScanStore,
        id: u32,
    ) -> Result<(FileSource, Option<u64>), QueryError> {
        let n = *self.rec(id).ok_or(QueryError::NotFound(u64::from(id)))?;
        if n.kind.is_folder() {
            return Err(QueryError::BadRequest(format!("node {id} is a folder")));
        }
        if n.kind == NodeKind::Carved {
            let e = self
                .carved_entry(id)
                .ok_or(QueryError::NotFound(u64::from(id)))?;
            return Ok((
                FileSource::Extents {
                    runs: vec![Run::Data {
                        offset: e.offset,
                        len: e.len,
                    }],
                    real_size: e.len,
                    initialized_size: e.len,
                },
                None,
            ));
        }
        let base = RecordId(n.rec);
        let exts = self.tree.exts_of(id);
        let g = self.instances.get(usize::from(n.inst)).map(|c| &c.geometry);
        let cs = g.map_or(4096, |g| u64::from(g.cluster_size));
        let vo = g.map_or(0, |g| g.volume_offset);
        let m = groups::merge(store, base, &exts, cs);
        let record_offset = store.record(base).map(|r| r.disk_offset());
        Ok((source_of(store, &m, cs, vo), record_offset))
    }

    /// `read_file`'s check: `bad_request` for a folder, `unsupported` for a
    /// file rated `unsupported` (compressed, encrypted, a placeholder, or
    /// browse-only inside a disk image), `not_found` for an unknown id.
    pub fn readable(&self, id: NodeId) -> Result<(), QueryError> {
        let n = self.rec(id.0).ok_or(QueryError::NotFound(id.wire()))?;
        if n.kind.is_folder() {
            return Err(QueryError::BadRequest(format!("node {} is a folder", id.0)));
        }
        if self.rating(id) == Some(Rating::Unsupported) {
            let why: Vec<&str> = self
                .flags(id)
                .intersection(hds_recover::UNSUPPORTED_FLAGS)
                .iter()
                .map(|f| f.name())
                .collect();
            return Err(QueryError::Unsupported(format!(
                "the content of node {} is not stored as plain clusters ({})",
                id.0,
                why.join(", ")
            )));
        }
        Ok(())
    }

    /// `check_destination`'s `lost_files` / `lost_bytes`: catalog files
    /// (deleted ones included) with data inside `extents` (absolute byte
    /// ranges of the source), and their sizes.
    pub fn files_in_extents(&self, extents: &[Range<u64>]) -> (u64, u64) {
        let mut nodes: Vec<u32> = Vec::new();
        for r in extents {
            for k in self.health.extents_in(r.start, r.end) {
                if let Some(&(_, _, n)) = self.health.extents.get(k) {
                    nodes.push(n);
                }
            }
            for (i, e) in self.carved.entries.iter().enumerate() {
                let Some(e) = e else { continue };
                if e.offset < r.end && e.offset.saturating_add(e.len) > r.start {
                    let id = u32::try_from(i)
                        .unwrap_or(NONE)
                        .saturating_add(self.carved.base);
                    nodes.push(id);
                }
            }
        }
        nodes.sort_unstable();
        nodes.dedup();
        let bytes = nodes
            .iter()
            .fold(0u64, |acc, &n| acc.saturating_add(self.numbers(n).0));
        (u64::try_from(nodes.len()).unwrap_or(u64::MAX), bytes)
    }

    /// ALGORITHMS I2 (a), the `protected_files` of
    /// [`CarveInput`](crate::carve::CarveInput): the absolute byte ranges of
    /// the data of every in-use file of the `main` instance that is neither
    /// `overwritten` nor `out_of_bounds` (flags found by reads included).
    /// Empty without a `main` instance.
    pub fn carve_protected(&self) -> RangeSet {
        let mut out = RangeSet::new();
        let Some(main) = self
            .instances
            .iter()
            .find(|i| i.kind == InstanceKind::Main)
            .map(|i| i.id)
        else {
            return out;
        };
        for &(start, end, node) in &self.health.extents {
            let Some(n) = self.rec(node) else { continue };
            if u32::from(n.inst) != main || n.kind != NodeKind::File || n.deleted() {
                continue;
            }
            let flags = self.flags(NodeId(node));
            if flags.contains(hds_recover::Flag::Overwritten)
                || flags.contains(hds_recover::Flag::OutOfBounds)
            {
                continue;
            }
            if start < end {
                out.insert(start..end);
            }
        }
        out
    }

    /// The instance kind of a node.
    pub fn instance_kind(&self, id: NodeId) -> Option<InstanceKind> {
        let n = self.rec(id.0)?;
        self.instances.get(usize::from(n.inst)).map(|i| i.kind)
    }
}

fn timestamps(t: [u64; 4]) -> Timestamps {
    Timestamps {
        created: t[0],
        modified: t[1],
        accessed: t[3],
    }
}

/// The [`FileSource`] of a merged stream (H7): resident data in its
/// record; otherwise the runs in VCN order, with holes for VCNs the chain
/// does not map (they read as zeros; the file is `missing_fragments`).
pub(crate) fn source_of(store: &ScanStore, m: &Merged, cs: u64, vo: u64) -> FileSource {
    if let Some(r) = m.resident {
        if let Some(rec) = store.record(r.record) {
            return FileSource::Resident {
                record_offset: rec.disk_offset(),
                record_size: rec.record_size(),
                value_offset: r.value_offset,
                value_len: r.value_len,
                hash: rec.hash(),
            };
        }
    }
    let mut runs: Vec<Run> = Vec::with_capacity(m.runs.len());
    let mut next = 0u64;
    for r in &m.runs {
        if r.vcn > next {
            runs.push(Run::Hole {
                len: r.vcn.saturating_sub(next).saturating_mul(cs),
            });
        }
        runs.push(Run::from_clusters(r.lcn, r.len, cs, vo));
        next = r.vcn.saturating_add(r.len);
    }
    FileSource::Extents {
        runs,
        real_size: m.size,
        initialized_size: m.initialized.min(m.size),
    }
}
