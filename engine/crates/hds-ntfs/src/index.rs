//! Directory index structures (docs/notes/ntfs-on-disk.md, section 9):
//! index entries, the `$INDEX_ROOT` value and `INDX` blocks.

use core::iter::FusedIterator;

use crate::bytes::{u16_at, u32_at, u32_from, u64_at, u8_at, usize_from};
use crate::error::Error;
use crate::file_name::FileName;
use crate::fixup::{apply_validated, usa_fits, FixupStatus, SECTOR_SIZE};
use crate::reference::FileRef;

/// Signature of an index block.
pub const INDX_MAGIC: [u8; 4] = *b"INDX";

/// Smallest index block.
pub const MIN_INDEX_BLOCK_SIZE: u32 = 512;
/// Largest index block: the boot sector cannot describe a larger one, and
/// the update sequence array of a larger one would not fit in 512 bytes.
pub const MAX_INDEX_BLOCK_SIZE: u32 = 65_536;

/// Index entry flag: the last 8 bytes of the entry hold the VCN of a
/// sub-node.
pub const INDEX_ENTRY_HAS_SUBNODE: u16 = 0x0001;
/// Index entry flag: last entry of the node. It carries no key.
pub const INDEX_ENTRY_LAST: u16 = 0x0002;
/// Node header flag: the node has sub-nodes (it is not a leaf).
pub const INDEX_NODE_HAS_SUBNODES: u8 = 0x01;

/// Offset of the node header inside an `INDX` block.
const BLOCK_NODE_OFFSET: usize = 0x18;
/// Offset of the node header inside an `$INDEX_ROOT` value.
const ROOT_NODE_OFFSET: usize = 0x10;
/// Size of a node header, and of an index entry header.
const HEADER_LEN: usize = 0x10;
/// Number of distinct parent references [`IndexEntries::owner`] tracks.
const OWNER_CANDIDATES: usize = 16;

/// The node header that precedes the entries of an index node. All three
/// sizes count from the start of this header (0x18 in an `INDX` block, 0x10
/// in an `$INDEX_ROOT` value).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeHeader {
    /// Offset of the first entry.
    pub entries_offset: u32,
    /// End of the entries in use.
    pub used_size: u32,
    /// Size allocated to the node.
    pub allocated_size: u32,
    /// Flags; see [`INDEX_NODE_HAS_SUBNODES`].
    pub flags: u8,
}

impl NodeHeader {
    /// Reads the 16-byte header at `node` and returns it together with the
    /// bytes of its entries, `[node + entries_offset, node + used_size)`,
    /// and the position of the first entry in `container`. `None` when the
    /// header contradicts itself or the container.
    fn read(container: &[u8], node: usize) -> Option<(NodeHeader, &[u8], usize)> {
        let header = NodeHeader {
            entries_offset: u32_at(container, node)?,
            used_size: u32_at(container, node.checked_add(4)?)?,
            allocated_size: u32_at(container, node.checked_add(8)?)?,
            flags: u8_at(container, node.checked_add(12)?)?,
        };
        if usize_from(header.entries_offset) < HEADER_LEN
            || header.entries_offset > header.used_size
        {
            return None;
        }
        let first = node.checked_add(usize_from(header.entries_offset))?;
        let end = node.checked_add(usize_from(header.used_size))?;
        Some((header, container.get(first..end)?, first))
    }

    /// Whether the node has sub-nodes.
    #[inline]
    #[must_use]
    pub const fn has_subnodes(&self) -> bool {
        self.flags & INDEX_NODE_HAS_SUBNODES != 0
    }
}

/// One directory entry of an index node: a file reference and a copy of one
/// of that file's `$FILE_NAME` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexEntry<'a> {
    /// Offset of the entry inside its `INDX` block, or inside the record
    /// for an entry of an `$INDEX_ROOT`.
    pub offset: u32,
    /// The file or directory the entry names (its base record).
    pub child: FileRef,
    /// Entry flags; see [`INDEX_ENTRY_HAS_SUBNODE`].
    pub flags: u16,
    /// VCN of the sub-node holding the entries that sort before this one,
    /// when the entry has one.
    pub subnode_vcn: Option<u64>,
    /// The key: parent reference, namespace, name, flags, times and sizes.
    pub file_name: FileName<'a>,
}

