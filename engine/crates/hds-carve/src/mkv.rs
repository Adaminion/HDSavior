//! Matroska and WebM (EBML, RFC 8794 / RFC 9559): header, segment size,
//! top-level element walk.
//!
//! An EBML file is a tree of elements `id(1-4 bytes) size(1-8 bytes) data`.
//! Both numbers are variable-length integers whose length is given by the
//! position of the first set bit; a size whose value bits are all ones means
//! "unknown". A Matroska file is the `EBML` header element (which names the
//! document type, `matroska` or `webm`) followed by one `Segment` holding
//! everything else: `SeekHead`, `Info`, `Tracks`, `Cluster`s with the media,
//! `Cues`, `Chapters`, `Attachments`, `Tags`, `Void` padding.
//!
//! A muxer that can seek writes the segment size when it finishes, and the
//! file ends at `segment data + size`. The walker does not take that number
//! on trust: it hops over the segment's children (headers only, never the
//! cluster payload) and reports `Exact` only if they are all known element
//! types and the last one ends exactly at the segment end.
//!
//! A muxer that cannot seek (a pipe, a live stream, a crash before the end)
//! leaves the segment size unknown. Then the children are followed for as
//! long as they are valid, complete, sized elements, and the end of the last
//! one is reported as `Probable`. If a cluster itself has unknown size (live
//! WebM), finding its end would mean parsing every block; such files are not
//! measured.

use crate::reader::Src;
use crate::{Confidence, Measured};

const ID_EBML: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_SEEK_HEAD: u32 = 0x114D_9B74;
const ID_INFO: u32 = 0x1549_A966;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_CUES: u32 = 0x1C53_BB6B;
const ID_ATTACHMENTS: u32 = 0x1941_A469;
const ID_CHAPTERS: u32 = 0x1043_A770;
const ID_TAGS: u32 = 0x1254_C367;
const ID_VOID: u32 = 0xEC;
const ID_CRC32: u32 = 0xBF;
const ID_DOCTYPE: u32 = 0x4282;

const MAX_ELEMENTS: u32 = 1 << 21;
/// Largest EBML header accepted (real ones are about 40 bytes).
const MAX_HEADER: u64 = 4096;

/// Element ID at the start of `b`: the ID (marker bits included) and its
/// length in bytes.
fn read_id(b: &[u8]) -> Option<(u32, usize)> {
    let n = b.first()?.leading_zeros() as usize + 1;
    if n > 4 {
        return None;
    }
    let id = b.get(..n)?.iter().fold(0u32, |v, &c| v << 8 | c as u32);
    Some((id, n))
}

/// Element size at the start of `b`: the value (`None` = unknown) and the
/// length of the field in bytes.
fn read_size(b: &[u8]) -> Option<(Option<u64>, usize)> {
    let (&first, rest) = b.split_first()?;
    let n = first.leading_zeros() as usize + 1;
    if n > 8 {
        return None;
    }
    let mut v = (first as u64) & (0xFF_u64 >> n);
    for &c in rest.get(..n - 1)? {
        v = v << 8 | c as u64;
    }
    let unknown = v == (1u64 << (7 * n)) - 1;
    Some((if unknown { None } else { Some(v) }, n))
}

/// An element header: ID, data size (`None` = unknown), header length.
fn element(b: &[u8]) -> Option<(u32, Option<u64>, usize)> {
    let (id, n) = read_id(b)?;
    let (size, m) = read_size(b.get(n..)?)?;
    Some((id, size, n + m))
}

fn element_at(src: &mut Src, pos: u64) -> Option<(u32, Option<u64>, usize)> {
    element(src.peek(pos, 12))
}

/// The document types this crate carves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DocType {
    Matroska,
    Webm,
}

/// Parses an EBML header at the start of `h` as far as `h` reaches: the
/// document type and the offset of the element after the header. `None` if
/// this is not an EBML header or the document type is not visible in `h`.
pub(crate) fn header(h: &[u8]) -> Option<(DocType, u64)> {
    let (id, size, head) = element(h)?;
    let size = size.filter(|&s| s <= MAX_HEADER)?;
    if id != ID_EBML {
        return None;
    }
    let end = head as u64 + size;
    let body = h.get(head..)?;
    let body = body.get(..(size as usize).min(body.len()))?;
    let mut p = 0usize;
    while let Some((id, size, n)) = body.get(p..).and_then(element) {
        // every child of the EBML header has a two-byte ID 42xx; padding and
        // checksums are the only others allowed anywhere
        if id >> 8 != 0x42 && id != ID_VOID && id != ID_CRC32 {
            return None;
        }
        let size = usize::try_from(size?).ok()?;
        let data = p.checked_add(n)?;
        if id == ID_DOCTYPE {
            return match body.get(data..data.checked_add(size)?)? {
                b"matroska" => Some((DocType::Matroska, end)),
                b"webm" => Some((DocType::Webm, end)),
                _ => None,
            };
        }
        p = data.checked_add(size)?;
    }
    None
}

fn level_one(id: u32) -> bool {
    matches!(
        id,
        ID_SEEK_HEAD
            | ID_INFO
            | ID_TRACKS
            | ID_CLUSTER
            | ID_CUES
            | ID_ATTACHMENTS
            | ID_CHAPTERS
            | ID_TAGS
            | ID_VOID
            | ID_CRC32
    )
}

