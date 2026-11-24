//! What the scan does at every 512-byte offset of a chunk (ALGORITHMS A3)
//! and how a record becomes a [`RawRecord`] (A4) and index entries (A5).

use std::sync::atomic::AtomicBool;

use hds_carve::sniff;
use hds_io::{read_unaligned, AlignedBuf, RangedSource, ReadOutcome};
use hds_ntfs::{
    attr_type, fnv1a64, is_boot_sector_candidate, parse_boot_sector, parse_file_record,
    parse_index_block, probe_file_record, probe_index_block, AttrBody, AttributeList, FileName,
    FileRecord, FixupStatus, Utf16Name, FILE_MAGIC, I30, INDX_MAGIC, MAX_INDEX_BLOCK_SIZE,
    MAX_RECORD_SIZE, SECTOR_SIZE, WOF_COMPRESSED_DATA,
};

use super::Abort;
use crate::store::index::{entry_bits, IndexEntryRec};
use crate::store::record::{bits, AuxBuilder, ExtentTarget, RawRecord};
use crate::store::{BootHit, CarveHit, DropCounters, IndexBlockInfo, ScanStore};

/// What one chunk contributed besides the stored items.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ChunkOutcome {
    pub drops: DropCounters,
    /// The cap on file-signature hits was reached in this chunk.
    pub carve_capped: bool,
}

/// A directory entry waiting for its record to be stored.
struct PendingEntry {
    rec: IndexEntryRec,
    name: std::ops::Range<usize>,
}

/// Parses chunks into a store. Holds the scratch buffers, which are reused
/// for every structure.
pub(crate) struct ChunkParser<'a> {
    source: &'a RangedSource,
    cancel: &'a AtomicBool,
    max_record_size: u32,
    carve: bool,
    carve_cap: usize,
    /// Structures that cross the chunk end are read into this buffer.
    read_buf: AlignedBuf,
    /// Structures are fixed up and parsed here, so that the chunk stays as
    /// read (fixups change the last two bytes of every 512-byte block).
    parse_buf: Vec<u8>,
    aux: AuxBuilder,
    entries: Vec<PendingEntry>,
    names: Vec<u8>,
}

impl<'a> ChunkParser<'a> {
    pub(crate) fn new(
        source: &'a RangedSource,
        cancel: &'a AtomicBool,
        max_record_size: u32,
        carve: bool,
        carve_cap: usize,
    ) -> Self {
        let largest = MAX_RECORD_SIZE.max(MAX_INDEX_BLOCK_SIZE);
        let largest = usize::try_from(largest).unwrap_or(65_536);
        ChunkParser {
            source,
            cancel,
            max_record_size,
            carve,
            carve_cap,
            read_buf: AlignedBuf::new(largest),
            parse_buf: vec![0; largest],
            aux: AuxBuilder::default(),
            entries: Vec::new(),
            names: Vec::new(),
        }
    }

    /// Looks at every 512-byte offset of `data`, which was read at the
    /// absolute offset `start`, and appends what it finds to `store`.
    /// On an error the caller rolls the store back.
    pub(crate) fn parse(
        &mut self,
        store: &mut ScanStore,
        start: u64,
        data: &[u8],
    ) -> Result<ChunkOutcome, Abort> {
        let mut outcome = ChunkOutcome::default();
        for (index, sector) in data.chunks_exact(SECTOR_SIZE).enumerate() {
            let rel = index.saturating_mul(SECTOR_SIZE);
            let offset = start.saturating_add(u64::try_from(rel).unwrap_or(u64::MAX));
            match sector.first_chunk::<4>() {
                Some(magic) if *magic == FILE_MAGIC => {
                    self.on_file(store, offset, data, rel, &mut outcome.drops)?;
                }
                Some(magic) if *magic == INDX_MAGIC => {
                    self.on_indx(store, offset, data, rel, &mut outcome.drops)?;
                }
                _ => {}
            }
            if is_boot_sector_candidate(sector) {
                match parse_boot_sector(sector) {
                    Ok(boot) => {
                        if !store.push_boot(BootHit {
                            disk_offset: offset,
                            boot,
                        }) {
                            bump(&mut outcome.drops.capacity_dropped);
                        }
                    }
                    Err(_) => bump(&mut outcome.drops.boot_refused),
                }
            }
            if self.carve {
                if let Some(kind) = sniff(sector) {
                    if store.carve.len() < self.carve_cap {
                        match CarveHit::new(offset, kind) {
                            Some(hit) => store.push_carve(hit),
                            None => bump(&mut outcome.drops.capacity_dropped),
                        }
                    } else {
                        outcome.carve_capped = true;
                        bump(&mut outcome.drops.carve_dropped);
                    }
                }
            }
        }
        Ok(outcome)
    }

