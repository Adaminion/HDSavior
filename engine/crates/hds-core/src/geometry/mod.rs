//! Geometry per instance (ALGORITHMS F): cluster size (F1), offset
//! candidates (F2), validation by file signatures (F3), the choice (F4),
//! the volume size and the lost-volume range (F5), the validated verdict
//! (F6) and the manual override (F7, applied by
//! [`Layout::apply`](crate::instances::Layout::apply)).
//!
//! A geometry is `(cluster_size, volume_offset)`; `volume_offset` is the
//! absolute byte offset of cluster 0 of the instance's volume. The
//! computation is driven by [`crate::instances`] (ALGORITHMS D5 step 3 and
//! 4); the heads it reads are kept in [`Heads`] so that a catalog built
//! from a loaded session without a source comes out the same.
//!
//! Choices where the contract is silent (see the crate report):
//!
//! * F1 ties: the smaller cluster size.
//! * F2 MFT map ties: the lower offset.
//! * F3 "the best candidates (at most 8)": ranked by distinct voting
//!   directories, signature votes, the source order of F4 (`mft`; the
//!   boot readings, one consistent with its hidden sectors first, then
//!   `boot`, then `backup_boot`; `default`; then `index` and `signature`),
//!   agreeing MFT records, and the lower offset. Only the best
//!   [`MAX_CANDIDATES`] are kept.
//! * F3 sample: the files checkable under the candidate (first cluster
//!   inside the session range, outside the overwritten set) are sampled,
//!   evenly over their record numbers.
//! * F4 "evidence of their own": index votes, an MFT map, signature votes
//!   or at least one matching signature.

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

mod heads;
#[cfg(test)]
mod tests;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use hds_carve::{kinds_for_extension, matches_extension, FileKind, SNIFF_LEN};
use hds_io::Protected;
use serde::Serialize;

pub use heads::{Head, HeadError, Heads};

use crate::instances::files::{self, Group, GroupIndex, VRun};
use crate::live::LiveIndex;
use crate::store::{carve_kind_code, DataExtent, RecordId, ScanStore};

/// F3: at most this many candidates are checked.
pub const CHECK_CANDIDATES: usize = 8;
/// F3: at most this many files are checked per candidate.
pub const CHECK_FILES: usize = 200;
/// F4/F6: signature matches decide from this many checked files on.
pub const MIN_CHECKED: u64 = 20;
/// F2: signature anchors use at most this many files.
pub const ANCHOR_FILES: usize = 64;
/// F2: a signature-anchored offset needs this many votes.
pub const ANCHOR_VOTES: u64 = 3;
/// F6: structural evidence needs this many voting directories.
pub const STRUCT_DIRS: u64 = 3;
/// F6: or an MFT map with this many agreeing records.
pub const STRUCT_MFT: u64 = 16;
/// F5: `end_by_files` is the value of this rank.
pub const END_RANK: usize = 4;
/// At most this many candidates of an instance are kept
/// ([`crate::instances::Instance::candidates`], the protocol's
/// `geometry_candidates`): the best by F3's ranking, which include every
/// checked one. A crafted disk can produce millions of candidate offsets
/// (boot sectors, signature anchors); F7 gathers the evidence of any other
/// pair itself.
pub const MAX_CANDIDATES: usize = 16;
/// Long loops poll the cancel flag every this many steps.
const CANCEL_POLL: u32 = 1 << 16;

/// Where a cluster size came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterSource {
    /// The most frequent allocated size per cluster count (F1).
    Attr,
    /// A boot sector at the range start or a partition start.
    Boot,
    /// Nothing qualified: 4096.
    Default,
    /// Set by the user (F7).
    Manual,
}

/// Where a volume offset came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OffsetSource {
    /// The MFT map of the instance's record 0.
    Mft,
    /// A primary boot sector.
    Boot,
    /// A backup boot sector.
    BackupBoot,
    /// The range start or a partition start.
    Default,
    /// Index-block votes.
    Index,
    /// Signature anchors.
    Signature,
    /// Taken over from the main instance (F4).
    Inherited,
    /// Set by the user (F7).
    Manual,
}

impl OffsetSource {
    /// The wire name.
    pub fn name(self) -> &'static str {
        match self {
            OffsetSource::Mft => "mft",
            OffsetSource::Boot => "boot",
            OffsetSource::BackupBoot => "backup_boot",
            OffsetSource::Default => "default",
            OffsetSource::Index => "index",
            OffsetSource::Signature => "signature",
            OffsetSource::Inherited => "inherited",
            OffsetSource::Manual => "manual",
        }
    }

    fn bit(self) -> u8 {
        match self {
            OffsetSource::Mft => 1,
            OffsetSource::Boot => 2,
            OffsetSource::BackupBoot => 4,
            OffsetSource::Default => 8,
            OffsetSource::Index => 16,
            OffsetSource::Signature => 32,
            OffsetSource::Inherited => 64,
            OffsetSource::Manual => 128,
        }
    }
}

/// The order of F4 among the structural sources.
const SOURCE_ORDER: [OffsetSource; 6] = [
    OffsetSource::Mft,
    OffsetSource::Boot,
    OffsetSource::BackupBoot,
    OffsetSource::Default,
    OffsetSource::Index,
    OffsetSource::Signature,
];

