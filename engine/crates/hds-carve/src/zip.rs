//! ZIP (PKWARE APPNOTE) and the document formats packaged in it: local
//! headers, central directory, end-of-central-directory record.
//!
//! Layout of an archive written in one piece:
//!
//! ```text
//! [local file header + name + extra] [data] [data descriptor]?   (per entry)
//! [central directory header + name + extra + comment]            (per entry)
//! [zip64 end of central directory record] [zip64 locator]        (optional)
//! [end of central directory record] [archive comment]
//! ```
//!
//! The walker goes forward from the local header at offset 0. An entry's
//! data is skipped without being read when the local header says how long
//! it is (32-bit field, or the ZIP64 extra field when that is `FFFFFFFF`).
//! Entries written by a streaming compressor have general-purpose bit 3 set
//! and no size up front; the size follows the data in a *data descriptor*.
//! When such an entry is DEFLATE-compressed (and not encrypted), the
//! compressed stream is walked to its final block ([`deflate`]) and the
//! descriptor must be right behind it; a stream that breaks off ends the
//! candidate. Entries of other methods have to be scanned for the
//! descriptor, which is accepted only if the compressed size it states
//! equals the distance scanned and a header signature follows it - a ZIP
//! stored inside the entry brings its own descriptors, and they fail that
//! test.
//!
//! Then the central directory is walked, and the end record must follow it
//! directly. The record states where the directory starts, counted from the
//! start of the archive; that must be where the walk found it, otherwise the
//! candidate is not the start of an archive but a local header in the middle
//! of one (every entry of every archive has one, and one in 512 of them
//! sits on a sector boundary). `Exact` is reported when the record also
//! agrees with the walk on entry count and directory size (taken from the
//! ZIP64 record when the 32-bit fields are saturated), `Probable` when
//! those two disagree.
//!
//! Compressed data is never inflated and CRCs are not checked: that would
//! mean reading every byte of archives that can be skipped in a handful of
//! reads.
//!
//! The extension is refined from what is inside: a first entry `mimetype`
//! (EPUB, OpenDocument), `[Content_Types].xml` with `word/`, `xl/` or
//! `ppt/` (Office Open XML), `AndroidManifest.xml` (APK),
//! `META-INF/MANIFEST.MF` (JAR).

use crate::deflate;
use crate::reader::Src;
use crate::util::{le16, le32, le64, Crc32};
use crate::{Confidence, Measured};

pub(crate) const SIG_LOCAL: u32 = 0x0403_4B50;
const SIG_CENTRAL: u32 = 0x0201_4B50;
const SIG_END: u32 = 0x0605_4B50;
const SIG_DESCRIPTOR: u32 = 0x0807_4B50;
const SIG_END64: u32 = 0x0606_4B50;
const SIG_LOCATOR64: u32 = 0x0706_4B50;
const SIG_EXTRA_DATA: u32 = 0x0806_4B50;
const SIG_DIGITAL_SIGNATURE: u32 = 0x0505_4B50;

const MAX_ENTRIES: u64 = 1 << 24;

/// Compression methods assigned in APPNOTE section 4.4.5.
pub(crate) fn known_method(m: u16) -> bool {
    matches!(m, 0..=10 | 12 | 14 | 16 | 18..=20 | 93..=99)
}

/// A local file header at the start of `h` as writers produce it:
/// version needed 1.0 to 6.3, reserved flag bits clear, a method from the
/// specification, a name of 1 to 4096 bytes whose visible part holds no
/// control characters.
pub(crate) fn local_header_plausible(h: &[u8]) -> bool {
    let (Some(SIG_LOCAL), Some(version), Some(flags), Some(method), Some(name_len)) = (
        le32(h, 0),
        le16(h, 4),
        le16(h, 6),
        le16(h, 8),
        le16(h, 26),
    ) else {
        return false;
    };
    let name_len = name_len as usize;
    // flag bits 7-10, 12, 14 and 15 are unused or reserved
    if version & 0xFF > 63
        || flags & 0xD780 != 0
        || !known_method(method)
        || name_len == 0
        || name_len > 4096
    {
        return false;
    }
    let name = h.get(30..).unwrap_or(&[]);
    let name = name.get(..name_len.min(name.len())).unwrap_or(name);
    name.iter().all(|&c| c >= 0x20 && c != 0x7F)
}

