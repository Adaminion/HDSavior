//! ALGORITHMS D4 (joining), D5 (order of work, steps 2 to 4), D7
//! (embedded instances) and D6 (strays and copies).
//!
//! Instances are sets of segments (union-find). Each one keeps what the
//! rules ask about it, merged when two instances merge: its record numbers
//! as runs (exact sets), its time range, the cuts and anchor fragments it
//! touches, the F1 histogram, the reference counts to every other
//! instance. Pairs are taken from a priority queue in the order of D4
//! ("the pair with the strongest evidence merges first", then the lower
//! instance ids); a queued pair is stale once either side changed, and the
//! changed side's pairs are queued anew. Evidence and the forbidding
//! conditions of a pair depend only on the two sides, so this gives the
//! same merges as re-evaluating every pair after every merge.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

use super::frag::{Frags, Timed, FT_DAY};
use super::{files, CutSide, NONE};
use crate::geometry::{self, GeoContext, GeoResult, HeadError, Heads, InstanceFiles, MftMap};
use crate::live::LiveIndex;
use crate::store::{RecordId, ScanStore};

/// D4/D5/D6 thresholds.
pub(crate) const MIN_RECORDS: u64 = 16;
const MAP_MIN: u64 = 16;
const CONTENT_MIN: u64 = 3;
const CONTRA_REFS_MIN: u64 = 3;
/// References to a record number held by more scanned records than this
/// are not counted (a bound against crafted input; real disks hold a
/// handful of copies of a number).
const MAX_TARGETS: usize = 256;

/// Exact set of record numbers as runs `start -> end` (inclusive).
#[derive(Debug, Clone, Default)]
struct NumSet(BTreeMap<u32, u32>);

impl NumSet {
    fn from_sorted(nums: impl Iterator<Item = u32>) -> NumSet {
        let mut m = BTreeMap::new();
        let mut cur: Option<(u32, u32)> = None;
        for n in nums {
            cur = match cur {
                Some((s, e)) if e.checked_add(1) == Some(n) => Some((s, n)),
                Some((s, e)) if n <= e => Some((s, e)),
                Some((s, e)) => {
                    m.insert(s, e);
                    Some((n, n))
                }
                None => Some((n, n)),
            };
        }
        if let Some((s, e)) = cur {
            m.insert(s, e);
        }
        NumSet(m)
    }

    fn runs(&self) -> usize {
        self.0.len()
    }

    fn min(&self) -> Option<u32> {
        self.0.keys().next().copied()
    }

    fn contains_any(&self, s: u32, e: u32) -> bool {
        self.0
            .range(..=e)
            .next_back()
            .is_some_and(|(_, &end)| end >= s)
    }

    fn intersects(&self, other: &NumSet) -> bool {
        let (small, big) = if self.runs() <= other.runs() {
            (self, other)
        } else {
            (other, self)
        };
        small.0.iter().any(|(&s, &e)| big.contains_any(s, e))
    }

    fn absorb(&mut self, other: NumSet) {
        for (s, e) in other.0 {
            let (mut s, mut e) = (s, e);
            if let Some((&ps, &pe)) = self.0.range(..=s).next_back() {
                if pe.saturating_add(1) >= s {
                    s = ps;
                    e = e.max(pe);
                    self.0.remove(&ps);
                }
            }
            while let Some((&ns, &ne)) = self.0.range(s..).next() {
                if ns > e.saturating_add(1) {
                    break;
                }
                e = e.max(ne);
                self.0.remove(&ns);
            }
            self.0.insert(s, e);
        }
    }
}

/// D4's evidence, ordered from weakest to strongest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ev {
    /// (d) the same validated geometry.
    Same,
    /// (b) matched of checked files.
    Content { matched: u64, checked: u64 },
    /// (a) resolving references.
    Refs(u64),
    /// (c) records placed by a record 0.
    Map(u64),
}

impl Ev {
    fn rank(&self) -> u8 {
        match self {
            Ev::Same => 0,
            Ev::Content { .. } => 1,
            Ev::Refs(_) => 2,
            Ev::Map(_) => 3,
        }
    }
}

impl Ord for Ev {
    fn cmp(&self, other: &Ev) -> Ordering {
        self.rank()
            .cmp(&other.rank())
            .then_with(|| match (self, other) {
                (
                    Ev::Content {
                        matched: m1,
                        checked: c1,
                    },
                    Ev::Content {
                        matched: m2,
                        checked: c2,
                    },
                ) => u128::from(*m1)
                    .saturating_mul(u128::from(*c2))
                    .cmp(&u128::from(*m2).saturating_mul(u128::from(*c1))),
                (Ev::Refs(a), Ev::Refs(b)) | (Ev::Map(a), Ev::Map(b)) => a.cmp(b),
                _ => Ordering::Equal,
            })
    }
}

