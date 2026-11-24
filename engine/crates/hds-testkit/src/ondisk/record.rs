//! `FILE` records and attribute headers (notes sections 2 to 4).

use super::runlist::{encode_runlist, Run};
use super::{align8, apply_fixups, put_u16, put_u32, put_u64, utf16_bytes, FileRef, FIXUP_STRIDE};

/// Attribute type codes (notes section 4).
pub mod attr_type {
    #![allow(missing_docs)]
    pub const STANDARD_INFORMATION: u32 = 0x10;
    pub const ATTRIBUTE_LIST: u32 = 0x20;
    pub const FILE_NAME: u32 = 0x30;
    pub const OBJECT_ID: u32 = 0x40;
    pub const SECURITY_DESCRIPTOR: u32 = 0x50;
    pub const VOLUME_NAME: u32 = 0x60;
    pub const VOLUME_INFORMATION: u32 = 0x70;
    pub const DATA: u32 = 0x80;
    pub const INDEX_ROOT: u32 = 0x90;
    pub const INDEX_ALLOCATION: u32 = 0xA0;
    pub const BITMAP: u32 = 0xB0;
    pub const REPARSE_POINT: u32 = 0xC0;
    pub const EA_INFORMATION: u32 = 0xD0;
    pub const EA: u32 = 0xE0;
    pub const LOGGED_UTILITY_STREAM: u32 = 0x100;
    pub const END: u32 = 0xFFFF_FFFF;
}

/// Record header flag: the record is in use.
pub const FLAG_IN_USE: u16 = 0x0001;
/// Record header flag: the record is a directory (has a `$I30` index).
pub const FLAG_DIRECTORY: u16 = 0x0002;
/// Record header flag seen on some `$Extend` files.
pub const FLAG_IS_4: u16 = 0x0004;
/// Record header flag: the record has a non-`$I30` ("view") index.
pub const FLAG_VIEW_INDEX: u16 = 0x0008;

/// Attribute flag: LZNT1 compressed (`0x00FF` is the compression mask).
pub const ATTR_COMPRESSED: u16 = 0x0001;
/// Attribute flag: encrypted.
pub const ATTR_ENCRYPTED: u16 = 0x4000;
/// Attribute flag: sparse.
pub const ATTR_SPARSE: u16 = 0x8000;

/// The four bytes behind the end marker in records written by the Windows
/// driver (`82 79 47 11` on disk; notes section 4).
pub const END_TAIL_WINDOWS: u32 = 0x1147_7982;

/// Which of the two header layouts a record has (notes section 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderForm {
    /// NTFS 3.1 and later: 48-byte header, update sequence array at 0x30.
    /// `number` is the value written at 0x2C (the record's claim about its
    /// own number, which need not be true) and `number_high` the 16 bits at
    /// 0x2A.
    V31 {
        /// Low 32 bits of the record number as stored.
        number: u32,
        /// The 16 bits at 0x2A (high part of the number; normally 0).
        number_high: u16,
    },
    /// NTFS 3.0 and earlier (NT4, Windows 2000): 42-byte header without a
    /// record number, update sequence array at 0x2A.
    V30,
}

impl HeaderForm {
    /// Offset of the update sequence array.
    pub const fn usa_offset(self) -> usize {
        match self {
            HeaderForm::V31 { .. } => 0x30,
            HeaderForm::V30 => 0x2A,
        }
    }
}

/// Value part of an attribute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttrBody {
    /// The value is stored in the record.
    Resident {
        /// The value bytes.
        value: Vec<u8>,
        /// The "indexed" flag at 0x16 (set on `$FILE_NAME` by Windows).
        indexed: bool,
    },
    /// The value is stored in clusters described by a runlist.
    NonResident(NonResident),
}

