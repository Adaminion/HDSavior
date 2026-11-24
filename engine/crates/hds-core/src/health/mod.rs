//! Health of the catalog's files (ALGORITHMS G): the flags of G1, the
//! overlap conflicts of G2 and the ratings of G3 (through
//! [`hds_recover::rate`]), plus the aggregates of E10.
//!
//! [`Health`] is computed from the store, the [`Layout`] (kinds, D2 marks,
//! the geometry in effect of every instance, the overwritten set of C5)
//! and the stored heads; nothing is read from the source. It is separate
//! from the tree so that `geometry_set` can recompute it on the same tree
//! (F7).
//!
//! # Readings (see the crate report)
//!
//! * `out_of_bounds`: a run with `LCN + length > total_clusters` (the
//!   text; the lab model's default reads `LCN >= total_clusters`), or
//!   mapped bytes outside the session range. A geometry without a size
//!   (`total_clusters` 0, the fallback) checks the range only.
//! * `older_generation` from D2: the file's base record carries the mark.
//! * G2 compares the data of files (not the index allocation of
//!   directories) of all instances but `embedded` ones, each mapped with
//!   its instance's geometry in effect. It runs as one sweep over the
//!   extents sorted by start, so its cost is `O(E log E)` for `E` extents
//!   whatever the overlaps: every file is marked at most once per flag.
//! * The record time of G2 is the record time of the file's base record
//!   (its `$STANDARD_INFORMATION` MFT-change time and the `$FILE_NAME`
//!   times stored in that record).
//! * `signature_ok` / `signature_mismatch` at build time come from heads
//!   stored by geometry validation (F3) at the file's first data byte.

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

#[cfg(test)]
mod tests;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::ops::Bound::{Excluded, Included, Unbounded};
use std::ops::Range;

use hds_io::RangeSet;
use hds_recover::{Flag, FlagSet, Rating, RatingContext};

use crate::catalog::groups::{self, Merged};
use crate::catalog::lost::run_bytes;
use crate::catalog::{
    bits, BuildError, CarvedEntry, CarvedSection, CatalogInstance, NameSrc, NodeKind, NodeRec,
    Tree, CARVED_FILES, NONE,
};
use crate::geometry::{Head, Heads};
use crate::instances::{InstanceKind, Layout};
use crate::store::{RecordId, ScanStore};

/// `has_children` bits.
pub(crate) mod child_bits {
    /// At least one child folder.
    pub const ANY: u8 = 1;
    /// At least one child folder that a listing without `show_deleted`
    /// shows (not deleted, or with a non-deleted file below).
    pub const VISIBLE: u8 = 2;
}

/// Flags, sizes and aggregates of every node of a tree.
#[derive(Debug, Default)]
pub struct Health {
    /// Per node: G1 flags known to the catalog ([`FlagSet::bits`]).
    pub(crate) flags: Vec<u32>,
    /// Per node: real size of a file; total size of the non-deleted files
    /// below a folder (E10).
    pub(crate) size: Vec<u64>,
    /// Per node: non-deleted files below (folders).
    pub(crate) files: Vec<u32>,
    /// Per node: deleted files below (folders).
    pub(crate) deleted_files: Vec<u32>,
    /// Per node: bytes of the deleted files below (folders).
    pub(crate) deleted_size: Vec<u64>,
    /// Per node: [`child_bits`].
    pub(crate) child_dirs: Vec<u8>,
    /// Absolute byte ranges of the data of every file of the
    /// non-embedded instances: (start, end, node), sorted.
    pub(crate) extents: Vec<(u64, u64, u32)>,
    /// Running maximum of the ends of [`Health::extents`].
    pub(crate) max_end: Vec<u64>,
    /// The first data byte (the mapped run at VCN 0) of every deleted
    /// file of the non-embedded instances: (offset, node), sorted. A
    /// carved file starting there is that file's own content (I4).
    pub(crate) deleted_first: Vec<(u64, u32)>,
    /// Files with data outside the session range, and their bytes.
    pub(crate) out_of_range: (u64, u64),
    /// The overwritten set (C5) the flags were computed with.
    pub(crate) overwritten: RangeSet,
    /// Per catalog instance: its geometry in effect is validated.
    pub(crate) validated: Vec<bool>,
}

