//! Records: the classes of ALGORITHMS A3 and C3, the overwritten set of C5,
//! and the fragments and generation cuts of D1 to D3.
//!
//! Everything here works on the `FILE` records the manifest lists, the way
//! a scanner sees them: the record number is what the header claims (or the
//! positional number D1 gives an unnumbered record), not the slot the
//! builder put the record in.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::facts::{ArtifactState, RecordFact};
use crate::manifest::{
    ContradictionFact, ContradictionKind, CutFact, InstanceRole, Manifest, RecordClass,
};
use crate::ondisk::attr::FN_DIRECTORY;
use crate::ondisk::{next_seq, FileRef};
use crate::time;

use super::ASSUMED_SCAN_TIME;

/// The `$MFT` of a current volume, as ALGORITHMS C2 reads it.
#[derive(Clone, Debug)]
pub(crate) struct CurrentMft {
    pub instance: u32,
    pub record_size: u64,
    pub cluster_size: u64,
    pub volume_offset: u64,
    /// Creation time of its record 3 (`format_time` of C2).
    pub format_time: u64,
    /// `(absolute offset, first slot, slots)` of every extent.
    pub extents: Vec<(u64, u64, u64)>,
    /// Slots below the initialized size of `$MFT:$DATA`.
    pub initialized: u64,
    /// Slots its `$MFT:$BITMAP` marks, as `(first, count)`.
    pub live_slots: Vec<(u64, u64)>,
    /// `[start, end)` of its `$MFTMirr`.
    pub mirror: (u64, u64),
}

impl CurrentMft {
    /// The slot whose record starts at `offset`, if `offset` lies inside an
    /// extent below the initialized size.
    fn slot_at(&self, offset: u64) -> Option<u64> {
        self.extents.iter().find_map(|&(start, first, slots)| {
            let end = start + slots * self.record_size;
            (offset >= start && offset < end)
                .then(|| first + (offset - start) / self.record_size)
                .filter(|&slot| slot < self.initialized)
        })
    }

    /// Where record `number` lies: through the extent that holds it, or,
    /// beyond the last slot, on the grid of the extent of `near` (the same
    /// origin, as D1 sees it).
    fn offset_of(&self, number: u64, near: u64) -> Option<u64> {
        let inside = self.extents.iter().find_map(|&(start, first, slots)| {
            (first..first + slots)
                .contains(&number)
                .then(|| start + (number - first) * self.record_size)
        });
        inside.or_else(|| {
            let &(start, first, _) = self.extents.iter().find(|&&(start, _, slots)| {
                near >= start && near < start + slots * self.record_size
            })?;
            (number >= first).then(|| start + (number - first) * self.record_size)
        })
    }

    fn slot_live(&self, slot: u64) -> bool {
        self.live_slots
            .iter()
            .any(|&(first, count)| (first..first + count).contains(&slot))
    }

    /// Origins of its extents, as `(record size, origin)`.
    pub fn origins(&self) -> Vec<(u64, i64)> {
        self.extents
            .iter()
            .map(|&(start, first, _)| {
                (
                    self.record_size,
                    start as i64 - (first * self.record_size) as i64,
                )
            })
            .collect()
    }
}

/// The current volumes of the manifest (instances whose role is `Current`).
pub(crate) fn current_mfts(m: &Manifest) -> Vec<CurrentMft> {
    m.instances
        .iter()
        .filter(|i| i.role == InstanceRole::Current)
        .map(|i| {
            let g = &i.geometry;
            let cs = g.cluster_size as u64;
            let mirror = g.volume_offset + g.mftmirr_lcn * cs;
            CurrentMft {
                instance: i.id,
                record_size: g.record_size as u64,
                cluster_size: cs,
                volume_offset: g.volume_offset,
                format_time: i.format_time,
                extents: g
                    .mft_extents
                    .iter()
                    .map(|e| (e.offset, e.first_record, e.records))
                    .collect(),
                initialized: g.mft_initialized_records,
                live_slots: i.live_slots.clone(),
                mirror: (mirror, mirror + g.mftmirr_clusters * cs),
            }
        })
        .collect()
}

