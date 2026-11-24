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
//! Those entries have to be scanned for the descriptor, which is accepted
//! only if the compressed size it states equals the distance scanned and a
//! header signature follows it - a ZIP stored inside the entry brings its
//! own descriptors, and they fail that test.
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

use crate::reader::Src;
use crate::util::{le16, le32, le64};
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

/// After the data of an entry whose compressed size is known but which also
/// has bit 3 set: the offset after its data descriptor. The descriptor has
/// four possible shapes (with or without signature, 32- or 64-bit sizes).
fn skip_descriptor(src: &mut Src, at: u64, size: u64, zip64: bool) -> Option<u64> {
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
        let stated = if wide {
            le64(b, p + 4)
        } else {
            le32(b, p + 4).map(u64::from)
        };
        let len = p + 4 + if wide { 16 } else { 8 };
        if stated == Some(size) && le32(b, len).is_some_and(starts_entry) {
            return at.checked_add(len as u64);
        }
    }
    None
}

/// For an entry of unknown size whose data starts at `data`: scans for its
/// data descriptor and returns the offset of the header that follows it.
fn find_descriptor(src: &mut Src, data: u64, zip64: bool) -> Option<u64> {
    src.scan_mode();
    let mut pos = data;
    let mut candidates: Vec<(u64, u32)> = Vec::new();
    let found = loop {
        let w = src.window_min(pos, 4);
        if w.len() < 4 {
            break None;
        }
        let wlen = w.len();
        candidates.clear();
        let mut from = 0usize;
        while let Some(p) = w.get(from..).and_then(|s| crate::util::find_sub(s, b"PK")) {
            let at = from + p;
            if let Some(sig) = le32(w, at) {
                if sig == SIG_DESCRIPTOR || sig == SIG_LOCAL || sig == SIG_CENTRAL {
                    candidates.push((pos + at as u64, sig));
                }
            }
            from = at + 2;
        }
        let mut hit = None;
        for &(at, sig) in &candidates {
            hit = if sig == SIG_DESCRIPTOR {
                // the descriptor carries a signature
                skip_descriptor(src, at, at - data, zip64)
            } else {
                // a header: the descriptor in front of it has no signature
                descriptor_without_signature(src, data, at, zip64)
            };
            if hit.is_some() {
                break;
            }
        }
        if hit.is_some() {
            break hit;
        }
        pos = pos.checked_add((wlen - 3) as u64)?;
    };
    src.hop_mode();
    found
}

/// Checks for a signature-less descriptor that ends at `header`.
fn descriptor_without_signature(src: &mut Src, data: u64, header: u64, zip64: bool) -> Option<u64> {
    let order = if zip64 { [20u64, 12] } else { [12u64, 20] };
    for len in order {
        let Some(start) = header.checked_sub(len).filter(|&s| s >= data) else {
            continue;
        };
        let size = start - data;
        // (crc, then the compressed size; `start + 4 < header` cannot overflow)
        let stated = if len == 20 {
            src.side::<8>(start + 4).map(u64::from_le_bytes)
        } else {
            src.side::<4>(start + 4)
                .map(|a| u32::from_le_bytes(a) as u64)
        };
        if stated == Some(size) {
            return Some(header);
        }
    }
    None
}

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    let limit = src.limit();
    let mut pos = 0u64;
    let mut contents = Contents::default();
    let mut entries = 0u64;

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
        pos = if streamed && size == 0 {
            // Either really empty, or unknown when the header was written:
            // the descriptor decides, wherever it is.
            find_descriptor(src, data, zip64)?
        } else {
            let end = data.checked_add(size)?;
            if end > limit {
                return None;
            }
            if entries == 1 && name == b"mimetype" && method == 0 && size <= 96 {
                contents.mimetype(src.bytes(data, size as usize)?);
            }
            if streamed {
                skip_descriptor(src, end, size, zip64)?
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
        // or size that disagree it is not trusted
        end_verified: consistent,
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
