//! Descriptions of the structures a builder wrote: one fact per `FILE`
//! record, `INDX` block and boot sector.
//!
//! Facts are derived from the *model* a structure was encoded from, never by
//! parsing the image. They are what the manifest lists under `records`,
//! `index_blocks` and `boot_sectors`, and what the cross-check test compares
//! field by field with an independent decoder's reading of the image.

use serde::{Deserialize, Serialize};

use crate::ondisk::index::IndexEntry;
use crate::ondisk::record::{attr_type, AttrBody, HeaderForm, RecordModel};
use crate::ondisk::runlist::Run;
use crate::ondisk::FileRef;

/// Why a record exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordRole {
    /// Base record of a file or folder.
    Base,
    /// Extension record of a file or folder.
    Extension,
    /// A formatted slot that was never used.
    Blank,
    /// A record a scenario placed by hand.
    Raw,
    /// A copy inside `$MFTMirr`.
    Mirror,
    /// A copy somewhere else on the disk (shadow-copy block, old MFT
    /// cluster, …).
    StaleCopy,
    /// An image of a record as `$LogFile` keeps it: every 512-byte block
    /// fails its fixup check (ALGORITHMS A3 drops it).
    LoggedImage,
}

/// What became of a structure by the time the image was finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactState {
    /// Every byte is as written.
    Intact,
    /// Its first sector (and with it the magic) is as written but later
    /// bytes were overwritten, or the structure is split over a fragment
    /// boundary of the file that contains it. A scanner still sees the
    /// magic; what it decodes is not described by the fact.
    Damaged,
}

/// One `$FILE_NAME` attribute or index key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameFact {
    /// The name with unpaired surrogates replaced by U+FFFD.
    pub name: String,
    /// The exact UTF-16 units.
    pub name_utf16: Vec<u16>,
    /// 0 POSIX, 1 Win32, 2 DOS, 3 Win32 + DOS.
    pub namespace: u8,
    /// Parent directory reference.
    pub parent: FileRef,
    /// Creation, modification, MFT change and access time of the name copy.
    pub times: [u64; 4],
    /// Allocated size stored with the name.
    pub allocated_size: u64,
    /// Real size stored with the name.
    pub real_size: u64,
    /// File attribute flags stored with the name (`0x10000000` = directory).
    pub flags: u32,
    /// The field at 0x3C (EA size or reparse tag).
    pub ea_or_reparse: u32,
    /// Attribute id (0 for index keys).
    pub attr_id: u16,
}

/// One `$DATA` or `$INDEX_ALLOCATION` attribute extent as stored in a record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamFact {
    /// Attribute type (0x80 or 0xA0).
    pub type_code: u32,
    /// Attribute name ("" for the unnamed stream).
    pub name: String,
    /// Attribute flags.
    pub flags: u16,
    /// Attribute id.
    pub attr_id: u16,
    /// Offset of the attribute header inside the record.
    pub attr_offset: u32,
    /// Resident value: offset inside the record and length.
    pub resident: Option<(u32, u32)>,
    /// Non-resident extent.
    pub non_resident: Option<ExtentFact>,
}

/// The non-resident part of a [`StreamFact`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtentFact {
    /// Lowest VCN as stored.
    pub lowest_vcn: i64,
    /// Highest VCN as stored.
    pub highest_vcn: i64,
    /// The runs of this extent.
    pub runs: Vec<Run>,
    /// Allocated size as stored (0 in extents that do not start at VCN 0).
    pub allocated_size: u64,
    /// Real size as stored.
    pub real_size: u64,
    /// Initialized size as stored.
    pub initialized_size: u64,
    /// The extra size field of the 0x48-byte header, if the header has it.
    pub compressed_size: Option<u64>,
    /// Compression unit as stored.
    pub compression_unit: u8,
}

