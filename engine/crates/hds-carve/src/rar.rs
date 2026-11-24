//! RAR archives, formats 1.5-4.x ("RAR4") and 5.0+ ("RAR5"): block walk
//! with header checksums up to the end-of-archive block.
//!
//! **RAR4** (`Rar!\x1A\x07\x00`) is a chain of blocks, each starting with
//!
//! ```text
//! HEAD_CRC(2)  HEAD_TYPE(1)  HEAD_FLAGS(2)  HEAD_SIZE(2)  [ADD_SIZE(4)]
//! ```
//!
//! `HEAD_SIZE` covers the whole header; data follows for file (`74`) and
//! service (`7A`) blocks (`PACK_SIZE`, plus a high half when flag `0100` is
//! set) and for any block with flag `8000` (`ADD_SIZE`). `HEAD_CRC` is the
//! low half of the CRC-32 of the header from `HEAD_TYPE` on. Block `73` is
//! the archive header, `7B` ends the archive.
//!
//! **RAR5** (`Rar!\x1A\x07\x01\x00`) blocks are
//!
//! ```text
//! CRC32(4)  header_size(vint)  type(vint)  flags(vint)
//! [extra_size(vint) if flags & 1]  [data_size(vint) if flags & 2]  ...
//! ```
//!
//! where `header_size` counts from `type` and the CRC covers `header_size`
//! and the header. Type 1 is the archive header, 2 a file, 3 a service
//! block (comments, recovery record, quick-open data), 4 the encryption
//! header, 5 the end of the archive.
//!
//! Every header checksum is verified; packed data is skipped, not read. The
//! walk must reach the end block for `Exact`. RAR4 archives made by very old
//! versions (or with `-en`) have no end block: the walk then stops at the
//! first position that is not a block with a valid checksum, and the result
//! is `Probable`. Archives with encrypted headers (`-hp`) cannot be walked
//! and are not measured. Each volume of a multi-volume set is an archive of
//! its own.

use crate::reader::Src;
use crate::util::{crc32, le16, le32};
use crate::{Confidence, Measured};

pub(crate) const SIGNATURE_V4: &[u8; 7] = b"Rar!\x1A\x07\x00";
pub(crate) const SIGNATURE_V5: &[u8; 8] = b"Rar!\x1A\x07\x01\x00";

const MAX_BLOCKS: u32 = 1 << 21;
/// RAR5 readers refuse headers above 2 MiB.
const MAX_HEADER_V5: u64 = 2 * 1024 * 1024;

/// A RAR5 variable-length integer (7 bits per byte, low bits first, at most
/// 10 bytes): the value and the bytes it occupies.
pub(crate) fn vint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &c) in b.iter().take(10).enumerate() {
        v |= ((c & 0x7F) as u64).checked_shl(7 * i as u32)?;
        if c & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    let sig = src.arr::<8>(0)?;
    if &sig == SIGNATURE_V5 {
        measure_v5(src)
    } else if sig.starts_with(SIGNATURE_V4) {
        measure_v4(src)
    } else {
        None
    }
}

fn measure_v4(src: &mut Src) -> Option<Measured> {
    let limit = src.limit();
    let mut pos = 7u64;
    let mut main = false;
    let mut files = 0u32;
    let mut blocks = 0u32;
    loop {
        blocks += 1;
        if blocks > MAX_BLOCKS {
            return None;
        }
        let Some(block) = block_v4(src, pos, limit) else {
            break;
        };
        match block.kind {
            0x73 => {
                // flag 0080: block headers are encrypted from here on
                if main || block.flags & 0x0080 != 0 {
                    return None;
                }
                main = true;
            }
            _ if !main => return None,
            0x74 => files += 1,
            0x7B => {
                return Some(Measured {
                    len: block.end,
                    confidence: Confidence::Exact,
                    ext: "rar",
                    // the end-of-archive block, its header CRC verified
                    end_verified: true,
                });
            }
            _ => {}
        }
        pos = block.end;
    }
    // no end block: accept what was walked if it held at least one file
    // and all of it exists
    if files == 0 || !src.has_byte(pos - 1) {
        return None;
    }
    Some(Measured {
        len: pos,
        confidence: Confidence::Probable,
        ext: "rar",
        // no end block: more blocks may have followed
        end_verified: false,
    })
}

