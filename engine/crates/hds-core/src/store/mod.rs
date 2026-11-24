//! The scan store: everything a scan finds (ALGORITHMS A3 to A5, B1), kept
//! in memory, and the read API the later steps build on.
//!
//! # What is stored
//!
//! * [`RawRecord`]s - one per `FILE` record that is not a never-used record
//!   and not a `$LogFile` copy (A3.1, A4);
//! * directory index entries ([`IndexEntryView`], A5) from `INDX` blocks
//!   and from `$INDEX_ROOT:$I30` attributes of stored records;
//! * one [`IndexBlockInfo`] per `INDX` block;
//! * [`BootHit`]s and packed [`CarveHit`]s (capped by the scanner);
//! * the done set and the bad ranges ([`RangeSet`]s of absolute bytes);
//! * the counters of what was dropped, and the warnings raised.
//!
//! # Memory layout
//!
//! No value is allocated per record. Everything lives in a handful of
//! containers that grow in discovery order, in fixed-size chunks (so that
//! nothing stored is ever moved and at most one chunk per container is
//! partly empty - a doubling `Vec` would carry up to as much spare capacity
//! as data and hold two copies while it grows):
//!
//! | container | chunk | element | bytes |
//! |---|---|---|---|
//! | `records` | 16384 items | [`RawRecord`]: offset, LSN, hash, base reference, the four `$STANDARD_INFORMATION` times, address and length of the aux region, number, sequence, flags, fixup status, size | 104 |
//! | `aux` | 1 MiB | the variable parts of all records: names, `$DATA` and `$INDEX_ALLOCATION` extents with their raw runlists, attribute-list entries, as tagged byte-packed items (see [`record`]) | 18 + 1 per unit per name (2 when a unit is 0x100 or above), 13 per resident `$DATA`, 41 + runlist per extent (25 + runlist after VCN 0), 21 per attribute-list entry |
//! | `entries`, `entry_names` | 16384 items, 256 KiB | index entries and their names | 48 + name |
//! | `blocks` | - | [`IndexBlockInfo`] | 48 |
//! | `boots` | - | [`BootHit`] | 72 |
//! | `carve` | 65536 items | [`CarveHit`] | 8 |
//!
//! A record therefore costs 104 bytes plus its items: about 160 bytes for
//! a file with one ASCII name and resident data, 190 to 200 with a
//! non-resident `$DATA` of one or two runs ([`ScanStore::memory_bytes`],
//! measured on real volumes in the crate's report).
//!
//! Records, entries and blocks are kept in discovery order and never
//! moved: a session file appends what is new since its last checkpoint.
//! Sorted views are permutations built on first use and cached
//! ([`records_by_offset`](ScanStore::records_by_offset),
//! [`index_entries_by_source`](ScanStore::index_entries_by_source),
//! [`index_blocks_by_offset`](ScanStore::index_blocks_by_offset)); the
//! fragment grouping of ALGORITHMS D1 is built on request
//! ([`fragment_index`](ScanStore::fragment_index)). Two stores holding the
//! same structures compare equal whatever the order they were found in.

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

mod chunked;
pub mod hits;
pub mod index;
pub mod record;

#[cfg(test)]
mod tests;

use std::ops::Range;
use std::sync::OnceLock;

use hds_io::{RangeSet, Warning};
use hds_ntfs::{FileRef, Namespace};
use serde::Serialize;

pub use hits::{carve_kind_code, carve_kind_from_code, BootHit, CarveHit};
pub use index::{EntrySource, EntrySourceKind, IndexBlockInfo, IndexEntryView};
pub use record::{
    AttrListEntryView, AttrListName, AttributeListState, DataExtent, FragmentKey, NameUnits,
    NameView, NonResidentExtent, RawRecord, RecordDamage, Reparse, ResidentData, StoredName,
    StoredRuns,
};

use chunked::{Arena, Chunked};
use index::{entry_bits, IndexEntryRec};
use record::{AuxItem, AuxItems};

/// Index of a record in the store (its position in discovery order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordId(pub u32);

