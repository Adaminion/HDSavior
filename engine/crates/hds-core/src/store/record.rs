//! [`RawRecord`] (ALGORITHMS A4) and the encoding of its variable-length
//! parts in the store's `aux` arena.
//!
//! # The aux region of a record
//!
//! Names, `$DATA` and `$INDEX_ALLOCATION:$I30` extents and resident
//! attribute-list entries of one record are stored back to back in one
//! region of a byte arena, as tagged items. All integers are little-endian;
//! the items are byte-packed (no alignment):
//!
//! | tag | item | layout after the tag byte |
//! |---|---|---|
//! | 1 | `$FILE_NAME` | namespace u8, length u8 (UTF-16 units), encoding u8 (0 = one byte per unit, all units below 0x100; 1 = UTF-16LE), attribute id u16, flags u32, parent reference u64, then the units |
//! | 2 | resident unnamed `$DATA` | attribute id u16, flags u16, value offset u32, value length u32 |
//! | 3 | non-resident extent with start VCN 0 | target u8 (0 = unnamed `$DATA`, 1 = `$INDEX_ALLOCATION:$I30`), attribute id u16, flags u16, runlist status u8, last VCN i64, allocated u64, real u64, initialized u64, runlist length u16, runlist bytes |
//! | 4 | non-resident extent with start VCN > 0 | target u8, attribute id u16, flags u16, runlist status u8, start VCN u64, last VCN i64, runlist length u16, runlist bytes |
//! | 5 | resident `$ATTRIBUTE_LIST` entry for type 0x80 or 0xA0 | type u8 (0 = 0x80, 1 = 0xA0), name class u8 (0 unnamed, 1 `$I30`, 2 other), attribute id u16, start VCN u64, holder reference u64 |
//!
//! The runlist bytes are the attribute's own mapping pairs, cut where the
//! decoder of `hds-ntfs` stopped: up to and including the terminator for a
//! complete list, up to the end of the last valid run otherwise. Decoding
//! them again with [`hds_ntfs::Runs`] gives exactly the valid runs; the
//! status byte says why the list ended. Most runs take 3 to 7 bytes this way
//! instead of 16 decoded.
//!
//! Items are written in the order names, `$DATA`, `$INDEX_ALLOCATION`,
//! attribute-list entries, each group in on-disk order, so the encoding of
//! a record is a function of the record's bytes alone.

use hds_ntfs::{
    Extent, FileName, FileRef, FixupStatus, Namespace, Run, RunlistError, RunlistStatus, Runs,
    StandardInformation, StreamSizes, Times, ATTR_FLAG_COMPRESSION_MASK, ATTR_FLAG_ENCRYPTED,
    ATTR_FLAG_SPARSE, RECORD_IN_USE, RECORD_IS_DIRECTORY,
};

/// Flag bits of [`RawRecord`] (private encoding, also used in session files).
pub(crate) mod bits {
    /// The header stores a record number (NTFS 3.1+).
    pub const HAS_NUMBER: u16 = 1 << 0;
    /// A `$STANDARD_INFORMATION` was decoded.
    pub const HAS_SI: u16 = 1 << 1;
    /// A `$DATA` stream named `WofCompressedData` exists.
    pub const WOF_STREAM: u16 = 1 << 2;
    /// An `$ATTRIBUTE_LIST` exists.
    pub const ATTR_LIST: u16 = 1 << 3;
    /// ... it is resident (its 0x80 / 0xA0 entries are stored).
    pub const ATTR_LIST_RESIDENT: u16 = 1 << 4;
    /// ... its resident entries did not end properly.
    pub const ATTR_LIST_INCOMPLETE: u16 = 1 << 5;
    /// ... its attribute header contradicts itself.
    pub const ATTR_LIST_MALFORMED: u16 = 1 << 6;
    /// A `$REPARSE_POINT` exists.
    pub const REPARSE: u16 = 1 << 7;
    /// ... and its tag is known (resident, at least 4 bytes).
    pub const REPARSE_TAG: u16 = 1 << 8;
    /// The attribute walk stopped before the end marker.
    pub const CHAIN_BROKEN: u16 = 1 << 9;
    /// An unnamed `$DATA` attribute is malformed: content location unknown.
    pub const DATA_MALFORMED: u16 = 1 << 10;
    /// A `$INDEX_ALLOCATION:$I30` attribute is malformed or resident.
    pub const IA_MALFORMED: u16 = 1 << 11;
    /// A `$FILE_NAME` attribute does not decode.
    pub const NAME_MALFORMED: u16 = 1 << 12;
    /// A `$STANDARD_INFORMATION` attribute does not decode.
    pub const SI_MALFORMED: u16 = 1 << 13;
    /// The record has at least one attribute.
    pub const HAS_ATTRIBUTES: u16 = 1 << 14;
    /// Every bit defined above.
    pub const ALL: u16 = (1 << 15) - 1;
}

