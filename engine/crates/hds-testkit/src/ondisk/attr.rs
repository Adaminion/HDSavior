//! Attribute values: `$STANDARD_INFORMATION`, `$FILE_NAME`,
//! `$ATTRIBUTE_LIST`, `$VOLUME_INFORMATION`, `$REPARSE_POINT` (notes
//! sections 6 to 8 and 11).

use super::{align8, put_u16, put_u32, put_u64, utf16_bytes, FileRef};

/// The NTFS 3.x tail of `$STANDARD_INFORMATION` (72-byte form).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiV3 {
    /// Owner id (0x30).
    pub owner_id: u32,
    /// Security id (0x34).
    pub security_id: u32,
    /// Quota charged (0x38).
    pub quota_charged: u64,
    /// Update sequence number of the change journal (0x40).
    pub usn: u64,
}

/// `$STANDARD_INFORMATION` value: 48 bytes without `v3`, 72 with it.
/// `times` are creation, modification, MFT change and access time.
pub fn standard_information(times: &[u64; 4], file_attributes: u32, v3: Option<SiV3>) -> Vec<u8> {
    let mut b = vec![0u8; if v3.is_some() { 72 } else { 48 }];
    for (i, &t) in times.iter().enumerate() {
        put_u64(&mut b, 8 * i, t);
    }
    put_u32(&mut b, 0x20, file_attributes);
    if let Some(v3) = v3 {
        put_u32(&mut b, 0x30, v3.owner_id);
        put_u32(&mut b, 0x34, v3.security_id);
        put_u64(&mut b, 0x38, v3.quota_charged);
        put_u64(&mut b, 0x40, v3.usn);
    }
    b
}

/// `$FILE_NAME` flag: the name belongs to a directory.
pub const FN_DIRECTORY: u32 = 0x1000_0000;
/// `$FILE_NAME` flag: the file has a view index.
pub const FN_VIEW_INDEX: u32 = 0x2000_0000;

/// Namespace of a file name (notes section 7).
pub mod namespace {
    #![allow(missing_docs)]
    pub const POSIX: u8 = 0;
    pub const WIN32: u8 = 1;
    pub const DOS: u8 = 2;
    pub const WIN32_AND_DOS: u8 = 3;
}

/// The fields of a `$FILE_NAME` value (also the key of a `$I30` entry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileNameValue {
    /// Parent directory reference.
    pub parent: FileRef,
    /// Creation, modification, MFT change and access time.
    pub times: [u64; 4],
    /// Allocated size of the unnamed stream at the time of writing.
    pub allocated_size: u64,
    /// Real size of the unnamed stream at the time of writing.
    pub real_size: u64,
    /// File attribute flags, plus [`FN_DIRECTORY`] for directories.
    pub flags: u32,
    /// EA size, or the reparse tag when the file is a reparse point (0x3C).
    pub ea_or_reparse: u32,
    /// Namespace: 0 POSIX, 1 Win32, 2 DOS, 3 Win32 + DOS.
    pub namespace: u8,
    /// The name in UTF-16 code units (1 to 255; unpaired surrogates are legal).
    pub name: Vec<u16>,
}

/// `$FILE_NAME` value: 0x42 bytes plus the name.
pub fn file_name(v: &FileNameValue) -> Vec<u8> {
    assert!(v.name.len() <= 255, "file name of {} units cannot be stored", v.name.len());
    let mut b = vec![0u8; 0x42 + 2 * v.name.len()];
    put_u64(&mut b, 0x00, v.parent.raw());
    for (i, &t) in v.times.iter().enumerate() {
        put_u64(&mut b, 0x08 + 8 * i, t);
    }
    put_u64(&mut b, 0x28, v.allocated_size);
    put_u64(&mut b, 0x30, v.real_size);
    put_u32(&mut b, 0x38, v.flags);
    put_u32(&mut b, 0x3C, v.ea_or_reparse);
    b[0x40] = v.name.len() as u8;
    b[0x41] = v.namespace;
    b[0x42..].copy_from_slice(&utf16_bytes(&v.name));
    b
}

