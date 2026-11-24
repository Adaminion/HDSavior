//! `sniff`: does a file of a known kind start in these bytes?
//!
//! The scanner calls this for every 512-byte sector of the disk, so the
//! shape of the function matters more than anything in it: one length check,
//! one dispatch on the first byte, one more byte compared for the formats
//! whose signature is not at offset 0 (ISO-BMFF box types sit at offset 4).
//! All real checking happens behind those two tests, on the handful of
//! sectors that pass them. Nothing here allocates.
//!
//! Every signature accepted here has at least 32 bits that are fixed by the
//! format, and almost all have far more (see the table in the crate
//! documentation): beyond the magic number each format's first header is
//! checked for the values a real encoder can produce.

use crate::util::{be16, be32, is_printable, le16, le32};
use crate::{bmff, bmp, mkv, mp3, png, rar, sevenz, tiff, zip, FileKind};

/// Number of leading bytes [`sniff`] looks at. Passing more is harmless
/// (the rest is ignored); passing fewer makes it more conservative, never
/// less: a check that needs unseen bytes fails.
pub const SNIFF_LEN: usize = 64;

/// Tells whether a file of a supported kind starts with `head`.
///
/// `None` means "nothing recognised here", which is the answer for virtually
/// every sector of a disk and costs a few nanoseconds. `Some(kind)` is a
/// *candidate*: the signature and the first header look right, but only
/// [`measure`](crate::measure) establishes that a whole file follows.
///
/// Never panics, whatever the content and length of `head`.
#[inline]
pub fn sniff(head: &[u8]) -> Option<FileKind> {
    let head = head.get(..SNIFF_LEN).unwrap_or(head);
    if head.len() < 8 {
        return None;
    }
    let found = match head[0] {
        0xFF => sniff_ff(head),
        0x89 => sniff_png(head),
        b'G' => sniff_gif(head),
        b'B' => sniff_b(head),
        b'I' => sniff_i(head),
        b'M' => sniff_tiff(head),
        b'%' => sniff_pdf(head),
        b'P' => sniff_zip(head),
        b'R' => sniff_r(head),
        0x1A => sniff_ebml(head),
        b'7' => sniff_7z(head),
        _ => None,
    };
    if found.is_some() {
        return found;
    }
    // ISO-BMFF: a box size of any value, then one of ftyp, moov, mdat, wide
    match head[4] {
        b'f' | b'm' | b'w' => bmff::sniff(head),
        _ => None,
    }
}

// ----------------------------------------------------------- JPEG, MP3 --

/// `FF` starts a JPEG (`FF D8`) or an MPEG audio frame (eleven set bits,
/// then for layer III the bits `01` in the layer field). Sectors filled
/// with `FF` are common, so they are turned away here, before any call.
#[inline]
fn sniff_ff(h: &[u8]) -> Option<FileKind> {
    match h[1] {
        0xD8 => sniff_jpeg(h).then_some(FileKind::Jpeg),
        b if b & 0xE6 == 0xE2 => mp3::sniff_frame(h).then_some(FileKind::Mp3),
        _ => None,
    }
}

