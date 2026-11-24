//! `FILE` records (docs/notes/ntfs-on-disk.md, section 3).

use crate::attr::{attr_len, Attribute, Attributes, StandardInformation};
use crate::bytes::{u32_from, usize_from};
use crate::error::Error;
use crate::file_name::FileName;
use crate::fixup::{apply_validated, usa_fits, FixupStatus, SECTOR_SIZE};
use crate::reference::FileRef;

/// Signature of an MFT record.
pub const FILE_MAGIC: [u8; 4] = *b"FILE";

/// Smallest MFT record.
pub const MIN_RECORD_SIZE: u32 = 1024;
/// Largest record the scanner accepts by default. Windows writes 1024-byte
/// records, and 4096-byte ones on 4Kn disks and with `format /L`.
pub const DEFAULT_MAX_RECORD_SIZE: u32 = 4096;
/// Largest record that can exist: the update sequence array of a larger
/// one would not fit in the first 512 bytes.
pub const MAX_RECORD_SIZE: u32 = 65_536;

/// Record header flag: the record is in use.
pub const RECORD_IN_USE: u16 = 0x0001;
/// Record header flag: the record is a directory (it has a `$I30` index).
/// Kept when a directory is deleted; never set on an extension record.
pub const RECORD_IS_DIRECTORY: u16 = 0x0002;

/// Length of the header up to and including the record number.
const HEADER_LEN: usize = 0x30;
/// Smallest offset the update sequence array can have: the end of the
/// 42-byte header of NTFS 3.0 and older.
const MIN_USA_OFFSET: usize = 0x2A;
/// From this update sequence offset on, the header has the NTFS 3.1 layout
/// with the record number at 0x2C.
const NUMBERED_USA_OFFSET: u16 = 0x30;

/// What the first sector of a `FILE` record says about the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecordProbe {
    /// Allocated size of the record in bytes: a power of two.
    pub record_size: u32,
    /// Offset of the update sequence array.
    pub usa_offset: u16,
    /// Entries of the update sequence array: `record_size / 512 + 1`.
    pub usa_count: u16,
}

/// Cheap check of the first sector of a possible `FILE` record.
///
/// Accepts when: the magic is `FILE`; the allocated size (u32 at 0x1C) is a
/// power of two in `1024..=max_record_size`, where `max_record_size` is
/// itself capped at 65536; the update sequence array has
/// `size / 512 + 1` entries, starts at or behind 0x2A and ends before the
/// last two bytes of the sector (`offset + 2 * count <= 510`).
///
/// Pass [`DEFAULT_MAX_RECORD_SIZE`] unless larger records are expected. At
/// least the first 0x30 bytes must be given; a real caller passes the
/// 512-byte sector. The check costs one comparison for a sector that does
/// not start with the magic.
#[inline]
#[must_use]
pub fn probe_file_record(first_sector: &[u8], max_record_size: u32) -> Option<RecordProbe> {
    check_file_record(first_sector, max_record_size).ok()
}

/// [`probe_file_record`] with the reason for a refusal.
#[inline]
fn check_file_record(first_sector: &[u8], max_record_size: u32) -> Result<RecordProbe, Error> {
    let head = first_sector
        .first_chunk::<HEADER_LEN>()
        .ok_or(Error::TooShort)?;
    if head[0x00..0x04] != FILE_MAGIC {
        return Err(Error::BadMagic);
    }
    let usa_offset = u16::from_le_bytes([head[0x04], head[0x05]]);
    let usa_count = u16::from_le_bytes([head[0x06], head[0x07]]);
    let record_size = u32::from_le_bytes([head[0x1C], head[0x1D], head[0x1E], head[0x1F]]);
    if !record_size.is_power_of_two()
        || record_size < MIN_RECORD_SIZE
        || record_size > max_record_size.min(MAX_RECORD_SIZE)
    {
        return Err(Error::BadRecordSize);
    }
    let blocks = usize_from(record_size) / SECTOR_SIZE;
    if usize::from(usa_count) != blocks.saturating_add(1)
        || !usa_fits(
            usize::from(usa_offset),
            usize::from(usa_count),
            MIN_USA_OFFSET,
        )
    {
        return Err(Error::BadUsa);
    }
    Ok(RecordProbe {
        record_size,
        usa_offset,
        usa_count,
    })
}