impl PartialOrd for Ev {
    fn partial_cmp(&self, other: &Ev) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The state of one instance (kept at its root segment).
#[derive(Debug, Clone)]
pub(crate) struct IState {
    pub segs: Vec<u32>,
    pub nrec: u64,
    nums: NumSet,
    pub times: Option<(u64, u64)>,
    pub lowest: u64,
    max_num: u32,
    f1: BTreeMap<u32, u64>,
    rec0: Option<RecordId>,
    rec1: Option<RecordId>,
    rec0_exts: Vec<RecordId>,
    pub version: u64,
    pub geo: Option<GeoResult>,
    pub embedded_in: Option<(u32, RecordId)>,
    pub stale: Option<(u32, RecordId)>,
    adj: BTreeMap<u32, (u64, u64)>,
    cuts: Vec<u32>,
    anchor_min: Vec<(u32, u32)>,
}

/// What decides a holder's placement: its cluster size, record 0, record
/// 1 and the extension records of record 0.
type MapSignature = (u32, RecordId, Option<RecordId>, Vec<RecordId>);

/// Members of an instance and its files, cached per version.
type FilesEntry = (u64, Rc<Vec<(u32, RecordId)>>, Rc<InstanceFiles>);

/// What a holder of a record 0 places (D4 c), per own offset.
struct Placement {
    signature: MapSignature,
    /// Initialized size of its `$MFT:$DATA` and the record size of record 0.
    init: Option<u64>,
    rec_size: u32,
    /// Count of placed records per instance root, per own offset.
    counts: Vec<BTreeMap<u32, u64>>,
}

/// The work state of D4-D7.
pub(crate) struct Work<'a> {
    pub store: &'a ScanStore,
    pub live: &'a LiveIndex,
    pub frags: &'a Frags,
    pub timed: Timed,
    pub ctx: &'a GeoContext<'a>,
    /// Anchor fragments with the current volume they belong to.
    pub anchor_frags: Vec<(u32, usize)>,
    parent: Vec<u32>,
    pub insts: Vec<Option<IState>>,
    next_version: u64,
    placements: BTreeMap<u32, Placement>,
    files_cache: HashMap<u32, FilesEntry>,
    content_cache: HashMap<(u32, u64, u32, u64), (u64, u64)>,
    pass_roots: Vec<u32>,
    pass_validated: Vec<u32>,
    pass_holders: Vec<u32>,
    pub log: Vec<String>,
}

