//! ISO base media file format (ISO/IEC 14496-12) and its relatives: MP4,
//! QuickTime MOV, M4V/M4A, 3GP, HEIF/HEIC/AVIF, Canon CR3.
//!
//! The file is a chain of boxes: `size(4, BE) type(4)`, then the payload.
//! `size == 1` means a 64-bit size follows the type; `size == 0` means "to
//! the end of the file". The walker reads box headers only and never touches
//! the media data: a multi-gigabyte `mdat` costs one seek.
//!
//! What a file must have to be measured:
//!
//! * a recognisable start: `ftyp`, or - QuickTime files written before `ftyp`
//!   existed - `moov`, `mdat`, or `wide` directly followed by `mdat`;
//! * a `moov` whose children form a consistent box chain including `mvhd`
//!   (or `cmov`), or a `meta` with a handler (HEIF), or movie fragments;
//!   a file without `ftyp` must have the `moov`;
//! * media: a `moov` with neither `mdat` nor fragments is rejected (an
//!   initialisation segment or a file cut right after its index).
//!
//! Where the file ends: at the first position that does not start a known
//! top-level box. A second `ftyp` starts the next file. After a `moov` that
//! came behind its `mdat` (the layout of every camera), another `mdat`,
//! `wide` or `moov` also starts the next file. An unknown box type is
//! accepted only if it looks like a four-character code *and* a known box
//! follows it, so that a vendor box in the middle of a file does not cut the
//! file short while text or noise behind the file is not swallowed.
//!
//! Not measurable: a box that extends "to the end of the file" (`size == 0`),
//! which is what an interrupted recording leaves behind.
//!
//! `Exact` means the box chain was followed from `ftyp` to its end.
//! `Probable` is used when the end is a judgement call (something box-like
//! but unknown follows, or a second `moov` was met) and for every file
//! without `ftyp`, because a sector-aligned `mdat` or `moov` in the middle of
//! a larger file looks exactly like the start of such a file. Data appended
//! outside the box structure
//! (Samsung `SEFT` and Insta360 trailers) is not part of the measured file.
//! The sample tables inside `moov` are not checked against the `mdat` extent.

use crate::reader::Src;
use crate::util::{be32, be64};
use crate::{Confidence, FileKind, Measured};

const MAX_BOXES: u32 = 1 << 20;
const MAX_CHILDREN: u32 = 4096;
/// `ftyp` payloads are a few brands; anything larger is not a file type box.
pub(crate) const MAX_FTYP: u64 = 1024;

/// Top-level boxes defined by ISO 14496-12, QuickTime, HEIF and DASH, plus
/// the ones cameras, phones and dashcams are known to add (see ExifTool's
/// QuickTime tables).
fn known_top_level(t: &[u8; 4]) -> bool {
    matches!(
        t,
        b"ftyp"
            | b"styp"
            | b"moov"
            | b"moof"
            | b"mfra"
            | b"mdat"
            | b"imda"
            | b"free"
            | b"skip"
            | b"wide"
            | b"pnot"
            | b"PICT"
            | b"pict"
            | b"junk"
            | b"uuid"
            | b"meta"
            | b"meco"
            | b"sidx"
            | b"ssix"
            | b"prft"
            | b"pdin"
            | b"emsg"
            | b"udta"
            | b"thum"
            | b"frea"
            | b"_htc"
            | b"ardt"
            | b"prrt"
            | b"udat"
            | b"IDIT"
            | b"gps0"
            | b"gsen"
            | b"sefd"
            | b"mpvd"
    )
}

/// Characters of four-character codes as vendors actually choose them.
fn fourcc_like(t: &[u8; 4]) -> bool {
    t.iter()
        .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b' ' | b'_' | b'-' | b'.' | 0xA9))
        && t[0] != b' '
}

/// The kind a major brand stands for.
pub(crate) fn kind_of_brand(major: &[u8; 4]) -> FileKind {
    match major {
        b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"hevm" | b"hevs" | b"mif1"
        | b"msf1" | b"avif" | b"avis" | b"miaf" => FileKind::Heic,
        b"crx " => FileKind::Cr3,
        _ => FileKind::Mp4,
    }
}

