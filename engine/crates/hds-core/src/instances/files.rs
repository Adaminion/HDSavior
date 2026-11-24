//! Files of one instance as far as instances and geometry need them: base
//! and extension records joined by ALGORITHMS E1, `$DATA` and
//! `$INDEX_ALLOCATION` extents merged by E2. The catalog builds the full
//! version (names, parents, flags); this one keeps only what D7 and F
//! look at, and allocates per group only the runs that are asked for.

use hds_ntfs::{FileRef, FixupStatus, Namespace, Run, REPARSE_TAG_WOF};

use crate::store::{DataExtent, NonResidentExtent, RecordId, ScanStore};

/// One file or directory: a base record (or, for an orphan group, its
/// first extension record) and its extension records.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Group {
    pub base: RecordId,
    pub number: u32,
    pub orphan: bool,
    ext_start: u32,
    ext_len: u32,
}

/// The groups of a set of records with distinct numbers.
#[derive(Debug, Default)]
pub(crate) struct GroupIndex {
    pub groups: Vec<Group>,
    exts: Vec<RecordId>,
}

impl GroupIndex {
    /// Groups `members` (record number, record) sorted by number with
    /// distinct numbers, by ALGORITHMS E1.
    pub fn build(store: &ScanStore, members: &[(u32, RecordId)]) -> GroupIndex {
        let find = |n: u64| -> Option<RecordId> {
            let n = u32::try_from(n).ok()?;
            members
                .binary_search_by_key(&n, |&(m, _)| m)
                .ok()
                .and_then(|k| members.get(k))
                .map(|&(_, id)| id)
        };
        // (key of the group: base number, orphan flag, base reference), ext
        let mut attached: Vec<(u32, RecordId)> = Vec::new();
        let mut orphans: Vec<(u64, u32, RecordId)> = Vec::new();
        for &(num, id) in members {
            let Some(x) = store.record(id) else { continue };
            if !x.is_extension() {
                continue;
            }
            let bref = x.base();
            let base = find(bref.number())
                .and_then(|b| store.record(b).map(|r| (b, r)))
                .filter(|(_, r)| !r.is_extension());
            let mut matched = None;
            if let Some((b, br)) = base {
                let both_in_use =
                    x.is_in_use() && br.is_in_use() && br.sequence() == bref.sequence();
                let both_free = !x.is_in_use()
                    && !br.is_in_use()
                    && br.has_attributes()
                    && (br.sequence() == bref.sequence()
                        || br.sequence() == hds_ntfs::next_seq(bref.sequence()));
                if both_in_use || both_free {
                    matched = Some(b);
                } else if !x.is_in_use() && br.is_in_use() {
                    continue; // a leftover
                }
            }
            match matched {
                Some(b) => {
                    let bn = store
                        .record(b)
                        .and_then(|r| r.record_number())
                        .unwrap_or_else(|| u32::try_from(bref.number()).unwrap_or(u32::MAX));
                    attached.push((bn, id));
                }
                None => orphans.push((bref.raw(), num, id)),
            }
        }
        attached.sort_by_key(|&(bn, id)| (bn, id));
        orphans.sort_by_key(|&(r, n, id)| (r, n, id));
        let mut idx = GroupIndex::default();
        let mut a = 0usize;
        for &(num, id) in members {
            let Some(r) = store.record(id) else { continue };
            if r.is_extension() {
                continue;
            }
            let start = idx.exts.len();
            while let Some(&(bn, x)) = attached.get(a) {
                if bn < num {
                    a = a.saturating_add(1);
                    continue;
                }
                if bn > num {
                    break;
                }
                idx.exts.push(x);
                a = a.saturating_add(1);
            }
            idx.push(id, num, false, start);
        }
        let mut k = 0usize;
        while let Some(&(raw, _, first)) = orphans.get(k) {
            let number = u32::try_from(FileRef::from_raw(raw).number()).unwrap_or(u32::MAX);
            let start = idx.exts.len();
            k = k.saturating_add(1);
            while let Some(&(r2, _, x)) = orphans.get(k) {
                if r2 != raw {
                    break;
                }
                idx.exts.push(x);
                k = k.saturating_add(1);
            }
            idx.push(first, number, true, start);
        }
        idx
    }