/// One entry of a resident `$ATTRIBUTE_LIST`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListEntryFact {
    /// Attribute type.
    pub type_code: u32,
    /// Attribute name.
    pub name: String,
    /// Lowest VCN of the extent.
    pub lowest_vcn: u64,
    /// Record holding the extent.
    pub holder: FileRef,
    /// Attribute id inside that record.
    pub attr_id: u16,
}

/// The `$ATTRIBUTE_LIST` of a record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListFact {
    /// `true` when the list is stored in the record.
    pub resident: bool,
    /// The entries. Also filled for a non-resident list (whose bytes are in
    /// `runs`), although a record parser cannot see them there.
    pub entries: Vec<ListEntryFact>,
    /// Runs of a non-resident list.
    pub runs: Vec<Run>,
}

/// One entry of a directory index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntryFact {
    /// The indexed file.
    pub child: FileRef,
    /// Its name as stored in the entry.
    pub key: NameFact,
    /// VCN of the sub-node below this entry.
    pub subnode: Option<u64>,
}

/// One `FILE` record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordFact {
    /// Absolute byte offset in the image.
    pub offset: u64,
    /// Record size in bytes.
    pub size: u32,
    /// Instance (file system) the record belongs to.
    pub instance: u32,
    /// MFT slot the record occupies in its instance. For copies: the slot of
    /// the original.
    pub slot: u64,
    /// Why the record exists.
    pub role: RecordRole,
    /// Manifest entry (file or folder) the record belongs to.
    pub entry: Option<u32>,
    /// Record number stored in the header; `None` for the pre-3.1 header.
    /// Usually equal to `slot`; scenarios with lying records differ.
    pub number: Option<u64>,
    /// Sequence number.
    pub sequence: u16,
    /// Header flags (`0x1` in use, `0x2` directory).
    pub flags: u16,
    /// Link count.
    pub link_count: u16,
    /// `$LogFile` sequence number.
    pub lsn: u64,
    /// Base record reference (zero in base records).
    pub base: FileRef,
    /// Bytes in use.
    pub bytes_in_use: u32,
    /// Update sequence number.
    pub usn: u16,
    /// 512-byte blocks whose fixup is deliberately broken.
    pub torn_blocks: Vec<u32>,
    /// Types of all attributes in on-disk order.
    pub attr_types: Vec<u32>,
    /// `$STANDARD_INFORMATION`: times (creation, modification, MFT change,
    /// access), file attributes, value length (48 or 72).
    pub standard_information: Option<([u64; 4], u32, u32)>,
    /// Every `$FILE_NAME` in on-disk order.
    pub names: Vec<NameFact>,
    /// Every `$DATA` extent (named streams included) in on-disk order.
    pub data: Vec<StreamFact>,
    /// Every `$INDEX_ALLOCATION` extent.
    pub index_allocation: Vec<StreamFact>,
    /// Entries of the `$INDEX_ROOT:$I30`, without the end entry.
    pub index_root: Option<Vec<IndexEntryFact>>,
    /// The `$ATTRIBUTE_LIST`.
    pub attribute_list: Option<ListFact>,
    /// Tag of the `$REPARSE_POINT`.
    pub reparse_tag: Option<u32>,
    /// FNV-1a 64 of the record bytes as they lie on disk (before fixups are
    /// undone): the `hash` of ALGORITHMS A3.
    pub fnv1a64: u64,
    /// State in the finished image.
    pub state: ArtifactState,
    /// What a correct scan and catalog make of the record (class, catalog
    /// instance, generation mark). Unlike every other field this is an
    /// expectation, filled in when the manifest is finished.
    pub expect: crate::manifest::RecordExpect,
}

