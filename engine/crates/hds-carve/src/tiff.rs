//! TIFF 6.0 and the camera RAW formats built on it: IFD walk, furthest
//! referenced byte.
//!
//! Layout: an 8-byte header (`II` or `MM` byte order, the number 42, offset of
//! the first image file directory). An IFD is a 16-bit entry count, 12-byte
//! entries (`tag, type, count, value-or-offset`) and the 32-bit offset of the
//! next IFD. Everything else - values that do not fit in four bytes, image
//! strips and tiles, embedded JPEG previews, child IFDs - is found only
//! through offsets stored in entries, anywhere in the file, in any order.
//!
//! So a TIFF has no end marker and no length field: the best a reader can do
//! is take the furthest byte that anything it understands points to. This
//! walker follows
//!
//! * the IFD chain and `SubIFDs` (330), strictly: an offset that does not
//!   lead to a valid IFD, or image data that does not fit in `max_len`,
//!   rejects the file (a silently shortened RAW file is worse than none);
//! * the Exif, GPS and Interoperability IFDs (34665, 34853, 40965), leniently;
//! * every out-of-line value, including opaque blobs such as `MakerNote`;
//! * strips, tiles and `JPEGInterchangeFormat` previews.
//!
//! It does not follow vendor-private pointers *inside* opaque blobs (maker
//! notes, Sony `SR2Private`). The result is therefore always `Probable`: for
//! the layouts cameras actually write the image data comes last and the
//! result is the true length, but the format cannot prove it. See the table
//! in the crate documentation for what is known per vendor.
//!
//! Variants with their own magic: Canon CR2 (`CR\x02\0` at offset 8),
//! Olympus ORF (`RO`/`RS` instead of 42), Panasonic RW2 (85 instead of 42;
//! its raw data is located by `RawDataOffset` without a length, so it is
//! recognised but not measurable). BigTIFF (43) is not supported.

use std::collections::{HashSet, VecDeque};

use crate::reader::Src;
use crate::{Confidence, Measured};

const MAX_IFDS: usize = 65_536;
const MAX_ENTRIES: usize = 4096;
/// Strips or tiles per image.
const MAX_STRIPS: u64 = 1 << 20;

/// Signature at offset 8 of Panasonic RW2 / Leica RWL files.
pub(crate) const RW2_SIGNATURE: [u8; 16] =
    [0x88, 0xE7, 0x74, 0xD8, 0xF8, 0x25, 0x1D, 0x4D, 0x94, 0x7A, 0x6E, 0x77, 0x82, 0x2B, 0x5D, 0x6A];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavour {
    Classic,
    Cr2,
    Orf,
    Rw2,
}

/// The fixed part of a TIFF header.
pub(crate) struct Header {
    pub(crate) little_endian: bool,
    pub(crate) flavour: Flavour,
    pub(crate) first_ifd: u32,
}

/// Recognises the header variants this module understands. `h` needs 8
/// bytes; CR2 and RW2 are only told apart from plain TIFF with 24.
pub(crate) fn parse_header(h: &[u8]) -> Option<Header> {
    let little_endian = match h.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let pair = <[u8; 2]>::try_from(h.get(2..4)?).ok()?;
    let quad = <[u8; 4]>::try_from(h.get(4..8)?).ok()?;
    let (magic, first_ifd) = if little_endian {
        (u16::from_le_bytes(pair), u32::from_le_bytes(quad))
    } else {
        (u16::from_be_bytes(pair), u32::from_be_bytes(quad))
    };
    let flavour = match magic {
        42 if little_endian && first_ifd >= 16 && h.get(8..12) == Some(b"CR\x02\0") => Flavour::Cr2,
        42 => Flavour::Classic,
        0x4F52 | 0x5352 => Flavour::Orf,
        0x55 if little_endian && first_ifd >= 24 && h.get(8..24) == Some(&RW2_SIGNATURE[..]) => Flavour::Rw2,
        _ => return None,
    };
    // IFDs start on a word boundary, after the header
    if first_ifd < 8 || first_ifd & 1 != 0 {
        return None;
    }
    Some(Header { little_endian, flavour, first_ifd })
}

/// Size in bytes of one value of a TIFF field type.
pub(crate) fn type_size(t: u16) -> Option<u64> {
    match t {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 | 13 => Some(4),
        5 | 10 | 12 | 16 | 17 | 18 => Some(8),
        _ => None,
    }
}

/// An array-valued entry: where its values are, and how to read them.
#[derive(Clone, Copy)]
struct Field {
    off: u64,
    typ: u16,
    count: u64,
}

