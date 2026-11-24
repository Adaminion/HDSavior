//! Payloads of the session-file blocks (format version 1).
//!
//! Every integer is little-endian; strings are a u32 byte length and UTF-8.
//! Each decoder validates a whole payload before it changes the store, so a
//! block is taken completely or not at all.
//!
//! | block | payload |
//! |---|---|
//! | `RECORDS` | u32 n; n x 91-byte records (offset, LSN, hash, base, the four `$STANDARD_INFORMATION` times, `$SI` flags, number, reparse tag, aux length, sequence, header flags, link count, flag bits, fixup first bad block and count, log2 of the size); u64 aux length; the aux regions of the records in order (see `store::record`) |
//! | `INDEX_ENTRIES` | u32 n; n x 47 bytes (child, parent, MFT-change time, source offset, source LSN, name offset relative to this block, name length, namespace, flag bits); u64 name-bytes length; name bytes |
//! | `INDEX_BLOCKS` | u32 n; n x 39 bytes (offset, VCN, LSN, owner, size, owner present, fixup first bad block and count) |
//! | `BOOT_HITS` | u32 n; n x 64 bytes (offset and the fields of the boot sector) |
//! | `CARVE_HITS` | u64 n; n x u64 packed hits |
//! | `BAD_RANGES` | u64 n; n x (start u64, end u64) |
//! | `LIVE`, `HEADS`, `OVERRIDES`, `CARVED` | u8 flags (bit 0: discard the earlier payloads of this kind; bit 1: no payload follows), then the owner's bytes |
//! | `CHECKPOINT` | u64 sequence; identity (kind u8, path, size u64, sector sizes 2 x u32, model and serial as optional strings); range (2 x u64); options (carve hits u8, maximum record size u32); totals (records, entries, blocks, boot hits, carve hits, bad bytes, and the four opaque list lengths: 10 x u64); done set (u64 n, n x (start, end)); u32 n and n x u64 drop counters; zero-check state (u8, u64); u32 n and n warnings (code, message) |

use std::ops::Range;

use hds_io::{RangeSet, SourceKind, Warning};
use hds_ntfs::BootSector;

use super::wire::{Get, Malformed, Put};
use super::{SessionMeta, SourceIdentity};
use crate::scan::ScanOptions;
use crate::store::index::{entry_bits, IndexEntryRec};
use crate::store::record::{bits, validate_region, ItemCounts, RawRecord};
use crate::store::{
    hits::boot_sector_is_sound, BootHit, CarveHit, DropCounters, IndexBlockInfo, RecordId,
    ScanStore, ZeroCheck,
};

/// Encoded size of one record in a `RECORDS` payload.
pub(crate) const RECORD_WIRE: usize = 91;
const ENTRY_WIRE: usize = 47;
const BLOCK_WIRE: usize = 39;
const BOOT_WIRE: usize = 64;
/// At most this many warnings in a checkpoint.
const MAX_WARNINGS: usize = 256;

/// Opaque block flag: discard the earlier payloads of this kind.
pub(crate) const OPAQUE_RESET: u8 = 1;
/// Opaque block flag: no payload follows (a bare reset).
pub(crate) const OPAQUE_EMPTY: u8 = 2;

fn usize_of(n: u64) -> Result<usize, Malformed> {
    usize::try_from(n).map_err(|_| Malformed("value too large"))
}

/// Whether `start..start + len` lies in the session range at a multiple of
/// 512 bytes.
fn inside(meta: &SessionMeta, start: u64, len: u64) -> bool {
    start.is_multiple_of(512)
        && start >= meta.range.start
        && start
            .checked_add(len)
            .is_some_and(|end| end <= meta.range.end)
}

fn fixup_sound(first_bad: u8, bad: u8, blocks: u32) -> bool {
    if bad == 0 {
        first_bad == 0
    } else {
        u32::from(bad) <= blocks && u32::from(first_bad) < blocks
    }
}