/// How a walk over a list of entries ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WalkEnd {
    /// The list ended the way the format says it must: for index entries at
    /// the entry flagged [`INDEX_ENTRY_LAST`], for an attribute list
    /// exactly at the end of the value.
    Complete,
    /// The walk met an entry that cannot be: a length that is too small,
    /// not aligned or reaching past the end, a key that does not fit, or
    /// the end of the data without a proper end. Entries yielded before
    /// that point were sound.
    Broken,
}

impl WalkEnd {
    /// Whether the list ended properly.
    #[inline]
    #[must_use]
    pub const fn is_complete(self) -> bool {
        matches!(self, WalkEnd::Complete)
    }
}

/// The index-entry walker shared by `INDX` blocks and `$INDEX_ROOT` values:
/// an iterator over the entries of one node whose key is a valid
/// `$FILE_NAME`.
///
/// Per entry: file reference (8 bytes), entry length (u16 at 0x08, at least
/// 0x10, a multiple of 8), key length (u16 at 0x0A), flags (u16 at 0x0C),
/// key at 0x10. An entry flagged [`INDEX_ENTRY_HAS_SUBNODE`] is at least
/// 0x18 bytes long and ends with the sub-node VCN. The entry flagged
/// [`INDEX_ENTRY_LAST`] has no key and ends the walk.
///
/// A key validates as a `$FILE_NAME` when it is exactly `0x42 + 2 * n`
/// bytes long for its name length `n >= 1` and has a namespace of 0 to 3.
/// Entries with other keys (the indexes of `$Secure`, `$ObjId`, `$Quota`
/// and `$Reparse` use the same block format) are skipped and counted.
///
/// The walk is bounded by the bytes it was given and advances by at least
/// 16 bytes per step.
#[derive(Clone, Debug)]
pub struct IndexEntries<'a> {
    bytes: &'a [u8],
    pos: usize,
    base: u32,
    skipped: u32,
    end: Option<WalkEnd>,
}

impl<'a> IndexEntries<'a> {
    /// Walks the entries stored in `bytes`: from the first entry of a node
    /// to the end of its used size. Reported entry offsets are
    /// `base_offset` plus the position inside `bytes`.
    ///
    /// [`IndexBlock::entries`] and [`IndexRoot::entries`] call this with
    /// the right bytes; use it directly for a node obtained elsewhere.
    #[inline]
    #[must_use]
    pub const fn new(bytes: &'a [u8], base_offset: u32) -> Self {
        IndexEntries {
            bytes,
            pos: 0,
            base: base_offset,
            skipped: 0,
            end: None,
        }
    }

    /// How the walk ended; `None` while the iterator has not returned
    /// `None` yet.
    #[inline]
    #[must_use]
    pub const fn end(&self) -> Option<WalkEnd> {
        self.end
    }

    /// Number of entries skipped so far because their key is not a
    /// `$FILE_NAME`.
    #[inline]
    #[must_use]
    pub const fn skipped(&self) -> u32 {
        self.skipped
    }

    /// The parent reference shared by most of the remaining entries: the
    /// directory this node belongs to (ALGORITHMS.md A3). `None` when no
    /// entry is left.
    ///
    /// In a sound `$I30` node every entry has the same parent. For the
    /// others: among equally frequent references the one met first wins,
    /// and only the first 16 distinct references are counted.
    #[must_use]
    pub fn owner(self) -> Option<FileRef> {
        let mut candidates = [(FileRef::ZERO, 0_u32); OWNER_CANDIDATES];
        let mut used = 0_usize;
        for entry in self {
            let parent = entry.file_name.parent;
            let known = candidates
                .iter_mut()
                .take(used)
                .find(|(candidate, _)| *candidate == parent);
            if let Some((_, count)) = known {
                *count = count.saturating_add(1);
            } else if let Some(slot) = candidates.get_mut(used) {
                *slot = (parent, 1);
                used = used.saturating_add(1);
            }
        }
        let mut best: Option<(FileRef, u32)> = None;
        for &(candidate, count) in candidates.iter().take(used) {
            if best.is_none_or(|(_, best_count)| count > best_count) {
                best = Some((candidate, count));
            }
        }
        best.map(|(parent, _)| parent)
    }