#[derive(Default)]
struct Contents {
    mimetype: Option<&'static str>,
    content_types: bool,
    word: bool,
    excel: bool,
    powerpoint: bool,
    visio: bool,
    manifest: bool,
    android: bool,
}

impl Contents {
    fn name(&mut self, name: &[u8]) {
        match name {
            b"[Content_Types].xml" => self.content_types = true,
            b"META-INF/MANIFEST.MF" => self.manifest = true,
            b"AndroidManifest.xml" => self.android = true,
            n if n.starts_with(b"word/") => self.word = true,
            n if n.starts_with(b"xl/") => self.excel = true,
            n if n.starts_with(b"ppt/") => self.powerpoint = true,
            n if n.starts_with(b"visio/") => self.visio = true,
            _ => {}
        }
    }

    fn mimetype(&mut self, value: &[u8]) {
        let value = value.strip_suffix(b"\n").unwrap_or(value);
        let value = value.strip_suffix(b"\r").unwrap_or(value);
        self.mimetype = match value {
            b"application/epub+zip" => Some("epub"),
            b"application/vnd.oasis.opendocument.text" => Some("odt"),
            b"application/vnd.oasis.opendocument.spreadsheet" => Some("ods"),
            b"application/vnd.oasis.opendocument.presentation" => Some("odp"),
            b"application/vnd.oasis.opendocument.graphics" => Some("odg"),
            _ => None,
        };
    }

    fn ext(&self) -> &'static str {
        if let Some(e) = self.mimetype {
            return e;
        }
        if self.content_types {
            if self.word {
                return "docx";
            }
            if self.excel {
                return "xlsx";
            }
            if self.powerpoint {
                return "pptx";
            }
            if self.visio {
                return "vsdx";
            }
        }
        if self.android {
            return "apk";
        }
        if self.manifest {
            return "jar";
        }
        "zip"
    }
}

fn starts_entry(sig: u32) -> bool {
    matches!(sig, SIG_LOCAL | SIG_CENTRAL | SIG_EXTRA_DATA)
}

/// What a data descriptor must say beyond the compressed size.
#[derive(Clone, Copy)]
enum Expect {
    /// Nothing more: the data was walked (DEFLATE) or its size was in the
    /// header.
    Size,
    /// A stored entry: the CRC-32 of the bytes in front of the descriptor,
    /// and an uncompressed size equal to the compressed one.
    Stored(u32),
}

/// After the data of an entry whose compressed size is known but which also
/// has bit 3 set: the offset after its data descriptor. The descriptor has
/// four possible shapes (with or without signature, 32- or 64-bit sizes).
fn skip_descriptor(src: &mut Src, at: u64, size: u64, zip64: bool, expect: Expect) -> Option<u64> {
    let (buf, got) = src.side_peek::<28>(at);
    let b = buf.get(..got)?;
    let shapes: [(bool, bool); 4] = if zip64 {
        [(true, true), (true, false), (false, true), (false, false)]
    } else {
        [(true, false), (true, true), (false, false), (false, true)]
    };
    for (signed, wide) in shapes {
        let mut p = 0usize;
        if signed {
            if le32(b, 0) != Some(SIG_DESCRIPTOR) {
                continue;
            }
            p = 4;
        }
        // crc, compressed size, uncompressed size
        let field = |at: usize| {
            if wide {
                le64(b, at)
            } else {
                le32(b, at).map(u64::from)
            }
        };
        let w = if wide { 8 } else { 4 };
        let stated = field(p + 4);
        let len = p + 4 + 2 * w;
        let content_ok = match expect {
            Expect::Size => true,
            Expect::Stored(crc) => le32(b, p) == Some(crc) && field(p + 4 + w) == Some(size),
        };
        if stated == Some(size) && content_ok && le32(b, len).is_some_and(starts_entry) {
            return at.checked_add(len as u64);
        }
    }
    None
}