// ------------------------------------------------------------------ records

/// Collects records into one `RECORDS` payload.
#[derive(Debug, Default)]
pub(crate) struct RecordsEncoder {
    count: u32,
    fixed: Vec<u8>,
    aux: Vec<u8>,
}

impl RecordsEncoder {
    /// Adds a record with its aux region. Returns false when the payload
    /// cannot take it (count overflow); the caller starts a new block.
    pub(crate) fn push(&mut self, record: &RawRecord, region: &[u8]) -> bool {
        let Some(count) = self.count.checked_add(1) else {
            return false;
        };
        let Ok(aux_len) = u32::try_from(region.len()) else {
            return false;
        };
        self.count = count;
        let f = &mut self.fixed;
        f.u64(record.disk_offset);
        f.u64(record.lsn);
        f.u64(record.hash);
        f.u64(record.base);
        for t in record.si_times {
            f.u64(t);
        }
        f.u32(record.si_flags);
        f.u32(record.number);
        f.u32(record.reparse_tag);
        f.u32(aux_len);
        f.u16(record.sequence);
        f.u16(record.header_flags);
        f.u16(record.link_count);
        f.u16(record.bits);
        f.u8(record.fixup_first_bad);
        f.u8(record.fixup_bad);
        f.u8(record.size_log2);
        self.aux.extend_from_slice(region);
        true
    }

    /// Bytes of the payload so far.
    pub(crate) fn len(&self) -> usize {
        self.fixed.len().saturating_add(self.aux.len())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len().saturating_add(12));
        out.u32(self.count);
        out.bytes(&self.fixed);
        out.u64(u64::try_from(self.aux.len()).unwrap_or(u64::MAX));
        out.bytes(&self.aux);
        out
    }
}

/// Encodes the records `ids` of `store` in that order into payloads of at
/// most about `limit` bytes each.
pub(crate) fn encode_records(
    store: &ScanStore,
    ids: impl Iterator<Item = RecordId>,
    limit: usize,
    mut emit: impl FnMut(Vec<u8>) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut encoder = RecordsEncoder::default();
    for id in ids {
        let (Some(record), region) = (store.record(id), store.aux_region(id)) else {
            continue;
        };
        if encoder.len() >= limit || !encoder.push(record, region) {
            emit(std::mem::take(&mut encoder).finish())?;
            encoder.push(record, region);
        }
    }
    if !encoder.is_empty() {
        emit(encoder.finish())?;
    }
    Ok(())
}

fn record_sound(r: &RawRecord, meta: &SessionMeta, counts: ItemCounts) -> bool {
    let size_ok =
        (10..=16).contains(&r.size_log2) && r.record_size() <= meta.options.max_record_size;
    let blocks = r.record_size() / 512;
    let b = r.bits;
    let list = b & bits::ATTR_LIST != 0;
    let resident = b & bits::ATTR_LIST_RESIDENT != 0;
    size_ok
        && inside(meta, r.disk_offset, u64::from(r.record_size()))
        && b & !bits::ALL == 0
        && fixup_sound(r.fixup_first_bad, r.fixup_bad, blocks)
        // Records in which every block failed are never stored.
        && u32::from(r.fixup_bad) < blocks
        // Never-used records are never stored.
        && (r.is_in_use() || r.has_attributes())
        && (b & bits::HAS_NUMBER != 0 || r.number == 0)
        && (b & bits::HAS_SI != 0 || (r.si_times == [0; 4] && r.si_flags == 0))
        && (b & bits::REPARSE_TAG == 0 || b & bits::REPARSE != 0)
        && (b & bits::REPARSE_TAG != 0 || r.reparse_tag == 0)
        && (list || b & (bits::ATTR_LIST_RESIDENT | bits::ATTR_LIST_INCOMPLETE | bits::ATTR_LIST_MALFORMED) == 0)
        && !(resident && b & bits::ATTR_LIST_MALFORMED != 0)
        && (resident || b & bits::ATTR_LIST_INCOMPLETE == 0)
        && (resident || counts.al == 0)
}

