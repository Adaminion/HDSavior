//! Framing of session files (ALGORITHMS B2): the header, the blocks, and
//! loading a file back to the state of its last valid checkpoint.
//!
//! ```text
//! file   = "HDSC" | u32 format version | block*
//! block  = u32 type | u32 reserved (0) | u64 payload length | u32 CRC-32 of payload | payload
//! ```
//!
//! The first block of every file is a `CHECKPOINT` (it fixes the identity,
//! range and options before any data arrives). Data blocks carry what was
//! added since the previous checkpoint; a checkpoint carries the done set,
//! counters, options, identity, range, warnings, and the totals of
//! everything committed so far, which the loader compares with what it has
//! accumulated.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use hds_io::out::OutFile;

use super::codec::{self, Checkpoint, Totals, OPAQUE_EMPTY, OPAQUE_RESET};
use super::crc::crc32;
use super::{OpaqueKind, SessionError, SessionMeta};
use crate::store::{Mark, ScanStore};

/// The first four bytes of a session file.
pub const MAGIC: [u8; 4] = *b"HDSC";
/// The format version this code writes and reads.
pub const FORMAT_VERSION: u32 = 1;
/// Size of the file header.
pub(crate) const FILE_HEADER: u64 = 8;
/// Size of a block header.
pub(crate) const BLOCK_HEADER: u64 = 20;
/// Largest payload written or accepted (1 GiB).
pub(crate) const MAX_PAYLOAD: u64 = 1 << 30;
/// Data blocks are cut at about this size when written (64 MiB), so that
/// loading never holds more than one such block in memory besides the store.
pub(crate) const BLOCK_TARGET: usize = 64 << 20;

/// Block types (ALGORITHMS B2), with their numbers in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BlockType {
    Records = 1,
    IndexEntries = 2,
    IndexBlocks = 3,
    BootHits = 4,
    CarveHits = 5,
    BadRanges = 6,
    Live = 7,
    Heads = 8,
    Overrides = 9,
    Carved = 10,
    Checkpoint = 11,
}

impl BlockType {
    fn from_u32(v: u32) -> Option<BlockType> {
        Some(match v {
            1 => BlockType::Records,
            2 => BlockType::IndexEntries,
            3 => BlockType::IndexBlocks,
            4 => BlockType::BootHits,
            5 => BlockType::CarveHits,
            6 => BlockType::BadRanges,
            7 => BlockType::Live,
            8 => BlockType::Heads,
            9 => BlockType::Overrides,
            10 => BlockType::Carved,
            11 => BlockType::Checkpoint,
            _ => return None,
        })
    }

    pub(crate) fn of_opaque(kind: OpaqueKind) -> BlockType {
        match kind {
            OpaqueKind::Live => BlockType::Live,
            OpaqueKind::Heads => BlockType::Heads,
            OpaqueKind::Overrides => BlockType::Overrides,
            OpaqueKind::Carved => BlockType::Carved,
        }
    }

    fn opaque_index(self) -> Option<usize> {
        match self {
            BlockType::Live => Some(0),
            BlockType::Heads => Some(1),
            BlockType::Overrides => Some(2),
            BlockType::Carved => Some(3),
            _ => None,
        }
    }
}

/// Where the last checkpoint of a file is, to recognise the file again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Origin {
    /// End of the last valid checkpoint: the file's state ends here.
    pub valid_end: u64,
    /// Offset of that checkpoint's block header.
    pub block_offset: u64,
    /// Its payload length and CRC.
    pub block_len: u64,
    pub block_crc: u32,
    /// Its sequence number.
    pub sequence: u64,
}

/// The last block written: offset of its header, payload length, CRC.
pub(crate) type LastBlock = (u64, u64, u32);

/// Writes blocks to a file and keeps track of the position.
pub(crate) struct BlockSink<W: Write> {
    out: BufWriter<W>,
    pos: u64,
    last: Option<LastBlock>,
}

impl<W: Write> BlockSink<W> {
    pub(crate) fn new(out: W, pos: u64) -> Self {
        BlockSink {
            out: BufWriter::with_capacity(1 << 20, out),
            pos,
            last: None,
        }
    }