/// The record time (ALGORITHMS, vocabulary): the latest of the
/// `$STANDARD_INFORMATION` MFT-change time and the four times of every
/// `$FILE_NAME` of the record; `None` when it has neither.
pub(crate) fn record_time(r: &RecordFact) -> Option<u64> {
    let si = r.standard_information.map(|(times, _, _)| times[2]);
    let names = r
        .names
        .iter()
        .flat_map(|n| n.times)
        .max()
        .filter(|&t| t != 0);
    match (si, names) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// The entry time (ALGORITHMS, vocabulary): the latest of the four times of
/// an index entry's `$FILE_NAME` copy.
pub(crate) fn entry_time(times: &[u64; 4]) -> u64 {
    times.iter().copied().max().unwrap_or(0)
}

/// A plausible record time (D2 "timed"): not 0, not before 1980, not
/// more than a day after the scan.
pub(crate) fn plausible(t: u64) -> bool {
    t != 0 && t >= time::ymd(1980, 1, 1) && t <= ASSUMED_SCAN_TIME + time::DAY
}

pub(crate) fn timed(r: &RecordFact) -> Option<u64> {
    record_time(r).filter(|&t| plausible(t))
}

pub(crate) fn in_use(r: &RecordFact) -> bool {
    r.flags & 1 != 0
}

/// ALGORITHMS E3: header flag 0x0002 or a name with the directory flag.
pub(crate) fn is_directory(r: &RecordFact) -> bool {
    r.flags & 2 != 0 || r.names.iter().any(|n| n.flags & FN_DIRECTORY != 0)
}

/// The reference rule: `(N, s)` resolves to `r` (whose number is `N`).
pub(crate) fn resolves(r: &RecordFact, seq: u16) -> bool {
    if in_use(r) {
        r.sequence == seq
    } else {
        r.sequence == next_seq(seq)
    }
}

/// Classifies every record (ALGORITHMS A3 and C3). `None` for records whose
/// bytes the manifest does not describe.
pub(crate) fn classify(m: &Manifest, current: &[CurrentMft]) -> Vec<Option<RecordClass>> {
    let by_offset: HashMap<u64, usize> = m
        .records
        .iter()
        .enumerate()
        .map(|(i, r)| (r.offset, i))
        .collect();
    let mut classes: Vec<Option<RecordClass>> = vec![None; m.records.len()];
    // extension records in free slots whose class depends on their base
    let mut pending: Vec<(usize, usize)> = Vec::new();
    let (range_start, range_end) = m.scan_range;
    for (i, r) in m.records.iter().enumerate() {
        // damaged records are not described; records outside the session
        // range are never read
        if r.state != ArtifactState::Intact
            || r.offset < range_start
            || r.offset + r.size as u64 > range_end
        {
            continue;
        }
        let blocks = (r.size / 512) as usize;
        let never_used = !in_use(r) && r.attr_types.is_empty();
        let all_torn = blocks > 0 && r.torn_blocks.len() >= blocks;
        if never_used || all_torn {
            classes[i] = Some(RecordClass::Dropped);
            continue;
        }
        let mut class = RecordClass::Old;
        for (k, cv) in current.iter().enumerate() {
            if r.offset >= cv.mirror.0 && r.offset < cv.mirror.1 {
                class = RecordClass::Live;
                break;
            }
            let Some(slot) = cv.slot_at(r.offset) else {
                continue;
            };
            class = if cv.slot_live(slot) {
                RecordClass::Live
            } else if let Some(t) = r.standard_information.and_then(|_| record_time(r)) {
                // C3: a record with a $STANDARD_INFORMATION, by its record time
                if t >= cv.format_time {
                    RecordClass::PostFormat
                } else {
                    RecordClass::Old
                }
            } else if in_use(r) {
                RecordClass::Old
            } else {
                pending.push((i, k));
                RecordClass::PostFormat
            };
            break;
        }
        classes[i] = Some(class);
    }
    // C3, last case: a free extension record is old only if its base
    // reference resolves to an old record of the same $MFT.
    let decided = classes.clone();
    for (i, k) in pending {
        let r = &m.records[i];
        let cv = &current[k];
        let base = r.base;
        let resolved = base != FileRef::NONE
            && cv
                .offset_of(base.number, r.offset)
                .and_then(|at| by_offset.get(&at))
                .is_some_and(|&b| {
                    decided[b] == Some(RecordClass::Old)
                        && m.records[b].number.is_none_or(|n| n == base.number)
                        && resolves(&m.records[b], base.seq)
                });
        if resolved {
            classes[i] = Some(RecordClass::Old);
        }
    }
    classes
}

/// For every record, the current volume whose `$MFT` (below the initialized
/// size) or `$MFTMirr` holds it.
pub(crate) fn current_holder(m: &Manifest, current: &[CurrentMft]) -> Vec<Option<usize>> {
    m.records
        .iter()
        .map(|r| {
            current.iter().position(|cv| {
                (r.offset >= cv.mirror.0 && r.offset < cv.mirror.1)
                    || cv.slot_at(r.offset).is_some()
            })
        })
        .collect()
}

/// ALGORITHMS C5: live-allocated ranges of the current volumes plus the
/// non-resident extents (unnamed `$DATA`, `$INDEX_ALLOCATION:$I30`) of
/// `post_format` records, mapped with the current volume's geometry.
pub(crate) fn overwritten_set(
    m: &Manifest,
    current: &[CurrentMft],
    classes: &[Option<RecordClass>],
) -> Vec<(u64, u64)> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for cv in current {
        ranges.extend(
            m.instances[cv.instance as usize]
                .live_allocated
                .iter()
                .copied(),
        );
    }
    let holders = current_holder(m, current);
    for (i, r) in m.records.iter().enumerate() {
        if classes[i] != Some(RecordClass::PostFormat) {
            continue;
        }
        let Some(k) = holders[i] else { continue };
        let cv = &current[k];
        for (lcn, len) in cluster_runs(r) {
            ranges.push((
                cv.volume_offset + lcn * cv.cluster_size,
                len * cv.cluster_size,
            ));
        }
    }
    crate::disk::merge_ranges(ranges)
}