/// One geometry with its evidence (the protocol's `Geometry` object plus
/// the F5 values).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Geometry {
    /// Cluster size in bytes.
    pub cluster_size: u32,
    /// Absolute byte offset of cluster 0.
    pub volume_offset: u64,
    /// Where the cluster size came from.
    pub cluster_source: ClusterSource,
    /// Where the offset came from (the reported source).
    pub offset_source: OffsetSource,
    /// Distinct directories whose index blocks voted for this offset.
    pub index_dirs: u64,
    /// Index blocks that voted for it.
    pub index_blocks: u64,
    /// Records the MFT map places here, besides record 0 itself.
    pub mft_agree: u64,
    /// Files whose signature anchors voted for it.
    pub signature_votes: u64,
    /// Files checked (F3).
    pub signature_checked: u64,
    /// Of those, files whose signature matched their extension.
    pub signature_ok: u64,
    /// F6.
    pub validated: bool,
    /// F5: volume size in clusters (decides `out_of_bounds`).
    pub total_clusters: u64,
    /// F5: the 4th-largest file end.
    pub end_by_files: u64,
    /// F5: the largest file end.
    pub end_of_all: u64,
    /// F5: the lost-volume range (clipped to the source).
    pub lost_volume: Range<u64>,
}

impl Geometry {
    /// Structural evidence of F6.
    pub fn structural(&self) -> bool {
        self.index_dirs >= STRUCT_DIRS || self.mft_agree >= STRUCT_MFT
    }

    /// F4's "evidence of its own".
    pub fn own_evidence(&self) -> bool {
        self.index_dirs > 0
            || self.mft_agree > 0
            || self.signature_votes > 0
            || self.signature_ok > 0
    }

    /// The lost-volume range as a guard input.
    pub fn protected(&self) -> Protected {
        Protected::lost_volume(self.lost_volume.clone())
    }

    fn validate(&mut self) {
        let checked = self.signature_checked;
        let ok = self.signature_ok;
        self.validated = if checked >= MIN_CHECKED {
            ok.saturating_mul(5) >= checked.saturating_mul(4)
                || (ok.saturating_mul(2) >= checked && self.structural())
        } else {
            self.structural()
        };
    }
}

/// What every geometry computation of one analysis shares.
pub(crate) struct GeoContext<'a> {
    pub store: &'a ScanStore,
    pub live: &'a LiveIndex,
    pub range: Range<u64>,
    pub source_size: u64,
    pub sector: u64,
    /// The range start and the partition starts inside the range.
    pub starts: Vec<u64>,
    /// Index blocks by owner record number: (number, position).
    blocks_by_owner: Vec<(u64, usize)>,
    /// Carve hit offsets per kind code, ascending (built on first use).
    carve: OnceLock<Vec<Vec<u64>>>,
    /// F5: per (cluster size, volume offset) the largest size in bytes of
    /// a boot hit that agrees with it, as a primary (at the offset) or as
    /// a backup (size bytes behind it).
    boot_sizes: BTreeMap<(u32, u64), u64>,
}

impl<'a> GeoContext<'a> {
    pub fn new(
        store: &'a ScanStore,
        live: &'a LiveIndex,
        range: Range<u64>,
        source_size: u64,
        sector: u64,
        starts: Vec<u64>,
    ) -> GeoContext<'a> {
        let mut blocks_by_owner: Vec<(u64, usize)> = store
            .index_blocks()
            .iter()
            .enumerate()
            .filter_map(|(i, b)| b.owner.map(|o| (o.number(), i)))
            .collect();
        blocks_by_owner.sort_unstable();
        let mut boot_sizes: BTreeMap<(u32, u64), u64> = BTreeMap::new();
        for bh in store.boot_hits() {
            let size = bh
                .boot
                .total_sectors
                .saturating_mul(u64::from(bh.boot.bytes_per_sector));
            let readings = [Some(bh.disk_offset), bh.disk_offset.checked_sub(size)];
            for at in readings.into_iter().flatten() {
                let e = boot_sizes.entry((bh.boot.cluster_size, at)).or_insert(0);
                *e = (*e).max(size);
            }
        }
        GeoContext {
            store,
            live,
            range,
            source_size,
            sector: sector.max(1),
            starts,
            blocks_by_owner,
            carve: OnceLock::new(),
            boot_sizes,
        }
    }

    /// The largest size (bytes) of a boot hit with cluster size `cs` that
    /// agrees with volume offset `off` (F5).
    fn boot_size_at(&self, cs: u32, off: u64) -> Option<u64> {
        self.boot_sizes.get(&(cs, off)).copied()
    }

    fn carve_by_kind(&self) -> &Vec<Vec<u64>> {
        self.carve.get_or_init(|| {
            let mut v: Vec<Vec<u64>> = vec![Vec::new(); 256];
            for h in self.store.carve_hits() {
                if let Some(list) = v.get_mut(usize::from(carve_kind_code(h.kind()))) {
                    list.push(h.disk_offset());
                }
            }
            for list in &mut v {
                list.sort_unstable();
            }
            v
        })
    }

    fn valid_offset(&self, off: i128) -> Option<u64> {
        let off = u64::try_from(off).ok()?;
        (off.checked_rem(self.sector) == Some(0) && off < self.source_size).then_some(off)
    }
}

/// A file whose first sector can be checked (F3), before the candidate's
/// own conditions. 24 bytes; the extension is an index into
/// [`InstanceFiles::exts`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Checkable {
    pub first_lcn: u64,
    pub real_size: u64,
    pub ext: u32,
}

