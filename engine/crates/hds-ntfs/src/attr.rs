//! Attributes of a `FILE` record (docs/notes/ntfs-on-disk.md, sections 4,
//! 6, 8 and 11).

use core::iter::FusedIterator;

use crate::bytes::{i64_at, u16_at, u32_at, u32_from, u64_at, u8_at, usize_from};
use crate::file_name::{FileName, Times};
use crate::index::{IndexRoot, WalkEnd};
use crate::name::Utf16Name;
use crate::reference::FileRef;
use crate::runlist::{decode_runlist, DecodedRunlist, Run, Runs};

/// Attribute type codes.
pub mod attr_type {
    /// `$STANDARD_INFORMATION`: times and flags of the file.
    pub const STANDARD_INFORMATION: u32 = 0x10;
    /// `$ATTRIBUTE_LIST`: where the attributes of a multi-record file are.
    pub const ATTRIBUTE_LIST: u32 = 0x20;
    /// `$FILE_NAME`: one name of the file.
    pub const FILE_NAME: u32 = 0x30;
    /// `$OBJECT_ID`.
    pub const OBJECT_ID: u32 = 0x40;
    /// `$SECURITY_DESCRIPTOR`.
    pub const SECURITY_DESCRIPTOR: u32 = 0x50;
    /// `$VOLUME_NAME`: the label, in record 3.
    pub const VOLUME_NAME: u32 = 0x60;
    /// `$VOLUME_INFORMATION`.
    pub const VOLUME_INFORMATION: u32 = 0x70;
    /// `$DATA`: file content (unnamed) or an alternate stream (named).
    pub const DATA: u32 = 0x80;
    /// `$INDEX_ROOT`: root node of an index.
    pub const INDEX_ROOT: u32 = 0x90;
    /// `$INDEX_ALLOCATION`: the `INDX` blocks of an index.
    pub const INDEX_ALLOCATION: u32 = 0xA0;
    /// `$BITMAP`.
    pub const BITMAP: u32 = 0xB0;
    /// `$REPARSE_POINT`.
    pub const REPARSE_POINT: u32 = 0xC0;
    /// `$EA_INFORMATION`.
    pub const EA_INFORMATION: u32 = 0xD0;
    /// `$EA`.
    pub const EA: u32 = 0xE0;
    /// `$LOGGED_UTILITY_STREAM`.
    pub const LOGGED_UTILITY_STREAM: u32 = 0x100;
    /// The marker that ends the attributes of a record. The four bytes
    /// behind it are not a length.
    pub const END: u32 = 0xFFFF_FFFF;
}

/// Attribute flag mask: any bit set means the attribute is compressed, if
/// it is non-resident (`0x0001` = LZNT1).
pub const ATTR_FLAG_COMPRESSION_MASK: u16 = 0x00FF;
/// Attribute flag: encrypted (EFS).
pub const ATTR_FLAG_ENCRYPTED: u16 = 0x4000;
/// Attribute flag: sparse.
pub const ATTR_FLAG_SPARSE: u16 = 0x8000;

/// Name of the directory index attributes (`$INDEX_ROOT`,
/// `$INDEX_ALLOCATION` and their `$BITMAP`).
pub const I30: &str = "$I30";

/// Name of the `$DATA` stream that holds the content of a WOF-compressed
/// file; its unnamed `$DATA` is one large hole.
pub const WOF_COMPRESSED_DATA: &str = "WofCompressedData";

/// Reparse tag: mount point or junction.
pub const REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// Reparse tag: symbolic link.
pub const REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// Reparse tag: Data Deduplication stub; the content is not in the file.
pub const REPARSE_TAG_DEDUP: u32 = 0x8000_0013;
/// Reparse tag: WOF-compressed file ("compact /exe", CompactOS); the
/// content is in a named `$DATA` stream `WofCompressedData`.
pub const REPARSE_TAG_WOF: u32 = 0x8000_0017;
/// Reparse tag of cloud placeholders (OneDrive) after masking with
/// [`REPARSE_TAG_CLOUD_MASK`].
pub const REPARSE_TAG_CLOUD: u32 = 0x9000_001A;
/// Mask under which all cloud placeholder tags (`0x9000001A`,
/// `0x9000101A` ... `0x9000F01A`) equal [`REPARSE_TAG_CLOUD`].
pub const REPARSE_TAG_CLOUD_MASK: u32 = 0xFFFF_0FFF;

