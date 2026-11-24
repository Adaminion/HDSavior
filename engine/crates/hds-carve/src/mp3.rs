//! MP3 (MPEG-1/2/2.5 audio layer III): ID3v2 tag, frame walk, trailing tags.
//!
//! An MP3 file is a sequence of frames, each starting with a 32-bit header:
//! 11 sync bits, version, layer, bit-rate index, sampling-rate index, padding
//! flag, channel mode. Header fields give the frame length, so frames can be
//! followed without decoding. Most files start with an ID3v2 tag (`ID3`,
//! version, flags, 28-bit "synchsafe" size) and many end with a 128-byte
//! ID3v1 tag (`TAG`) or an APEv2 tag. Encoders usually make the first frame
//! an empty one carrying a `Xing`/`Info` (or `VBRI`) header with the number
//! of frames and bytes in the stream.
//!
//! The 11-bit sync word is far too weak a signature, so [`sniff`](crate::sniff)
//! accepts an MP3 only when it starts with an ID3v2 tag or with a frame that
//! carries a `Xing`/`Info`/`VBRI` header. A file of bare frames is not
//! recognised when carving (it is by [`matches_extension`](crate::matches_extension)).
//!
//! There is no end marker. The walker follows frames while they stay valid
//! and consistent (same version, layer and sampling rate); if the `Xing`
//! header states the stream length and a frame boundary falls exactly there,
//! the walk stops at that point, otherwise at the first position that is not
//! a frame. A frame carrying another `Xing` header, or an `ID3` tag, is the
//! start of the next file. Trailing APEv2 and ID3v1 tags are included. The
//! result is always `Probable`: a last frame cut short by the recorder, or a
//! tag format this walker does not know, cannot be told from foreign data.

use crate::reader::Src;
use crate::util::{be32, le32};
use crate::{Confidence, Measured};

/// Fewest consecutive consistent frames accepted as an MP3 stream.
const MIN_FRAMES: u64 = 4;
/// Largest ID3v2 tag accepted in front of the audio (cover art included).
const MAX_ID3: u64 = 64 * 1024 * 1024;

/// A decoded frame header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Frame {
    /// 3 = MPEG-1, 2 = MPEG-2, 0 = MPEG-2.5 (the header's own encoding).
    version: u8,
    /// 1, 2 or 3.
    layer: u8,
    sample_rate: u32,
    /// Frame length in bytes, header included.
    pub(crate) len: u32,
    mono: bool,
    crc: bool,
}

impl Frame {
    /// Offset, from the start of the frame, of a `Xing`/`Info` header: it
    /// follows the header (and CRC) and the layer III side information.
    fn vbr_tag_offset(&self) -> usize {
        let side = match (self.version == 3, self.mono) {
            (true, false) => 32,
            (true, true) | (false, false) => 17,
            (false, true) => 9,
        };
        4 + if self.crc { 2 } else { 0 } + side
    }
}