/// What D7 and F need of one group, compactly (16 bytes).
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileBits {
    bits: u8,
    first_lcn: u64,
}

impl FileBits {
    const IN_USE: u8 = 1;
    const DIR: u8 = 2;

    pub fn in_use(&self) -> bool {
        self.bits & Self::IN_USE != 0
    }

    pub fn is_dir(&self) -> bool {
        self.bits & Self::DIR != 0
    }

    pub fn first_lcn(&self) -> Option<u64> {
        (self.first_lcn != u64::MAX).then_some(self.first_lcn)
    }
}

/// The facts of an instance that every geometry of it needs.
pub(crate) struct InstanceFiles {
    pub groups: GroupIndex,
    /// Per group (same order as `groups.groups`).
    pub bits: Vec<FileBits>,
    /// Checkable files by record number (F3 before the candidate's
    /// conditions).
    pub checkable: Vec<Checkable>,
    /// The extensions `Checkable::ext` refers to.
    pub exts: Vec<String>,
    /// The largest [`END_RANK`] file ends (highest mapped cluster + 1) of
    /// in-use files with intact fixups and complete runlists, descending;
    /// fewer when there are fewer such files.
    pub ends: Vec<u64>,
}

impl InstanceFiles {
    pub fn build(store: &ScanStore, members: &[(u32, RecordId)]) -> InstanceFiles {
        let groups = GroupIndex::build(store, members);
        let mut bits = Vec::with_capacity(groups.groups.len());
        let mut checkable = Vec::new();
        let mut exts: Vec<String> = Vec::new();
        let mut ends: Vec<u64> = Vec::new();
        for g in &groups.groups {
            let f = files::facts(store, &groups, g);
            if f.in_use && f.fixups_ok && f.complete && f.end_cluster > 0 {
                // Keep the largest END_RANK ends, descending.
                let at = ends.partition_point(|&e| e >= f.end_cluster);
                if at < END_RANK {
                    ends.insert(at, f.end_cluster);
                    ends.truncate(END_RANK);
                }
            }
            if f.in_use
                && !f.is_dir
                && f.has_data
                && !f.sparse
                && !f.compressed
                && !f.encrypted
                && f.fixups_ok
            {
                if let Some(l) = f.first_lcn {
                    if let Some(ext) = files::best_extension(store, &groups, g) {
                        if !kinds_for_extension(&ext).is_empty() {
                            let k = match exts.iter().position(|e| *e == ext) {
                                Some(k) => k,
                                None => {
                                    exts.push(ext);
                                    exts.len().saturating_sub(1)
                                }
                            };
                            checkable.push(Checkable {
                                first_lcn: l,
                                real_size: f.real_size.unwrap_or(u64::MAX),
                                ext: u32::try_from(k).unwrap_or(u32::MAX),
                            });
                        }
                    }
                }
            }
            let mut b = 0u8;
            if f.in_use {
                b |= FileBits::IN_USE;
            }
            if f.is_dir {
                b |= FileBits::DIR;
            }
            bits.push(FileBits {
                bits: b,
                first_lcn: f.first_lcn.unwrap_or(u64::MAX),
            });
        }
        InstanceFiles {
            groups,
            bits,
            checkable,
            exts,
            ends,
        }
    }
}

/// F1's histogram for a set of records: cluster size -> attributes.
pub(crate) fn f1_histogram(
    store: &ScanStore,
    ids: impl Iterator<Item = RecordId>,
) -> BTreeMap<u32, u64> {
    let mut h = BTreeMap::new();
    for id in ids {
        let mut add = |start_vcn: u64, last_vcn: i64, alloc: Option<u64>| {
            if start_vcn != 0 || last_vcn < 0 {
                return;
            }
            let (Some(alloc), Ok(n)) = (alloc, u64::try_from(last_vcn)) else {
                return;
            };
            let Some(n) = n.checked_add(1) else { return };
            if alloc.checked_rem(n) != Some(0) {
                return;
            }
            let Some(cs) = alloc.checked_div(n) else {
                return;
            };
            if cs.is_power_of_two() && (512..=(2 << 20)).contains(&cs) {
                if let Ok(cs) = u32::try_from(cs) {
                    let e = h.entry(cs).or_insert(0u64);
                    *e = e.saturating_add(1);
                }
            }
        };
        for e in store.data_extents(id) {
            if let DataExtent::NonResident(x) = e {
                add(x.start_vcn, x.last_vcn, x.sizes.map(|s| s.allocated));
            }
        }
        for x in store.index_allocation_extents(id) {
            add(x.start_vcn, x.last_vcn, x.sizes.map(|s| s.allocated));
        }
    }
    h
}

/// F1's choice from a histogram: the most frequent, the smaller on ties.
pub(crate) fn f1_choice(h: &BTreeMap<u32, u64>) -> Option<u32> {
    let mut best: Option<(u64, u32)> = None;
    for (&cs, &n) in h {
        if best.is_none_or(|(bn, _)| n > bn) {
            best = Some((n, cs));
        }
    }
    best.map(|(_, cs)| cs)
}

