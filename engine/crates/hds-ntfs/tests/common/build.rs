//! Hand-built NTFS structures for the tests.
//!
//! Written from the tables of docs/notes/ntfs-on-disk.md, field by field
//! with literal offsets, and sharing no code with the crate under test: a
//! parser that misreads an offset must disagree with these builders.

pub fn put16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

pub fn get16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

pub fn get32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

/// UTF-16LE bytes of `s`.
pub fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// UTF-16LE bytes of raw code units (for unpaired surrogates).
pub fn units_le(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

pub fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// A file reference as stored: 48-bit record number, 16-bit sequence.
pub fn mref(number: u64, sequence: u16) -> u64 {
    assert!(number < 1 << 48);
    number | (u64::from(sequence) << 48)
}

/// Turns a structure with a multi-sector header from its in-memory form
/// into its on-disk form: the last two bytes of every 512-byte block move
/// into the update sequence array and are replaced by `usn`.
pub fn protect(block: &mut [u8], usn: u16) {
    let usa = get16(block, 0x04) as usize;
    let count = get16(block, 0x06) as usize;
    put16(block, usa, usn);
    for i in 1..count {
        let tail = i * 512 - 2;
        let saved = get16(block, tail);
        put16(block, usa + 2 * i, saved);
        put16(block, tail, usn);
    }
}

/// The 4 bytes the Windows driver leaves behind the end marker.
pub const WINDOWS_END_TAIL: u32 = 0x1147_7982;

/// Builds a `FILE` record in its in-memory (fixed-up) form.
pub struct RecordBuilder {
    pub bytes: Vec<u8>,
    next: usize,
    end_tail: u32,
}

impl RecordBuilder {
    /// NTFS 3.1 layout: update sequence array at 0x30, record number at
    /// 0x2C. In use, sequence 1, one link.
    pub fn new(size: usize) -> RecordBuilder {
        Self::with_usa_offset(size, 0x30)
    }

    /// The 42-byte header of NTFS 3.0 and older: update sequence array at
    /// 0x2A, no record number.
    pub fn old_layout(size: usize) -> RecordBuilder {
        Self::with_usa_offset(size, 0x2A)
    }

    pub fn with_usa_offset(size: usize, usa_offset: usize) -> RecordBuilder {
        let mut bytes = vec![0u8; size];
        let count = size / 512 + 1;
        bytes[0..4].copy_from_slice(b"FILE");
        put16(&mut bytes, 0x04, usa_offset as u16);
        put16(&mut bytes, 0x06, count as u16);
        put16(&mut bytes, 0x10, 1); // sequence
        put16(&mut bytes, 0x12, 1); // link count
        let first = align8(usa_offset + 2 * count);
        put16(&mut bytes, 0x14, first as u16);
        put16(&mut bytes, 0x16, 0x0001); // in use
        put32(&mut bytes, 0x1C, size as u32);
        RecordBuilder {
            bytes,
            next: first,
            end_tail: 0,
        }
    }

    pub fn lsn(mut self, lsn: u64) -> Self {
        put64(&mut self.bytes, 0x08, lsn);
        self
    }

    pub fn sequence(mut self, sequence: u16) -> Self {
        put16(&mut self.bytes, 0x10, sequence);
        self
    }

    pub fn links(mut self, links: u16) -> Self {
        put16(&mut self.bytes, 0x12, links);
        self
    }

    pub fn flags(mut self, flags: u16) -> Self {
        put16(&mut self.bytes, 0x16, flags);
        self
    }

    pub fn base(mut self, base: u64) -> Self {
        put64(&mut self.bytes, 0x20, base);
        self
    }

    pub fn next_attr_id(mut self, id: u16) -> Self {
        put16(&mut self.bytes, 0x28, id);
        self
    }

    /// The record number field at 0x2C (only meaningful with the 3.1
    /// layout; with the old layout these bytes are part of the array).
    pub fn number(mut self, number: u32) -> Self {
        put32(&mut self.bytes, 0x2C, number);
        self
    }

    /// The 4 bytes behind the end marker.
    pub fn end_tail(mut self, tail: u32) -> Self {
        self.end_tail = tail;
        self
    }

    /// Offset the next attribute will get.
    pub fn next_offset(&self) -> usize {
        self.next
    }

    /// Appends an attribute record.
    pub fn attr(mut self, attr: &[u8]) -> Self {
        self.bytes[self.next..self.next + attr.len()].copy_from_slice(attr);
        self.next += attr.len();
        self
    }

    /// Writes the end marker and the used size; returns the record in its
    /// in-memory form.
    pub fn finish(mut self) -> Vec<u8> {
        put32(&mut self.bytes, self.next, 0xFFFF_FFFF);
        put32(&mut self.bytes, self.next + 4, self.end_tail);
        put32(&mut self.bytes, 0x18, (self.next + 8) as u32);
        self.bytes
    }

    /// As [`finish`](Self::finish), then converted to the on-disk form.
    pub fn finish_on_disk(self, usn: u16) -> Vec<u8> {
        let mut bytes = self.finish();
        protect(&mut bytes, usn);
        bytes
    }
}

/// A resident attribute record. The name sits at 0x18, the value behind it
/// at the next multiple of 8.
pub fn resident(type_code: u32, name: &str, flags: u16, id: u16, value: &[u8]) -> Vec<u8> {
    let name = utf16le(name);
    let value_offset = align8(0x18 + name.len());
    let mut a = vec![0u8; align8(value_offset + value.len())];
    put32(&mut a, 0x00, type_code);
    let len = a.len() as u32;
    put32(&mut a, 0x04, len);
    a[0x08] = 0; // resident
    a[0x09] = (name.len() / 2) as u8;
    put16(&mut a, 0x0A, 0x18); // name offset
    put16(&mut a, 0x0C, flags);
    put16(&mut a, 0x0E, id);
    put32(&mut a, 0x10, value.len() as u32);
    put16(&mut a, 0x14, value_offset as u16);
    a[0x18..0x18 + name.len()].copy_from_slice(&name);
    a[value_offset..value_offset + value.len()].copy_from_slice(value);
    a
}

/// The fields of a non-resident attribute record.
pub struct NonResident<'a> {
    pub type_code: u32,
    pub name: &'a str,
    pub flags: u16,
    pub id: u16,
    pub start_vcn: u64,
    pub last_vcn: i64,
    pub compression_unit: u8,
    pub allocated: u64,
    pub real: u64,
    pub initialized: u64,
    /// `Some` selects the 0x48-byte header with the extra size field.
    pub total_allocated: Option<u64>,
    pub runlist: &'a [u8],
}