/// One `INDX` block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexBlockFact {
    /// Absolute byte offset in the image.
    pub offset: u64,
    /// Block size in bytes.
    pub size: u32,
    /// Instance the block belongs to.
    pub instance: u32,
    /// The directory that owns the block (its base record).
    pub owner: FileRef,
    /// Manifest entry of that directory.
    pub owner_entry: Option<u32>,
    /// The block's VCN as stored at 0x10.
    pub vcn: u64,
    /// `$LogFile` sequence number.
    pub lsn: u64,
    /// `true` when the block holds no sub-node pointers.
    pub leaf: bool,
    /// The entries, without the end entry.
    pub entries: Vec<IndexEntryFact>,
    /// `true` for a copy placed outside the directory's index allocation.
    pub stale_copy: bool,
    /// State in the finished image.
    pub state: ArtifactState,
}

/// One NTFS boot sector.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootFact {
    /// Absolute byte offset in the image.
    pub offset: u64,
    /// Instance the sector belongs to.
    pub instance: u32,
    /// `true` for the copy in the last sector of the partition.
    pub backup: bool,
    /// Bytes per sector.
    pub bytes_per_sector: u32,
    /// Cluster size in bytes.
    pub cluster_size: u32,
    /// Total sectors of the volume.
    pub total_sectors: u64,
    /// `$MFT` start cluster.
    pub mft_lcn: u64,
    /// `$MFTMirr` start cluster.
    pub mftmirr_lcn: u64,
    /// MFT record size in bytes.
    pub record_size: u32,
    /// Index block size in bytes.
    pub index_block_size: u32,
    /// Hidden sectors.
    pub hidden_sectors: u32,
    /// Serial number.
    pub serial: u64,
    /// State in the finished image.
    pub state: ArtifactState,
}