/// F1 with its fallback: the boot hit at the range start or a partition
/// start nearest below `lowest` (the instance's lowest record offset).
pub(crate) fn cluster_size(
    ctx: &GeoContext<'_>,
    hist: &BTreeMap<u32, u64>,
    lowest: u64,
) -> (u32, ClusterSource) {
    if let Some(cs) = f1_choice(hist) {
        return (cs, ClusterSource::Attr);
    }
    let boot = ctx
        .store
        .boot_hits()
        .iter()
        .filter(|b| ctx.starts.contains(&b.disk_offset) && b.disk_offset <= lowest)
        .max_by_key(|b| b.disk_offset);
    match boot {
        Some(b) => (b.boot.cluster_size, ClusterSource::Boot),
        None => (4096, ClusterSource::Default),
    }
}

/// The MFT map of an instance (F2, D4 c): its record 0 named `$MFT` and
/// the runs of `$MFT:$DATA` from it and the instance's extension records
/// of it.
pub(crate) struct MftMap {
    pub rec0: RecordId,
    pub runs: Vec<VRun>,
    /// Initialized size of `$MFT:$DATA` (VCN-0 extent).
    pub initialized: Option<u64>,
}

/// Whether a record carries the name `$MFT`.
pub(crate) fn is_mft_record(store: &ScanStore, id: RecordId) -> bool {
    store.names(id).any(|n| n.name.eq_str("$MFT"))
}

impl MftMap {
    /// The map of the instance whose members (number, record) are given;
    /// `None` without a record 0 named `$MFT`.
    pub fn of(store: &ScanStore, members: &[(u32, RecordId)]) -> Option<MftMap> {
        let &(_, rec0) = members
            .first()
            .filter(|&&(n, id)| n == 0 && is_mft_record(store, id))?;
        let r0 = store.record(rec0)?;
        let mut exts = Vec::new();
        let mut initialized = None;
        for e in store.data_extents(rec0) {
            if let DataExtent::NonResident(x) = e {
                if let Some(s) = x.sizes {
                    initialized = Some(s.initialized);
                }
                exts.push(x);
            }
        }
        for &(n, id) in members {
            if n == 0 {
                continue;
            }
            let Some(r) = store.record(id) else { continue };
            let b = r.base();
            if r.is_extension() && b.number() == 0 && b.resolves_to(r0.sequence(), r0.is_in_use()) {
                for e in store.data_extents(id) {
                    if let DataExtent::NonResident(x) = e {
                        exts.push(x);
                    }
                }
            }
        }
        exts.sort_by_key(|e| e.start_vcn);
        let mut runs = Vec::new();
        for e in &exts {
            let mut vcn = e.start_vcn;
            for run in e.runs() {
                runs.push(VRun { vcn, run });
                match vcn.checked_add(run.length) {
                    Some(v) => vcn = v,
                    None => break,
                }
            }
        }
        Some(MftMap {
            rec0,
            runs,
            initialized,
        })
    }

    /// The volume byte of record `n` of size `size` under cluster size `cs`.
    pub fn place(&self, n: u64, size: u32, cs: u64) -> Option<u64> {
        files::map_pos(&self.runs, cs, n.checked_mul(u64::from(size))?)
    }
}

/// F3's ranking of candidates: distinct voting directories, signature
/// votes, the source order ([`Cand::source_rank`]), agreeing MFT records
/// (each descending except the source order).
type RankKey = (Reverse<u64>, Reverse<u64>, usize, Reverse<u64>);

/// One offset candidate under construction.
#[derive(Debug, Clone, Default)]
struct Cand {
    sources: u8,
    dirs: u64,
    blocks: u64,
    mft_agree: u64,
    sig: u64,
    boot_pref: bool,
    checked: u64,
    matched: u64,
    evaluated: bool,
}

impl Cand {
    fn has(&self, s: OffsetSource) -> bool {
        self.sources & s.bit() != 0
    }

    /// F4's source order: `mft`, then the boot readings — one consistent
    /// with its hidden sectors first (F2), then `boot`, then
    /// `backup_boot` — then `default`, `index`, `signature`.
    fn source_rank(&self) -> usize {
        let boot = self.has(OffsetSource::Boot) || self.has(OffsetSource::BackupBoot);
        if self.has(OffsetSource::Mft) {
            0
        } else if boot && self.boot_pref {
            1
        } else {
            SOURCE_ORDER
                .iter()
                .position(|&s| self.has(s))
                .map_or(SOURCE_ORDER.len().saturating_add(1), |p| {
                    p.saturating_add(1)
                })
        }
    }

    /// F3's ranking key (smaller is better; the offset breaks ties).
    fn rank_key(&self) -> RankKey {
        (
            Reverse(self.dirs),
            Reverse(self.sig),
            self.source_rank(),
            Reverse(self.mft_agree),
        )
    }

    fn reported_source(&self) -> OffsetSource {
        if self.checked >= MIN_CHECKED
            && self.sig > 0
            && self.has(OffsetSource::Signature)
            && self.dirs == 0
        {
            OffsetSource::Signature
        } else if self.dirs > 0 {
            OffsetSource::Index
        } else {
            SOURCE_ORDER
                .iter()
                .copied()
                .find(|&s| self.has(s))
                .unwrap_or(OffsetSource::Default)
        }
    }
}

/// What [`compute`] returns: the chosen geometry and every candidate.
#[derive(Debug, Clone)]
pub(crate) struct GeoResult {
    pub chosen: Geometry,
    pub candidates: Vec<Geometry>,
}

