//! ALGORITHMS D1 (fragments), D2 (generation cuts) and D3 (boundary
//! times).

use std::collections::{BTreeMap, BTreeSet};

use hds_ntfs::FixupStatus;

use super::{Cut, CutSide, SegmentId, NONE};
use crate::live::LiveIndex;
use crate::store::{DataExtent, RecordId, ScanStore};

/// Unnumbered records join a fragment when they lie within this many
/// records of its number span (D1).
const D1_REACH: i128 = 4096;

/// FILETIME of 1980-01-01 00:00 UTC.
pub(crate) const FT_1980: u64 = 119_600_064_000_000_000;
/// One day in FILETIME ticks.
pub(crate) const FT_DAY: u64 = 864_000_000_000;

/// D2's "timed": a plausible MFT-change time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timed {
    /// Latest plausible time (reference + one day); `None`: no bound.
    pub limit: Option<u64>,
}

impl Timed {
    pub fn new(reference: Option<u64>) -> Timed {
        Timed {
            limit: reference.map(|r| r.saturating_add(FT_DAY)),
        }
    }

    pub fn ok(&self, t: u64) -> bool {
        t != 0 && t >= FT_1980 && self.limit.is_none_or(|l| t <= l)
    }

    /// The record's MFT-change time when it is timed.
    pub fn of(&self, store: &ScanStore, id: RecordId) -> Option<u64> {
        store
            .record(id)
            .and_then(|r| r.mft_changed())
            .filter(|&t| self.ok(t))
    }
}

/// One fragment: numbered records on one grid, sorted by number.
#[derive(Debug, Clone)]
pub(crate) struct FragmentData {
    pub record_size: u32,
    pub origin: i64,
    pub records: Vec<(u32, RecordId)>,
    /// Segment ids, lowest numbers first.
    pub segments: Vec<SegmentId>,
}

/// One segment: a slice of a fragment's records between kept cuts.
#[derive(Debug, Clone)]
pub(crate) struct SegmentData {
    pub fragment: u32,
    pub index: u32,
    pub start: usize,
    pub end: usize,
}

/// The result of D1-D3.
#[derive(Debug, Default)]
pub(crate) struct Frags {
    pub fragments: Vec<FragmentData>,
    pub segments: Vec<SegmentData>,
    pub cuts: Vec<Cut>,
    /// Effective record number per record id (`NONE`: none).
    pub num: Vec<u32>,
    /// Segment per record id (`NONE`: not in a segment).
    pub seg: Vec<u32>,
    /// D1 strays: old records without a number that joined no fragment, by
    /// ascending offset.
    pub strays: Vec<RecordId>,
    /// Records marked `older_generation` by D2.
    pub older: Vec<RecordId>,
    /// Number of unexplained contradictions.
    pub unexplained: u64,
    /// Number of contradictions found.
    pub contradictions: u64,
}

impl Frags {
    pub fn segment_records(&self, s: SegmentId) -> &[(u32, RecordId)] {
        let Some(seg) = self.segments.get(s.index()) else {
            return &[];
        };
        self.fragments
            .get(usize::try_from(seg.fragment).unwrap_or(usize::MAX))
            .and_then(|f| f.records.get(seg.start..seg.end))
            .unwrap_or(&[])
    }
}