/// The G2 key: instance kind, in use, record time, LSN (larger wins).
type Key = (u8, u8, u64, u64);

fn kind_win(k: InstanceKind) -> u8 {
    match k {
        InstanceKind::Main => 4,
        InstanceKind::Other => 3,
        InstanceKind::Older => 2,
        InstanceKind::Strays => 1,
        InstanceKind::Embedded => 0,
    }
}

/// The lower-case extension of a folded name (`""` if none).
pub(crate) fn ext_of(folded: &str) -> &str {
    match folded.rfind('.') {
        Some(i) => folded.get(i.saturating_add(1)..).unwrap_or(""),
        None => "",
    }
}

impl Health {
    /// G1, G2 and E10 for every node of `tree`.
    pub(crate) fn compute(
        store: &ScanStore,
        layout: &Layout,
        tree: &Tree,
        instances: &[CatalogInstance],
        heads: &Heads,
    ) -> Health {
        let n = tree.nodes.len();
        let mut h = Health {
            flags: vec![0; n],
            size: vec![0; n],
            files: vec![0; n],
            deleted_files: vec![0; n],
            deleted_size: vec![0; n],
            child_dirs: vec![0; n],
            extents: Vec::new(),
            max_end: Vec::new(),
            deleted_first: Vec::new(),
            out_of_range: (0, 0),
            overwritten: layout.live().overwritten().clone(),
            validated: instances.iter().map(|i| i.geometry.validated).collect(),
        };
        let range = tree.range.clone();
        let bad = store.bad_ranges();
        let overwritten = layout.live().overwritten();
        let mut keys: Vec<Key> = vec![(0, 0, 0, 0); n];
        let mut ext_cursor = 0usize;
        let mut exts: Vec<RecordId> = Vec::new();
        for (i, node) in tree.nodes.iter().enumerate() {
            let id = u32::try_from(i).unwrap_or(NONE);
            exts.clear();
            while let Some(&(nd, r)) = tree.exts.get(ext_cursor) {
                if nd > id {
                    break;
                }
                if nd == id {
                    exts.push(r);
                }
                ext_cursor = ext_cursor.saturating_add(1);
            }
            if node.rec == NONE {
                continue;
            }
            let base = RecordId(node.rec);
            let Some(ci) = instances.get(usize::from(node.inst)) else {
                continue;
            };
            let mut f = FlagSet::EMPTY;
            if node.deleted() {
                f.insert(Flag::Deleted);
            }
            match ci.kind {
                InstanceKind::Older => f.insert(Flag::OlderGeneration),
                InstanceKind::Embedded => f.insert(Flag::Embedded),
                InstanceKind::Strays => f.insert(Flag::Stray),
                InstanceKind::Main | InstanceKind::Other => {}
            }
            if layout.is_older_generation(base) {
                f.insert(Flag::OlderGeneration);
            }
            if node.kind == NodeKind::Dir {
                let torn = groups::group_records(base, &exts).any(|r| {
                    store
                        .record(r)
                        .is_some_and(|x| x.fixup() != hds_ntfs::FixupStatus::Ok)
                });
                if torn {
                    f.insert(Flag::TornRecord);
                }
                if let Some(slot) = h.flags.get_mut(i) {
                    *slot = f.bits();
                }
                continue;
            }
            let g = &ci.geometry;
            let cs = u64::from(g.cluster_size);
            let vo = g.volume_offset;
            let m = groups::merge(store, base, &exts, cs);
            f = f.union(merged_flags(&m));
            let mut outside = false;
            let mut first = true;
            for r in &m.runs {
                let Some(lcn) = r.lcn else {
                    first = false;
                    continue;
                };
                let beyond = g.total_clusters > 0
                    && lcn.checked_add(r.len).is_none_or(|e| e > g.total_clusters);
                let Some(bytes) = run_bytes(Some(lcn), r.len, cs, vo) else {
                    f.insert(Flag::OutOfBounds);
                    outside = true;
                    continue;
                };
                if beyond {
                    f.insert(Flag::OutOfBounds);
                }
                if bytes.start < range.start || bytes.end > range.end {
                    f.insert(Flag::OutOfBounds);
                    outside = true;
                }
                if overwritten.intersects(bytes.clone()) {
                    f.insert(Flag::Overwritten);
                }
                if bad.intersects(bytes.clone()) {
                    f.insert(Flag::BadSectors);
                }
                if first && r.vcn == 0 {
                    if node.deleted() && ci.kind != InstanceKind::Embedded {
                        h.deleted_first.push((bytes.start, id));
                    }
                    if let Some(Head::Bytes(head)) = heads.stored(bytes.start) {
                        let ext = ext_of(tree.folded(node));
                        match hds_carve::matches_extension(&head, ext) {
                            Some(true) => f.insert(Flag::SignatureOk),
                            Some(false) => f.insert(Flag::SignatureMismatch),
                            None => {}
                        }
                    }
                }
                first = false;
                if ci.kind != InstanceKind::Embedded {
                    h.extents.push((bytes.start, bytes.end, id));
                }
            }
            if outside {
                h.out_of_range.0 = h.out_of_range.0.saturating_add(1);
                h.out_of_range.1 = h.out_of_range.1.saturating_add(m.size);
            }
            if let Some(slot) = h.size.get_mut(i) {
                *slot = m.size;
            }
            if let Some(slot) = h.flags.get_mut(i) {
                *slot = f.bits();
            }
            let rec = store.record(base);
            if let Some(k) = keys.get_mut(i) {
                *k = (
                    kind_win(ci.kind),
                    u8::from(!node.deleted()),
                    rec.and_then(|r| r.record_time()).unwrap_or(0),
                    rec.map_or(0, |r| r.lsn()),
                );
            }
        }
        h.extents.sort_unstable();
        h.deleted_first.sort_unstable();
        overlap_sweep(&h.extents, &keys, &mut h.flags);
        // D7: a stale container is overwritten.
        for inst in layout.instances() {
            if let Some(c) = &inst.stale_container {
                if let Some(node) = tree.node_of_record(c.record) {
                    if let Some(slot) = h.flags.get_mut(usize::try_from(node).unwrap_or(usize::MAX))
                    {
                        if tree.node(node).is_some_and(|x| x.kind == NodeKind::File) {
                            *slot |= Flag::Overwritten.bit();
                        }
                    }
                }
            }
        }
        let mut acc = 0u64;
        h.max_end = h
            .extents
            .iter()
            .map(|&(_, e, _)| {
                acc = acc.max(e);
                acc
            })
            .collect();
        h.aggregate(&tree.nodes);
        h
    }

