//! MFT instances (ALGORITHMS D): fragments (D1), generation cuts (D2,
//! D3), joining (D4) in the order of D5, strays and copies (D6), embedded
//! instances (D7), the main instance (D8) and kinds (D9); with the
//! geometry of every instance (F, [`crate::geometry`]) and the protected
//! ranges of S2.
//!
//! # Use
//!
//! ```text
//! let live = LiveState::load(&session)?;            // LIVE block
//! let mut heads = Heads::load(&session)?;           // HEADS block
//! heads.attach(Some(ranged.clone()));               // or None without a source
//! let overrides = Overrides::load(&session)?;       // OVERRIDES block
//! let layout = analyze(&AnalysisInput::new(session.store(), &meta, &live), &mut heads, &overrides, &cancel)?;
//! heads.save_new(&mut session)?;
//! ```
//!
//! [`analyze`] runs D1 to D7 and F once ([`Layout`] keeps everything the
//! later choices need) and then [`Layout::apply`], the part that
//! `instance_set` and `geometry_set` re-run from stored state: D8, D9,
//! the labels, F4 inheritance, F7 overrides and the protected ranges.
//! Everything is deterministic: the same store, `LIVE`, `HEADS` and
//! `OVERRIDES` give the same layout, with or without a source.
//!
//! # Readings where the contract is silent (see the crate report)
//!
//! * D1: an unnumbered record that fits several fragments joins the one
//!   with most records (then the smaller record size and origin); the
//!   "number span" is that of the fragment's numbered records.
//! * D2: "not a directory" uses E3 (header flag or `$FILE_NAME` flag).
//!   D3: `A_c` is undefined (nothing is *after* the cut) when no record
//!   below the cut is newer than `B_c`.
//! * D4: references to a record number that more than 256 scanned records
//!   carry are not counted (a bound against crafted input). Merging keeps
//!   the id of the larger instance; ties between equal evidence go to the
//!   lower instance ids.
//! * D7: the container's time is compared with the oldest change time of
//!   the instance's records inside that container, as written.
//! * D8 (2), (3): ties go to the instance with more records.
//! * Labels: `[Older file system]`, `[Older file system 2]`, ...;
//!   `[File system 2]`, `[File system 3]`, ... in instance order.

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

pub(crate) mod files;
mod frag;
mod join;
mod overrides;
#[cfg(test)]
pub(crate) mod testlab;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::ops::Range;
use std::sync::atomic::AtomicBool;

use hds_io::Protected;
use serde::Serialize;

use crate::geometry::{self, GeoContext, Geometry, HeadError, Heads, InstanceFiles};
use crate::live::{LiveIndex, LiveSnapshot, LiveState, RecordClass};
use crate::session::SessionMeta;
use crate::store::{RecordId, ScanStore};

pub use overrides::{KindChoice, Overrides};

pub(crate) const NONE: u32 = u32::MAX;

/// Warning code: records of an older file system could not be separated
/// cleanly (D2).
pub const WARN_MIXED_GENERATIONS: &str = "mixed_generations";

/// D6: instances with fewer records go to strays.
pub const MIN_RECORDS: u64 = join::MIN_RECORDS;

/// A segment of a fragment (D2): the records between two kept cuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct SegmentId(pub u32);

impl SegmentId {
    /// As an index.
    pub fn index(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }
}

/// An instance of a [`Layout`]: its position in [`Layout::instances`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct InstanceId(pub u32);

impl InstanceId {
    /// As an index.
    pub fn index(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }
}

/// A kept generation cut (D2) with its boundary times (D3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cut {
    /// The fragment it cuts (position in [`Layout::fragments`]).
    pub fragment: u32,
    /// The record number `p`: records numbered `p` or above are on the
    /// older side.
    pub at: u32,
    /// `B_c`: the newest timed change time of the segment above.
    pub older_newest: Option<u64>,
    /// `A_c`: the oldest timed change time of the segment below that is
    /// newer than `B_c`.
    pub newer_oldest: Option<u64>,
    /// The segment below the cut (newer side).
    pub below: SegmentId,
    /// The segment above the cut (older side).
    pub above: SegmentId,
}

/// How a set of times relates to a cut (D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CutSide {
    /// Newest time at most `B_c`.
    Before,
    /// Oldest time after `B_c` and newest at least `A_c`.
    After,
    /// Neither, or no timed record.
    Undecided,
}

/// What an instance is (D9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceKind {
    /// The file system that was lost.
    Main,
    /// A file system from before the previous format.
    Older,
    /// Another volume, or records nothing ties to main.
    Other,
    /// Inside a disk-image file (browse only).
    Embedded,
    /// The pseudo-instance of strays and copies.
    Strays,
}