/// Decodes a `RECORDS` payload and appends its records to `store`.
pub(crate) fn decode_records(
    payload: &[u8],
    meta: &SessionMeta,
    store: &mut ScanStore,
) -> Result<(), Malformed> {
    let mut get = Get::new(payload);
    let count = get.count(false, RECORD_WIRE)?;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let disk_offset = get.u64()?;
        let lsn = get.u64()?;
        let hash = get.u64()?;
        let base = get.u64()?;
        let si_times = [get.u64()?, get.u64()?, get.u64()?, get.u64()?];
        records.push(RawRecord {
            disk_offset,
            lsn,
            hash,
            base,
            si_times,
            aux_off: 0,
            si_flags: get.u32()?,
            number: get.u32()?,
            reparse_tag: get.u32()?,
            aux_len: get.u32()?,
            sequence: get.u16()?,
            header_flags: get.u16()?,
            link_count: get.u16()?,
            bits: get.u16()?,
            fixup_first_bad: get.u8()?,
            fixup_bad: get.u8()?,
            size_log2: get.u8()?,
        });
    }
    let aux_len = usize_of(get.u64()?)?;
    let aux = get.bytes(aux_len)?;
    get.finish()?;

    // The regions follow each other and fill the aux bytes exactly.
    let mut regions: Vec<Range<usize>> = Vec::with_capacity(records.len());
    let mut start = 0_usize;
    for r in &records {
        let end = start
            .checked_add(usize::try_from(r.aux_len).map_err(|_| Malformed("aux length"))?)
            .ok_or(Malformed("aux length"))?;
        let region = aux.get(start..end).ok_or(Malformed("record aux region"))?;
        let counts = validate_region(region).ok_or(Malformed("record aux items"))?;
        if !record_sound(r, meta, counts) {
            return Err(Malformed("record fields"));
        }
        regions.push(start..end);
        start = end;
    }
    if start != aux.len() {
        return Err(Malformed("aux bytes not used by any record"));
    }
    for (record, region) in records.into_iter().zip(regions) {
        let bytes = aux.get(region).unwrap_or(&[]);
        if !store.push_record_raw(record, bytes) {
            return Err(Malformed("too many records"));
        }
    }
    Ok(())
}

// ------------------------------------------------------------ index entries

/// Encodes index entries (in the given order) into one payload.
pub(crate) fn encode_entries(
    store: &ScanStore,
    ids: impl Iterator<Item = crate::store::EntryId>,
) -> Vec<u8> {
    let mut fixed = Vec::new();
    let mut names: Vec<u8> = Vec::new();
    let mut count = 0_u32;
    for id in ids {
        let Some((rec, name)) = store.entry_raw(id) else {
            continue;
        };
        let Ok(name_off) = u32::try_from(names.len()) else {
            break;
        };
        let Some(next) = count.checked_add(1) else {
            break;
        };
        count = next;
        fixed.u64(rec.child);
        fixed.u64(rec.parent);
        fixed.u64(rec.mft_changed);
        fixed.u64(rec.source_offset);
        fixed.u64(rec.source_lsn);
        fixed.u32(name_off);
        fixed.u8(rec.name_len);
        fixed.u8(rec.namespace);
        fixed.u8(rec.bits);
        names.extend_from_slice(name);
    }
    let mut out = Vec::with_capacity(fixed.len().saturating_add(names.len()).saturating_add(12));
    out.u32(count);
    out.bytes(&fixed);
    out.u64(u64::try_from(names.len()).unwrap_or(u64::MAX));
    out.bytes(&names);
    out
}

