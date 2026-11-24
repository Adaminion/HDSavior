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
//! list. `Exact` means the block structure was followed from the header to
//! the trailer.
//!
//! There are no checksums. What vouches for the content is the LZW data of
//! every image, which is decoded as far as needed to count pixels: every
//! code must be one the table holds at that point, the codes must produce
//! exactly the image's `width × height` pixels, and the end-of-information
//! code must close them. Only then is the end verified. Foreign data where
//! a fragmented file continues breaks one of these (and a chain of
//! sub-blocks through it that happens to reach a trailer no longer
//! counts); a GIF whose encoder leaves out the end-of-information code or
//! writes a different number of pixels is measured all the same, without a
//! verified end.

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
    let mut lzw = Lzw::new();
    let mut verified = true;
    for _ in 0..MAX_BLOCKS {
        match src.u8(pos)? {
            0x2C => {
                let d = src.arr::<10>(pos)?;
                if le16(&d, 5)? == 0 || le16(&d, 7)? == 0 {
                    return None;
                }
                let pixels = u64::from(le16(&d, 5)?) * u64::from(le16(&d, 7)?);
                pos = pos.checked_add(10 + colour_table_len(d[9]))?;
                let min_code_size = src.u8(pos)?;
                if !(2..=11).contains(&min_code_size) {
                    return None;
                }
                let (end, whole) = image_data(src, pos + 1, &mut lzw, min_code_size, pixels)?;
                verified &= whole;
                pos = end;
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
                    // the trailer, reached by walking every block, behind
                    // images whose LZW data held exactly their pixels
                    end_verified: verified,
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

/// Walks the data sub-blocks of an image from `pos`, decoding the LZW codes
/// in them. Returns the offset after the terminator, and whether the codes
/// were valid, produced exactly `pixels` pixels and ended with the
/// end-of-information code.
fn image_data(
    src: &mut Src,
    mut pos: u64,
    lzw: &mut Lzw,
    min_code_size: u8,
    pixels: u64,
) -> Option<(u64, bool)> {
    lzw.start(min_code_size, pixels);
    loop {
        let n = src.u8(pos)?;
        pos += 1;
        if n == 0 {
            return Some((pos, lzw.finish()));
        }
        // the sub-block, which may span two buffer refills
        let mut left = usize::from(n);
        while left > 0 {
            let w = src.window(pos);
            if w.is_empty() {
                return None;
            }
            let here = w.get(..left).unwrap_or(w);
            lzw.feed(here);
            left -= here.len();
            pos += here.len() as u64;
        }
        lzw.end_sub_block();
    }
}

/// Just enough of a GIF LZW decoder to check the code stream: the string
/// table is kept as lengths only.
struct Lzw {
    /// Length in pixels of the string of each code.
    lens: Box<[u16; 4096]>,
    min_size: u32,
    size: u32,
    next: u32,
    prev: Option<u32>,
    acc: u32,
    bits: u32,
    pixels: u64,
    expected: u64,
    ended: bool,
    /// A sub-block ended after the end code.
    closed: bool,
    bad: bool,
}

impl Lzw {
    fn new() -> Lzw {
        Lzw {
            lens: Box::new([1; 4096]),
            min_size: 2,
            size: 3,
            next: 6,
            prev: None,
            acc: 0,
            bits: 0,
            pixels: 0,
            expected: 0,
            ended: false,
            closed: false,
            bad: false,
        }
    }

    fn start(&mut self, min_code_size: u8, pixels: u64) {
        self.min_size = u32::from(min_code_size);
        self.acc = 0;
        self.bits = 0;
        self.pixels = 0;
        self.expected = pixels;
        self.ended = false;
        self.closed = false;
        // a code size above 8 cannot come from an 8-bit palette
        self.bad = self.min_size > 8;
        // the single pixels (an earlier image may have used these codes
        // for longer strings)
        let roots = (1usize << self.min_size.min(11)).min(self.lens.len());
        if let Some(r) = self.lens.get_mut(..roots) {
            r.fill(1);
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.size = self.min_size + 1;
        self.next = (1 << self.min_size) + 2;
        self.prev = None;
    }

    fn feed(&mut self, data: &[u8]) {
        if self.bad {
            return;
        }
        if self.ended {
            // Some encoders flush a byte more than the end code needs; the
            // rest of its sub-block is ignored, as decoders do. A further
            // sub-block is data the image does not account for.
            self.bad |= self.closed;
            return;
        }
        for &b in data {
            if self.ended || self.bad {
                return;
            }
            self.acc |= u32::from(b) << self.bits;
            self.bits += 8;
            while self.bits >= self.size && !self.ended && !self.bad {
                let code = self.acc & ((1 << self.size) - 1);
                self.acc >>= self.size;
                self.bits -= self.size;
                self.code(code);
            }
        }
    }

    fn end_sub_block(&mut self) {
        self.closed = self.ended;
    }

    fn code(&mut self, code: u32) {
        let clear = 1u32 << self.min_size;
        if code == clear {
            self.reset();
            return;
        }
        if code == clear + 1 {
            self.ended = true;
            return;
        }
        let len = match self.prev {
            // after a clear only a single pixel can come
            None if code < clear => 1,
            None => {
                self.bad = true;
                return;
            }
            Some(prev) => {
                let prev_len = u32::from(self.lens[prev as usize]);
                let len = if code < self.next {
                    u32::from(self.lens[code as usize])
                } else if code == self.next && self.next < 4096 {
                    // the string about to be defined: prev + its first pixel
                    prev_len + 1
                } else {
                    self.bad = true;
                    return;
                };
                if self.next < 4096 {
                    self.lens[self.next as usize] = (prev_len + 1).min(4096) as u16;
                    self.next += 1;
                    if self.next == 1 << self.size && self.size < 12 {
                        self.size += 1;
                    }
                }
                len
            }
        };
        self.prev = Some(code);
        self.pixels += u64::from(len);
        if self.pixels > self.expected {
            self.bad = true;
        }
    }

    /// Whether the image's data was a complete, valid code stream.
    fn finish(&mut self) -> bool {
        !self.bad && self.ended && self.pixels == self.expected
    }
}