impl InstanceKind {
    /// The wire name.
    pub fn name(self) -> &'static str {
        match self {
            InstanceKind::Main => "main",
            InstanceKind::Older => "older",
            InstanceKind::Other => "other",
            InstanceKind::Embedded => "embedded",
            InstanceKind::Strays => "strays",
        }
    }
}

/// The file that holds an embedded instance (D7), or the stale file whose
/// clusters an instance reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    /// The instance the file belongs to.
    pub instance: InstanceId,
    /// Its base record (or first extension record of an orphan group).
    pub record: RecordId,
    /// Its best name.
    pub name: Option<String>,
}

/// One instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// Its id.
    pub id: InstanceId,
    /// Stable identity across rebuilds: the lowest disk offset of its
    /// records (the key of [`Overrides`]).
    pub key: u64,
    /// Its kind.
    pub kind: InstanceKind,
    /// True when the kind comes from an override (`instance_set`).
    pub manual: bool,
    /// Display name of its root (`""` for main).
    pub label: String,
    /// Its records as (effective record number, id), by number (for
    /// strays: unnumbered records carry `u32::MAX`), then id.
    pub records: Vec<(u32, RecordId)>,
    /// Its segments (empty for strays).
    pub segments: Vec<SegmentId>,
    /// Oldest and newest timed MFT-change time of its records.
    pub times: Option<(u64, u64)>,
    /// Indices into [`Layout::live_volumes`] of the current volumes whose
    /// `$MFT` origin it holds (D8 anchor).
    pub anchor_of: Vec<usize>,
    /// D7: the file it is embedded in (`None` when it is not, also after
    /// an override).
    pub embedded_in: Option<Container>,
    /// D7: a stale file of another instance whose clusters this instance's
    /// file system reused (that file gets `overwritten`).
    pub stale_container: Option<Container>,
    /// D6: it was a copy set (strays only).
    pub copy_set: bool,
    /// The geometry computed from its own evidence (F1-F6), when it has
    /// 16 or more records or is an anchor.
    pub own_geometry: Option<Geometry>,
    /// Every candidate of its own geometry.
    pub candidates: Vec<Geometry>,
    /// The geometry in effect: own, inherited from main (F4) or manual
    /// (F7).
    pub geometry: Option<Geometry>,
    /// Its lost-volume range when it is protected (F5, S2).
    pub protected: Option<Range<u64>>,
    auto_embedded: Option<Container>,
    ends: Vec<u64>,
}

impl Instance {
    /// The record with this number (non-strays instances: numbers are
    /// unique).
    pub fn record_numbered(&self, n: u32) -> Option<RecordId> {
        let i = self.records.partition_point(|&(k, _)| k < n);
        self.records
            .get(i)
            .filter(|&&(k, _)| k == n)
            .map(|&(_, id)| id)
    }

    /// Whether the geometry in effect is validated.
    pub fn validated(&self) -> bool {
        self.geometry.as_ref().is_some_and(|g| g.validated)
    }
}

/// Records that went to strays together (a former instance, D6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StrayGroup {
    /// Records in the group.
    pub records: u64,
    /// It was a copy set.
    pub copy_set: bool,
    /// Instances of which at least 90 % of its records repeat records (D6
    /// key): it is a stale copy of them (E6 b).
    pub copy_of: Vec<InstanceId>,
}

/// One fragment (D1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Fragment {
    /// Record size.
    pub record_size: u32,
    /// `disk offset - number x record size`.
    pub origin: i64,
    /// Records in it.
    pub records: u64,
    /// Its segments, lowest numbers first.
    pub segments: Vec<SegmentId>,
}

/// One segment (D2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Segment {
    /// Its fragment.
    pub fragment: u32,
    /// 0 at the lowest record numbers.
    pub index: u32,
    /// Records in it.
    pub records: u64,
}

/// A current volume as the catalog reports it (`CatalogInfo.live_volumes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveVolume {
    /// Offset of the volume.
    pub offset: u64,
    /// Size in bytes.
    pub size: u64,
    /// `format_time` (FILETIME).
    pub format_time: u64,
    /// Disk offset of its `$MFT` (the anchor origin).
    pub mft_origin: Option<u64>,
    /// MFT record size.
    pub record_size: u32,
    /// Cluster size.
    pub cluster_size: u32,
}

/// Why an analysis stopped.
#[derive(Debug)]
pub enum AnalysisError {
    /// The device stopped answering while heads were read (S7).
    SourceLost(io::Error),
    /// The cancel flag was set.
    Cancelled,
}

impl fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnalysisError::SourceLost(e) => write!(f, "the source stopped answering: {e}"),
            AnalysisError::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for AnalysisError {}