    /// The bytes of a structure of `size` bytes at `rel` in the chunk: from
    /// the chunk, or read on their own when it crosses the chunk end.
    /// `Ok(None)` when it reaches beyond the session range.
    fn structure<'b>(
        source: &RangedSource,
        cancel: &AtomicBool,
        read_buf: &'b mut AlignedBuf,
        data: &'b [u8],
        offset: u64,
        rel: usize,
        size: usize,
    ) -> Result<Option<&'b [u8]>, Abort> {
        if let Some(bytes) = rel.checked_add(size).and_then(|end| data.get(rel..end)) {
            return Ok(Some(bytes));
        }
        let fits = u64::try_from(size)
            .ok()
            .and_then(|size| offset.checked_add(size))
            .is_some_and(|end| end <= source.end());
        if !fits {
            return Ok(None);
        }
        let Some(buf) = read_buf.get_mut(..size) else {
            return Ok(None);
        };
        match read_unaligned(source, offset, buf, cancel) {
            ReadOutcome::Data { .. } => Ok(read_buf.get(..size)),
            ReadOutcome::SourceLost(e) => Err(Abort::SourceLost(e)),
            ReadOutcome::Cancelled => Err(Abort::Cancelled),
            ReadOutcome::Internal(e) => Err(Abort::Internal(e)),
        }
    }

    fn on_file(
        &mut self,
        store: &mut ScanStore,
        offset: u64,
        data: &[u8],
        rel: usize,
        drops: &mut DropCounters,
    ) -> Result<(), Abort> {
        let Some(sector) = data.get(rel..) else {
            return Ok(());
        };
        let Some(probe) = probe_file_record(sector, self.max_record_size) else {
            bump(&mut drops.file_refused);
            return Ok(());
        };
        bump(&mut drops.file_hits);
        let size = usize::try_from(probe.record_size).unwrap_or(usize::MAX);
        let Some(raw) = Self::structure(
            self.source,
            self.cancel,
            &mut self.read_buf,
            data,
            offset,
            rel,
            size,
        )?
        else {
            bump(&mut drops.truncated);
            return Ok(());
        };
        // The hash is of the bytes as read, before fixups.
        let hash = fnv1a64(raw);
        let Some(buf) = self.parse_buf.get_mut(..size) else {
            bump(&mut drops.file_refused);
            return Ok(());
        };
        buf.copy_from_slice(raw);
        let Ok(record) = parse_file_record(buf) else {
            bump(&mut drops.file_refused);
            return Ok(());
        };
        let blocks = size / SECTOR_SIZE;
        if let FixupStatus::Torn { bad_blocks, .. } = record.fixup() {
            if usize::from(bad_blocks) >= blocks {
                // Every block failed: a $LogFile copy of the record, which
                // must never compete with the real one (A3.1).
                bump(&mut drops.log_copies);
                return Ok(());
            }
        }
        if record.is_never_used() {
            bump(&mut drops.never_used);
            return Ok(());
        }
        let stored = describe_record(
            &record,
            offset,
            hash,
            &mut self.aux,
            &mut self.entries,
            &mut self.names,
        );
        if !store.push_record(stored, &self.aux) {
            bump(&mut drops.capacity_dropped);
            return Ok(());
        }
        for pending in &self.entries {
            let name = self.names.get(pending.name.clone()).unwrap_or(&[]);
            if !store.push_entry(pending.rec, name) {
                bump(&mut drops.capacity_dropped);
            }
        }
        Ok(())
    }

    fn on_indx(
        &mut self,
        store: &mut ScanStore,
        offset: u64,
        data: &[u8],
        rel: usize,
        drops: &mut DropCounters,
    ) -> Result<(), Abort> {
        let Some(sector) = data.get(rel..) else {
            return Ok(());
        };
        let Some(probe) = probe_index_block(sector) else {
            bump(&mut drops.indx_refused);
            return Ok(());
        };
        let size = usize::try_from(probe.block_size).unwrap_or(usize::MAX);
        let Some(raw) = Self::structure(
            self.source,
            self.cancel,
            &mut self.read_buf,
            data,
            offset,
            rel,
            size,
        )?
        else {
            bump(&mut drops.truncated);
            return Ok(());
        };
        let Some(buf) = self.parse_buf.get_mut(..size) else {
            bump(&mut drops.indx_refused);
            return Ok(());
        };
        buf.copy_from_slice(raw);
        let Ok(block) = parse_index_block(buf) else {
            bump(&mut drops.indx_refused);
            return Ok(());
        };
        let info = IndexBlockInfo {
            disk_offset: offset,
            vcn: block.vcn(),
            size: block.size(),
            lsn: block.lsn(),
            owner: block.owner(),
            fixup: block.fixup(),
        };
        if !store.push_block(info) {
            bump(&mut drops.capacity_dropped);
            return Ok(());
        }
        let source_bits = if block.fixup().is_torn() {
            entry_bits::SOURCE_TORN
        } else {
            0
        };
        for entry in block.entries() {
            if !entry.file_name.is_directory() {
                continue;
            }
            self.names.clear();
            let rec = entry_rec(
                entry.child.raw(),
                &entry.file_name,
                offset,
                block.lsn(),
                source_bits,
                &mut self.names,
            );
            if !store.push_entry(rec, &self.names) {
                bump(&mut drops.capacity_dropped);
            }
        }
        Ok(())
    }
}

