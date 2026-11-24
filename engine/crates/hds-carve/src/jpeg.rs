//! JPEG (ITU-T T.81 / JFIF / Exif): marker-segment walk, then a scan of the
//! entropy-coded data up to the end-of-image marker.
//!
//! Layout: `SOI`, then marker segments (`FF xx`, two-byte big-endian length
//! that counts itself), among them one frame header (`SOFn`) and tables; each
//! scan starts with `SOS` and is followed by entropy-coded bytes in which a
//! literal `FF` is always followed by a stuffed `00`, so that the only `FF xx`
//! pairs are restart markers (`RST0..7`, in cyclic order, only if `DRI`
//! declared an interval) and the marker that ends the scan. Progressive and
//! non-interleaved files have several scans, with tables in between. The file
//! ends with `EOI`.
//!
//! What ends the measured file: the first `EOI` at marker level. An Exif
//! thumbnail is a complete JPEG, but it lives inside the `APP1` segment and is
//! skipped with it. Anything after `EOI` (the further images of a
//! Multi-Picture file, Samsung/Google motion-photo trailers) is deliberately
//! left out.
//!
//! No checksums exist in JPEG and the entropy data is not decoded, so a file
//! whose tail was replaced by the tail of another JPEG still measures as
//! `Exact`: the length is right for a contiguous file, the content is not
//! proven.

use crate::reader::Src;
use crate::util::{be16, find_byte};
use crate::{Confidence, Measured};

/// Upper bound on marker segments before the image data ends (a file with
/// more is not a photograph; every segment consumes at least four bytes).
const MAX_SEGMENTS: u32 = 1 << 20;
/// Fill bytes (`FF`) tolerated in front of one marker.
const MAX_FILL: u32 = 64 * 1024;

struct Frame {
    ids: [u8; 4],
    count: usize,
}

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    src.scan_mode();
    if src.arr::<2>(0)? != [0xFF, 0xD8] {
        return None;
    }
    let mut pos: u64 = 2;
    let mut frame: Option<Frame> = None;
    let mut scans = 0u32;
    let mut restart_interval = 0u16;
    let mut segments = 0u32;
    loop {
        segments += 1;
        if segments > MAX_SEGMENTS {
            return None;
        }
        let mut m = src.arr::<2>(pos)?;
        if m[0] != 0xFF {
            return None;
        }
        let mut fill = 0u32;
        while m[1] == 0xFF {
            fill += 1;
            if fill > MAX_FILL {
                return None;
            }
            pos += 1;
            m = src.arr::<2>(pos)?;
        }
        let code = m[1];
        match code {
            // EOI: the end, provided there was image data
            0xD9 => {
                if scans == 0 {
                    return None;
                }
                return Some(Measured {
                    len: pos + 2,
                    confidence: Confidence::Exact,
                    ext: "jpg",
                });
            }
            // stuffed zero, TEM, a restart marker outside a scan, a second SOI
            0x00 | 0x01 | 0xD0..=0xD8 => return None,
            _ => {}
        }
        let len = src.be16(pos + 2)? as u64;
        if len < 2 {
            return None;
        }
        let body_off = pos + 4;
        let body_len = (len - 2) as usize;
        let next = pos + 2 + len;
        match code {
            // SOS: scan header, then entropy-coded data up to the next marker
            0xDA => {
                let f = frame.as_ref()?;
                check_sos(src.bytes(body_off, body_len)?, f)?;
                pos = scan_entropy(src, next, restart_interval)?;
                scans += 1;
                continue;
            }
            // SOF0-3, 5-7, 9-11, 13-15: exactly one frame header per image
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                if frame.is_some() {
                    return None;
                }
                frame = Some(parse_sof(src.bytes(body_off, body_len)?, code)?);
            }
            0xC4 => check_dht(src.bytes(body_off, body_len)?)?,
            0xDB => check_dqt(src.bytes(body_off, body_len)?)?,
            0xDD => {
                if len != 4 {
                    return None;
                }
                restart_interval = src.be16(body_off)?;
            }
            // DNL: only meaningful right after the first scan
            0xDC => {
                if len != 4 || scans == 0 {
                    return None;
                }
            }
            // DAC (arithmetic conditioning), APPn, COM: opaque, skipped
            0xCC | 0xE0..=0xEF | 0xFE => {}
            // JPG extensions, hierarchical mode (DHP/EXP), reserved codes
            _ => return None,
        }
        pos = next;
    }
}