impl From<HeadError> for AnalysisError {
    fn from(e: HeadError) -> AnalysisError {
        match e {
            HeadError::SourceLost(e) => AnalysisError::SourceLost(e),
            HeadError::Cancelled => AnalysisError::Cancelled,
        }
    }
}

/// What an analysis reads.
#[derive(Debug, Clone, Copy)]
pub struct AnalysisInput<'a> {
    /// The scan store.
    pub store: &'a ScanStore,
    /// The session range.
    pub range: &'a Range<u64>,
    /// Size of the source in bytes (candidate offsets must lie inside).
    pub source_size: u64,
    /// Logical sector size (candidate offsets are multiples of it).
    pub sector_size: u32,
    /// The `LIVE` state (snapshot and scan time).
    pub live: &'a LiveState,
}

impl<'a> AnalysisInput<'a> {
    /// The input of a session.
    pub fn new(
        store: &'a ScanStore,
        meta: &'a SessionMeta,
        live: &'a LiveState,
    ) -> AnalysisInput<'a> {
        AnalysisInput {
            store,
            range: &meta.range,
            source_size: meta.identity.size,
            sector_size: meta.identity.sector_size,
            live,
        }
    }
}

/// Everything C, D and F decided.
#[derive(Debug)]
pub struct Layout {
    range: Range<u64>,
    source_size: u64,
    sector: u64,
    starts: Vec<u64>,
    live_volumes: Vec<LiveVolume>,
    snapshot_taken: Option<u64>,
    live: LiveIndex,
    fragments: Vec<Fragment>,
    segments: Vec<Segment>,
    cuts: Vec<Cut>,
    num: Vec<u32>,
    older: Vec<RecordId>,
    rec_inst: Vec<u32>,
    instances: Vec<Instance>,
    stray_groups: Vec<StrayGroup>,
    stray_group_of: Vec<(RecordId, u32)>,
    main: Option<InstanceId>,
    warnings: Vec<&'static str>,
    protected: Vec<Protected>,
    contradictions: u64,
    unexplained: u64,
    /// Log lines of the decisions (merges, choices), for diagnostics.
    pub log: Vec<String>,
}

