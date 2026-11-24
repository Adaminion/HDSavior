//! ALGORITHMS C3 (classes of stored records), C4 (current index data) and
//! C5 (the overwritten set), computed from a [`LiveSnapshot`] and the scan
//! store alone.

use hds_io::RangeSet;
use hds_ntfs::FileRef;

use super::{CurrentVolume, LiveSnapshot};
use crate::store::{DataExtent, EntryId, EntrySourceKind, RecordId, ScanStore};

/// What C3 makes of a stored record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RecordClass {
    /// A record of a lost (or embedded, or other) file system: input of D.
    Old,
    /// A record of a current volume, in use there: hidden, counted.
    Live,
    /// A file created and deleted again on a current volume: hidden,
    /// counted; its extents are overwritten (C5).
    PostFormat,
}

impl RecordClass {
    fn code(self) -> u8 {
        match self {
            RecordClass::Old => 0,
            RecordClass::Live => 1,
            RecordClass::PostFormat => 2,
        }
    }

    fn from_code(c: u8) -> RecordClass {
        match c {
            1 => RecordClass::Live,
            2 => RecordClass::PostFormat,
            _ => RecordClass::Old,
        }
    }

    /// The name used in reports (`"old"`, `"live"`, `"post_format"`).
    pub fn name(self) -> &'static str {
        match self {
            RecordClass::Old => "old",
            RecordClass::Live => "live",
            RecordClass::PostFormat => "post_format",
        }
    }
}

/// A fixed-size bit set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Bits(Vec<u64>);

impl Bits {
    pub(crate) fn new(n: usize) -> Bits {
        Bits(vec![0; n.div_ceil(64)])
    }

    pub(crate) fn set(&mut self, i: usize) {
        if let Some(w) = self.0.get_mut(i >> 6) {
            *w |= 1u64 << (i & 63);
        }
    }

    pub(crate) fn get(&self, i: usize) -> bool {
        self.0.get(i >> 6).is_some_and(|w| w >> (i & 63) & 1 != 0)
    }
}

/// The classes of all stored records (C3), the current index data (C4)
/// and the overwritten set (C5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveIndex {
    classes: Vec<u8>,
    live: u64,
    post_format: u64,
    current_blocks: Bits,
    current_entries: Bits,
    overwritten: RangeSet,
    live_allocated: RangeSet,
    format_times: Vec<u64>,
}

impl LiveIndex {
    /// An index without current volumes: every record is old, nothing is
    /// current, nothing is overwritten.
    pub fn empty(store: &ScanStore) -> LiveIndex {
        LiveIndex::build(store, &LiveSnapshot::default())
    }