/// `(lcn, clusters)` of every allocated run of the record's unnamed `$DATA`
/// and `$INDEX_ALLOCATION:$I30` extents.
pub(crate) fn cluster_runs(r: &RecordFact) -> Vec<(u64, u64)> {
    let data = r.data.iter().filter(|s| s.name.is_empty());
    let index = r.index_allocation.iter().filter(|s| s.name == "$I30");
    data.chain(index)
        .filter_map(|s| s.non_resident.as_ref())
        .flat_map(|x| {
            x.runs
                .iter()
                .filter_map(|run| run.lcn.map(|lcn| (lcn, run.len)))
        })
        .filter(|&(_, len)| len > 0)
        .collect()
}

/// A contradiction before reduction: `(lo, hi, kind)` by record number.
type RawContradiction = (u64, u64, ContradictionKind);

/// One MFT fragment of old records (D1) with its generation analysis (D2).
#[derive(Clone, Debug, Default)]
pub(crate) struct Fragment {
    pub record_size: u64,
    pub origin: i64,
    /// `number → record index`, numbered and positional members.
    pub members: BTreeMap<u64, usize>,
    pub contradictions: Vec<ContradictionFact>,
    pub cuts: Vec<CutFact>,
    /// Record indexes marked `older_generation` by D2.
    pub marked: BTreeSet<usize>,
    /// A record the manifest cannot describe lies on the fragment's grid:
    /// what D2 concludes here is not certain.
    pub uncertain: bool,
}

impl Fragment {
    /// The segment of record number `number`: the number of kept cuts at or
    /// below it.
    pub fn segment(&self, number: u64) -> usize {
        self.cuts.iter().filter(|c| c.position <= number).count()
    }

    /// Number of segments.
    pub fn segments(&self) -> usize {
        self.cuts.len() + 1
    }
}