/// D1-D3 over the old records of `store`.
pub(crate) fn build(store: &ScanStore, live: &LiveIndex, timed: Timed) -> Frags {
    let n = store.record_count();
    let mut out = Frags {
        num: vec![NONE; n],
        seg: vec![NONE; n],
        ..Frags::default()
    };
    let index = store.fragment_index(|id, _| live.is_old(id));
    let mut frags: Vec<FragmentData> = index
        .fragments()
        .map(|f| FragmentData {
            record_size: f.record_size,
            origin: f.origin,
            records: f
                .records
                .iter()
                .filter_map(|&id| {
                    store
                        .record(id)
                        .and_then(|r| r.record_number())
                        .map(|k| (k, id))
                })
                .collect(),
            segments: Vec::new(),
        })
        .collect();
    // Priority: most records first, then record size and origin.
    frags.sort_by(|a, b| {
        b.records
            .len()
            .cmp(&a.records.len())
            .then(a.record_size.cmp(&b.record_size))
            .then(a.origin.cmp(&b.origin))
    });
    place_unnumbered(store, index.unnumbered(), &mut frags, &mut out.strays);

    for (fi, f) in frags.iter_mut().enumerate() {
        let fid = u32::try_from(fi).unwrap_or(NONE);
        for &(k, id) in &f.records {
            if let Some(slot) = out.num.get_mut(id.index()) {
                *slot = k;
            }
        }
        let result = cut_fragment(store, &f.records, timed);
        out.contradictions = out.contradictions.saturating_add(result.contradictions);
        out.unexplained = out
            .unexplained
            .saturating_add(u64::try_from(result.unexplained.len()).unwrap_or(0));
        out.older.extend(result.unexplained.iter().copied());
        // Segments between kept cuts.
        let mut bounds = vec![0usize];
        for &c in &result.kept {
            bounds.push(f.records.partition_point(|&(k, _)| k < c));
        }
        bounds.push(f.records.len());
        let first_seg = out.segments.len();
        for (si, w) in bounds.windows(2).enumerate() {
            let (Some(&a), Some(&b)) = (w.first(), w.get(1)) else {
                continue;
            };
            let sid = u32::try_from(out.segments.len()).unwrap_or(NONE);
            for &(_, id) in f.records.get(a..b).unwrap_or(&[]) {
                if let Some(slot) = out.seg.get_mut(id.index()) {
                    *slot = sid;
                }
            }
            out.segments.push(SegmentData {
                fragment: fid,
                index: u32::try_from(si).unwrap_or(NONE),
                start: a,
                end: b,
            });
            f.segments.push(SegmentId(sid));
        }
        // D3 boundary times.
        for (k, &c) in result.kept.iter().enumerate() {
            let below = first_seg.saturating_add(k);
            let above = below.saturating_add(1);
            let times = |s: usize| -> Vec<u64> {
                let Some(seg) = out.segments.get(s) else {
                    return Vec::new();
                };
                f.records
                    .get(seg.start..seg.end)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|&(_, id)| timed.of(store, id))
                    .collect()
            };
            let b = times(above).into_iter().max();
            let a = b.and_then(|b| times(below).into_iter().filter(|&t| t > b).min());
            out.cuts.push(Cut {
                fragment: fid,
                at: c,
                older_newest: b,
                newer_oldest: a,
                below: SegmentId(u32::try_from(below).unwrap_or(NONE)),
                above: SegmentId(u32::try_from(above).unwrap_or(NONE)),
            });
        }
    }
    out.fragments = frags;
    out.older.sort_unstable();
    out
}