    /// Writes raw bytes (the file header).
    pub(crate) fn raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.pos = self
            .pos
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        Ok(())
    }

    /// Writes one block.
    pub(crate) fn emit(&mut self, ty: BlockType, payload: &[u8]) -> io::Result<()> {
        let len = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        if len > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a session block would exceed 1 GiB",
            ));
        }
        let crc = crc32(payload);
        let mut header = [0_u8; 20];
        let fields = (ty as u32)
            .to_le_bytes()
            .into_iter()
            .chain(0_u32.to_le_bytes())
            .chain(len.to_le_bytes())
            .chain(crc.to_le_bytes());
        for (slot, byte) in header.iter_mut().zip(fields) {
            *slot = byte;
        }
        let at = self.pos;
        self.out.write_all(&header)?;
        self.out.write_all(payload)?;
        self.pos = self.pos.saturating_add(BLOCK_HEADER).saturating_add(len);
        self.last = Some((at, len, crc));
        Ok(())
    }

    /// Flushes and returns the end position and the last block written.
    pub(crate) fn finish(mut self) -> io::Result<(u64, Option<LastBlock>)> {
        self.out.flush()?;
        Ok((self.pos, self.last))
    }
}

/// The file header.
pub(crate) fn header_bytes() -> [u8; 8] {
    let mut h = [0_u8; 8];
    for (slot, byte) in h
        .iter_mut()
        .zip(MAGIC.into_iter().chain(FORMAT_VERSION.to_le_bytes()))
    {
        *slot = byte;
    }
    h
}

fn corrupt(message: impl Into<String>) -> SessionError {
    SessionError::Corrupt(message.into())
}

/// Checks the file header.
fn check_header(header: &[u8; 8]) -> Result<(), SessionError> {
    let (magic, version) = header.split_at(4);
    if magic != MAGIC {
        return Err(corrupt("not an HDSavior session file"));
    }
    let version = u32::from_le_bytes([
        *version.first().unwrap_or(&0),
        *version.get(1).unwrap_or(&0),
        *version.get(2).unwrap_or(&0),
        *version.get(3).unwrap_or(&0),
    ]);
    if version > FORMAT_VERSION {
        return Err(SessionError::Unsupported(version));
    }
    if version != FORMAT_VERSION {
        return Err(corrupt(format!("unknown session format version {version}")));
    }
    Ok(())
}

/// One block header.
struct RawHeader {
    ty: u32,
    reserved: u32,
    len: u64,
    crc: u32,
}

fn parse_block_header(b: &[u8; 20]) -> RawHeader {
    let u32_at = |at: usize| {
        b.get(at..)
            .and_then(|r| r.first_chunk::<4>())
            .map_or(0, |c| u32::from_le_bytes(*c))
    };
    let len = b
        .get(8..)
        .and_then(|r| r.first_chunk::<8>())
        .map_or(0, |c| u64::from_le_bytes(*c));
    RawHeader {
        ty: u32_at(0),
        reserved: u32_at(4),
        len,
        crc: u32_at(16),
    }
}

/// Reads exactly `buf.len()` bytes; `Ok(false)` at a clean or torn end.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    match r.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

/// A block read from a file: type, payload, CRC.
type ReadBlock = (BlockType, Vec<u8>, u32);

/// Reads the next block: `Ok(Err(reason))` when the file ends or the block
/// is not valid (the state ends before it).
fn next_block(
    r: &mut impl Read,
    pos: u64,
    file_len: u64,
) -> io::Result<Result<ReadBlock, &'static str>> {
    let mut header = [0_u8; 20];
    if file_len.saturating_sub(pos) < BLOCK_HEADER || !read_full(r, &mut header)? {
        return Ok(Err("the file ends inside a block header"));
    }
    let h = parse_block_header(&header);
    if h.reserved != 0 {
        return Ok(Err("a block header is damaged"));
    }
    let Some(ty) = BlockType::from_u32(h.ty) else {
        return Ok(Err("a block has an unknown type"));
    };
    let available = file_len.saturating_sub(pos).saturating_sub(BLOCK_HEADER);
    if h.len > available {
        return Ok(Err("the file ends inside a block"));
    }
    if h.len > MAX_PAYLOAD {
        return Ok(Err("a block is larger than 1 GiB"));
    }
    // The length has been compared with the bytes that are really there.
    let Ok(len) = usize::try_from(h.len) else {
        return Ok(Err("a block is too large for this system"));
    };
    let mut payload = vec![0_u8; len];
    if !read_full(r, &mut payload)? {
        return Ok(Err("the file ends inside a block"));
    }
    if crc32(&payload) != h.crc {
        return Ok(Err("a block fails its checksum"));
    }
    Ok(Ok((ty, payload, h.crc)))
}