struct BlockV4 {
    kind: u8,
    flags: u16,
    end: u64,
}

/// Reads and verifies the RAR4 block at `pos`.
fn block_v4(src: &mut Src, pos: u64, limit: u64) -> Option<BlockV4> {
    let h = src.arr::<7>(pos)?;
    let stored_crc = le16(&h, 0)?;
    let kind = h[2];
    let flags = le16(&h, 3)?;
    let size = le16(&h, 5)? as usize;
    if !(0x73..=0x7B).contains(&kind) || size < 7 {
        return None;
    }
    let header = src.bytes(pos, size)?;
    // Old-style comments embedded in the archive or file header are not
    // covered by the checksum; where they are flagged it is not checked.
    let embedded_comment = matches!(
        (kind, flags & 0x0002, flags & 0x0008),
        (0x73, 2, _) | (0x74, _, 8)
    );
    if !embedded_comment && crc32(header.get(2..)?) as u16 != stored_crc {
        return None;
    }
    let data = match kind {
        // file and service blocks: PACK_SIZE, and its high half with flag 0100
        0x74 | 0x7A => {
            let low = le32(header, 7)? as u64;
            let high = if flags & 0x0100 != 0 {
                le32(header, 32)? as u64
            } else {
                0
            };
            high << 32 | low
        }
        _ if flags & 0x8000 != 0 => le32(header, 7)? as u64,
        _ => 0,
    };
    let end = pos.checked_add(size as u64)?.checked_add(data)?;
    (end <= limit).then_some(BlockV4 { kind, flags, end })
}

fn measure_v5(src: &mut Src) -> Option<Measured> {
    let limit = src.limit();
    let mut pos = 8u64;
    let mut main = false;
    for _ in 0..MAX_BLOCKS {
        let stored_crc = src.le32(pos)?;
        let (size, size_len) = vint(src.peek(pos.checked_add(4)?, 10))?;
        if size == 0 || size > MAX_HEADER_V5 {
            return None;
        }
        // the checksum covers the size field and the header proper
        let covered = src.bytes(pos + 4, size_len + size as usize)?;
        if crc32(covered) != stored_crc {
            return None;
        }
        let mut fields = covered.get(size_len..)?;
        let mut next = || {
            let (v, n) = vint(fields)?;
            fields = fields.get(n..)?;
            Some(v)
        };
        let kind = next()?;
        let flags = next()?;
        if flags & 1 != 0 {
            next()?;
        }
        let data = if flags & 2 != 0 { next()? } else { 0 };
        let end = pos
            .checked_add(4 + size_len as u64 + size)?
            .checked_add(data)?;
        if end > limit {
            return None;
        }
        match kind {
            1 if !main => main = true,
            // a second archive header, or anything before the first
            1 => return None,
            _ if !main => return None,
            // archive encryption header: everything after it is ciphertext
            4 => return None,
            5 => {
                return Some(Measured {
                    len: end,
                    confidence: Confidence::Exact,
                    ext: "rar",
                    // the end-of-archive block, its header CRC verified
                    end_verified: true,
                });
            }
            _ => {}
        }
        pos = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_length_integers() {
        assert_eq!(vint(&[0x00]), Some((0, 1)));
        assert_eq!(vint(&[0x7F, 0xFF]), Some((127, 1)));
        assert_eq!(vint(&[0x80, 0x01]), Some((128, 2)));
        assert_eq!(vint(&[0xE1, 0xCE, 0x80, 0x00]), Some((0x2761, 4)));
        assert_eq!(
            vint(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]),
            Some((u64::MAX, 10))
        );
        assert_eq!(vint(&[0x80; 10]), None, "never terminated");
        assert_eq!(vint(&[0x80]), None);
        assert_eq!(vint(&[]), None);
    }
}
