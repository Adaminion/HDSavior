//! ALGORITHMS E1 (base and extension records) and E2 (merging the
//! unnamed `$DATA` extents of a group), per instance.

use hds_ntfs::{FileRef, FixupStatus, REPARSE_TAG_WOF};

use crate::instances::Layout;
use crate::store::{DataExtent, NonResidentExtent, RecordId, Reparse, ScanStore};

/// No record number (strays: a record without a number in its header).
pub(crate) const NO_NUMBER: u32 = u32::MAX;

/// Base records with one number an extension is compared with (the strays
/// can hold many records of one number; a bound against crafted input).
const MAX_BASE_CANDIDATES: usize = 64;

/// Reparse tag of a deduplication stub (G1 `placeholder`).
const TAG_DEDUP: u32 = 0x8000_0013;
/// Cloud placeholder tags: `(tag & 0xFFFF0FFF) == 0x9000001A` (G1).
const TAG_CLOUD: u32 = 0x9000_001A;
const TAG_CLOUD_MASK: u32 = 0xFFFF_0FFF;

/// One file or directory of an instance: a base record (for an orphan
/// group: its first extension record) and its extension records.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Group {
    /// The base record (or the first extension record of an orphan group).
    pub base: RecordId,
    /// Effective record number of the base ([`NO_NUMBER`] when unknown);
    /// for an orphan group the number its extensions name.
    pub number: u32,
    /// An orphan group (E7): extension records whose base was not found.
    pub orphan: bool,
    ext_start: u32,
    ext_len: u32,
}

/// The groups of one instance.
#[derive(Debug, Default)]
pub(crate) struct Groups {
    pub groups: Vec<Group>,
    exts: Vec<RecordId>,
}

impl Groups {
    /// Groups `members` ((effective number, record) sorted by number; for
    /// strays numbers may repeat and unnumbered records carry
    /// [`NO_NUMBER`]) by the rules of E1. A base record marked
    /// `older_generation` by D2 is never a base target. Extension records
    /// numbered below 24 are system records (E9) and are left out.
    pub fn build(store: &ScanStore, layout: &Layout, members: &[(u32, RecordId)]) -> Groups {
        // Bases: (number, record), in member order (sorted by number).
        let mut bases: Vec<(u32, RecordId)> = Vec::new();
        for &(n, id) in members {
            if store.record(id).is_some_and(|r| !r.is_extension()) {
                bases.push((n, id));
            }
        }
        let mut attached: Vec<(u32, RecordId)> = Vec::new(); // (base index, ext)
        let mut orphans: Vec<(u64, RecordId)> = Vec::new(); // (base ref raw, ext)
        for &(n, id) in members {
            let Some(x) = store.record(id) else { continue };
            if !x.is_extension() {
                continue;
            }
            if n < 24 {
                continue; // E9
            }
            let bref = x.base();
            let target = u32::try_from(bref.number())
                .ok()
                .filter(|&t| t != NO_NUMBER);
            let mut matched: Option<usize> = None;
            let mut base_in_use = false;
            if let Some(t) = target {
                let lo = bases.partition_point(|&(m, _)| m < t);
                let mut k = lo;
                while let Some(&(m, b)) = bases.get(k) {
                    if m != t || k.saturating_sub(lo) >= MAX_BASE_CANDIDATES {
                        break;
                    }
                    if let Some(br) = store.record(b) {
                        base_in_use |= br.is_in_use();
                        let marked = layout.is_older_generation(b);
                        let both_in_use =
                            x.is_in_use() && br.is_in_use() && br.sequence() == bref.sequence();
                        let both_free = !x.is_in_use()
                            && !br.is_in_use()
                            && br.has_attributes()
                            && (br.sequence() == bref.sequence()
                                || br.sequence() == hds_ntfs::next_seq(bref.sequence()));
                        if !marked && (both_in_use || both_free) && matched.is_none() {
                            matched = Some(k);
                        }
                    }
                    k = k.saturating_add(1);
                }
            }
            match matched {
                Some(k) => attached.push((u32::try_from(k).unwrap_or(u32::MAX), id)),
                None if !x.is_in_use() && base_in_use => {} // a leftover (E1)
                None => orphans.push((bref.raw(), id)),
            }
        }
        attached.sort_unstable();
        orphans.sort_unstable();
        let mut out = Groups::default();
        let mut a = 0usize;
        for (k, &(n, id)) in bases.iter().enumerate() {
            let k = u32::try_from(k).unwrap_or(u32::MAX);
            let start = out.exts.len();
            while let Some(&(bk, x)) = attached.get(a) {
                if bk != k {
                    break;
                }
                out.exts.push(x);
                a = a.saturating_add(1);
            }
            out.push(id, n, false, start);
        }
        let mut i = 0usize;
        while let Some(&(raw, first)) = orphans.get(i) {
            let start = out.exts.len();
            i = i.saturating_add(1);
            while let Some(&(r2, x)) = orphans.get(i) {
                if r2 != raw {
                    break;
                }
                out.exts.push(x);
                i = i.saturating_add(1);
            }
            let bref = FileRef::from_raw(raw);
            let number = u32::try_from(bref.number()).unwrap_or(NO_NUMBER);
            out.push(first, number, true, start);
        }
        out
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

    /// The extension records of a group (for an orphan group: those after
    /// its first record).
    pub fn exts(&self, g: &Group) -> &[RecordId] {
        let a = usize::try_from(g.ext_start).unwrap_or(usize::MAX);
        let b = a.saturating_add(usize::try_from(g.ext_len).unwrap_or(0));
        self.exts.get(a..b).unwrap_or(&[])
    }
}

/// All records of a group, base first.
pub(crate) fn group_records(
    base: RecordId,
    exts: &[RecordId],
) -> impl Iterator<Item = RecordId> + '_ {
    std::iter::once(base).chain(exts.iter().copied())
}