/// D1: an unnumbered record on the grid of a fragment, within 4096 records
/// of its number span (of its numbered records), takes the positional
/// number and joins it; the fragments are tried by priority. The others
/// become strays.
fn place_unnumbered(
    store: &ScanStore,
    unnumbered: &[RecordId],
    frags: &mut [FragmentData],
    strays: &mut Vec<RecordId>,
) {
    if unnumbered.is_empty() {
        return;
    }
    // Pool per (record size, offset mod size), by offset.
    let mut pool: BTreeMap<(u32, u64), BTreeSet<(u64, RecordId)>> = BTreeMap::new();
    for &id in unnumbered {
        let Some(r) = store.record(id) else { continue };
        let size = r.record_size();
        let off = r.disk_offset();
        let res = off.checked_rem(u64::from(size)).unwrap_or(0);
        pool.entry((size, res)).or_default().insert((off, id));
    }
    for f in frags.iter_mut() {
        let (Some(&(lo, _)), Some(&(hi, _))) = (f.records.first(), f.records.last()) else {
            continue;
        };
        let size = i128::from(f.record_size);
        let origin = i128::from(f.origin);
        let res = u64::try_from(origin.rem_euclid(size)).unwrap_or(0);
        let Some(set) = pool.get_mut(&(f.record_size, res)) else {
            continue;
        };
        let from = origin
            .saturating_add(i128::from(lo).saturating_sub(D1_REACH).saturating_mul(size))
            .max(0);
        let to =
            origin.saturating_add(i128::from(hi).saturating_add(D1_REACH).saturating_mul(size));
        if to < 0 {
            continue;
        }
        let from = u64::try_from(from).unwrap_or(u64::MAX);
        let to = u64::try_from(to).unwrap_or(u64::MAX);
        let mut taken: Vec<(u64, RecordId)> = Vec::new();
        let mut placed: Vec<(u32, RecordId)> = Vec::new();
        for &(off, id) in set.range((from, RecordId(0))..=(to, RecordId(u32::MAX))) {
            let n = (i128::from(off).saturating_sub(origin)).checked_div(size);
            let Some(n) = n.and_then(|n| u32::try_from(n).ok()) else {
                continue;
            };
            if f.records.binary_search_by_key(&n, |&(k, _)| k).is_ok() {
                continue;
            }
            taken.push((off, id));
            placed.push((n, id));
        }
        for t in &taken {
            set.remove(t);
        }
        if !placed.is_empty() {
            f.records.extend(placed);
            f.records.sort_unstable();
        }
    }
    for set in pool.into_values() {
        strays.extend(set.into_iter().map(|(_, id)| id));
    }
    strays.sort_by_key(|&id| store.record(id).map_or(0, |r| r.disk_offset()));
}

struct CutResult {
    kept: Vec<u32>,
    unexplained: Vec<RecordId>,
    contradictions: u64,
}

/// Interval map of D2(A): which record number last mapped each cluster.
#[derive(Default)]
struct Paint {
    map: BTreeMap<u64, (u64, u32)>,
}

impl Paint {
    /// The highest number painted anywhere in `[a, b)`.
    fn max_over(&self, a: u64, b: u64) -> Option<u32> {
        let mut best: Option<u32> = None;
        for (_, &(end, num)) in self.map.range(..b).rev() {
            if end <= a {
                break;
            }
            best = Some(best.map_or(num, |x| x.max(num)));
        }
        best
    }

    /// Paints `[a, b)` with `num`.
    fn paint(&mut self, a: u64, b: u64, num: u32) {
        let mut remove = Vec::new();
        let mut keep = Vec::new();
        for (&s, &(e, n)) in self.map.range(..b).rev() {
            if e <= a {
                break;
            }
            remove.push(s);
            if s < a {
                keep.push((s, (a, n)));
            }
            if e > b {
                keep.push((b, (e, n)));
            }
        }
        for s in remove {
            self.map.remove(&s);
        }
        for (s, v) in keep {
            self.map.insert(s, v);
        }
        self.map.insert(a, (b, num));
    }
}