impl Default for NonResident<'_> {
    fn default() -> Self {
        NonResident {
            type_code: 0x80,
            name: "",
            flags: 0,
            id: 0,
            start_vcn: 0,
            last_vcn: 0,
            compression_unit: 0,
            allocated: 0,
            real: 0,
            initialized: 0,
            total_allocated: None,
            runlist: &[0],
        }
    }
}

impl NonResident<'_> {
    /// The attribute record: header of 0x40 or 0x48 bytes, the name directly
    /// behind it, the runlist at the next multiple of 8.
    pub fn build(&self) -> Vec<u8> {
        let name = utf16le(self.name);
        let header = if self.total_allocated.is_some() {
            0x48
        } else {
            0x40
        };
        let runs_offset = align8(header + name.len());
        let mut a = vec![0u8; align8(runs_offset + self.runlist.len())];
        put32(&mut a, 0x00, self.type_code);
        let len = a.len() as u32;
        put32(&mut a, 0x04, len);
        a[0x08] = 1; // non-resident
        a[0x09] = (name.len() / 2) as u8;
        put16(&mut a, 0x0A, header as u16); // name offset
        put16(&mut a, 0x0C, self.flags);
        put16(&mut a, 0x0E, self.id);
        put64(&mut a, 0x10, self.start_vcn);
        put64(&mut a, 0x18, self.last_vcn as u64);
        put16(&mut a, 0x20, runs_offset as u16);
        a[0x22] = self.compression_unit;
        put64(&mut a, 0x28, self.allocated);
        put64(&mut a, 0x30, self.real);
        put64(&mut a, 0x38, self.initialized);
        if let Some(total) = self.total_allocated {
            put64(&mut a, 0x40, total);
        }
        a[header..header + name.len()].copy_from_slice(&name);
        a[runs_offset..runs_offset + self.runlist.len()].copy_from_slice(self.runlist);
        a
    }
}