/// Header fields and runs of a non-resident attribute extent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NonResident {
    /// Lowest VCN of this extent (0x10).
    pub lowest_vcn: i64,
    /// Highest VCN of this extent (0x18); -1 for an empty attribute.
    pub highest_vcn: i64,
    /// The runs of this extent.
    pub runs: Vec<Run>,
    /// Compression unit, log2 clusters (0x22).
    pub compression_unit: u8,
    /// Allocated size (0x28); meaningful only when `lowest_vcn` is 0.
    pub allocated_size: u64,
    /// Real size (0x30).
    pub real_size: u64,
    /// Initialized size (0x38).
    pub initialized_size: u64,
    /// `Some` selects the 0x48-byte header of sparse and compressed
    /// attributes and is the value of its extra field at 0x40.
    pub compressed_size: Option<u64>,
    /// Bytes placed directly behind the runlist terminator (padding that is
    /// "not always zero", notes section 5).
    pub tail: Vec<u8>,
}

/// One attribute of a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrModel {
    /// Attribute type code.
    pub type_code: u32,
    /// Attribute name in UTF-16 units (empty = unnamed).
    pub name: Vec<u16>,
    /// Attribute flags (0x0C).
    pub flags: u16,
    /// Attribute instance / id (0x0E).
    pub id: u16,
    /// Value of the name-offset field when the attribute has no name.
    /// Windows writes 0 in about half of the unnamed attributes and the
    /// header size in the others; `None` writes the header size.
    pub unnamed_name_offset: Option<u16>,
    /// The value.
    pub body: AttrBody,
}

impl AttrModel {
    /// An unnamed resident attribute with id 0.
    pub fn resident(type_code: u32, value: Vec<u8>) -> Self {
        AttrModel {
            type_code,
            name: Vec::new(),
            flags: 0,
            id: 0,
            unnamed_name_offset: None,
            body: AttrBody::Resident { value, indexed: false },
        }
    }

    /// An unnamed non-resident attribute with id 0.
    pub fn non_resident(type_code: u32, body: NonResident) -> Self {
        AttrModel {
            type_code,
            name: Vec::new(),
            flags: 0,
            id: 0,
            unnamed_name_offset: None,
            body: AttrBody::NonResident(body),
        }
    }

    /// Sets the name.
    pub fn named(mut self, name: &[u16]) -> Self {
        self.name = name.to_vec();
        self
    }

    /// Sets the attribute id.
    pub fn with_id(mut self, id: u16) -> Self {
        self.id = id;
        self
    }

    /// Sets the attribute flags.
    pub fn with_flags(mut self, flags: u16) -> Self {
        self.flags = flags;
        self
    }

    /// `true` for a non-resident attribute.
    pub fn is_non_resident(&self) -> bool {
        matches!(self.body, AttrBody::NonResident(_))
    }

    /// Offset of the value inside the encoded attribute (resident only).
    pub fn value_offset(&self) -> Option<usize> {
        match self.body {
            AttrBody::Resident { .. } => Some(align8(0x18 + 2 * self.name.len())),
            AttrBody::NonResident(_) => None,
        }
    }

    /// The attribute as stored in a record (length is a multiple of 8).
    pub fn encode(&self) -> Vec<u8> {
        assert!(self.name.len() <= 255, "attribute name too long");
        let name = utf16_bytes(&self.name);
        match &self.body {
            AttrBody::Resident { value, indexed } => {
                let header = 0x18;
                let value_offset = align8(header + name.len());
                let length = align8(value_offset + value.len());
                let mut b = vec![0u8; length];
                self.common_header(&mut b, length, 0, header);
                put_u32(&mut b, 0x10, value.len() as u32);
                put_u16(&mut b, 0x14, value_offset as u16);
                b[0x16] = u8::from(*indexed);
                b[header..header + name.len()].copy_from_slice(&name);
                b[value_offset..value_offset + value.len()].copy_from_slice(value);
                b
            }
            AttrBody::NonResident(nr) => {
                let header = if nr.compressed_size.is_some() { 0x48 } else { 0x40 };
                let runs_offset = align8(header + name.len());
                let mut runs = encode_runlist(&nr.runs);
                runs.extend_from_slice(&nr.tail);
                let length = align8(runs_offset + runs.len());
                let mut b = vec![0u8; length];
                self.common_header(&mut b, length, 1, header);
                put_u64(&mut b, 0x10, nr.lowest_vcn as u64);
                put_u64(&mut b, 0x18, nr.highest_vcn as u64);
                put_u16(&mut b, 0x20, runs_offset as u16);
                b[0x22] = nr.compression_unit;
                put_u64(&mut b, 0x28, nr.allocated_size);
                put_u64(&mut b, 0x30, nr.real_size);
                put_u64(&mut b, 0x38, nr.initialized_size);
                if let Some(compressed) = nr.compressed_size {
                    put_u64(&mut b, 0x40, compressed);
                }
                b[header..header + name.len()].copy_from_slice(&name);
                b[runs_offset..runs_offset + runs.len()].copy_from_slice(&runs);
                b
            }
        }
    }