/// Item tags of the aux region.
pub(crate) const TAG_NAME: u8 = 1;
pub(crate) const TAG_RESIDENT: u8 = 2;
pub(crate) const TAG_EXTENT0: u8 = 3;
pub(crate) const TAG_EXTENT: u8 = 4;
pub(crate) const TAG_ATTR_LIST: u8 = 5;

/// Fixed part of a name item, the tag included.
const NAME_HEADER: usize = 18;
/// Size of a resident `$DATA` item.
const RESIDENT_ITEM: usize = 13;
/// Fixed part of an extent item with start VCN 0, the tag included.
const EXTENT0_HEADER: usize = 41;
/// Fixed part of an extent item with start VCN > 0, the tag included.
const EXTENT_HEADER: usize = 25;
/// Size of an attribute-list item.
const ATTR_LIST_ITEM: usize = 21;

/// One MFT record as the scan found it (ALGORITHMS A4). 104 bytes; the
/// variable-length parts live in the store's arena and are read through
/// [`ScanStore`](crate::store::ScanStore) (`names`, `data_extents`, ...).
///
/// Every number is as stored on the disk and untrusted: use checked
/// arithmetic on it.
#[derive(Clone, Copy, Debug)]
pub struct RawRecord {
    pub(crate) disk_offset: u64,
    pub(crate) lsn: u64,
    pub(crate) hash: u64,
    pub(crate) base: u64,
    /// Creation, modification, MFT change, access (FILETIME).
    pub(crate) si_times: [u64; 4],
    /// Address of the record's aux region in the store's arena.
    pub(crate) aux_off: u64,
    pub(crate) si_flags: u32,
    pub(crate) number: u32,
    pub(crate) reparse_tag: u32,
    /// Length of the aux region.
    pub(crate) aux_len: u32,
    pub(crate) sequence: u16,
    pub(crate) header_flags: u16,
    pub(crate) link_count: u16,
    pub(crate) bits: u16,
    pub(crate) fixup_first_bad: u8,
    pub(crate) fixup_bad: u8,
    /// log2 of the record size (10 to 16).
    pub(crate) size_log2: u8,
}

/// State of a record's `$ATTRIBUTE_LIST`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttributeListState {
    /// The record has no `$ATTRIBUTE_LIST`.
    Absent,
    /// The list is resident; its entries for types 0x80 and 0xA0 are stored
    /// ([`ScanStore::attribute_list_entries`](crate::store::ScanStore::attribute_list_entries)).
    /// `complete` is false when the walk over the entries met a broken one
    /// (the entries before it are stored).
    Resident {
        /// Whether the entries filled the value exactly.
        complete: bool,
    },
    /// The list is stored in clusters; only its presence is known.
    NonResident,
    /// The attribute's header contradicts itself.
    Malformed,
}

/// What a record says about a `$REPARSE_POINT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reparse {
    /// No `$REPARSE_POINT` attribute.
    None,
    /// A resident `$REPARSE_POINT` with this tag.
    Tag(u32),
    /// A `$REPARSE_POINT` whose tag is not in the record: non-resident, or
    /// malformed (ALGORITHMS A4: counts as tag unknown).
    Unknown,
}

/// Damage seen while the record was decoded, beyond the fixup status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RecordDamage {
    /// The attribute walk stopped before the end marker: attributes behind
    /// that point are unknown (so "no `$DATA` found" does not mean "no
    /// data").
    pub attributes_cut: bool,
    /// An unnamed `$DATA` attribute is malformed: the content location is
    /// unknown (never "no data").
    pub data_malformed: bool,
    /// A `$INDEX_ALLOCATION:$I30` attribute is malformed or resident.
    pub index_allocation_malformed: bool,
    /// A `$FILE_NAME` attribute does not decode.
    pub name_malformed: bool,
    /// A `$STANDARD_INFORMATION` attribute does not decode.
    pub standard_information_malformed: bool,
}

impl RecordDamage {
    /// Whether anything at all is damaged.
    #[must_use]
    pub fn any(&self) -> bool {
        self.attributes_cut
            || self.data_malformed
            || self.index_allocation_malformed
            || self.name_malformed
            || self.standard_information_malformed
    }
}

impl RawRecord {
    /// Absolute byte offset of the record on the source.
    #[must_use]
    pub fn disk_offset(&self) -> u64 {
        self.disk_offset
    }

    /// Allocated size of the record: a power of two from 1024 to 65536.
    #[must_use]
    pub fn record_size(&self) -> u32 {
        1_u32.checked_shl(u32::from(self.size_log2)).unwrap_or(0)
    }