/// The fields of a `$FILE_NAME` value.
#[derive(Clone)]
pub struct Name {
    pub parent: u64,
    pub times: [u64; 4],
    pub allocated: u64,
    pub real: u64,
    pub flags: u32,
    pub reparse: u32,
    pub namespace: u8,
    pub name: Vec<u16>,
}

impl Name {
    pub fn new(parent: u64, name: &str, namespace: u8) -> Name {
        Name {
            parent,
            times: [
                0x01D9_0000_0000_0001,
                0x01D9_0000_0000_0002,
                0x01D9_0000_0000_0003,
                0x01D9_0000_0000_0004,
            ],
            allocated: 0,
            real: 0,
            flags: 0x20,
            reparse: 0,
            namespace,
            name: name.encode_utf16().collect(),
        }
    }

    /// The value: 0x42 bytes and the name.
    pub fn value(&self) -> Vec<u8> {
        let mut v = vec![0u8; 0x42];
        put64(&mut v, 0x00, self.parent);
        put64(&mut v, 0x08, self.times[0]);
        put64(&mut v, 0x10, self.times[1]);
        put64(&mut v, 0x18, self.times[2]);
        put64(&mut v, 0x20, self.times[3]);
        put64(&mut v, 0x28, self.allocated);
        put64(&mut v, 0x30, self.real);
        put32(&mut v, 0x38, self.flags);
        put32(&mut v, 0x3C, self.reparse);
        v[0x40] = self.name.len() as u8;
        v[0x41] = self.namespace;
        v.extend(units_le(&self.name));
        v
    }
}

/// A `$STANDARD_INFORMATION` value of `len` bytes (48 or 72).
pub fn standard_information(times: [u64; 4], flags: u32, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    put64(&mut v, 0x00, times[0]);
    put64(&mut v, 0x08, times[1]);
    put64(&mut v, 0x10, times[2]);
    put64(&mut v, 0x18, times[3]);
    put32(&mut v, 0x20, flags);
    v
}

/// An index entry: child reference, lengths, flags, the key, and the
/// sub-node VCN in the last 8 bytes when there is one.
pub fn index_entry(child: u64, key: &[u8], flags: u16, subnode: Option<u64>) -> Vec<u8> {
    let mut e = vec![0u8; align8(0x10 + key.len()) + if subnode.is_some() { 8 } else { 0 }];
    put64(&mut e, 0x00, child);
    let len = e.len();
    put16(&mut e, 0x08, len as u16);
    put16(&mut e, 0x0A, key.len() as u16);
    put16(&mut e, 0x0C, flags | u16::from(subnode.is_some()));
    e[0x10..0x10 + key.len()].copy_from_slice(key);
    if let Some(vcn) = subnode {
        put64(&mut e, len - 8, vcn);
    }
    e
}

/// The entry that ends a node: no key, flag 0x2.
pub fn last_entry(subnode: Option<u64>) -> Vec<u8> {
    index_entry(0, &[], 0x0002, subnode)
}