/// Runs C3-C5, D1-D7 and F, then [`Layout::apply`] with `overrides`.
///
/// Reads nothing but heads (through `heads`, which reads from its source
/// when one is attached and keeps what it read). Fails only when the
/// device is lost or `cancel` is set.
pub fn analyze(
    input: &AnalysisInput<'_>,
    heads: &mut Heads,
    overrides: &Overrides,
    cancel: &AtomicBool,
) -> Result<Layout, AnalysisError> {
    let store = input.store;
    let empty = LiveSnapshot::default();
    let snapshot = input.live.snapshot.as_ref().unwrap_or(&empty);
    let live = LiveIndex::build(store, snapshot);
    let timed = frag::Timed::new(input.live.reference_time());
    let frags = frag::build(store, &live, timed);
    let starts = snapshot.volume_starts(input.range);
    let ctx = GeoContext::new(
        store,
        &live,
        input.range.clone(),
        input.source_size,
        u64::from(input.sector_size),
        starts.clone(),
    );
    // Anchor fragments: old records at the origin of a current $MFT.
    let mut anchor_frags = Vec::new();
    for (vi, v) in snapshot.volumes.iter().enumerate() {
        let Some(origin) = v.mft_origin() else {
            continue;
        };
        for (fi, f) in frags.fragments.iter().enumerate() {
            if i128::from(f.origin) == i128::from(origin) && f.record_size == v.record_size {
                anchor_frags.push((u32::try_from(fi).unwrap_or(NONE), vi));
            }
        }
    }
    let mut work = join::Work::new(store, &live, &frags, timed, &ctx, anchor_frags.clone());
    work.run(heads, cancel)?;
    let stray = join::strays(&mut work);

    // Final instances: kept roots by key (lowest record offset), strays last.
    let mut kept: Vec<(u64, u32)> = stray
        .kept
        .iter()
        .map(|&r| (work.state(r).map_or(u64::MAX, |s| s.lowest), r))
        .collect();
    kept.sort_unstable();
    let mut root_to_id: BTreeMap<u32, InstanceId> = BTreeMap::new();
    for (i, &(_, r)) in kept.iter().enumerate() {
        root_to_id.insert(r, InstanceId(u32::try_from(i).unwrap_or(NONE)));
    }
    let anchor_roots: Vec<(u32, usize)> = anchor_frags
        .iter()
        .filter_map(|&(f, vi)| {
            let s = frags
                .fragments
                .get(usize::try_from(f).ok()?)?
                .segments
                .first()?
                .0;
            Some((work.find(s), vi))
        })
        .collect();
    let mut instances: Vec<Instance> = Vec::new();
    let mut rec_inst = vec![NONE; store.record_count()];
    for &(key, r) in &kept {
        let id = root_to_id.get(&r).copied().unwrap_or(InstanceId(NONE));
        let (members, inst_files) = work.files_of(r);
        let Some(st) = work.state(r) else { continue };
        for &(_, rid) in members.iter() {
            if let Some(slot) = rec_inst.get_mut(rid.index()) {
                *slot = id.0;
            }
        }
        let container = |c: Option<(u32, RecordId)>, w: &join::Work<'_>| -> Option<Container> {
            let (y, rid) = c?;
            let _ = w;
            Some(Container {
                instance: root_to_id.get(&y).copied()?,
                record: rid,
                name: None,
            })
        };
        let mut segs: Vec<SegmentId> = st.segs.iter().map(|&s| SegmentId(s)).collect();
        segs.sort_unstable();
        let ends: Vec<u64> = inst_files
            .ends
            .iter()
            .take(geometry::END_RANK)
            .copied()
            .collect();
        let geo = st.geo.clone();
        instances.push(Instance {
            id,
            key,
            kind: InstanceKind::Other,
            manual: false,
            label: String::new(),
            records: members.as_ref().clone(),
            segments: segs,
            times: st.times,
            anchor_of: anchor_roots
                .iter()
                .filter(|&&(ar, _)| ar == r)
                .map(|&(_, vi)| vi)
                .collect(),
            embedded_in: None,
            stale_container: container(st.stale, &work),
            copy_set: false,
            own_geometry: geo.as_ref().map(|g| g.chosen.clone()),
            candidates: geo.map(|g| g.candidates).unwrap_or_default(),
            geometry: None,
            protected: None,
            auto_embedded: container(st.embedded_in, &work),
            ends,
        });
    }
    // Container names.
    for inst in &mut instances {
        for c in [&mut inst.auto_embedded, &mut inst.stale_container]
            .into_iter()
            .flatten()
        {
            c.name = container_name(store, c.record);
        }
    }
    // Strays.
    let strays_id = InstanceId(u32::try_from(instances.len()).unwrap_or(NONE));
    let mut stray_records: Vec<(u32, RecordId)> = Vec::new();
    let mut stray_groups: Vec<StrayGroup> = Vec::new();
    let mut stray_group_of: Vec<(RecordId, u32)> = Vec::new();
    for &(r, copy) in &stray.strays {
        let members = work.members(r);
        let gid = u32::try_from(stray_groups.len()).unwrap_or(NONE);
        for &(n, rid) in members.iter() {
            stray_records.push((n, rid));
            stray_group_of.push((rid, gid));
        }
        stray_groups.push(StrayGroup {
            records: u64::try_from(members.len()).unwrap_or(u64::MAX),
            copy_set: copy,
            copy_of: Vec::new(),
        });
    }
    for &rid in &frags.strays {
        stray_records.push((NONE, rid));
    }
    stray_group_of.sort_unstable();
    stray_copies(
        store,
        &instances,
        &mut stray_groups,
        &stray_group_of,
        &frags.num,
    );
    stray_records.sort_unstable();
    for &(_, rid) in &stray_records {
        if let Some(slot) = rec_inst.get_mut(rid.index()) {
            *slot = strays_id.0;
        }
    }
    let stray_key = stray_records
        .iter()
        .filter_map(|&(_, id)| store.record(id).map(|r| r.disk_offset()))
        .min()
        .unwrap_or(u64::MAX);
    instances.push(Instance {
        id: strays_id,
        key: stray_key,
        kind: InstanceKind::Strays,
        manual: false,
        label: "[Stray records]".to_string(),
        times: stray_times(store, timed, &stray_records),
        records: stray_records,
        segments: Vec::new(),
        anchor_of: Vec::new(),
        embedded_in: None,
        stale_container: None,
        copy_set: false,
        own_geometry: None,
        candidates: Vec::new(),
        geometry: None,
        protected: None,
        auto_embedded: None,
        ends: Vec::new(),
    });

    let fragments = frags
        .fragments
        .iter()
        .map(|f| Fragment {
            record_size: f.record_size,
            origin: f.origin,
            records: u64::try_from(f.records.len()).unwrap_or(u64::MAX),
            segments: f.segments.clone(),
        })
        .collect();
    let segments = frags
        .segments
        .iter()
        .map(|s| Segment {
            fragment: s.fragment,
            index: s.index,
            records: u64::try_from(s.end.saturating_sub(s.start)).unwrap_or(0),
        })
        .collect();
    let live_volumes = snapshot
        .volumes
        .iter()
        .map(|v| LiveVolume {
            offset: v.offset,
            size: v.volume_bytes(),
            format_time: v.format_time,
            mft_origin: v.mft_origin(),
            record_size: v.record_size,
            cluster_size: v.cluster_size,
        })
        .collect();
    let mut warnings: Vec<&'static str> = snapshot.warnings();
    if frags.unexplained > 0 {
        warnings.push(WARN_MIXED_GENERATIONS);
    }
    let log = std::mem::take(&mut work.log);
    drop(work);
    drop(ctx);
    let mut layout = Layout {
        range: input.range.clone(),
        source_size: input.source_size,
        sector: u64::from(input.sector_size).max(1),
        starts,
        live_volumes,
        snapshot_taken: input.live.snapshot.as_ref().map(|s| s.taken_at),
        fragments,
        segments,
        cuts: frags.cuts.clone(),
        num: frags.num.clone(),
        older: frags.older.clone(),
        rec_inst,
        instances,
        stray_groups,
        stray_group_of,
        main: None,
        warnings,
        protected: Vec::new(),
        contradictions: frags.contradictions,
        unexplained: frags.unexplained,
        live,
        log,
    };
    layout.apply(store, overrides, heads, cancel)?;
    Ok(layout)
}