fn bump(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

/// Appends the units of `name` to `out` (one byte per unit when all are
/// below 0x100) and returns whether the narrow form was used.
fn encode_name(name: &Utf16Name<'_>, out: &mut Vec<u8>) -> bool {
    let narrow = name.units().all(|unit| unit < 0x100);
    if narrow {
        out.extend(name.units().map(|unit| unit.to_le_bytes()[0]));
    } else {
        out.extend_from_slice(name.as_bytes());
    }
    narrow
}

/// An index entry record for `file_name`; its name is appended to `names`
/// and `name_off` is left at 0 (the store sets it).
fn entry_rec(
    child: u64,
    file_name: &FileName<'_>,
    source_offset: u64,
    source_lsn: u64,
    source_bits: u8,
    names: &mut Vec<u8>,
) -> IndexEntryRec {
    let narrow = encode_name(&file_name.name, names);
    IndexEntryRec {
        child,
        parent: file_name.parent.raw(),
        mft_changed: file_name.times.mft_changed,
        source_offset,
        source_lsn,
        name_off: 0,
        // FileName guarantees 1 to 255 units.
        name_len: u8::try_from(file_name.name.len()).unwrap_or(u8::MAX),
        namespace: file_name.namespace.as_u8(),
        bits: source_bits | if narrow { entry_bits::NARROW } else { 0 },
    }
}

/// Walks the attributes of a record once and describes it: the fixed
/// fields (returned), the aux items (in `aux`) and the directory entries of
/// its `$INDEX_ROOT:$I30` (in `entries`, names in `names`).
fn describe_record(
    record: &FileRecord<'_>,
    offset: u64,
    hash: u64,
    aux: &mut AuxBuilder,
    entries: &mut Vec<PendingEntry>,
    names: &mut Vec<u8>,
) -> RawRecord {
    aux.clear();
    entries.clear();
    names.clear();
    let header = record.header();
    let mut flags = 0_u16;
    let mut si_times = [0_u64; 4];
    let mut si_flags = 0_u32;
    let mut reparse_tag = 0_u32;
    if header.record_number.is_some() {
        flags |= bits::HAS_NUMBER;
    }
    if record.attribute_count() > 0 {
        flags |= bits::HAS_ATTRIBUTES;
    }
    if !record.attr_chain().is_complete() {
        flags |= bits::CHAIN_BROKEN;
    }
    let source_bits = if record.fixup().is_torn() {
        entry_bits::FROM_ROOT | entry_bits::SOURCE_TORN
    } else {
        entry_bits::FROM_ROOT
    };

    for attr in record.attributes() {
        match attr.type_code {
            attr_type::STANDARD_INFORMATION => {
                if flags & bits::HAS_SI != 0 {
                    continue;
                }
                match attr.standard_information() {
                    Some(si) => {
                        flags |= bits::HAS_SI;
                        si_times = [
                            si.times.created,
                            si.times.modified,
                            si.times.mft_changed,
                            si.times.accessed,
                        ];
                        si_flags = si.flags;
                    }
                    None => flags |= bits::SI_MALFORMED,
                }
            }
            attr_type::ATTRIBUTE_LIST => {
                if flags & bits::ATTR_LIST != 0 {
                    continue;
                }
                flags |= bits::ATTR_LIST;
                match attr.attribute_list() {
                    Some(AttributeList::Resident(mut list)) => {
                        flags |= bits::ATTR_LIST_RESIDENT;
                        for entry in &mut list {
                            aux.push_attr_list_entry(&entry);
                        }
                        if !list.end().is_some_and(|end| end.is_complete()) {
                            flags |= bits::ATTR_LIST_INCOMPLETE;
                        }
                    }
                    Some(AttributeList::NonResident(_)) => {}
                    None => flags |= bits::ATTR_LIST_MALFORMED,
                }
            }
            attr_type::FILE_NAME => match attr.file_name() {
                Some(name) => aux.push_name(&name, attr.id),
                None => flags |= bits::NAME_MALFORMED,
            },
            attr_type::DATA => {
                if attr.is_unnamed() {
                    match attr.body {
                        AttrBody::Resident(value) => {
                            aux.push_resident_data(attr.id, attr.flags, value.offset, value.len());
                        }
                        AttrBody::NonResident(extent) => {
                            aux.push_extent(ExtentTarget::Data, attr.id, attr.flags, &extent);
                        }
                        AttrBody::Malformed => flags |= bits::DATA_MALFORMED,
                    }
                } else if attr.is_named(WOF_COMPRESSED_DATA) {
                    flags |= bits::WOF_STREAM;
                }
            }
            attr_type::INDEX_ROOT if attr.is_named(I30) => {
                if let Some(root) = attr.index_root() {
                    for entry in root.entries() {
                        if !entry.file_name.is_directory() {
                            continue;
                        }
                        let from = names.len();
                        let rec = entry_rec(
                            entry.child.raw(),
                            &entry.file_name,
                            offset,
                            header.lsn,
                            source_bits,
                            names,
                        );
                        entries.push(PendingEntry {
                            rec,
                            name: from..names.len(),
                        });
                    }
                }
            }
            attr_type::INDEX_ALLOCATION if attr.is_named(I30) => match attr.body {
                AttrBody::NonResident(extent) => {
                    aux.push_extent(ExtentTarget::IndexAllocation, attr.id, attr.flags, &extent);
                }
                AttrBody::Resident(_) | AttrBody::Malformed => flags |= bits::IA_MALFORMED,
            },
            attr_type::REPARSE_POINT => {
                flags |= bits::REPARSE;
                if let Some(tag) = attr.reparse_tag() {
                    if flags & bits::REPARSE_TAG == 0 {
                        flags |= bits::REPARSE_TAG;
                        reparse_tag = tag;
                    }
                }
            }
            _ => {}
        }
    }

    let (fixup_first_bad, fixup_bad) = match record.fixup() {
        FixupStatus::Ok => (0, 0),
        FixupStatus::Torn {
            first_bad_block,
            bad_blocks,
        } => (first_bad_block, bad_blocks),
    };
    RawRecord {
        disk_offset: offset,
        lsn: header.lsn,
        hash,
        base: header.base.raw(),
        si_times,
        aux_off: 0,
        si_flags,
        number: header.record_number.unwrap_or(0),
        reparse_tag,
        aux_len: 0,
        sequence: header.sequence,
        header_flags: header.flags,
        link_count: header.link_count,
        bits: flags,
        fixup_first_bad,
        fixup_bad,
        // A power of two from 1024 to 65536 (checked by the probe).
        size_log2: u8::try_from(record.allocated_size().trailing_zeros()).unwrap_or(0),
    }
}