/// Bytes a brand may consist of: printable ASCII.
pub(crate) fn brand_like(b: &[u8]) -> bool {
    b.iter().all(|&c| (0x20..=0x7E).contains(&c))
}

struct BoxHead {
    typ: [u8; 4],
    /// Header length: 8, or 16 with a 64-bit size.
    head: u64,
    /// Total size including the header; `None` = extends to end of file.
    size: Option<u64>,
}

/// Reads a box header at `pos`. `None` when fewer than 8 (or 16) bytes
/// exist or the size is smaller than its own header.
fn box_head(src: &mut Src, pos: u64) -> Option<BoxHead> {
    let h = src.arr::<8>(pos)?;
    let typ = [h[4], h[5], h[6], h[7]];
    match be32(&h, 0)? {
        0 => Some(BoxHead {
            typ,
            head: 8,
            size: None,
        }),
        1 => {
            let size = src.be64(pos.checked_add(8)?)?;
            (size >= 16).then_some(BoxHead {
                typ,
                head: 16,
                size: Some(size),
            })
        }
        n => (n >= 8).then_some(BoxHead {
            typ,
            head: 8,
            size: Some(n as u64),
        }),
    }
}

/// Walks the child boxes in `[start, end)`. Returns `None` unless they form
/// a consistent chain; otherwise a bit mask telling which of the `wanted`
/// types are among them (bit `i` for `wanted[i]`).
fn children(src: &mut Src, start: u64, end: u64, wanted: &[&[u8; 4]]) -> Option<u32> {
    let mut pos = start;
    let mut found = 0u32;
    for _ in 0..MAX_CHILDREN {
        // QuickTime allows a 32-bit zero terminator after the last child
        if end.checked_sub(pos)? < 8 {
            return Some(found);
        }
        let b = box_head(src, pos)?;
        let size = b.size.unwrap_or(end - pos);
        let next = pos.checked_add(size)?;
        if next > end || !fourcc_like(&b.typ) {
            return None;
        }
        if let Some(i) = wanted.iter().position(|w| **w == b.typ) {
            found |= 1 << i;
        }
        pos = next;
    }
    None
}

/// Decides the extension from the brands in `ftyp`.
fn ext_for(kind: FileKind, major: &[u8; 4], compatible: &[u8]) -> &'static str {
    let has = |brand: &[u8; 4]| major == brand || compatible.chunks_exact(4).any(|c| c == brand);
    match kind {
        FileKind::Cr3 => "cr3",
        FileKind::Heic => {
            if has(b"avif") || has(b"avis") {
                "avif"
            } else {
                "heic"
            }
        }
        _ => match major {
            b"qt  " => "mov",
            b"M4A " => "m4a",
            b"M4B " => "m4b",
            b"M4P " => "m4p",
            b"M4V " | b"M4VH" | b"M4VP" => "m4v",
            b"F4V " | b"f4v " => "f4v",
            m if m.starts_with(b"3g2") => "3g2",
            m if m.starts_with(b"3g") => "3gp",
            _ => "mp4",
        },
    }
}