/// `FF D8 FF`, then a first segment as cameras, phones, scanners and
/// libraries write it. The marker code alone would leave only 24 fixed bits;
/// each accepted first segment adds its own identifier or structure.
#[cold]
fn sniff_jpeg(h: &[u8]) -> bool {
    if h[2] != 0xFF {
        return false;
    }
    let (Some(len), Some(body)) = (be16(h, 4), h.get(6..)) else {
        return false;
    };
    let len = len as usize;
    match h[3] {
        // APP0: JFIF (16 bytes at least) or its thumbnail extension
        0xE0 => len >= 16 && (body.starts_with(b"JFIF\0") || body.starts_with(b"JFXX\0")),
        // APP1: Exif (every camera) or XMP
        0xE1 => {
            len >= 8 && (body.starts_with(b"Exif\0\0") || body.starts_with(b"http://ns.adobe.com/"))
        }
        // APP2: ICC profile, Multi-Picture index, FlashPix
        0xE2 => {
            len >= 8
                && (body.starts_with(b"ICC_PROFILE\0")
                    || body.starts_with(b"MPF\0")
                    || body.starts_with(b"FPXR\0"))
        }
        // APP8 SPIFF, APP12 Ducky (Photoshop "save for web"), APP13 Photoshop
        0xE8 => body.starts_with(b"SPIFF\0"),
        0xEC => body.starts_with(b"Ducky"),
        0xED => body.starts_with(b"Photoshop 3.0\0"),
        // APP14 Adobe: CMYK and RGB files written without JFIF
        0xEE => len == 14 && body.starts_with(b"Adobe"),
        // COM: FFmpeg and other libraries put their name first
        0xFE => {
            let text = body
                .get(..(len.saturating_sub(2)).min(body.len()))
                .unwrap_or(&[]);
            let next_marker = h.get(4 + len).is_none_or(|&b| b == 0xFF);
            len >= 6
                && next_marker
                && text
                    .iter()
                    .all(|&c| is_printable(c) || matches!(c, 0 | b'\t' | b'\r' | b'\n'))
        }
        // DQT: one to four tables of 65 (8-bit) or 129 (16-bit) bytes
        0xDB => {
            let id_ok = body.first().is_some_and(|&b| b >> 4 <= 1 && b & 15 <= 3);
            let payload = len.saturating_sub(2);
            let sizes_ok = (0..=4usize).any(|wide| {
                payload >= 129 * wide
                    && (payload - 129 * wide).is_multiple_of(65)
                    && (1..=4).contains(&(wide + (payload - 129 * wide) / 65))
            });
            id_ok && sizes_ok
        }
        // DHT: class/id, 16 code-length counts, that many symbols
        0xC4 => {
            let id_ok = body.first().is_some_and(|&b| b >> 4 <= 1 && b & 15 <= 3);
            let symbols: usize = body
                .get(1..17)
                .map_or(usize::MAX, |c| c.iter().map(|&n| n as usize).sum());
            id_ok && symbols <= 256 && 19 + symbols <= len
        }
        // SOF0-2 first: precision, height, width, 1/3/4 components
        0xC0..=0xC2 => {
            let count = body.get(5).copied().unwrap_or(0) as usize;
            matches!(count, 1 | 3 | 4)
                && len == 8 + 3 * count
                && matches!(body.first(), Some(8 | 12))
                && be16(body, 3).is_some_and(|w| w != 0)
                && body.get(6..).is_some_and(|c| {
                    c.chunks_exact(3).take(count).all(|c| {
                        (1..=4).contains(&(c[1] >> 4))
                            && (1..=4).contains(&(c[1] & 15))
                            && c[2] <= 3
                    })
                })
        }
        // DRI first: fixed length, another marker right behind
        0xDD => len == 4 && h.get(8) == Some(&0xFF),
        _ => false,
    }
}

// ------------------------------------------------------------------ PNG --

#[cold]
fn sniff_png(h: &[u8]) -> Option<FileKind> {
    // signature, then IHDR with its fixed length of 13
    if h.get(..16)? != b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR" {
        return None;
    }
    // the header fields, when the caller supplied them
    match h.get(16..29) {
        Some(ihdr) if !png::ihdr_ok(ihdr) => None,
        _ => Some(FileKind::Png),
    }
}

// ------------------------------------------------------------------ GIF --

#[cold]
fn sniff_gif(h: &[u8]) -> Option<FileKind> {
    if !(h.starts_with(b"GIF87a") || h.starts_with(b"GIF89a")) {
        return None;
    }
    // logical screen: non-zero size; packed fields, background, aspect
    if le16(h, 6)? == 0 || le16(h, 8)? == 0 {
        return None;
    }
    let packed = *h.get(10)?;
    h.get(12)?;
    // without a global colour table the first block is right there
    if packed & 0x80 == 0 && !matches!(h.get(13), None | Some(0x2C | 0x21)) {
        return None;
    }
    Some(FileKind::Gif)
}

// ------------------------------------------------------------ BMP, BW64 --

#[cold]
fn sniff_b(h: &[u8]) -> Option<FileKind> {
    if h[1] == b'M' {
        let header = bmp::parse_header(h)?;
        return header.plausible_size().then_some(FileKind::Bmp);
    }
    sniff_rf64(h, b"BW64")
}

// ------------------------------------------------------- TIFF, RAW, ID3 --

#[cold]
fn sniff_i(h: &[u8]) -> Option<FileKind> {
    if h[1] == b'I' {
        return sniff_tiff(h);
    }
    sniff_id3(h)
}

#[cold]
fn sniff_tiff(h: &[u8]) -> Option<FileKind> {
    let header = tiff::parse_header(h)?;
    // When the first IFD is inside the head (it is for every camera RAW
    // format: offset 8, or 16 for CR2), its first entry must make sense.
    let at = header.first_ifd as usize;
    match at.checked_add(14).and_then(|end| h.get(at..end)) {
        Some(ifd) => {
            let rd16 = |o: usize| {
                if header.little_endian {
                    le16(ifd, o)
                } else {
                    be16(ifd, o)
                }
            };
            let rd32 = |o: usize| {
                if header.little_endian {
                    le32(ifd, o)
                } else {
                    be32(ifd, o)
                }
            };
            let entries = rd16(0)?;
            let field_type = rd16(4)?;
            let count = rd32(6)?;
            if !(1..=1024).contains(&entries) || tiff::type_size(field_type).is_none() || count == 0
            {
                return None;
            }
        }
        // Plain TIFF writers (libtiff and all that build on it) put the
        // directory behind the image data: then the byte order mark, the
        // 42 and an even offset - 33 fixed bits - are all there is to see.
        None if header.flavour == tiff::Flavour::Classic => {}
        // Cameras do not do that: Olympus and Panasonic files start with
        // their directory.
        None => return None,
    }
    Some(FileKind::Tiff)
}