/// What a session file holds, as of its last valid checkpoint.
pub(crate) struct Loaded {
    pub meta: SessionMeta,
    pub store: ScanStore,
    pub opaque: [Vec<Vec<u8>>; 4],
    pub origin: Origin,
    pub checkpoints: u64,
    pub file_len: u64,
    /// Why bytes behind the last valid checkpoint were ignored, if any are.
    pub stop: Option<&'static str>,
}

/// One change of an opaque list, applied when the next checkpoint is valid.
enum OpaqueOp {
    Reset,
    Push(Vec<u8>),
}

fn apply_ops(list: &mut Vec<Vec<u8>>, ops: &mut Vec<OpaqueOp>) {
    for op in ops.drain(..) {
        match op {
            OpaqueOp::Reset => list.clear(),
            OpaqueOp::Push(p) => list.push(p),
        }
    }
}

fn list_len_after(list: &[Vec<u8>], ops: &[OpaqueOp]) -> u64 {
    let mut len = list.len();
    for op in ops {
        match op {
            OpaqueOp::Reset => len = 0,
            OpaqueOp::Push(_) => len = len.saturating_add(1),
        }
    }
    u64::try_from(len).unwrap_or(u64::MAX)
}

fn store_totals(store: &ScanStore) -> [u64; 6] {
    let len = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    [
        len(store.records.len()),
        len(store.entries.len()),
        len(store.blocks.len()),
        len(store.boots.len()),
        len(store.carve.len()),
        store.bad.total_len(),
    ]
}