impl RecordId {
    /// The position as a `usize`.
    #[must_use]
    pub fn index(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }
}

/// Index of an index entry in the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(pub u32);

impl EntryId {
    /// The position as a `usize`.
    #[must_use]
    pub fn index(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }
}

/// Counters of a scan. The first group is derived from what is stored; the
/// second counts what the rules of ALGORITHMS A3 dropped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
pub struct ScanCounters {
    /// Stored records.
    pub records: u64,
    /// In-use base records without the directory flag (A8).
    pub files: u64,
    /// In-use base records with the directory flag (A8).
    pub dirs: u64,
    /// Stored index entries.
    pub index_entries: u64,
    /// Stored `INDX` blocks.
    pub index_blocks: u64,
    /// Stored boot sectors.
    pub boot_hits: u64,
    /// Stored file-signature hits.
    pub carve_hits: u64,
    /// Bytes in bad ranges.
    pub bad_bytes: u64,
    /// Sectors whose first bytes passed the `FILE` header probe (stored or
    /// not); the "FILE hits" of A2.
    pub file_hits: u64,
    /// Records dropped as never used: not in use and without attributes.
    pub never_used: u64,
    /// Records dropped because every 512-byte block failed the fixup check
    /// (`$LogFile` copies).
    pub log_copies: u64,
    /// Sectors starting with `FILE` that are not a record (implausible
    /// header, refused by the parser).
    pub file_refused: u64,
    /// Sectors starting with `INDX` that are not an index block.
    pub indx_refused: u64,
    /// Sectors with the two NTFS boot signatures that do not parse.
    pub boot_refused: u64,
    /// Records and index blocks that start inside the session range but end
    /// behind it (they cannot be read without leaving the range).
    pub truncated: u64,
    /// File-signature hits dropped because the cap was reached.
    pub carve_dropped: u64,
    /// Structures dropped because a store limit was reached (4 billion
    /// items, 4 GiB of entry names, a record whose variable parts exceed
    /// 1 MiB); not reachable on real disks.
    pub capacity_dropped: u64,
}

/// The counters that are not derived from stored items (persisted in
/// session files).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct DropCounters {
    pub file_hits: u64,
    pub never_used: u64,
    pub log_copies: u64,
    pub file_refused: u64,
    pub indx_refused: u64,
    pub boot_refused: u64,
    pub truncated: u64,
    pub carve_dropped: u64,
    pub capacity_dropped: u64,
}

impl DropCounters {
    /// Number of values in the persisted form.
    pub(crate) const COUNT: usize = 9;

    pub(crate) fn to_array(self) -> [u64; Self::COUNT] {
        [
            self.file_hits,
            self.never_used,
            self.log_copies,
            self.file_refused,
            self.indx_refused,
            self.boot_refused,
            self.truncated,
            self.carve_dropped,
            self.capacity_dropped,
        ]
    }

    pub(crate) fn from_array(values: [u64; Self::COUNT]) -> Self {
        let [file_hits, never_used, log_copies, file_refused, indx_refused, boot_refused, truncated, carve_dropped, capacity_dropped] =
            values;
        DropCounters {
            file_hits,
            never_used,
            log_copies,
            file_refused,
            indx_refused,
            boot_refused,
            truncated,
            carve_dropped,
            capacity_dropped,
        }
    }

    pub(crate) fn add(&mut self, other: &DropCounters) {
        let mut mine = self.to_array();
        for (a, b) in mine.iter_mut().zip(other.to_array()) {
            *a = a.saturating_add(b);
        }
        *self = DropCounters::from_array(mine);
    }
}

/// Progress of the dead-source check of ALGORITHMS A7, persisted so that a
/// resumed scan does not start it again in the middle of the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ZeroCheck {
    /// Still looking: this many successfully read bytes were all zero.
    Checking(u64),
    /// A non-zero byte was seen; the check is over.
    NonZeroSeen,
    /// The warning `source_reads_zero` was raised.
    Raised,
}