/// The best name of the group whose base (or first) record is `id`.
fn container_name(store: &ScanStore, id: RecordId) -> Option<String> {
    let mut best: Option<(u8, u16, String)> = None;
    for n in store.names(id) {
        let rank = match n.namespace {
            hds_ntfs::Namespace::Win32 | hds_ntfs::Namespace::Win32AndDos => 0,
            hds_ntfs::Namespace::Posix => 1,
            _ => 2,
        };
        if best
            .as_ref()
            .is_none_or(|(a, b, _)| (rank, n.attr_id) < (*a, *b))
        {
            best = Some((rank, n.attr_id, n.name.to_string_lossy()));
        }
    }
    best.map(|(_, _, s)| s)
}

fn stray_times(
    store: &ScanStore,
    timed: frag::Timed,
    recs: &[(u32, RecordId)],
) -> Option<(u64, u64)> {
    let mut t: Option<(u64, u64)> = None;
    for &(_, id) in recs {
        if let Some(x) = timed.of(store, id) {
            t = Some(t.map_or((x, x), |(a, b)| (a.min(x), b.max(x))));
        }
    }
    t
}

/// E6(b) support: which instances each stray group copies (at least 90 %
/// of its records repeat records of the instance, D6 key).
fn stray_copies(
    store: &ScanStore,
    instances: &[Instance],
    groups: &mut [StrayGroup],
    group_of: &[(RecordId, u32)],
    num: &[u32],
) {
    if groups.is_empty() {
        return;
    }
    let mut entries: Vec<((u64, u64), u32)> = group_of
        .iter()
        .map(|&(id, g)| {
            let n = num.get(id.index()).copied().unwrap_or(NONE);
            (join::copy_key(store, n, id), g)
        })
        .collect();
    entries.sort_unstable();
    let mut counts: BTreeMap<(u32, u32), u64> = BTreeMap::new();
    for inst in instances {
        for &(n, id) in &inst.records {
            let k = join::copy_key(store, n, id);
            let mut i = entries.partition_point(|e| e.0 < k);
            while let Some(&(ek, g)) = entries.get(i) {
                if ek != k {
                    break;
                }
                let e = counts.entry((g, inst.id.0)).or_insert(0);
                *e = e.saturating_add(1);
                i = i.saturating_add(1);
            }
        }
    }
    for ((g, inst), c) in counts {
        let Some(group) = groups.get_mut(usize::try_from(g).unwrap_or(usize::MAX)) else {
            continue;
        };
        if c.saturating_mul(10) >= group.records.saturating_mul(9) {
            group.copy_of.push(InstanceId(inst));
        }
    }
}