/// D2 for one fragment.
fn cut_fragment(store: &ScanStore, recs: &[(u32, RecordId)], timed: Timed) -> CutResult {
    // Timed records by number.
    let tn: Vec<(u32, u64)> = recs
        .iter()
        .filter_map(|&(k, id)| timed.of(store, id).map(|t| (k, t)))
        .collect();
    let t = tn.len();
    // Valid changepoint stretches: k timed records below p.
    let mut valid_k: Vec<usize> = Vec::new();
    if t >= 2 {
        let mut pref_min = Vec::with_capacity(t);
        let mut m = u64::MAX;
        for &(_, x) in &tn {
            m = m.min(x);
            pref_min.push(m);
        }
        let mut suf_max = vec![0u64; t];
        let mut m = 0u64;
        for (i, &(_, x)) in tn.iter().enumerate().rev() {
            m = m.max(x);
            if let Some(s) = suf_max.get_mut(i) {
                *s = m;
            }
        }
        for k in 1..t {
            let (Some(&sm), Some(&pm)) = (suf_max.get(k), pref_min.get(k.saturating_sub(1))) else {
                continue;
            };
            if sm < pm {
                valid_k.push(k);
            }
        }
    }
    let k_of = |p: u32| tn.partition_point(|&(k, _)| k < p);
    let highest_changepoint = |lo: u32, hi: u32| -> Option<u32> {
        let k = k_of(hi);
        let idx = valid_k.partition_point(|&v| v <= k).checked_sub(1)?;
        let kk = *valid_k.get(idx)?;
        let p = if kk == k { hi } else { tn.get(kk)?.0 };
        (p > lo).then_some(p)
    };

    // Contradictions: hi -> highest lo.
    let mut pairs: BTreeMap<u32, u32> = BTreeMap::new();
    let mut add = |a: u32, b: u32| {
        let (lo, hi) = (a.min(b), a.max(b));
        if lo == hi {
            return;
        }
        let e = pairs.entry(hi).or_insert(lo);
        if *e < lo {
            *e = lo;
        }
    };
    let cand = |id: RecordId| {
        store
            .record(id)
            .is_some_and(|r| r.is_in_use() && r.fixup() == FixupStatus::Ok)
    };
    // (A) shared cluster numbers.
    let mut paint = Paint::default();
    let mut ivs: Vec<(u64, u64)> = Vec::new();
    for &(k, id) in recs {
        if !cand(id) {
            continue;
        }
        ivs.clear();
        let mut push = |runs: &mut dyn Iterator<Item = hds_ntfs::Run>| {
            for run in runs {
                if let Some(l) = run.lcn {
                    ivs.push((l, l.saturating_add(run.length)));
                }
            }
        };
        for e in store.data_extents(id) {
            if let DataExtent::NonResident(x) = e {
                push(&mut x.runs());
            }
        }
        for x in store.index_allocation_extents(id) {
            push(&mut x.runs());
        }
        for &(a, b) in &ivs {
            if let Some(lo) = paint.max_over(a, b) {
                if lo != k {
                    add(lo, k);
                }
            }
        }
        for &(a, b) in &ivs {
            if a < b {
                paint.paint(a, b, k);
            }
        }
    }
    drop(paint);
    // (B) references that do not fit.
    for &(k, id) in recs {
        if !cand(id) {
            continue;
        }
        let Some(r) = store.record(id) else { continue };
        let mut refs: Vec<(u64, u16, bool)> = store
            .names(id)
            .map(|n| (n.parent.number(), n.parent.sequence(), true))
            .collect();
        if r.is_extension() {
            refs.push((r.base().number(), r.base().sequence(), false));
        }
        for (n, s, parent) in refs {
            let Ok(n32) = u32::try_from(n) else { continue };
            let Ok(pos) = recs.binary_search_by_key(&n32, |&(x, _)| x) else {
                continue;
            };
            let Some(&(_, qid)) = recs.get(pos) else {
                continue;
            };
            if qid == id || !cand(qid) {
                continue;
            }
            let Some(q) = store.record(qid) else { continue };
            let bad = q.sequence() != s || (parent && !super::files::is_directory(store, qid));
            if bad {
                add(k, n32);
            }
        }
    }
    let contradictions = u64::try_from(pairs.len()).unwrap_or(u64::MAX);

    // Choosing cuts.
    let mut fallback: Option<Fallback> = None;
    let mut chosen: BTreeSet<u32> = BTreeSet::new();
    for (&hi, &lo) in &pairs {
        if chosen.range(lo.saturating_add(1)..=hi).next().is_some() {
            continue;
        }
        if let Some(p) = highest_changepoint(lo, hi) {
            chosen.insert(p);
            continue;
        }
        let fb = fallback.get_or_insert_with(|| Fallback::new(&tn));
        if let Some(p) = fb.best_in(lo, hi, &tn) {
            chosen.insert(p);
        }
    }
    // Kept cuts: inside the intervals of at least 2 contradictions.
    let mut los: Vec<u32> = pairs.values().copied().collect();
    los.sort_unstable();
    let his: Vec<u32> = pairs.keys().copied().collect();
    let kept: Vec<u32> = chosen
        .iter()
        .copied()
        .filter(|&c| {
            let below = los.partition_point(|&l| l < c);
            let ended = his.partition_point(|&h| h < c);
            below.saturating_sub(ended) >= 2
        })
        .collect();
    let kept_set: BTreeSet<u32> = kept.iter().copied().collect();
    let mut unexplained = Vec::new();
    for (&hi, &lo) in &pairs {
        if kept_set.range(lo.saturating_add(1)..=hi).next().is_none() {
            if let Ok(pos) = recs.binary_search_by_key(&hi, |&(x, _)| x) {
                if let Some(&(_, id)) = recs.get(pos) {
                    unexplained.push(id);
                }
            }
        }
    }
    CutResult {
        kept,
        unexplained,
        contradictions,
    }
}