    fn push(&mut self, base: RecordId, number: u32, orphan: bool, start: usize) {
        let ext_start = u32::try_from(start).unwrap_or(u32::MAX);
        let ext_len = u32::try_from(self.exts.len().saturating_sub(start)).unwrap_or(0);
        self.groups.push(Group {
            base,
            number,
            orphan,
            ext_start,
            ext_len,
        });
    }

    /// The extension records of a group.
    pub fn exts(&self, g: &Group) -> &[RecordId] {
        let a = usize::try_from(g.ext_start).unwrap_or(usize::MAX);
        let b = a.saturating_add(usize::try_from(g.ext_len).unwrap_or(0));
        self.exts.get(a..b).unwrap_or(&[])
    }

    /// All records of a group, base first.
    pub fn records(&self, g: &Group) -> impl Iterator<Item = RecordId> + '_ {
        std::iter::once(g.base).chain(self.exts(g).iter().copied())
    }
}

/// What D7 and F look at in one group.
#[derive(Debug, Clone, Default)]
pub(crate) struct Facts {
    pub in_use: bool,
    pub is_dir: bool,
    pub fixups_ok: bool,
    /// Non-resident unnamed `$DATA` extents exist.
    pub has_data: bool,
    /// The `$DATA` chain starts at VCN 0, has no hole or overlap, every
    /// runlist is complete, no `$DATA` is malformed, and the mapped
    /// clusters cover the real size (E2: no `missing_fragments`).
    pub complete: bool,
    pub sparse: bool,
    pub compressed: bool,
    pub encrypted: bool,
    /// First LCN of the VCN-0 extent (when its first run is not a hole).
    pub first_lcn: Option<u64>,
    /// Highest mapped cluster + 1 over the `$DATA` runs (0: none).
    pub end_cluster: u64,
    pub real_size: Option<u64>,
}

/// One `$DATA` or `$I30` run with its VCN.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VRun {
    pub vcn: u64,
    pub run: Run,
}

fn sorted_extents<'a>(mut v: Vec<NonResidentExtent<'a>>) -> Vec<NonResidentExtent<'a>> {
    v.sort_by_key(|e| e.start_vcn);
    v
}

/// The non-resident unnamed `$DATA` extents of a group.
fn data_extents<'a>(
    store: &'a ScanStore,
    idx: &GroupIndex,
    g: &Group,
) -> Vec<NonResidentExtent<'a>> {
    let mut v = Vec::new();
    for id in idx.records(g) {
        for e in store.data_extents(id) {
            if let DataExtent::NonResident(x) = e {
                v.push(x);
            }
        }
    }
    sorted_extents(v)
}