    fn common_header(&self, b: &mut [u8], length: usize, non_resident: u8, header: usize) {
        put_u32(b, 0x00, self.type_code);
        put_u32(b, 0x04, length as u32);
        b[0x08] = non_resident;
        b[0x09] = self.name.len() as u8;
        let name_offset = if self.name.is_empty() {
            self.unnamed_name_offset.unwrap_or(header as u16)
        } else {
            header as u16
        };
        put_u16(b, 0x0A, name_offset);
        put_u16(b, 0x0C, self.flags);
        put_u16(b, 0x0E, self.id);
    }
}

/// Everything that determines the bytes of one `FILE` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordModel {
    /// Header layout and the record number written into it.
    pub header: HeaderForm,
    /// Allocated size: 1024 or 4096 (any multiple of 512 can be encoded).
    pub record_size: u32,
    /// `$LogFile` sequence number (0x08).
    pub lsn: u64,
    /// Sequence number (0x10).
    pub sequence: u16,
    /// Link count (0x12).
    pub link_count: u16,
    /// Header flags (0x16).
    pub flags: u16,
    /// Base record reference (0x20); [`FileRef::NONE`] in a base record.
    pub base: FileRef,
    /// Next attribute instance (0x28).
    pub next_attr_id: u16,
    /// Update sequence number; 0 and 0xFFFF are not used by real drivers.
    pub usn: u16,
    /// Attributes in the order they are written.
    pub attrs: Vec<AttrModel>,
    /// The four bytes behind the end marker: 0, or [`END_TAIL_WINDOWS`].
    pub end_tail: u32,
    /// Bytes left behind the end marker (remains of earlier attributes).
    pub slack: Vec<u8>,
    /// Indexes of 512-byte blocks whose fixup is broken after encoding, as
    /// an interrupted multi-sector write leaves them (the block still ends
    /// in the previous update sequence number).
    pub torn_blocks: Vec<usize>,
}