/// `true` when the `Tracks` element at `[data, data + size)` declares audio
/// but no video track. `TrackType` is element `83`, one byte: 1 video,
/// 2 audio.
fn audio_only(src: &mut Src, data: u64, size: u64) -> bool {
    let b = src.peek(data, size.min(64 * 1024) as usize);
    let has = |kind: u8| b.windows(3).any(|w| w == [0x83, 0x81, kind]);
    has(2) && !has(1)
}

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    let limit = src.limit();
    let (doctype, mut pos) = header(src.peek(0, 128))?;

    // padding between the header and the segment is legal, if unusual
    let (segment_size, head) = loop {
        let (id, size, head) = element_at(src, pos)?;
        match id {
            ID_SEGMENT => break (size, head),
            ID_VOID if pos < 65_536 => pos = pos.checked_add(head as u64)?.checked_add(size?)?,
            _ => return None,
        }
    };
    let data = pos.checked_add(head as u64)?;
    let segment_end = match segment_size {
        Some(s) => Some(data.checked_add(s).filter(|&e| e <= limit)?),
        None => None,
    };

    let mut p = data;
    let mut tracks = false;
    let mut clusters = 0u32;
    let mut audio = false;
    let mut last = (data, false);
    // the last element that is not padding carries bulk payload (a
    // `Cluster`, `Attachments`), whose extent only its own header states
    let mut payload_last = false;
    let mut elements = 0u32;
    loop {
        elements += 1;
        if elements > MAX_ELEMENTS {
            return None;
        }
        if let Some(end) = segment_end {
            if p == end {
                if !tracks {
                    return None;
                }
                let ext = ext_for(doctype, audio);
                return Some(Measured {
                    len: end,
                    confidence: Confidence::Exact,
                    ext,
                    // an element header read behind the last cluster, and
                    // the chain landing on the segment size
                    end_verified: !payload_last,
                });
            }
        }
        let bound = segment_end.unwrap_or(limit);
        let Some((id, size, head)) = element_at(src, p).filter(|&(id, _, _)| level_one(id)) else {
            break;
        };
        // an element of unknown size inside the segment (a live cluster)
        let size = size?;
        let body = p.checked_add(head as u64)?;
        let Some(next) = body.checked_add(size).filter(|&n| n <= bound) else {
            break;
        };
        match id {
            ID_TRACKS => {
                tracks = true;
                audio = audio_only(src, body, size);
            }
            ID_CLUSTER => clusters += 1,
            _ => {}
        }
        match id {
            ID_CLUSTER | ID_ATTACHMENTS => payload_last = true,
            ID_VOID | ID_CRC32 => {}
            _ => payload_last = false,
        }
        last = (p, id == ID_CLUSTER);
        p = next;
    }

    // Reaching this point with a sized segment means its children do not add
    // up to it: damaged, truncated or not contiguous.
    if segment_end.is_some() {
        return None;
    }
    // Unknown segment size: keep the complete elements. The last hop may
    // have jumped past the end of the data.
    if p > data && !src.has_byte(p - 1) {
        p = last.0;
        if last.1 {
            clusters -= 1;
        }
    }
    if !tracks || clusters == 0 {
        return None;
    }
    Some(Measured {
        len: p,
        confidence: Confidence::Probable,
        ext: ext_for(doctype, audio),
        // without a segment size nothing says this was all
        end_verified: false,
    })
}

fn ext_for(doctype: DocType, audio_only: bool) -> &'static str {
    match (doctype, audio_only) {
        (DocType::Webm, _) => "webm",
        (DocType::Matroska, true) => "mka",
        (DocType::Matroska, false) => "mkv",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_length_integers() {
        assert_eq!(read_id(&[0x1A, 0x45, 0xDF, 0xA3, 0x00]), Some((ID_EBML, 4)));
        assert_eq!(read_id(&[0xEC]), Some((ID_VOID, 1)));
        assert_eq!(read_id(&[0x42, 0x82]), Some((ID_DOCTYPE, 2)));
        assert_eq!(read_id(&[0x08, 0, 0, 0, 0]), None);
        assert_eq!(read_id(&[0x00]), None);
        assert_eq!(read_id(&[0x1A, 0x45]), None);
        assert_eq!(read_size(&[0x81]), Some((Some(1), 1)));
        assert_eq!(read_size(&[0xA3]), Some((Some(0x23), 1)));
        assert_eq!(read_size(&[0x40, 0x02]), Some((Some(2), 2)));
        assert_eq!(read_size(&[0xFF]), Some((None, 1)));
        assert_eq!(
            read_size(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
            Some((None, 8))
        );
        assert_eq!(
            read_size(&[0x01, 0, 0, 0, 0, 0, 0x20, 0xFD]),
            Some((Some(0x20FD), 8))
        );
        assert_eq!(read_size(&[0x00, 0xFF]), None);
        assert_eq!(read_size(&[0x40]), None);
        assert_eq!(read_size(&[]), None);
    }

    #[test]
    fn ebml_header() {
        let mut h = vec![
            0x1A, 0x45, 0xDF, 0xA3, 0xA3, 0x42, 0x86, 0x81, 0x01, 0x42, 0xF7, 0x81, 0x01,
        ];
        h.extend([
            0x42, 0xF2, 0x81, 0x04, 0x42, 0xF3, 0x81, 0x08, 0x42, 0x82, 0x88,
        ]);
        h.extend(b"matroska");
        h.extend([0x42, 0x87, 0x81, 0x04, 0x42, 0x85, 0x81, 0x02]);
        assert_eq!(header(&h), Some((DocType::Matroska, 40)));
        // the document type is all that has to be visible
        assert_eq!(header(&h[..32]), Some((DocType::Matroska, 40)));
        assert_eq!(header(&h[..31]), None);
        let mut other = h.clone();
        other[24..32].copy_from_slice(b"notmedia");
        assert_eq!(header(&other), None);
        let mut bad_child = h.clone();
        bad_child[5] = 0x43;
        assert_eq!(header(&bad_child), None);
    }
}