/// One entry of an `$ATTRIBUTE_LIST` (notes section 8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListEntry {
    /// Type of the attribute the entry describes.
    pub type_code: u32,
    /// Name of that attribute.
    pub name: Vec<u16>,
    /// Lowest VCN of the extent (0 unless a non-resident attribute is split).
    pub lowest_vcn: u64,
    /// The record that holds the extent.
    pub holder: FileRef,
    /// Attribute id inside that record.
    pub id: u16,
}

/// The value of an `$ATTRIBUTE_LIST`: the entries in the given order, each
/// padded to 8 bytes (0x20 unnamed, 0x28 for `$I30`).
pub fn attribute_list(entries: &[ListEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        let length = align8(0x1A + 2 * e.name.len());
        let mut b = vec![0u8; length];
        put_u32(&mut b, 0x00, e.type_code);
        put_u16(&mut b, 0x04, length as u16);
        b[0x06] = e.name.len() as u8;
        b[0x07] = 0x1A;
        put_u64(&mut b, 0x08, e.lowest_vcn);
        put_u64(&mut b, 0x10, e.holder.raw());
        put_u16(&mut b, 0x18, e.id);
        b[0x1A..0x1A + 2 * e.name.len()].copy_from_slice(&utf16_bytes(&e.name));
        out.extend_from_slice(&b);
    }
    out
}

/// `$VOLUME_INFORMATION` value (12 bytes): NTFS version and volume flags.
pub fn volume_information(major: u8, minor: u8, flags: u16) -> Vec<u8> {
    let mut b = vec![0u8; 12];
    b[8] = major;
    b[9] = minor;
    put_u16(&mut b, 10, flags);
    b
}

/// `$REPARSE_POINT` value: tag, data length, reserved, data (notes section 11).
pub fn reparse_point(tag: u32, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 8 + data.len()];
    put_u32(&mut b, 0, tag);
    put_u16(&mut b, 4, data.len() as u16);
    b[8..].copy_from_slice(data);
    b
}

/// Reparse tags HDSavior has to recognise (notes section 11).
pub mod reparse_tag {
    #![allow(missing_docs)]
    pub const MOUNT_POINT: u32 = 0xA000_0003;
    pub const SYMLINK: u32 = 0xA000_000C;
    pub const DEDUP: u32 = 0x8000_0013;
    pub const WOF: u32 = 0x8000_0017;
    pub const CLOUD: u32 = 0x9000_001A;
    /// A cloud tag with a non-zero value in bits 12 to 15 (`0x9000101A` …
    /// `0x9000F01A` are all cloud-files tags).
    pub const CLOUD_6: u32 = 0x9000_601A;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_information_both_forms() {
        let times = [1, 2, 3, 4];
        let short = standard_information(&times, 0x26, None);
        assert_eq!(short.len(), 48);
        assert_eq!(&short[0x10..0x18], &3u64.to_le_bytes(), "MFT change time is the third");
        assert_eq!(&short[0x20..0x24], &[0x26, 0, 0, 0]);
        let long = standard_information(
            &times,
            0x20,
            Some(SiV3 { owner_id: 9, security_id: 0x107, quota_charged: 5, usn: 0x1122 }),
        );
        assert_eq!(long.len(), 72);
        assert_eq!(&long[..0x24], &{
            let mut v = short[..0x24].to_vec();
            v[0x20] = 0x20;
            v
        }[..]);
        assert_eq!(&long[0x30..0x34], &9u32.to_le_bytes());
        assert_eq!(&long[0x34..0x38], &0x107u32.to_le_bytes());
        assert_eq!(&long[0x38..0x40], &5u64.to_le_bytes());
        assert_eq!(&long[0x40..0x48], &0x1122u64.to_le_bytes());
    }