/// What D1 and D2 make of the old records.
#[derive(Clone, Debug, Default)]
pub(crate) struct Analysis {
    pub fragments: Vec<Fragment>,
    /// Per record: the number a scanner works with (claimed, or positional
    /// for an unnumbered record that joins a fragment).
    pub numbers: Vec<Option<u64>>,
    /// Per record: its fragment.
    pub fragment_of: Vec<Option<usize>>,
    /// Unnumbered old records on no fragment's grid: strays (D1).
    pub unplaced: Vec<usize>,
}

impl Analysis {
    /// `true` when D2 marks the record `older_generation`; `None` when the
    /// fragment's analysis is not certain or the record is not old.
    pub fn marked(&self, record: usize) -> Option<bool> {
        let f = &self.fragments[self.fragment_of[record]?];
        (!f.uncertain).then(|| f.marked.contains(&record))
    }
}

/// D1 and D2 over the records of class `Old`.
pub(crate) fn analyse(m: &Manifest, classes: &[Option<RecordClass>]) -> Analysis {
    let n = m.records.len();
    let mut analysis = Analysis {
        numbers: vec![None; n],
        fragment_of: vec![None; n],
        ..Analysis::default()
    };
    let mut index: BTreeMap<(u64, i64), usize> = BTreeMap::new();
    let old = |i: usize| classes[i] == Some(RecordClass::Old);
    // numbered records
    for (i, r) in m.records.iter().enumerate().filter(|(i, _)| old(*i)) {
        let Some(number) = r.number else { continue };
        let size = r.size as u64;
        let origin = r.offset as i64 - (number * size) as i64;
        let f = *index.entry((size, origin)).or_insert_with(|| {
            analysis.fragments.push(Fragment {
                record_size: size,
                origin,
                ..Fragment::default()
            });
            analysis.fragments.len() - 1
        });
        analysis.fragments[f].members.insert(number, i);
        analysis.numbers[i] = Some(number);
        analysis.fragment_of[i] = Some(f);
    }
    // unnumbered records: the positional number of a fragment whose grid
    // they lie on, within 4096 records of its number span
    let spans: Vec<(u64, u64)> = analysis
        .fragments
        .iter()
        .map(|f| {
            (
                *f.members.keys().next().expect("not empty"),
                *f.members.keys().last().expect("not empty"),
            )
        })
        .collect();
    for (i, r) in m
        .records
        .iter()
        .enumerate()
        .filter(|(i, r)| old(*i) && r.number.is_none())
    {
        let size = r.size as u64;
        let place = analysis.fragments.iter().enumerate().find_map(|(f, frag)| {
            let delta = r.offset as i64 - frag.origin;
            if frag.record_size != size || delta < 0 || delta % size as i64 != 0 {
                return None;
            }
            let pos = (delta / size as i64) as u64;
            let (lo, hi) = spans[f];
            (pos + 4096 >= lo && pos <= hi + 4096).then_some((f, pos))
        });
        match place {
            Some((f, pos)) => {
                analysis.fragments[f].members.insert(pos, i);
                analysis.numbers[i] = Some(pos);
                analysis.fragment_of[i] = Some(f);
            }
            None => analysis.unplaced.push(i),
        }
    }
    // records the manifest cannot describe that a scanner would put into a
    // fragment make that fragment's analysis uncertain
    for r in m
        .records
        .iter()
        .filter(|r| r.state == ArtifactState::Damaged)
    {
        if let Some(number) = r.number {
            let origin = r.offset as i64 - (number * r.size as u64) as i64;
            if let Some(&f) = index.get(&(r.size as u64, origin)) {
                analysis.fragments[f].uncertain = true;
            }
        }
    }
    for f in &mut analysis.fragments {
        generation_cuts(m, f);
    }
    analysis
}