    /// E10 and the `has_children` bits, bottom-up over the preorder.
    fn aggregate(&mut self, nodes: &[NodeRec]) {
        for i in (0..nodes.len()).rev() {
            let Some(node) = nodes.get(i) else { continue };
            let deleted = node.deleted();
            let (files, dfiles, size, dsize) = if node.kind.is_file() {
                let s = self.size.get(i).copied().unwrap_or(0);
                if deleted {
                    (0u32, 1u32, 0u64, s)
                } else {
                    (1, 0, s, 0)
                }
            } else {
                (
                    self.files.get(i).copied().unwrap_or(0),
                    self.deleted_files.get(i).copied().unwrap_or(0),
                    self.size.get(i).copied().unwrap_or(0),
                    self.deleted_size.get(i).copied().unwrap_or(0),
                )
            };
            let p = node.parent;
            if p == NONE {
                continue;
            }
            let pi = usize::try_from(p).unwrap_or(usize::MAX);
            if node.kind.is_folder() {
                if let Some(b) = self.child_dirs.get_mut(pi) {
                    *b |= child_bits::ANY;
                    if !deleted || files > 0 {
                        *b |= child_bits::VISIBLE;
                    }
                }
            }
            if let Some(x) = self.files.get_mut(pi) {
                *x = x.saturating_add(files);
            }
            if let Some(x) = self.deleted_files.get_mut(pi) {
                *x = x.saturating_add(dfiles);
            }
            if let Some(x) = self.size.get_mut(pi) {
                *x = x.saturating_add(size);
            }
            if let Some(x) = self.deleted_size.get_mut(pi) {
                *x = x.saturating_add(dsize);
            }
        }
    }