pub(crate) fn measure(src: &mut Src, kind: FileKind) -> Option<Measured> {
    let limit = src.limit();
    let mut pos = 0u64;
    let mut ext = "mov";
    let mut has_ftyp = false;
    let mut moov = false;
    let mut meta = false;
    let mut fragments = false;
    // an `mdat`, or item data stored inside `meta` itself
    let mut media = false;
    let mut media_boxes = 0u32;
    let mut last = *b"\0\0\0\0";
    // `moov` was written after media data: nothing but padding and metadata
    // can follow it
    let mut moov_last = false;
    let mut confidence = Confidence::Exact;

    let mut index = 0u32;
    loop {
        if index == MAX_BOXES {
            // more boxes than any real file has: not walked to an end
            return None;
        }
        // The chain ends where the data ends ...
        let Some(b) = box_head(src, pos) else { break };
        let known = known_top_level(&b.typ);
        if index == 0 {
            // ... but it has to start like sniff() says a file starts
            let start_ok = match &b.typ {
                b"ftyp" | b"moov" | b"mdat" => true,
                b"wide" => b.size == Some(8) && src.arr::<4>(12) == Some(*b"mdat"),
                _ => false,
            };
            if !start_ok {
                return None;
            }
        } else if &b.typ == b"ftyp" {
            // the next file
            break;
        }
        let Some(size) = b.size else {
            if known {
                // "to the end of the file": there is no telling where that is
                return None;
            }
            break;
        };
        let Some(next) = pos.checked_add(size).filter(|&n| n <= limit) else {
            if known {
                // a real box that does not fit: truncated or fragmented
                return None;
            }
            break;
        };
        if !known {
            // A vendor box is believed only when a known box follows it.
            let plausible = fourcc_like(&b.typ);
            let followed = plausible
                && box_head(src, next)
                    .is_some_and(|n| known_top_level(&n.typ) && &n.typ != b"ftyp");
            if !followed {
                if plausible && (moov || meta || fragments) {
                    // it may well belong to the file; say so
                    confidence = Confidence::Probable;
                }
                break;
            }
        }
        if moov_last && matches!(&b.typ, b"mdat" | b"wide" | b"moov") {
            break;
        }

        match &b.typ {
            b"ftyp" => {
                if !(16..=MAX_FTYP).contains(&size) {
                    return None;
                }
                let body = src.bytes(pos + 8, (size - 8) as usize)?;
                let major: [u8; 4] = body.get(..4)?.try_into().ok()?;
                if kind_of_brand(&major) != kind {
                    return None;
                }
                ext = ext_for(kind, &major, body.get(8..)?);
                has_ftyp = true;
            }
            b"moov" => {
                if moov {
                    // two movies in one file do not exist; where the first
                    // one's data ended is not known
                    confidence = Confidence::Probable;
                    break;
                }
                if !children(src, pos.checked_add(b.head)?, next, &[b"mvhd", b"cmov"])
                    .is_some_and(|m| m != 0)
                {
                    return None;
                }
                moov = true;
                moov_last = media_boxes > 0 && !fragments;
            }
            b"meta" => {
                // a full box (version and flags) in ISO files, a plain
                // container in QuickTime ones
                let body = pos.checked_add(b.head)?;
                let wanted: [&[u8; 4]; 2] = [b"hdlr", b"idat"];
                let found = match children(src, body.checked_add(4)?, next, &wanted) {
                    Some(m) if m & 1 != 0 => m,
                    _ => children(src, body, next, &wanted).unwrap_or(0),
                };
                meta |= found & 1 != 0;
                media |= found & 2 != 0;
            }
            b"moof" => {
                fragments = true;
                moov_last = false;
            }
            b"mdat" | b"imda" => {
                media_boxes += 1;
                media = true;
            }
            _ => {}
        }
        last = b.typ;
        pos = next;
        index += 1;
    }

    if !has_ftyp && (kind != FileKind::Mp4 || !moov) {
        return None;
    }
    let valid = match kind {
        FileKind::Heic => meta,
        FileKind::Cr3 => moov,
        // movie fragments never come without the `moov` that initialises them
        _ => moov || meta,
    };
    // An index without the media it describes is not a recoverable file:
    // an initialisation segment, or a file cut right behind its `moov`.
    if !valid || !media || (moov && media_boxes == 0) {
        return None;
    }
    if fragments && &last != b"mfra" {
        // A fragmented file is complete after every fragment. Only the
        // random access index that some writers put last says "this was
        // all"; without it more fragments may have followed.
        confidence = Confidence::Probable;
    }
    if !has_ftyp {
        // Nothing marks the first byte of such a file: `mdat` or `moov` at
        // this offset may just as well be the rest of a file that started
        // earlier (its chunk offsets would then be wrong).
        confidence = Confidence::Probable;
    }
    if kind == FileKind::Cr3 && is_cinema_raw(src) {
        ext = "crm";
    }
    Some(Measured {
        len: pos,
        confidence,
        ext,
    })
}

/// Canon writes its compressor version first thing inside `moov`; movies say
/// `CanonCRM`, stills `CanonCR3`.
fn is_cinema_raw(src: &mut Src) -> bool {
    let head = src.peek(0, 512);
    crate::util::find_sub(head, b"CNCV").and_then(|p| head.get(p + 4..p + 12)) == Some(b"CanonCRM")
}