/// Why a record could not be encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The attributes need more bytes than the record has.
    Overflow {
        /// Bytes needed (header, attributes, end marker).
        needed: usize,
        /// Record size.
        record_size: usize,
    },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::Overflow { needed, record_size } => {
                write!(f, "record needs {needed} bytes but has {record_size}")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

impl RecordModel {
    /// An in-use base record without attributes.
    pub fn new(number: u64, sequence: u16, record_size: u32) -> Self {
        RecordModel {
            header: HeaderForm::V31 { number: number as u32, number_high: (number >> 32) as u16 },
            record_size,
            lsn: 0,
            sequence,
            link_count: 0,
            flags: FLAG_IN_USE,
            base: FileRef::NONE,
            next_attr_id: 0,
            usn: 1,
            attrs: Vec::new(),
            end_tail: 0,
            slack: Vec::new(),
            torn_blocks: Vec::new(),
        }
    }

    /// Offset of the first attribute for this header form and size.
    pub fn attrs_offset(&self) -> usize {
        let usa_count = self.record_size as usize / FIXUP_STRIDE + 1;
        align8(self.header.usa_offset() + 2 * usa_count)
    }

    /// Bytes in use: header, attributes and the 8-byte end marker.
    pub fn bytes_in_use(&self) -> usize {
        self.attrs_offset() + self.attrs.iter().map(|a| a.encode().len()).sum::<usize>() + 8
    }

    /// Bytes still free for attributes.
    pub fn free_bytes(&self) -> usize {
        (self.record_size as usize).saturating_sub(self.bytes_in_use())
    }

    /// Offset of attribute `index` inside the record.
    pub fn attr_offset(&self, index: usize) -> usize {
        self.attrs_offset() + self.attrs[..index].iter().map(|a| a.encode().len()).sum::<usize>()
    }

    /// The record as it lies on disk, fixups applied.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let size = self.record_size as usize;
        assert!(size >= 1024 && size.is_multiple_of(FIXUP_STRIDE), "unsupported record size {size}");
        let needed = self.bytes_in_use();
        if needed > size {
            return Err(EncodeError::Overflow { needed, record_size: size });
        }
        let usa_offset = self.header.usa_offset();
        let attrs_offset = self.attrs_offset();
        let mut b = vec![0u8; size];
        b[0..4].copy_from_slice(b"FILE");
        put_u16(&mut b, 0x04, usa_offset as u16);
        put_u16(&mut b, 0x06, (size / FIXUP_STRIDE + 1) as u16);
        put_u64(&mut b, 0x08, self.lsn);
        put_u16(&mut b, 0x10, self.sequence);
        put_u16(&mut b, 0x12, self.link_count);
        put_u16(&mut b, 0x14, attrs_offset as u16);
        put_u16(&mut b, 0x16, self.flags);
        put_u32(&mut b, 0x18, needed as u32);
        put_u32(&mut b, 0x1C, size as u32);
        put_u64(&mut b, 0x20, self.base.raw());
        put_u16(&mut b, 0x28, self.next_attr_id);
        if let HeaderForm::V31 { number, number_high } = self.header {
            put_u16(&mut b, 0x2A, number_high);
            put_u32(&mut b, 0x2C, number);
        }
        let mut pos = attrs_offset;
        for attr in &self.attrs {
            let bytes = attr.encode();
            b[pos..pos + bytes.len()].copy_from_slice(&bytes);
            pos += bytes.len();
        }
        put_u32(&mut b, pos, attr_type::END);
        put_u32(&mut b, pos + 4, self.end_tail);
        pos += 8;
        let slack = self.slack.len().min(size - pos);
        b[pos..pos + slack].copy_from_slice(&self.slack[..slack]);
        apply_fixups(&mut b, usa_offset, self.usn);
        for &block in &self.torn_blocks {
            assert!(block < size / FIXUP_STRIDE, "torn block {block} outside the record");
            let tail = (block + 1) * FIXUP_STRIDE - 2;
            let stale = match self.usn.wrapping_sub(1) {
                0 => 0xFFFE,
                v => v,
            };
            put_u16(&mut b, tail, stale);
        }
        Ok(b)
    }
}

/// A record slot that was formatted but never used, in the forms real
/// formatters leave (notes section 3, "Record states seen").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlankForm {
    /// `FILE`, flags 0, no attributes, correct number, the given sequence
    /// number: 1 on Windows 7-era volumes and ntfs-3g, 0 on Windows XP.
    Empty {
        /// Sequence number (0 or 1 on real volumes).
        sequence: u16,
    },
    /// mkntfs records 16 to 23: sequence = record number, number field 0, a
    /// lone 48-byte `$STANDARD_INFORMATION`.
    Mkntfs,
}

/// The model of a never-used record for slot `number`. (The third form,
/// all zero bytes as on Windows 10, needs no model.) `old_header` selects
/// the pre-3.1 header without record number.
pub fn blank_model(
    form: BlankForm,
    number: u64,
    record_size: u32,
    lsn: u64,
    time: u64,
    old_header: bool,
) -> RecordModel {
    let mut model = RecordModel::new(number, 1, record_size);
    model.flags = 0;
    model.lsn = lsn;
    match form {
        BlankForm::Empty { sequence } => model.sequence = sequence,
        BlankForm::Mkntfs => {
            model.sequence = number as u16;
            model.header = HeaderForm::V31 { number: 0, number_high: 0 };
            let si = super::attr::standard_information(&[time; 4], 0x06, None);
            model.attrs.push(AttrModel::resident(attr_type::STANDARD_INFORMATION, si));
            model.next_attr_id = 1;
        }
    }
    if old_header {
        model.header = HeaderForm::V30;
    }
    model
}