impl Layout {
    /// D8, D9, labels, F4 inheritance, F7 overrides and the protected
    /// ranges from the stored state and `overrides` (`instance_set`,
    /// `geometry_set`). Reads heads only for a manual geometry.
    pub fn apply(
        &mut self,
        store: &ScanStore,
        overrides: &Overrides,
        heads: &mut Heads,
        cancel: &AtomicBool,
    ) -> Result<(), AnalysisError> {
        let strays = self
            .instances
            .iter()
            .position(|i| i.kind == InstanceKind::Strays);
        // Kind overrides (never on strays).
        let choices: Vec<Option<KindChoice>> = self
            .instances
            .iter()
            .enumerate()
            .map(|(i, inst)| {
                if Some(i) == strays {
                    None
                } else {
                    overrides.kind(inst.key)
                }
            })
            .collect();
        for (inst, c) in self.instances.iter_mut().zip(&choices) {
            if Some(inst.id.index()) == strays {
                continue;
            }
            inst.manual = c.is_some();
            inst.embedded_in = if c.is_some() {
                None
            } else {
                inst.auto_embedded.clone()
            };
            inst.kind = InstanceKind::Other;
        }

        // D8.
        let pickable: Vec<usize> = (0..self.instances.len())
            .filter(|&i| Some(i) != strays)
            .filter(|&i| {
                self.instances
                    .get(i)
                    .is_some_and(|x| x.embedded_in.is_none())
            })
            .filter(|&i| choices.get(i).copied().flatten() != Some(KindChoice::Other))
            .collect();
        let auto = self.auto_main(&pickable);
        let forced = (0..self.instances.len()).rev().find(|&i| {
            choices.get(i).copied().flatten() == Some(KindChoice::Main) && Some(i) != strays
        });
        let main = forced.or(auto);
        self.main = main.and_then(|i| u32::try_from(i).ok()).map(InstanceId);
        self.log.push(format!(
            "D8 main {:?} (auto {:?}, override {:?})",
            main, auto, forced
        ));

        // D9.
        let mut kinds: Vec<InstanceKind> = Vec::with_capacity(self.instances.len());
        for (i, inst) in self.instances.iter().enumerate() {
            let k = if Some(i) == strays {
                InstanceKind::Strays
            } else if Some(i) == main {
                InstanceKind::Main
            } else if inst.embedded_in.is_some() {
                InstanceKind::Embedded
            } else if choices.get(i).copied().flatten() == Some(KindChoice::Other)
                || (forced.is_some() && Some(i) == auto)
            {
                InstanceKind::Other
            } else if main.is_some_and(|m| self.is_older(i, m)) {
                InstanceKind::Older
            } else {
                InstanceKind::Other
            };
            kinds.push(k);
        }
        // Labels.
        let (mut n_other, mut n_older) = (1u32, 0u32);
        for (inst, &k) in self.instances.iter_mut().zip(&kinds) {
            inst.kind = k;
            inst.label = match k {
                InstanceKind::Main => String::new(),
                InstanceKind::Older => {
                    n_older = n_older.saturating_add(1);
                    if n_older == 1 {
                        "[Older file system]".to_string()
                    } else {
                        format!("[Older file system {n_older}]")
                    }
                }
                InstanceKind::Other => {
                    n_other = n_other.saturating_add(1);
                    format!("[File system {n_other}]")
                }
                InstanceKind::Embedded => {
                    let name = inst
                        .embedded_in
                        .as_ref()
                        .and_then(|c| c.name.clone())
                        .unwrap_or_else(|| {
                            inst.embedded_in
                                .as_ref()
                                .map_or_else(String::new, |c| format!("#{}", c.record.0))
                        });
                    format!("[Inside {name}]")
                }
                InstanceKind::Strays => "[Stray records]".to_string(),
            };
        }

        // Geometry in effect: F7 overrides, F4 inheritance, own.
        let ctx_live = &self.live;
        let ctx = GeoContext::new(
            store,
            ctx_live,
            self.range.clone(),
            self.source_size,
            self.sector,
            self.starts.clone(),
        );
        let mut effective: Vec<Option<Geometry>> = Vec::with_capacity(self.instances.len());
        for inst in &self.instances {
            let g = match overrides.geometry(inst.key) {
                Some((cs, off)) if Some(inst.id.index()) != strays => {
                    let files = InstanceFiles::build(store, &inst.records);
                    Some(geometry::manual(
                        &ctx,
                        cs,
                        off,
                        &inst.candidates,
                        &files,
                        heads,
                        cancel,
                    )?)
                }
                _ => inst.own_geometry.clone(),
            };
            effective.push(g);
        }
        let main_geo = main.and_then(|m| effective.get(m).cloned().flatten());
        for (i, inst) in self.instances.iter().enumerate() {
            let manual = overrides.geometry(inst.key).is_some() && Some(i) != strays;
            let inherit = !manual
                && matches!(inst.kind, InstanceKind::Older | InstanceKind::Strays)
                && !effective
                    .get(i)
                    .and_then(|g| g.as_ref())
                    .is_some_and(|g| g.own_evidence());
            if inherit {
                if let (Some(mg), Some(slot)) = (&main_geo, effective.get_mut(i)) {
                    *slot = Some(geometry::inherited(&ctx, mg, &inst.ends));
                }
            }
        }
        drop(ctx);
        for (inst, g) in self.instances.iter_mut().zip(effective) {
            inst.geometry = g;
        }

        // S2: the session range, then the lost-volume ranges of main and
        // other instances with validated geometry, and of the anchors.
        let mut protected = vec![Protected::session(self.range.clone())];
        let mut order: Vec<usize> = Vec::new();
        order.extend(main);
        order.extend((0..self.instances.len()).filter(|&i| {
            Some(i) != main
                && self
                    .instances
                    .get(i)
                    .is_some_and(|x| x.kind == InstanceKind::Other)
        }));
        let anchors: Vec<usize> = (0..self.instances.len())
            .filter(|&i| {
                self.instances
                    .get(i)
                    .is_some_and(|x| !x.anchor_of.is_empty())
            })
            .collect();
        for inst in &mut self.instances {
            inst.protected = None;
        }
        for i in order.into_iter().chain(anchors.iter().copied()) {
            let Some(inst) = self.instances.get_mut(i) else {
                continue;
            };
            let Some(g) = &inst.geometry else { continue };
            let is_anchor = anchors.contains(&i);
            if !(g.validated || is_anchor) {
                continue;
            }
            let r = g.lost_volume.clone();
            if r.start >= r.end {
                continue;
            }
            inst.protected = Some(r.clone());
            let p = Protected::lost_volume(r);
            if !protected.contains(&p) {
                protected.push(p);
            }
        }
        self.protected = protected;
        Ok(())
    }

