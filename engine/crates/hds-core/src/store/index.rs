//! Directory index data: [`IndexEntryView`] (ALGORITHMS A5) and
//! [`IndexBlockInfo`] (the tuple of A3.2).

use hds_ntfs::{FileRef, FixupStatus, Namespace};

use super::record::StoredName;

/// Bits of [`IndexEntryRec::bits`].
pub(crate) mod entry_bits {
    /// The name is stored with one byte per unit.
    pub const NARROW: u8 = 1 << 0;
    /// The entry comes from the `$INDEX_ROOT` of a record (else from an
    /// `INDX` block).
    pub const FROM_ROOT: u8 = 1 << 1;
    /// The source structure had a fixup mismatch.
    pub const SOURCE_TORN: u8 = 1 << 2;
    /// Every bit defined above.
    pub const ALL: u8 = (1 << 3) - 1;
}

/// A directory entry as stored (48 bytes; the name lives in the store's
/// entry-name arena).
#[derive(Clone, Copy, Debug)]
pub(crate) struct IndexEntryRec {
    pub child: u64,
    pub parent: u64,
    pub mft_changed: u64,
    pub source_offset: u64,
    pub source_lsn: u64,
    pub name_off: u32,
    pub name_len: u8,
    pub namespace: u8,
    pub bits: u8,
}

impl IndexEntryRec {
    /// Bytes the name occupies in the arena.
    pub(crate) fn name_bytes(&self) -> usize {
        let len = usize::from(self.name_len);
        if self.bits & entry_bits::NARROW != 0 {
            len
        } else {
            len.saturating_mul(2)
        }
    }
}

/// Where an index entry was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntrySourceKind {
    /// An `INDX` block at the source offset.
    Block,
    /// The `$INDEX_ROOT:$I30` of the record at the source offset.
    RecordRoot,
}

/// Where an index entry was found, and how trustworthy that place is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EntrySource {
    /// Absolute disk offset of the `INDX` block or of the record.
    pub offset: u64,
    /// `$LogFile` sequence number of that block or record (ALGORITHMS E6:
    /// among several entries, the one with the highest source LSN wins).
    pub lsn: u64,
    /// Block or record.
    pub kind: EntrySourceKind,
    /// Whether the block or record had a fixup mismatch.
    pub torn: bool,
}

/// One directory entry of an index node: an entry whose `$FILE_NAME` key
/// carries the directory flag (ALGORITHMS A5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexEntryView<'a> {
    /// The directory the entry names (its base record).
    pub child: FileRef,
    /// The directory that holds the entry (the key's parent reference).
    pub parent: FileRef,
    /// Namespace of the name.
    pub namespace: Namespace,
    /// The name.
    pub name: StoredName<'a>,
    /// MFT-change time of the key's `$FILE_NAME` copy (FILETIME).
    pub mft_changed: u64,
    /// Where the entry was found.
    pub source: EntrySource,
}

/// One `INDX` block found by the scan (ALGORITHMS A3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexBlockInfo {
    /// Absolute disk offset.
    pub disk_offset: u64,
    /// The block's VCN as stored (unit: cluster, or 512 bytes when the
    /// cluster is larger than the block). Untrusted: multiply with checked
    /// arithmetic.
    pub vcn: u64,
    /// Block size in bytes: a power of two from 512 to 65536.
    pub size: u32,
    /// `$LogFile` sequence number.
    pub lsn: u64,
    /// The parent reference shared by most file-name entries: the directory
    /// the block belongs to; `None` when the block has no such entry.
    pub owner: Option<FileRef>,
    /// Fixup status of the block.
    pub fixup: FixupStatus,
}

impl IndexBlockInfo {
    /// One past the last byte of the block.
    #[must_use]
    pub fn end_offset(&self) -> u64 {
        self.disk_offset.saturating_add(u64::from(self.size))
    }
}