/// An `INDX` block in its in-memory form: update sequence array at 0x28,
/// first entry at the next multiple of 8 behind it.
pub fn index_block(
    size: usize,
    vcn: u64,
    lsn: u64,
    node_flags: u8,
    entries: &[Vec<u8>],
) -> Vec<u8> {
    let mut b = vec![0u8; size];
    let count = size / 512 + 1;
    b[0..4].copy_from_slice(b"INDX");
    put16(&mut b, 0x04, 0x28);
    put16(&mut b, 0x06, count as u16);
    put64(&mut b, 0x08, lsn);
    put64(&mut b, 0x10, vcn);
    let first = align8(0x28 + 2 * count);
    let mut pos = first;
    for entry in entries {
        b[pos..pos + entry.len()].copy_from_slice(entry);
        pos += entry.len();
    }
    put32(&mut b, 0x18, (first - 0x18) as u32); // entries offset, from the node header
    put32(&mut b, 0x1C, (pos - 0x18) as u32); // used size
    put32(&mut b, 0x20, (size - 0x18) as u32); // allocated size
    b[0x24] = node_flags;
    b
}

/// An `$INDEX_ROOT` value for a `$I30` index.
pub fn index_root(
    index_block_size: u32,
    blocks: u8,
    node_flags: u8,
    entries: &[Vec<u8>],
) -> Vec<u8> {
    let mut v = vec![0u8; 0x20];
    put32(&mut v, 0x00, 0x30); // indexed attribute type
    put32(&mut v, 0x04, 1); // collation: file name
    put32(&mut v, 0x08, index_block_size);
    v[0x0C] = blocks;
    for entry in entries {
        v.extend_from_slice(entry);
    }
    let used = (v.len() - 0x10) as u32;
    put32(&mut v, 0x10, 0x10); // entries offset, from the node header
    put32(&mut v, 0x14, used);
    put32(&mut v, 0x18, used);
    v[0x1C] = node_flags;
    v
}

/// An `$ATTRIBUTE_LIST` entry: 0x1A fixed bytes, the name, padding to 8.
pub fn attr_list_entry(
    type_code: u32,
    name: &str,
    start_vcn: u64,
    holder: u64,
    attr_id: u16,
) -> Vec<u8> {
    let name = utf16le(name);
    let mut e = vec![0u8; align8(0x1A + name.len())];
    put32(&mut e, 0x00, type_code);
    let len = e.len() as u16;
    put16(&mut e, 0x04, len);
    e[0x06] = (name.len() / 2) as u8;
    e[0x07] = 0x1A;
    put64(&mut e, 0x08, start_vcn);
    put64(&mut e, 0x10, holder);
    put16(&mut e, 0x18, attr_id);
    e[0x1A..0x1A + name.len()].copy_from_slice(&name);
    e
}

/// The fields of a boot sector, with the raw encodings of the three size
/// bytes.
#[derive(Clone)]
pub struct Boot {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster_raw: u8,
    pub hidden_sectors: u32,
    pub total_sectors: i64,
    pub mft_lcn: i64,
    pub mftmirr_lcn: i64,
    pub record_size_raw: u8,
    pub index_size_raw: u8,
    pub serial: u64,
}

impl Default for Boot {
    /// A 1 GiB volume with 4 KiB clusters as Windows formats it.
    fn default() -> Self {
        Boot {
            bytes_per_sector: 512,
            sectors_per_cluster_raw: 8,
            hidden_sectors: 2048,
            total_sectors: 2_097_151,
            mft_lcn: 87_381,
            mftmirr_lcn: 2,
            record_size_raw: 0xF6,
            index_size_raw: 0x01,
            serial: 0xA1B2_C3D4_E5F6_0718,
        }
    }
}

impl Boot {
    pub fn build(&self) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        s[0x03..0x0B].copy_from_slice(b"NTFS    ");
        put16(&mut s, 0x0B, self.bytes_per_sector);
        s[0x0D] = self.sectors_per_cluster_raw;
        s[0x15] = 0xF8;
        put32(&mut s, 0x1C, self.hidden_sectors);
        put64(&mut s, 0x28, self.total_sectors as u64);
        put64(&mut s, 0x30, self.mft_lcn as u64);
        put64(&mut s, 0x38, self.mftmirr_lcn as u64);
        s[0x40] = self.record_size_raw;
        s[0x44] = self.index_size_raw;
        put64(&mut s, 0x48, self.serial);
        s[0x1FE] = 0x55;
        s[0x1FF] = 0xAA;
        s
    }
}