impl Default for ZeroCheck {
    fn default() -> Self {
        ZeroCheck::Checking(0)
    }
}

/// The state of the store's containers at one moment, to roll back to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    records: usize,
    aux: (usize, usize),
    entries: usize,
    entry_names: (usize, usize),
    blocks: usize,
    boots: usize,
    carve: usize,
    bad_log: usize,
}

/// One fragment of ALGORITHMS D1: numbered records on one grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fragment<'a> {
    /// Record size in bytes.
    pub record_size: u32,
    /// `disk offset - number x record size`.
    pub origin: i64,
    /// The records, by ascending record number (and so by ascending offset).
    pub records: &'a [RecordId],
}

/// The numbered records grouped by `(record size, origin)` and sorted by
/// number inside each group, and the records without a number. Built by
/// [`ScanStore::fragment_index`].
#[derive(Clone, Debug, Default)]
pub struct FragmentIndex {
    order: Vec<RecordId>,
    groups: Vec<(u32, i64, Range<usize>)>,
    unnumbered: Vec<RecordId>,
}

impl FragmentIndex {
    /// The fragments, ordered by record size, then origin.
    pub fn fragments(&self) -> impl ExactSizeIterator<Item = Fragment<'_>> + '_ {
        self.groups
            .iter()
            .map(|(record_size, origin, span)| Fragment {
                record_size: *record_size,
                origin: *origin,
                records: self.order.get(span.clone()).unwrap_or(&[]),
            })
    }

    /// Number of fragments.
    #[must_use]
    pub fn fragment_count(&self) -> usize {
        self.groups.len()
    }

    /// Every numbered record in fragment order: by `(record size, origin,
    /// number)`.
    #[must_use]
    pub fn numbered(&self) -> &[RecordId] {
        &self.order
    }

    /// The included records without a record number (pre-3.1 headers), by
    /// ascending disk offset.
    #[must_use]
    pub fn unnumbered(&self) -> &[RecordId] {
        &self.unnumbered
    }
}

/// Everything a scan found. See the [module documentation](self).
#[derive(Clone, Default)]
pub struct ScanStore {
    pub(crate) records: Chunked<RawRecord, 14>,
    pub(crate) aux: Arena<20>,
    pub(crate) entries: Chunked<IndexEntryRec, 14>,
    pub(crate) entry_names: Arena<18>,
    pub(crate) blocks: Vec<IndexBlockInfo>,
    pub(crate) boots: Vec<BootHit>,
    pub(crate) carve: Chunked<CarveHit, 16>,
    pub(crate) done: RangeSet,
    pub(crate) bad: RangeSet,
    /// The bad ranges in the order they were recorded (`bad` is their union);
    /// a session file appends the new ones at each checkpoint.
    pub(crate) bad_log: Vec<Range<u64>>,
    pub(crate) drops: DropCounters,
    pub(crate) files: u64,
    pub(crate) dirs: u64,
    pub(crate) zero: ZeroCheck,
    pub(crate) warnings: Vec<Warning>,
    /// Scratch for assembling aux regions.
    scratch: Vec<u8>,
    by_offset: OnceLock<Vec<RecordId>>,
    entries_by_source: OnceLock<Vec<EntryId>>,
    blocks_by_offset: OnceLock<Vec<u32>>,
}

impl std::fmt::Debug for ScanStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanStore")
            .field("counters", &self.counters())
            .field("done", &self.done)
            .field("bad", &self.bad)
            .field("warnings", &self.warnings)
            .finish()
    }
}

/// At most this many items of one kind (ids are `u32`).
const MAX_ITEMS: usize = u32::MAX as usize;