/// Loads a session file (ALGORITHMS B2): the state of its last valid
/// checkpoint. Refused when the file is not a session file, has a newer
/// format, has no valid checkpoint, or contradicts itself.
pub(crate) fn load(path: &Path) -> Result<Loaded, SessionError> {
    let meta_of_path = std::fs::metadata(path).map_err(SessionError::Io)?;
    if !meta_of_path.is_file() {
        return Err(corrupt("not a regular file"));
    }
    let file = File::open(path).map_err(SessionError::Io)?;
    let file_len = file.metadata().map_err(SessionError::Io)?.len();
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut header = [0_u8; 8];
    if !read_full(&mut r, &mut header).map_err(SessionError::Io)? {
        return Err(corrupt("not an HDSavior session file"));
    }
    check_header(&header)?;

    let mut pos = FILE_HEADER;
    let mut store = ScanStore::new();
    let mut meta: Option<SessionMeta> = None;
    let mut committed: Option<(Checkpoint, Mark, Origin)> = None;
    let mut checkpoints = 0_u64;
    let mut lists: [Vec<Vec<u8>>; 4] = Default::default();
    let mut ops: [Vec<OpaqueOp>; 4] = Default::default();
    let mut stop = None;

    while pos < file_len {
        let (ty, payload, crc) =
            match next_block(&mut r, pos, file_len).map_err(SessionError::Io)? {
                Ok(block) => block,
                Err(reason) => {
                    stop = Some(reason);
                    break;
                }
            };
        let block_len = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        let block_at = pos;
        pos = pos.saturating_add(BLOCK_HEADER).saturating_add(block_len);
        let outcome: Result<(), &'static str> = match (ty, meta.as_ref()) {
            (BlockType::Checkpoint, _) => match codec::decode_checkpoint(&payload) {
                Err(e) => Err(e.0),
                Ok(c) => {
                    let first = meta.is_none();
                    let same_session = meta
                        .as_ref()
                        .is_none_or(|m| m.range == c.meta.range && m.options == c.meta.options);
                    let previous = committed.as_ref().map_or(0, |(p, _, _)| p.sequence);
                    let [records, entries, blocks, boots, carve, bad] = store_totals(&store);
                    let t = c.totals;
                    let opaque_ok = (0..4).all(|i| {
                        let list = lists.get(i).map_or(&[][..], Vec::as_slice);
                        let pending = ops.get(i).map_or(&[][..], Vec::as_slice);
                        t.opaque.get(i).copied() == Some(list_len_after(list, pending))
                    });
                    if !same_session {
                        Err("a checkpoint belongs to another session")
                    } else if !first && c.sequence <= previous {
                        Err("checkpoints out of order")
                    } else if [
                        t.records,
                        t.entries,
                        t.blocks,
                        t.boots,
                        t.carve,
                        t.bad_bytes,
                    ] != [records, entries, blocks, boots, carve, bad]
                        || !opaque_ok
                    {
                        Err("a checkpoint does not match the blocks before it")
                    } else {
                        for (list, pending) in lists.iter_mut().zip(ops.iter_mut()) {
                            apply_ops(list, pending);
                        }
                        if first {
                            meta = Some(c.meta.clone());
                        } else if let Some(m) = meta.as_mut() {
                            // The identity can change (a source attached at
                            // another path); range and options cannot.
                            m.identity = c.meta.identity.clone();
                        }
                        let origin = Origin {
                            valid_end: pos,
                            block_offset: block_at,
                            block_len,
                            block_crc: crc,
                            sequence: c.sequence,
                        };
                        checkpoints = checkpoints.saturating_add(1);
                        committed = Some((c, store.mark(), origin));
                        Ok(())
                    }
                }
            },
            (_, None) => Err("data before the first checkpoint"),
            (BlockType::Records, Some(m)) => {
                codec::decode_records(&payload, m, &mut store).map_err(|e| e.0)
            }
            (BlockType::IndexEntries, Some(m)) => {
                codec::decode_entries(&payload, m, &mut store).map_err(|e| e.0)
            }
            (BlockType::IndexBlocks, Some(m)) => {
                codec::decode_blocks(&payload, m, &mut store).map_err(|e| e.0)
            }
            (BlockType::BootHits, Some(m)) => {
                codec::decode_boots(&payload, m, &mut store).map_err(|e| e.0)
            }
            (BlockType::CarveHits, Some(m)) => {
                codec::decode_carve(&payload, m, &mut store).map_err(|e| e.0)
            }
            (BlockType::BadRanges, Some(m)) => {
                codec::decode_bad(&payload, m, &mut store).map_err(|e| e.0)
            }
            (opaque, Some(_)) => match codec::decode_opaque(&payload) {
                Err(e) => Err(e.0),
                Ok((flags, bytes)) => {
                    let index = opaque.opaque_index().unwrap_or(0);
                    if let Some(pending) = ops.get_mut(index) {
                        if flags & OPAQUE_RESET != 0 {
                            pending.push(OpaqueOp::Reset);
                        }
                        if flags & OPAQUE_EMPTY == 0 {
                            pending.push(OpaqueOp::Push(bytes.to_vec()));
                        }
                    }
                    Ok(())
                }
            },
        };
        if let Err(reason) = outcome {
            stop = Some(reason);
            break;
        }
    }

    let Some((checkpoint, mark, origin)) = committed else {
        return Err(corrupt(format!(
            "no valid checkpoint ({})",
            stop.unwrap_or("the file holds no block")
        )));
    };
    let Some(mut meta) = meta else {
        return Err(corrupt("no valid checkpoint"));
    };
    meta.identity = checkpoint.meta.identity.clone();
    store.rollback(mark);
    store.done = checkpoint.done;
    store.drops = checkpoint.drops;
    store.zero = checkpoint.zero;
    store.warnings = checkpoint.warnings;
    store.invalidate();
    validate_store(&store)?;
    Ok(Loaded {
        meta,
        store,
        opaque: lists,
        origin,
        checkpoints,
        file_len,
        stop: if origin.valid_end < file_len {
            stop.or(Some(
                "the blocks after the last checkpoint are not complete",
            ))
        } else {
            None
        },
    })
}