/// Length of the header of a resident attribute.
const RESIDENT_HEADER_LEN: usize = 0x18;
/// Length of the header of a non-resident attribute without the optional
/// total-allocated-size field.
const NON_RESIDENT_HEADER_LEN: usize = 0x40;
/// Length of the header of a non-resident attribute with that field.
const NON_RESIDENT_HEADER_EX_LEN: usize = 0x48;
/// Smallest `$STANDARD_INFORMATION` value (NTFS 1.2 layout).
const STANDARD_INFORMATION_MIN_LEN: usize = 48;
/// Fixed part of an `$ATTRIBUTE_LIST` entry.
const ATTR_LIST_ENTRY_MIN_LEN: usize = 0x1A;

/// One attribute of a `FILE` record.
///
/// The header fields common to all attributes are decoded; the rest is
/// borrowed from the record and decoded by the typed accessors on demand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attribute<'a> {
    /// Type code; see [`attr_type`].
    pub type_code: u32,
    /// Offset of the attribute inside the record.
    pub offset: u32,
    /// Length of the attribute record.
    pub length: u32,
    /// Flags (u16 at 0x0C): [`ATTR_FLAG_COMPRESSION_MASK`],
    /// [`ATTR_FLAG_ENCRYPTED`], [`ATTR_FLAG_SPARSE`].
    pub flags: u16,
    /// Attribute id ("instance"), unique inside its record.
    pub id: u16,
    /// Name of the attribute, empty for an unnamed one.
    pub name: Utf16Name<'a>,
    /// The resident value or the non-resident extent.
    pub body: AttrBody<'a>,
}

/// Where an attribute keeps its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrBody<'a> {
    /// The value is inside the record.
    Resident(ResidentValue<'a>),
    /// The value is in clusters described by a runlist. One record holds
    /// one extent of it, not necessarily the first.
    NonResident(Extent<'a>),
    /// The header contradicts itself: a name, value or runlist outside the
    /// attribute, or a form byte that is neither 0 nor 1. The attribute's
    /// length was plausible, so the walk continues behind it, but nothing
    /// of its content is offered. For a `$DATA` attribute this means
    /// "content location unknown", not "empty file".
    Malformed,
}

/// The value of a resident attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResidentValue<'a> {
    /// Offset of the value inside the record.
    pub offset: u32,
    /// The value, borrowed from the record (valid because fixups have been
    /// applied: a value can lie across the end of a 512-byte block).
    pub bytes: &'a [u8],
}

impl ResidentValue<'_> {
    /// Length of the value in bytes.
    #[inline]
    #[must_use]
    pub fn len(&self) -> u32 {
        u32_from(self.bytes.len())
    }

    /// Whether the value is empty.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// The three sizes of a non-resident attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamSizes {
    /// Bytes allocated: clusters times cluster size (for a sparse or
    /// compressed attribute: of all VCNs, allocated or not).
    pub allocated: u64,
    /// Length of the value in bytes.
    pub real: u64,
    /// Bytes that have been written ("valid data length"). The bytes from
    /// here to `real` read as zeros whatever the clusters hold.
    pub initialized: u64,
}