fn parse_sof(b: &[u8], code: u8) -> Option<Frame> {
    let precision = *b.first()?;
    let width = be16(b, 3)?;
    let count = *b.get(5)? as usize;
    if !(1..=4).contains(&count) || b.len() != 6 + 3 * count || width == 0 {
        return None;
    }
    let lossless = matches!(code, 0xC3 | 0xC7 | 0xCB | 0xCF);
    let precision_ok = if lossless {
        (2..=16).contains(&precision)
    } else {
        precision == 8 || precision == 12
    };
    if !precision_ok {
        return None;
    }
    let mut ids = [0u8; 4];
    for (i, c) in b.get(6..)?.chunks_exact(3).enumerate() {
        let (h, v) = (c[1] >> 4, c[1] & 15);
        if !(1..=4).contains(&h) || !(1..=4).contains(&v) || c[2] > 3 {
            return None;
        }
        *ids.get_mut(i)? = c[0];
    }
    Some(Frame { ids, count })
}

fn check_sos(b: &[u8], frame: &Frame) -> Option<()> {
    let ns = *b.first()? as usize;
    if !(1..=4).contains(&ns) || ns > frame.count || b.len() != 4 + 2 * ns {
        return None;
    }
    let known = frame.ids.get(..frame.count)?;
    for c in b.get(1..1 + 2 * ns)?.chunks_exact(2) {
        if !known.contains(&c[0]) || (c[1] >> 4) > 3 || (c[1] & 15) > 3 {
            return None;
        }
    }
    // spectral selection (or predictor) and the end of band
    let ss = *b.get(1 + 2 * ns)?;
    let se = *b.get(2 + 2 * ns)?;
    (ss <= 63 && se <= 63).then_some(())
}

fn check_dht(mut b: &[u8]) -> Option<()> {
    if b.is_empty() {
        return None;
    }
    while !b.is_empty() {
        let class_id = *b.first()?;
        if (class_id >> 4) > 1 || (class_id & 15) > 3 {
            return None;
        }
        let symbols: usize = b.get(1..17)?.iter().map(|&n| n as usize).sum();
        if symbols > 256 {
            return None;
        }
        b = b.get(17 + symbols..)?;
    }
    Some(())
}

fn check_dqt(mut b: &[u8]) -> Option<()> {
    if b.is_empty() {
        return None;
    }
    while !b.is_empty() {
        let precision_id = *b.first()?;
        if (precision_id >> 4) > 1 || (precision_id & 15) > 3 {
            return None;
        }
        let size = if precision_id >> 4 == 0 { 64 } else { 128 };
        b = b.get(1 + size..)?;
    }
    Some(())
}

/// Scans entropy-coded data from `pos`; returns the offset of the `FF` that
/// starts the marker ending the scan. `None` when the data ends first or the
/// restart markers are out of order.
fn scan_entropy(src: &mut Src, mut pos: u64, restart_interval: u16) -> Option<u64> {
    let mut next_rst = 0u8;
    // The previous window ended with an `FF` whose partner is not seen yet.
    let mut pending_ff = false;
    loop {
        let w = src.window(pos);
        if w.is_empty() {
            return None;
        }
        let mut i = 0usize;
        if pending_ff {
            pending_ff = false;
            match *w.first()? {
                0x00 => i = 1,
                // a fill byte: it may itself be the start of the marker
                0xFF => {}
                c @ 0xD0..=0xD7 => {
                    restart_ok(c, &mut next_rst, restart_interval)?;
                    i = 1;
                }
                _ => return Some(pos - 1),
            }
        }
        while let Some(j) = find_byte(w.get(i..)?, 0xFF) {
            let k = i + j;
            let Some(&c) = w.get(k + 1) else {
                pending_ff = true;
                break;
            };
            match c {
                0x00 => i = k + 2,
                0xFF => i = k + 1,
                0xD0..=0xD7 => {
                    restart_ok(c, &mut next_rst, restart_interval)?;
                    i = k + 2;
                }
                _ => return Some(pos + k as u64),
            }
        }
        pos += w.len() as u64;
    }
}

/// `RSTn` must count 0, 1, .. 7, 0, .. and needs a restart interval.
fn restart_ok(code: u8, next: &mut u8, restart_interval: u16) -> Option<()> {
    if restart_interval == 0 || code & 7 != *next {
        return None;
    }
    *next = (*next + 1) & 7;
    Some(())
}