impl<'a> Work<'a> {
    pub fn new(
        store: &'a ScanStore,
        live: &'a LiveIndex,
        frags: &'a Frags,
        timed: Timed,
        ctx: &'a GeoContext<'a>,
        anchor_frags: Vec<(u32, usize)>,
    ) -> Work<'a> {
        let nseg = frags.segments.len();
        let mut w = Work {
            store,
            live,
            frags,
            timed,
            ctx,
            anchor_frags,
            parent: (0..nseg)
                .map(|i| u32::try_from(i).unwrap_or(NONE))
                .collect(),
            insts: Vec::with_capacity(nseg),
            next_version: 1,
            placements: BTreeMap::new(),
            files_cache: HashMap::new(),
            content_cache: HashMap::new(),
            pass_roots: Vec::new(),
            pass_validated: Vec::new(),
            pass_holders: Vec::new(),
            log: Vec::new(),
        };
        // Cuts touching each segment.
        let mut seg_cuts: Vec<Vec<u32>> = vec![Vec::new(); nseg];
        for (ci, c) in frags.cuts.iter().enumerate() {
            let ci = u32::try_from(ci).unwrap_or(NONE);
            for s in [c.below.0, c.above.0] {
                if let Some(v) = seg_cuts.get_mut(usize::try_from(s).unwrap_or(usize::MAX)) {
                    v.push(ci);
                }
            }
        }
        for (si, seg) in frags.segments.iter().enumerate() {
            let sid = u32::try_from(si).unwrap_or(NONE);
            let recs = frags.segment_records(super::SegmentId(sid));
            let nums = NumSet::from_sorted(recs.iter().map(|&(n, _)| n));
            let mut times: Option<(u64, u64)> = None;
            let mut lowest = u64::MAX;
            let mut rec0 = None;
            let mut rec1 = None;
            let mut rec0_exts = Vec::new();
            for &(n, id) in recs {
                if let Some(t) = timed.of(store, id) {
                    times = Some(times.map_or((t, t), |(a, b)| (a.min(t), b.max(t))));
                }
                let Some(r) = store.record(id) else { continue };
                lowest = lowest.min(r.disk_offset());
                if n == 0 && geometry::is_mft_record(store, id) {
                    rec0 = Some(id);
                }
                if n == 1 {
                    rec1 = Some(id);
                }
                if r.is_extension() && r.base().number() == 0 && n != 0 {
                    rec0_exts.push(id);
                }
            }
            let f1 = geometry::f1_histogram(store, recs.iter().map(|&(_, id)| id));
            let anchor_min = w
                .anchor_frags
                .iter()
                .filter(|&&(f, _)| f == seg.fragment)
                .filter_map(|&(f, _)| recs.first().map(|&(n, _)| (f, n)))
                .collect();
            w.insts.push(Some(IState {
                segs: vec![sid],
                nrec: u64::try_from(recs.len()).unwrap_or(u64::MAX),
                nums,
                times,
                lowest,
                max_num: recs.last().map_or(0, |&(n, _)| n),
                f1,
                rec0,
                rec1,
                rec0_exts,
                version: 0,
                geo: None,
                embedded_in: None,
                stale: None,
                adj: BTreeMap::new(),
                cuts: seg_cuts.get(si).cloned().unwrap_or_default(),
                anchor_min,
            }));
        }
        w.count_references();
        w
    }

    fn st(&self, r: u32) -> Option<&IState> {
        self.insts
            .get(usize::try_from(r).ok()?)
            .and_then(|x| x.as_ref())
    }

    fn st_mut(&mut self, r: u32) -> Option<&mut IState> {
        self.insts
            .get_mut(usize::try_from(r).ok()?)
            .and_then(|x| x.as_mut())
    }

    /// The root (instance id) of a segment.
    pub fn find(&mut self, s: u32) -> u32 {
        let mut root = s;
        let mut guard = 0usize;
        while let Some(&p) = self.parent.get(usize::try_from(root).unwrap_or(usize::MAX)) {
            if p == root || guard > self.parent.len() {
                break;
            }
            root = p;
            guard = guard.saturating_add(1);
        }
        // Path compression.
        let mut cur = s;
        while cur != root {
            let Some(slot) = self
                .parent
                .get_mut(usize::try_from(cur).unwrap_or(usize::MAX))
            else {
                break;
            };
            let next = *slot;
            *slot = root;
            cur = next;
        }
        root
    }

    /// The root of the instance holding a record (`None` outside segments).
    pub fn root_of(&mut self, id: RecordId) -> Option<u32> {
        let s = *self.frags.seg.get(id.index())?;
        (s != NONE).then(|| self.find(s))
    }

    /// Roots of the live instances, ascending.
    pub fn roots(&self) -> Vec<u32> {
        self.insts
            .iter()
            .enumerate()
            .filter(|(_, x)| x.is_some())
            .filter_map(|(i, _)| u32::try_from(i).ok())
            .collect()
    }

    /// D4(a) counts at segment level: one pass over the references.
    fn count_references(&mut self) {
        let store = self.store;
        let frags = self.frags;
        // Global index of D's input by effective number.
        let mut by_num: Vec<(u32, RecordId)> = Vec::new();
        for (i, &n) in frags.num.iter().enumerate() {
            if n == NONE {
                continue;
            }
            if let Ok(id) = u32::try_from(i).map(RecordId) {
                if self.live.is_old(id) {
                    by_num.push((n, id));
                }
            }
        }
        by_num.sort_unstable();
        let mut pairs: HashMap<(u32, u32), (u64, u64)> = HashMap::new();
        let mut seen: Vec<(u64, u16, bool)> = Vec::new();
        for f in &frags.fragments {
            for &(_, rid) in &f.records {
                let Some(r) = store.record(rid) else { continue };
                let Some(&sr) = frags.seg.get(rid.index()) else {
                    continue;
                };
                seen.clear();
                for nm in store.names(rid) {
                    let item = (nm.parent.number(), nm.parent.sequence(), true);
                    if !seen.contains(&item) {
                        seen.push(item);
                    }
                }
                if r.is_extension() {
                    let b = r.base();
                    let item = (b.number(), b.sequence(), false);
                    if !seen.contains(&item) {
                        seen.push(item);
                    }
                }
                for &(n, s, parent) in &seen {
                    if n == 5 || n == 11 {
                        continue;
                    }
                    let Ok(n32) = u32::try_from(n) else { continue };
                    let lo = by_num.partition_point(|&(k, _)| k < n32);
                    let hi = by_num.partition_point(|&(k, _)| k <= n32);
                    let cands = by_num.get(lo..hi).unwrap_or(&[]);
                    if cands.len() > MAX_TARGETS {
                        continue;
                    }
                    // Ambiguous: more than one distinct record resolves it.
                    let mut hash: Option<u64> = None;
                    let mut ambiguous = false;
                    for &(_, x) in cands {
                        if x == rid {
                            continue;
                        }
                        let Some(xr) = store.record(x) else { continue };
                        if hds_ntfs::reference_resolves(s, xr.sequence(), xr.is_in_use()) {
                            match hash {
                                Some(h) if h != xr.hash() => ambiguous = true,
                                _ => hash = Some(xr.hash()),
                            }
                        }
                    }
                    if ambiguous {
                        continue;
                    }
                    for &(_, t) in cands {
                        if t == rid {
                            continue;
                        }
                        let Some(&st) = frags.seg.get(t.index()) else {
                            continue;
                        };
                        if st == NONE || st == sr {
                            continue;
                        }
                        if parent && !files::is_directory(store, t) {
                            continue;
                        }
                        let Some(tr) = store.record(t) else { continue };
                        let res = u64::from(hds_ntfs::reference_resolves(
                            s,
                            tr.sequence(),
                            tr.is_in_use(),
                        ));
                        let key = (sr.min(st), sr.max(st));
                        let e = pairs.entry(key).or_insert((0, 0));
                        e.0 = e.0.saturating_add(1);
                        e.1 = e.1.saturating_add(res);
                    }
                }
            }
        }
        let mut sorted: Vec<((u32, u32), (u64, u64))> = pairs.into_iter().collect();
        sorted.sort_unstable();
        for ((a, b), v) in sorted {
            if let Some(x) = self.st_mut(a) {
                x.adj.insert(b, v);
            }
            if let Some(x) = self.st_mut(b) {
                x.adj.insert(a, v);
            }
        }
    }

    fn refs(&self, a: u32, b: u32) -> (u64, u64) {
        self.st(a)
            .and_then(|x| x.adj.get(&b).copied())
            .unwrap_or((0, 0))
    }

    /// The records (number, id) of an instance by number.
    pub fn members(&mut self, r: u32) -> Rc<Vec<(u32, RecordId)>> {
        self.files_of(r).0
    }

    /// Members and files of an instance (cached per version).
    pub fn files_of(&mut self, r: u32) -> (Rc<Vec<(u32, RecordId)>>, Rc<InstanceFiles>) {
        let version = self.st(r).map_or(0, |x| x.version);
        if let Some((v, m, f)) = self.files_cache.get(&r) {
            if *v == version {
                return (m.clone(), f.clone());
            }
        }
        let mut members: Vec<(u32, RecordId)> = Vec::new();
        if let Some(x) = self.st(r) {
            for &s in &x.segs {
                members.extend_from_slice(self.frags.segment_records(super::SegmentId(s)));
            }
        }
        members.sort_unstable();
        let files = InstanceFiles::build(self.store, &members);
        let m = Rc::new(members);
        let f = Rc::new(files);
        self.files_cache.insert(r, (version, m.clone(), f.clone()));
        (m, f)
    }

    /// (c): the placement of holder `x` (recomputed when its map changed).
    fn placement(&mut self, x: u32) -> Option<&Placement> {
        let st = self.st(x)?;
        let rec0 = st.rec0?;
        let cs = geometry::f1_choice(&st.f1)
            .unwrap_or_else(|| geometry::cluster_size(self.ctx, &st.f1, st.lowest).0);
        let mut exts = st.rec0_exts.clone();
        exts.sort_unstable();
        let sig = (cs, rec0, st.rec1, exts);
        let fresh = self.placements.get(&x).is_some_and(|p| p.signature == sig);
        if !fresh {
            let p = self.compute_placement(sig);
            self.placements.insert(x, p);
        }
        self.placements.get(&x)
    }

    fn compute_placement(&mut self, sig: MapSignature) -> Placement {
        let store = self.store;
        let (cs, rec0, rec1, exts) = &sig;
        let cs64 = u64::from(*cs);
        let mut members: Vec<(u32, RecordId)> = vec![(0, *rec0)];
        for &e in exts {
            members.push((self.frags.num.get(e.index()).copied().unwrap_or(NONE), e));
        }
        members.sort_unstable();
        let r0 = store.record(*rec0);
        let rec_size = r0.map_or(1024, |r| r.record_size());
        let Some(map) = MftMap::of(store, &members) else {
            return Placement {
                signature: sig,
                init: None,
                rec_size,
                counts: Vec::new(),
            };
        };
        // Own offsets: record 0 at the start of its $MFT, or at the start of
        // the $MFTMirr that record 1 describes.
        let mut own: Vec<u64> = Vec::new();
        if let (Some(r0), Some(b0)) = (r0, map.place(0, rec_size, cs64)) {
            if let Some(o) = r0.disk_offset().checked_sub(b0) {
                own.push(o);
            }
        }
        if let (Some(r0), Some(r1)) = (r0, rec1.and_then(|id| store.record(id).map(|_| id))) {
            for e in store.data_extents(r1) {
                if let crate::store::DataExtent::NonResident(x) = e {
                    if x.start_vcn != 0 {
                        continue;
                    }
                    if let Some(l) = x.runs().next().and_then(|run| run.lcn) {
                        if let Some(o) = l
                            .checked_mul(cs64)
                            .and_then(|b| r0.disk_offset().checked_sub(b))
                        {
                            if !own.contains(&o) {
                                own.push(o);
                            }
                        }
                    }
                }
            }
        }
        let by_offset = store.records_by_offset();
        let mut counts = Vec::new();
        for o in own {
            let mut c: BTreeMap<u32, u64> = BTreeMap::new();
            for vr in &map.runs {
                let Some(lcn) = vr.run.lcn else { continue };
                let Some(start) = lcn.checked_mul(cs64).and_then(|b| b.checked_add(o)) else {
                    continue;
                };
                let end = vr.run.length.saturating_mul(cs64).saturating_add(start);
                let Some(vbase) = vr.vcn.checked_mul(cs64) else {
                    continue;
                };
                let first = by_offset
                    .partition_point(|&id| store.record(id).map_or(0, |r| r.disk_offset()) < start);
                for &id in by_offset.get(first..).unwrap_or(&[]) {
                    let Some(t) = store.record(id) else { continue };
                    let d = t.disk_offset();
                    if d >= end {
                        break;
                    }
                    let n = self.frags.num.get(id.index()).copied().unwrap_or(NONE);
                    if n == NONE {
                        continue;
                    }
                    let want = vbase.saturating_add(d.saturating_sub(start));
                    if u64::from(n).checked_mul(u64::from(t.record_size())) != Some(want) {
                        continue;
                    }
                    if let Some(root) = self.root_of(id) {
                        let e = c.entry(root).or_insert(0);
                        *e = e.saturating_add(1);
                    }
                }
            }
            counts.push(c);
        }
        Placement {
            signature: sig,
            init: map.initialized,
            rec_size,
            counts,
        }
    }

    /// D4(c) from `x` (holder) to `y`: the number of `y`'s records placed,
    /// or 0 when a condition fails.
    fn map_evidence(&mut self, x: u32, y: u32) -> u64 {
        let y_max = self.st(y).map_or(0, |s| s.max_num);
        let Some(p) = self.placement(x) else { return 0 };
        let Some(init) = p.init else { return 0 };
        let need = u64::from(y_max)
            .saturating_add(1)
            .saturating_mul(u64::from(p.rec_size));
        if need > init {
            return 0;
        }
        p.counts
            .iter()
            .map(|c| c.get(&y).copied().unwrap_or(0))
            .max()
            .unwrap_or(0)
    }

    fn map_both(&mut self, a: u32, b: u32) -> u64 {
        self.map_evidence(a, b).max(self.map_evidence(b, a))
    }

    fn validated_geo(&self, r: u32) -> Option<(u32, u64)> {
        self.st(r)
            .and_then(|x| x.geo.as_ref())
            .filter(|g| g.chosen.validated)
            .map(|g| (g.chosen.cluster_size, g.chosen.volume_offset))
    }

    /// F3 of `x`'s validated geometry over `y`'s files (cached).
    fn content(
        &mut self,
        x: u32,
        y: u32,
        heads: &mut Heads,
        cancel: &AtomicBool,
    ) -> Result<Option<(u64, u64)>, HeadError> {
        let Some((cs, off)) = self.validated_geo(x) else {
            return Ok(None);
        };
        let xv = self.st(x).map_or(0, |s| s.version);
        let yv = self.st(y).map_or(0, |s| s.version);
        if let Some(&r) = self.content_cache.get(&(x, xv, y, yv)) {
            return Ok(Some(r));
        }
        let (_, files) = self.files_of(y);
        let r = geometry::content_check(self.ctx, cs, off, &files, heads, cancel)?;
        self.content_cache.insert((x, xv, y, yv), r);
        Ok(Some(r))
    }

    /// The best evidence for joining `a` and `b` in `step` (2 or 4).
    fn evidence(
        &mut self,
        a: u32,
        b: u32,
        step: u8,
        heads: &mut Heads,
        cancel: &AtomicBool,
    ) -> Result<Option<Ev>, HeadError> {
        let (cnt, res) = self.refs(a, b);
        if cnt >= CONTRA_REFS_MIN && res.saturating_mul(5) < cnt {
            return Ok(None);
        }
        if step == 2 {
            let n = self.map_both(a, b);
            if n >= MAP_MIN {
                return Ok(Some(Ev::Map(n)));
            }
            if res >= 1 && res.saturating_mul(10) >= cnt.saturating_mul(9) {
                return Ok(Some(Ev::Refs(res)));
            }
            return Ok(None);
        }
        let mut best: Option<Ev> = None;
        for (x, y) in [(a, b), (b, a)] {
            if let Some((chk, mt)) = self.content(x, y, heads, cancel)? {
                if chk >= CONTENT_MIN && mt.saturating_mul(5) >= chk.saturating_mul(4) {
                    let e = Ev::Content {
                        matched: mt,
                        checked: chk,
                    };
                    if best.is_none_or(|bb| e > bb) {
                        best = Some(e);
                    }
                }
            }
        }
        if best.is_none() {
            if let (Some(ga), Some(gb)) = (self.validated_geo(a), self.validated_geo(b)) {
                if ga == gb {
                    best = Some(Ev::Same);
                }
            }
        }
        Ok(best)
    }

    /// Times of an instance as D3 compares them.
    fn times(&self, r: u32) -> Option<(u64, u64)> {
        self.st(r).and_then(|x| x.times)
    }

    /// The forbidding conditions of D4.
    fn forbidden(
        &mut self,
        a: u32,
        b: u32,
        step: u8,
        heads: &mut Heads,
        cancel: &AtomicBool,
    ) -> Result<Option<&'static str>, HeadError> {
        let (Some(sa), Some(sb)) = (self.st(a), self.st(b)) else {
            return Ok(Some("gone"));
        };
        if sa.nums.intersects(&sb.nums) {
            return Ok(Some("numbers"));
        }
        // Cuts.
        let mut cuts: Vec<u32> = sa.cuts.iter().chain(sb.cuts.iter()).copied().collect();
        cuts.sort_unstable();
        cuts.dedup();
        for ci in cuts {
            let Some(c) = self
                .frags
                .cuts
                .get(usize::try_from(ci).unwrap_or(usize::MAX))
            else {
                continue;
            };
            let (below, above) = (c.below.0, c.above.0);
            let hb = self.find(below);
            let ha = self.find(above);
            for (x, y) in [(a, b), (b, a)] {
                if x == hb && y == ha {
                    return Ok(Some("cut"));
                }
                if x == hb && c.relation(self.times(y)) != CutSide::After {
                    return Ok(Some("cut"));
                }
                if x == ha && c.relation(self.times(y)) == CutSide::After {
                    return Ok(Some("cut"));
                }
            }
        }
        // The current volume's $MFT origin.
        for (x, y) in [(a, b), (b, a)] {
            let (Some(sx), Some(sy)) = (self.st(x), self.st(y)) else {
                continue;
            };
            let Some(ymin) = sy.nums.min() else { continue };
            if sx.anchor_min.iter().any(|&(_, low)| ymin < low) && self.map_both(x, y) < MAP_MIN {
                return Ok(Some("anchor"));
            }
        }
        if step == 4 {
            let emb = |s: &IState, other: u32| s.embedded_in.is_some_and(|(y, _)| y == other);
            if self.st(a).is_some_and(|s| emb(s, b)) || self.st(b).is_some_and(|s| emb(s, a)) {
                return Ok(Some("embedded"));
            }
            for (x, y) in [(a, b), (b, a)] {
                if let Some((chk, mt)) = self.content(x, y, heads, cancel)? {
                    if chk >= CONTENT_MIN && mt.saturating_mul(5) < chk {
                        return Ok(Some("content"));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Merges `b` into `a` or the reverse (the larger keeps its id; on
    /// equal size the lower id). Returns the keeper.
    fn merge(&mut self, a: u32, b: u32) -> u32 {
        let (na, nb) = (
            self.st(a).map_or(0, |x| x.nrec),
            self.st(b).map_or(0, |x| x.nrec),
        );
        let (keep, gone) = if nb > na || (nb == na && b < a) {
            (b, a)
        } else {
            (a, b)
        };
        let Some(g) = self
            .insts
            .get_mut(usize::try_from(gone).unwrap_or(usize::MAX))
            .and_then(|x| x.take())
        else {
            return keep;
        };
        if let Some(p) = self
            .parent
            .get_mut(usize::try_from(gone).unwrap_or(usize::MAX))
        {
            *p = keep;
        }
        let version = self.next_version;
        self.next_version = self.next_version.saturating_add(1);
        // Adjacency: move gone's entries to keep.
        let gone_adj = g.adj;
        for (&k, &(c, r)) in &gone_adj {
            if k == keep {
                continue;
            }
            if let Some(other) = self.st_mut(k) {
                other.adj.remove(&gone);
                let e = other.adj.entry(keep).or_insert((0, 0));
                e.0 = e.0.saturating_add(c);
                e.1 = e.1.saturating_add(r);
            }
        }
        // Holder placements: counts move to the keeper.
        for p in self.placements.values_mut() {
            for c in &mut p.counts {
                if let Some(v) = c.remove(&gone) {
                    let e = c.entry(keep).or_insert(0);
                    *e = e.saturating_add(v);
                }
            }
        }
        if let Some(p) = self.placements.remove(&gone) {
            self.placements.entry(keep).or_insert(p);
        }
        self.files_cache.remove(&gone);
        self.files_cache.remove(&keep);
        self.content_cache
            .retain(|&(x, _, y, _), _| x != gone && y != gone && x != keep && y != keep);
        if let Some(k) = self.st_mut(keep) {
            k.adj.remove(&gone);
            for (kk, (c, r)) in gone_adj {
                if kk == keep {
                    continue;
                }
                let e = k.adj.entry(kk).or_insert((0, 0));
                e.0 = e.0.saturating_add(c);
                e.1 = e.1.saturating_add(r);
            }
            k.segs.extend(g.segs);
            k.nrec = k.nrec.saturating_add(g.nrec);
            k.nums.absorb(g.nums);
            k.times = match (k.times, g.times) {
                (Some((a1, b1)), Some((a2, b2))) => Some((a1.min(a2), b1.max(b2))),
                (x, None) => x,
                (None, y) => y,
            };
            k.lowest = k.lowest.min(g.lowest);
            k.max_num = k.max_num.max(g.max_num);
            for (cs, n) in g.f1 {
                let e = k.f1.entry(cs).or_insert(0);
                *e = e.saturating_add(n);
            }
            k.rec0 = k.rec0.or(g.rec0);
            k.rec1 = k.rec1.or(g.rec1);
            k.rec0_exts.extend(g.rec0_exts);
            k.version = version;
            k.geo = None;
            k.embedded_in = None;
            k.cuts.extend(g.cuts);
            k.cuts.sort_unstable();
            k.cuts.dedup();
            for (f, m) in g.anchor_min {
                match k.anchor_min.iter_mut().find(|(ff, _)| *ff == f) {
                    Some(e) => e.1 = e.1.min(m),
                    None => k.anchor_min.push((f, m)),
                }
            }
        }
        keep
    }

    fn alive(&self, r: u32) -> bool {
        self.st(r).is_some()
    }

    fn version(&self, r: u32) -> u64 {
        self.st(r).map_or(0, |x| x.version)
    }

    /// The partners worth evaluating with `m` in `step`.
    fn partners(&mut self, m: u32, step: u8) -> Vec<u32> {
        let mut out: BTreeSet<u32> = BTreeSet::new();
        if step == 2 {
            if let Some(s) = self.st(m) {
                out.extend(s.adj.keys().copied());
            }
            let holders: Vec<u32> = self
                .pass_holders
                .iter()
                .copied()
                .filter(|&r| self.st(r).is_some_and(|s| s.rec0.is_some()))
                .collect();
            for x in holders {
                if x == m {
                    if let Some(p) = self.placement(m) {
                        for c in &p.counts {
                            out.extend(c.iter().filter(|(_, &n)| n >= MAP_MIN).map(|(&k, _)| k));
                        }
                    }
                } else if self.map_evidence(x, m) >= MAP_MIN {
                    out.insert(x);
                }
            }
        } else if self.validated_geo(m).is_some() {
            out.extend(self.pass_roots.iter().copied());
        } else {
            out.extend(
                self.pass_validated
                    .iter()
                    .copied()
                    .filter(|&r| self.validated_geo(r).is_some()),
            );
        }
        out.remove(&m);
        out.into_iter().filter(|&r| self.alive(r)).collect()
    }

    /// D4 joining with the evidence of `step` (2: a and c; 4: b and d)
    /// until no pair qualifies. Returns whether anything merged.
    pub fn join(
        &mut self,
        step: u8,
        heads: &mut Heads,
        cancel: &AtomicBool,
    ) -> Result<bool, HeadError> {
        type Entry = (Ev, Reverse<u32>, Reverse<u32>, u64, u64);
        let mut heap: BinaryHeap<Entry> = BinaryHeap::new();
        // The roots of this pass: partners are taken from these lists
        // instead of scanning every segment on every push.
        self.pass_roots = self.roots();
        self.pass_validated = self
            .pass_roots
            .iter()
            .copied()
            .filter(|&r| self.validated_geo(r).is_some())
            .collect();
        self.pass_holders = self
            .pass_roots
            .iter()
            .copied()
            .filter(|&r| self.st(r).is_some_and(|s| s.rec0.is_some()))
            .collect();
        let push = |w: &mut Work<'_>,
                    heap: &mut BinaryHeap<Entry>,
                    m: u32,
                    heads: &mut Heads|
         -> Result<(), HeadError> {
            for p in w.partners(m, step) {
                let (a, b) = (m.min(p), m.max(p));
                if let Some(ev) = w.evidence(a, b, step, heads, cancel)? {
                    heap.push((ev, Reverse(a), Reverse(b), w.version(a), w.version(b)));
                }
            }
            Ok(())
        };
        for r in self.roots() {
            push(self, &mut heap, r, heads)?;
        }
        let mut merged = false;
        while let Some((ev, Reverse(a), Reverse(b), va, vb)) = heap.pop() {
            if !self.alive(a) || !self.alive(b) || self.version(a) != va || self.version(b) != vb {
                continue;
            }
            if let Some(why) = self.forbidden(a, b, step, heads, cancel)? {
                let _ = why;
                continue;
            }
            let keep = self.merge(a, b);
            let gone = if keep == a { b } else { a };
            for h in &mut self.pass_holders {
                if *h == gone {
                    *h = keep;
                }
            }
            merged = true;
            self.log
                .push(format!("D4 step {step} merged {a}+{b} by {ev:?} -> {keep}"));
            push(self, &mut heap, keep, heads)?;
        }
        Ok(merged)
    }

    /// Whether an instance needs a geometry of its own (D5 step 3): at
    /// least 16 records, or it is an anchor.
    pub fn wants_geometry(&mut self, r: u32) -> bool {
        let n = self.st(r).map_or(0, |s| s.nrec);
        n >= MIN_RECORDS || self.anchor_roots().contains(&r)
    }

    /// The anchor instances: the holders of segment 0 of an anchor
    /// fragment.
    pub fn anchor_roots(&mut self) -> Vec<u32> {
        let segs: Vec<u32> = self
            .anchor_frags
            .iter()
            .filter_map(|&(f, _)| {
                self.frags
                    .fragments
                    .get(usize::try_from(f).ok()?)
                    .and_then(|fr| fr.segments.first())
                    .map(|s| s.0)
            })
            .collect();
        segs.into_iter().map(|s| self.find(s)).collect()
    }

    /// Computes the missing geometries (D5 steps 3 and 4).
    pub fn geometries(&mut self, heads: &mut Heads, cancel: &AtomicBool) -> Result<(), HeadError> {
        for r in self.roots() {
            if self.st(r).is_some_and(|s| s.geo.is_some()) || !self.wants_geometry(r) {
                continue;
            }
            let (members, files) = self.files_of(r);
            let lowest = self.st(r).map_or(0, |s| s.lowest);
            let g = geometry::compute(self.ctx, &members, &files, lowest, heads, cancel)?;
            if let Some(s) = self.st_mut(r) {
                s.geo = Some(g);
            }
        }
        Ok(())
    }

    /// D7 over all instances; returns whether an embedded relation changed.
    pub fn embedded(&mut self) -> bool {
        let store = self.store;
        let anchors = self.anchor_roots();
        let roots = self.roots();
        let validated: Vec<u32> = roots
            .iter()
            .copied()
            .filter(|&r| self.validated_geo(r).is_some())
            .collect();
        // For every X: (inside count, Y, container group base) of the best Y.
        let mut best: BTreeMap<u32, (u64, u32, RecordId, Option<u64>)> = BTreeMap::new();
        let by_offset = store.records_by_offset();
        for &y in &validated {
            let Some((cs, off)) = self.validated_geo(y) else {
                continue;
            };
            let cs64 = u64::from(cs);
            let (_, files) = self.files_of(y);
            // (record, X, container base)
            let mut hits: Vec<(RecordId, u32, RecordId)> = Vec::new();
            for (g, f) in geometry::group_facts(&files) {
                if !f.in_use() || f.is_dir() {
                    continue;
                }
                for vr in files::data_runs(store, &files.groups, g) {
                    let Some(l) = vr.run.lcn else { continue };
                    let Some(a) = l.checked_mul(cs64).and_then(|b| b.checked_add(off)) else {
                        continue;
                    };
                    let b = vr.run.length.saturating_mul(cs64).saturating_add(a);
                    let first = by_offset
                        .partition_point(|&id| store.record(id).map_or(0, |r| r.disk_offset()) < a);
                    for &id in by_offset.get(first..).unwrap_or(&[]) {
                        let Some(rec) = store.record(id) else {
                            continue;
                        };
                        if rec.disk_offset() >= b {
                            break;
                        }
                        if rec.end_offset() > b {
                            continue;
                        }
                        let Some(x) = self.root_of(id) else { continue };
                        if x != y {
                            hits.push((id, x, g.base));
                        }
                    }
                }
            }
            hits.sort_unstable_by_key(|&(id, _, _)| id);
            hits.dedup_by_key(|h| h.0);
            // Per X: inside count and per container counts.
            let mut per_x: BTreeMap<u32, BTreeMap<RecordId, (u64, Option<u64>)>> = BTreeMap::new();
            for &(id, x, c) in &hits {
                let t = self.timed.of(store, id);
                let e = per_x.entry(x).or_default().entry(c).or_insert((0, None));
                e.0 = e.0.saturating_add(1);
                if let Some(t) = t {
                    e.1 = Some(e.1.map_or(t, |o: u64| o.min(t)));
                }
            }
            for (x, conts) in per_x {
                let inside: u64 = conts.values().map(|v| v.0).sum();
                let nrec = self.st(x).map_or(0, |s| s.nrec);
                if inside.saturating_mul(10) < nrec.saturating_mul(9) || nrec == 0 {
                    continue;
                }
                let Some((&cont, &(_, oldest))) = conts
                    .iter()
                    .max_by(|p, q| p.1 .0.cmp(&q.1 .0).then(q.0.cmp(p.0)))
                else {
                    continue;
                };
                let better = best.get(&x).is_none_or(|b| inside > b.0);
                if better {
                    best.insert(x, (inside, y, cont, oldest));
                }
            }
        }
        let mut changed = false;
        for r in roots {
            let old = self.st(r).and_then(|s| s.embedded_in.map(|e| e.0));
            let mut emb = None;
            let mut stale = None;
            if let Some(&(_, y, cont, oldest)) = best.get(&r) {
                let ct = store
                    .record(cont)
                    .and_then(|c| c.mft_changed())
                    .unwrap_or(0);
                let too_old = oldest.is_some_and(|o| ct < o.saturating_sub(FT_DAY));
                if too_old || self.volume_of_disk(r) {
                    stale = Some((y, cont));
                } else if !anchors.contains(&r) {
                    emb = Some((y, cont));
                }
            }
            if let Some(s) = self.st_mut(r) {
                s.embedded_in = emb;
                s.stale = stale;
            }
            if old != emb.map(|e| e.0) {
                changed = true;
            }
        }
        changed
    }

    /// D7: `x` is a volume of the disk itself: its validated geometry puts
    /// its volume at the range start or a partition start, and fewer than
    /// 90 % of its records repeat records of another instance (D6 keys).
    fn volume_of_disk(&mut self, x: u32) -> bool {
        let Some((_, off)) = self.validated_geo(x) else {
            return false;
        };
        if !self.ctx.starts.contains(&off) {
            return false;
        }
        let members = self.members(x);
        let mut keys: HashMap<(u64, u64), bool> = HashMap::with_capacity(members.len());
        for &(n, id) in members.iter() {
            keys.insert(copy_key(self.store, n, id), false);
        }
        for (i, &n) in self.frags.num.iter().enumerate() {
            if n == NONE {
                continue;
            }
            let Ok(id) = u32::try_from(i).map(RecordId) else {
                continue;
            };
            match self.root_of(id) {
                Some(r) if r != x => {
                    if let Some(v) = keys.get_mut(&copy_key(self.store, n, id)) {
                        *v = true;
                    }
                }
                _ => {}
            }
        }
        let rep = members
            .iter()
            .filter(|&&(n, id)| {
                keys.get(&copy_key(self.store, n, id))
                    .copied()
                    .unwrap_or(false)
            })
            .count();
        u64::try_from(rep).unwrap_or(u64::MAX).saturating_mul(10)
            < u64::try_from(members.len()).unwrap_or(0).saturating_mul(9)
    }

    /// D5 steps 2 to 4.
    pub fn run(&mut self, heads: &mut Heads, cancel: &AtomicBool) -> Result<(), HeadError> {
        self.join(2, heads, cancel)?;
        self.geometries(heads, cancel)?;
        // Each pass merges or changes an embedded relation; merges are
        // bounded by the number of segments, and a pass without merges
        // that changes nothing ends the loop. The bound catches a relation
        // flipping back and forth.
        let bound = self.frags.segments.len().saturating_add(8);
        for _ in 0..bound {
            let changed = self.embedded();
            let merged = self.join(4, heads, cancel)?;
            self.geometries(heads, cancel)?;
            if !changed && !merged {
                break;
            }
        }
        Ok(())
    }

    /// The state of an instance root.
    pub fn state(&self, r: u32) -> Option<&IState> {
        self.st(r)
    }
}

/// The key of D6: number, sequence and `$STANDARD_INFORMATION` creation
/// time (for an extension record: the base reference).
pub(crate) fn copy_key(store: &ScanStore, num: u32, id: RecordId) -> (u64, u64) {
    let Some(r) = store.record(id) else {
        return (u64::MAX, u64::MAX);
    };
    let head = u64::from(num) << 16 | u64::from(r.sequence());
    match r.standard_information() {
        Some(si) => (head, si.times.created),
        None => (head | 1 << 63, r.base().raw()),
    }
}

/// The result of D6.
pub(crate) struct StrayResult {
    /// Roots that stay instances.
    pub kept: Vec<u32>,
    /// Roots merged into strays, with whether they were a copy set.
    pub strays: Vec<(u32, bool)>,
}

/// D6: instances under 16 records (other than an anchor) and copy sets go
/// to strays.
pub(crate) fn strays(w: &mut Work<'_>) -> StrayResult {
    let anchors = w.anchor_roots();
    let mut roots = w.roots();
    roots.sort_by_key(|&r| (Reverse(w.state(r).map_or(0, |s| s.nrec)), r));
    let size = |w: &Work<'_>, r: u32| w.state(r).map_or(0, |s| s.nrec);
    // Candidates for copy sets: every instance of 16 or more records but
    // the largest and the anchors.
    let largest = roots.first().copied();
    let cands: Vec<u32> = roots
        .iter()
        .copied()
        .filter(|&r| Some(r) != largest && !anchors.contains(&r) && size(w, r) >= MIN_RECORDS)
        .collect();
    // (key, candidate index, size of its instance)
    let mut entries: Vec<((u64, u64), usize, u64)> = Vec::new();
    for (ci, &r) in cands.iter().enumerate() {
        let n = size(w, r);
        for &(num, id) in w.members(r).iter() {
            entries.push((copy_key(w.store, num, id), ci, n));
        }
    }
    entries.sort_unstable();
    // The largest instance holding each key.
    let mut group_max: Vec<u64> = vec![0; entries.len()];
    if !entries.is_empty() {
        for &r in &roots {
            let n = size(w, r);
            for &(num, id) in w.members(r).iter() {
                let k = copy_key(w.store, num, id);
                let at = entries.partition_point(|e| e.0 < k);
                if entries.get(at).is_some_and(|e| e.0 == k) {
                    if let Some(g) = group_max.get_mut(at) {
                        *g = (*g).max(n);
                    }
                }
            }
        }
    }
    let mut repeated: Vec<u64> = vec![0; cands.len()];
    let mut i = 0usize;
    while i < entries.len() {
        let k = entries.get(i).map(|e| e.0);
        let gm = group_max.get(i).copied().unwrap_or(0);
        let mut j = i;
        while j < entries.len() && entries.get(j).map(|e| e.0) == k {
            if let Some(&(_, ci, n)) = entries.get(j) {
                if gm > n {
                    if let Some(c) = repeated.get_mut(ci) {
                        *c = c.saturating_add(1);
                    }
                }
            }
            j = j.saturating_add(1);
        }
        i = j;
    }
    let mut out = StrayResult {
        kept: Vec::new(),
        strays: Vec::new(),
    };
    for &r in &roots {
        let n = size(w, r);
        if anchors.contains(&r) {
            out.kept.push(r);
            continue;
        }
        if n < MIN_RECORDS {
            out.strays.push((r, false));
            continue;
        }
        let copy = cands
            .iter()
            .position(|&c| c == r)
            .and_then(|ci| repeated.get(ci))
            .is_some_and(|&rep| rep.saturating_mul(10) >= n.saturating_mul(9));
        if copy {
            out.strays.push((r, true));
        } else {
            out.kept.push(r);
        }
    }
    out.kept.sort_unstable();
    out.strays.sort_unstable();
    out
}