/// Bytes a streamed entry of a method other than DEFLATE and stored (or an
/// encrypted one) is searched for its data descriptor. Nothing in such data
/// can be checked on the way, so a false candidate - a local header in
/// front of foreign data - would otherwise be read to the work limit.
pub(crate) const OPAQUE_WINDOW: u64 = 32 << 20;

/// Bytes kept from one search window to the next: a signature-less
/// descriptor of up to 20 bytes in front of a 4-byte header signature.
const OVERLAP: usize = 24;

/// CRC-32 states of the bytes from the entry's start up to each of
/// `points` (ascending, all in `[pos, pos + w.len()]`), given `crc` of the
/// bytes up to `pos` and the window `w` that starts there.
fn crc_points(mut crc: Crc32, w: &[u8], pos: u64, points: &[u64]) -> Vec<(u64, u32)> {
    let mut out = Vec::with_capacity(points.len());
    let mut at = pos;
    for &q in points {
        let (Some(from), Some(to)) = (
            usize::try_from(at - pos).ok(),
            usize::try_from(q - pos).ok(),
        ) else {
            break;
        };
        if let Some(bytes) = w.get(from..to) {
            crc.update(bytes);
        }
        at = q;
        out.push((q, crc.finish()));
    }
    out
}

/// For an entry of unknown size whose data starts at `data`: scans for its
/// data descriptor and returns the offset of the header that follows it.
///
/// `stored`: the entry is stored, not encrypted; the descriptor must carry
/// the CRC-32 of the bytes in front of it. Otherwise the search gives up
/// after [`OPAQUE_WINDOW`] bytes.
fn find_descriptor(src: &mut Src, data: u64, zip64: bool, stored: bool) -> Option<u64> {
    src.scan_mode();
    let mut pos = data;
    // CRC-32 of the bytes `data..pos`, when it is needed
    let mut crc = Crc32::new();
    let mut candidates: Vec<(u64, u32)> = Vec::new();
    let mut points: Vec<u64> = Vec::new();
    let found = loop {
        if !stored && pos - data > OPAQUE_WINDOW {
            break None;
        }
        let w = src.window_min(pos, OVERLAP + 4);
        if w.len() < 4 {
            break None;
        }
        let wlen = w.len();
        let last = wlen <= OVERLAP;
        // Signatures in the first bytes of a window were looked at in the
        // previous one, which also holds the descriptor in front of them.
        let from_rel = if pos == data { 0 } else { OVERLAP - 4 };
        candidates.clear();
        let mut from = from_rel;
        while let Some(p) = w.get(from..).and_then(|s| crate::util::find_sub(s, b"PK")) {
            let at = from + p;
            if let Some(sig) = le32(w, at) {
                if sig == SIG_DESCRIPTOR || sig == SIG_LOCAL || sig == SIG_CENTRAL {
                    candidates.push((pos + at as u64, sig));
                }
            }
            from = at + 2;
        }
        // the CRC at every place a descriptor may start, and where the
        // next window starts
        let next = pos + if last { wlen } else { wlen - OVERLAP } as u64;
        let mut crcs = Vec::new();
        if stored {
            points.clear();
            for &(at, sig) in &candidates {
                if sig == SIG_DESCRIPTOR {
                    points.push(at);
                } else {
                    points.extend([20u64, 12].iter().filter_map(|&n| {
                        at.checked_sub(n).filter(|&d| d >= data && d >= pos)
                    }));
                }
            }
            points.push(next);
            points.sort_unstable();
            points.dedup();
            crcs = crc_points(crc, w, pos, &points);
            if let Some(bytes) = usize::try_from(next - pos).ok().and_then(|n| w.get(..n)) {
                crc.update(bytes);
            }
        }
        let crc_at = |d: u64| -> Option<u32> {
            crcs.binary_search_by_key(&d, |&(q, _)| q)
                .ok()
                .and_then(|i| crcs.get(i))
                .map(|&(_, c)| c)
        };
        let mut hit = None;
        for &(at, sig) in &candidates {
            hit = if sig == SIG_DESCRIPTOR {
                // the descriptor carries a signature
                let expect = if stored {
                    match crc_at(at) {
                        Some(c) => Expect::Stored(c),
                        None => continue,
                    }
                } else {
                    Expect::Size
                };
                skip_descriptor(src, at, at - data, zip64, expect)
            } else {
                // a header: the descriptor in front of it has no signature
                descriptor_without_signature(src, data, at, zip64, stored.then_some(&crc_at))
            };
            if hit.is_some() {
                break;
            }
        }
        if hit.is_some() {
            break hit;
        }
        if last {
            break None;
        }
        pos = next;
    };
    src.hop_mode();
    found
}