/// The contradictions of D2 inside one fragment, before the per-`hi`
/// reduction.
fn contradictions(m: &Manifest, f: &Fragment) -> Vec<RawContradiction> {
    let eligible = |i: usize| {
        let r = &m.records[i];
        in_use(r) && r.torn_blocks.is_empty() && r.state == ArtifactState::Intact
    };
    let mut out: Vec<RawContradiction> = Vec::new();
    // (A) shared cluster numbers
    let mut runs: Vec<(u64, u64, u64)> = Vec::new(); // (first lcn, end lcn, number)
    for (&number, &i) in &f.members {
        if eligible(i) {
            runs.extend(
                cluster_runs(&m.records[i])
                    .into_iter()
                    .map(|(lcn, len)| (lcn, lcn + len, number)),
            );
        }
    }
    runs.sort_unstable();
    let mut active: Vec<(u64, u64)> = Vec::new(); // (end lcn, number)
    for &(start, end, number) in &runs {
        active.retain(|&(e, _)| e > start);
        for &(_, other) in &active {
            if other != number {
                out.push((
                    other.min(number),
                    other.max(number),
                    ContradictionKind::SharedCluster,
                ));
            }
        }
        active.push((end, number));
    }
    // (B) a parent or base reference to a slot whose in-use record does not fit
    for (&number, &i) in &f.members {
        if !eligible(i) {
            continue;
        }
        let r = &m.records[i];
        let parents = r
            .names
            .iter()
            .map(|n| (n.parent, ContradictionKind::ParentSlot));
        let base = (r.base != FileRef::NONE).then_some((r.base, ContradictionKind::BaseSlot));
        for (target, kind) in parents.chain(base) {
            if target.number == number || target.seq == 0 {
                continue;
            }
            let Some(&j) = f.members.get(&target.number) else {
                continue;
            };
            if !eligible(j) {
                continue;
            }
            let y = &m.records[j];
            let wrong_kind = kind == ContradictionKind::ParentSlot && !is_directory(y);
            if y.sequence != target.seq || wrong_kind {
                out.push((number.min(target.number), number.max(target.number), kind));
            }
        }
    }
    out
}

/// The highest changepoint in `(lo, hi]`, given the timed records of the
/// fragment as `(number, time)` sorted by number.
fn highest_changepoint(timed: &[(u64, u64)], lo: u64, hi: u64) -> Option<u64> {
    let k = timed.len();
    if k < 2 {
        return None;
    }
    let mut prefix_min = vec![u64::MAX; k];
    let mut running = u64::MAX;
    for (i, &(_, t)) in timed.iter().enumerate() {
        running = running.min(t);
        prefix_min[i] = running;
    }
    let mut suffix_max = vec![0u64; k];
    let mut running = 0u64;
    for i in (0..k).rev() {
        running = running.max(timed[i].1);
        suffix_max[i] = running;
    }
    // split after timed record i: records <= n_i below, >= n_{i+1} above;
    // valid changepoints are (n_i, n_{i+1}]
    (0..k - 1).rev().find_map(|i| {
        if suffix_max[i + 1] >= prefix_min[i] {
            return None;
        }
        let lower = timed[i].0.max(lo);
        let upper = timed[i + 1].0.min(hi);
        (upper > lower).then_some(upper)
    })
}