/// The fixed header of a `FILE` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecordHeader {
    /// `$LogFile` sequence number of the last change (u64 at 0x08). Always
    /// 0 on volumes written by ntfs-3g.
    pub lsn: u64,
    /// Sequence number (u16 at 0x10): bumped when the record is freed.
    pub sequence: u16,
    /// Link count (u16 at 0x12): number of `$FILE_NAME` attributes. Not a
    /// deletion marker: Windows leaves it unchanged when a file is deleted.
    pub link_count: u16,
    /// Offset of the first attribute (u16 at 0x14).
    pub attrs_offset: u16,
    /// Flags (u16 at 0x16); see [`RECORD_IN_USE`] and
    /// [`RECORD_IS_DIRECTORY`].
    pub flags: u16,
    /// Bytes in use (u32 at 0x18): the end marker ends here.
    pub used_size: u32,
    /// Bytes allocated (u32 at 0x1C): the record size.
    pub allocated_size: u32,
    /// Base record reference (u64 at 0x20): zero in a base record, the
    /// reference to the base record in an extension record.
    pub base: FileRef,
    /// Next attribute id (u16 at 0x28).
    pub next_attr_id: u16,
    /// The record's own number (u32 at 0x2C). Present exactly when the
    /// update sequence array starts at 0x30 or later (NTFS 3.1 and newer);
    /// `None` for the 42-byte header of older volumes. A claim to be
    /// checked, not the truth: mkntfs leaves 0 in records 16-23.
    pub record_number: Option<u32>,
    /// Offset of the update sequence array (u16 at 0x04).
    pub usa_offset: u16,
    /// Entries of the update sequence array (u16 at 0x06).
    pub usa_count: u16,
}

/// How the walk over the attributes of a record ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttrChain {
    /// The end marker was reached.
    Complete,
    /// The walk stopped at `offset` because no attribute can start there:
    /// the used size ended without an end marker, or the length field is
    /// smaller than an attribute header, not a multiple of 8, or reaches
    /// past the used size. The attributes before it were walked normally.
    Broken {
        /// Offset inside the record where the walk stopped.
        offset: u32,
    },
}

impl AttrChain {
    /// Whether the end marker was reached.
    #[inline]
    #[must_use]
    pub const fn is_complete(self) -> bool {
        matches!(self, AttrChain::Complete)
    }
}

/// A parsed `FILE` record: the header, decoded, and a view of the
/// attributes, which are decoded while iterating. Borrows the record
/// buffer; nothing is copied or allocated.
#[derive(Clone, Copy, Debug)]
pub struct FileRecord<'a> {
    bytes: &'a [u8],
    header: RecordHeader,
    fixup: FixupStatus,
    attribute_count: u32,
    chain: AttrChain,
}

impl<'a> FileRecord<'a> {
    /// The header fields.
    #[inline]
    #[must_use]
    pub const fn header(&self) -> &RecordHeader {
        &self.header
    }

    /// The whole record (allocated size), after fixups. Offsets reported
    /// for attributes and resident values index into this slice.
    #[inline]
    #[must_use]
    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Outcome of the fixup step.
    #[inline]
    #[must_use]
    pub const fn fixup(&self) -> FixupStatus {
        self.fixup
    }

    /// `$LogFile` sequence number of the last change.
    #[inline]
    #[must_use]
    pub const fn lsn(&self) -> u64 {
        self.header.lsn
    }

    /// Sequence number.
    #[inline]
    #[must_use]
    pub const fn sequence(&self) -> u16 {
        self.header.sequence
    }

    /// Link count.
    #[inline]
    #[must_use]
    pub const fn link_count(&self) -> u16 {
        self.header.link_count
    }

    /// Header flags.
    #[inline]
    #[must_use]
    pub const fn flags(&self) -> u16 {
        self.header.flags
    }

    /// Whether the in-use flag is set.
    #[inline]
    #[must_use]
    pub const fn is_in_use(&self) -> bool {
        self.header.flags & RECORD_IN_USE != 0
    }