    /// D8's automatic choice among `pickable` instance indices.
    fn auto_main(&self, pickable: &[usize]) -> Option<usize> {
        let inst = |i: usize| self.instances.get(i);
        let nrec = |i: usize| inst(i).map_or(0, |x| x.records.len());
        // (1) anchors: of the current volume with the newest format time,
        // then most records.
        let anchors: Vec<(usize, u64)> = pickable
            .iter()
            .filter_map(|&i| {
                let x = inst(i)?;
                let ft = x
                    .anchor_of
                    .iter()
                    .filter_map(|&v| self.live_volumes.get(v).map(|lv| lv.format_time))
                    .max()?;
                Some((i, ft))
            })
            .collect();
        if let Some(newest) = anchors.iter().map(|&(_, ft)| ft).max() {
            return anchors
                .iter()
                .filter(|&&(_, ft)| ft == newest)
                .map(|&(i, _)| i)
                .max_by(|&a, &b| nrec(a).cmp(&nrec(b)).then(b.cmp(&a)));
        }
        let newest = |i: usize| inst(i).and_then(|x| x.times).map(|t| t.1);
        // (2) validated geometry at the range start or a partition start.
        let at_start: Vec<usize> = pickable
            .iter()
            .copied()
            .filter(|&i| {
                inst(i)
                    .and_then(|x| x.own_geometry.as_ref())
                    .is_some_and(|g| g.validated && self.starts.contains(&g.volume_offset))
            })
            .collect();
        let pick = |set: &[usize]| {
            set.iter().copied().max_by(|&a, &b| {
                newest(a)
                    .cmp(&newest(b))
                    .then(nrec(a).cmp(&nrec(b)))
                    .then(b.cmp(&a))
            })
        };
        if !at_start.is_empty() {
            return pick(&at_start);
        }
        // (3) the newest change time.
        let timed: Vec<usize> = pickable
            .iter()
            .copied()
            .filter(|&i| newest(i).is_some())
            .collect();
        if !timed.is_empty() {
            return pick(&timed);
        }
        // (4) most records.
        pickable
            .iter()
            .copied()
            .max_by(|&a, &b| nrec(a).cmp(&nrec(b)).then(b.cmp(&a)))
    }

    /// D9: whether instance `i` is `older` than main `m`.
    fn is_older(&self, i: usize, m: usize) -> bool {
        let (Some(x), Some(mn)) = (self.instances.get(i), self.instances.get(m)) else {
            return false;
        };
        for c in &self.cuts {
            if !mn.segments.contains(&c.below) {
                continue;
            }
            if x.segments.contains(&c.above) || c.relation(x.times) == CutSide::Before {
                return true;
            }
        }
        let (Some((_, newest)), Some((oldest, _))) = (x.times, mn.times) else {
            return false;
        };
        if newest >= oldest {
            return false;
        }
        let elsewhere = match (&x.own_geometry, &mn.own_geometry) {
            (Some(g), Some(mg)) => g.validated && g.volume_offset != mg.volume_offset,
            _ => false,
        };
        !elsewhere
    }

    /// The session range.
    pub fn range(&self) -> &Range<u64> {
        &self.range
    }

    /// The range start and the partition starts inside the range.
    pub fn volume_starts(&self) -> &[u64] {
        &self.starts
    }