/// Checks for a signature-less descriptor that ends at `header`; for a
/// stored entry `crc_at` gives the CRC-32 of the data up to a place.
fn descriptor_without_signature(
    src: &mut Src,
    data: u64,
    header: u64,
    zip64: bool,
    crc_at: Option<&dyn Fn(u64) -> Option<u32>>,
) -> Option<u64> {
    let order = if zip64 { [20u64, 12] } else { [12u64, 20] };
    for len in order {
        let Some(start) = header.checked_sub(len).filter(|&s| s >= data) else {
            continue;
        };
        let size = start - data;
        // crc, then the compressed and the uncompressed size
        let mut d = [0u8; 20];
        let read = if len == 20 {
            src.side::<20>(start).map(|a| d = a)
        } else {
            src.side::<12>(start).map(|a| d[..12].copy_from_slice(&a))
        };
        if read.is_none() {
            continue;
        }
        let (stated, plain) = if len == 20 {
            (le64(&d, 4), le64(&d, 12))
        } else {
            (le32(&d, 4).map(u64::from), le32(&d, 8).map(u64::from))
        };
        if stated != Some(size) {
            continue;
        }
        match crc_at {
            None => return Some(header),
            Some(crc_at) => {
                let crc = crc_at(start);
                if crc.is_some() && le32(&d, 0) == crc && plain == Some(size) {
                    return Some(header);
                }
            }
        }
    }
    None
}

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    let limit = src.limit();
    let mut pos = 0u64;
    let mut contents = Contents::default();
    let mut entries = 0u64;
    // an entry was delimited by a descriptor found in data that could not
    // be checked
    let mut opaque = false;

    // local headers and data
    let mut records = 0u64;
    loop {
        records += 1;
        if records > MAX_ENTRIES {
            return None;
        }
        let sig = src.le32(pos)?;
        if sig == SIG_CENTRAL {
            break;
        }
        if sig == SIG_EXTRA_DATA {
            // archive extra data record (central directory encryption)
            let len = src.le32(pos.checked_add(4)?)? as u64;
            pos = pos.checked_add(8 + len)?;
            continue;
        }
        if sig != SIG_LOCAL {
            return None;
        }
        entries += 1;
        let h = src.arr::<30>(pos)?;
        let flags = le16(&h, 6)?;
        let method = le16(&h, 8)?;
        let size32 = le32(&h, 18)?;
        let name_len = le16(&h, 26)? as u64;
        let extra_len = le16(&h, 28)? as u64;
        if !known_method(method) {
            return None;
        }
        let name_at = pos.checked_add(30)?;
        let extra_at = name_at.checked_add(name_len)?;
        let data = extra_at.checked_add(extra_len)?;
        if data > limit {
            return None;
        }

        let mut name = [0u8; 64];
        let shown = name_len.min(name.len() as u64) as usize;
        name.get_mut(..shown)?
            .copy_from_slice(src.bytes(name_at, shown)?);
        // names longer than the buffer cannot equal any name of interest
        let name = if name_len as usize == shown {
            name.get(..shown)?
        } else {
            &[][..]
        };
        contents.name(name);

        // ZIP64 extended information: uncompressed, then compressed size
        let mut zip64 = false;
        let mut size64 = None;
        let extra = src.bytes(extra_at, extra_len as usize)?;
        let mut e = 0usize;
        while let (Some(id), Some(len)) = (le16(extra, e), le16(extra, e + 2)) {
            if id == 0x0001 {
                zip64 = true;
                size64 = le64(extra, e + 12).filter(|_| len >= 16);
            }
            e += 4 + len as usize;
        }

        let streamed = flags & 0x0008 != 0;
        let size = if size32 == 0xFFFF_FFFF {
            size64?
        } else {
            size32 as u64
        };
        pos = if streamed && size == 0 && method == 8 && flags & 1 == 0 {
            // DEFLATE delimits itself: the descriptor must follow the end
            // of the stream. A stream that breaks off is the end of the
            // candidate (a fragmented archive, or none at all); searching
            // on for something that looks like a descriptor would read
            // to the work limit.
            src.scan_mode();
            let end = deflate::end(src, data);
            src.hop_mode();
            let end = end?;
            skip_descriptor(src, end, end - data, zip64, Expect::Size)?
        } else if streamed && size == 0 {
            // Either really empty, or unknown when the header was written:
            // the descriptor decides. A stored entry's descriptor must
            // carry the CRC-32 of the bytes in front of it; for any other
            // method nothing on the way can be checked, so the search is
            // bounded and the archive's end is not verified.
            let stored = method == 0 && flags & 1 == 0;
            opaque |= !stored;
            find_descriptor(src, data, zip64, stored)?
        } else {
            let end = data.checked_add(size)?;
            if end > limit {
                return None;
            }
            if entries == 1 && name == b"mimetype" && method == 0 && size <= 96 {
                contents.mimetype(src.bytes(data, size as usize)?);
            }
            if streamed {
                skip_descriptor(src, end, size, zip64, Expect::Size)?
            } else {
                end
            }
        };
    }
    if entries == 0 {
        return None;
    }

    // central directory
    let directory = pos;
    let mut listed = 0u64;
    while src.le32(pos) == Some(SIG_CENTRAL) {
        listed += 1;
        if listed > MAX_ENTRIES {
            return None;
        }
        let h = src.arr::<46>(pos)?;
        let variable = le16(&h, 28)? as u64 + le16(&h, 30)? as u64 + le16(&h, 32)? as u64;
        pos = pos.checked_add(46 + variable)?;
    }
    let directory_len = pos - directory;
    if src.le32(pos)? == SIG_DIGITAL_SIGNATURE {
        let len = src.le16(pos.checked_add(4)?)? as u64;
        pos = pos.checked_add(6 + len)?;
    }

    // optional ZIP64 end record and its locator
    let mut wide: Option<(u64, u64, u64)> = None;
    let mut consistent = true;
    if src.le32(pos)? == SIG_END64 {
        let h = src.arr::<56>(pos)?;
        let rest = le64(&h, 4)?;
        if rest < 44 {
            return None;
        }
        wide = Some((le64(&h, 32)?, le64(&h, 40)?, le64(&h, 48)?));
        let locator_at = pos.checked_add(12)?.checked_add(rest)?;
        let locator = src.arr::<20>(locator_at)?;
        if le32(&locator, 0)? != SIG_LOCATOR64 {
            return None;
        }
        consistent &= le64(&locator, 8)? == pos;
        pos = locator_at.checked_add(20)?;
    }

    let h = src.arr::<22>(pos)?;
    if le32(&h, 0)? != SIG_END {
        return None;
    }
    let count = le16(&h, 10)?;
    let size = le32(&h, 12)?;
    let offset = le32(&h, 16)?;
    let comment = le16(&h, 20)? as u64;
    let end = pos.checked_add(22 + comment)?;

    let (wide_count, wide_size, wide_offset) = wide.unwrap_or((u64::MAX, u64::MAX, u64::MAX));
    // The directory must be where the end record says it is, counted from
    // this candidate's first byte. If it is not, the candidate is a local
    // header somewhere inside an archive (or behind a self-extractor stub),
    // not the start of one.
    let stated_offset = if offset == 0xFFFF_FFFF && wide.is_some() {
        wide_offset
    } else {
        offset as u64
    };
    if stated_offset != directory {
        return None;
    }
    consistent &= if count == 0xFFFF && wide.is_some() {
        wide_count == listed
    } else {
        count as u64 == listed & 0xFFFF
    };
    consistent &= if size == 0xFFFF_FFFF && wide.is_some() {
        wide_size == directory_len
    } else {
        size as u64 == directory_len
    };

    let confidence = if consistent {
        Confidence::Exact
    } else {
        Confidence::Probable
    };
    Some(Measured {
        len: end,
        confidence,
        ext: contents.ext(),
        // the end record, found where the walk through every entry and the
        // central directory led, names that directory's offset; with counts
        // or size that disagree it is not trusted, nor when an entry's end
        // rests on a descriptor found in data that could not be checked
        end_verified: consistent && !opaque,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(names: &[&str], mimetype: Option<&str>) -> &'static str {
        let mut c = Contents::default();
        for n in names {
            c.name(n.as_bytes());
        }
        if let Some(m) = mimetype {
            c.mimetype(m.as_bytes());
        }
        c.ext()
    }

    #[test]
    fn extension_from_contents() {
        assert_eq!(classify(&["a.txt", "b/c.png"], None), "zip");
        assert_eq!(
            classify(
                &["[Content_Types].xml", "_rels/.rels", "word/document.xml"],
                None
            ),
            "docx"
        );
        assert_eq!(
            classify(&["xl/workbook.xml", "[Content_Types].xml"], None),
            "xlsx"
        );
        assert_eq!(
            classify(&["[Content_Types].xml", "ppt/presentation.xml"], None),
            "pptx"
        );
        assert_eq!(
            classify(&["[Content_Types].xml", "visio/document.xml"], None),
            "vsdx"
        );
        // a folder called word/ alone does not make a document
        assert_eq!(classify(&["word/notes.txt"], None), "zip");
        assert_eq!(
            classify(&["[Content_Types].xml", "FixedDocSeq.fdseq"], None),
            "zip"
        );
        assert_eq!(
            classify(&["META-INF/MANIFEST.MF", "org/x/Main.class"], None),
            "jar"
        );
        assert_eq!(
            classify(
                &["META-INF/MANIFEST.MF", "AndroidManifest.xml", "classes.dex"],
                None
            ),
            "apk"
        );
        assert_eq!(
            classify(
                &["mimetype", "META-INF/container.xml"],
                Some("application/epub+zip")
            ),
            "epub"
        );
        assert_eq!(
            classify(
                &["mimetype"],
                Some("application/vnd.oasis.opendocument.text")
            ),
            "odt"
        );
        assert_eq!(
            classify(
                &["mimetype"],
                Some("application/vnd.oasis.opendocument.spreadsheet\n")
            ),
            "ods"
        );
        assert_eq!(
            classify(
                &["mimetype"],
                Some("application/vnd.oasis.opendocument.presentation")
            ),
            "odp"
        );
        assert_eq!(
            classify(&["mimetype"], Some("application/x-something-else")),
            "zip"
        );
    }

    #[test]
    fn methods() {
        for m in [0u16, 8, 9, 12, 14, 93, 95, 98, 99] {
            assert!(known_method(m), "{m}");
        }
        for m in [11u16, 13, 15, 17, 21, 92, 100, 0x8000, 0xFFFF] {
            assert!(!known_method(m), "{m}");
        }
    }
}