/// One extent of a non-resident attribute: the header fields and the
/// runlist of the VCN range that this record describes.
///
/// All numbers are as stored and not checked against each other; treat
/// them as untrusted and use checked arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Extent<'a> {
    /// First VCN of this extent (the stored value read as unsigned).
    /// Non-zero only for the further extents of an attribute that is
    /// spread over several records.
    pub start_vcn: u64,
    /// Last VCN of this extent **as stored** (signed): -1 for an empty
    /// attribute, possibly 0 in old single-extent attributes. Only a
    /// cross-check; the runs say how long the extent is.
    pub last_vcn: i64,
    /// Compression unit, log2 clusters (u8 at 0x22). Not a compression
    /// indicator: it is set on sparse, uncompressed attributes too.
    pub compression_unit: u8,
    /// Allocated size as stored. Meaningful only when `start_vcn == 0`;
    /// see [`sizes`](Extent::sizes).
    pub allocated_size: u64,
    /// Real size as stored. Meaningful only when `start_vcn == 0`.
    pub real_size: u64,
    /// Initialized size as stored. Meaningful only when `start_vcn == 0`.
    pub initialized_size: u64,
    /// Bytes really allocated, for sparse and compressed attributes: the
    /// extra field of the 0x48-byte header. `None` when the header is the
    /// 0x40-byte form (the name or the runlist starts before 0x48).
    pub total_allocated: Option<u64>,
    /// The runlist bytes: from the mapping pairs offset to the end of the
    /// attribute. The list ends at its terminator; what follows is padding.
    pub runlist: &'a [u8],
}

impl<'a> Extent<'a> {
    /// A decoder over the runs of this extent. Allocates nothing.
    #[inline]
    #[must_use]
    pub const fn runs(&self) -> Runs<'a> {
        Runs::new(self.runlist)
    }

    /// Appends the runs of this extent to `out`; see [`decode_runlist`].
    #[inline]
    pub fn decode_runs(&self, out: &mut Vec<Run>) -> DecodedRunlist {
        decode_runlist(self.runlist, out)
    }

    /// The sizes of the attribute, which only the extent with start VCN 0
    /// carries; `None` in every other extent (their size fields are not
    /// maintained).
    #[inline]
    #[must_use]
    pub const fn sizes(&self) -> Option<StreamSizes> {
        if self.start_vcn == 0 {
            Some(StreamSizes {
                allocated: self.allocated_size,
                real: self.real_size,
                initialized: self.initialized_size,
            })
        } else {
            None
        }
    }
}

/// The fields of `$STANDARD_INFORMATION` a recovery needs. The value is 48
/// bytes (NTFS 1.2 layout, still written by `format` and ntfs-3g) or 72
/// bytes; both are accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StandardInformation {
    /// The four timestamps (0x00..0x20).
    pub times: Times,
    /// File attribute flags (u32 at 0x20); see the `FILE_ATTR_*` constants.
    /// The directory bit is not here: do not derive "is a directory" from
    /// this attribute.
    pub flags: u32,
}

/// One entry of an `$ATTRIBUTE_LIST`: where one extent of one attribute of
/// the file is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AttrListEntry<'a> {
    /// Type code of the attribute.
    pub type_code: u32,
    /// Name of the attribute, empty when unnamed.
    pub name: Utf16Name<'a>,
    /// First VCN of the extent (0 unless a non-resident attribute is split).
    pub start_vcn: u64,
    /// The record that holds the extent: the base record itself or one of
    /// its extension records.
    pub holder: FileRef,
    /// Attribute id of the extent inside that record.
    pub attr_id: u16,
}

/// Iterator over the entries of an `$ATTRIBUTE_LIST` value.
///
/// Per entry: type (u32 at 0x00), entry length (u16 at 0x04, at least
/// 0x1A), name length (u8 at 0x06) and name offset (u8 at 0x07), start VCN
/// (u64 at 0x08), holder reference (u64 at 0x10), attribute id (u16 at
/// 0x18). The walk is bounded by the value and advances by at least 26
/// bytes per entry.
#[derive(Clone, Debug)]
pub struct AttrListEntries<'a> {
    bytes: &'a [u8],
    pos: usize,
    end: Option<WalkEnd>,
}