/// Decodes an `INDEX_ENTRIES` payload into `store`.
pub(crate) fn decode_entries(
    payload: &[u8],
    meta: &SessionMeta,
    store: &mut ScanStore,
) -> Result<(), Malformed> {
    let mut get = Get::new(payload);
    let count = get.count(false, ENTRY_WIRE)?;
    let mut recs = Vec::with_capacity(count);
    for _ in 0..count {
        recs.push(IndexEntryRec {
            child: get.u64()?,
            parent: get.u64()?,
            mft_changed: get.u64()?,
            source_offset: get.u64()?,
            source_lsn: get.u64()?,
            name_off: get.u32()?,
            name_len: get.u8()?,
            namespace: get.u8()?,
            bits: get.u8()?,
        });
    }
    let names_len = usize_of(get.u64()?)?;
    let names = get.bytes(names_len)?;
    get.finish()?;
    for rec in &recs {
        let start = usize::try_from(rec.name_off).map_err(|_| Malformed("entry name"))?;
        let end = start
            .checked_add(rec.name_bytes())
            .ok_or(Malformed("entry name"))?;
        if rec.name_len == 0
            || rec.namespace > 3
            || rec.bits & !entry_bits::ALL != 0
            || end > names.len()
            || !inside(meta, rec.source_offset, 0)
            || rec.source_offset >= meta.range.end
        {
            return Err(Malformed("index entry fields"));
        }
    }
    for rec in recs {
        let start = usize::try_from(rec.name_off).unwrap_or(usize::MAX);
        let name = names
            .get(start..start.saturating_add(rec.name_bytes()))
            .unwrap_or(&[]);
        if !store.push_entry(rec, name) {
            return Err(Malformed("too many index entries"));
        }
    }
    Ok(())
}

// ------------------------------------------------------------- index blocks

/// Encodes index blocks into one payload.
pub(crate) fn encode_blocks<'a>(blocks: impl Iterator<Item = &'a IndexBlockInfo>) -> Vec<u8> {
    let mut body = Vec::new();
    let mut count = 0_u32;
    for b in blocks {
        let Some(next) = count.checked_add(1) else {
            break;
        };
        count = next;
        body.u64(b.disk_offset);
        body.u64(b.vcn);
        body.u64(b.lsn);
        body.u64(b.owner.map_or(0, |o| o.raw()));
        body.u32(b.size);
        body.u8(u8::from(b.owner.is_some()));
        let (first, bad) = match b.fixup {
            hds_ntfs::FixupStatus::Ok => (0, 0),
            hds_ntfs::FixupStatus::Torn {
                first_bad_block,
                bad_blocks,
            } => (first_bad_block, bad_blocks),
        };
        body.u8(first);
        body.u8(bad);
    }
    let mut out = Vec::with_capacity(body.len().saturating_add(4));
    out.u32(count);
    out.bytes(&body);
    out
}