/// Decodes a frame header; `None` for anything a decoder would reject.
pub(crate) fn parse_frame(b: [u8; 4]) -> Option<Frame> {
    if b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (b[1] >> 3) & 3;
    let layer = 4 - ((b[1] >> 1) & 3);
    let bitrate_index = (b[2] >> 4) as usize;
    let rate_index = ((b[2] >> 2) & 3) as usize;
    let padding = ((b[2] >> 1) & 1) as u32;
    // reserved version, layer, sampling rate or emphasis; free or bad bit rate
    if version == 1
        || layer == 4
        || rate_index == 3
        || b[3] & 3 == 2
        || bitrate_index == 0
        || bitrate_index == 15
    {
        return None;
    }
    const V1_L1: [u32; 14] = [
        32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ];
    const V1_L2: [u32; 14] = [
        32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    const V1_L3: [u32; 14] = [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const V2_L1: [u32; 14] = [
        32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ];
    const V2_L23: [u32; 14] = [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let table = match (version == 3, layer) {
        (true, 1) => &V1_L1,
        (true, 2) => &V1_L2,
        (true, _) => &V1_L3,
        (false, 1) => &V2_L1,
        (false, _) => &V2_L23,
    };
    let bitrate = *table.get(bitrate_index - 1)? * 1000;
    let base = *[44_100u32, 48_000, 32_000].get(rate_index)?;
    let sample_rate = match version {
        3 => base,
        2 => base / 2,
        _ => base / 4,
    };
    let len = match layer {
        1 => (12 * bitrate / sample_rate + padding) * 4,
        2 => 144 * bitrate / sample_rate + padding,
        _ => (if version == 3 { 144 } else { 72 }) * bitrate / sample_rate + padding,
    };
    Some(Frame {
        version,
        layer,
        sample_rate,
        len,
        mono: b[3] >> 6 == 3,
        crc: b[1] & 1 == 0,
    })
}

/// Total length of an ID3v2 tag whose 10-byte header is at the start of `h`.
pub(crate) fn id3v2_len(h: &[u8]) -> Option<u64> {
    let h = h.get(..10)?;
    if !h.starts_with(b"ID3") {
        return None;
    }
    let (major, revision, flags) = (h[3], h[4], h[5]);
    if !(2..=4).contains(&major) || revision != 0 || flags & 0x0F != 0 {
        return None;
    }
    let mut size = 0u64;
    for &b in h.get(6..10)? {
        if b & 0x80 != 0 {
            return None;
        }
        size = size << 7 | b as u64;
    }
    // version 2.4 may repeat the header as a footer
    let footer = if major == 4 && flags & 0x10 != 0 {
        10
    } else {
        0
    };
    Some(10 + size + footer)
}

/// `true` when `frame`, the bytes of a frame from its header on, carries a
/// `Xing`, `Info` or `VBRI` header.
pub(crate) fn has_vbr_tag(frame: &[u8], f: &Frame) -> bool {
    let at = f.vbr_tag_offset();
    matches!(frame.get(at..at + 4), Some(b"Xing" | b"Info")) || frame.get(36..40) == Some(b"VBRI")
}

/// The check `sniff` applies to a head that starts with a frame sync:
/// a layer III frame with a VBR header.
#[cold]
pub(crate) fn sniff_frame(h: &[u8]) -> bool {
    let Some(f) = crate::util::rd::<4>(h, 0).and_then(parse_frame) else {
        return false;
    };
    f.layer == 3 && has_vbr_tag(h, &f)
}

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    src.scan_mode();
    let limit = src.limit();
    let mut pos = 0u64;
    // a file may carry more than one ID3v2 tag (rare, but taggers do it)
    for _ in 0..4 {
        let Some(len) = src.arr::<10>(pos).and_then(|h| id3v2_len(&h)) else {
            break;
        };
        if len > MAX_ID3 {
            return None;
        }
        pos = pos.checked_add(len)?;
    }
    let audio = pos;
    let first = parse_frame(src.arr::<4>(pos)?).filter(|f| f.layer == 3)?;

    // stream length announced by a Xing/Info header: flags, [frames], [bytes]
    let mut declared_end = None;
    {
        let head = src.peek(pos, 64);
        let at = first.vbr_tag_offset();
        if matches!(head.get(at..at + 4), Some(b"Xing" | b"Info")) {
            let flags = be32(head, at + 4)?;
            if flags & 2 != 0 {
                let bytes = be32(head, at + 8 + if flags & 1 != 0 { 4 } else { 0 })?;
                declared_end = audio.checked_add(bytes as u64).filter(|_| bytes != 0);
            }
        }
    }

    let mut frames = 0u64;
    loop {
        if frames > 0 && declared_end == Some(pos) {
            break;
        }
        let Some(f) = src.arr::<4>(pos).and_then(parse_frame) else {
            break;
        };
        if f.version != first.version
            || f.layer != first.layer
            || f.sample_rate != first.sample_rate
        {
            break;
        }
        let Some(next) = pos.checked_add(f.len as u64).filter(|&n| n <= limit) else {
            break;
        };
        if frames > 0 && has_vbr_tag(src.peek(pos, 40), &f) {
            // the first frame of the next file
            break;
        }
        // the whole frame has to exist
        if !src.has_byte(next - 1) {
            break;
        }
        pos = next;
        frames += 1;
    }
    if frames < MIN_FRAMES {
        return None;
    }

    // APEv2 tag with a header: preamble, version, size (items and footer),
    // item count, flags (bit 29: this is the header)
    if let Some(h) = src.arr::<32>(pos) {
        if h.starts_with(b"APETAGEX") {
            let size = le32(&h, 12)? as u64;
            let is_header = le32(&h, 20)? & 0x2000_0000 != 0;
            let end = pos.checked_add(32 + size)?;
            if is_header && (32..=16 << 20).contains(&size) && end <= limit && src.has_byte(end - 1)
            {
                pos = end;
            }
        }
    }
    // ID3v1
    if src.arr::<3>(pos) == Some(*b"TAG") && src.has_byte(pos.saturating_add(127)) {
        pos += 128;
    }
    Some(Measured {
        len: pos,
        confidence: Confidence::Probable,
        ext: "mp3",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_lengths() {
        // MPEG-1 layer III, 128 kbit/s, 44.1 kHz, stereo: 417 bytes (+1 padded)
        let f = parse_frame([0xFF, 0xFB, 0x90, 0x00]).unwrap();
        assert_eq!(
            (f.version, f.layer, f.sample_rate, f.len, f.mono, f.crc),
            (3, 3, 44_100, 417, false, false)
        );
        assert_eq!(parse_frame([0xFF, 0xFB, 0x92, 0x00]).unwrap().len, 418);
        assert_eq!(f.vbr_tag_offset(), 36);
        // MPEG-2 layer III, 64 kbit/s, 22.05 kHz
        let f = parse_frame([0xFF, 0xF3, 0x80, 0xC4]).unwrap();
        assert_eq!(
            (f.version, f.sample_rate, f.len, f.mono),
            (2, 22_050, 208, true)
        );
        assert_eq!(f.vbr_tag_offset(), 13);
        // MPEG-2.5, 16 kbit/s, 11.025 kHz
        let f = parse_frame([0xFF, 0xE3, 0x20, 0xC4]).unwrap();
        assert_eq!((f.version, f.sample_rate, f.len), (0, 11_025, 104));
        // layer I: 32 kbit/s at 32 kHz, 12 slots of 4 bytes
        assert_eq!(parse_frame([0xFF, 0xFF, 0x18, 0x00]).unwrap().len, 48);
        // with CRC the tag moves by two bytes
        assert_eq!(
            parse_frame([0xFF, 0xFA, 0x90, 0x00])
                .unwrap()
                .vbr_tag_offset(),
            38
        );
    }

    #[test]
    fn rejects_reserved_fields() {
        assert!(
            parse_frame([0xFF, 0xD8, 0xFF, 0xE0]).is_none(),
            "JPEG SOI is not a sync word"
        );
        assert!(
            parse_frame([0xFF, 0xEB, 0x90, 0x00]).is_none(),
            "reserved version"
        );
        assert!(
            parse_frame([0xFF, 0xF9, 0x90, 0x00]).is_none(),
            "reserved layer"
        );
        assert!(
            parse_frame([0xFF, 0xFB, 0x00, 0x00]).is_none(),
            "free format"
        );
        assert!(
            parse_frame([0xFF, 0xFB, 0xF0, 0x00]).is_none(),
            "bad bit rate"
        );
        assert!(
            parse_frame([0xFF, 0xFB, 0x9C, 0x00]).is_none(),
            "reserved sampling rate"
        );
        assert!(
            parse_frame([0xFF, 0xFB, 0x90, 0x02]).is_none(),
            "reserved emphasis"
        );
        assert!(
            parse_frame([0xFF, 0xFF, 0xFF, 0xFF]).is_none(),
            "erased flash is not audio"
        );
    }

    #[test]
    fn id3_header() {
        assert_eq!(id3v2_len(b"ID3\x04\x00\x00\x00\x00\x00\x23"), Some(45));
        assert_eq!(
            id3v2_len(b"ID3\x03\x00\x00\x00\x00\x02\x01"),
            Some(10 + 257)
        );
        assert_eq!(
            id3v2_len(b"ID3\x04\x00\x10\x00\x00\x00\x23"),
            Some(55),
            "footer"
        );
        assert_eq!(id3v2_len(b"ID3\x05\x00\x00\x00\x00\x00\x23"), None);
        assert_eq!(
            id3v2_len(b"ID3\x03\x00\x00\x00\x00\x80\x23"),
            None,
            "size bytes are 7-bit"
        );
        assert_eq!(
            id3v2_len(b"ID3\x03\x00\x01\x00\x00\x00\x23"),
            None,
            "undefined flag"
        );
        assert_eq!(id3v2_len(b"ID3\x03\x00"), None);
    }
}