    /// Whether the directory flag is set.
    #[inline]
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.header.flags & RECORD_IS_DIRECTORY != 0
    }

    /// Bytes in use.
    #[inline]
    #[must_use]
    pub const fn used_size(&self) -> u32 {
        self.header.used_size
    }

    /// Bytes allocated: the record size.
    #[inline]
    #[must_use]
    pub const fn allocated_size(&self) -> u32 {
        self.header.allocated_size
    }

    /// The base record reference; zero in a base record.
    #[inline]
    #[must_use]
    pub const fn base(&self) -> FileRef {
        self.header.base
    }

    /// Whether this is an extension record (the base reference is not
    /// zero).
    #[inline]
    #[must_use]
    pub const fn is_extension(&self) -> bool {
        !self.header.base.is_zero()
    }

    /// The record's own number, when the header stores one.
    #[inline]
    #[must_use]
    pub const fn record_number(&self) -> Option<u32> {
        self.header.record_number
    }

    /// Number of attributes the walk stepped over, well formed or not.
    /// Zero for a record that was never used (or wiped).
    #[inline]
    #[must_use]
    pub const fn attribute_count(&self) -> u32 {
        self.attribute_count
    }

    /// How the walk over the attributes ended.
    #[inline]
    #[must_use]
    pub const fn attr_chain(&self) -> AttrChain {
        self.chain
    }

    /// Whether the record was never used: not in use and without a single
    /// attribute. Such records fill the unused slots of an MFT and carry
    /// no information.
    #[inline]
    #[must_use]
    pub const fn is_never_used(&self) -> bool {
        !self.is_in_use() && self.attribute_count == 0
    }

    /// The attributes in on-disk order. This is the efficient way to read a
    /// record: one pass, matching on [`Attribute::type_code`].
    #[inline]
    #[must_use]
    pub fn attributes(&self) -> Attributes<'a> {
        let used = self
            .bytes
            .get(..usize_from(self.header.used_size))
            .unwrap_or(self.bytes);
        Attributes::new(used, usize::from(self.header.attrs_offset))
    }

    /// The first attribute with this type code and exactly this name (`""`
    /// for an unnamed attribute). Walks the attributes.
    #[must_use]
    pub fn find(&self, type_code: u32, name: &str) -> Option<Attribute<'a>> {
        self.attributes()
            .find(|attr| attr.type_code == type_code && attr.is_named(name))
    }

    /// The decoded `$STANDARD_INFORMATION`, when the record has one. Walks
    /// the attributes.
    #[must_use]
    pub fn standard_information(&self) -> Option<StandardInformation> {
        self.attributes()
            .find_map(|attr| attr.standard_information())
    }

    /// Every decodable `$FILE_NAME` of the record, in on-disk order (in a
    /// record written by Windows the DOS alias comes before the long name).
    /// Walks the attributes; use [`attributes`](FileRecord::attributes) to
    /// get the attribute ids as well.
    pub fn file_names(&self) -> impl Iterator<Item = FileName<'a>> + 'a {
        self.attributes().filter_map(|attr| attr.file_name())
    }
}

/// Undoes the multi-sector protection of the `FILE` record at the start of
/// `record`, in place, and parses it.
///
/// `record` must hold at least the whole record (its size is in the
/// header, see [`probe_file_record`]); bytes behind it are neither read nor
/// modified. Records of up to 65536 bytes are accepted: the size policy is
/// the caller's, through the probe.
///
/// Compute [`fnv1a64`](crate::fnv1a64) *before* calling this: the hash is
/// of the bytes as read. If the same buffer is scanned on at 512-byte
/// steps afterwards, note that the last two bytes of each 512-byte block of
/// the record have changed.
///
/// A fixup mismatch is not an error: it is reported by
/// [`FileRecord::fixup`] and parsing goes on with what is there. Faults in
/// the attributes are not errors either; see [`FileRecord::attr_chain`]
/// and [`AttrBody::Malformed`](crate::AttrBody::Malformed).
///
/// # Errors
///
/// * [`Error::TooShort`], [`Error::BadMagic`], [`Error::BadRecordSize`],
///   [`Error::BadUsa`]: the header checks of [`probe_file_record`] failed,
///   or `record` is shorter than the record. Nothing was modified.
/// * [`Error::BadAttrsOffset`]: the first attribute would start inside the
///   header or the update sequence array, outside the record, or at an
///   offset that is not a multiple of 8.
/// * [`Error::BadUsedSize`]: the used size exceeds the record or ends
///   before the first attribute's type field.
///
/// For the last two, the fixups have been applied.
pub fn parse_file_record(record: &mut [u8]) -> Result<FileRecord<'_>, Error> {
    let probe = check_file_record(record, MAX_RECORD_SIZE)?;
    let record = record
        .get_mut(..usize_from(probe.record_size))
        .ok_or(Error::TooShort)?;
    let fixup = apply_validated(
        record,
        usize::from(probe.usa_offset),
        usize::from(probe.usa_count),
    );
    parse_fixed(record, probe, fixup)
}