/// F3 for one geometry over a set of checkable files: `(checked,
/// matched)`. Files whose first cluster lies outside the session range or
/// touches the overwritten set are not checkable under it; of the rest up
/// to [`CHECK_FILES`] are sampled evenly.
pub(crate) fn content_check(
    ctx: &GeoContext<'_>,
    cs: u32,
    off: u64,
    inst_files: &InstanceFiles,
    heads: &mut Heads,
    cancel: &AtomicBool,
) -> Result<(u64, u64), HeadError> {
    let files = &inst_files.checkable;
    let cs64 = u64::from(cs);
    let mut usable: Vec<(u64, &Checkable)> = Vec::new();
    for f in files {
        let Some(at) = f
            .first_lcn
            .checked_mul(cs64)
            .and_then(|b| b.checked_add(off))
        else {
            continue;
        };
        let Some(end) = at.checked_add(cs64) else {
            continue;
        };
        if at < ctx.range.start || end > ctx.range.end || ctx.live.overwritten().intersects(at..end)
        {
            continue;
        }
        usable.push((at, f));
    }
    let n = usable.len();
    let take = n.min(CHECK_FILES);
    let (mut checked, mut matched) = (0u64, 0u64);
    for i in 0..take {
        let k = i
            .checked_mul(n)
            .and_then(|x| x.checked_div(take))
            .unwrap_or(0);
        let Some(&(at, f)) = usable.get(k) else {
            continue;
        };
        let Some(Head::Bytes(head)) = heads.get(at, cancel)? else {
            continue;
        };
        let len = usize::try_from(f.real_size)
            .unwrap_or(usize::MAX)
            .min(SNIFF_LEN);
        let Some(ext) = inst_files
            .exts
            .get(usize::try_from(f.ext).unwrap_or(usize::MAX))
        else {
            continue;
        };
        let Some(m) = matches_extension(head.get(..len).unwrap_or(&head), ext) else {
            continue;
        };
        checked = checked.saturating_add(1);
        if m {
            matched = matched.saturating_add(1);
        }
    }
    Ok((checked, matched))
}

/// How [`gather`] collects signature anchors.
#[derive(Debug, Clone, Copy)]
enum SigMode {
    /// Keep the best `n` anchor offsets by the F3 ranking. Signature
    /// anchors exist only without index votes, so a candidate with
    /// signature votes ranks above every candidate without: the best `n`
    /// candidates overall are exactly what they would be with every
    /// anchor offset kept, evidence included.
    Top(usize),
    /// Count the votes for this one offset only (F7).
    At(u64),
}