/// Lossy display form of a UTF-16 name.
pub fn lossy(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// Reads back a `$FILE_NAME` value produced by [`crate::ondisk::attr::file_name`].
/// `None` when the bytes are not such a value (hostile records).
pub(crate) fn name_fact(value: &[u8], attr_id: u16) -> Option<NameFact> {
    if value.len() < 0x42 {
        return None;
    }
    let len = value[0x40] as usize;
    if value.len() < 0x42 + 2 * len || value[0x41] > 3 {
        return None;
    }
    let units: Vec<u16> = (0..len).map(|i| u16_at(value, 0x42 + 2 * i)).collect();
    Some(NameFact {
        name: lossy(&units),
        name_utf16: units,
        namespace: value[0x41],
        parent: FileRef::from_raw(u64_at(value, 0)),
        times: [u64_at(value, 0x08), u64_at(value, 0x10), u64_at(value, 0x18), u64_at(value, 0x20)],
        allocated_size: u64_at(value, 0x28),
        real_size: u64_at(value, 0x30),
        flags: u32_at(value, 0x38),
        ea_or_reparse: u32_at(value, 0x3C),
        attr_id,
    })
}

pub(crate) fn index_entry_facts(entries: &[IndexEntry]) -> Vec<IndexEntryFact> {
    entries
        .iter()
        .filter(|e| !e.last)
        .filter_map(|e| Some(IndexEntryFact { child: e.child, key: name_fact(&e.key, 0)?, subnode: e.subnode }))
        .collect()
}

/// Describes a record model. Offset, instance, slot, role and entry are the
/// caller's knowledge; `bytes` is the encoded record.
pub(crate) fn record_fact(model: &RecordModel, bytes: &[u8]) -> RecordFact {
    let mut fact = RecordFact {
        offset: 0,
        size: model.record_size,
        instance: 0,
        slot: 0,
        role: RecordRole::Raw,
        entry: None,
        number: match model.header {
            HeaderForm::V31 { number, .. } => Some(number as u64),
            HeaderForm::V30 => None,
        },
        sequence: model.sequence,
        flags: model.flags,
        link_count: model.link_count,
        lsn: model.lsn,
        base: model.base,
        bytes_in_use: model.bytes_in_use() as u32,
        usn: model.usn,
        torn_blocks: model.torn_blocks.iter().map(|&b| b as u32).collect(),
        attr_types: model.attrs.iter().map(|a| a.type_code).collect(),
        standard_information: None,
        names: Vec::new(),
        data: Vec::new(),
        index_allocation: Vec::new(),
        index_root: None,
        attribute_list: None,
        reparse_tag: None,
        fnv1a64: crate::hash::fnv1a64(bytes),
        state: ArtifactState::Intact,
        expect: crate::manifest::RecordExpect::default(),
    };
    for (index, attr) in model.attrs.iter().enumerate() {
        let attr_offset = model.attr_offset(index) as u32;
        let stream = |non_resident: Option<ExtentFact>, resident: Option<(u32, u32)>| StreamFact {
            type_code: attr.type_code,
            name: lossy(&attr.name),
            flags: attr.flags,
            attr_id: attr.id,
            attr_offset,
            resident,
            non_resident,
        };
        match (&attr.body, attr.type_code) {
            (AttrBody::Resident { value, .. }, attr_type::STANDARD_INFORMATION) if value.len() >= 36 => {
                fact.standard_information = Some((
                    [u64_at(value, 0), u64_at(value, 8), u64_at(value, 16), u64_at(value, 24)],
                    u32_at(value, 32),
                    value.len() as u32,
                ));
            }
            (AttrBody::Resident { value, .. }, attr_type::FILE_NAME) => {
                if let Some(name) = name_fact(value, attr.id) {
                    fact.names.push(name);
                }
            }
            (AttrBody::Resident { value, .. }, attr_type::DATA) => {
                let value_offset = attr_offset + attr.value_offset().expect("resident") as u32;
                fact.data.push(stream(None, Some((value_offset, value.len() as u32))));
            }
            (AttrBody::Resident { value, .. }, attr_type::REPARSE_POINT) if value.len() >= 4 => {
                fact.reparse_tag = Some(u32_at(value, 0));
            }
            (AttrBody::Resident { value, .. }, attr_type::ATTRIBUTE_LIST) => {
                fact.attribute_list =
                    Some(ListFact { resident: true, entries: list_entry_facts(value), runs: Vec::new() });
            }
            (AttrBody::NonResident(nr), attr_type::ATTRIBUTE_LIST) => {
                fact.attribute_list =
                    Some(ListFact { resident: false, entries: Vec::new(), runs: nr.runs.clone() });
            }
            (AttrBody::NonResident(nr), attr_type::DATA | attr_type::INDEX_ALLOCATION) => {
                let extent = ExtentFact {
                    lowest_vcn: nr.lowest_vcn,
                    highest_vcn: nr.highest_vcn,
                    runs: nr.runs.clone(),
                    allocated_size: nr.allocated_size,
                    real_size: nr.real_size,
                    initialized_size: nr.initialized_size,
                    compressed_size: nr.compressed_size,
                    compression_unit: nr.compression_unit,
                };
                if attr.type_code == attr_type::DATA {
                    fact.data.push(stream(Some(extent), None));
                } else {
                    fact.index_allocation.push(stream(Some(extent), None));
                }
            }
            _ => {}
        }
    }
    // `index_root` is filled in by the assembler, which has the structured
    // entries at hand.
    fact
}

/// Reads back an attribute list produced by [`crate::ondisk::attr::attribute_list`].
pub(crate) fn list_entry_facts(value: &[u8]) -> Vec<ListEntryFact> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 0x1A <= value.len() {
        let length = u16_at(value, pos + 4) as usize;
        let name_len = value[pos + 6] as usize;
        if length < 0x1A || pos + length > value.len() || 0x1A + 2 * name_len > length {
            break;
        }
        let units: Vec<u16> = (0..name_len).map(|i| u16_at(value, pos + 0x1A + 2 * i)).collect();
        out.push(ListEntryFact {
            type_code: u32_at(value, pos),
            name: lossy(&units),
            lowest_vcn: u64_at(value, pos + 8),
            holder: FileRef::from_raw(u64_at(value, pos + 0x10)),
            attr_id: u16_at(value, pos + 0x18),
        });
        pos += length;
    }
    out
}