/// The bytes of a never-used record; see [`blank_model`].
pub fn blank_record(
    form: BlankForm,
    number: u64,
    record_size: u32,
    lsn: u64,
    time: u64,
    old_header: bool,
) -> Vec<u8> {
    blank_model(form, number, record_size, lsn, time, old_header).encode().expect("a blank record always fits")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }
    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    /// Reproduces record 35 (`passwords.txt`) of the Windows 10 sample volume
    /// `ntfs-fixed.vhd` byte for byte from its field values. The expected
    /// bytes below were copied from the sample (docs/notes section 15).
    #[test]
    fn reproduces_a_record_written_by_windows_10() {
        let created = 0x01D6_9FC2_EFF1_A386u64;
        let modified = 0x01D6_9FC2_EFF3_293Cu64;
        let accessed = 0x01D6_9FC2_EFF7_0107u64;
        let si = super::super::attr::standard_information(
            &[created, modified, modified, accessed],
            0x20,
            Some(super::super::attr::SiV3 { owner_id: 0, security_id: 0x107, quota_charged: 0, usn: 0 }),
        );
        let name: Vec<u16> = "passwords.txt".encode_utf16().collect();
        let fname = super::super::attr::file_name(&super::super::attr::FileNameValue {
            parent: FileRef::new(5, 5),
            times: [created; 4],
            allocated_size: 0,
            real_size: 0,
            flags: 0x20,
            ea_or_reparse: 0,
            namespace: 0,
            name: name.clone(),
        });
        let content = b"place,user,password \r\nbank,joesmith,superrich \r\nalarm system,-,1234 \r\ntreasure chest,-,1111 \r\nuber secret laire,admin,admin \r\n";
        assert_eq!(content.len(), 0x7E);
        let mut model = RecordModel::new(35, 1, 1024);
        model.lsn = 0x1063EF;
        model.link_count = 1;
        model.next_attr_id = 3;
        model.usn = 2;
        model.end_tail = END_TAIL_WINDOWS;
        let mut a0 = AttrModel::resident(attr_type::STANDARD_INFORMATION, si);
        a0.unnamed_name_offset = Some(0);
        let mut a1 = AttrModel::resident(attr_type::FILE_NAME, fname).with_id(2);
        a1.unnamed_name_offset = Some(0);
        a1.body = match a1.body {
            AttrBody::Resident { value, .. } => AttrBody::Resident { value, indexed: true },
            other => other,
        };
        let a2 = AttrModel::resident(attr_type::DATA, content.to_vec()).with_id(1);
        model.attrs = vec![a0, a1, a2];
        let b = model.encode().unwrap();

        let expected_head: [u8; 0x130] = [
            0x46, 0x49, 0x4c, 0x45, 0x30, 0x00, 0x03, 0x00, 0xef, 0x63, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x01, 0x00, 0x01, 0x00, 0x38, 0x00, 0x01, 0x00, 0xb0, 0x01, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x23, 0x00, 0x00, 0x00,
            0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x60, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x48, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00,
            0x86, 0xa3, 0xf1, 0xef, 0xc2, 0x9f, 0xd6, 0x01, 0x3c, 0x29, 0xf3, 0xef, 0xc2, 0x9f, 0xd6, 0x01,
            0x3c, 0x29, 0xf3, 0xef, 0xc2, 0x9f, 0xd6, 0x01, 0x07, 0x01, 0xf7, 0xef, 0xc2, 0x9f, 0xd6, 0x01,
            0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x07, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x00, 0x00, 0x00, 0x78, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x5c, 0x00, 0x00, 0x00, 0x18, 0x00, 0x01, 0x00,
            0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x86, 0xa3, 0xf1, 0xef, 0xc2, 0x9f, 0xd6, 0x01,
            0x86, 0xa3, 0xf1, 0xef, 0xc2, 0x9f, 0xd6, 0x01, 0x86, 0xa3, 0xf1, 0xef, 0xc2, 0x9f, 0xd6, 0x01,
            0x86, 0xa3, 0xf1, 0xef, 0xc2, 0x9f, 0xd6, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x0d, 0x00, 0x70, 0x00, 0x61, 0x00, 0x73, 0x00, 0x73, 0x00, 0x77, 0x00, 0x6f, 0x00, 0x72, 0x00,
            0x64, 0x00, 0x73, 0x00, 0x2e, 0x00, 0x74, 0x00, 0x78, 0x00, 0x74, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x80, 0x00, 0x00, 0x00, 0x98, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x7e, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x70, 0x6c, 0x61, 0x63, 0x65, 0x2c, 0x75, 0x73,
        ];
        assert_eq!(&b[..0x130], &expected_head[..]);
        // end marker with the Windows tail directly behind the data attribute
        assert_eq!(u32_at(&b, 0x1A8), 0xFFFF_FFFF);
        assert_eq!(&b[0x1AC..0x1B0], &[0x82, 0x79, 0x47, 0x11]);
        assert_eq!(u32_at(&b, 0x18), 0x1B0);
    }

    #[test]
    fn header_fields_for_both_record_sizes_and_header_forms() {
        let m = RecordModel::new(0x1_2345_6789, 7, 4096);
        let b = m.encode().unwrap();
        assert_eq!(&b[..4], b"FILE");
        assert_eq!(u16_at(&b, 4), 0x30);
        assert_eq!(u16_at(&b, 6), 9, "4096-byte record: 8 fixups + the number");
        assert_eq!(u16_at(&b, 0x14), 0x48, "first attribute behind the 18-byte array");
        assert_eq!(u32_at(&b, 0x18), 0x50);
        assert_eq!(u32_at(&b, 0x1C), 4096);
        assert_eq!(u16_at(&b, 0x2A), 1, "high part of the record number");
        assert_eq!(u32_at(&b, 0x2C), 0x2345_6789);
        assert_eq!(u32_at(&b, 0x48), 0xFFFF_FFFF);
        for block in 0..8 {
            assert_eq!(u16_at(&b, block * 512 + 510), 1, "fixup of block {block}");
        }

        let mut old = RecordModel::new(99, 3, 1024);
        old.header = HeaderForm::V30;
        let b = old.encode().unwrap();
        assert_eq!(u16_at(&b, 4), 0x2A, "pre-3.1 header: array directly behind the 42-byte header");
        assert_eq!(u16_at(&b, 6), 3);
        assert_eq!(u16_at(&b, 0x14), 0x30);
        assert_eq!(u16_at(&b, 0x2A), 1, "the array starts where 3.1 has its reserved field");
        assert_eq!(u32_at(&b, 0x30), 0xFFFF_FFFF);
        assert_eq!(u32_at(&b, 0x18), 0x38);
    }

    #[test]
    fn non_resident_header_is_0x40_or_0x48_bytes_and_offsets_follow_the_name() {
        let body = NonResident {
            lowest_vcn: 0,
            highest_vcn: 15,
            runs: vec![Run::at(1000, 16)],
            compression_unit: 0,
            allocated_size: 65536,
            real_size: 60000,
            initialized_size: 50000,
            compressed_size: None,
            tail: Vec::new(),
        };
        // plain: 0x40 / 0x40
        let plain = AttrModel::non_resident(attr_type::DATA, body.clone()).encode();
        assert_eq!(u16_at(&plain, 0x0A), 0x40);
        assert_eq!(u16_at(&plain, 0x20), 0x40);
        assert_eq!(plain.len(), 0x48);
        assert_eq!(u64_at(&plain, 0x18), 15);
        assert_eq!(u64_at(&plain, 0x28), 65536);
        assert_eq!(u64_at(&plain, 0x30), 60000);
        assert_eq!(u64_at(&plain, 0x38), 50000);
        assert_eq!(&plain[0x40..0x45], &[0x21, 0x10, 0xE8, 0x03, 0x00]);
        // named $I30: name at 0x40, runs at 0x48 (notes section 4)
        let i30: Vec<u16> = "$I30".encode_utf16().collect();
        let named = AttrModel::non_resident(attr_type::INDEX_ALLOCATION, body.clone()).named(&i30).encode();
        assert_eq!(named[0x09], 4);
        assert_eq!(u16_at(&named, 0x0A), 0x40);
        assert_eq!(u16_at(&named, 0x20), 0x48);
        assert_eq!(&named[0x40..0x48], &[b'$', 0, b'I', 0, b'3', 0, b'0', 0]);
        // sparse or compressed: both at 0x48, extra size field at 0x40
        let mut sparse_body = body.clone();
        sparse_body.compressed_size = Some(4096);
        sparse_body.compression_unit = 4;
        let sparse = AttrModel::non_resident(attr_type::DATA, sparse_body).with_flags(ATTR_SPARSE).encode();
        assert_eq!(u16_at(&sparse, 0x0A), 0x48);
        assert_eq!(u16_at(&sparse, 0x20), 0x48);
        assert_eq!(u64_at(&sparse, 0x40), 4096);
        assert_eq!(sparse[0x22], 4);
        assert_eq!(u16_at(&sparse, 0x0C), 0x8000);
        // a 9-character stream name: name 0x40, runs 0x58 ("bigstream")
        let nine: Vec<u16> = "bigstream".encode_utf16().collect();
        let stream = AttrModel::non_resident(attr_type::DATA, body.clone()).named(&nine).encode();
        assert_eq!(u16_at(&stream, 0x20), 0x58);
        // garbage behind the terminator stays inside the padding
        let mut tail_body = body;
        tail_body.tail = vec![0xFF];
        let tail = AttrModel::non_resident(attr_type::DATA, tail_body).encode();
        assert_eq!(&tail[0x40..0x48], &[0x21, 0x10, 0xE8, 0x03, 0x00, 0xFF, 0x00, 0x00]);
        assert_eq!(tail.len(), 0x48);
    }

    #[test]
    fn empty_non_resident_attribute() {
        // notes section 4: lowest 0, highest -1, all sizes 0, no runs
        let body = NonResident {
            lowest_vcn: 0,
            highest_vcn: -1,
            runs: Vec::new(),
            compression_unit: 0,
            allocated_size: 0,
            real_size: 0,
            initialized_size: 0,
            compressed_size: None,
            tail: Vec::new(),
        };
        let b = AttrModel::non_resident(attr_type::DATA, body).encode();
        assert_eq!(u64_at(&b, 0x18), u64::MAX);
        assert_eq!(b[0x40], 0);
        assert_eq!(b.len(), 0x48);
    }

    #[test]
    fn resident_value_offsets() {
        // unnamed: 0x18; $I30: 0x20; a 15-character name: 0x38 (notes section 4)
        let a = AttrModel::resident(attr_type::DATA, vec![1, 2, 3]);
        assert_eq!(a.value_offset(), Some(0x18));
        let b = a.encode();
        assert_eq!(u16_at(&b, 0x14), 0x18);
        assert_eq!(u32_at(&b, 0x10), 3);
        assert_eq!(b.len(), 0x20);
        let i30: Vec<u16> = "$I30".encode_utf16().collect();
        let b = AttrModel::resident(attr_type::INDEX_ROOT, vec![0; 48]).named(&i30).encode();
        assert_eq!(u16_at(&b, 0x14), 0x20);
        let fifteen: Vec<u16> = "Zone.Identifier".encode_utf16().collect();
        let b = AttrModel::resident(attr_type::DATA, vec![0; 5]).named(&fifteen).encode();
        assert_eq!(u16_at(&b, 0x14), 0x38);
        assert_eq!(u16_at(&b, 0x0A), 0x18);
        assert_eq!(b[0x09], 15);
    }

    #[test]
    fn overflow_is_reported_not_truncated() {
        let mut m = RecordModel::new(1, 1, 1024);
        m.attrs.push(AttrModel::resident(attr_type::DATA, vec![0xAB; 1024 - 0x38 - 0x18 - 8]));
        assert_eq!(m.free_bytes(), 0);
        assert!(m.encode().is_ok());
        m.attrs[0] = AttrModel::resident(attr_type::DATA, vec![0xAB; 1024 - 0x38 - 0x18 - 8 + 1]);
        assert_eq!(
            m.encode(),
            Err(EncodeError::Overflow { needed: 1032, record_size: 1024 })
        );
    }

    #[test]
    fn data_under_a_fixup_position_survives_in_the_array() {
        let mut m = RecordModel::new(1, 1, 1024);
        m.usn = 0x7788;
        let value: Vec<u8> = (0..700u32).map(|i| (i % 250 + 1) as u8).collect();
        m.attrs.push(AttrModel::resident(attr_type::DATA, value.clone()));
        let b = m.encode().unwrap();
        let value_at = 0x38 + 0x18;
        let under_fixup = 510 - value_at;
        assert_eq!(u16_at(&b, 510), 0x7788);
        assert_eq!(&b[0x32..0x34], &value[under_fixup..under_fixup + 2]);
        assert_eq!(&b[value_at..510], &value[..under_fixup]);
        assert_eq!(&b[512..value_at + 700], &value[under_fixup + 2..]);
    }

    #[test]
    fn torn_block_keeps_the_previous_sequence_number() {
        let mut m = RecordModel::new(1, 1, 1024);
        m.usn = 5;
        m.torn_blocks = vec![1];
        let b = m.encode().unwrap();
        assert_eq!(u16_at(&b, 0x30), 5);
        assert_eq!(u16_at(&b, 510), 5);
        assert_eq!(u16_at(&b, 1022), 4, "second block was not written");
        m.usn = 1;
        let b = m.encode().unwrap();
        assert_ne!(u16_at(&b, 1022), 1);
    }

    #[test]
    fn blank_records_in_the_known_forms() {
        // Windows 7 era: FILE, flags 0, sequence 1, no attributes, number kept
        let b = blank_record(BlankForm::Empty { sequence: 1 }, 200, 1024, 0x1234, 0, false);
        assert_eq!(&b[..4], b"FILE");
        assert_eq!(u16_at(&b, 0x10), 1);
        assert_eq!(u16_at(&b, 0x16), 0);
        assert_eq!(u32_at(&b, 0x18), 0x40, "bytes in use: header and end marker");
        assert_eq!(u32_at(&b, 0x2C), 200);
        assert_eq!(u32_at(&b, 0x38), 0xFFFF_FFFF);
        assert_eq!(u64_at(&b, 8), 0x1234);
        // Windows XP: the same with sequence 0
        let b = blank_record(BlankForm::Empty { sequence: 0 }, 17, 1024, 0, 0, false);
        assert_eq!(u16_at(&b, 0x10), 0);
        // mkntfs 16-23: sequence = number, number field 0, lone 48-byte $SI
        let b = blank_record(BlankForm::Mkntfs, 18, 1024, 0, 0x01D6_0000_0000_0000, false);
        assert_eq!(u16_at(&b, 0x10), 18);
        assert_eq!(u32_at(&b, 0x2C), 0);
        assert_eq!(u16_at(&b, 0x16), 0);
        assert_eq!(u32_at(&b, 0x38), 0x10);
        assert_eq!(u32_at(&b, 0x38 + 0x10), 48);
        assert_eq!(u32_at(&b, 0x18), 136, "as measured on mkntfs volumes");
        // Windows 2000: the same emptiness behind the 42-byte header
        let b = blank_record(BlankForm::Empty { sequence: 1 }, 200, 1024, 0, 0, true);
        assert_eq!(u16_at(&b, 4), 0x2A);
        assert_eq!(u32_at(&b, 0x30), 0xFFFF_FFFF);
        assert_eq!(u32_at(&b, 0x18), 0x38);
    }
}