/// The runs of an extent list with their VCNs (each extent from its own
/// start VCN).
fn vruns(exts: &[NonResidentExtent<'_>]) -> Vec<VRun> {
    let mut out = Vec::new();
    for e in exts {
        let mut vcn = e.start_vcn;
        for run in e.runs() {
            out.push(VRun { vcn, run });
            match vcn.checked_add(run.length) {
                Some(v) => vcn = v,
                None => break,
            }
        }
    }
    out
}

/// The `$DATA` runs of a group with their VCNs.
pub(crate) fn data_runs(store: &ScanStore, idx: &GroupIndex, g: &Group) -> Vec<VRun> {
    vruns(&data_extents(store, idx, g))
}

/// The `$INDEX_ALLOCATION:$I30` runs of a group with their VCNs.
pub(crate) fn index_runs(store: &ScanStore, idx: &GroupIndex, g: &Group) -> Vec<VRun> {
    let mut v = Vec::new();
    for id in idx.records(g) {
        v.extend(store.index_allocation_extents(id));
    }
    vruns(&sorted_extents(v))
}

/// Maps byte `pos` of a stream through `runs` to a volume byte (LCN x
/// cluster size + offset in the cluster); `None` for a hole or unmapped.
pub(crate) fn map_pos(runs: &[VRun], cs: u64, pos: u64) -> Option<u64> {
    let vcn = pos.checked_div(cs)?;
    let within = pos.checked_rem(cs)?;
    let r = runs
        .iter()
        .find(|r| r.vcn <= vcn && r.vcn.checked_add(r.run.length).is_some_and(|e| vcn < e))?;
    let lcn = r.run.lcn?.checked_add(vcn.checked_sub(r.vcn)?)?;
    lcn.checked_mul(cs)?.checked_add(within)
}

/// Whether a group is a directory (E3): header flag 0x2 on the base, or a
/// name with the directory flag.
pub(crate) fn is_directory(store: &ScanStore, id: RecordId) -> bool {
    store.record(id).is_some_and(|r| r.is_directory()) || store.names(id).any(|n| n.is_directory())
}

/// The facts of one group.
pub(crate) fn facts(store: &ScanStore, idx: &GroupIndex, g: &Group) -> Facts {
    let Some(base) = store.record(g.base) else {
        return Facts::default();
    };
    let mut f = Facts {
        in_use: base.is_in_use(),
        is_dir: is_directory(store, g.base),
        fixups_ok: true,
        ..Facts::default()
    };
    let mut malformed = false;
    let mut wof = false;
    for id in idx.records(g) {
        let Some(r) = store.record(id) else { continue };
        if r.fixup() != FixupStatus::Ok {
            f.fixups_ok = false;
        }
        if r.damage().data_malformed {
            malformed = true;
        }
        if r.has_wof_stream() {
            wof = true;
        }
    }
    if matches!(base.reparse(), crate::store::Reparse::Tag(REPARSE_TAG_WOF)) {
        wof = true;
    }
    let exts = data_extents(store, idx, g);
    f.has_data = !exts.is_empty();
    let mut complete = !exts.is_empty() && !malformed;
    let mut expected = 0u64;
    let mut saw_v0 = false;
    for e in &exts {
        if e.start_vcn != expected {
            complete = false;
        }
        if !e.runlist_status.is_complete() {
            complete = false;
        }
        if e.is_compressed() {
            f.compressed = true;
        }
        if e.is_encrypted() {
            f.encrypted = true;
        }
        if e.has_sparse_flag() {
            f.sparse = true;
        }
        if e.start_vcn == 0 {
            saw_v0 = true;
            if let Some(s) = e.sizes {
                f.real_size = Some(s.real);
            }
        }
        let mut first = true;
        let mut clusters = 0u64;
        for run in e.runs() {
            clusters = clusters.saturating_add(run.length);
            match run.lcn {
                Some(l) => {
                    f.end_cluster = f.end_cluster.max(l.saturating_add(run.length));
                    if first && e.start_vcn == 0 {
                        f.first_lcn = Some(l);
                    }
                }
                None => f.sparse = true,
            }
            first = false;
        }
        expected = e.start_vcn.saturating_add(clusters);
    }
    if !saw_v0 {
        complete = false;
    }
    if let (Some(real), Some(cs)) = (f.real_size, cluster_hint(&exts)) {
        let needed = real.div_ceil(cs);
        if expected < needed {
            complete = false;
        }
    }
    f.complete = complete;
    f.compressed |= wof;
    f
}

/// The cluster size implied by the VCN-0 extent (allocated size / clusters),
/// used to check that the chain covers the real size.
fn cluster_hint(exts: &[NonResidentExtent<'_>]) -> Option<u64> {
    let e = exts.iter().find(|e| e.start_vcn == 0)?;
    let s = e.sizes?;
    let clusters = u64::try_from(e.last_vcn).ok()?.checked_add(1)?;
    let cs = s.allocated.checked_div(clusters)?;
    (cs.is_power_of_two() && (512..=(2 << 20)).contains(&cs)).then_some(cs)
}

fn ns_rank(ns: Namespace) -> u8 {
    match ns {
        Namespace::Win32 | Namespace::Win32AndDos => 0,
        Namespace::Posix => 1,
        Namespace::Dos => 2,
        #[allow(unreachable_patterns)]
        _ => 3,
    }
}

/// The extension (lower case, no dot) of the best name of a group (E4
/// namespace preference, then the lowest attribute id).
pub(crate) fn best_extension(store: &ScanStore, idx: &GroupIndex, g: &Group) -> Option<String> {
    let name = best_name(store, idx, g)?;
    let dot = name.rfind('.')?;
    let ext = name.get(dot.saturating_add(1)..)?;
    if ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// The best name of a group (E4 namespace preference, then the lowest
/// attribute id), lossily converted.
pub(crate) fn best_name(store: &ScanStore, idx: &GroupIndex, g: &Group) -> Option<String> {
    let mut best: Option<(u8, u16, String)> = None;
    for id in idx.records(g) {
        for n in store.names(id) {
            let key = (ns_rank(n.namespace), n.attr_id);
            if best.as_ref().is_none_or(|(a, b, _)| key < (*a, *b)) {
                best = Some((key.0, key.1, n.name.to_string_lossy()));
            }
        }
    }
    best.map(|(_, _, s)| s)
}