fn to_u32(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

impl ScanStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> ScanStore {
        ScanStore::default()
    }

    // ----------------------------------------------------------------- records

    /// Number of stored records.
    #[must_use]
    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    /// The record with this id.
    #[must_use]
    pub fn record(&self, id: RecordId) -> Option<&RawRecord> {
        self.records.get(id.index())
    }

    /// All records in storage (discovery) order.
    pub fn records(&self) -> impl Iterator<Item = (RecordId, &RawRecord)> + '_ {
        self.records
            .iter()
            .enumerate()
            .map(|(i, record)| (RecordId(to_u32(i)), record))
    }

    fn offset_of(&self, id: RecordId) -> u64 {
        self.record(id).map_or(0, |r| r.disk_offset)
    }

    /// The record ids sorted by disk offset (built on first use, cached).
    pub fn records_by_offset(&self) -> &[RecordId] {
        self.by_offset.get_or_init(|| {
            let mut ids: Vec<RecordId> = (0..self.records.len())
                .map(|i| RecordId(to_u32(i)))
                .collect();
            // Discovery order consists of a few ascending runs (probe
            // regions, then the sweep): the stable sort merges them.
            ids.sort_by_key(|&id| self.offset_of(id));
            ids
        })
    }

    /// The record stored at exactly this disk offset.
    #[must_use]
    pub fn record_at(&self, disk_offset: u64) -> Option<RecordId> {
        let sorted = self.records_by_offset();
        let at = sorted.partition_point(|&id| self.offset_of(id) < disk_offset);
        let id = *sorted.get(at)?;
        (self.offset_of(id) == disk_offset).then_some(id)
    }

    /// The aux bytes of a record (see [`record`](self::record)).
    pub(crate) fn aux_region(&self, id: RecordId) -> &[u8] {
        self.record(id)
            .and_then(|r| {
                let len = usize::try_from(r.aux_len).ok()?;
                self.aux.get(r.aux_off, len)
            })
            .unwrap_or(&[])
    }

    /// The `$FILE_NAME` values of a record, in on-disk order.
    pub fn names(&self, id: RecordId) -> impl Iterator<Item = NameView<'_>> + '_ {
        AuxItems::new(self.aux_region(id)).filter_map(|item| match item {
            AuxItem::Name(name) => Some(name),
            _ => None,
        })
    }

    /// The unnamed `$DATA` extents of a record (resident or not), in
    /// on-disk order.
    pub fn data_extents(&self, id: RecordId) -> impl Iterator<Item = DataExtent<'_>> + '_ {
        AuxItems::new(self.aux_region(id)).filter_map(|item| match item {
            AuxItem::Data(extent) => Some(extent),
            _ => None,
        })
    }

    /// The `$INDEX_ALLOCATION:$I30` extents of a record, in on-disk order.
    pub fn index_allocation_extents(
        &self,
        id: RecordId,
    ) -> impl Iterator<Item = NonResidentExtent<'_>> + '_ {
        AuxItems::new(self.aux_region(id)).filter_map(|item| match item {
            AuxItem::IndexAllocation(extent) => Some(extent),
            _ => None,
        })
    }

    /// The entries of a resident `$ATTRIBUTE_LIST` for types 0x80 and 0xA0.
    pub fn attribute_list_entries(
        &self,
        id: RecordId,
    ) -> impl Iterator<Item = AttrListEntryView> + '_ {
        AuxItems::new(self.aux_region(id)).filter_map(|item| match item {
            AuxItem::AttrList(entry) => Some(entry),
            _ => None,
        })
    }

    /// Groups the numbered records for which `include` answers true by
    /// `(record size, origin)` and sorts each group by record number
    /// (ALGORITHMS D1). Records without a number are listed separately.
    /// Costs one sort of the included records and 24 bytes per record of
    /// temporary memory.
    pub fn fragment_index(
        &self,
        mut include: impl FnMut(RecordId, &RawRecord) -> bool,
    ) -> FragmentIndex {
        let mut keyed: Vec<(u8, u64, u32, RecordId)> = Vec::new();
        let mut unnumbered = Vec::new();
        for &id in self.records_by_offset() {
            let Some(record) = self.record(id) else {
                continue;
            };
            if !include(id, record) {
                continue;
            }
            match record.fragment_key() {
                // Order-preserving map of the signed origin to unsigned.
                Some(key) => keyed.push((
                    record.size_log2,
                    key.origin.cast_unsigned() ^ (1 << 63),
                    key.number,
                    id,
                )),
                None => unnumbered.push(id),
            }
        }
        keyed.sort_unstable_by_key(|&(size, origin, number, _)| (size, origin, number));
        let mut index = FragmentIndex {
            order: Vec::with_capacity(keyed.len()),
            groups: Vec::new(),
            unnumbered,
        };
        let mut start = 0_usize;
        for (i, &(size, origin, _, id)) in keyed.iter().enumerate() {
            index.order.push(id);
            let next = keyed.get(i.saturating_add(1));
            if next.is_none_or(|&(s, o, _, _)| (s, o) != (size, origin)) {
                let end = i.saturating_add(1);
                let record_size = 1_u32.checked_shl(u32::from(size)).unwrap_or(0);
                let origin = (origin ^ (1 << 63)).cast_signed();
                index.groups.push((record_size, origin, start..end));
                start = end;
            }
        }
        index
    }

    // ----------------------------------------------------------- index entries

    /// Number of stored index entries.
    #[must_use]
    pub fn index_entry_count(&self) -> usize {
        self.entries.len()
    }

    fn entry_name(&self, rec: &IndexEntryRec) -> &[u8] {
        self.entry_names
            .get(u64::from(rec.name_off), rec.name_bytes())
            .unwrap_or(&[])
    }

    fn entry_view<'a>(&'a self, rec: &IndexEntryRec) -> IndexEntryView<'a> {
        IndexEntryView {
            child: FileRef::from_raw(rec.child),
            parent: FileRef::from_raw(rec.parent),
            namespace: Namespace::from_u8(rec.namespace).unwrap_or(Namespace::Posix),
            name: StoredName::new(self.entry_name(rec), rec.bits & entry_bits::NARROW == 0),
            mft_changed: rec.mft_changed,
            source: EntrySource {
                offset: rec.source_offset,
                lsn: rec.source_lsn,
                kind: if rec.bits & entry_bits::FROM_ROOT != 0 {
                    EntrySourceKind::RecordRoot
                } else {
                    EntrySourceKind::Block
                },
                torn: rec.bits & entry_bits::SOURCE_TORN != 0,
            },
        }
    }

    /// The index entry with this id.
    #[must_use]
    pub fn index_entry(&self, id: EntryId) -> Option<IndexEntryView<'_>> {
        self.entries.get(id.index()).map(|rec| self.entry_view(rec))
    }

    /// An index entry as stored, with its name bytes.
    pub(crate) fn entry_raw(&self, id: EntryId) -> Option<(&IndexEntryRec, &[u8])> {
        let rec = self.entries.get(id.index())?;
        Some((rec, self.entry_name(rec)))
    }

    /// All index entries in storage order; the entries of one block or
    /// index root are consecutive and in on-disk order.
    pub fn index_entries(&self) -> impl Iterator<Item = (EntryId, IndexEntryView<'_>)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .map(|(i, rec)| (EntryId(to_u32(i)), self.entry_view(rec)))
    }

    /// The entry ids sorted by source offset (stable: the entries of one
    /// source stay in on-disk order). Built on first use, cached.
    pub fn index_entries_by_source(&self) -> &[EntryId] {
        self.entries_by_source.get_or_init(|| {
            let mut ids: Vec<EntryId> = (0..self.entries.len())
                .map(|i| EntryId(to_u32(i)))
                .collect();
            ids.sort_by_key(|id| self.entries.get(id.index()).map_or(0, |e| e.source_offset));
            ids
        })
    }

    // ------------------------------------------------------------ index blocks

    /// All `INDX` blocks in storage order.
    #[must_use]
    pub fn index_blocks(&self) -> &[IndexBlockInfo] {
        &self.blocks
    }

    fn block(&self, i: u32) -> Option<&IndexBlockInfo> {
        usize::try_from(i).ok().and_then(|i| self.blocks.get(i))
    }

    /// Positions in [`index_blocks`](Self::index_blocks), sorted by disk
    /// offset. Built on first use, cached.
    pub fn index_blocks_by_offset(&self) -> &[u32] {
        self.blocks_by_offset.get_or_init(|| {
            let mut ids: Vec<u32> = (0..self.blocks.len()).map(to_u32).collect();
            ids.sort_by_key(|&i| self.block(i).map_or(0, |b| b.disk_offset));
            ids
        })
    }

    /// The block stored at exactly this disk offset.
    #[must_use]
    pub fn index_block_at(&self, disk_offset: u64) -> Option<&IndexBlockInfo> {
        let sorted = self.index_blocks_by_offset();
        let at =
            sorted.partition_point(|&i| self.block(i).map_or(0, |b| b.disk_offset) < disk_offset);
        self.block(*sorted.get(at)?)
            .filter(|b| b.disk_offset == disk_offset)
    }

    // -------------------------------------------------------------------- hits

    /// All boot sectors in storage order.
    #[must_use]
    pub fn boot_hits(&self) -> &[BootHit] {
        &self.boots
    }

    /// Number of stored file-signature hits.
    #[must_use]
    pub fn carve_hit_count(&self) -> usize {
        self.carve.len()
    }

    /// All file-signature hits in storage order.
    pub fn carve_hits(&self) -> impl Iterator<Item = CarveHit> + '_ {
        self.carve.iter().copied()
    }

    /// The file-signature hits sorted by offset, in a new vector
    /// (ALGORITHMS I3 processes candidates in ascending order).
    #[must_use]
    pub fn carve_hits_sorted(&self) -> Vec<CarveHit> {
        let mut hits: Vec<CarveHit> = self.carve_hits().collect();
        hits.sort_unstable();
        hits
    }

    // ------------------------------------------------------------- scan state

    /// The parts of the session range that have been scanned.
    #[must_use]
    pub fn done(&self) -> &RangeSet {
        &self.done
    }

    /// Unreadable (zero-filled) parts of the done set.
    #[must_use]
    pub fn bad_ranges(&self) -> &RangeSet {
        &self.bad
    }

    /// The counters.
    #[must_use]
    pub fn counters(&self) -> ScanCounters {
        let len = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        ScanCounters {
            records: len(self.records.len()),
            files: self.files,
            dirs: self.dirs,
            index_entries: len(self.entries.len()),
            index_blocks: len(self.blocks.len()),
            boot_hits: len(self.boots.len()),
            carve_hits: len(self.carve.len()),
            bad_bytes: self.bad.total_len(),
            file_hits: self.drops.file_hits,
            never_used: self.drops.never_used,
            log_copies: self.drops.log_copies,
            file_refused: self.drops.file_refused,
            indx_refused: self.drops.indx_refused,
            boot_refused: self.drops.boot_refused,
            truncated: self.drops.truncated,
            carve_dropped: self.drops.carve_dropped,
            capacity_dropped: self.drops.capacity_dropped,
        }
    }

    /// The warnings raised so far (each code at most once).
    #[must_use]
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// Raises a warning unless one with the same code is there; returns
    /// whether it is new.
    pub fn raise_warning(&mut self, code: &str, message: &str) -> bool {
        if self.warnings.iter().any(|w| w.code == code) {
            return false;
        }
        self.warnings.push(Warning {
            code: code.to_string(),
            message: message.to_string(),
        });
        true
    }

    /// Heap bytes held by the store, cached permutations included (range
    /// sets estimated at 48 bytes per range).
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        fn vec_bytes<T>(v: &Vec<T>) -> usize {
            v.capacity().saturating_mul(std::mem::size_of::<T>())
        }
        let sets = self
            .done
            .range_count()
            .saturating_add(self.bad.range_count())
            .saturating_mul(48);
        [
            self.records.heap_bytes(),
            self.aux.heap_bytes(),
            self.entries.heap_bytes(),
            self.entry_names.heap_bytes(),
            vec_bytes(&self.blocks),
            vec_bytes(&self.boots),
            self.carve.heap_bytes(),
            vec_bytes(&self.bad_log),
            vec_bytes(&self.scratch),
            self.by_offset.get().map_or(0, vec_bytes),
            self.entries_by_source.get().map_or(0, vec_bytes),
            self.blocks_by_offset.get().map_or(0, vec_bytes),
            sets,
        ]
        .iter()
        .fold(0_usize, |sum, &n| sum.saturating_add(n))
    }

    /// Bytes the records take without spare capacity: 104 per record plus
    /// their aux bytes.
    #[must_use]
    pub fn record_bytes(&self) -> usize {
        self.records
            .len()
            .saturating_mul(std::mem::size_of::<RawRecord>())
            .saturating_add(self.aux.len())
    }

    // --------------------------------------------------- crate-internal writes

    pub(crate) fn mark(&self) -> Mark {
        Mark {
            records: self.records.len(),
            aux: self.aux.mark(),
            entries: self.entries.len(),
            entry_names: self.entry_names.mark(),
            blocks: self.blocks.len(),
            boots: self.boots.len(),
            carve: self.carve.len(),
            bad_log: self.bad_log.len(),
        }
    }

    /// Drops everything added after `mark` (records, entries, blocks, hits,
    /// bad ranges). Counters, the done set and warnings are not touched:
    /// their changes are only applied when a chunk is committed.
    pub(crate) fn rollback(&mut self, mark: Mark) {
        let (mut files, mut dirs) = (0_u64, 0_u64);
        for record in self.records.iter_from(mark.records) {
            files = files.saturating_add(u64::from(is_file(record)));
            dirs = dirs.saturating_add(u64::from(is_dir(record)));
        }
        self.files = self.files.saturating_sub(files);
        self.dirs = self.dirs.saturating_sub(dirs);
        self.records.truncate(mark.records);
        self.aux.rollback(mark.aux);
        self.entries.truncate(mark.entries);
        self.entry_names.rollback(mark.entry_names);
        self.blocks.truncate(mark.blocks);
        self.boots.truncate(mark.boots);
        self.carve.truncate(mark.carve);
        if self.bad_log.len() > mark.bad_log {
            self.bad_log.truncate(mark.bad_log);
            self.bad = self.bad_log.iter().cloned().collect();
        }
        self.invalidate();
    }

    /// Forgets the cached permutations.
    pub(crate) fn invalidate(&mut self) {
        self.by_offset.take();
        self.entries_by_source.take();
        self.blocks_by_offset.take();
    }

    /// Appends a record and the aux region built for it. Returns false (and
    /// stores nothing) when a limit is reached.
    pub(crate) fn push_record(&mut self, record: RawRecord, aux: &record::AuxBuilder) -> bool {
        if aux.overflow {
            return false;
        }
        let mut scratch = std::mem::take(&mut self.scratch);
        let pushed = self.push_record_raw(record, aux.assemble(&mut scratch));
        self.scratch = scratch;
        pushed
    }

    /// Appends a record with its aux region as bytes (already validated).
    pub(crate) fn push_record_raw(&mut self, mut record: RawRecord, region: &[u8]) -> bool {
        if self.records.len() >= MAX_ITEMS {
            return false;
        }
        let (Some(address), Ok(len)) = (self.aux.push(region), u32::try_from(region.len())) else {
            return false;
        };
        record.aux_off = address;
        record.aux_len = len;
        self.files = self.files.saturating_add(u64::from(is_file(&record)));
        self.dirs = self.dirs.saturating_add(u64::from(is_dir(&record)));
        self.records.push(record);
        if self.by_offset.get().is_some() {
            self.by_offset.take();
        }
        true
    }

    /// Appends an index entry; false when a limit is reached.
    pub(crate) fn push_entry(&mut self, mut rec: IndexEntryRec, name: &[u8]) -> bool {
        if self.entries.len() >= MAX_ITEMS {
            return false;
        }
        let mark = self.entry_names.mark();
        let Some(address) = self.entry_names.push(name) else {
            return false;
        };
        let Ok(name_off) = u32::try_from(address) else {
            self.entry_names.rollback(mark);
            return false;
        };
        rec.name_off = name_off;
        self.entries.push(rec);
        if self.entries_by_source.get().is_some() {
            self.entries_by_source.take();
        }
        true
    }

    /// Appends an index block; false when a limit is reached.
    pub(crate) fn push_block(&mut self, block: IndexBlockInfo) -> bool {
        if self.blocks.len() >= MAX_ITEMS {
            return false;
        }
        self.blocks.push(block);
        if self.blocks_by_offset.get().is_some() {
            self.blocks_by_offset.take();
        }
        true
    }

    /// Appends a boot hit; false when a limit is reached.
    pub(crate) fn push_boot(&mut self, hit: BootHit) -> bool {
        if self.boots.len() >= MAX_ITEMS {
            return false;
        }
        self.boots.push(hit);
        true
    }

    /// Appends a file-signature hit.
    pub(crate) fn push_carve(&mut self, hit: CarveHit) {
        self.carve.push(hit);
    }

    /// Records unreadable sectors.
    pub(crate) fn add_bad(&mut self, range: Range<u64>) {
        if range.start < range.end {
            self.bad.insert(range.clone());
            self.bad_log.push(range);
        }
    }

    // ------------------------------------------------------- logical equality

    /// Canonical form of one record: its fixed fields and its aux bytes.
    fn record_key(&self, id: RecordId) -> Option<(u64, [u64; 12], &[u8])> {
        let r = self.record(id)?;
        let [c, m, x, a] = r.si_times;
        Some((
            r.disk_offset,
            [
                r.lsn,
                r.hash,
                r.base,
                c,
                m,
                x,
                a,
                u64::from(r.si_flags),
                u64::from(r.number),
                u64::from(r.reparse_tag),
                (u64::from(r.sequence) << 48)
                    | (u64::from(r.header_flags) << 32)
                    | (u64::from(r.link_count) << 16)
                    | u64::from(r.bits),
                (u64::from(r.fixup_first_bad) << 16)
                    | (u64::from(r.fixup_bad) << 8)
                    | u64::from(r.size_log2),
            ],
            self.aux_region(id),
        ))
    }
}