/// An ID3v2 tag: `ID3`, version 2.2-2.4, defined flag bits only, a 28-bit
/// size stored in four 7-bit bytes. Almost always an MP3 follows;
/// `measure` checks that.
fn sniff_id3(h: &[u8]) -> Option<FileKind> {
    let len = mp3::id3v2_len(h)?;
    if len <= 10 {
        return None;
    }
    // The first frame ID: upper-case letters and digits (three of them in
    // v2.2), or zero padding. With an extended header it comes later.
    let extended = h.get(5)? & 0x40 != 0;
    let id_len = if h.get(3)? == &2 { 3 } else { 4 };
    if !extended {
        let id = h.get(10..10 + id_len)?;
        if !(id
            .iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            || id.iter().all(|&c| c == 0))
        {
            return None;
        }
    }
    // a tag that ends inside the head must be followed by an audio frame
    if let Some(frame) = usize::try_from(len)
        .ok()
        .and_then(|l| crate::util::rd::<4>(h, l))
    {
        mp3::parse_frame(frame)?;
    }
    Some(FileKind::Mp3)
}

// ------------------------------------------------------------------ PDF --

#[cold]
fn sniff_pdf(h: &[u8]) -> Option<FileKind> {
    // %PDF-1.0 .. %PDF-1.7, %PDF-2.0
    (h.starts_with(b"%PDF-")
        && matches!(h[5], b'1' | b'2')
        && h[6] == b'.'
        && h[7].is_ascii_digit())
    .then_some(FileKind::Pdf)
}

// ------------------------------------------------------------------ ZIP --

#[cold]
fn sniff_zip(h: &[u8]) -> Option<FileKind> {
    if le32(h, 0)? != zip::SIG_LOCAL {
        return None;
    }
    let version = le16(h, 4)?;
    let flags = le16(h, 6)?;
    let method = le16(h, 8)?;
    let name_len = le16(h, 26)? as usize;
    // version needed: 1.0 .. 6.3; flag bits 7-10, 12, 14, 15 are unused or
    // reserved; a method from the specification; a name
    if version & 0xFF > 63
        || flags & 0xD780 != 0
        || !zip::known_method(method)
        || name_len == 0
        || name_len > 4096
    {
        return None;
    }
    // the visible part of the name holds no control characters
    let name = h.get(30..).unwrap_or(&[]);
    let name = name.get(..name_len.min(name.len())).unwrap_or(name);
    name.iter()
        .all(|&c| c >= 0x20 && c != 0x7F)
        .then_some(FileKind::Zip)
}

// ------------------------------------------------------------ RIFF, RAR --

#[cold]
fn sniff_r(h: &[u8]) -> Option<FileKind> {
    match h.get(..4)? {
        b"RIFF" => sniff_riff(h),
        b"RF64" => sniff_rf64(h, b"RF64"),
        b"Rar!" => sniff_rar(h),
        _ => None,
    }
}

fn sniff_riff(h: &[u8]) -> Option<FileKind> {
    let size = le32(h, 4)?;
    let first = h.get(12..16)?;
    // the form type, then the first sub-chunk each form must start with
    match h.get(8..12)? {
        b"AVI " => (first == b"LIST" && h.get(20..24)? == b"hdrl").then_some(FileKind::Avi),
        b"WAVE" => (size >= 20 && first.iter().all(|&c| is_printable(c))).then_some(FileKind::Wav),
        b"WEBP" => matches!(first, b"VP8 " | b"VP8L" | b"VP8X").then_some(FileKind::Webp),
        _ => None,
    }
}

/// RF64 / BW64: saturated 32-bit size, `WAVE`, and the mandatory `ds64`.
fn sniff_rf64(h: &[u8], id: &[u8; 4]) -> Option<FileKind> {
    (h.starts_with(id) && le32(h, 4)? == u32::MAX && h.get(8..16)? == b"WAVEds64")
        .then_some(FileKind::Wav)
}

fn sniff_rar(h: &[u8]) -> Option<FileKind> {
    if h.starts_with(rar::SIGNATURE_V5) {
        // CRC, header size, then the type: archive header or encryption header
        let (size, n) = rar::vint(h.get(12..)?)?;
        let kind = *h.get(12 + n)?;
        return (size >= 3 && matches!(kind, 1 | 4)).then_some(FileKind::Rar);
    }
    if h.starts_with(rar::SIGNATURE_V4) {
        // the archive header block: type 73, at least 13 bytes
        return (*h.get(9)? == 0x73 && le16(h, 12)? >= 13).then_some(FileKind::Rar);
    }
    None
}

// ------------------------------------------------------- Matroska, WebM --

#[cold]
fn sniff_ebml(h: &[u8]) -> Option<FileKind> {
    mkv::header(h).map(|_| FileKind::Mkv)
}

// ------------------------------------------------------------------- 7z --

#[cold]
fn sniff_7z(h: &[u8]) -> Option<FileKind> {
    sevenz::parse_start(h).map(|_| FileKind::SevenZip)
}