impl<'a> AttrListEntries<'a> {
    /// Walks the entries in `value`: the value of a resident
    /// `$ATTRIBUTE_LIST`, or the content of a non-resident one that the
    /// caller has read (cut to its real size).
    #[inline]
    #[must_use]
    pub const fn new(value: &'a [u8]) -> Self {
        AttrListEntries {
            bytes: value,
            pos: 0,
            end: None,
        }
    }

    /// How the walk ended; `None` while the iterator has not returned
    /// `None` yet. [`WalkEnd::Complete`] means the entries filled the value
    /// exactly.
    #[inline]
    #[must_use]
    pub const fn end(&self) -> Option<WalkEnd> {
        self.end
    }

    fn step(&mut self) -> Result<AttrListEntry<'a>, WalkEnd> {
        let rest = self.bytes.get(self.pos..).ok_or(WalkEnd::Broken)?;
        if rest.is_empty() {
            return Err(WalkEnd::Complete);
        }
        let length = usize::from(u16_at(rest, 0x04).ok_or(WalkEnd::Broken)?);
        if length < ATTR_LIST_ENTRY_MIN_LEN {
            return Err(WalkEnd::Broken);
        }
        let entry = rest.get(..length).ok_or(WalkEnd::Broken)?;
        let name_units = usize::from(u8_at(entry, 0x06).ok_or(WalkEnd::Broken)?);
        let name = if name_units == 0 {
            Utf16Name::EMPTY
        } else {
            let start = usize::from(u8_at(entry, 0x07).ok_or(WalkEnd::Broken)?);
            let end = name_units
                .checked_mul(2)
                .and_then(|bytes| bytes.checked_add(start))
                .ok_or(WalkEnd::Broken)?;
            if start < ATTR_LIST_ENTRY_MIN_LEN {
                return Err(WalkEnd::Broken);
            }
            Utf16Name::from_bytes(entry.get(start..end).ok_or(WalkEnd::Broken)?)
        };
        let parsed = AttrListEntry {
            type_code: u32_at(entry, 0x00).ok_or(WalkEnd::Broken)?,
            name,
            start_vcn: u64_at(entry, 0x08).ok_or(WalkEnd::Broken)?,
            holder: FileRef::from_raw(u64_at(entry, 0x10).ok_or(WalkEnd::Broken)?),
            attr_id: u16_at(entry, 0x18).ok_or(WalkEnd::Broken)?,
        };
        self.pos = self.pos.saturating_add(length);
        Ok(parsed)
    }
}

impl<'a> Iterator for AttrListEntries<'a> {
    type Item = AttrListEntry<'a>;

    fn next(&mut self) -> Option<AttrListEntry<'a>> {
        if self.end.is_some() {
            return None;
        }
        match self.step() {
            Ok(entry) => Some(entry),
            Err(end) => {
                self.end = Some(end);
                None
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.end.is_some() {
            return (0, Some(0));
        }
        let left = self.bytes.len().saturating_sub(self.pos);
        (0, Some(left / ATTR_LIST_ENTRY_MIN_LEN))
    }
}

impl FusedIterator for AttrListEntries<'_> {}

/// An `$ATTRIBUTE_LIST`, in either form.
#[derive(Clone, Debug)]
pub enum AttributeList<'a> {
    /// The list is inside the record: its entries.
    Resident(AttrListEntries<'a>),
    /// The list is stored in clusters; its runlist is always in the base
    /// record. Read `real_size` bytes through the runs and walk them with
    /// [`AttrListEntries::new`].
    NonResident(Extent<'a>),
}

impl<'a> Attribute<'a> {
    /// Whether the attribute's name is exactly `name` (`""` for unnamed).
    #[inline]
    #[must_use]
    pub fn is_named(&self, name: &str) -> bool {
        self.name.eq_str(name)
    }

    /// Whether the attribute is unnamed (for `$DATA`: the file's content).
    #[inline]
    #[must_use]
    pub const fn is_unnamed(&self) -> bool {
        self.name.is_empty()
    }

    /// The resident value, when the attribute is resident and well formed.
    #[inline]
    #[must_use]
    pub const fn resident(&self) -> Option<ResidentValue<'a>> {
        match self.body {
            AttrBody::Resident(value) => Some(value),
            _ => None,
        }
    }

    /// The extent, when the attribute is non-resident and well formed.
    #[inline]
    #[must_use]
    pub const fn extent(&self) -> Option<Extent<'a>> {
        match self.body {
            AttrBody::NonResident(extent) => Some(extent),
            _ => None,
        }
    }