    /// One step of the walk: `Ok(Some(entry))` for a file-name entry,
    /// `Ok(None)` for an entry that was skipped, `Err(end)` when the walk
    /// is over.
    fn step(&mut self) -> Result<Option<IndexEntry<'a>>, WalkEnd> {
        let rest = self.bytes.get(self.pos..).ok_or(WalkEnd::Broken)?;
        let length = usize::from(u16_at(rest, 0x08).ok_or(WalkEnd::Broken)?);
        let key_length = usize::from(u16_at(rest, 0x0A).ok_or(WalkEnd::Broken)?);
        let flags = u16_at(rest, 0x0C).ok_or(WalkEnd::Broken)?;
        if length < HEADER_LEN || !length.is_multiple_of(8) {
            return Err(WalkEnd::Broken);
        }
        let entry = rest.get(..length).ok_or(WalkEnd::Broken)?;

        // The sub-node VCN occupies the last 8 bytes, behind the key.
        let (body, subnode_vcn) = if flags & INDEX_ENTRY_HAS_SUBNODE != 0 {
            let vcn_at = length.checked_sub(8).ok_or(WalkEnd::Broken)?;
            if vcn_at < HEADER_LEN {
                return Err(WalkEnd::Broken);
            }
            let vcn = u64_at(entry, vcn_at).ok_or(WalkEnd::Broken)?;
            (entry.get(..vcn_at).ok_or(WalkEnd::Broken)?, Some(vcn))
        } else {
            (entry, None)
        };
        if flags & INDEX_ENTRY_LAST != 0 {
            return Err(WalkEnd::Complete);
        }
        let key_end = HEADER_LEN.checked_add(key_length).ok_or(WalkEnd::Broken)?;
        let key = body.get(HEADER_LEN..key_end).ok_or(WalkEnd::Broken)?;

        let offset = self.base.saturating_add(u32_from(self.pos));
        self.pos = self.pos.saturating_add(length);
        let Some(file_name) = FileName::parse_exact(key) else {
            self.skipped = self.skipped.saturating_add(1);
            return Ok(None);
        };
        Ok(Some(IndexEntry {
            offset,
            child: FileRef::from_raw(u64_at(entry, 0x00).ok_or(WalkEnd::Broken)?),
            flags,
            subnode_vcn,
            file_name,
        }))
    }
}

impl<'a> Iterator for IndexEntries<'a> {
    type Item = IndexEntry<'a>;

    fn next(&mut self) -> Option<IndexEntry<'a>> {
        while self.end.is_none() {
            match self.step() {
                Ok(Some(entry)) => return Some(entry),
                Ok(None) => {}
                Err(end) => self.end = Some(end),
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.end.is_some() {
            return (0, Some(0));
        }
        // A file-name entry takes at least 0x10 + 0x44 bytes, rounded up.
        let left = self.bytes.len().saturating_sub(self.pos);
        (0, Some(left / 0x58))
    }
}

impl FusedIterator for IndexEntries<'_> {}

/// The value of an `$INDEX_ROOT` attribute: the root node of an index,
/// stored inside the MFT record. A small directory has all its entries
/// here; a large one keeps the entries that separate its `INDX` blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexRoot<'a> {
    /// Type of the indexed attribute (u32 at 0x00): 0x30 for `$I30`.
    pub indexed_type: u32,
    /// Collation rule (u32 at 0x04): 1 for file names.
    pub collation: u32,
    /// Size of the index blocks of this index in bytes (u32 at 0x08).
    pub index_block_size: u32,
    /// Size of an index block in clusters, or in 512-byte units when the
    /// cluster is larger than the block (u8 at 0x0C).
    pub blocks_per_index_block: u8,
    /// The node header at 0x10.
    pub node: NodeHeader,
    entries: &'a [u8],
    entries_offset: u32,
}