/// D2 inside one fragment: contradictions, chosen and kept cuts, marks.
fn generation_cuts(m: &Manifest, f: &mut Fragment) {
    let mut raw = contradictions(m, f);
    raw.sort_unstable();
    raw.dedup();
    // per hi only the highest-numbered lo, with every kind found for that pair
    let mut per_hi: BTreeMap<u64, (u64, BTreeSet<ContradictionKind>)> = BTreeMap::new();
    for &(lo, hi, kind) in &raw {
        let slot = per_hi.entry(hi).or_insert((lo, BTreeSet::new()));
        if lo > slot.0 {
            *slot = (lo, BTreeSet::new());
        }
        if lo == slot.0 {
            slot.1.insert(kind);
        }
    }
    let timed: Vec<(u64, u64)> = f
        .members
        .iter()
        .filter_map(|(&number, &i)| Some((number, timed(&m.records[i])?)))
        .collect();
    let mut chosen: Vec<u64> = Vec::new();
    for (&hi, &(lo, _)) in &per_hi {
        if chosen.iter().any(|&c| lo < c && c <= hi) {
            continue;
        }
        if let Some(p) = highest_changepoint(&timed, lo, hi) {
            chosen.push(p);
        }
    }
    let mut kept: Vec<CutFact> = Vec::new();
    for &c in &chosen {
        let support = per_hi
            .iter()
            .filter(|(&hi, &(lo, _))| lo < c && c <= hi)
            .count() as u32;
        if support >= 2 {
            kept.push(CutFact {
                position: c,
                older_newest: 0,
                newer_oldest: 0,
                support,
            });
        }
    }
    kept.sort_by_key(|c| c.position);
    kept.dedup_by_key(|c| c.position);
    // boundary times of D3
    for j in 0..kept.len() {
        let below_start = if j == 0 { 0 } else { kept[j - 1].position };
        let above_end = kept.get(j + 1).map_or(u64::MAX, |c| c.position);
        let c = kept[j].position;
        let above = timed
            .iter()
            .filter(|&&(n, _)| n >= c && n < above_end)
            .map(|&(_, t)| t);
        let below = timed
            .iter()
            .filter(|&&(n, _)| n >= below_start && n < c)
            .map(|&(_, t)| t);
        kept[j].older_newest = above.max().unwrap_or(0);
        kept[j].newer_oldest = below.min().unwrap_or(0);
    }
    f.contradictions = per_hi
        .iter()
        .map(|(&hi, (lo, kinds))| {
            let explained_by = kept
                .iter()
                .rev()
                .find(|c| *lo < c.position && c.position <= hi)
                .map(|c| c.position);
            ContradictionFact {
                lo: *lo,
                hi,
                kinds: kinds.iter().copied().collect(),
                explained_by,
            }
        })
        .collect();
    f.marked = f
        .contradictions
        .iter()
        .filter(|c| c.explained_by.is_none())
        .filter_map(|c| f.members.get(&c.hi).copied())
        .collect();
    f.cuts = kept;
}

/// D3: where a set of times stands relative to a kept cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Before,
    After,
    Undecided,
}

