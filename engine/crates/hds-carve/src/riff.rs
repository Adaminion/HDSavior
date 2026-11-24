//! RIFF containers: AVI (including OpenDML files beyond 2 GiB), WAV
//! (including RF64 / BW64) and WebP.
//!
//! A RIFF file is one chunk: `"RIFF" size(4, LE) form(4)` followed by
//! `size - 4` bytes of sub-chunks `id(4) size(4, LE) data`, each padded to an
//! even length. The total file length is `8 + size`.
//!
//! * AVI: the form is `AVI `, the first sub-chunk is `LIST hdrl`, the frames
//!   are in `LIST movi`. A 32-bit size cannot describe a long recording, so
//!   OpenDML writers close the first RIFF chunk at about 1 GiB and continue
//!   with further top-level chunks `RIFF ... AVIX`. The file ends after the
//!   last of them.
//! * WAV: the form is `WAVE` with a `fmt ` and a `data` chunk. RF64 (EBU
//!   Tech 3306) and BW64 (ITU-R BS.2088) replace `RIFF` by `RF64`/`BW64`, set
//!   the 32-bit sizes to `FFFFFFFF` and put the real 64-bit sizes into a
//!   mandatory first chunk `ds64`.
//! * WebP: the form is `WEBP`, the first sub-chunk is `VP8 `, `VP8L` or
//!   `VP8X`.
//!
//! Only chunk headers are read; `movi` and `data` are skipped. `Exact` means
//! the sub-chunk chain ends precisely where the RIFF size says the file
//! ends, which cross-checks the size field against the structure. If the
//! chain breaks earlier but the essential chunks were seen, the size field
//! is reported as `Probable`. A recording that was never finalised (size 0
//! or `FFFFFFFF` without `ds64`) cannot be measured.

use crate::reader::Src;
use crate::util::{is_printable, le32, le64};
use crate::{Confidence, FileKind, Measured};

const MAX_CHUNKS: u32 = 1 << 20;
const MAX_DS64_TABLE: u64 = 64;
/// RIFF-AVIX continuation chunks (about 1 GiB each in real files).
const MAX_PARTS: u32 = 65_536;

#[derive(Default)]
struct Seen {
    fmt: bool,
    data: bool,
    hdrl: bool,
    movi: bool,
    image: bool,
    /// The chunk read last carries the bulk payload (`data`, `LIST movi`,
    /// image data), whose length only its own header states.
    payload_last: bool,
}

/// 64-bit sizes of an RF64 file.
struct Ds64 {
    data: u64,
    table_entries: u64,
}

impl Ds64 {
    /// The 64-bit size of a chunk whose 32-bit size is saturated.
    fn size_of(&self, src: &mut Src, id: &[u8; 4]) -> Option<u64> {
        if id == b"data" {
            return Some(self.data);
        }
        for i in 0..self.table_entries.min(MAX_DS64_TABLE) {
            let e = src.arr::<12>(48 + 12 * i)?;
            if e.starts_with(id) {
                return le64(&e, 4);
            }
        }
        None
    }
}