    /// One past the last byte of the record on the source.
    #[must_use]
    pub fn end_offset(&self) -> u64 {
        self.disk_offset
            .saturating_add(u64::from(self.record_size()))
    }

    /// The record number the header stores; `None` for the 42-byte header
    /// of NTFS 3.0 and older. A claim to be checked, not the truth.
    #[must_use]
    pub fn record_number(&self) -> Option<u32> {
        (self.bits & bits::HAS_NUMBER != 0).then_some(self.number)
    }

    /// Sequence number.
    #[must_use]
    pub fn sequence(&self) -> u16 {
        self.sequence
    }

    /// Header flags (u16 at 0x16).
    #[must_use]
    pub fn header_flags(&self) -> u16 {
        self.header_flags
    }

    /// Whether the in-use flag is set.
    #[must_use]
    pub fn is_in_use(&self) -> bool {
        self.header_flags & RECORD_IN_USE != 0
    }

    /// Whether the directory flag of the header is set. (A directory can
    /// also be recognised by the `$FILE_NAME` flag 0x10000000; ALGORITHMS
    /// E3.)
    #[must_use]
    pub fn is_directory(&self) -> bool {
        self.header_flags & RECORD_IS_DIRECTORY != 0
    }

    /// Link count (u16 at 0x12). Not a deletion marker.
    #[must_use]
    pub fn link_count(&self) -> u16 {
        self.link_count
    }

    /// `$LogFile` sequence number of the record.
    #[must_use]
    pub fn lsn(&self) -> u64 {
        self.lsn
    }

    /// The base record reference; zero in a base record.
    #[must_use]
    pub fn base(&self) -> FileRef {
        FileRef::from_raw(self.base)
    }

    /// Whether this is an extension record (non-zero base reference).
    #[must_use]
    pub fn is_extension(&self) -> bool {
        self.base != 0
    }

    /// FNV-1a 64 of the record's bytes as read, before fixups.
    #[must_use]
    pub fn hash(&self) -> u64 {
        self.hash
    }

    /// The fixup status. Records in which every 512-byte block failed are
    /// never stored.
    #[must_use]
    pub fn fixup(&self) -> FixupStatus {
        if self.fixup_bad == 0 {
            FixupStatus::Ok
        } else {
            FixupStatus::Torn {
                first_bad_block: self.fixup_first_bad,
                bad_blocks: self.fixup_bad,
            }
        }
    }

    /// The decoded `$STANDARD_INFORMATION` (the first one of the record),
    /// when the record has one.
    #[must_use]
    pub fn standard_information(&self) -> Option<StandardInformation> {
        (self.bits & bits::HAS_SI != 0).then(|| {
            let [created, modified, mft_changed, accessed] = self.si_times;
            StandardInformation {
                times: Times {
                    created,
                    modified,
                    mft_changed,
                    accessed,
                },
                flags: self.si_flags,
            }
        })
    }

    /// The MFT-change time of `$STANDARD_INFORMATION`, when present.
    #[must_use]
    pub fn mft_changed(&self) -> Option<u64> {
        let [_, _, mft_changed, _] = self.si_times;
        (self.bits & bits::HAS_SI != 0).then_some(mft_changed)
    }

    /// Whether a `$DATA` stream named `WofCompressedData` exists.
    #[must_use]
    pub fn has_wof_stream(&self) -> bool {
        self.bits & bits::WOF_STREAM != 0
    }

    /// The state of the record's `$ATTRIBUTE_LIST`.
    #[must_use]
    pub fn attribute_list(&self) -> AttributeListState {
        if self.bits & bits::ATTR_LIST == 0 {
            AttributeListState::Absent
        } else if self.bits & bits::ATTR_LIST_MALFORMED != 0 {
            AttributeListState::Malformed
        } else if self.bits & bits::ATTR_LIST_RESIDENT != 0 {
            AttributeListState::Resident {
                complete: self.bits & bits::ATTR_LIST_INCOMPLETE == 0,
            }
        } else {
            AttributeListState::NonResident
        }
    }

    /// What the record says about a `$REPARSE_POINT`.
    #[must_use]
    pub fn reparse(&self) -> Reparse {
        if self.bits & bits::REPARSE == 0 {
            Reparse::None
        } else if self.bits & bits::REPARSE_TAG != 0 {
            Reparse::Tag(self.reparse_tag)
        } else {
            Reparse::Unknown
        }
    }

    /// Damage found while decoding the attributes.
    #[must_use]
    pub fn damage(&self) -> RecordDamage {
        RecordDamage {
            attributes_cut: self.bits & bits::CHAIN_BROKEN != 0,
            data_malformed: self.bits & bits::DATA_MALFORMED != 0,
            index_allocation_malformed: self.bits & bits::IA_MALFORMED != 0,
            name_malformed: self.bits & bits::NAME_MALFORMED != 0,
            standard_information_malformed: self.bits & bits::SI_MALFORMED != 0,
        }
    }

