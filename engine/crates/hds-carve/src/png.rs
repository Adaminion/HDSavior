//! PNG (ISO/IEC 15948): chunk walk with CRC verification through `IEND`.
//!
//! Layout: 8-byte signature, then chunks `length(4, BE) type(4) data(length)
//! crc(4)`; the CRC-32 covers type and data. The first chunk is `IHDR`
//! (13 bytes), the last is `IEND` (empty). APNG, text, colour and vendor
//! chunks are ordinary chunks.
//!
//! Every chunk's CRC is verified, which means the whole file is read. That is
//! affordable (PNG files are megabytes, the read is one sequential pass) and
//! makes an `Exact` result here proof of an undamaged, unfragmented file - the
//! only image format in this crate that offers this.

use crate::reader::Src;
use crate::util::{be32, Crc32};
use crate::{Confidence, Measured};

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const MAX_CHUNKS: u32 = 1 << 22;

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    src.scan_mode();
    if src.arr::<8>(0)? != SIGNATURE {
        return None;
    }
    let mut pos: u64 = 8;
    let mut seen_ihdr = false;
    let mut seen_idat = false;
    for _ in 0..MAX_CHUNKS {
        let head = src.arr::<8>(pos)?;
        let len = be32(&head, 0)? as u64;
        let typ: [u8; 4] = [head[4], head[5], head[6], head[7]];
        if len > 0x7FFF_FFFF || !typ.iter().all(u8::is_ascii_alphabetic) {
            return None;
        }
        let data = pos.checked_add(8)?;
        let data_end = data.checked_add(len)?;
        let end = data_end.checked_add(4)?;
        // A chunk that cannot fit is rejected before a byte of it is read.
        if end > src.limit() {
            return None;
        }
        if seen_ihdr == (&typ == b"IHDR") {
            // IHDR must come first, and only once
            return None;
        }
        if &typ == b"IHDR" {
            if len != 13 || !ihdr_ok(src.bytes(data, 13)?) {
                return None;
            }
            seen_ihdr = true;
        }

        let mut crc = Crc32::new();
        crc.update(&typ);
        let mut p = data;
        while p < data_end {
            let w = src.window(p);
            if w.is_empty() {
                return None;
            }
            let n = (w.len() as u64).min(data_end - p) as usize;
            crc.update(w.get(..n)?);
            p += n as u64;
        }
        if src.be32(data_end)? != crc.finish() {
            return None;
        }

        match &typ {
            b"IDAT" => seen_idat = true,
            b"IEND" => {
                if len != 0 || !seen_idat {
                    return None;
                }
                return Some(Measured { len: end, confidence: Confidence::Exact, ext: "png" });
            }
            _ => {}
        }
        pos = end;
    }
    None
}

/// Validates the 13 data bytes of `IHDR`.
pub(crate) fn ihdr_ok(d: &[u8]) -> bool {
    let (Some(w), Some(h)) = (be32(d, 0), be32(d, 4)) else {
        return false;
    };
    let Some(&[depth, colour, compression, filter, interlace]) = d.get(8..13).and_then(|s| <&[u8; 5]>::try_from(s).ok())
    else {
        return false;
    };
    let depth_ok = match colour {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8),
        2 | 4 | 6 => matches!(depth, 8 | 16),
        _ => false,
    };
    (1..=0x7FFF_FFFF).contains(&w)
        && (1..=0x7FFF_FFFF).contains(&h)
        && depth_ok
        && compression == 0
        && filter == 0
        && interlace <= 1
}