    /// The files whose data intersects `set` (H1).
    pub(crate) fn files_intersecting(&self, set: &RangeSet) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .extents
            .iter()
            .filter(|&&(a, b, _)| set.intersects(a..b))
            .map(|&(_, _, n)| n)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The extents intersecting `[a, b)`: indices into
    /// [`Health::extents`].
    pub(crate) fn extents_in(&self, a: u64, b: u64) -> impl Iterator<Item = usize> + '_ {
        // First extent whose running maximum end passes `a`.
        let lo = self.max_end.partition_point(|&e| e <= a);
        let hi = self.extents.partition_point(|&(s, _, _)| s < b);
        (lo..hi.max(lo)).filter(move |&k| self.extents.get(k).is_some_and(|&(_, e, _)| e > a))
    }

    /// Whether `offset` is the first data byte of the deleted file `node`.
    pub(crate) fn is_deleted_first_byte(&self, offset: u64, node: u32) -> bool {
        self.deleted_first.binary_search(&(offset, node)).is_ok()
    }

    /// Whether the geometry of catalog instance `inst` is validated.
    pub(crate) fn validated(&self, inst: u16) -> bool {
        self.validated
            .get(usize::from(inst))
            .copied()
            .unwrap_or(false)
    }
}

/// The flags of G1 that E2 and the records decide.
pub(crate) fn merged_flags(m: &Merged) -> FlagSet {
    let mut f = FlagSet::EMPTY;
    for (on, flag) in [
        (m.torn, Flag::TornRecord),
        (m.missing, Flag::MissingFragments),
        (m.estimated, Flag::SizeEstimated),
        (m.no_data, Flag::NoData),
        (m.compressed, Flag::Compressed),
        (m.encrypted, Flag::Encrypted),
        (m.placeholder, Flag::Placeholder),
        (m.sparse, Flag::Sparse),
    ] {
        if on {
            f.insert(flag);
        }
    }
    f
}

/// G2: every pair of files that share bytes is decided by [`Key`]. The
/// loser gets `overwritten`; the winner `overlap` when both are in use and
/// of the same instance kind; a full tie gives both `overlap`.
fn overlap_sweep(extents: &[(u64, u64, u32)], keys: &[Key], flags: &mut [u32]) {
    let key = |n: u32| -> Key {
        keys.get(usize::try_from(n).unwrap_or(usize::MAX))
            .copied()
            .unwrap_or((0, 0, 0, 0))
    };
    let mut mark = |n: u32, f: Flag| {
        if let Some(slot) = flags.get_mut(usize::try_from(n).unwrap_or(usize::MAX)) {
            *slot |= f.bit();
        }
    };
    let mut ends: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::new();
    let mut active: BTreeMap<(Key, u32), u32> = BTreeMap::new();
    let mut need_ow: BTreeSet<(Key, u32)> = BTreeSet::new();
    let mut need_ov: BTreeSet<(Key, u32)> = BTreeSet::new();
    let mut ow: BTreeSet<u32> = BTreeSet::new();
    let mut ov: BTreeSet<u32> = BTreeSet::new();
    for &(a, b, node) in extents {
        while let Some(&Reverse((e, x))) = ends.peek() {
            if e > a {
                break;
            }
            ends.pop();
            let k = (key(x), x);
            if let Some(c) = active.get_mut(&k) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    active.remove(&k);
                    need_ow.remove(&k);
                    need_ov.remove(&k);
                }
            }
        }
        let k = key(node);
        let me = (k, node);
        // Lower keys lose.
        let lower: Vec<(Key, u32)> = need_ow
            .range((Unbounded, Excluded((k, 0))))
            .copied()
            .collect();
        for x in lower {
            need_ow.remove(&x);
            ow.insert(x.1);
            mark(x.1, Flag::Overwritten);
        }
        // A higher key: this file loses.
        if active
            .range((Excluded((k, NONE)), Unbounded))
            .next()
            .is_some()
            && ow.insert(node)
        {
            mark(node, Flag::Overwritten);
        }
        // Full ties: both overlap. Whether another file ties is one look
        // (skipping this file's own earlier extents); the files to mark are
        // those not marked yet, each taken once.
        let any_tie = active
            .range((Included((k, 0)), Included((k, NONE))))
            .any(|(&(_, x), _)| x != node);
        if any_tie {
            let ties: Vec<(Key, u32)> = need_ov
                .range((Included((k, 0)), Included((k, NONE))))
                .copied()
                .filter(|&(_, x)| x != node)
                .collect();
            for x in ties {
                need_ov.remove(&x);
                ov.insert(x.1);
                mark(x.1, Flag::Overlap);
            }
            if ov.insert(node) {
                mark(node, Flag::Overlap);
            }
        }
        // The winner of two in-use files of one instance kind: overlap.
        if k.1 == 1 {
            let same_lo = ((k.0, 1, 0, 0), 0);
            if active
                .range((Included(same_lo), Excluded((k, 0))))
                .next()
                .is_some()
                && ov.insert(node)
            {
                mark(node, Flag::Overlap);
            }
            let same_hi = ((k.0, 1, u64::MAX, u64::MAX), NONE);
            let winners: Vec<(Key, u32)> = need_ov
                .range((Excluded((k, NONE)), Included(same_hi)))
                .copied()
                .collect();
            for x in winners {
                need_ov.remove(&x);
                ov.insert(x.1);
                mark(x.1, Flag::Overlap);
            }
        }
        let c = active.entry(me).or_insert(0);
        *c = c.saturating_add(1);
        if !ow.contains(&node) {
            need_ow.insert(me);
        }
        if !ov.contains(&node) {
            need_ov.insert(me);
        }
        ends.push(Reverse((b, node)));
    }
}