    /// Whether the record has at least one attribute (ALGORITHMS E1: "the
    /// base has attributes").
    #[must_use]
    pub fn has_attributes(&self) -> bool {
        self.bits & bits::HAS_ATTRIBUTES != 0
    }

    /// The key of ALGORITHMS D1: `(record size, origin)` with `origin =
    /// disk offset - number x record size` (signed), and the number. `None`
    /// for a record without a number.
    #[must_use]
    pub fn fragment_key(&self) -> Option<FragmentKey> {
        let number = self.record_number()?;
        let span = u64::from(number).checked_mul(u64::from(self.record_size()))?;
        let origin = i64::try_from(self.disk_offset)
            .ok()?
            .checked_sub(i64::try_from(span).ok()?)?;
        Some(FragmentKey {
            record_size: self.record_size(),
            origin,
            number,
        })
    }
}

/// Sort key of the fragment grouping (ALGORITHMS D1). Ordered by record
/// size, then origin, then number; equal `(record_size, origin)` means the
/// same fragment grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FragmentKey {
    /// Record size in bytes.
    pub record_size: u32,
    /// `disk offset - number x record size`; negative for fragments that
    /// hold high record numbers early on the disk.
    pub origin: i64,
    /// The record number.
    pub number: u32,
}

/// A name as stored: UTF-16 code units, possibly with unpaired surrogates.
/// Names whose units are all below 0x100 are kept with one byte per unit.
#[derive(Clone, Copy)]
pub struct StoredName<'a> {
    bytes: &'a [u8],
    wide: bool,
}

impl<'a> StoredName<'a> {
    pub(crate) fn new(bytes: &'a [u8], wide: bool) -> Self {
        StoredName { bytes, wide }
    }

    /// Number of UTF-16 code units (1 to 255 for names from a disk).
    #[must_use]
    pub fn len(&self) -> usize {
        if self.wide {
            self.bytes.len() / 2
        } else {
            self.bytes.len()
        }
    }

    /// Whether the name has no units.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The code units.
    #[must_use]
    pub fn units(&self) -> NameUnits<'a> {
        if self.wide {
            let (pairs, _) = self.bytes.as_chunks::<2>();
            NameUnits::Wide(pairs.iter())
        } else {
            NameUnits::Narrow(self.bytes.iter())
        }
    }

    /// The units collected into a vector.
    #[must_use]
    pub fn to_units(&self) -> Vec<u16> {
        self.units().collect()
    }

    /// The name as a `String`; unpaired surrogates become U+FFFD.
    #[must_use]
    pub fn to_string_lossy(&self) -> String {
        char::decode_utf16(self.units())
            .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect()
    }

    /// Whether the name consists of exactly the UTF-16 units of `s`.
    #[must_use]
    pub fn eq_str(&self, s: &str) -> bool {
        self.units().eq(s.encode_utf16())
    }
}

impl PartialEq for StoredName<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.units().eq(other.units())
    }
}

impl Eq for StoredName<'_> {}

impl std::fmt::Debug for StoredName<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.to_string_lossy())
    }
}

/// Iterator over the units of a [`StoredName`].
#[derive(Clone, Debug)]
pub enum NameUnits<'a> {
    /// One byte per unit.
    Narrow(std::slice::Iter<'a, u8>),
    /// UTF-16LE.
    Wide(std::slice::Iter<'a, [u8; 2]>),
}

impl Iterator for NameUnits<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        match self {
            NameUnits::Narrow(bytes) => bytes.next().map(|&b| u16::from(b)),
            NameUnits::Wide(pairs) => pairs.next().map(|pair| u16::from_le_bytes(*pair)),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            NameUnits::Narrow(bytes) => bytes.size_hint(),
            NameUnits::Wide(pairs) => pairs.size_hint(),
        }
    }
}

impl ExactSizeIterator for NameUnits<'_> {}

/// One `$FILE_NAME` of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameView<'a> {
    /// Reference to the directory holding this name.
    pub parent: FileRef,
    /// Namespace.
    pub namespace: Namespace,
    /// `$FILE_NAME` flags (0x10000000: directory).
    pub flags: u32,
    /// Attribute id inside the record.
    pub attr_id: u16,
    /// The name.
    pub name: StoredName<'a>,
}

impl NameView<'_> {
    /// Whether the name carries the directory flag 0x10000000.
    #[must_use]
    pub fn is_directory(&self) -> bool {
        self.flags & hds_ntfs::FILE_ATTR_DIRECTORY != 0
    }
}