impl<'a> IndexRoot<'a> {
    /// Decodes an `$INDEX_ROOT` value. `value_offset` is where the value
    /// starts inside its record; it is added to the offsets of the entries.
    ///
    /// Returns `None` when the value is shorter than its two headers (0x20
    /// bytes) or the node header does not describe a range inside the
    /// value: entries must start at or behind 0x10 (counted from the node
    /// header) and `0x10 + used size` must not exceed the value.
    #[must_use]
    pub fn parse(value: &'a [u8], value_offset: u32) -> Option<IndexRoot<'a>> {
        let (node, entries, first) = NodeHeader::read(value, ROOT_NODE_OFFSET)?;
        Some(IndexRoot {
            indexed_type: u32_at(value, 0x00)?,
            collation: u32_at(value, 0x04)?,
            index_block_size: u32_at(value, 0x08)?,
            blocks_per_index_block: u8_at(value, 0x0C)?,
            node,
            entries,
            entries_offset: value_offset.saturating_add(u32_from(first)),
        })
    }

    /// The file-name entries of the root node; offsets are relative to the
    /// record.
    #[inline]
    #[must_use]
    pub const fn entries(&self) -> IndexEntries<'a> {
        IndexEntries::new(self.entries, self.entries_offset)
    }
}

/// What the first sector of an `INDX` block says about the block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexBlockProbe {
    /// Size of the block in bytes: a power of two in 512..=65536.
    pub block_size: u32,
    /// Offset of the update sequence array.
    pub usa_offset: u16,
    /// Entries of the update sequence array: `block_size / 512 + 1`.
    pub usa_count: u16,
}

/// Cheap check of the first sector of a possible `INDX` block.
///
/// Accepts when: the magic is `INDX`; the block size `0x18 + allocated
/// size of the node header` is a power of two in 512..=65536; the update
/// sequence array has `size / 512 + 1` entries, starts at or behind 0x28
/// and ends before the last two bytes of the sector.
///
/// At least the first 0x28 bytes must be given; a real caller passes the
/// 512-byte sector.
#[inline]
#[must_use]
pub fn probe_index_block(first_sector: &[u8]) -> Option<IndexBlockProbe> {
    check_index_block(first_sector).ok()
}

/// [`probe_index_block`] with the reason for a refusal.
pub(crate) fn check_index_block(first_sector: &[u8]) -> Result<IndexBlockProbe, Error> {
    let head = first_sector.first_chunk::<0x28>().ok_or(Error::TooShort)?;
    if head[0x00..0x04] != INDX_MAGIC {
        return Err(Error::BadMagic);
    }
    let usa_offset = u16::from_le_bytes([head[0x04], head[0x05]]);
    let usa_count = u16::from_le_bytes([head[0x06], head[0x07]]);
    let allocated = u32::from_le_bytes([head[0x20], head[0x21], head[0x22], head[0x23]]);
    let block_size = allocated
        .checked_add(u32_from(BLOCK_NODE_OFFSET))
        .filter(|size| {
            size.is_power_of_two() && (MIN_INDEX_BLOCK_SIZE..=MAX_INDEX_BLOCK_SIZE).contains(size)
        })
        .ok_or(Error::BadIndexBlockSize)?;
    let blocks = usize_from(block_size) / SECTOR_SIZE;
    if usize::from(usa_count) != blocks.saturating_add(1)
        || !usa_fits(usize::from(usa_offset), usize::from(usa_count), 0x28)
    {
        return Err(Error::BadUsa);
    }
    Ok(IndexBlockProbe {
        block_size,
        usa_offset,
        usa_count,
    })
}

/// A parsed `INDX` block: one node of a directory index, stored in the
/// directory's `$INDEX_ALLOCATION`.
#[derive(Clone, Copy, Debug)]
pub struct IndexBlock<'a> {
    bytes: &'a [u8],
    lsn: u64,
    vcn: u64,
    node: NodeHeader,
    fixup: FixupStatus,
    entries: &'a [u8],
    entries_offset: u32,
}

impl<'a> IndexBlock<'a> {
    /// `$LogFile` sequence number of the last change (u64 at 0x08). Among
    /// several copies of a block the highest is the newest.
    #[inline]
    #[must_use]
    pub const fn lsn(&self) -> u64 {
        self.lsn
    }