/// F2 for one instance under cluster size `cs`: every offset candidate
/// with its evidence (MFT map, index votes, boot sectors, starts, and the
/// signature anchors `sig` asks for).
fn gather(
    ctx: &GeoContext<'_>,
    members: &[(u32, RecordId)],
    inst_files: &InstanceFiles,
    cs: u32,
    sig: SigMode,
    cancel: &AtomicBool,
) -> Result<BTreeMap<u64, Cand>, HeadError> {
    let store = ctx.store;
    let cs64 = u64::from(cs);
    let mut cands: BTreeMap<u64, Cand> = BTreeMap::new();
    fn add(
        cands: &mut BTreeMap<u64, Cand>,
        ctx: &GeoContext<'_>,
        off: i128,
        src: OffsetSource,
    ) -> Option<u64> {
        let off = ctx.valid_offset(off)?;
        cands.entry(off).or_default().sources |= src.bit();
        Some(off)
    }

    // MFT map.
    let mut mft_best: Option<(u64, u64)> = None;
    if let Some(map) = MftMap::of(store, members) {
        let mut cnt: HashMap<i128, u64> = HashMap::new();
        for &(n, id) in members {
            let Some(r) = store.record(id) else { continue };
            let Some(b) = map.place(u64::from(n), r.record_size(), cs64) else {
                continue;
            };
            let d = i128::from(r.disk_offset()).saturating_sub(i128::from(b));
            let e = cnt.entry(d).or_insert(0);
            *e = e.saturating_add(1);
        }
        let best = cnt
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .map(|(&o, &c)| (o, c));
        if let Some((o, c)) = best {
            let self_agrees = store
                .record(map.rec0)
                .zip(map.place(0, 1024, cs64))
                .is_some_and(|(r0, b0)| {
                    i128::from(r0.disk_offset()).saturating_sub(i128::from(b0)) == o
                });
            if let Some(off) = add(&mut cands, ctx, o, OffsetSource::Mft) {
                let agree = c.saturating_sub(u64::from(self_agrees));
                mft_best = Some((off, agree));
            }
        }
    }
    if let Some((off, agree)) = mft_best {
        if let Some(c) = cands.get_mut(&off) {
            c.mft_agree = c.mft_agree.max(agree);
        }
    }

    // Index votes, per distinct directory.
    let mut votes: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
    let blocks = store.index_blocks();
    for (gi, g) in inst_files.groups.groups.iter().enumerate() {
        let Some(f) = inst_files.bits.get(gi) else {
            continue;
        };
        if !f.is_dir() || g.orphan {
            continue;
        }
        let Some(base) = store.record(g.base) else {
            continue;
        };
        let first = ctx
            .blocks_by_owner
            .partition_point(|&(n, _)| n < u64::from(g.number));
        let mut runs: Option<Vec<VRun>> = None;
        let mut voted: Vec<u64> = Vec::new();
        for &(n, pos) in ctx.blocks_by_owner.get(first..).unwrap_or(&[]) {
            if n != u64::from(g.number) {
                break;
            }
            if ctx.live.is_current_block(pos) {
                continue;
            }
            let Some(b) = blocks.get(pos) else { continue };
            let Some(owner) = b.owner else { continue };
            if !owner.resolves_to(base.sequence(), base.is_in_use()) {
                continue;
            }
            let runs = runs.get_or_insert_with(|| files::index_runs(store, &inst_files.groups, g));
            if runs.is_empty() {
                break;
            }
            let unit = if cs64 <= u64::from(b.size) { cs64 } else { 512 };
            let Some(p) = b.vcn.checked_mul(unit) else {
                continue;
            };
            let Some(vb) = files::map_pos(runs, cs64, p) else {
                continue;
            };
            let d = i128::from(b.disk_offset).saturating_sub(i128::from(vb));
            let Some(off) = ctx.valid_offset(d) else {
                continue;
            };
            let e = votes.entry(off).or_insert((0, 0));
            e.1 = e.1.saturating_add(1);
            if !voted.contains(&off) {
                voted.push(off);
                e.0 = e.0.saturating_add(1);
            }
        }
    }
    for (&off, &(dirs, blks)) in &votes {
        if let Some(o) = add(&mut cands, ctx, i128::from(off), OffsetSource::Index) {
            if let Some(c) = cands.get_mut(&o) {
                c.dirs = c.dirs.max(dirs);
                c.blocks = c.blocks.max(blks);
            }
        }
    }

    // Boot sectors: both readings, the one consistent with the hidden
    // sectors preferred.
    for bh in store.boot_hits() {
        if bh.boot.cluster_size != cs {
            continue;
        }
        let x = i128::from(bh.disk_offset);
        let size =
            i128::from(bh.boot.total_sectors).saturating_mul(i128::from(bh.boot.bytes_per_sector));
        let back = x.saturating_sub(size);
        let hidden =
            i128::from(bh.boot.hidden_sectors).saturating_mul(i128::from(bh.boot.bytes_per_sector));
        let (pc, bc) = (hidden == x, hidden == back);
        if let Some(o) = add(&mut cands, ctx, x, OffsetSource::Boot) {
            if let Some(c) = cands.get_mut(&o) {
                c.boot_pref |= pc && !bc;
            }
        }
        if let Some(o) = add(&mut cands, ctx, back, OffsetSource::BackupBoot) {
            if let Some(c) = cands.get_mut(&o) {
                c.boot_pref |= bc && !pc;
            }
        }
    }

    // Partition starts and the range start.
    for &s in &ctx.starts {
        add(&mut cands, ctx, i128::from(s), OffsetSource::Default);
    }

    // Signature anchors, only without index votes.
    if votes.is_empty() {
        let anchors = anchor_files(ctx, inst_files, cs64);
        let mut found: Vec<(u64, u64)> = Vec::new();
        match sig {
            SigMode::At(off) => {
                let n = anchor_votes_at(ctx, &anchors, off);
                if n >= ANCHOR_VOTES {
                    found.push((off, n));
                }
            }
            SigMode::Top(keep) => {
                // The `keep` best by the F3 ranking key, in a bounded
                // max-heap (the worst kept one on top).
                let mut best: BinaryHeap<(RankKey, u64, u64)> = BinaryHeap::new();
                merge_anchor_votes(ctx, &anchors, cancel, &mut |off, n| {
                    if ctx.valid_offset(i128::from(off)).is_none() {
                        return;
                    }
                    let mut c = cands.get(&off).cloned().unwrap_or_default();
                    c.sources |= OffsetSource::Signature.bit();
                    c.sig = c.sig.max(n);
                    best.push((c.rank_key(), off, n));
                    if best.len() > keep {
                        best.pop();
                    }
                })?;
                found.extend(best.into_iter().map(|(_, off, n)| (off, n)));
            }
        }
        for (off, n) in found {
            if let Some(o) = add(&mut cands, ctx, i128::from(off), OffsetSource::Signature) {
                if let Some(c) = cands.get_mut(&o) {
                    c.sig = c.sig.max(n);
                }
            }
        }
    }
    Ok(cands)
}