    #[test]
    fn file_name_layout() {
        // lone surrogates are legal name units and must be stored as they are
        let v = FileNameValue {
            parent: FileRef::new(0x1234, 7),
            times: [10, 20, 30, 40],
            allocated_size: 0x2000,
            real_size: 0x1234,
            flags: 0x20 | FN_DIRECTORY,
            ea_or_reparse: 0xA000_000C,
            namespace: namespace::WIN32,
            name: vec![0x41, 0xD800, 0x42],
        };
        let b = file_name(&v);
        assert_eq!(b.len(), 0x42 + 6);
        assert_eq!(&b[0..8], &[0x34, 0x12, 0, 0, 0, 0, 7, 0]);
        assert_eq!(&b[0x08..0x10], &10u64.to_le_bytes());
        assert_eq!(&b[0x18..0x20], &30u64.to_le_bytes());
        assert_eq!(&b[0x28..0x30], &0x2000u64.to_le_bytes());
        assert_eq!(&b[0x30..0x38], &0x1234u64.to_le_bytes());
        assert_eq!(&b[0x38..0x3C], &0x1000_0020u32.to_le_bytes());
        assert_eq!(&b[0x3C..0x40], &0xA000_000Cu32.to_le_bytes());
        assert_eq!(b[0x40], 3);
        assert_eq!(b[0x41], 1);
        assert_eq!(&b[0x42..], &[0x41, 0, 0x00, 0xD8, 0x42, 0]);
    }

    #[test]
    fn attribute_list_entries() {
        let i30: Vec<u16> = "$I30".encode_utf16().collect();
        let b = attribute_list(&[
            ListEntry { type_code: 0x10, name: vec![], lowest_vcn: 0, holder: FileRef::new(100, 2), id: 0 },
            ListEntry { type_code: 0x80, name: vec![], lowest_vcn: 468, holder: FileRef::new(101, 1), id: 0 },
            ListEntry { type_code: 0xA0, name: i30, lowest_vcn: 134, holder: FileRef::new(102, 3), id: 4 },
        ]);
        // unnamed entries are 0x20 bytes, a $I30 entry 0x28 (notes section 8)
        assert_eq!(b.len(), 0x20 + 0x20 + 0x28);
        assert_eq!(&b[0..8], &[0x10, 0, 0, 0, 0x20, 0, 0, 0x1A]);
        assert_eq!(&b[0x10..0x18], &[100, 0, 0, 0, 0, 0, 2, 0]);
        let second = &b[0x20..0x40];
        assert_eq!(&second[0..4], &0x80u32.to_le_bytes());
        assert_eq!(&second[8..16], &468u64.to_le_bytes());
        assert_eq!(&second[0x10..0x18], &[101, 0, 0, 0, 0, 0, 1, 0]);
        let third = &b[0x40..];
        assert_eq!(&third[4..8], &[0x28, 0, 4, 0x1A]);
        assert_eq!(&third[0x18..0x1A], &[4, 0]);
        assert_eq!(&third[0x1A..0x22], &[b'$', 0, b'I', 0, b'3', 0, b'0', 0]);
    }

    #[test]
    fn volume_information_and_reparse_point() {
        assert_eq!(volume_information(3, 1, 0), [0, 0, 0, 0, 0, 0, 0, 0, 3, 1, 0, 0]);
        // notes section 11: a symbolic link value starts "0C 00 00 A0 80 00 00 00"
        let b = reparse_point(reparse_tag::SYMLINK, &[0x55; 0x80]);
        assert_eq!(&b[..8], &[0x0C, 0x00, 0x00, 0xA0, 0x80, 0x00, 0x00, 0x00]);
        assert_eq!(b.len(), 0x88);
        // every cloud tag matches the documented mask
        for tag in [reparse_tag::CLOUD, reparse_tag::CLOUD_6] {
            assert_eq!(tag & 0xFFFF_0FFF, 0x9000_001A);
        }
    }
}