    /// The block's own position inside `$INDEX_ALLOCATION` (u64 at 0x10).
    /// The unit is a cluster when the cluster is not larger than the index
    /// block, else 512 bytes. As stored: treat it as untrusted and multiply
    /// with checked arithmetic.
    #[inline]
    #[must_use]
    pub const fn vcn(&self) -> u64 {
        self.vcn
    }

    /// Size of the block in bytes.
    #[inline]
    #[must_use]
    pub fn size(&self) -> u32 {
        u32_from(self.bytes.len())
    }

    /// Outcome of the fixup step.
    #[inline]
    #[must_use]
    pub const fn fixup(&self) -> FixupStatus {
        self.fixup
    }

    /// The node header at 0x18.
    #[inline]
    #[must_use]
    pub const fn node(&self) -> NodeHeader {
        self.node
    }

    /// The bytes of the block, after fixups.
    #[inline]
    #[must_use]
    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The file-name entries of the block; offsets are relative to the
    /// block.
    #[inline]
    #[must_use]
    pub const fn entries(&self) -> IndexEntries<'a> {
        IndexEntries::new(self.entries, self.entries_offset)
    }

    /// The directory the block belongs to: the parent reference shared by
    /// most entries. See [`IndexEntries::owner`].
    #[inline]
    #[must_use]
    pub fn owner(&self) -> Option<FileRef> {
        self.entries().owner()
    }
}

/// Undoes the multi-sector protection of the `INDX` block at the start of
/// `block`, in place, and parses it.
///
/// `block` must hold at least the whole block (its size is in the header,
/// see [`probe_index_block`]); bytes behind it are neither read nor
/// modified. A fixup mismatch is not an error: it is reported by
/// [`IndexBlock::fixup`] and parsing goes on.
///
/// # Errors
///
/// * [`Error::TooShort`], [`Error::BadMagic`], [`Error::BadIndexBlockSize`],
///   [`Error::BadUsa`]: the header checks of [`probe_index_block`] failed,
///   or `block` is shorter than the block. Nothing was modified.
/// * [`Error::BadIndexHeader`]: the node header at 0x18 puts the entries
///   inside itself, inside the update sequence array or behind the used
///   size, or the used size beyond the allocated size. The fixups have
///   been applied.
pub fn parse_index_block(block: &mut [u8]) -> Result<IndexBlock<'_>, Error> {
    let probe = check_index_block(block)?;
    let block = block
        .get_mut(..usize_from(probe.block_size))
        .ok_or(Error::TooShort)?;
    let fixup = apply_validated(
        block,
        usize::from(probe.usa_offset),
        usize::from(probe.usa_count),
    );
    parse_fixed(block, probe, fixup)
}

/// Parses an `INDX` block whose multi-sector protection has already been
/// undone (by [`apply_fixups`](crate::apply_fixups), or because the bytes
/// come from a place that stores them that way). `fixup` is recorded as the
/// block's fixup status.
///
/// # Errors
///
/// As [`parse_index_block`].
pub fn parse_index_block_fixed(block: &[u8], fixup: FixupStatus) -> Result<IndexBlock<'_>, Error> {
    let probe = check_index_block(block)?;
    let block = block
        .get(..usize_from(probe.block_size))
        .ok_or(Error::TooShort)?;
    parse_fixed(block, probe, fixup)
}

fn parse_fixed(
    block: &[u8],
    probe: IndexBlockProbe,
    fixup: FixupStatus,
) -> Result<IndexBlock<'_>, Error> {
    // The allocated size is the block size less 0x18 (the probe made sure),
    // so a used size beyond it is one that leaves the block.
    let (node, entries, first) =
        NodeHeader::read(block, BLOCK_NODE_OFFSET).ok_or(Error::BadIndexHeader)?;
    // Like the first attribute of a record, the first entry lies behind
    // the update sequence array.
    let usa_end = usize::from(probe.usa_count)
        .saturating_mul(2)
        .saturating_add(usize::from(probe.usa_offset));
    if first < usa_end {
        return Err(Error::BadIndexHeader);
    }
    Ok(IndexBlock {
        bytes: block,
        lsn: u64_at(block, 0x08).ok_or(Error::TooShort)?,
        vcn: u64_at(block, 0x10).ok_or(Error::TooShort)?,
        node,
        fixup,
        entries,
        entries_offset: u32_from(first),
    })
}