    /// The current volumes.
    pub fn live_volumes(&self) -> &[LiveVolume] {
        &self.live_volumes
    }

    /// When the snapshot in use was taken (FILETIME).
    pub fn snapshot_time(&self) -> Option<u64> {
        self.snapshot_taken
    }

    /// C3-C5.
    pub fn live(&self) -> &LiveIndex {
        &self.live
    }

    /// C3 class of a record.
    pub fn class(&self, id: RecordId) -> RecordClass {
        self.live.class(id)
    }

    /// The instance holding an old record (`None` for live and post_format
    /// records).
    pub fn instance_of(&self, id: RecordId) -> Option<InstanceId> {
        self.rec_inst
            .get(id.index())
            .copied()
            .filter(|&x| x != NONE)
            .map(InstanceId)
    }

    /// Whether D2 marked the record `older_generation`.
    pub fn is_older_generation(&self, id: RecordId) -> bool {
        self.older.binary_search(&id).is_ok()
    }

    /// The records D2 marked `older_generation`, by id.
    pub fn older_generation_records(&self) -> &[RecordId] {
        &self.older
    }

    /// The effective record number (D1 gives unnumbered records on a
    /// fragment's grid their positional number).
    pub fn number(&self, id: RecordId) -> Option<u32> {
        self.num.get(id.index()).copied().filter(|&n| n != NONE)
    }

    /// All instances; the last one is the strays pseudo-instance.
    pub fn instances(&self) -> &[Instance] {
        &self.instances
    }

    /// One instance.
    pub fn instance(&self, id: InstanceId) -> Option<&Instance> {
        self.instances.get(id.index())
    }

    /// The instance with this key.
    pub fn instance_by_key(&self, key: u64) -> Option<&Instance> {
        self.instances.iter().find(|i| i.key == key)
    }

    /// The main instance.
    pub fn main(&self) -> Option<InstanceId> {
        self.main
    }

    /// The anchors (D8): instances holding old records at the origin of a
    /// current `$MFT`.
    pub fn anchors(&self) -> Vec<InstanceId> {
        self.instances
            .iter()
            .filter(|i| !i.anchor_of.is_empty())
            .map(|i| i.id)
            .collect()
    }

    /// The fragments (D1).
    pub fn fragments(&self) -> &[Fragment] {
        &self.fragments
    }

    /// The segments (D2).
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// The kept cuts with their boundary times (D2, D3).
    pub fn cuts(&self) -> &[Cut] {
        &self.cuts
    }

    /// The kept cuts that concern an instance: it holds the segment below
    /// or above them.
    pub fn cuts_of(&self, id: InstanceId) -> Vec<&Cut> {
        let Some(inst) = self.instance(id) else {
            return Vec::new();
        };
        self.cuts
            .iter()
            .filter(|c| inst.segments.contains(&c.below) || inst.segments.contains(&c.above))
            .collect()
    }

    /// E6's test: a time is on the instance's side of every kept cut that
    /// concerns it (D3 comparison: *after* a cut whose newer side it
    /// holds, *before* one whose older side it holds).
    pub fn on_side_of_cuts(&self, id: InstanceId, time: u64) -> bool {
        let Some(inst) = self.instance(id) else {
            return false;
        };
        self.cuts.iter().all(|c| {
            let rel = c.relation(Some((time, time)));
            if inst.segments.contains(&c.below) && rel != CutSide::After {
                return false;
            }
            if inst.segments.contains(&c.above) && rel != CutSide::Before {
                return false;
            }
            true
        })
    }

    /// The stray group of a record in strays (D6), if it came from one.
    pub fn stray_group(&self, id: RecordId) -> Option<&StrayGroup> {
        let i = self.stray_group_of.partition_point(|&(r, _)| r < id);
        let &(r, g) = self.stray_group_of.get(i)?;
        if r != id {
            return None;
        }
        self.stray_groups.get(usize::try_from(g).ok()?)
    }

    /// All stray groups.
    pub fn stray_groups(&self) -> &[StrayGroup] {
        &self.stray_groups
    }

    /// Warning codes: `live_volume_incomplete`, `writes_not_detectable`,
    /// `mixed_generations`.
    pub fn warnings(&self) -> &[&'static str] {
        &self.warnings
    }

    /// Contradictions found by D2, and how many stayed unexplained.
    pub fn contradiction_counts(&self) -> (u64, u64) {
        (self.contradictions, self.unexplained)
    }

    /// The protected ranges (S2): the session range first, then the
    /// lost-volume ranges of main and the other instances with validated
    /// geometry, and of the anchors.
    pub fn protected(&self) -> &[Protected] {
        &self.protected
    }
}