/// D2's fallback choice: for every split position k (k timed records
/// below the cut), the fewest timed records out of order for the best
/// time T; a range-minimum tree answers "the best k in a range, the
/// highest on ties".
struct Fallback {
    /// Range-min tree over (cost, k) with ties to the larger k.
    tree: Vec<(u64, usize)>,
    size: usize,
    total: usize,
}

impl Fallback {
    fn new(tn: &[(u32, u64)]) -> Fallback {
        let t = tn.len();
        let mut times: Vec<u64> = tn.iter().map(|&(_, x)| x).collect();
        times.sort_unstable();
        times.dedup();
        let d = times.len();
        // cost(pos) with every timed record above the cut: position 0 is a
        // T below all times; position j (1-based) is T = times[j-1].
        // cost = #{t > T}.
        let mut init = Vec::with_capacity(d.saturating_add(1));
        let mut sorted: Vec<u64> = tn.iter().map(|&(_, x)| x).collect();
        sorted.sort_unstable();
        init.push(u64::try_from(t).unwrap_or(u64::MAX));
        for &tt in &times {
            let le = sorted.partition_point(|&x| x <= tt);
            init.push(u64::try_from(t.saturating_sub(le)).unwrap_or(0));
        }
        let mut sweep = LazyMin::new(&init);
        let mut best = Vec::with_capacity(t.saturating_add(1));
        best.push(sweep.min());
        for &(_, x) in tn {
            // This record moves below the cut.
            let j = times.partition_point(|&v| v < x).saturating_add(1);
            sweep.add(j, init.len(), 1);
            sweep.add(0, j, -1);
            best.push(sweep.min());
        }
        let size = best.len().next_power_of_two();
        let mut tree = vec![(u64::MAX, 0usize); size.saturating_mul(2)];
        for (k, &b) in best.iter().enumerate() {
            if let Some(slot) = tree.get_mut(size.saturating_add(k)) {
                *slot = (u64::try_from(b.max(0)).unwrap_or(0), k);
            }
        }
        for i in (1..size).rev() {
            let l = tree
                .get(i.saturating_mul(2))
                .copied()
                .unwrap_or((u64::MAX, 0));
            let r = tree
                .get(i.saturating_mul(2).saturating_add(1))
                .copied()
                .unwrap_or((u64::MAX, 0));
            if let Some(slot) = tree.get_mut(i) {
                *slot = better(l, r);
            }
        }
        Fallback {
            tree,
            size,
            total: t,
        }
    }

    /// The best cut position in `(lo, hi]` when it leaves at most a quarter
    /// of the timed records out of order.
    fn best_in(&self, lo: u32, hi: u32, tn: &[(u32, u64)]) -> Option<u32> {
        if self.total == 0 {
            return None;
        }
        let k_lo = tn.partition_point(|&(k, _)| k <= lo);
        let k_hi = tn.partition_point(|&(k, _)| k < hi);
        if k_lo > k_hi {
            return None;
        }
        let (cost, k) = self.query(k_lo, k_hi.saturating_add(1));
        if cost.saturating_mul(4) > u64::try_from(self.total).unwrap_or(u64::MAX) {
            return None;
        }
        if k == k_hi {
            Some(hi)
        } else {
            tn.get(k).map(|&(n, _)| n)
        }
    }