/// D3 for a segment of another fragment whose timed records span
/// `[oldest, newest]` (`None`: no timed record).
pub(crate) fn side(cut: &CutFact, span: Option<(u64, u64)>) -> Side {
    match span {
        None => Side::Undecided,
        Some((_, newest)) if newest <= cut.older_newest => Side::Before,
        Some((oldest, newest)) if oldest > cut.older_newest && newest >= cut.newer_oldest => {
            Side::After
        }
        Some(_) => Side::Undecided,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{ExtentFact, NameFact, RecordRole, StreamFact};
    use crate::ondisk::runlist::Run;

    /// A minimal in-use record of fragment origin 0 with 1 KiB records.
    fn record(number: u64, change: Option<u64>) -> RecordFact {
        RecordFact {
            offset: number * 1024,
            size: 1024,
            instance: 0,
            slot: number,
            role: RecordRole::Base,
            entry: None,
            number: Some(number),
            sequence: 1,
            flags: 1,
            link_count: 1,
            lsn: 0,
            base: FileRef::NONE,
            bytes_in_use: 0x100,
            usn: 1,
            torn_blocks: Vec::new(),
            attr_types: vec![0x10, 0x30],
            standard_information: change.map(|t| ([t; 4], 0x20, 72)),
            names: Vec::new(),
            data: Vec::new(),
            index_allocation: Vec::new(),
            index_root: None,
            attribute_list: None,
            reparse_tag: None,
            fnv1a64: number,
            state: ArtifactState::Intact,
            expect: Default::default(),
        }
    }

    fn with_parent(mut r: RecordFact, parent: FileRef) -> RecordFact {
        r.names.push(NameFact {
            name: format!("r{}", r.slot),
            name_utf16: Vec::new(),
            namespace: 1,
            parent,
            times: [0; 4],
            allocated_size: 0,
            real_size: 0,
            flags: 0,
            ea_or_reparse: 0,
            attr_id: 1,
        });
        r
    }

    fn with_run(mut r: RecordFact, lcn: u64, len: u64) -> RecordFact {
        r.data.push(StreamFact {
            type_code: 0x80,
            name: String::new(),
            flags: 0,
            attr_id: 2,
            attr_offset: 0x98,
            resident: None,
            non_resident: Some(ExtentFact {
                lowest_vcn: 0,
                highest_vcn: len as i64 - 1,
                runs: vec![Run::at(lcn, len)],
                allocated_size: len * 4096,
                real_size: len * 4096,
                initialized_size: len * 4096,
                compressed_size: None,
                compression_unit: 0,
            }),
        });
        r
    }

    fn manifest(records: Vec<RecordFact>) -> Manifest {
        let mut m = Manifest::blank("unit", 1 << 30);
        m.records = records;
        m
    }

    fn year(y: u32) -> u64 {
        time::ymd(y, 6, 1)
    }

    fn analyse_all(m: &Manifest) -> Analysis {
        let classes = vec![Some(RecordClass::Old); m.records.len()];
        analyse(m, &classes)
    }

    #[test]
    fn changepoints_need_strict_order_and_a_timed_record_on_each_side() {
        let timed = [
            (10, year(2022)),
            (11, year(2023)),
            (20, year(2018)),
            (21, year(2019)),
        ];
        // the only split: below 11, above 20; valid p in (11, 20]
        assert_eq!(highest_changepoint(&timed, 0, 100), Some(20));
        assert_eq!(highest_changepoint(&timed, 0, 15), Some(15));
        assert_eq!(highest_changepoint(&timed, 11, 12), Some(12));
        assert_eq!(
            highest_changepoint(&timed, 20, 30),
            None,
            "(20, 30] holds no changepoint"
        );
        assert_eq!(highest_changepoint(&timed, 0, 11), None);
        // equal times are not strictly older
        let tie = [(10, year(2020)), (20, year(2020))];
        assert_eq!(highest_changepoint(&tie, 0, 30), None);
        // one record above that is newer than one below: no changepoint at all
        let mixed = [(10, year(2022)), (20, year(2018)), (21, year(2023))];
        assert_eq!(highest_changepoint(&mixed, 0, 30), None);
        assert_eq!(highest_changepoint(&[(10, 1)], 0, 30), None);
    }

    #[test]
    fn a_cut_needs_a_changepoint_and_two_contradictions() {
        // newer generation 100..=102, older 200..=203; 200 and 201 share
        // clusters with 101 and 102, 203 names 100 as parent but 100 is a file
        let mut records = vec![
            with_run(record(100, Some(year(2022))), 5000, 4),
            with_run(record(101, Some(year(2022))), 6000, 4),
            with_run(record(102, Some(year(2023))), 7000, 4),
            with_run(record(200, Some(year(2018))), 6002, 4),
            with_run(record(201, Some(year(2018))), 7003, 1),
            record(202, Some(year(2019))),
            with_parent(record(203, Some(year(2019))), FileRef::new(100, 1)),
        ];
        records[0].flags = 1; // a file
        let m = manifest(records);
        let a = analyse_all(&m);
        assert_eq!(a.fragments.len(), 1);
        let f = &a.fragments[0];
        let pairs: Vec<(u64, u64)> = f.contradictions.iter().map(|c| (c.lo, c.hi)).collect();
        assert_eq!(pairs, [(101, 200), (102, 201), (100, 203)]);
        assert_eq!(f.contradictions[2].kinds, [ContradictionKind::ParentSlot]);
        assert_eq!(f.cuts.len(), 1);
        assert_eq!(
            f.cuts[0].position, 200,
            "the highest changepoint in (101, 200]"
        );
        assert_eq!(f.cuts[0].support, 3);
        assert_eq!(f.cuts[0].older_newest, year(2019));
        assert_eq!(f.cuts[0].newer_oldest, year(2022));
        assert!(f.marked.is_empty());
        assert_eq!((f.segment(102), f.segment(200), f.segment(203)), (0, 1, 1));
    }

    #[test]
    fn one_contradiction_is_not_enough_and_marks_its_stale_record() {
        let m = manifest(vec![
            with_run(record(100, Some(year(2022))), 5000, 4),
            record(101, Some(year(2022))),
            with_run(record(200, Some(year(2018))), 5001, 1),
            record(201, Some(year(2018))),
        ]);
        let a = analyse_all(&m);
        let f = &a.fragments[0];
        assert!(f.cuts.is_empty(), "a cut with a single hi is dropped");
        assert_eq!(f.contradictions[0].explained_by, None);
        assert_eq!(
            f.marked
                .iter()
                .map(|&i| m.records[i].slot)
                .collect::<Vec<_>>(),
            [200]
        );
    }

    #[test]
    fn without_a_changepoint_every_contradiction_is_unexplained() {
        // the stale records carry newer times than the records in front of them
        let m = manifest(vec![
            with_run(record(100, Some(year(2020))), 5000, 4),
            with_run(record(101, Some(year(2020))), 6000, 4),
            with_run(record(200, Some(year(2021))), 5000, 1),
            with_run(record(201, Some(year(2018))), 6000, 1),
        ]);
        let a = analyse_all(&m);
        let f = &a.fragments[0];
        assert!(f.cuts.is_empty());
        let marked: Vec<u64> = f.marked.iter().map(|&i| m.records[i].slot).collect();
        assert_eq!(marked, [200, 201]);
    }

    #[test]
    fn deleted_torn_and_matching_records_do_not_contradict() {
        let mut deleted = with_run(record(200, Some(year(2018))), 5000, 1);
        deleted.flags = 0;
        deleted.sequence = 2;
        let mut torn = with_run(record(201, Some(year(2018))), 5001, 1);
        torn.torn_blocks = vec![1];
        let mut folder = record(100, Some(year(2022)));
        folder.flags = 3;
        let m = manifest(vec![
            with_run(folder, 5000, 4),
            deleted,
            torn,
            // parent (100, 1) is an in-use directory with sequence 1: fine
            with_parent(record(202, Some(year(2018))), FileRef::new(100, 1)),
            // parent (5, 5): no record 5 in the fragment
            with_parent(record(203, Some(year(2018))), FileRef::new(5, 5)),
        ]);
        let a = analyse_all(&m);
        assert!(a.fragments[0].contradictions.is_empty());
    }

    #[test]
    fn per_hi_only_the_highest_lo_is_kept() {
        let m = manifest(vec![
            with_run(record(100, Some(year(2022))), 5000, 1),
            with_run(record(150, Some(year(2022))), 5001, 1),
            with_run(record(200, Some(year(2018))), 5000, 2),
        ]);
        let a = analyse_all(&m);
        let pairs: Vec<(u64, u64)> = a.fragments[0]
            .contradictions
            .iter()
            .map(|c| (c.lo, c.hi))
            .collect();
        assert_eq!(pairs, [(150, 200)]);
    }

    #[test]
    fn origins_group_fragments_and_unnumbered_records_take_the_grid_position() {
        let mut far = record(5000, Some(year(2020)));
        far.offset = 64 << 20;
        let mut unnumbered = record(3, Some(year(2020)));
        unnumbered.number = None;
        let mut off_grid = record(4, Some(year(2020)));
        off_grid.number = None;
        off_grid.offset = 4 * 1024 + 512;
        let m = manifest(vec![
            record(1, None),
            record(2, None),
            unnumbered,
            off_grid,
            far,
        ]);
        let a = analyse_all(&m);
        assert_eq!(a.fragments.len(), 2);
        assert_eq!(
            a.numbers[2],
            Some(3),
            "positional number on the grid of the first fragment"
        );
        assert_eq!(a.fragment_of[2], Some(0));
        assert_eq!(a.unplaced, [3], "not on any grid: a stray");
        assert_eq!(a.fragments[1].origin, (64 << 20) - 5000 * 1024);
    }

    #[test]
    fn boundary_sides_follow_d3() {
        let cut = CutFact {
            position: 512,
            older_newest: year(2019),
            newer_oldest: year(2021),
            support: 2,
        };
        assert_eq!(side(&cut, Some((year(2017), year(2019)))), Side::Before);
        assert_eq!(side(&cut, Some((year(2021), year(2023)))), Side::After);
        assert_eq!(
            side(&cut, Some((year(2020), year(2020)))),
            Side::Undecided,
            "newest before A_c"
        );
        assert_eq!(
            side(&cut, Some((year(2018), year(2023)))),
            Side::Undecided,
            "straddles B_c"
        );
        assert_eq!(side(&cut, None), Side::Undecided);
    }
}
