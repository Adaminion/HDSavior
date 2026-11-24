//! Builders for hand-made NTFS structures, written from
//! `docs/notes/ntfs-on-disk.md` (tests only). They share no code with
//! `hds-ntfs`.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    dead_code
)]

pub const SI: u32 = 0x10;
pub const ATTRIBUTE_LIST: u32 = 0x20;
pub const FILE_NAME: u32 = 0x30;
pub const DATA: u32 = 0x80;
pub const INDEX_ROOT: u32 = 0x90;
pub const INDEX_ALLOCATION: u32 = 0xA0;
pub const REPARSE_POINT: u32 = 0xC0;

/// Reference `(number, sequence)` as stored.
pub fn fref(number: u64, sequence: u16) -> u64 {
    number | (u64::from(sequence) << 48)
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn pad8(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(8) {
        v.push(0);
    }
}

/// A `$FILE_NAME` value.
pub fn file_name_value(
    parent: u64,
    name: &str,
    namespace: u8,
    flags: u32,
    times: [u64; 4],
) -> Vec<u8> {
    let units = utf16(name);
    let mut v = Vec::new();
    v.extend_from_slice(&parent.to_le_bytes());
    for t in times {
        v.extend_from_slice(&t.to_le_bytes());
    }
    v.extend_from_slice(&4096u64.to_le_bytes());
    v.extend_from_slice(&1234u64.to_le_bytes());
    v.extend_from_slice(&flags.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.push((units.len() / 2) as u8);
    v.push(namespace);
    v.extend_from_slice(&units);
    v
}

/// A resident attribute record.
pub fn resident_attr(type_code: u32, name: &str, id: u16, flags: u16, value: &[u8]) -> Vec<u8> {
    let name = utf16(name);
    let name_off = 0x18usize;
    let mut value_off = name_off + name.len();
    value_off = (value_off + 7) & !7;
    let mut a = vec![0u8; value_off];
    a[0..4].copy_from_slice(&type_code.to_le_bytes());
    a[8] = 0;
    a[9] = (name.len() / 2) as u8;
    a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
    a[0x0C..0x0E].copy_from_slice(&flags.to_le_bytes());
    a[0x0E..0x10].copy_from_slice(&id.to_le_bytes());
    a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
    a[0x14..0x16].copy_from_slice(&(value_off as u16).to_le_bytes());
    a[name_off..name_off + name.len()].copy_from_slice(&name);
    a.extend_from_slice(value);
    pad8(&mut a);
    let len = a.len() as u32;
    a[4..8].copy_from_slice(&len.to_le_bytes());
    a
}

/// A non-resident attribute record.
#[allow(clippy::too_many_arguments)]
pub fn nonresident_attr(
    type_code: u32,
    name: &str,
    id: u16,
    flags: u16,
    start_vcn: u64,
    last_vcn: i64,
    sizes: (u64, u64, u64),
    runlist: &[u8],
) -> Vec<u8> {
    let name = utf16(name);
    let name_off = 0x40usize;
    let mut rl_off = name_off + name.len();
    rl_off = (rl_off + 7) & !7;
    let mut a = vec![0u8; rl_off];
    a[0..4].copy_from_slice(&type_code.to_le_bytes());
    a[8] = 1;
    a[9] = (name.len() / 2) as u8;
    a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
    a[0x0C..0x0E].copy_from_slice(&flags.to_le_bytes());
    a[0x0E..0x10].copy_from_slice(&id.to_le_bytes());
    a[0x10..0x18].copy_from_slice(&start_vcn.to_le_bytes());
    a[0x18..0x20].copy_from_slice(&last_vcn.to_le_bytes());
    a[0x20..0x22].copy_from_slice(&(rl_off as u16).to_le_bytes());
    a[0x28..0x30].copy_from_slice(&sizes.0.to_le_bytes());
    a[0x30..0x38].copy_from_slice(&sizes.1.to_le_bytes());
    a[0x38..0x40].copy_from_slice(&sizes.2.to_le_bytes());
    a[name_off..name_off + name.len()].copy_from_slice(&name);
    a.extend_from_slice(runlist);
    pad8(&mut a);
    let len = a.len() as u32;
    a[4..8].copy_from_slice(&len.to_le_bytes());
    a
}

fn signed_le(value: i64) -> Vec<u8> {
    let bytes = value.to_le_bytes();
    let mut n = 8;
    while n > 1 {
        let top = bytes[n - 1];
        let next_sign = bytes[n - 2] & 0x80;
        if (top == 0 && next_sign == 0) || (top == 0xFF && next_sign != 0) {
            n -= 1;
        } else {
            break;
        }
    }
    bytes[..n].to_vec()
}

/// Encodes a runlist from absolute runs `(Some(lcn) | None for a hole, length)`.
pub fn runlist(runs: &[(Option<u64>, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0i64;
    for &(lcn, len) in runs {
        let len_bytes = signed_le(len as i64);
        match lcn {
            Some(lcn) => {
                let delta = signed_le(lcn as i64 - prev);
                prev = lcn as i64;
                out.push(((delta.len() as u8) << 4) | len_bytes.len() as u8);
                out.extend_from_slice(&len_bytes);
                out.extend_from_slice(&delta);
            }
            None => {
                out.push(len_bytes.len() as u8);
                out.extend_from_slice(&len_bytes);
            }
        }
    }
    out.push(0);
    out
}

/// One index entry for an `$INDEX_ROOT` or `INDX` node.
pub fn index_entry(
    child: u64,
    parent: u64,
    name: &str,
    namespace: u8,
    flags: u32,
    times: [u64; 4],
) -> Vec<u8> {
    let key = file_name_value(parent, name, namespace, flags, times);
    let mut e = vec![0u8; 0x10];
    e[0..8].copy_from_slice(&child.to_le_bytes());
    e[0x0A..0x0C].copy_from_slice(&(key.len() as u16).to_le_bytes());
    e.extend_from_slice(&key);
    pad8(&mut e);
    let len = e.len() as u16;
    e[8..10].copy_from_slice(&len.to_le_bytes());
    e
}

fn end_entry() -> Vec<u8> {
    let mut e = vec![0u8; 0x10];
    e[8..10].copy_from_slice(&0x10u16.to_le_bytes());
    e[0x0C..0x0E].copy_from_slice(&2u16.to_le_bytes());
    e
}

/// An `$INDEX_ROOT:$I30` value holding `entries`.
pub fn index_root_value(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut nodes = Vec::new();
    for e in entries {
        nodes.extend_from_slice(e);
    }
    nodes.extend_from_slice(&end_entry());
    let mut v = vec![0u8; 0x20];
    v[0..4].copy_from_slice(&0x30u32.to_le_bytes());
    v[4..8].copy_from_slice(&1u32.to_le_bytes());
    v[8..12].copy_from_slice(&4096u32.to_le_bytes());
    v[12] = 1;
    let used = (0x10 + nodes.len()) as u32;
    v[0x10..0x14].copy_from_slice(&0x10u32.to_le_bytes());
    v[0x14..0x18].copy_from_slice(&used.to_le_bytes());
    v[0x18..0x1C].copy_from_slice(&used.to_le_bytes());
    v.extend_from_slice(&nodes);
    v
}

/// Writes the update sequence array of a structure (`usa` at `usa_off`,
/// one entry per 512-byte block) and puts the USN into the block tails:
/// the form in which it is stored on disk.
pub fn protect(bytes: &mut [u8], usa_off: usize, usn: u16) {
    let blocks = bytes.len() / 512;
    bytes[usa_off..usa_off + 2].copy_from_slice(&usn.to_le_bytes());
    for i in 0..blocks {
        let tail = (i + 1) * 512 - 2;
        let saved = [bytes[tail], bytes[tail + 1]];
        let at = usa_off + 2 + 2 * i;
        bytes[at..at + 2].copy_from_slice(&saved);
        bytes[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
    }
}

/// Makes 512-byte block `block` of a protected structure fail the fixup check.
pub fn tear(bytes: &mut [u8], block: usize) {
    let tail = (block + 1) * 512 - 2;
    bytes[tail] ^= 0x5A;
    bytes[tail + 1] ^= 0xA5;
}

/// A `FILE` record under construction.
#[derive(Clone, Debug)]
pub struct Rec {
    pub size: usize,
    pub number: Option<u32>,
    pub sequence: u16,
    pub flags: u16,
    pub link_count: u16,
    pub lsn: u64,
    pub base: u64,
    pub usn: u16,
    pub attrs: Vec<Vec<u8>>,
    next_id: u16,
}

impl Rec {
    /// An in-use 1 KiB file record with this number, sequence 1.
    pub fn new(number: u32) -> Rec {
        Rec {
            size: 1024,
            number: Some(number),
            sequence: 1,
            flags: 1,
            link_count: 1,
            lsn: 0x1000 + u64::from(number),
            base: 0,
            usn: 7,
            attrs: Vec::new(),
            next_id: 0,
        }
    }

    pub fn size(mut self, size: usize) -> Rec {
        self.size = size;
        self
    }

    pub fn unnumbered(mut self) -> Rec {
        self.number = None;
        self
    }

    pub fn seq(mut self, sequence: u16) -> Rec {
        self.sequence = sequence;
        self
    }

    pub fn flags(mut self, flags: u16) -> Rec {
        self.flags = flags;
        self
    }

    pub fn links(mut self, links: u16) -> Rec {
        self.link_count = links;
        self
    }

    pub fn lsn(mut self, lsn: u64) -> Rec {
        self.lsn = lsn;
        self
    }

    pub fn base(mut self, base: u64) -> Rec {
        self.base = base;
        self
    }

    fn id(&mut self) -> u16 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn raw_attr(mut self, attr: Vec<u8>) -> Rec {
        self.attrs.push(attr);
        self
    }

    /// A 72-byte `$STANDARD_INFORMATION`.
    pub fn si(mut self, times: [u64; 4], file_attributes: u32) -> Rec {
        let mut v = Vec::new();
        for t in times {
            v.extend_from_slice(&t.to_le_bytes());
        }
        v.extend_from_slice(&file_attributes.to_le_bytes());
        v.resize(72, 0);
        let id = self.id();
        self.attrs.push(resident_attr(SI, "", id, 0, &v));
        self
    }

    pub fn name(self, parent: u64, name: &str, namespace: u8, flags: u32) -> Rec {
        self.name_at(parent, name, namespace, flags, [1, 2, 3, 4])
    }

    /// A `$FILE_NAME` with the given four times.
    pub fn name_at(
        mut self,
        parent: u64,
        name: &str,
        namespace: u8,
        flags: u32,
        times: [u64; 4],
    ) -> Rec {
        let v = file_name_value(parent, name, namespace, flags, times);
        let id = self.id();
        self.attrs.push(resident_attr(FILE_NAME, "", id, 0, &v));
        self
    }

    pub fn resident_data(mut self, content: &[u8]) -> Rec {
        let id = self.id();
        self.attrs.push(resident_attr(DATA, "", id, 0, content));
        self
    }

    pub fn named_data(mut self, name: &str, content: &[u8]) -> Rec {
        let id = self.id();
        self.attrs.push(resident_attr(DATA, name, id, 0, content));
        self
    }

    pub fn data_extent(
        mut self,
        start_vcn: u64,
        last_vcn: i64,
        sizes: (u64, u64, u64),
        runlist: &[u8],
        flags: u16,
    ) -> Rec {
        let id = self.id();
        self.attrs.push(nonresident_attr(
            DATA, "", id, flags, start_vcn, last_vcn, sizes, runlist,
        ));
        self
    }

    pub fn index_root(mut self, entries: &[Vec<u8>]) -> Rec {
        let v = index_root_value(entries);
        let id = self.id();
        self.attrs
            .push(resident_attr(INDEX_ROOT, "$I30", id, 0, &v));
        self
    }

    pub fn index_allocation(
        mut self,
        start_vcn: u64,
        last_vcn: i64,
        alloc: u64,
        runlist: &[u8],
    ) -> Rec {
        let id = self.id();
        self.attrs.push(nonresident_attr(
            INDEX_ALLOCATION,
            "$I30",
            id,
            0,
            start_vcn,
            last_vcn,
            (alloc, alloc, alloc),
            runlist,
        ));
        self
    }

    /// A resident `$ATTRIBUTE_LIST` with entries `(type, name, start VCN, holder, attribute id)`.
    pub fn attr_list(mut self, entries: &[(u32, &str, u64, u64, u16)]) -> Rec {
        let mut v = Vec::new();
        for &(type_code, name, start_vcn, holder, attr_id) in entries {
            let name = utf16(name);
            let mut e = vec![0u8; 0x1A];
            e[0..4].copy_from_slice(&type_code.to_le_bytes());
            e[6] = (name.len() / 2) as u8;
            e[7] = 0x1A;
            e[8..16].copy_from_slice(&start_vcn.to_le_bytes());
            e[16..24].copy_from_slice(&holder.to_le_bytes());
            e[24..26].copy_from_slice(&attr_id.to_le_bytes());
            e.extend_from_slice(&name);
            pad8(&mut e);
            let len = e.len() as u16;
            e[4..6].copy_from_slice(&len.to_le_bytes());
            v.extend_from_slice(&e);
        }
        let id = self.id();
        self.attrs
            .push(resident_attr(ATTRIBUTE_LIST, "", id, 0, &v));
        self
    }

    pub fn reparse(mut self, tag: u32) -> Rec {
        let mut v = tag.to_le_bytes().to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]);
        let id = self.id();
        self.attrs.push(resident_attr(REPARSE_POINT, "", id, 0, &v));
        self
    }

    /// The record as stored on disk (fixups applied in protected form).
    pub fn build(&self) -> Vec<u8> {
        let mut r = vec![0u8; self.size];
        let usa_off: usize = if self.number.is_some() { 0x30 } else { 0x2A };
        let usa_count = self.size / 512 + 1;
        let first_attr = (usa_off + 2 * usa_count + 7) & !7;
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&(usa_off as u16).to_le_bytes());
        r[6..8].copy_from_slice(&(usa_count as u16).to_le_bytes());
        r[8..16].copy_from_slice(&self.lsn.to_le_bytes());
        r[0x10..0x12].copy_from_slice(&self.sequence.to_le_bytes());
        r[0x12..0x14].copy_from_slice(&self.link_count.to_le_bytes());
        r[0x14..0x16].copy_from_slice(&(first_attr as u16).to_le_bytes());
        r[0x16..0x18].copy_from_slice(&self.flags.to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&(self.size as u32).to_le_bytes());
        r[0x20..0x28].copy_from_slice(&self.base.to_le_bytes());
        r[0x28..0x2A].copy_from_slice(&self.next_id.to_le_bytes());
        if let Some(number) = self.number {
            r[0x2C..0x30].copy_from_slice(&number.to_le_bytes());
        }
        let mut pos = first_attr;
        for a in &self.attrs {
            r[pos..pos + a.len()].copy_from_slice(a);
            pos += a.len();
        }
        r[pos..pos + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        r[pos + 4..pos + 8].copy_from_slice(&0x1147_7982u32.to_le_bytes());
        let used = (pos + 8) as u32;
        r[0x18..0x1C].copy_from_slice(&used.to_le_bytes());
        protect(&mut r, usa_off, self.usn);
        r
    }
}

/// A never-used record: `FILE`, not in use, no attributes.
pub fn never_used(number: u32) -> Vec<u8> {
    Rec::new(number).flags(0).links(0).build()
}

/// An `INDX` block of `size` bytes with the given entries.
pub fn indx_block(vcn: u64, lsn: u64, size: usize, entries: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0u8; size];
    let usa_off = 0x28usize;
    let usa_count = size / 512 + 1;
    let first = (usa_off + 2 * usa_count + 7) & !7;
    b[0..4].copy_from_slice(b"INDX");
    b[4..6].copy_from_slice(&(usa_off as u16).to_le_bytes());
    b[6..8].copy_from_slice(&(usa_count as u16).to_le_bytes());
    b[8..16].copy_from_slice(&lsn.to_le_bytes());
    b[0x10..0x18].copy_from_slice(&vcn.to_le_bytes());
    let mut nodes = Vec::new();
    for e in entries {
        nodes.extend_from_slice(e);
    }
    nodes.extend_from_slice(&end_entry());
    let entries_off = (first - 0x18) as u32;
    let used = entries_off + nodes.len() as u32;
    b[0x18..0x1C].copy_from_slice(&entries_off.to_le_bytes());
    b[0x1C..0x20].copy_from_slice(&used.to_le_bytes());
    b[0x20..0x24].copy_from_slice(&((size - 0x18) as u32).to_le_bytes());
    b[first..first + nodes.len()].copy_from_slice(&nodes);
    protect(&mut b, usa_off, 3);
    b
}

/// An NTFS boot sector.
#[allow(clippy::too_many_arguments)]
pub fn boot_sector(
    bytes_per_sector: u16,
    sectors_per_cluster_raw: u8,
    total_sectors: u64,
    mft_lcn: u64,
    mftmirr_lcn: u64,
    record_raw: i8,
    index_raw: i8,
    hidden_sectors: u32,
) -> Vec<u8> {
    let mut s = vec![0u8; 512];
    s[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    s[3..11].copy_from_slice(b"NTFS    ");
    s[0x0B..0x0D].copy_from_slice(&bytes_per_sector.to_le_bytes());
    s[0x0D] = sectors_per_cluster_raw;
    s[0x15] = 0xF8;
    s[0x1C..0x20].copy_from_slice(&hidden_sectors.to_le_bytes());
    s[0x28..0x30].copy_from_slice(&total_sectors.to_le_bytes());
    s[0x30..0x38].copy_from_slice(&mft_lcn.to_le_bytes());
    s[0x38..0x40].copy_from_slice(&mftmirr_lcn.to_le_bytes());
    s[0x40] = record_raw as u8;
    s[0x44] = index_raw as u8;
    s[0x48..0x50].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
    s[0x1FE] = 0x55;
    s[0x1FF] = 0xAA;
    s
}

/// A classic MBR with primary partitions `(start LBA, sectors, type)`.
pub fn mbr(partitions: &[(u32, u32, u8)]) -> Vec<u8> {
    let mut s = vec![0u8; 512];
    for (i, &(start, sectors, kind)) in partitions.iter().enumerate() {
        let e = 0x1BE + 16 * i;
        s[e] = 0x00;
        s[e + 4] = kind;
        s[e + 8..e + 12].copy_from_slice(&start.to_le_bytes());
        s[e + 12..e + 16].copy_from_slice(&sectors.to_le_bytes());
    }
    s[0x1FE] = 0x55;
    s[0x1FF] = 0xAA;
    s
}

/// A complete 64-byte PNG header (signature and a valid IHDR chunk).
pub fn png_head() -> Vec<u8> {
    let mut v = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    v.extend_from_slice(&[0, 0, 0, 0x0D, b'I', b'H', b'D', b'R']);
    v.extend_from_slice(&[
        0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0, 0x3A, 0x7E, 0x9B, 0x55,
    ]);
    v.extend_from_slice(&[0, 0, 0, 0x0A, b'I', b'D', b'A', b'T']);
    v.resize(64, 0);
    v
}