/// F1-F6 for one instance. `members` are its records (number, id) by
/// number; `lowest` its lowest record offset. The result keeps the chosen
/// geometry and the best [`MAX_CANDIDATES`] candidates in F3's ranking
/// order (F5 is computed for those only).
pub(crate) fn compute(
    ctx: &GeoContext<'_>,
    members: &[(u32, RecordId)],
    inst_files: &InstanceFiles,
    lowest: u64,
    heads: &mut Heads,
    cancel: &AtomicBool,
) -> Result<GeoResult, HeadError> {
    let hist = f1_histogram(ctx.store, members.iter().map(|&(_, id)| id));
    let (cs, cluster_source) = cluster_size(ctx, &hist, lowest);
    let mut cands = gather(
        ctx,
        members,
        inst_files,
        cs,
        SigMode::Top(MAX_CANDIDATES.max(CHECK_CANDIDATES)),
        cancel,
    )?;

    // F3 on the best candidates.
    let mut ranked: Vec<(RankKey, u64)> = cands.iter().map(|(&o, c)| (c.rank_key(), o)).collect();
    ranked.sort_unstable();
    ranked.truncate(MAX_CANDIDATES.max(CHECK_CANDIDATES));
    let order: Vec<u64> = ranked.into_iter().map(|(_, o)| o).collect();
    for &off in order.iter().take(CHECK_CANDIDATES) {
        let (checked, matched) = content_check(ctx, cs, off, inst_files, heads, cancel)?;
        if let Some(c) = cands.get_mut(&off) {
            c.checked = checked;
            c.matched = matched;
            c.evaluated = true;
        }
    }
    // F4.
    let sigkey = |c: &Cand| -> i128 {
        if c.checked >= MIN_CHECKED {
            i128::from(c.matched)
        } else {
            -1
        }
    };
    let chosen_off = order
        .iter()
        .take(CHECK_CANDIDATES)
        .filter_map(|&o| {
            let c = cands.get(&o)?;
            Some(((Reverse(sigkey(c)), Reverse(c.dirs), c.source_rank()), o))
        })
        .min()
        .map_or(ctx.range.start, |(_, o)| o);

    let make = |off: u64, c: &Cand| -> Geometry {
        let mut g = Geometry {
            cluster_size: cs,
            volume_offset: off,
            cluster_source,
            offset_source: c.reported_source(),
            index_dirs: c.dirs,
            index_blocks: c.blocks,
            mft_agree: c.mft_agree,
            signature_votes: c.sig,
            signature_checked: c.checked,
            signature_ok: c.matched,
            validated: false,
            total_clusters: 0,
            end_by_files: 0,
            end_of_all: 0,
            lost_volume: off..off,
        };
        g.validate();
        volume_size(ctx, &inst_files.ends, &mut g);
        g
    };
    let default_cand = Cand {
        sources: OffsetSource::Default.bit(),
        ..Cand::default()
    };
    let chosen = make(chosen_off, cands.get(&chosen_off).unwrap_or(&default_cand));
    let candidates = order
        .iter()
        .take(MAX_CANDIDATES)
        .filter_map(|o| cands.get(o).map(|c| make(*o, c)))
        .collect();
    Ok(GeoResult { chosen, candidates })
}

