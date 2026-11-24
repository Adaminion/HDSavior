//! 7z archives: the signature header locates and checksums the end header.
//!
//! Layout (7zFormat.txt): a 32-byte signature header
//!
//! ```text
//! '7' 'z' BC AF 27 1C   major(1) minor(1)   StartHeaderCRC(4)
//! NextHeaderOffset(8)   NextHeaderSize(8)   NextHeaderCRC(4)
//! ```
//!
//! then the packed streams, then the "next header" (the archive's
//! directory, usually itself compressed) at `32 + NextHeaderOffset`. The
//! archive ends where that header ends. `StartHeaderCRC` is the CRC-32 of
//! the 20 bytes after it; `NextHeaderCRC` is the CRC-32 of the next header.
//!
//! Both CRCs are verified: the first makes the 32 bytes at the start
//! trustworthy on their own (it is even checked in `sniff`), the second
//! proves that the end of the archive is where the start says it is. The
//! packed streams in between are skipped, not read. A next header too large
//! to be worth reading for its checksum is reported as `Probable`.

use crate::reader::Src;
use crate::util::{crc32, le32, le64, Crc32};
use crate::{Confidence, Measured};

pub(crate) const SIGNATURE: [u8; 6] = [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C];
/// Largest next header whose CRC is verified.
const MAX_CHECKED_HEADER: u64 = 64 * 1024 * 1024;

/// What a valid signature header says.
pub(crate) struct Start {
    next_offset: u64,
    next_size: u64,
    next_crc: u32,
}

/// Validates the 32-byte signature header, CRC included.
pub(crate) fn parse_start(h: &[u8]) -> Option<Start> {
    let h = h.get(..32)?;
    // format version 0.x (0.2 - 0.4 in the wild)
    if !h.starts_with(&SIGNATURE) || h[6] != 0 || h[7] > 9 {
        return None;
    }
    if le32(h, 8)? != crc32(h.get(12..32)?) {
        return None;
    }
    let start = Start {
        next_offset: le64(h, 12)?,
        next_size: le64(h, 20)?,
        next_crc: le32(h, 28)?,
    };
    // an empty archive is the signature header alone: nothing to rescue
    (start.next_size != 0).then_some(start)
}

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    let start = parse_start(&src.arr::<32>(0)?)?;
    let header = 32u64.checked_add(start.next_offset)?;
    let end = header.checked_add(start.next_size)?;
    if end > src.limit() {
        return None;
    }
    // the directory starts with kHeader (01) or kEncodedHeader (17)
    if !matches!(src.u8(header)?, 0x01 | 0x17) {
        return None;
    }
    let mut confidence = Confidence::Probable;
    if start.next_size <= MAX_CHECKED_HEADER {
        src.scan_mode();
        let mut crc = Crc32::new();
        let mut p = header;
        while p < end {
            let w = src.window(p);
            if w.is_empty() {
                return None;
            }
            let n = (w.len() as u64).min(end - p) as usize;
            crc.update(w.get(..n)?);
            p += n as u64;
        }
        if crc.finish() != start.next_crc {
            return None;
        }
        confidence = Confidence::Exact;
    }
    Some(Measured {
        len: end,
        confidence,
        ext: "7z",
        // the end header was read and its CRC matched
        end_verified: confidence == Confidence::Exact,
    })
}