    /// Classifies `store` against `snapshot`.
    pub fn build(store: &ScanStore, snapshot: &LiveSnapshot) -> LiveIndex {
        let n = store.record_count();
        let mut classes = vec![RecordClass::Old.code(); n];
        let by_offset = store.records_by_offset();
        for cv in &snapshot.volumes {
            classify_volume(store, by_offset, cv, &mut classes);
        }
        let mut live = 0u64;
        let mut post_format = 0u64;
        for &c in &classes {
            match RecordClass::from_code(c) {
                RecordClass::Live => live = live.saturating_add(1),
                RecordClass::PostFormat => post_format = post_format.saturating_add(1),
                RecordClass::Old => {}
            }
        }

        // C5: live-allocated ranges and the extents of post_format records.
        let mut live_allocated = RangeSet::new();
        for cv in &snapshot.volumes {
            live_allocated.union_with(&cv.allocated);
        }
        let mut overwritten = live_allocated.clone();
        let slot_ranges: Vec<Vec<(std::ops::Range<u64>, u64)>> =
            snapshot.volumes.iter().map(|v| v.slot_ranges()).collect();
        for (i, &c) in classes.iter().enumerate() {
            if RecordClass::from_code(c) != RecordClass::PostFormat {
                continue;
            }
            let Ok(id) = u32::try_from(i).map(RecordId) else {
                continue;
            };
            let Some(rec) = store.record(id) else {
                continue;
            };
            let off = rec.disk_offset();
            let Some(cv) = snapshot
                .volumes
                .iter()
                .zip(&slot_ranges)
                .find(|(_, sr)| sr.iter().any(|(r, _)| r.contains(&off)))
                .map(|(cv, _)| cv)
            else {
                continue;
            };
            let cs = u64::from(cv.cluster_size);
            let mut add = |runs: &mut dyn Iterator<Item = hds_ntfs::Run>| {
                for run in runs {
                    let Some(lcn) = run.lcn else { continue };
                    let a = lcn.saturating_mul(cs).saturating_add(cv.offset);
                    let b = run.length.saturating_mul(cs).saturating_add(a);
                    if a < b {
                        overwritten.insert(a..b);
                    }
                }
            };
            for ext in store.data_extents(id) {
                if let DataExtent::NonResident(e) = ext {
                    add(&mut e.runs());
                }
            }
            for e in store.index_allocation_extents(id) {
                add(&mut e.runs());
            }
        }

        // C4: current index blocks and entries.
        let blocks = store.index_blocks();
        let mut current_blocks = Bits::new(blocks.len());
        for (i, b) in blocks.iter().enumerate() {
            let r = b.disk_offset..b.end_offset();
            if !snapshot.volumes.is_empty() && live_allocated.contains_range(r) {
                current_blocks.set(i);
            }
        }
        let mut current_entries = Bits::new(store.index_entry_count());
        if !snapshot.volumes.is_empty() {
            let min_ft = snapshot
                .volumes
                .iter()
                .map(|v| v.format_time)
                .min()
                .unwrap_or(u64::MAX);
            for (id, e) in store.index_entries() {
                let src = e.source.offset;
                let from_current = match e.source.kind {
                    EntrySourceKind::Block => store
                        .index_blocks_by_offset()
                        .binary_search_by_key(&src, |&p| {
                            usize::try_from(p)
                                .ok()
                                .and_then(|p| blocks.get(p))
                                .map_or(u64::MAX, |b| b.disk_offset)
                        })
                        .ok()
                        .and_then(|k| store.index_blocks_by_offset().get(k))
                        .and_then(|&p| usize::try_from(p).ok())
                        .is_some_and(|p| current_blocks.get(p)),
                    EntrySourceKind::RecordRoot => store.record_at(src).is_some_and(|rid| {
                        classes
                            .get(rid.index())
                            .is_some_and(|&c| RecordClass::from_code(c) != RecordClass::Old)
                    }),
                };
                // The format time of the current volume that holds the
                // source, else the oldest one (the contract names one
                // format_time; see the module documentation).
                let ft = snapshot
                    .volumes
                    .iter()
                    .find(|v| v.volume_range().contains(&src))
                    .map_or(min_ft, |v| v.format_time);
                if from_current || e.time >= ft {
                    current_entries.set(id.index());
                }
            }
        }
        let format_times = snapshot.volumes.iter().map(|v| v.format_time).collect();
        LiveIndex {
            classes,
            live,
            post_format,
            current_blocks,
            current_entries,
            overwritten,
            live_allocated,
            format_times,
        }
    }

    /// The class of a record (`Old` for an unknown id).
    pub fn class(&self, id: RecordId) -> RecordClass {
        self.classes
            .get(id.index())
            .map_or(RecordClass::Old, |&c| RecordClass::from_code(c))
    }

    /// Whether a record is input of ALGORITHMS D (class `Old`).
    pub fn is_old(&self, id: RecordId) -> bool {
        self.class(id) == RecordClass::Old
    }

    /// Records of class `live`.
    pub fn live_records(&self) -> u64 {
        self.live
    }

    /// Records of class `post_format`.
    pub fn post_format_records(&self) -> u64 {
        self.post_format
    }

    /// Whether the index block at position `pos` of
    /// [`ScanStore::index_blocks`] belongs to a current volume (C4).
    pub fn is_current_block(&self, pos: usize) -> bool {
        self.current_blocks.get(pos)
    }

    /// Whether an index entry belongs to a current volume (C4).
    pub fn is_current_entry(&self, id: EntryId) -> bool {
        self.current_entries.get(id.index())
    }