/// The checks `sniff` applies to a head that has a box type at offset 4.
pub(crate) fn sniff(h: &[u8]) -> Option<FileKind> {
    let size = be32(h, 0)?;
    match h.get(4..8)? {
        b"ftyp" => {
            // 8 + major brand + minor version + compatible brands
            if size < 16 || size as u64 > MAX_FTYP || size % 4 != 0 {
                return None;
            }
            let major: [u8; 4] = h.get(8..12)?.try_into().ok()?;
            if !brand_like(&major) {
                return None;
            }
            let visible = h.get(16..h.len().min(size as usize)).unwrap_or(&[]);
            if !visible
                .chunks_exact(4)
                .all(|c| brand_like(c) || c == [0; 4])
            {
                return None;
            }
            Some(kind_of_brand(&major))
        }
        // the 8-byte placeholder QuickTime reserves in front of `mdat`
        b"wide" => (size == 8 && h.get(12..16)? == b"mdat").then_some(FileKind::Mp4),
        b"moov" => {
            // the movie header comes first: 108 bytes (version 0) or 120
            let (head, total) = match size {
                1 => (16, be64(h, 8)?),
                n => (8, n as u64),
            };
            let child = be32(h, head)? as u64;
            let ok = match h.get(head + 4..head + 8)? {
                b"mvhd" => child == 108 || child == 120,
                b"cmov" | b"udta" | b"trak" | b"iods" | b"meta" => child >= 8,
                _ => false,
            };
            (ok && total >= head as u64 + child).then_some(FileKind::Mp4)
        }
        b"mdat" => match size {
            // 64-bit size: the upper bytes of a plausible size are zero
            1 => be64(h, 8)
                .is_some_and(|n| (16..1 << 48).contains(&n))
                .then_some(FileKind::Mp4),
            // Media data first with a 32-bit size: nothing but the type is
            // fixed, and any text containing the word would match. Only
            // sizes below 16 MiB are accepted (a zero first byte, which
            // text does not have): the clips of the old digital cameras
            // that wrote this layout. measure() demands a `moov` behind it.
            n => (n >= 16 && n >> 24 == 0).then_some(FileKind::Mp4),
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brands() {
        assert_eq!(kind_of_brand(b"isom"), FileKind::Mp4);
        assert_eq!(kind_of_brand(b"qt  "), FileKind::Mp4);
        assert_eq!(kind_of_brand(b"heic"), FileKind::Heic);
        assert_eq!(kind_of_brand(b"mif1"), FileKind::Heic);
        assert_eq!(kind_of_brand(b"avif"), FileKind::Heic);
        assert_eq!(kind_of_brand(b"crx "), FileKind::Cr3);
        assert_eq!(ext_for(FileKind::Mp4, b"qt  ", b""), "mov");
        assert_eq!(ext_for(FileKind::Mp4, b"M4A ", b"M4A isomiso2"), "m4a");
        assert_eq!(ext_for(FileKind::Mp4, b"3gp4", b"3gp4isom"), "3gp");
        assert_eq!(ext_for(FileKind::Mp4, b"3g2a", b""), "3g2");
        assert_eq!(ext_for(FileKind::Mp4, b"avc1", b"avc1isom"), "mp4");
        assert_eq!(ext_for(FileKind::Mp4, b"XAVC", b"XAVCmp42iso2"), "mp4");
        assert_eq!(ext_for(FileKind::Heic, b"mif1", b"mif1avifmiaf"), "avif");
        assert_eq!(ext_for(FileKind::Heic, b"mif1", b"mif1heicmiaf"), "heic");
        assert_eq!(ext_for(FileKind::Heic, b"heic", b"mif1heic"), "heic");
        assert_eq!(ext_for(FileKind::Cr3, b"crx ", b"crx isom"), "cr3");
    }

    #[test]
    fn four_character_codes() {
        assert!(fourcc_like(b"mvhd"));
        assert!(fourcc_like(b"\xA9nam"));
        assert!(fourcc_like(b"CNCV"));
        assert!(fourcc_like(b"thm "));
        assert!(!fourcc_like(b"    "));
        assert!(!fourcc_like(b"\0\0\0\0"));
        assert!(!fourcc_like(b"ab\ncd"[..4].try_into().unwrap()));
    }
}