/// Decodes an `INDEX_BLOCKS` payload into `store`.
pub(crate) fn decode_blocks(
    payload: &[u8],
    meta: &SessionMeta,
    store: &mut ScanStore,
) -> Result<(), Malformed> {
    let mut get = Get::new(payload);
    let count = get.count(false, BLOCK_WIRE)?;
    let mut blocks = Vec::with_capacity(count);
    for _ in 0..count {
        let disk_offset = get.u64()?;
        let vcn = get.u64()?;
        let lsn = get.u64()?;
        let owner = get.u64()?;
        let size = get.u32()?;
        let has_owner = get.bool()?;
        let first = get.u8()?;
        let bad = get.u8()?;
        let size_ok = size.is_power_of_two() && (512..=65_536).contains(&size);
        if !size_ok
            || !inside(meta, disk_offset, u64::from(size))
            || (!has_owner && owner != 0)
            || !fixup_sound(first, bad, size / 512)
        {
            return Err(Malformed("index block fields"));
        }
        blocks.push(IndexBlockInfo {
            disk_offset,
            vcn,
            size,
            lsn,
            owner: has_owner.then(|| hds_ntfs::FileRef::from_raw(owner)),
            fixup: if bad == 0 {
                hds_ntfs::FixupStatus::Ok
            } else {
                hds_ntfs::FixupStatus::Torn {
                    first_bad_block: first,
                    bad_blocks: bad,
                }
            },
        });
    }
    get.finish()?;
    for b in blocks {
        if !store.push_block(b) {
            return Err(Malformed("too many index blocks"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- boot hits

/// Encodes boot hits into one payload.
pub(crate) fn encode_boots<'a>(hits: impl Iterator<Item = &'a BootHit>) -> Vec<u8> {
    let mut body = Vec::new();
    let mut count = 0_u32;
    for h in hits {
        let Some(next) = count.checked_add(1) else {
            break;
        };
        count = next;
        let b = &h.boot;
        body.u64(h.disk_offset);
        body.u32(b.bytes_per_sector);
        body.u32(b.sectors_per_cluster);
        body.u32(b.cluster_size);
        body.u64(b.total_sectors);
        body.u32(b.hidden_sectors);
        body.u64(b.mft_lcn);
        body.u64(b.mftmirr_lcn);
        body.u32(b.mft_record_size);
        body.u32(b.index_block_size);
        body.u64(b.serial);
    }
    let mut out = Vec::with_capacity(body.len().saturating_add(4));
    out.u32(count);
    out.bytes(&body);
    out
}

/// Decodes a `BOOT_HITS` payload into `store`.
pub(crate) fn decode_boots(
    payload: &[u8],
    meta: &SessionMeta,
    store: &mut ScanStore,
) -> Result<(), Malformed> {
    let mut get = Get::new(payload);
    let count = get.count(false, BOOT_WIRE)?;
    let mut hits = Vec::with_capacity(count);
    for _ in 0..count {
        let disk_offset = get.u64()?;
        let boot = BootSector {
            bytes_per_sector: get.u32()?,
            sectors_per_cluster: get.u32()?,
            cluster_size: get.u32()?,
            total_sectors: get.u64()?,
            hidden_sectors: get.u32()?,
            mft_lcn: get.u64()?,
            mftmirr_lcn: get.u64()?,
            mft_record_size: get.u32()?,
            index_block_size: get.u32()?,
            serial: get.u64()?,
        };
        if !boot_sector_is_sound(&boot) || !inside(meta, disk_offset, 512) {
            return Err(Malformed("boot sector fields"));
        }
        hits.push(BootHit { disk_offset, boot });
    }
    get.finish()?;
    for h in hits {
        if !store.push_boot(h) {
            return Err(Malformed("too many boot sectors"));
        }
    }
    Ok(())
}

// --------------------------------------------------------------- carve hits

/// Encodes carve hits into one payload.
pub(crate) fn encode_carve(hits: &[CarveHit]) -> Vec<u8> {
    let mut out = Vec::with_capacity(hits.len().saturating_mul(8).saturating_add(8));
    out.u64(u64::try_from(hits.len()).unwrap_or(u64::MAX));
    for h in hits {
        out.u64(h.raw());
    }
    out
}

/// Decodes a `CARVE_HITS` payload into `store`.
pub(crate) fn decode_carve(
    payload: &[u8],
    meta: &SessionMeta,
    store: &mut ScanStore,
) -> Result<(), Malformed> {
    let mut get = Get::new(payload);
    let count = get.count(true, 8)?;
    let mut hits = Vec::with_capacity(count);
    for _ in 0..count {
        let hit = CarveHit::from_raw(get.u64()?).ok_or(Malformed("carve hit kind"))?;
        if !inside(meta, hit.disk_offset(), 512) {
            return Err(Malformed("carve hit offset"));
        }
        hits.push(hit);
    }
    get.finish()?;
    for hit in hits {
        store.push_carve(hit);
    }
    Ok(())
}

// --------------------------------------------------------------- bad ranges

/// Encodes bad ranges into one payload.
pub(crate) fn encode_bad(ranges: &[Range<u64>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ranges.len().saturating_mul(16).saturating_add(8));
    out.u64(u64::try_from(ranges.len()).unwrap_or(u64::MAX));
    for r in ranges {
        out.u64(r.start);
        out.u64(r.end);
    }
    out
}

/// Decodes a `BAD_RANGES` payload into `store`.
pub(crate) fn decode_bad(
    payload: &[u8],
    meta: &SessionMeta,
    store: &mut ScanStore,
) -> Result<(), Malformed> {
    let mut get = Get::new(payload);
    let count = get.count(true, 16)?;
    let mut ranges = Vec::with_capacity(count);
    for _ in 0..count {
        let start = get.u64()?;
        let end = get.u64()?;
        if start >= end || start < meta.range.start || end > meta.range.end {
            return Err(Malformed("bad range"));
        }
        ranges.push(start..end);
    }
    get.finish()?;
    for r in ranges {
        store.add_bad(r);
    }
    Ok(())
}

// ------------------------------------------------------------------- opaque

/// Encodes an opaque block: flags and the owner's bytes.
pub(crate) fn encode_opaque(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len().saturating_add(1));
    out.u8(flags);
    out.bytes(payload);
    out
}

/// Decodes an opaque block into its flags and the owner's bytes.
pub(crate) fn decode_opaque(payload: &[u8]) -> Result<(u8, &[u8]), Malformed> {
    let (&flags, rest) = payload
        .split_first()
        .ok_or(Malformed("empty opaque block"))?;
    if flags & !(OPAQUE_RESET | OPAQUE_EMPTY) != 0
        || (flags & OPAQUE_EMPTY != 0 && (flags & OPAQUE_RESET == 0 || !rest.is_empty()))
    {
        return Err(Malformed("opaque block flags"));
    }
    Ok((flags, rest))
}

// --------------------------------------------------------------- checkpoint

/// Counts of everything committed up to a checkpoint, for the consistency
/// check on load.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub records: u64,
    pub entries: u64,
    pub blocks: u64,
    pub boots: u64,
    pub carve: u64,
    pub bad_bytes: u64,
    pub opaque: [u64; 4],
}

/// The content of a `CHECKPOINT` block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Checkpoint {
    pub sequence: u64,
    pub meta: SessionMeta,
    pub totals: Totals,
    pub done: RangeSet,
    pub drops: DropCounters,
    pub zero: ZeroCheck,
    pub warnings: Vec<Warning>,
}

fn put_meta(out: &mut Vec<u8>, meta: &SessionMeta) {
    let id = &meta.identity;
    out.u8(match id.kind {
        SourceKind::Disk => 0,
        SourceKind::Image => 1,
    });
    out.str(&id.path);
    out.u64(id.size);
    out.u32(id.sector_size);
    out.u32(id.physical_sector_size);
    out.opt_str(id.model.as_deref());
    out.opt_str(id.serial.as_deref());
    out.u64(meta.range.start);
    out.u64(meta.range.end);
    out.u8(u8::from(meta.options.collect_carve_hits));
    out.u32(meta.options.max_record_size);
}

fn get_meta(get: &mut Get<'_>) -> Result<SessionMeta, Malformed> {
    let kind = match get.u8()? {
        0 => SourceKind::Disk,
        1 => SourceKind::Image,
        _ => return Err(Malformed("source kind")),
    };
    let identity = SourceIdentity {
        kind,
        path: get.str()?,
        size: get.u64()?,
        sector_size: get.u32()?,
        physical_sector_size: get.u32()?,
        model: get.opt_str()?,
        serial: get.opt_str()?,
    };
    let range = get.u64()?..get.u64()?;
    let options = ScanOptions {
        collect_carve_hits: get.bool()?,
        max_record_size: get.u32()?,
    };
    SessionMeta::new(identity, range, options).map_err(|_| Malformed("session range or options"))
}

/// Encodes a checkpoint.
pub(crate) fn encode_checkpoint(c: &Checkpoint) -> Vec<u8> {
    let mut out = Vec::new();
    out.u64(c.sequence);
    put_meta(&mut out, &c.meta);
    let t = &c.totals;
    for v in [
        t.records,
        t.entries,
        t.blocks,
        t.boots,
        t.carve,
        t.bad_bytes,
    ] {
        out.u64(v);
    }
    for v in t.opaque {
        out.u64(v);
    }
    out.u64(u64::try_from(c.done.range_count()).unwrap_or(u64::MAX));
    for r in &c.done {
        out.u64(r.start);
        out.u64(r.end);
    }
    let drops = c.drops.to_array();
    out.u32(u32::try_from(drops.len()).unwrap_or(0));
    for v in drops {
        out.u64(v);
    }
    let (state, seen) = match c.zero {
        ZeroCheck::Checking(seen) => (0, seen),
        ZeroCheck::NonZeroSeen => (1, 0),
        ZeroCheck::Raised => (2, 0),
    };
    out.u8(state);
    out.u64(seen);
    let warnings = c.warnings.get(..MAX_WARNINGS).unwrap_or(&c.warnings);
    out.u32(u32::try_from(warnings.len()).unwrap_or(0));
    for w in warnings {
        out.str(&w.code);
        out.str(&w.message);
    }
    out
}

/// Decodes a checkpoint and checks its fields.
pub(crate) fn decode_checkpoint(payload: &[u8]) -> Result<Checkpoint, Malformed> {
    let mut get = Get::new(payload);
    let sequence = get.u64()?;
    let meta = get_meta(&mut get)?;
    let totals = Totals {
        records: get.u64()?,
        entries: get.u64()?,
        blocks: get.u64()?,
        boots: get.u64()?,
        carve: get.u64()?,
        bad_bytes: get.u64()?,
        opaque: [get.u64()?, get.u64()?, get.u64()?, get.u64()?],
    };
    let sector = u64::from(meta.identity.sector_size);
    let ranges = get.count(true, 16)?;
    let mut done = RangeSet::new();
    for _ in 0..ranges {
        let start = get.u64()?;
        let end = get.u64()?;
        if start >= end
            || start < meta.range.start
            || end > meta.range.end
            || !start.is_multiple_of(sector)
            || !end.is_multiple_of(sector)
        {
            return Err(Malformed("done range"));
        }
        done.insert(start..end);
    }
    let n = usize::try_from(get.u32()?).map_err(|_| Malformed("counters"))?;
    if n != DropCounters::COUNT {
        return Err(Malformed("number of counters"));
    }
    let mut values = [0_u64; DropCounters::COUNT];
    for v in &mut values {
        *v = get.u64()?;
    }
    let state = get.u8()?;
    let seen = get.u64()?;
    let zero = match state {
        0 => ZeroCheck::Checking(seen),
        1 if seen == 0 => ZeroCheck::NonZeroSeen,
        2 if seen == 0 => ZeroCheck::Raised,
        _ => return Err(Malformed("zero-check state")),
    };
    let count = usize::try_from(get.u32()?).map_err(|_| Malformed("warnings"))?;
    if count > MAX_WARNINGS {
        return Err(Malformed("too many warnings"));
    }
    let mut warnings: Vec<Warning> = Vec::with_capacity(count);
    for _ in 0..count {
        let code = get.str()?;
        let message = get.str()?;
        if warnings.iter().any(|w| w.code == code) {
            return Err(Malformed("duplicate warning"));
        }
        warnings.push(Warning { code, message });
    }
    get.finish()?;
    Ok(Checkpoint {
        sequence,
        meta,
        totals,
        done,
        drops: DropCounters::from_array(values),
        zero,
        warnings,
    })
}