/// A resident unnamed `$DATA` value: where it is inside the record. The
/// content itself is not stored (ALGORITHMS A4, H7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResidentData {
    /// Offset of the value inside the record (after fixups).
    pub value_offset: u32,
    /// Length of the value in bytes.
    pub value_len: u32,
    /// Attribute flags (resident data is never compressed, whatever they say).
    pub flags: u16,
    /// Attribute id inside the record.
    pub attr_id: u16,
}

/// One extent of a non-resident unnamed `$DATA` or `$INDEX_ALLOCATION:$I30`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NonResidentExtent<'a> {
    /// First VCN of this extent (as stored).
    pub start_vcn: u64,
    /// Last VCN as stored (a cross-check only; -1 for an empty attribute).
    pub last_vcn: i64,
    /// Allocated, real and initialized size; `Some` exactly when
    /// `start_vcn == 0` (the other extents' size fields mean nothing).
    pub sizes: Option<StreamSizes>,
    /// Attribute flags: `0x00FF` compression, `0x4000` encrypted, `0x8000`
    /// sparse.
    pub flags: u16,
    /// Attribute id inside the record.
    pub attr_id: u16,
    /// How the runlist ended: [`RunlistStatus::Complete`] when its
    /// terminator was reached; otherwise [`runs`](Self::runs) yields the
    /// valid runs before the fault.
    pub runlist_status: RunlistStatus,
    runlist: &'a [u8],
}

impl<'a> NonResidentExtent<'a> {
    /// The valid runs of the extent, decoded on the fly (no allocation).
    /// Each run satisfies the guarantees of [`hds_ntfs::Run`].
    #[must_use]
    pub fn runs(&self) -> StoredRuns<'a> {
        StoredRuns(Runs::new(self.runlist))
    }

    /// The stored mapping-pairs bytes (see the module documentation).
    #[must_use]
    pub fn runlist_bytes(&self) -> &'a [u8] {
        self.runlist
    }

    /// Total length of the valid runs in clusters.
    #[must_use]
    pub fn clusters(&self) -> u64 {
        let mut runs = Runs::new(self.runlist);
        for _ in &mut runs {}
        runs.clusters()
    }

    /// Whether a compression flag is set (non-resident, so it means
    /// compressed).
    #[must_use]
    pub fn is_compressed(&self) -> bool {
        self.flags & ATTR_FLAG_COMPRESSION_MASK != 0
    }

    /// Whether the encrypted flag is set.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.flags & ATTR_FLAG_ENCRYPTED != 0
    }

    /// Whether the sparse flag is set (a runlist can have holes without it).
    #[must_use]
    pub fn has_sparse_flag(&self) -> bool {
        self.flags & ATTR_FLAG_SPARSE != 0
    }
}

/// The runs of a stored extent.
#[derive(Clone, Debug)]
pub struct StoredRuns<'a>(Runs<'a>);

impl Iterator for StoredRuns<'_> {
    type Item = Run;

    fn next(&mut self) -> Option<Run> {
        self.0.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// One unnamed `$DATA` extent of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataExtent<'a> {
    /// The value is inside the record.
    Resident(ResidentData),
    /// The value is in clusters.
    NonResident(NonResidentExtent<'a>),
}

/// Which attribute an `$ATTRIBUTE_LIST` entry names, by its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttrListName {
    /// Unnamed (for type 0x80: the file's content).
    Unnamed,
    /// `$I30` (for type 0xA0: the directory index).
    I30,
    /// Any other name (an alternate data stream, another index).
    Other,
}

/// One entry of a resident `$ATTRIBUTE_LIST` for type 0x80 or 0xA0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AttrListEntryView {
    /// 0x80 (`$DATA`) or 0xA0 (`$INDEX_ALLOCATION`).
    pub type_code: u32,
    /// The attribute's name class.
    pub name: AttrListName,
    /// First VCN of the extent this entry points to.
    pub start_vcn: u64,
    /// The record that holds the extent.
    pub holder: FileRef,
    /// Attribute id of the extent inside that record.
    pub attr_id: u16,
}

/// Target of an extent item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExtentTarget {
    Data,
    IndexAllocation,
}