    /// The overwritten set (C5): live-allocated ranges and the extents of
    /// `post_format` records.
    pub fn overwritten(&self) -> &RangeSet {
        &self.overwritten
    }

    /// The live-allocated ranges of all current volumes.
    pub fn live_allocated(&self) -> &RangeSet {
        &self.live_allocated
    }

    /// The `format_time` of every current volume, in snapshot order.
    pub fn format_times(&self) -> &[u64] {
        &self.format_times
    }
}

/// C3 for one current volume.
fn classify_volume(
    store: &ScanStore,
    by_offset: &[RecordId],
    cv: &CurrentVolume,
    classes: &mut [u8],
) {
    let rs = u64::from(cv.record_size);
    // Records in slots: (record, slot number).
    let mut slotted: Vec<(RecordId, u64)> = Vec::new();
    for (range, vpos) in cv.slot_ranges() {
        let first = by_offset
            .partition_point(|&id| store.record(id).map_or(0, |r| r.disk_offset()) < range.start);
        for &id in by_offset.get(first..).unwrap_or(&[]) {
            let Some(rec) = store.record(id) else {
                continue;
            };
            let off = rec.disk_offset();
            if off >= range.end {
                break;
            }
            let pos = off.saturating_sub(range.start).saturating_add(vpos);
            if pos.checked_rem(rs) != Some(0) {
                continue;
            }
            if let Some(n) = pos.checked_div(rs) {
                slotted.push((id, n));
            }
        }
    }
    // Pass 1: in use per the bitmap, or decided by the record time.
    let mut undecided: Vec<(RecordId, u64)> = Vec::new();
    for &(id, n) in &slotted {
        let Some(rec) = store.record(id) else {
            continue;
        };
        let class = if cv.slot_in_use(n) {
            Some(RecordClass::Live)
        } else {
            // C3 asks for a $STANDARD_INFORMATION and compares the record
            // time: a file copied in after the format keeps the source's
            // MFT-change time, but its $FILE_NAME times are new.
            let time = rec
                .has_standard_information()
                .then(|| rec.record_time().unwrap_or(0));
            match time {
                Some(t) if t >= cv.format_time => Some(RecordClass::PostFormat),
                Some(_) => Some(RecordClass::Old),
                None if rec.is_in_use() => Some(RecordClass::Old),
                None => None,
            }
        };
        match class {
            Some(c) => {
                if let Some(slot) = classes.get_mut(id.index()) {
                    *slot = c.code();
                }
            }
            None => undecided.push((id, n)),
        }
    }
    // Pass 2: free extension records without $STANDARD_INFORMATION and
    // with the in-use flag clear: old only when their base reference
    // resolves to an old record of the same $MFT.
    for (id, _) in undecided {
        let Some(rec) = store.record(id) else {
            continue;
        };
        let base: FileRef = rec.base();
        let base_old = cv
            .slot_offset(base.number())
            .and_then(|off| store.record_at(off))
            .filter(|&b| b != id)
            .and_then(|b| store.record(b).map(|r| (b, r)))
            .is_some_and(|(b, r)| {
                classes
                    .get(b.index())
                    .is_some_and(|&c| RecordClass::from_code(c) == RecordClass::Old)
                    && base.resolves_to(r.sequence(), r.is_in_use())
            });
        let c = if base_old {
            RecordClass::Old
        } else {
            RecordClass::PostFormat
        };
        if let Some(slot) = classes.get_mut(id.index()) {
            *slot = c.code();
        }
    }
    // Records inside $MFTMirr are live.
    for m in &cv.mirror {
        let first = by_offset
            .partition_point(|&id| store.record(id).map_or(0, |r| r.disk_offset()) < m.start);
        for &id in by_offset.get(first..).unwrap_or(&[]) {
            let Some(rec) = store.record(id) else {
                continue;
            };
            if rec.disk_offset() >= m.end {
                break;
            }
            if let Some(slot) = classes.get_mut(id.index()) {
                *slot = RecordClass::Live.code();
            }
        }
    }
}