fn is_file(record: &RawRecord) -> bool {
    record.is_in_use() && !record.is_extension() && !record.is_directory()
}

fn is_dir(record: &RawRecord) -> bool {
    record.is_in_use() && !record.is_extension() && record.is_directory()
}

impl PartialEq for ScanStore {
    /// Logical equality: the same structures with the same fields, the same
    /// done set, bad ranges, counters and warnings, whatever the order in
    /// which they were found and stored.
    fn eq(&self, other: &Self) -> bool {
        if self.counters() != other.counters()
            || self.done != other.done
            || self.bad != other.bad
            || self.zero != other.zero
        {
            return false;
        }
        let records_equal = self
            .records_by_offset()
            .iter()
            .zip(other.records_by_offset())
            .all(|(&a, &b)| self.record_key(a) == other.record_key(b));
        let entries_equal = self
            .index_entries_by_source()
            .iter()
            .zip(other.index_entries_by_source())
            .all(|(&a, &b)| self.index_entry(a) == other.index_entry(b));
        let blocks_equal = self
            .index_blocks_by_offset()
            .iter()
            .zip(other.index_blocks_by_offset())
            .all(|(&a, &b)| self.block(a) == other.block(b));
        let mut boots_a = self.boots.clone();
        let mut boots_b = other.boots.clone();
        boots_a.sort_by_key(|b| b.disk_offset);
        boots_b.sort_by_key(|b| b.disk_offset);
        let mut warnings_a: Vec<_> = self
            .warnings
            .iter()
            .map(|w| (&w.code, &w.message))
            .collect();
        let mut warnings_b: Vec<_> = other
            .warnings
            .iter()
            .map(|w| (&w.code, &w.message))
            .collect();
        warnings_a.sort();
        warnings_b.sort();
        records_equal
            && entries_equal
            && blocks_equal
            && boots_a == boots_b
            && self.carve_hits_sorted() == other.carve_hits_sorted()
            && warnings_a == warnings_b
    }
}