#[derive(Default)]
struct Ifd {
    /// Furthest byte referenced by this IFD (itself included).
    end: u64,
    /// Bytes of image data it references.
    image_bytes: u64,
    children: Vec<(u64, bool)>,
    next: u64,
    raw_data: bool,
    dng: bool,
    make: [u8; 16],
}

struct Walker<'s, 'a> {
    src: &'s mut Src<'a>,
    le: bool,
    rw2: bool,
}

impl Walker<'_, '_> {
    fn u16(&self, b: &[u8], off: usize) -> Option<u16> {
        let a = <[u8; 2]>::try_from(b.get(off..off.checked_add(2)?)?).ok()?;
        Some(if self.le { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) })
    }

    fn u32(&self, b: &[u8], off: usize) -> Option<u32> {
        let a = <[u8; 4]>::try_from(b.get(off..off.checked_add(4)?)?).ok()?;
        Some(if self.le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) })
    }

    fn u64(&self, b: &[u8], off: usize) -> Option<u64> {
        let a = <[u8; 8]>::try_from(b.get(off..off.checked_add(8)?)?).ok()?;
        Some(if self.le { u64::from_le_bytes(a) } else { u64::from_be_bytes(a) })
    }

    /// One unsigned integer of a SHORT / LONG / IFD / LONG8 field.
    fn int(&self, b: &[u8], off: usize, typ: u16) -> Option<u64> {
        match typ {
            3 => self.u16(b, off).map(u64::from),
            4 | 13 => self.u32(b, off).map(u64::from),
            16 | 18 => self.u64(b, off),
            _ => None,
        }
    }

    /// All values of an integer array field (at most `MAX_STRIPS`).
    fn ints(&mut self, f: Field) -> Option<Vec<u64>> {
        let size = type_size(f.typ)?;
        if f.count > MAX_STRIPS {
            return None;
        }
        let mut out = Vec::with_capacity(f.count as usize);
        let mut done = 0u64;
        while done < f.count {
            let n = (f.count - done).min(4096);
            let off = f.off.checked_add(done.checked_mul(size)?)?;
            let chunk = self.src.bytes(off, (n * size) as usize)?.to_vec();
            for i in 0..n as usize {
                out.push(self.int(&chunk, i * size as usize, f.typ)?);
            }
            done += n;
        }
        Some(out)
    }

    /// Parses the IFD at `off`. `None` means it is not a usable IFD or it
    /// references data outside `max_len`.
    fn ifd(&mut self, off: u64, is_first: bool) -> Option<Ifd> {
        let limit = self.src.limit();
        let n = self.src.arr::<2>(off).map(|a| if self.le { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) })?
            as usize;
        if n == 0 || n > MAX_ENTRIES {
            return None;
        }
        let len = 2 + 12 * n + 4;
        let ifd_end = off.checked_add(len as u64)?;
        if ifd_end > limit {
            return None;
        }
        let raw = self.src.bytes(off, len)?.to_vec();
        let mut ifd = Ifd { end: ifd_end, next: self.u32(&raw, 2 + 12 * n)? as u64, ..Ifd::default() };

        let mut strip_offsets = None;
        let mut strip_counts = None;
        let mut tile_offsets = None;
        let mut tile_counts = None;
        let mut jpeg_offset = None;
        let mut jpeg_len = None;
        let mut unknown_types = 0usize;
        let mut raw_data_offset = false;

        for i in 0..n {
            let e = 2 + 12 * i;
            let tag = self.u16(&raw, e)?;
            let typ = self.u16(&raw, e + 2)?;
            let count = self.u32(&raw, e + 4)? as u64;
            let Some(size) = type_size(typ) else {
                unknown_types += 1;
                continue;
            };
            // count < 2^32 and size <= 8
            let total = count * size;
            let field = if total <= 4 {
                Field { off: off + e as u64 + 8, typ, count }
            } else {
                let value = self.u32(&raw, e + 8)? as u64;
                let value_end = value.checked_add(total)?;
                if value < 8 || value_end > limit {
                    return None;
                }
                ifd.end = ifd.end.max(value_end);
                Field { off: value, typ, count }
            };
            // the first value, when the field is a single inline integer
            let scalar = if count == 1 { self.int(&raw, e + 8, typ) } else { None };
            match tag {
                0x0111 => strip_offsets = Some(field),
                0x0117 => strip_counts = Some(field),
                0x0144 => tile_offsets = Some(field),
                0x0145 => tile_counts = Some(field),
                0x0201 => jpeg_offset = scalar,
                0x0202 => jpeg_len = scalar,
                // Compression: Nikon NEF, Sony ARW, Pentax PEF, Samsung SRW
                0x0103 => ifd.raw_data |= matches!(scalar, Some(34713 | 32767 | 65535 | 32770 | 32772)),
                // PhotometricInterpretation: colour filter array, linear raw
                0x0106 => ifd.raw_data |= matches!(scalar, Some(32803 | 34892)),
                0x014A => {
                    if count > 64 {
                        return None;
                    }
                    for v in self.ints(field)? {
                        ifd.children.push((v, true));
                    }
                }
                0x8769 | 0x8825 | 0xA005 => {
                    if let Some(v) = scalar {
                        ifd.children.push((v, false));
                    }
                }
                0xC612 => ifd.dng = true,
                0x010F if is_first && typ == 2 => {
                    let n = (count as usize).min(ifd.make.len());
                    let (bytes, got) = self.src.side_peek::<16>(field.off);
                    if let (Some(dst), Some(s)) = (ifd.make.get_mut(..n.min(got)), bytes.get(..n.min(got))) {
                        dst.copy_from_slice(s);
                    }
                }
                // Panasonic RW2 reuses tag 280 as RawDataOffset: the start of
                // the raw data, with no length anywhere
                0x0118 if self.rw2 => raw_data_offset = true,
                _ => {}
            }
        }
        if unknown_types * 2 > n {
            return None;
        }

        for (offsets, counts) in [(strip_offsets, strip_counts), (tile_offsets, tile_counts)] {
            match (offsets, counts) {
                (None, None) => {}
                (Some(o), Some(c)) if o.count == c.count => {
                    let offs = self.ints(o)?;
                    let lens = self.ints(c)?;
                    for (&at, &len) in offs.iter().zip(&lens) {
                        if len == 0 {
                            continue;
                        }
                        let end = at.checked_add(len)?;
                        if at < 8 || end > limit {
                            return None;
                        }
                        ifd.end = ifd.end.max(end);
                        ifd.image_bytes = ifd.image_bytes.saturating_add(len);
                    }
                }
                // offsets without lengths (or the reverse): cannot be sized
                _ => return None,
            }
        }
        if let (Some(at), Some(len)) = (jpeg_offset, jpeg_len) {
            if len != 0 {
                let end = at.checked_add(len)?;
                if at < 8 || end > limit {
                    return None;
                }
                ifd.end = ifd.end.max(end);
                ifd.image_bytes = ifd.image_bytes.saturating_add(len);
            }
        }
        if raw_data_offset && is_first {
            return None;
        }
        Some(ifd)
    }
}

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    let (head, got) = src.side_peek::<24>(0);
    let header = parse_header(head.get(..got)?)?;
    let mut w = Walker { src, le: header.little_endian, rw2: header.flavour == Flavour::Rw2 };

    let mut queue: VecDeque<(u64, bool, bool)> = VecDeque::new();
    let mut seen: HashSet<u64> = HashSet::new();
    queue.push_back((header.first_ifd as u64, true, true));
    let mut end = 8u64;
    let mut image_bytes = 0u64;
    let mut raw_data = false;
    let mut dng = false;
    let mut make = [0u8; 16];

    while let Some((off, strict, is_first)) = queue.pop_front() {
        if !seen.insert(off) {
            // a loop in the chain: a damaged or hostile file
            if strict {
                return None;
            }
            continue;
        }
        if seen.len() > MAX_IFDS {
            return None;
        }
        let Some(ifd) = w.ifd(off, is_first) else {
            if strict {
                return None;
            }
            continue;
        };
        end = end.max(ifd.end);
        image_bytes = image_bytes.saturating_add(ifd.image_bytes);
        raw_data |= ifd.raw_data;
        if is_first {
            dng = ifd.dng;
            make = ifd.make;
        }
        if ifd.next != 0 {
            queue.push_back((ifd.next, strict, false));
        }
        for (child, child_strict) in ifd.children {
            queue.push_back((child, strict && child_strict, false));
        }
    }
    if image_bytes == 0 {
        return None;
    }

    let ext = match header.flavour {
        Flavour::Cr2 => "cr2",
        Flavour::Orf => "orf",
        Flavour::Rw2 => "rw2",
        Flavour::Classic if dng => "dng",
        Flavour::Classic if raw_data => raw_ext(&make),
        Flavour::Classic => "tif",
    };
    Some(Measured { len: end, confidence: Confidence::Probable, ext })
}

/// Extension of a TIFF that carries sensor data, from the camera maker.
fn raw_ext(make: &[u8; 16]) -> &'static str {
    let mut upper = *make;
    upper.make_ascii_uppercase();
    for (prefix, ext) in
        [(&b"NIKON"[..], "nef"), (b"SONY", "arw"), (b"PENTAX", "pef"), (b"RICOH", "pef"), (b"SAMSUNG", "srw")]
    {
        if upper.starts_with(prefix) {
            return ext;
        }
    }
    "tif"
}