    /// Whether the header of the attribute contradicts itself; see
    /// [`AttrBody::Malformed`].
    #[inline]
    #[must_use]
    pub const fn is_malformed(&self) -> bool {
        matches!(self.body, AttrBody::Malformed)
    }

    /// Whether the content is stored compressed: the attribute is
    /// non-resident **and** a compression flag is set. A resident value is
    /// always stored raw, whatever its flags say, and the compression unit
    /// never decides.
    #[inline]
    #[must_use]
    pub const fn is_compressed(&self) -> bool {
        matches!(self.body, AttrBody::NonResident(_))
            && self.flags & ATTR_FLAG_COMPRESSION_MASK != 0
    }

    /// Whether the encrypted flag is set.
    #[inline]
    #[must_use]
    pub const fn is_encrypted(&self) -> bool {
        self.flags & ATTR_FLAG_ENCRYPTED != 0
    }

    /// Whether the sparse flag is set. A runlist can have holes without it.
    #[inline]
    #[must_use]
    pub const fn is_sparse(&self) -> bool {
        self.flags & ATTR_FLAG_SPARSE != 0
    }

    /// Decodes the attribute as `$STANDARD_INFORMATION`: type 0x10,
    /// resident, at least 48 bytes.
    #[must_use]
    pub fn standard_information(&self) -> Option<StandardInformation> {
        if self.type_code != attr_type::STANDARD_INFORMATION {
            return None;
        }
        let value = self.resident()?.bytes;
        if value.len() < STANDARD_INFORMATION_MIN_LEN {
            return None;
        }
        Some(StandardInformation {
            times: Times::read(value, 0x00)?,
            flags: u32_at(value, 0x20)?,
        })
    }

    /// Decodes the attribute as `$FILE_NAME`: type 0x30, resident, a value
    /// that [`FileName::parse`] accepts.
    #[must_use]
    pub fn file_name(&self) -> Option<FileName<'a>> {
        if self.type_code != attr_type::FILE_NAME {
            return None;
        }
        FileName::parse(self.resident()?.bytes)
    }

    /// Decodes the attribute as `$ATTRIBUTE_LIST` (type 0x20): the entries
    /// of a resident list, or the extent of a non-resident one. `None` for
    /// other types and for a malformed attribute.
    #[must_use]
    pub fn attribute_list(&self) -> Option<AttributeList<'a>> {
        if self.type_code != attr_type::ATTRIBUTE_LIST {
            return None;
        }
        match self.body {
            AttrBody::Resident(value) => {
                Some(AttributeList::Resident(AttrListEntries::new(value.bytes)))
            }
            AttrBody::NonResident(extent) => Some(AttributeList::NonResident(extent)),
            AttrBody::Malformed => None,
        }
    }

    /// Decodes the attribute as `$INDEX_ROOT`: type 0x90, resident, a value
    /// that [`IndexRoot::parse`] accepts. The name is not checked: a
    /// directory index is the one named [`I30`].
    #[must_use]
    pub fn index_root(&self) -> Option<IndexRoot<'a>> {
        if self.type_code != attr_type::INDEX_ROOT {
            return None;
        }
        let value = self.resident()?;
        IndexRoot::parse(value.bytes, value.offset)
    }

    /// The volume label, when the attribute is a resident `$VOLUME_NAME`
    /// (type 0x60).
    #[must_use]
    pub fn volume_name(&self) -> Option<Utf16Name<'a>> {
        if self.type_code != attr_type::VOLUME_NAME {
            return None;
        }
        Some(Utf16Name::from_bytes(self.resident()?.bytes))
    }

    /// The reparse tag, when the attribute is a resident `$REPARSE_POINT`
    /// (type 0xC0) of at least 4 bytes. For a non-resident reparse point
    /// the tag is in the first cluster of the value: this returns `None`
    /// and [`extent`](Attribute::extent) says where to read.
    #[must_use]
    pub fn reparse_tag(&self) -> Option<u32> {
        if self.type_code != attr_type::REPARSE_POINT {
            return None;
        }
        u32_at(self.resident()?.bytes, 0)
    }
}