/// The rating of a node with `flags` (G3); `None` for folders.
pub(crate) fn rating(
    kind: NodeKind,
    flags: FlagSet,
    validated: bool,
    end_verified: bool,
) -> Option<Rating> {
    if kind.is_folder() {
        return None;
    }
    Some(flags.rating(RatingContext {
        geometry_not_validated: !validated,
        carved_end_unverified: kind == NodeKind::Carved && !end_verified,
    }))
}

/// The rating context of a file (H2 plans).
pub(crate) fn context(kind: NodeKind, validated: bool, end_verified: bool) -> RatingContext {
    RatingContext {
        geometry_not_validated: !validated,
        carved_end_unverified: kind == NodeKind::Carved && !end_verified,
    }
}

/// Builds the carved section (I3, I4) with ids from `base`: `[Carved
/// files]` under the main root, a folder per extension, the files.
pub(crate) fn carved_section(
    tree: &Tree,
    instances: &[CatalogInstance],
    health: &Health,
    store: &ScanStore,
    base: u32,
    entries: &[CarvedEntry],
) -> Result<CarvedSection, BuildError> {
    let mut sec = CarvedSection {
        base,
        ..CarvedSection::default()
    };
    let Some(main) = instances.iter().find(|i| i.kind == InstanceKind::Main) else {
        return Ok(sec);
    };
    if entries.is_empty() {
        return Ok(sec);
    }
    let main_inst = u16::try_from(main.id).unwrap_or(0);
    let mut sorted: Vec<&CarvedEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| (a.ext.as_str(), a.offset).cmp(&(b.ext.as_str(), b.offset)));
    sorted.dedup_by(|a, b| a.offset == b.offset && a.ext == b.ext);
    let id_at = |k: usize| -> Result<u32, BuildError> {
        u32::try_from(k)
            .ok()
            .and_then(|k| base.checked_add(k))
            .filter(|&v| v < NONE)
            .ok_or(BuildError::TooManyNodes)
    };
    let mut fold = String::new();
    let mut push = |sec: &mut CarvedSection,
                    parent: u32,
                    kind: NodeKind,
                    name: String,
                    e: Option<CarvedEntry>|
     -> Result<u32, BuildError> {
        let id = id_at(sec.nodes.len())?;
        let t = u32::try_from(sec.texts.len()).map_err(|_| BuildError::TooManyNodes)?;
        let off = u32::try_from(fold.len()).map_err(|_| BuildError::TooManyNodes)?;
        fold.push_str(&crate::catalog::tree::fold(&name));
        fold.push('\0');
        sec.texts.push(name);
        sec.nodes.push(NodeRec {
            parent,
            end: id.saturating_add(1),
            rec: NONE,
            fold: off,
            name: NameSrc::Text(t).encode(),
            inst: main_inst,
            kind,
            bits: 0,
        });
        sec.entries.push(e);
        Ok(id)
    };
    let root = push(
        &mut sec,
        main.root.0,
        NodeKind::Virtual,
        CARVED_FILES.to_string(),
        None,
    )?;
    let mut k = 0usize;
    while let Some(first) = sorted.get(k) {
        let ext = first.ext.clone();
        let folder = push(&mut sec, root, NodeKind::Virtual, ext.clone(), None)?;
        while let Some(e) = sorted.get(k) {
            if e.ext != ext {
                break;
            }
            push(
                &mut sec,
                folder,
                NodeKind::Carved,
                e.name(),
                Some((*e).clone()),
            )?;
            k = k.saturating_add(1);
        }
        let end = id_at(sec.nodes.len())?;
        if let Some(n) = sec
            .nodes
            .get_mut(usize::try_from(folder.saturating_sub(base)).unwrap_or(usize::MAX))
        {
            n.end = end;
        }
    }
    let end = id_at(sec.nodes.len())?;
    if let Some(n) = sec.nodes.first_mut() {
        n.end = end;
    }
    sec.fold = fold;
    carved_health(&mut sec, tree, health, store, main.geometry.validated);
    Ok(sec)
}