/// Checks what a crash cannot produce: duplicates, findings outside the
/// done set, index entries without their source.
fn validate_store(store: &ScanStore) -> Result<(), SessionError> {
    let done = store.done();
    let mut previous: Option<u64> = None;
    for &id in store.records_by_offset() {
        let Some(r) = store.record(id) else { continue };
        // A structure that crosses into a chunk not yet scanned is fine:
        // it was read on its own. Its start lies in the done set.
        if previous == Some(r.disk_offset()) || !done.contains(r.disk_offset()) {
            return Err(corrupt(
                "two records at one offset, or a record outside the scanned part",
            ));
        }
        previous = Some(r.disk_offset());
    }
    let mut previous: Option<u64> = None;
    for &i in store.index_blocks_by_offset() {
        let Some(b) = usize::try_from(i).ok().and_then(|i| store.blocks.get(i)) else {
            continue;
        };
        if previous == Some(b.disk_offset) || !done.contains(b.disk_offset) {
            return Err(corrupt(
                "two index blocks at one offset, or one outside the scanned part",
            ));
        }
        previous = Some(b.disk_offset);
    }
    for (_, entry) in store.index_entries() {
        let source_ok = match entry.source.kind {
            crate::store::EntrySourceKind::Block => {
                store.index_block_at(entry.source.offset).is_some()
            }
            crate::store::EntrySourceKind::RecordRoot => {
                store.record_at(entry.source.offset).is_some()
            }
        };
        if !source_ok {
            return Err(corrupt("an index entry has no source"));
        }
    }
    let mut boots: Vec<u64> = store.boots.iter().map(|b| b.disk_offset).collect();
    boots.sort_unstable();
    let carve = store.carve_hits_sorted();
    if boots.windows(2).any(|w| w.first() == w.get(1))
        || carve.windows(2).any(|w| w.first() == w.get(1))
        || !boots.iter().all(|&o| done.contains(o))
        || !carve.iter().all(|h| done.contains(h.disk_offset()))
        || !store.bad.iter().all(|r| done.contains_range(r))
    {
        return Err(corrupt("hits or bad ranges lie outside the scanned part"));
    }
    Ok(())
}

/// Reads the identity, range and options of the session file at `file`
/// (from its first block) without loading it.
pub(crate) fn peek_meta(file: &mut (impl Read + Seek)) -> Result<SessionMeta, SessionError> {
    let file_len = file.seek(SeekFrom::End(0)).map_err(SessionError::Io)?;
    file.seek(SeekFrom::Start(0)).map_err(SessionError::Io)?;
    let mut header = [0_u8; 8];
    if !read_full(file, &mut header).map_err(SessionError::Io)? {
        return Err(corrupt("not an HDSavior session file"));
    }
    check_header(&header)?;
    match next_block(file, FILE_HEADER, file_len).map_err(SessionError::Io)? {
        Ok((BlockType::Checkpoint, payload, _)) => codec::decode_checkpoint(&payload)
            .map(|c| c.meta)
            .map_err(|e| corrupt(e.0)),
        Ok(_) => Err(corrupt("the file does not start with a checkpoint")),
        Err(reason) => Err(corrupt(reason)),
    }
}

/// Whether `file` still ends its valid part with the checkpoint described
/// by `origin`.
pub(crate) fn matches_origin(file: &mut OutFile, origin: &Origin) -> Result<bool, SessionError> {
    let len = file.len().map_err(SessionError::Io)?;
    if len < origin.valid_end {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(0)).map_err(SessionError::Io)?;
    let mut header = [0_u8; 8];
    if !read_full(file, &mut header).map_err(SessionError::Io)? || check_header(&header).is_err() {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(origin.block_offset))
        .map_err(SessionError::Io)?;
    match next_block(file, origin.block_offset, origin.valid_end).map_err(SessionError::Io)? {
        Ok((BlockType::Checkpoint, payload, crc)) => Ok(crc == origin.block_crc
            && u64::try_from(payload.len()).ok() == Some(origin.block_len)
            && codec::decode_checkpoint(&payload).is_ok_and(|c| c.sequence == origin.sequence)),
        _ => Ok(false),
    }
}

/// The totals of a store and opaque lists, for a checkpoint.
pub(crate) fn totals_of(store: &ScanStore, opaque: [u64; 4]) -> Totals {
    let [records, entries, blocks, boots, carve, bad_bytes] = store_totals(store);
    Totals {
        records,
        entries,
        blocks,
        boots,
        carve,
        bad_bytes,
        opaque,
    }
}