/// One step of the walk over the attributes of a record.
pub(crate) enum Step<'a> {
    /// An attribute, and the position of the next one.
    Attr(Attribute<'a>, usize),
    /// The end marker.
    End,
    /// No attribute can start here: the bytes end, or the length field is
    /// too small, not a multiple of 8, or reaches past the used size.
    Broken,
}

/// The length of the attribute at `pos` when it is one the walk can step
/// over: at least a resident header, a multiple of 8, inside `rec`.
/// `Ok(None)` for the end marker.
#[inline]
pub(crate) fn attr_len(rec: &[u8], pos: usize) -> Result<Option<usize>, ()> {
    let rest = rec.get(pos..).ok_or(())?;
    // The end marker needs its 4 type bytes only: what follows it is not a
    // length (Windows writes 0x11477982 there).
    if u32_at(rest, 0).ok_or(())? == attr_type::END {
        return Ok(None);
    }
    let length = usize_from(u32_at(rest, 4).ok_or(())?);
    if length < RESIDENT_HEADER_LEN || !length.is_multiple_of(8) || length > rest.len() {
        return Err(());
    }
    Ok(Some(length))
}

/// Parses the attribute at `pos` of `rec`, the record cut at its used size.
pub(crate) fn step(rec: &[u8], pos: usize) -> Step<'_> {
    let length = match attr_len(rec, pos) {
        Ok(Some(length)) => length,
        Ok(None) => return Step::End,
        Err(()) => return Step::Broken,
    };
    let (Some(end), Some(header)) = (
        pos.checked_add(length),
        rec.get(pos..).and_then(|rest| rest.get(..length)),
    ) else {
        return Step::Broken;
    };
    let Some(fixed) = header.first_chunk::<RESIDENT_HEADER_LEN>() else {
        return Step::Broken;
    };
    let (name, body) = parse_name_and_body(header, u32_from(pos));
    Step::Attr(
        Attribute {
            type_code: u32::from_le_bytes([fixed[0], fixed[1], fixed[2], fixed[3]]),
            offset: u32_from(pos),
            length: u32_from(length),
            flags: u16::from_le_bytes([fixed[0x0C], fixed[0x0D]]),
            id: u16::from_le_bytes([fixed[0x0E], fixed[0x0F]]),
            name,
            body,
        },
        end,
    )
}

/// Name and body of the attribute record `a`, which starts at `pos` in its
/// record. A header that contradicts itself gives [`AttrBody::Malformed`];
/// the name is kept when the fault is in the value or runlist part only.
#[inline]
fn parse_name_and_body(a: &[u8], pos: u32) -> (Utf16Name<'_>, AttrBody<'_>) {
    let malformed = (Utf16Name::EMPTY, AttrBody::Malformed);
    let non_resident = match u8_at(a, 0x08) {
        Some(0) => false,
        Some(1) => true,
        _ => return malformed,
    };
    let header_len = if non_resident {
        NON_RESIDENT_HEADER_LEN
    } else {
        RESIDENT_HEADER_LEN
    };
    if a.len() < header_len {
        return malformed;
    }
    let Some((name, name_start)) = parse_name(a, header_len) else {
        return malformed;
    };
    let body = if non_resident {
        parse_extent(a, name_start).map(AttrBody::NonResident)
    } else {
        parse_resident(a, pos).map(AttrBody::Resident)
    };
    (name, body.unwrap_or(AttrBody::Malformed))
}