/// Walks the sub-chunks in `[start, end)`. `Some(true)`: the chain ends
/// exactly at `end`; `Some(false)`: it breaks before; `None`: the data
/// itself ends before `end`.
fn walk(src: &mut Src, start: u64, end: u64, ds64: Option<&Ds64>, seen: &mut Seen) -> Option<bool> {
    let mut pos = start;
    for _ in 0..MAX_CHUNKS {
        if pos >= end {
            return Some(pos == end);
        }
        if end - pos < 8 {
            return Some(false);
        }
        let h = src.arr::<8>(pos)?;
        let id = [h[0], h[1], h[2], h[3]];
        let size32 = le32(&h, 4)?;
        if !id.iter().all(|&c| is_printable(c)) {
            return Some(false);
        }
        let size = match ds64 {
            Some(d) if size32 == u32::MAX => match d.size_of(src, &id) {
                Some(s) => s,
                None => return Some(false),
            },
            _ => size32 as u64,
        };
        let body = pos + 8;
        let Some(unpadded) = body.checked_add(size).filter(|&e| e <= end) else {
            return Some(false);
        };
        seen.payload_last = false;
        match &id {
            b"fmt " => seen.fmt = true,
            b"data" => {
                seen.data = true;
                seen.payload_last = true;
            }
            b"VP8 " | b"VP8L" | b"ANMF" | b"ALPH" => {
                seen.image |= id != *b"ALPH";
                seen.payload_last = true;
            }
            b"LIST" => match &src.arr::<4>(body)? {
                b"hdrl" => seen.hdrl = true,
                b"movi" => {
                    seen.movi = true;
                    seen.payload_last = true;
                }
                _ => {}
            },
            _ => {}
        }
        // chunks are padded to even sizes; a last odd chunk is often not
        pos = unpadded.saturating_add(size & 1).min(end);
    }
    None
}

pub(crate) fn measure(src: &mut Src, kind: FileKind) -> Option<Measured> {
    let limit = src.limit();
    let h = src.arr::<12>(0)?;
    let wide = match &h[..4] {
        b"RIFF" => false,
        b"RF64" | b"BW64" => true,
        _ => return None,
    };
    let (form, ext): (&[u8; 4], _) = match kind {
        FileKind::Avi => (b"AVI ", "avi"),
        FileKind::Wav => (b"WAVE", "wav"),
        FileKind::Webp => (b"WEBP", "webp"),
        _ => return None,
    };
    if &h[8..12] != form || (wide && kind != FileKind::Wav) {
        return None;
    }

    let mut ds64 = None;
    let size = if wide {
        // the 64-bit sizes: RIFF size, data size, sample count, table length
        let c = src.arr::<36>(12)?;
        if !c.starts_with(b"ds64") || le32(&c, 4)? < 28 {
            return None;
        }
        ds64 = Some(Ds64 {
            data: le64(&c, 16)?,
            table_entries: le32(&c, 32)? as u64,
        });
        le64(&c, 8)?
    } else {
        le32(&h, 4)? as u64
    };
    // form type plus at least one sub-chunk header
    if size < 12 {
        return None;
    }
    let mut end = size.checked_add(8)?;
    if end > limit {
        return None;
    }
    if kind == FileKind::Webp && !matches!(&src.arr::<4>(12)?, b"VP8 " | b"VP8L" | b"VP8X") {
        return None;
    }

    let mut seen = Seen::default();
    let mut exact = walk(src, 12, end, ds64.as_ref(), &mut seen)?;
    let essential = match kind {
        FileKind::Avi => seen.hdrl && seen.movi,
        FileKind::Wav => seen.fmt && seen.data,
        _ => seen.image,
    };
    if !essential {
        return None;
    }

    if kind == FileKind::Avi {
        // OpenDML: the recording continues in RIFF chunks of form AVIX
        let mut parts = 0u32;
        loop {
            parts += 1;
            if parts > MAX_PARTS {
                return None;
            }
            let at = end.saturating_add(end & 1);
            let Some(next) = src.arr::<12>(at) else { break };
            if !next.starts_with(b"RIFF") || &next[8..12] != b"AVIX" {
                break;
            }
            let part = le32(&next, 4)? as u64;
            let part_end = at.checked_add(8)?.checked_add(part)?;
            if part < 4 || part_end > limit {
                // the continuation is cut short: so is the file
                return None;
            }
            exact &= walk(src, at + 12, part_end, None, &mut seen)?;
            end = part_end;
        }
    }

    let confidence = if exact {
        Confidence::Exact
    } else {
        Confidence::Probable
    };
    Some(Measured {
        len: end,
        confidence,
        ext,
        // A chunk header read behind the last payload, with the chain
        // landing on the RIFF size, confirms that payload's extent; a file
        // that ends in its payload has only header arithmetic for its end.
        end_verified: exact && !seen.payload_last,
    })
}
