//! The content of `$AttrDef`: the attribute definition table of NTFS 3.x
//! (`layout.h` `ATTR_DEF`: 160 bytes per attribute, terminated by an
//! all-zero entry). The values are the ones every NTFS 3.1 volume carries;
//! the integration test compares the result with what `mkntfs` writes.

use super::{put_u32, put_u64};

/// `(name, type, flags, minimum size, maximum size)`; display rule and
/// collation rule are 0 for all of them.
const DEFINITIONS: [(&str, u32, u32, u64, u64); 15] = [
    ("$STANDARD_INFORMATION", 0x10, 0x40, 0x30, 0x48),
    ("$ATTRIBUTE_LIST", 0x20, 0x80, 0, u64::MAX),
    ("$FILE_NAME", 0x30, 0x42, 0x44, 0x242),
    ("$OBJECT_ID", 0x40, 0x40, 0, 0x100),
    ("$SECURITY_DESCRIPTOR", 0x50, 0x80, 0, u64::MAX),
    ("$VOLUME_NAME", 0x60, 0x40, 2, 0x100),
    ("$VOLUME_INFORMATION", 0x70, 0x40, 0xC, 0xC),
    ("$DATA", 0x80, 0x00, 0, u64::MAX),
    ("$INDEX_ROOT", 0x90, 0x40, 0, u64::MAX),
    ("$INDEX_ALLOCATION", 0xA0, 0x80, 0, u64::MAX),
    ("$BITMAP", 0xB0, 0x80, 0, u64::MAX),
    ("$REPARSE_POINT", 0xC0, 0x80, 0, 0x4000),
    ("$EA_INFORMATION", 0xD0, 0x40, 8, 8),
    ("$EA", 0xE0, 0x00, 0, 0x1_0000),
    ("$LOGGED_UTILITY_STREAM", 0x100, 0x80, 0, 0x1_0000),
];

/// The 2560 bytes of `$AttrDef`.
pub fn attrdef_table() -> Vec<u8> {
    let mut out = vec![0u8; 160 * (DEFINITIONS.len() + 1)];
    for (i, &(name, type_code, flags, min, max)) in DEFINITIONS.iter().enumerate() {
        let entry = &mut out[160 * i..160 * (i + 1)];
        for (k, unit) in name.encode_utf16().enumerate() {
            entry[2 * k..2 * k + 2].copy_from_slice(&unit.to_le_bytes());
        }
        put_u32(entry, 0x80, type_code);
        put_u32(entry, 0x8C, flags);
        put_u64(entry, 0x90, min);
        put_u64(entry, 0x98, max);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_shape() {
        let t = attrdef_table();
        assert_eq!(t.len(), 2560);
        assert_eq!(&t[..8], &[b'$', 0, b'S', 0, b'T', 0, b'A', 0]);
        assert_eq!(&t[0x80..0x84], &0x10u32.to_le_bytes());
        // $FILE_NAME: indexable + resident, 68 to 578 bytes
        let fname = &t[2 * 160..3 * 160];
        assert_eq!(&fname[0x8C..0x90], &0x42u32.to_le_bytes());
        assert_eq!(&fname[0x90..0x98], &68u64.to_le_bytes());
        assert_eq!(&fname[0x98..0xA0], &578u64.to_le_bytes());
        // the terminator
        assert!(t[15 * 160..].iter().all(|&b| b == 0));
    }
}