/// The name of the attribute record `a` and where it starts, located only
/// through the name offset field. That field means something only when
/// there is a name: Windows writes 0 there for about half of the unnamed
/// attributes.
#[inline]
fn parse_name(a: &[u8], header_len: usize) -> Option<(Utf16Name<'_>, Option<usize>)> {
    let name_units = usize::from(u8_at(a, 0x09)?);
    if name_units == 0 {
        return Some((Utf16Name::EMPTY, None));
    }
    let start = usize::from(u16_at(a, 0x0A)?);
    let end = name_units.checked_mul(2)?.checked_add(start)?;
    if start < header_len {
        return None;
    }
    Some((Utf16Name::from_bytes(a.get(start..end)?), Some(start)))
}

/// The non-resident part of the attribute record `a`. The header is 0x40
/// or 0x48 bytes long; the runlist is wherever the mapping pairs offset
/// says.
#[inline]
fn parse_extent(a: &[u8], name_start: Option<usize>) -> Option<Extent<'_>> {
    let runlist_start = usize::from(u16_at(a, 0x20)?);
    if runlist_start < NON_RESIDENT_HEADER_LEN {
        return None;
    }
    let runlist = a.get(runlist_start..)?;
    let first_content = name_start.map_or(runlist_start, |name| name.min(runlist_start));
    let total_allocated = if first_content >= NON_RESIDENT_HEADER_EX_LEN {
        Some(u64_at(a, 0x40)?)
    } else {
        None
    };
    Some(Extent {
        start_vcn: u64_at(a, 0x10)?,
        last_vcn: i64_at(a, 0x18)?,
        compression_unit: u8_at(a, 0x22)?,
        allocated_size: u64_at(a, 0x28)?,
        real_size: u64_at(a, 0x30)?,
        initialized_size: u64_at(a, 0x38)?,
        total_allocated,
        runlist,
    })
}

/// The resident part of the attribute record `a`, which starts at `pos` in
/// its record.
#[inline]
fn parse_resident(a: &[u8], pos: u32) -> Option<ResidentValue<'_>> {
    let value_len = usize_from(u32_at(a, 0x10)?);
    let value_start = usize::from(u16_at(a, 0x14)?);
    let value_end = value_start.checked_add(value_len)?;
    if value_len != 0 && value_start < RESIDENT_HEADER_LEN {
        return None;
    }
    Some(ResidentValue {
        offset: pos.checked_add(u32_from(value_start))?,
        bytes: a.get(value_start..value_end)?,
    })
}

/// Iterator over the attributes of a `FILE` record, in on-disk order.
///
/// Each attribute is located through its own length field, starting at the
/// record's first-attribute offset, until the end marker or the first
/// position where no attribute can start. The walk never leaves the used
/// part of the record and advances by at least 24 bytes per attribute.
#[derive(Clone, Debug)]
pub struct Attributes<'a> {
    rec: &'a [u8],
    pos: usize,
    done: bool,
}

impl<'a> Attributes<'a> {
    /// `rec` is the record cut at its used size, `pos` the offset of the
    /// first attribute.
    #[inline]
    pub(crate) const fn new(rec: &'a [u8], pos: usize) -> Self {
        Attributes {
            rec,
            pos,
            done: false,
        }
    }
}

impl<'a> Iterator for Attributes<'a> {
    type Item = Attribute<'a>;

    #[inline]
    fn next(&mut self) -> Option<Attribute<'a>> {
        if self.done {
            return None;
        }
        match step(self.rec, self.pos) {
            Step::Attr(attribute, next) => {
                self.pos = next;
                Some(attribute)
            }
            Step::End | Step::Broken => {
                self.done = true;
                None
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.done {
            return (0, Some(0));
        }
        let left = self.rec.len().saturating_sub(self.pos);
        (0, Some(left / RESIDENT_HEADER_LEN))
    }
}

impl FusedIterator for Attributes<'_> {}