/// The carved section of `old` with health recomputed (`geometry_set`).
pub(crate) fn rehealth_carved(
    old: &CarvedSection,
    tree: &Tree,
    instances: &[CatalogInstance],
    health: &Health,
    store: &ScanStore,
) -> CarvedSection {
    let mut sec = CarvedSection {
        base: old.base,
        nodes: old.nodes.clone(),
        entries: old.entries.clone(),
        texts: old.texts.clone(),
        fold: old.fold.clone(),
        ..CarvedSection::default()
    };
    let validated = instances
        .iter()
        .find(|i| i.kind == InstanceKind::Main)
        .is_some_and(|m| m.geometry.validated);
    carved_health(&mut sec, tree, health, store, validated);
    sec
}

fn carved_health(
    sec: &mut CarvedSection,
    _tree: &Tree,
    health: &Health,
    store: &ScanStore,
    _validated: bool,
) {
    let n = sec.nodes.len();
    sec.flags = vec![0; n];
    sec.size = vec![0; n];
    sec.files = vec![0; n];
    sec.deleted_files = vec![0; n];
    sec.deleted_size = vec![0; n];
    let mut overwrites: Vec<u32> = Vec::new();
    for (i, e) in sec.entries.iter().enumerate() {
        let Some(e) = e else { continue };
        let range: Range<u64> = e.offset..e.offset.saturating_add(e.len);
        let mut f = FlagSet::EMPTY;
        if health.overwritten.intersects(range.clone()) {
            f.insert(Flag::Overwritten);
        }
        if store.bad_ranges().intersects(range.clone()) {
            f.insert(Flag::BadSectors);
        }
        if let Some(slot) = sec.flags.get_mut(i) {
            *slot = f.bits();
        }
        if let Some(slot) = sec.size.get_mut(i) {
            *slot = e.len;
        }
        // I4: a carved file inside a deleted file's extents makes that
        // file overwritten, unless it is that file's own content: it
        // starts at the file's first data byte (the run at VCN 0). A
        // carved file at the start of a later fragment is another file.
        for k in health.extents_in(range.start, range.end) {
            let Some(&(_, _, node)) = health.extents.get(k) else {
                continue;
            };
            let deleted = health
                .flags
                .get(usize::try_from(node).unwrap_or(usize::MAX))
                .is_some_and(|&b| b & Flag::Deleted.bit() != 0);
            if deleted && !health.is_deleted_first_byte(range.start, node) {
                overwrites.push(node);
            }
        }
    }
    overwrites.sort_unstable();
    overwrites.dedup();
    sec.overwrites = overwrites;
    // Aggregates: files then their folders (files follow their folder).
    for i in (0..n).rev() {
        let Some(node) = sec.nodes.get(i).copied() else {
            continue;
        };
        let (files, size) = if node.kind.is_file() {
            (1u32, sec.size.get(i).copied().unwrap_or(0))
        } else {
            (
                sec.files.get(i).copied().unwrap_or(0),
                sec.size.get(i).copied().unwrap_or(0),
            )
        };
        let Some(pi) = node
            .parent
            .checked_sub(sec.base)
            .and_then(|p| usize::try_from(p).ok())
            .filter(|&p| p < n)
        else {
            continue;
        };
        if let Some(x) = sec.files.get_mut(pi) {
            *x = x.saturating_add(files);
        }
        if let Some(x) = sec.size.get_mut(pi) {
            *x = x.saturating_add(size);
        }
    }
    let _ = bits::DELETED;
}
