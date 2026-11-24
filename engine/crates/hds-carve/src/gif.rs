//! GIF (87a / 89a): block walk to the trailer.
//!
//! Layout: `GIF87a` or `GIF89a`, a 7-byte logical screen descriptor, an
//! optional global colour table, then blocks until the trailer byte `3B`:
//!
//! * `2C` image descriptor (9 bytes), optional local colour table, one byte
//!   LZW minimum code size, then data sub-blocks;
//! * `21` extension: a label byte, then data sub-blocks.
//!
//! A sub-block is a length byte followed by that many bytes; length 0 ends the
//! list. There are no checksums, and the LZW data is not decoded: `Exact`
//! means the block structure was followed from the header to the trailer.

use crate::reader::Src;
use crate::util::le16;
use crate::{Confidence, Measured};

const MAX_BLOCKS: u32 = 1 << 22;

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    src.scan_mode();
    let head = src.arr::<13>(0)?;
    if !(head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a")) {
        return None;
    }
    if le16(&head, 6)? == 0 || le16(&head, 8)? == 0 {
        return None;
    }
    let mut pos: u64 = 13 + colour_table_len(head[10]);
    let mut images = 0u32;
    for _ in 0..MAX_BLOCKS {
        match src.u8(pos)? {
            0x2C => {
                let d = src.arr::<10>(pos)?;
                if le16(&d, 5)? == 0 || le16(&d, 7)? == 0 {
                    return None;
                }
                pos = pos.checked_add(10 + colour_table_len(d[9]))?;
                if !(2..=11).contains(&src.u8(pos)?) {
                    return None;
                }
                pos = skip_sub_blocks(src, pos + 1)?;
                images += 1;
            }
            0x21 => {
                // graphic control, comment, plain text, application
                if !matches!(src.u8(pos + 1)?, 0xF9 | 0xFE | 0x01 | 0xFF) {
                    return None;
                }
                pos = skip_sub_blocks(src, pos + 2)?;
            }
            0x3B => {
                if images == 0 {
                    return None;
                }
                return Some(Measured {
                    len: pos + 1,
                    confidence: Confidence::Exact,
                    ext: "gif",
                    // the trailer, reached by walking every block
                    end_verified: true,
                });
            }
            _ => return None,
        }
    }
    None
}

/// Size of a colour table announced by a packed-fields byte.
fn colour_table_len(packed: u8) -> u64 {
    if packed & 0x80 != 0 {
        3 << ((packed & 7) + 1)
    } else {
        0
    }
}

/// Walks data sub-blocks from `pos`; returns the offset after the terminator.
fn skip_sub_blocks(src: &mut Src, mut pos: u64) -> Option<u64> {
    loop {
        let w = src.window(pos);
        if w.is_empty() {
            return None;
        }
        let mut i = 0usize;
        // `i` may step past the window; the sub-block then continues in the
        // next one. Every round consumes at least one byte.
        while let Some(&n) = w.get(i) {
            if n == 0 {
                return Some(pos + i as u64 + 1);
            }
            i += 1 + n as usize;
        }
        pos += i as u64;
    }
}
