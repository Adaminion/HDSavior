//! The end of a raw DEFLATE stream (RFC 1951), without inflating it.
//!
//! A ZIP entry written by a streaming compressor has no size in its local
//! header; the size follows the data in a descriptor. Searching the data
//! for something that looks like a descriptor reads up to the work limit
//! when there is none - when the archive is fragmented, or the candidate
//! is no archive at all. DEFLATE data, the method nearly every streamed
//! entry uses, delimits itself: its blocks are walked here, symbol by
//! symbol, to the end of the final block, and the descriptor must be right
//! there. Foreign data breaks the walk within a few hundred bytes as a
//! rule (an invalid block type, an over-subscribed code, a symbol that
//! does not exist, a distance reaching back before the start).
//!
//! Only lengths are tracked: the output is counted, never produced.

use crate::reader::Src;

/// Longest code (RFC 1951 3.2.7).
const MAX_BITS: usize = 15;
/// Bits resolved by one table look-up.
const FAST_BITS: u32 = 9;
/// Window of DEFLATE: no distance reaches further back.
const WINDOW: u64 = 32 * 1024;

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// Order of the code length code lengths in a dynamic block header.
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// A canonical Huffman code, decoded least significant bit first.
struct Code {
    /// For the next `FAST_BITS` bits: `length << 9 | symbol` of the code
    /// they start with; 0 when the code is longer (or there is none).
    fast: [u16; 1 << FAST_BITS],
    /// Number of codes of each length.
    count: [u16; MAX_BITS + 1],
    /// Symbols ordered by code.
    symbols: [u16; 288],
}