/// One decoded aux item.
#[derive(Clone, Copy, Debug)]
pub(crate) enum AuxItem<'a> {
    Name(NameView<'a>),
    Data(DataExtent<'a>),
    IndexAllocation(NonResidentExtent<'a>),
    AttrList(AttrListEntryView),
}

pub(crate) fn status_code(status: RunlistStatus) -> u8 {
    match status {
        RunlistStatus::Complete => 0,
        RunlistStatus::Invalid(RunlistError::Unterminated) => 1,
        RunlistStatus::Invalid(RunlistError::Truncated) => 2,
        RunlistStatus::Invalid(RunlistError::FieldTooLarge) => 3,
        RunlistStatus::Invalid(RunlistError::BadLength) => 4,
        RunlistStatus::Invalid(RunlistError::NegativeLcn) => 5,
        RunlistStatus::Invalid(RunlistError::Overflow) => 6,
    }
}

pub(crate) fn status_from_code(code: u8) -> Option<RunlistStatus> {
    Some(match code {
        0 => RunlistStatus::Complete,
        1 => RunlistStatus::Invalid(RunlistError::Unterminated),
        2 => RunlistStatus::Invalid(RunlistError::Truncated),
        3 => RunlistStatus::Invalid(RunlistError::FieldTooLarge),
        4 => RunlistStatus::Invalid(RunlistError::BadLength),
        5 => RunlistStatus::Invalid(RunlistError::NegativeLcn),
        6 => RunlistStatus::Invalid(RunlistError::Overflow),
        _ => return None,
    })
}

fn rd<const N: usize>(b: &[u8], at: usize) -> Option<[u8; N]> {
    b.get(at..)?.first_chunk::<N>().copied()
}

fn rd_u8(b: &[u8], at: usize) -> Option<u8> {
    b.get(at).copied()
}

fn rd_u16(b: &[u8], at: usize) -> Option<u16> {
    rd::<2>(b, at).map(u16::from_le_bytes)
}

fn rd_u32(b: &[u8], at: usize) -> Option<u32> {
    rd::<4>(b, at).map(u32::from_le_bytes)
}

fn rd_u64(b: &[u8], at: usize) -> Option<u64> {
    rd::<8>(b, at).map(u64::from_le_bytes)
}

fn rd_i64(b: &[u8], at: usize) -> Option<i64> {
    rd::<8>(b, at).map(i64::from_le_bytes)
}

/// Decodes the item at the start of `b`: the item and its encoded length.
/// `None` when the bytes are not a well-formed item.
pub(crate) fn decode_item(b: &[u8]) -> Option<(AuxItem<'_>, usize)> {
    match rd_u8(b, 0)? {
        TAG_NAME => {
            let namespace = Namespace::from_u8(rd_u8(b, 1)?)?;
            let len = usize::from(rd_u8(b, 2)?);
            let wide = match rd_u8(b, 3)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            if len == 0 {
                return None;
            }
            let unit_bytes = if wide { len.checked_mul(2)? } else { len };
            let end = NAME_HEADER.checked_add(unit_bytes)?;
            let units = b.get(NAME_HEADER..end)?;
            Some((
                AuxItem::Name(NameView {
                    parent: FileRef::from_raw(rd_u64(b, 10)?),
                    namespace,
                    flags: rd_u32(b, 6)?,
                    attr_id: rd_u16(b, 4)?,
                    name: StoredName::new(units, wide),
                }),
                end,
            ))
        }
        TAG_RESIDENT => {
            let item = ResidentData {
                attr_id: rd_u16(b, 1)?,
                flags: rd_u16(b, 3)?,
                value_offset: rd_u32(b, 5)?,
                value_len: rd_u32(b, 9)?,
            };
            (b.len() >= RESIDENT_ITEM)
                .then_some((AuxItem::Data(DataExtent::Resident(item)), RESIDENT_ITEM))
        }
        tag @ (TAG_EXTENT0 | TAG_EXTENT) => {
            let target = match rd_u8(b, 1)? {
                0 => ExtentTarget::Data,
                1 => ExtentTarget::IndexAllocation,
                _ => return None,
            };
            let attr_id = rd_u16(b, 2)?;
            let flags = rd_u16(b, 4)?;
            let runlist_status = status_from_code(rd_u8(b, 6)?)?;
            let (start_vcn, last_vcn, sizes, header) = if tag == TAG_EXTENT0 {
                (
                    0,
                    rd_i64(b, 7)?,
                    Some(StreamSizes {
                        allocated: rd_u64(b, 15)?,
                        real: rd_u64(b, 23)?,
                        initialized: rd_u64(b, 31)?,
                    }),
                    EXTENT0_HEADER,
                )
            } else {
                let start_vcn = rd_u64(b, 7)?;
                if start_vcn == 0 {
                    return None;
                }
                (start_vcn, rd_i64(b, 15)?, None, EXTENT_HEADER)
            };
            let rl_len_at = header.checked_sub(2)?;
            let rl_len = usize::from(rd_u16(b, rl_len_at)?);
            let end = header.checked_add(rl_len)?;
            let runlist = b.get(header..end)?;
            let extent = NonResidentExtent {
                start_vcn,
                last_vcn,
                sizes,
                flags,
                attr_id,
                runlist_status,
                runlist,
            };
            let item = match target {
                ExtentTarget::Data => AuxItem::Data(DataExtent::NonResident(extent)),
                ExtentTarget::IndexAllocation => AuxItem::IndexAllocation(extent),
            };
            Some((item, end))
        }
        TAG_ATTR_LIST => {
            let type_code = match rd_u8(b, 1)? {
                0 => hds_ntfs::attr_type::DATA,
                1 => hds_ntfs::attr_type::INDEX_ALLOCATION,
                _ => return None,
            };
            let name = match rd_u8(b, 2)? {
                0 => AttrListName::Unnamed,
                1 => AttrListName::I30,
                2 => AttrListName::Other,
                _ => return None,
            };
            let item = AttrListEntryView {
                type_code,
                name,
                attr_id: rd_u16(b, 3)?,
                start_vcn: rd_u64(b, 5)?,
                holder: FileRef::from_raw(rd_u64(b, 13)?),
            };
            (b.len() >= ATTR_LIST_ITEM).then_some((AuxItem::AttrList(item), ATTR_LIST_ITEM))
        }
        _ => None,
    }
}

/// Iterator over the items of one aux region. Stops at the first malformed
/// item (stored regions are validated, so that only happens to corrupt
/// data that was refused anyway).
#[derive(Clone, Debug)]
pub(crate) struct AuxItems<'a> {
    rest: &'a [u8],
}

impl<'a> AuxItems<'a> {
    pub(crate) fn new(region: &'a [u8]) -> Self {
        AuxItems { rest: region }
    }
}

impl<'a> Iterator for AuxItems<'a> {
    type Item = AuxItem<'a>;

    fn next(&mut self) -> Option<AuxItem<'a>> {
        let (item, len) = decode_item(self.rest)?;
        self.rest = self.rest.get(len..).unwrap_or(&[]);
        Some(item)
    }
}

