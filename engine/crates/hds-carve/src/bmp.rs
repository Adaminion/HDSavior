//! Windows / OS/2 bitmap: the length comes from the header.
//!
//! Layout: 14-byte file header (`BM`, file size, two reserved words, offset of
//! the pixel array) followed by a DIB header whose own size identifies the
//! variant (12 = OS/2 core, 40 = `BITMAPINFOHEADER`, 52/56 = with masks,
//! 108 = V4, 124 = V5, 64 = OS/2 2.x), colour table, pixel array (rows padded
//! to 32 bits) and, for V5 only, an optional ICC profile.
//!
//! BMP has no end marker and no checksum. For uncompressed pixel data the
//! size can be computed twice, from the file-size field and from the
//! dimensions; when both agree the result is `Exact`. For run-length encoded
//! data the header's size fields are all there is: `Probable`.

use crate::reader::Src;
use crate::util::{le16, le32};
use crate::{Confidence, Measured};

/// What the 14 + 40 header bytes say about a bitmap.
pub(crate) struct Header {
    file_size: u64,
    data_offset: u64,
    dib_size: u32,
    width: u64,
    height: u64,
    bpp: u16,
    compression: u32,
    image_size: u64,
}

/// Parses and sanity-checks a BMP header from the first bytes of a file.
/// Needs 30 bytes for a core header and 38 for the others.
pub(crate) fn parse_header(h: &[u8]) -> Option<Header> {
    if h.get(..2)? != b"BM" {
        return None;
    }
    let file_size = le32(h, 2)? as u64;
    // reserved, must be zero
    if le32(h, 6)? != 0 {
        return None;
    }
    let data_offset = le32(h, 10)? as u64;
    let dib_size = le32(h, 14)?;
    let (width, height, planes, bpp, compression, image_size) = match dib_size {
        12 => (le16(h, 18)? as u64, le16(h, 20)? as u64, le16(h, 22)?, le16(h, 24)?, 0, 0),
        40 | 52 | 56 | 64 | 108 | 124 => {
            let w = le32(h, 18)? as i32;
            let hgt = le32(h, 22)? as i32;
            // negative height = top-down rows; negative width is invalid
            if w <= 0 || hgt == 0 || hgt == i32::MIN {
                return None;
            }
            (w as u64, hgt.unsigned_abs() as u64, le16(h, 26)?, le16(h, 28)?, le32(h, 30)?, le32(h, 34)? as u64)
        }
        _ => return None,
    };
    if planes != 1 || width == 0 || height == 0 || width > 1 << 20 || height > 1 << 20 {
        return None;
    }
    let bpp_ok = match compression {
        0 => matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32) || (dib_size == 64 && bpp == 2),
        1 => bpp == 8,
        2 => bpp == 4,
        3 | 6 => matches!(bpp, 16 | 32),
        _ => false,
    };
    if !bpp_ok || data_offset < 14 + dib_size as u64 {
        return None;
    }
    Some(Header { file_size, data_offset, dib_size, width, height, bpp, compression, image_size })
}

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    let (head, n) = src.side_peek::<142>(0);
    let head = head.get(..n)?;
    let h = parse_header(head)?;
    let exact = |len| Measured { len, confidence: Confidence::Exact, ext: "bmp" };
    let probable = |len| Measured { len, confidence: Confidence::Probable, ext: "bmp" };

    if matches!(h.compression, 1 | 2) {
        // RLE: only the header can say how long the data is
        if h.image_size == 0 {
            return None;
        }
        let end = h.data_offset.checked_add(h.image_size)?;
        return match h.file_size {
            s if s == end => Some(probable(end)),
            // some writers round the file up to a multiple of four
            s if s > end && s - end <= 4 => Some(probable(s)),
            _ => None,
        };
    }

    // width, height <= 2^20 and bpp <= 32: no overflow
    let row = (h.width * h.bpp as u64).div_ceil(32) * 4;
    let mut end = h.data_offset.checked_add(row * h.height)?;
    // V5: an embedded ICC profile may follow the pixels (offset is relative
    // to the start of the DIB header)
    if h.dib_size == 124 && le32(head, 14 + 56) == Some(0x4D42_4544) {
        if let (Some(off), Some(size)) = (le32(head, 14 + 112), le32(head, 14 + 116)) {
            if size != 0 {
                end = end.max(14u64.checked_add(off as u64)?.checked_add(size as u64)?);
            }
        }
    }
    match h.file_size {
        s if s == end => Some(exact(end)),
        // trailing padding written by some encoders (two bytes by Photoshop,
        // alignment by others): take the header's word, without certainty
        s if s > end && s - end <= 1024 => Some(probable(s)),
        // file-size field missing or too small: the dimensions decide
        s if s < end => Some(probable(end)),
        _ => None,
    }
}