impl Code {
    /// The code with these lengths (0 = symbol unused); `None` when the
    /// lengths are over-subscribed, or incomplete other than as a single
    /// one-bit code (as zlib decides).
    fn new(lengths: &[u8]) -> Option<Box<Code>> {
        let mut c = Box::new(Code {
            fast: [0; 1 << FAST_BITS],
            count: [0; MAX_BITS + 1],
            symbols: [0; 288],
        });
        if lengths.len() > c.symbols.len() {
            return None;
        }
        for &l in lengths {
            *c.count.get_mut(usize::from(l))? += 1;
        }
        c.count[0] = 0;
        let mut left: i32 = 1;
        let mut max = 0usize;
        for len in 1..=MAX_BITS {
            left <<= 1;
            left -= i32::from(c.count[len]);
            if left < 0 {
                return None;
            }
            if c.count[len] != 0 {
                max = len;
            }
        }
        if left > 0 && max != 1 && max != 0 {
            return None;
        }
        // symbols in code order, and the first code of each length
        let mut offs = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offs[len + 1] = offs[len] + c.count[len];
        }
        // the first code of each length (RFC 1951 3.2.2)
        let mut next_code = [0u32; MAX_BITS + 1];
        let mut first = 0u32;
        for (next, &count) in next_code.iter_mut().zip(c.count.iter()).skip(1) {
            *next = first;
            first = (first + u32::from(count)) << 1;
        }
        for (sym, &l) in lengths.iter().enumerate() {
            if l == 0 {
                continue;
            }
            let len = usize::from(l);
            let at = usize::from(offs[len]);
            *c.symbols.get_mut(at)? = sym as u16;
            offs[len] += 1;
            let code = next_code[len];
            next_code[len] += 1;
            if l as u32 <= FAST_BITS {
                // the code is sent most significant bit first: reverse it
                let rev = reverse(code, u32::from(l));
                let mut i = rev as usize;
                while i < c.fast.len() {
                    c.fast[i] = (u16::from(l) << 9) | sym as u16;
                    i += 1 << l;
                }
            }
        }
        Some(c)
    }

    /// Decodes one symbol.
    fn decode(&self, bits: &mut Bits, src: &mut Src) -> Option<u16> {
        bits.need(MAX_BITS as u32, src);
        let e = self.fast[(bits.acc & ((1 << FAST_BITS) - 1)) as usize];
        if e != 0 {
            bits.drop(u32::from(e >> 9))?;
            return Some(e & 0x1FF);
        }
        // bit by bit (RFC 1951 3.2.2; as zlib's `puff`)
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAX_BITS {
            code |= ((bits.acc >> (len - 1)) & 1) as i32;
            let count = i32::from(self.count[len]);
            if code - count < first {
                bits.drop(len as u32)?;
                return self.symbols.get((index + code - first) as usize).copied();
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        None
    }
}

fn reverse(code: u32, len: u32) -> u32 {
    code.reverse_bits() >> (32 - len)
}

/// Least-significant-bit-first reader over a [`Src`].
struct Bits {
    acc: u64,
    n: u32,
    /// Next byte to load.
    pos: u64,
}

impl Bits {
    /// Loads bytes until at least `k` bits are held, or the data ends.
    fn need(&mut self, k: u32, src: &mut Src) {
        while self.n < k {
            let w = src.window(self.pos);
            let mut i = 0usize;
            while self.n <= 56 {
                let Some(&b) = w.get(i) else { break };
                self.acc |= u64::from(b) << self.n;
                self.n += 8;
                i += 1;
            }
            if i == 0 {
                return;
            }
            self.pos += i as u64;
        }
    }

    fn drop(&mut self, k: u32) -> Option<()> {
        if k > self.n {
            return None;
        }
        self.acc = self.acc.checked_shr(k).unwrap_or(0);
        self.n -= k;
        Some(())
    }

    /// `k` bits (at most 16) as a number.
    fn get(&mut self, k: u32, src: &mut Src) -> Option<u32> {
        self.need(k, src);
        let v = (self.acc & ((1u64 << k) - 1)) as u32;
        self.drop(k)?;
        Some(v)
    }

    /// Drops the bits up to the next byte boundary.
    fn align(&mut self) {
        let k = self.n % 8;
        self.acc >>= k;
        self.n -= k;
    }

    /// Offset of the first byte none of whose bits were used.
    fn offset(&self) -> u64 {
        self.pos - u64::from(self.n / 8)
    }
}

/// Walks the DEFLATE stream that starts at `start`; returns the offset just
/// behind its final block, or `None` when the bytes are not a complete,
/// valid stream (or the data ends first). Every symbol takes at least one
/// bit, so the walk is bounded by the data it reads.
pub(crate) fn end(src: &mut Src, start: u64) -> Option<u64> {
    let mut bits = Bits {
        acc: 0,
        n: 0,
        pos: start,
    };
    let fixed_lit = {
        let mut l = [0u8; 288];
        for (i, v) in l.iter_mut().enumerate() {
            *v = match i {
                0..=143 => 8,
                144..=255 => 9,
                256..=279 => 7,
                _ => 8,
            };
        }
        Code::new(&l)?
    };
    // 32 five-bit codes, of which 30 and 31 never occur (RFC 1951 3.2.6)
    let fixed_dist = Code::new(&[5u8; 32])?;
    let mut out: u64 = 0;
    loop {
        let last = bits.get(1, src)?;
        match bits.get(2, src)? {
            0 => {
                bits.align();
                let len = bits.get(16, src)?;
                let nlen = bits.get(16, src)?;
                if len != !nlen & 0xFFFF {
                    return None;
                }
                // the bytes themselves need not be looked at; the bit
                // buffer is empty at a byte boundary after the two words
                // unless it held more than them
                let held = u64::from(bits.n / 8);
                let skip = u64::from(len);
                if held >= skip {
                    bits.drop(len * 8)?;
                } else {
                    bits.acc = 0;
                    bits.n = 0;
                    bits.pos = bits.pos.checked_add(skip - held)?;
                }
                // the stored bytes must exist
                if len > 0 && !src.has_byte(bits.pos.checked_sub(1)?) {
                    return None;
                }
                out = out.saturating_add(skip);
            }
            1 => block(&mut bits, src, &fixed_lit, &fixed_dist, &mut out)?,
            2 => {
                let (lit, dist) = dynamic_codes(&mut bits, src)?;
                block(&mut bits, src, &lit, &dist, &mut out)?;
            }
            _ => return None,
        }
        if last == 1 {
            return Some(bits.offset());
        }
    }
}

/// Reads the code definitions of a dynamic block.
fn dynamic_codes(bits: &mut Bits, src: &mut Src) -> Option<(Box<Code>, Box<Code>)> {
    let nlen = bits.get(5, src)? as usize + 257;
    let ndist = bits.get(5, src)? as usize + 1;
    let ncode = bits.get(4, src)? as usize + 4;
    if nlen > 286 || ndist > 30 {
        return None;
    }
    let mut cl = [0u8; 19];
    for &i in CL_ORDER.iter().take(ncode) {
        *cl.get_mut(i)? = bits.get(3, src)? as u8;
    }
    let clcode = Code::new(&cl)?;
    let mut lengths = [0u8; 286 + 30];
    let mut i = 0usize;
    while i < nlen + ndist {
        let sym = clcode.decode(bits, src)?;
        let (value, repeat) = match sym {
            0..=15 => (sym as u8, 1usize),
            16 => {
                let prev = *lengths.get(i.checked_sub(1)?)?;
                (prev, 3 + bits.get(2, src)? as usize)
            }
            17 => (0, 3 + bits.get(3, src)? as usize),
            18 => (0, 11 + bits.get(7, src)? as usize),
            _ => return None,
        };
        if i + repeat > nlen + ndist {
            return None;
        }
        for l in lengths.get_mut(i..i + repeat)? {
            *l = value;
        }
        i += repeat;
    }
    // the end-of-block code must exist
    if lengths[256] == 0 {
        return None;
    }
    let lit = Code::new(lengths.get(..nlen)?)?;
    let dist = Code::new(lengths.get(nlen..nlen + ndist)?)?;
    Some((lit, dist))
}

/// Walks one compressed block to its end-of-block code.
fn block(bits: &mut Bits, src: &mut Src, lit: &Code, dist: &Code, out: &mut u64) -> Option<()> {
    loop {
        let sym = lit.decode(bits, src)?;
        match sym {
            0..=255 => *out += 1,
            256 => return Some(()),
            257..=285 => {
                let i = usize::from(sym - 257);
                let len = u64::from(LENGTH_BASE[i])
                    + u64::from(bits.get(u32::from(LENGTH_EXTRA[i]), src)?);
                let d = usize::from(dist.decode(bits, src)?);
                let distance = u64::from(*DIST_BASE.get(d)?)
                    + u64::from(bits.get(u32::from(*DIST_EXTRA.get(d)?), src)?);
                // nothing before the start of the stream (ZIP has no
                // preset dictionary), nothing beyond the window
                if distance > *out || distance > WINDOW {
                    return None;
                }
                *out += len;
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Limits;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The end of the stream at 3 in `data` (preceded by three bytes, and
    /// followed by whatever `data` holds behind it).
    fn end_of(stream: &[u8], behind: &[u8]) -> Option<u64> {
        let data = [&b"abc"[..], stream, behind].concat();
        let mut src = Src::new(&data, data.len() as u64, &Limits::DEFAULT);
        end(&mut src, 3)
    }

    #[test]
    fn streams_end_where_their_final_block_does() {
        // raw DEFLATE written by zlib: an empty input, back references,
        // a fixed-code block of text
        let fixed = hex(concat!(
            "734e2c2acbcc4b5748cbcc4b29069239a9c50a49950a2519a999450ac52545a5c9",
            "25a545a97a0acea3ea46d58d4075159555344200"
        ));
        for stream in [hex("0300"), hex("4b4c4a4e842100"), fixed] {
            for behind in [&b""[..], b"PK\x07\x08", &[0xFF; 40]] {
                assert_eq!(
                    end_of(&stream, behind),
                    Some(3 + stream.len() as u64),
                    "{stream:02x?}"
                );
            }
        }
        // stored blocks, not final then final, the bytes themselves skipped
        let mut stored = vec![0x00, 5, 0, !5, !0];
        stored.extend_from_slice(b"hello");
        stored.extend_from_slice(&[0x01, 0, 0, 0xFF, 0xFF]);
        assert_eq!(end_of(&stored, b"rest"), Some(3 + stored.len() as u64));
    }

    #[test]
    fn broken_streams_have_no_end() {
        // block type 3
        assert_eq!(end_of(&[0x07, 0, 0], &[0; 64]), None);
        // a stored block whose length is not confirmed by its complement
        assert_eq!(end_of(&[0x01, 5, 0, 5, 0, 1, 2, 3, 4, 5], &[]), None);
        // a stored block longer than the data
        assert_eq!(end_of(&[0x01, 0xFF, 0x7F, 0x00, 0x80, 1, 2], &[]), None);
        // a fixed block that starts with a back reference (nothing to
        // refer to): length code 257 (0000001), distance code 0 (00000)
        assert_eq!(end_of(&[0x03, 0x02, 0x00, 0x00], &[0; 16]), None);
        // a stream cut short
        let s = hex("4b4c4a4e842100");
        assert_eq!(end_of(&s[..s.len() - 2], &[]), None);
        // zeros: a non-final stored block of length 0 with a bad complement
        assert_eq!(end_of(&[0; 64], &[]), None);
    }
}