    /// Minimum over `[a, b)`.
    fn query(&self, a: usize, b: usize) -> (u64, usize) {
        let mut res = (u64::MAX, 0usize);
        let mut l = a.saturating_add(self.size);
        let mut r = b.saturating_add(self.size);
        while l < r {
            if l & 1 == 1 {
                res = better(res, self.tree.get(l).copied().unwrap_or((u64::MAX, 0)));
                l = l.saturating_add(1);
            }
            if r & 1 == 1 {
                r = r.saturating_sub(1);
                res = better(res, self.tree.get(r).copied().unwrap_or((u64::MAX, 0)));
            }
            l >>= 1;
            r >>= 1;
        }
        res
    }
}

/// The smaller cost; on equal cost the larger k.
fn better(a: (u64, usize), b: (u64, usize)) -> (u64, usize) {
    if b.0 < a.0 || (b.0 == a.0 && b.1 > a.1) {
        b
    } else {
        a
    }
}

/// Segment tree with range add and global minimum.
struct LazyMin {
    n: usize,
    min: Vec<i64>,
    lazy: Vec<i64>,
}

impl LazyMin {
    fn new(init: &[u64]) -> LazyMin {
        let n = init.len().max(1);
        let mut t = LazyMin {
            n,
            min: vec![0; n.saturating_mul(4)],
            lazy: vec![0; n.saturating_mul(4)],
        };
        t.build(1, 0, n, init);
        t
    }

    fn build(&mut self, node: usize, l: usize, r: usize, init: &[u64]) {
        if r.saturating_sub(l) == 1 {
            let v = init
                .get(l)
                .map_or(i64::MAX, |&x| i64::try_from(x).unwrap_or(i64::MAX));
            if let Some(s) = self.min.get_mut(node) {
                *s = v;
            }
            return;
        }
        let m = l.saturating_add(r.saturating_sub(l) / 2);
        let (a, b) = (
            node.saturating_mul(2),
            node.saturating_mul(2).saturating_add(1),
        );
        self.build(a, l, m, init);
        self.build(b, m, r, init);
        let v = self.get(a).min(self.get(b));
        if let Some(s) = self.min.get_mut(node) {
            *s = v;
        }
    }

    fn get(&self, node: usize) -> i64 {
        self.min.get(node).copied().unwrap_or(i64::MAX)
    }

    fn min(&self) -> i64 {
        self.get(1)
    }

    fn add(&mut self, a: usize, b: usize, v: i64) {
        if a < b {
            self.add_rec(1, 0, self.n, a, b, v);
        }
    }

    fn add_rec(&mut self, node: usize, l: usize, r: usize, a: usize, b: usize, v: i64) {
        if b <= l || r <= a {
            return;
        }
        if a <= l && r <= b {
            if let Some(s) = self.min.get_mut(node) {
                *s = s.saturating_add(v);
            }
            if let Some(s) = self.lazy.get_mut(node) {
                *s = s.saturating_add(v);
            }
            return;
        }
        let m = l.saturating_add(r.saturating_sub(l) / 2);
        let (ca, cb) = (
            node.saturating_mul(2),
            node.saturating_mul(2).saturating_add(1),
        );
        self.add_rec(ca, l, m, a, b, v);
        self.add_rec(cb, m, r, a, b, v);
        let lz = self.lazy.get(node).copied().unwrap_or(0);
        let v = self.get(ca).min(self.get(cb)).saturating_add(lz);
        if let Some(s) = self.min.get_mut(node) {
            *s = v;
        }
    }
}

impl Cut {
    /// D3: how a set of times (oldest, newest) relates to this cut.
    pub fn relation(&self, times: Option<(u64, u64)>) -> CutSide {
        let (Some((old, new)), Some(b)) = (times, self.older_newest) else {
            return CutSide::Undecided;
        };
        if new <= b {
            return CutSide::Before;
        }
        if old > b && self.newer_oldest.is_some_and(|a| new >= a) {
            return CutSide::After;
        }
        CutSide::Undecided
    }
}