/// Parses a `FILE` record whose multi-sector protection has already been
/// undone (by [`apply_fixups`](crate::apply_fixups)). `fixup` is recorded
/// as the record's fixup status.
///
/// # Errors
///
/// As [`parse_file_record`].
pub fn parse_file_record_fixed(record: &[u8], fixup: FixupStatus) -> Result<FileRecord<'_>, Error> {
    let probe = check_file_record(record, MAX_RECORD_SIZE)?;
    let record = record
        .get(..usize_from(probe.record_size))
        .ok_or(Error::TooShort)?;
    parse_fixed(record, probe, fixup)
}

fn parse_fixed(
    record: &[u8],
    probe: RecordProbe,
    fixup: FixupStatus,
) -> Result<FileRecord<'_>, Error> {
    let head = record.first_chunk::<HEADER_LEN>().ok_or(Error::TooShort)?;
    let attrs_offset = u16::from_le_bytes([head[0x14], head[0x15]]);
    let used_size = u32::from_le_bytes([head[0x18], head[0x19], head[0x1A], head[0x1B]]);

    let usa_end = usize::from(probe.usa_count)
        .saturating_mul(2)
        .saturating_add(usize::from(probe.usa_offset));
    let first = usize::from(attrs_offset);
    if first < usa_end || !first.is_multiple_of(8) || first >= record.len() {
        return Err(Error::BadAttrsOffset);
    }
    // The used size must at least leave room for the type field of the
    // first attribute, which may be the end marker.
    let used = record
        .get(..usize_from(used_size))
        .filter(|used| first.saturating_add(4) <= used.len())
        .ok_or(Error::BadUsedSize)?;

    let header = RecordHeader {
        lsn: u64::from_le_bytes([
            head[0x08], head[0x09], head[0x0A], head[0x0B], head[0x0C], head[0x0D], head[0x0E],
            head[0x0F],
        ]),
        sequence: u16::from_le_bytes([head[0x10], head[0x11]]),
        link_count: u16::from_le_bytes([head[0x12], head[0x13]]),
        attrs_offset,
        flags: u16::from_le_bytes([head[0x16], head[0x17]]),
        used_size,
        allocated_size: probe.record_size,
        base: FileRef::from_raw(u64::from_le_bytes([
            head[0x20], head[0x21], head[0x22], head[0x23], head[0x24], head[0x25], head[0x26],
            head[0x27],
        ])),
        next_attr_id: u16::from_le_bytes([head[0x28], head[0x29]]),
        record_number: (probe.usa_offset >= NUMBERED_USA_OFFSET)
            .then(|| u32::from_le_bytes([head[0x2C], head[0x2D], head[0x2E], head[0x2F]])),
        usa_offset: probe.usa_offset,
        usa_count: probe.usa_count,
    };

    // Count the attributes. Every step advances by at least 24 bytes and
    // stays inside the used size, so the loop is bounded by the record.
    let mut attribute_count = 0_u32;
    let mut pos = first;
    let chain = loop {
        match attr_len(used, pos) {
            Ok(Some(length)) => {
                attribute_count = attribute_count.saturating_add(1);
                pos = pos.saturating_add(length);
            }
            Ok(None) => break AttrChain::Complete,
            Err(()) => {
                break AttrChain::Broken {
                    offset: u32_from(pos),
                }
            }
        }
    };

    Ok(FileRecord {
        bytes: record,
        header,
        fixup,
        attribute_count,
        chain,
    })
}