/// Whether the group of `base` is a directory (E3): header flag 0x0002
/// on the base, or one of its names with `$FILE_NAME` flag 0x10000000.
pub(crate) fn is_directory(store: &ScanStore, base: RecordId, exts: &[RecordId]) -> bool {
    store.record(base).is_some_and(|r| r.is_directory())
        || group_records(base, exts).any(|id| store.names(id).any(|n| n.is_directory()))
}

/// One run of a merged stream, in VCN order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DataRun {
    /// First VCN of the run.
    pub vcn: u64,
    /// First LCN; `None` for a sparse run.
    pub lcn: Option<u64>,
    /// Length in clusters.
    pub len: u64,
}

/// Where a resident `$DATA` value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResidentAt {
    /// The record that holds it.
    pub record: RecordId,
    /// Offset of the value inside the record.
    pub value_offset: u32,
    /// Length of the value.
    pub value_len: u32,
}

/// A group's unnamed `$DATA` after E2, with the G1 facts that come from
/// the records alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Merged {
    /// Resident content, when the stream is resident.
    pub resident: Option<ResidentAt>,
    /// Non-resident runs in VCN order, overlaps removed.
    pub runs: Vec<DataRun>,
    /// Real size (estimated when `size_estimated`).
    pub size: u64,
    /// Initialized size.
    pub initialized: u64,
    /// Allocated size (0 when unknown).
    pub allocated: u64,
    /// E2 `missing_fragments`.
    pub missing: bool,
    /// E2 `size_estimated`.
    pub estimated: bool,
    /// E2 `no_data`.
    pub no_data: bool,
    /// G1 `compressed`.
    pub compressed: bool,
    /// G1 `encrypted`.
    pub encrypted: bool,
    /// G1 `sparse`.
    pub sparse: bool,
    /// G1 `placeholder`.
    pub placeholder: bool,
    /// G1 `torn_record`.
    pub torn: bool,
    /// A non-resident or resident unnamed `$DATA` exists in the group.
    pub has_data: bool,
}

impl Merged {
    /// Number of runs (data and sparse).
    pub fn run_count(&self) -> u64 {
        u64::try_from(self.runs.len()).unwrap_or(u64::MAX)
    }

    /// The first LCN of VCN 0, when it is mapped.
    pub fn first_lcn(&self) -> Option<u64> {
        self.runs.first().filter(|r| r.vcn == 0).and_then(|r| r.lcn)
    }
}