/// Counts of the items of each kind in a region.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ItemCounts {
    pub names: u16,
    pub data: u16,
    pub ia: u16,
    pub al: u16,
}

/// Checks that `region` is a sequence of well-formed items whose runlists
/// agree with their status bytes, and returns the counts. Used when a
/// session file is loaded.
pub(crate) fn validate_region(region: &[u8]) -> Option<ItemCounts> {
    let mut counts = ItemCounts::default();
    let mut rest = region;
    while !rest.is_empty() {
        let (item, len) = decode_item(rest)?;
        match item {
            AuxItem::Name(_) => counts.names = counts.names.checked_add(1)?,
            AuxItem::Data(DataExtent::Resident(_)) => counts.data = counts.data.checked_add(1)?,
            AuxItem::Data(DataExtent::NonResident(extent)) => {
                runlist_agrees(&extent)?;
                counts.data = counts.data.checked_add(1)?;
            }
            AuxItem::IndexAllocation(extent) => {
                runlist_agrees(&extent)?;
                counts.ia = counts.ia.checked_add(1)?;
            }
            AuxItem::AttrList(_) => counts.al = counts.al.checked_add(1)?,
        }
        rest = rest.get(len..)?;
    }
    Some(counts)
}

/// The stored bytes of a complete list decode completely and are used up;
/// those of a faulty list are exactly its valid runs.
fn runlist_agrees(extent: &NonResidentExtent<'_>) -> Option<()> {
    let mut runs = Runs::new(extent.runlist);
    for _ in &mut runs {}
    let consumed_all = runs.consumed() == extent.runlist.len();
    match (extent.runlist_status, runs.status()) {
        (RunlistStatus::Complete, Some(RunlistStatus::Complete)) if consumed_all => Some(()),
        (RunlistStatus::Invalid(_), Some(RunlistStatus::Invalid(RunlistError::Unterminated)))
            if consumed_all =>
        {
            Some(())
        }
        _ => None,
    }
}

/// Builds the aux region of one record while its attributes are walked.
/// The buffers are reused from record to record.
#[derive(Debug, Default)]
pub(crate) struct AuxBuilder {
    names: Vec<u8>,
    data: Vec<u8>,
    ia: Vec<u8>,
    al: Vec<u8>,
    pub counts: ItemCounts,
    /// An item did not fit the encoding (more than 65535 items of a kind, a
    /// runlist longer than 65535 bytes); the record is then dropped.
    pub overflow: bool,
}

impl AuxBuilder {
    pub(crate) fn clear(&mut self) {
        self.names.clear();
        self.data.clear();
        self.ia.clear();
        self.al.clear();
        self.counts = ItemCounts::default();
        self.overflow = false;
    }

    fn bump(counter: &mut u16, overflow: &mut bool) -> bool {
        match counter.checked_add(1) {
            Some(next) => {
                *counter = next;
                true
            }
            None => {
                *overflow = true;
                false
            }
        }
    }