/// The files of F2's signature anchors: up to [`ANCHOR_FILES`] in-use
/// files with a known signature and a first cluster, spread over the
/// record numbers, as (first cluster x `cs`, the kinds their extension
/// promises).
fn anchor_files(
    ctx: &GeoContext<'_>,
    inst_files: &InstanceFiles,
    cs: u64,
) -> Vec<(u64, &'static [FileKind])> {
    let store = ctx.store;
    let mut files: Vec<(u64, &'static [FileKind])> = Vec::new();
    for (gi, g) in inst_files.groups.groups.iter().enumerate() {
        let Some(f) = inst_files.bits.get(gi) else {
            continue;
        };
        if !f.in_use() || f.is_dir() {
            continue;
        }
        let Some(l) = f.first_lcn() else { continue };
        let Some(ext) = files::best_extension(store, &inst_files.groups, g) else {
            continue;
        };
        let kinds = kinds_for_extension(&ext);
        if kinds.is_empty() {
            continue;
        }
        files.push((l, kinds));
    }
    let n = files.len();
    let take = n.min(ANCHOR_FILES);
    let mut out = Vec::with_capacity(take);
    for i in 0..take {
        let k = i
            .checked_mul(n)
            .and_then(|x| x.checked_div(take))
            .unwrap_or(0);
        let Some(&(l, kinds)) = files.get(k) else {
            continue;
        };
        let Some(shift) = l.checked_mul(cs) else {
            continue;
        };
        out.push((shift, kinds));
    }
    out
}

/// F2 signature anchors: every offset `hit - shift` that at least
/// [`ANCHOR_VOTES`] anchor files vote for, in ascending order, with its
/// number of files, handed to `found`.
///
/// A k-way merge by cursors over the per-kind sorted hit lists, shifted on
/// the fly: memory is one heap entry per (file, kind), whatever the number
/// of carve hits; time is O(files x hits x log(files x kinds)), with
/// `cancel` polled every [`CANCEL_POLL`] steps.
fn merge_anchor_votes(
    ctx: &GeoContext<'_>,
    anchors: &[(u64, &'static [FileKind])],
    cancel: &AtomicBool,
    found: &mut dyn FnMut(u64, u64),
) -> Result<(), HeadError> {
    let hits = ctx.carve_by_kind();
    let list_of = |kind: FileKind| -> &[u64] {
        hits.get(usize::from(carve_kind_code(kind)))
            .map_or(&[][..], |l| l.as_slice())
    };
    // Entries: (offset, file, kind slot, position in that kind's list).
    let mut heap: BinaryHeap<Reverse<(u64, usize, usize, usize)>> = BinaryHeap::new();
    for (fi, &(shift, kinds)) in anchors.iter().enumerate() {
        for (ki, &kind) in kinds.iter().enumerate() {
            let list = list_of(kind);
            let from = list.partition_point(|&d| d < shift);
            if let Some(&d) = list.get(from) {
                heap.push(Reverse((d.saturating_sub(shift), fi, ki, from)));
            }
        }
    }
    // The offset being counted, its number of distinct files, and the
    // last file counted for it (entries of one file are adjacent).
    let mut cur: Option<(u64, u64, usize)> = None;
    let mut steps = 0u32;
    while let Some(Reverse((v, fi, ki, pos))) = heap.pop() {
        steps = steps.wrapping_add(1);
        if steps.checked_rem(CANCEL_POLL) == Some(0) && cancel.load(Ordering::Relaxed) {
            return Err(HeadError::Cancelled);
        }
        match &mut cur {
            Some((cv, cn, last)) if *cv == v => {
                if *last != fi {
                    *cn = cn.saturating_add(1);
                    *last = fi;
                }
            }
            _ => {
                if let Some((cv, cn, _)) = cur {
                    if cn >= ANCHOR_VOTES {
                        found(cv, cn);
                    }
                }
                cur = Some((v, 1, fi));
            }
        }
        let Some(&(shift, kinds)) = anchors.get(fi) else {
            continue;
        };
        let Some(&kind) = kinds.get(ki) else { continue };
        let next = pos.saturating_add(1);
        if let Some(&d) = list_of(kind).get(next) {
            heap.push(Reverse((d.saturating_sub(shift), fi, ki, next)));
        }
    }
    if let Some((cv, cn, _)) = cur {
        if cn >= ANCHOR_VOTES {
            found(cv, cn);
        }
    }
    Ok(())
}

/// The number of anchor files that vote for `off` (a hit of one of their
/// kinds at `off + shift`). O(files x kinds x log hits).
fn anchor_votes_at(ctx: &GeoContext<'_>, anchors: &[(u64, &'static [FileKind])], off: u64) -> u64 {
    let hits = ctx.carve_by_kind();
    let mut n = 0u64;
    for &(shift, kinds) in anchors {
        let Some(at) = off.checked_add(shift) else {
            continue;
        };
        let votes = kinds.iter().any(|&kind| {
            hits.get(usize::from(carve_kind_code(kind)))
                .is_some_and(|l| l.binary_search(&at).is_ok())
        });
        if votes {
            n = n.saturating_add(1);
        }
    }
    n
}

/// F5 for `g` given the instance's file ends (descending). O(log boot
/// hits): the boot hits that agree with a geometry are looked up in
/// [`GeoContext`]'s index.
pub(crate) fn volume_size(ctx: &GeoContext<'_>, ends: &[u64], g: &mut Geometry) {
    let ebf = if ends.len() >= END_RANK {
        ends.get(END_RANK.saturating_sub(1)).copied().unwrap_or(0)
    } else {
        ends.last().copied().unwrap_or(0)
    };
    let end_of_all = ends.first().copied().unwrap_or(0);
    let cs = u64::from(g.cluster_size);
    // The largest size of an agreeing boot hit decides: of the hits with
    // `end_by_files <= size / cs`, the largest is that one if any is.
    let tc = ctx
        .boot_size_at(g.cluster_size, g.volume_offset)
        .and_then(|size| size.checked_div(cs))
        .filter(|&t| ebf <= t);
    let tc = tc.unwrap_or_else(|| {
        let by_range = ctx
            .range
            .end
            .saturating_sub(g.volume_offset)
            .checked_div(cs)
            .unwrap_or(0);
        ebf.max(by_range)
    });
    g.end_by_files = ebf;
    g.end_of_all = end_of_all;
    g.total_clusters = tc;
    let top = tc.max(end_of_all);
    let hi = top
        .saturating_mul(cs)
        .saturating_add(g.volume_offset)
        .min(ctx.source_size);
    let lo = g.volume_offset.min(hi);
    g.lost_volume = lo..hi;
}

/// F7: a geometry set by the user. Its evidence is gathered for exactly
/// that pair (F2 under its cluster size, signature votes counted for that
/// offset), with a fresh F3 check.
pub(crate) fn manual(
    ctx: &GeoContext<'_>,
    cluster_size: u32,
    volume_offset: u64,
    members: &[(u32, RecordId)],
    inst_files: &InstanceFiles,
    heads: &mut Heads,
    cancel: &AtomicBool,
) -> Result<Geometry, HeadError> {
    let cands = gather(
        ctx,
        members,
        inst_files,
        cluster_size,
        SigMode::At(volume_offset),
        cancel,
    )?;
    let same = cands.get(&volume_offset);
    let (checked, matched) =
        content_check(ctx, cluster_size, volume_offset, inst_files, heads, cancel)?;
    let mut g = Geometry {
        cluster_size,
        volume_offset,
        cluster_source: ClusterSource::Manual,
        offset_source: OffsetSource::Manual,
        index_dirs: same.map_or(0, |c| c.dirs),
        index_blocks: same.map_or(0, |c| c.blocks),
        mft_agree: same.map_or(0, |c| c.mft_agree),
        signature_votes: same.map_or(0, |c| c.sig),
        signature_checked: checked,
        signature_ok: matched,
        validated: false,
        total_clusters: 0,
        end_by_files: 0,
        end_of_all: 0,
        lost_volume: volume_offset..volume_offset,
    };
    g.validate();
    volume_size(ctx, &inst_files.ends, &mut g);
    Ok(g)
}

/// F4: `main`'s geometry taken over by an instance (source `inherited`),
/// with F5 recomputed for the instance's own files.
pub(crate) fn inherited(ctx: &GeoContext<'_>, main: &Geometry, ends: &[u64]) -> Geometry {
    let mut g = main.clone();
    g.offset_source = OffsetSource::Inherited;
    volume_size(ctx, ends, &mut g);
    g
}

/// The group facts of an instance's records (helper for D7).
pub(crate) fn group_facts<'a>(
    inst_files: &'a InstanceFiles,
) -> impl Iterator<Item = (&'a Group, &'a FileBits)> + 'a {
    inst_files.groups.groups.iter().zip(inst_files.bits.iter())
}