/// E2 for one group: merges the unnamed `$DATA` extents of `base` and
/// `exts` by start VCN, with cluster size `cs` for the checks that need it
/// (`cs` 0: unknown). Also gathers the flags of G1 that the records alone
/// decide (`torn_record`, `compressed`, `encrypted`, `sparse`,
/// `placeholder`).
pub(crate) fn merge(store: &ScanStore, base: RecordId, exts: &[RecordId], cs: u64) -> Merged {
    let mut m = Merged::default();
    let mut extents: Vec<(u64, usize, NonResidentExtent<'_>)> = Vec::new();
    let mut resident: Option<ResidentAt> = None;
    let mut malformed = false;
    let mut cut = false;
    let mut has_list = false;
    let mut wof = false;
    for (k, id) in group_records(base, exts).enumerate() {
        let Some(r) = store.record(id) else { continue };
        if r.fixup() != FixupStatus::Ok {
            m.torn = true;
        }
        let damage = r.damage();
        malformed |= damage.data_malformed;
        cut |= damage.attributes_cut;
        has_list |= !matches!(r.attribute_list(), crate::store::AttributeListState::Absent);
        wof |= r.has_wof_stream();
        match r.reparse() {
            Reparse::Tag(t) => {
                if t == REPARSE_TAG_WOF {
                    wof = true;
                }
                if t == TAG_DEDUP || t & TAG_CLOUD_MASK == TAG_CLOUD {
                    m.placeholder = true;
                }
            }
            Reparse::None | Reparse::Unknown => {}
        }
        for e in store.data_extents(id) {
            match e {
                DataExtent::Resident(d) => {
                    if d.flags & hds_ntfs::ATTR_FLAG_ENCRYPTED != 0 {
                        m.encrypted = true;
                    }
                    if resident.is_none() {
                        resident = Some(ResidentAt {
                            record: id,
                            value_offset: d.value_offset,
                            value_len: d.value_len,
                        });
                    }
                }
                DataExtent::NonResident(x) => extents.push((x.start_vcn, k, x)),
            }
        }
    }
    m.compressed = wof;
    m.has_data = resident.is_some() || !extents.is_empty();
    if extents.is_empty() {
        if let Some(r) = resident {
            m.resident = Some(r);
            m.size = u64::from(r.value_len);
            m.initialized = m.size;
            m.allocated = m.size;
        } else if has_list || malformed || cut {
            // The stream exists somewhere (attribute list) or its location
            // is unknown (a damaged record): never "no data".
            m.missing = true;
        } else {
            m.no_data = true;
        }
        return m;
    }
    extents.sort_by_key(|&(v, k, _)| (v, k));
    let mut expected = 0u64;
    let mut saw_v0 = false;
    let mut highest_end = 0u64; // highest mapped VCN + 1
    let mut first = true;
    for (start, _, e) in &extents {
        if e.is_compressed() {
            m.compressed = true;
        }
        if e.is_encrypted() {
            m.encrypted = true;
        }
        if e.has_sparse_flag() {
            m.sparse = true;
        }
        if !e.runlist_status.is_complete() {
            m.missing = true;
        }
        if *start == 0 {
            if !saw_v0 {
                if let Some(s) = e.sizes {
                    m.size = s.real;
                    m.initialized = s.initialized.min(s.real);
                    m.allocated = s.allocated;
                }
            }
            saw_v0 = true;
        }
        if first {
            if *start != 0 {
                m.missing = true;
            }
        } else if *start != expected {
            m.missing = true; // a hole or an overlap in the chain
        }
        first = false;
        let mut vcn = *start;
        let mut clusters = 0u64;
        for run in e.runs() {
            let Some(end) = vcn.checked_add(run.length) else {
                m.missing = true;
                break;
            };
            clusters = clusters.saturating_add(run.length);
            if run.lcn.is_none() {
                m.sparse = true;
            }
            // Overlapping VCNs keep the earlier extent's mapping.
            let from = vcn.max(m.runs.last().map_or(0, |r| r.vcn.saturating_add(r.len)));
            if from < end {
                let skip = from.saturating_sub(vcn);
                m.runs.push(DataRun {
                    vcn: from,
                    lcn: run.lcn.map(|l| l.saturating_add(skip)),
                    len: end.saturating_sub(from),
                });
            }
            highest_end = highest_end.max(end);
            vcn = end;
        }
        if e.last_vcn > 0 {
            let stated = u64::try_from(e.last_vcn)
                .ok()
                .and_then(|l| l.checked_add(1));
            if stated != start.checked_add(clusters) {
                m.missing = true;
            }
        }
        expected = start.saturating_add(clusters);
    }
    if malformed {
        m.missing = true;
    }
    if !saw_v0 {
        m.missing = true;
        m.estimated = true;
        m.size = highest_end.saturating_mul(cs);
        m.initialized = m.size;
    } else if cs > 0 {
        let mapped_from_zero = m
            .runs
            .iter()
            .scan(0u64, |next, r| {
                if r.vcn != *next {
                    return None;
                }
                *next = r.vcn.saturating_add(r.len);
                Some(*next)
            })
            .last()
            .unwrap_or(0);
        // NTFS keeps real <= allocated for an attribute that is not
        // compressed: a larger real size comes from a damaged record. It is
        // not believed: the size is capped at what is allocated and mapped
        // from VCN 0, and flagged as estimated with missing fragments.
        if !m.compressed && m.size > m.allocated {
            m.size = m.allocated.min(mapped_from_zero.saturating_mul(cs));
            m.initialized = m.initialized.min(m.size);
            m.estimated = true;
            m.missing = true;
        }
        let needed = m.size.div_ceil(cs);
        if mapped_from_zero < needed {
            m.missing = true;
        }
    }
    m
}