    pub(crate) fn push_name(&mut self, name: &FileName<'_>, attr_id: u16) {
        // FileName::parse guarantees 1 to 255 units; anything else is skipped.
        let Ok(len) = u8::try_from(name.name.len()) else {
            return;
        };
        if len == 0 || !Self::bump(&mut self.counts.names, &mut self.overflow) {
            return;
        }
        let narrow = name.name.units().all(|unit| unit < 0x100);
        let out = &mut self.names;
        out.push(TAG_NAME);
        out.push(name.namespace.as_u8());
        out.push(len);
        out.push(u8::from(!narrow));
        out.extend_from_slice(&attr_id.to_le_bytes());
        out.extend_from_slice(&name.flags.to_le_bytes());
        out.extend_from_slice(&name.parent.raw().to_le_bytes());
        if narrow {
            // Every unit is below 0x100, so the low byte is the whole unit.
            out.extend(name.name.units().map(|unit| unit.to_le_bytes()[0]));
        } else {
            out.extend_from_slice(name.name.as_bytes());
        }
    }

    pub(crate) fn push_resident_data(
        &mut self,
        attr_id: u16,
        flags: u16,
        value_offset: u32,
        value_len: u32,
    ) {
        if !Self::bump(&mut self.counts.data, &mut self.overflow) {
            return;
        }
        let out = &mut self.data;
        out.push(TAG_RESIDENT);
        out.extend_from_slice(&attr_id.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&value_offset.to_le_bytes());
        out.extend_from_slice(&value_len.to_le_bytes());
    }

    pub(crate) fn push_extent(
        &mut self,
        target: ExtentTarget,
        attr_id: u16,
        flags: u16,
        extent: &Extent<'_>,
    ) {
        let mut runs = extent.runs();
        for _ in &mut runs {}
        let status = runs
            .status()
            .unwrap_or(RunlistStatus::Invalid(RunlistError::Unterminated));
        let Some(runlist) = extent.runlist.get(..runs.consumed()) else {
            self.overflow = true;
            return;
        };
        let Ok(rl_len) = u16::try_from(runlist.len()) else {
            self.overflow = true;
            return;
        };
        let (counter, out) = match target {
            ExtentTarget::Data => (&mut self.counts.data, &mut self.data),
            ExtentTarget::IndexAllocation => (&mut self.counts.ia, &mut self.ia),
        };
        if !Self::bump(counter, &mut self.overflow) {
            return;
        }
        let first = extent.start_vcn == 0;
        out.push(if first { TAG_EXTENT0 } else { TAG_EXTENT });
        out.push(match target {
            ExtentTarget::Data => 0,
            ExtentTarget::IndexAllocation => 1,
        });
        out.extend_from_slice(&attr_id.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.push(status_code(status));
        if first {
            out.extend_from_slice(&extent.last_vcn.to_le_bytes());
            out.extend_from_slice(&extent.allocated_size.to_le_bytes());
            out.extend_from_slice(&extent.real_size.to_le_bytes());
            out.extend_from_slice(&extent.initialized_size.to_le_bytes());
        } else {
            out.extend_from_slice(&extent.start_vcn.to_le_bytes());
            out.extend_from_slice(&extent.last_vcn.to_le_bytes());
        }
        out.extend_from_slice(&rl_len.to_le_bytes());
        out.extend_from_slice(runlist);
    }

    pub(crate) fn push_attr_list_entry(&mut self, entry: &hds_ntfs::AttrListEntry<'_>) {
        let type_byte = match entry.type_code {
            hds_ntfs::attr_type::DATA => 0,
            hds_ntfs::attr_type::INDEX_ALLOCATION => 1,
            _ => return,
        };
        if !Self::bump(&mut self.counts.al, &mut self.overflow) {
            return;
        }
        let name = if entry.name.is_empty() {
            0
        } else if entry.name.eq_str(hds_ntfs::I30) {
            1
        } else {
            2
        };
        let out = &mut self.al;
        out.push(TAG_ATTR_LIST);
        out.push(type_byte);
        out.push(name);
        out.extend_from_slice(&entry.attr_id.to_le_bytes());
        out.extend_from_slice(&entry.start_vcn.to_le_bytes());
        out.extend_from_slice(&entry.holder.raw().to_le_bytes());
    }

    /// Total encoded length.
    pub(crate) fn len(&self) -> usize {
        self.names
            .len()
            .saturating_add(self.data.len())
            .saturating_add(self.ia.len())
            .saturating_add(self.al.len())
    }

    /// The region in canonical order, assembled in `out` (cleared first).
    pub(crate) fn assemble<'a>(&self, out: &'a mut Vec<u8>) -> &'a [u8] {
        out.clear();
        out.reserve(self.len());
        out.extend_from_slice(&self.names);
        out.extend_from_slice(&self.data);
        out.extend_from_slice(&self.ia);
        out.extend_from_slice(&self.al);
        out
    }
}
