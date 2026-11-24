//! `Measured::end_verified`: the rule of "Verified ends" in the crate
//! documentation, held against real files of every kind (the samples in
//! `tests/data`, written by real encoders), against the same files cut
//! short, with their final structure damaged, and with foreign data where a
//! fragmented file would continue, and against layouts the encoders at hand
//! do not write (CR3, HEIC with item data first, a damaged `mfra`).
//!
//! `samples.rs` holds every sample to its expected value whatever follows it
//! on the disk; this file is about the cases where the answer must turn to
//! "not verified" (or to no file at all).

mod common;

use common::{samples, special, Rng, Sample};
use hds_carve::{measure, sniff, Confidence, FileKind, Measured};

fn run(kind: FileKind, data: &[u8]) -> Option<Measured> {
    measure(kind, &data, data.len() as u64)
}

fn rfind(data: &[u8], needle: &[u8]) -> Option<usize> {
    (0..=data.len().checked_sub(needle.len())?)
        .rev()
        .find(|&i| data[i..].starts_with(needle))
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// Top-level boxes of an ISO BMFF file: (offset, size, type).
fn boxes(data: &[u8]) -> Vec<(usize, usize, [u8; 4])> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 8 <= data.len() {
        let size = match be32(data, p) {
            1 => u64::from_be_bytes(data[p + 8..p + 16].try_into().unwrap()) as usize,
            n => n as usize,
        };
        assert!(size >= 8, "box at {p}");
        out.push((p, size, data[p + 4..p + 8].try_into().unwrap()));
        p += size;
    }
    out
}

/// Sub-chunks of a RIFF file's first chunk: (offset, id).
fn riff_chunks(data: &[u8]) -> Vec<(usize, [u8; 4])> {
    let end = 8 + le32(data, 4) as usize;
    let mut out = Vec::new();
    let mut p = 12;
    while p + 8 <= end.min(data.len()) {
        out.push((p, data[p..p + 4].try_into().unwrap()));
        let size = le32(data, p + 4) as usize;
        p += 8 + size + (size & 1);
    }
    out
}

/// EBML variable-length integer at `p`: (value, length); `keep` leaves the
/// length marker in (element IDs).
fn vint(b: &[u8], p: usize, keep: bool) -> (u64, usize) {
    let n = b[p].leading_zeros() as usize + 1;
    let mut v = if keep {
        b[p] as u64
    } else {
        (b[p] as u64) & (0xFF >> n)
    };
    for i in 1..n {
        v = v << 8 | b[p + i] as u64;
    }
    (v, n)
}

/// Offsets of the top-level elements inside a Matroska segment.
fn segment_children(data: &[u8]) -> Vec<usize> {
    let skip = |p: usize| {
        let (_, n) = vint(data, p, true);
        let (size, m) = vint(data, p + n, false);
        (p + n + m, size as usize)
    };
    let (body, size) = skip(0); // EBML header
    let (seg_body, seg_size) = skip(body + size);
    let mut out = Vec::new();
    let mut p = seg_body;
    while p < seg_body + seg_size {
        out.push(p);
        let (body, size) = skip(p);
        p = body + size;
    }
    out
}

/// Damages the structure that ends `s` - its end marker, end record, last
/// box, chunk or element - so that the end can no longer be confirmed.
fn damage_end(s: &Sample) -> Vec<u8> {
    let mut d = s.data.clone();
    let n = d.len();
    match s.kind {
        // EOI becomes FF 00, which is illegal outside a scan
        FileKind::Jpeg => d[n - 1] = 0x00,
        // IEND's CRC
        FileKind::Png => d[n - 1] ^= 0xFF,
        // the trailer
        FileKind::Gif => d[n - 1] = 0x00,
        // the end record's signature
        FileKind::Zip => {
            let at = rfind(&d, b"PK\x05\x06").unwrap();
            d[at + 3] = 0x07;
        }
        // the end header, covered by the start header's CRC; the end-of-
        // archive block, covered by its own header CRC
        FileKind::SevenZip | FileKind::Rar => d[n - 1] ^= 0x5A,
        // the last `%%EOF`
        FileKind::Pdf => {
            let at = rfind(&d, b"%%EOF").unwrap();
            d[at + 2] = b'X';
        }
        FileKind::Mp4 | FileKind::Heic | FileKind::Cr3 => {
            let (at, _, _) = *boxes(&d).last().unwrap();
            d[at + 4..at + 8].copy_from_slice(&[0; 4]);
        }
        FileKind::Avi | FileKind::Wav | FileKind::Webp => {
            let (at, _) = *riff_chunks(&d).last().unwrap();
            d[at..at + 4].copy_from_slice(&[0; 4]);
        }
        FileKind::Mkv => {
            let at = *segment_children(&d).last().unwrap();
            d[at] = 0x00;
        }
        FileKind::Bmp | FileKind::Tiff | FileKind::Mp3 => unreachable!("never verified"),
    }
    d
}

fn verified_samples() -> Vec<Sample> {
    let all: Vec<Sample> = samples()
        .into_iter()
        .filter(|s| run(s.kind, &s.data).unwrap().end_verified)
        .collect();
    // every kind that can verify its end has samples that do
    for &kind in FileKind::all() {
        let n = all.iter().filter(|s| s.kind == kind).count();
        match kind {
            FileKind::Bmp | FileKind::Tiff | FileKind::Mp3 | FileKind::Heic | FileKind::Cr3 => {
                assert_eq!(n, 0, "{kind:?}")
            }
            FileKind::Wav | FileKind::Webp => assert_eq!(n, 1, "{kind:?}"),
            _ => assert!(n >= 3, "{kind:?}: {n}"),
        }
    }
    all
}

#[test]
fn kinds_without_an_end_are_never_verified() {
    for s in samples() {
        let m = run(s.kind, &s.data).unwrap();
        if matches!(s.kind, FileKind::Bmp | FileKind::Tiff | FileKind::Mp3) {
            assert!(!m.end_verified, "{}", s.name);
        }
        // a verified end is not a statement about confidence and vice versa
        if s.kind == FileKind::Pdf {
            assert!(m.end_verified, "{}", s.name);
        }
    }
    for name in [
        "special_mkv_unknown_size.mkv",
        "special_mkv_live_stream.webm",
        "special_tiff_pillow_multipage_padded.tif",
    ] {
        let d = special(name);
        let m = run(sniff(&d).unwrap(), &d).unwrap();
        assert_eq!(m.confidence, Confidence::Probable, "{name}");
        assert!(!m.end_verified, "{name}");
    }
    // the first image of a multi-picture JPEG ends at its own EOI
    let mpo = special("special_jpeg_mpo.jpg");
    let m = run(FileKind::Jpeg, &mpo).unwrap();
    assert!(m.end_verified && (m.len as usize) < mpo.len());
    assert_eq!(&mpo[m.len as usize - 2..m.len as usize], &[0xFF, 0xD9]);
    // RAR 5 volume padding behind the end block is not part of the file
    let rar = special("special_rar_v5_volume_padded.rar");
    let m = run(FileKind::Rar, &rar).unwrap();
    assert!(m.end_verified && (m.len as usize) < rar.len());
}

#[test]
fn a_cut_file_never_has_a_verified_end() {
    // Cut anywhere: no file, or an end that is not verified - except a PDF
    // revision that is complete in itself (an incremental update cut off).
    for s in verified_samples() {
        let n = s.data.len();
        let mut cuts: Vec<usize> = vec![1, 2, n / 4, n / 2, n * 3 / 4, n - 2, n - 1];
        cuts.extend((512..n).step_by(512));
        for cut in cuts {
            let Some(m) = run(s.kind, &s.data[..cut]) else {
                continue;
            };
            if !m.end_verified {
                continue;
            }
            assert_eq!(s.kind, FileKind::Pdf, "{} cut at {cut}: {m:?}", s.name);
            let end = m.len as usize;
            let tail = &s.data[..end];
            let eof = rfind(tail, b"%%EOF").unwrap();
            assert!(
                matches!(&tail[eof + 5..], b"" | b"\n" | b"\r" | b"\r\n"),
                "{} cut at {cut}: {m:?}",
                s.name
            );
        }
    }
}

#[test]
fn a_damaged_final_structure_is_noticed() {
    for s in verified_samples() {
        let d = damage_end(&s);
        match run(s.kind, &d) {
            None => {}
            Some(m) if !m.end_verified => {}
            // a PDF falls back to an earlier, complete revision
            Some(m) if s.kind == FileKind::Pdf => {
                assert!((m.len as usize) < s.data.len(), "{}: {m:?}", s.name)
            }
            Some(m) => panic!("{}: {m:?} with its end damaged", s.name),
        }
    }
}

#[test]
fn foreign_data_inside_the_file_is_noticed() {
    // What a carver reads when the file was fragmented: its first clusters,
    // then a cluster of something else (here random bytes, or a cluster of
    // another sample), then - shifted - the rest.
    let mut rng = Rng::new(0x0E4D_0001);
    let others = samples();
    for s in verified_samples() {
        let n = s.data.len();
        for split in (512..n.saturating_sub(16)).step_by(1536) {
            for foreign in 0..2 {
                let filler = if foreign == 0 {
                    rng.bytes(4096)
                } else {
                    let o = &others[rng.below(others.len() as u64) as usize];
                    let mut f = o.data.clone();
                    f.resize(4096, 0);
                    f
                };
                let mut d = s.data[..split].to_vec();
                d.extend_from_slice(&filler);
                d.extend_from_slice(&s.data[split..]);
                let what = || format!("{}: 4096 foreign bytes at {split}", s.name);
                let Some(m) = run(s.kind, &d) else { continue };
                if !m.end_verified {
                    continue;
                }
                // A verified end must lie in the file's own first part (a
                // complete revision, the first image) - or the foreign data
                // landed in payload no walker reads (inside an `mdat`, a
                // `movi`, a Cluster, a stored ZIP entry, packed archive
                // data), and the end is the file's own, shifted - or, for
                // the chain formats, it landed in the payload of the last
                // structure, whose header was read before it (the
                // documented limit: that payload is not read).
                let len = m.len as usize;
                if s.kind == FileKind::Pdf && len > split && len != n + 4096 {
                    // The PDF check confirms where the cross-reference
                    // section of the revision is and that `%%EOF` ends it;
                    // what lies between the two (the trailer) is not
                    // checked. Foreign data there goes unnoticed - and only
                    // there.
                    let own = len - 4096;
                    let rev = &s.data[..own];
                    let at = rfind(rev, b"startxref").unwrap();
                    let digits: String = rev[at + 9..]
                        .iter()
                        .map(|&c| c as char)
                        .skip_while(|c| c.is_ascii_whitespace())
                        .take_while(char::is_ascii_digit)
                        .collect();
                    let xref: usize = digits.parse().unwrap();
                    assert!(xref < split && split < own, "{}: {m:?}", what());
                    continue;
                }
                let chain = matches!(
                    s.kind,
                    FileKind::Avi | FileKind::Wav | FileKind::Webp | FileKind::Mkv | FileKind::Mp4
                );
                assert!(
                    len <= split || len == n + 4096 || (chain && len == n),
                    "{}: {m:?}",
                    what()
                );
            }
        }
    }
}

#[test]
fn layouts_the_encoders_do_not_write() {
    let mp4 = samples()
        .into_iter()
        .find(|s| s.name == "mp4_ffmpeg_moov_last.mp4")
        .unwrap()
        .data;
    let b = boxes(&mp4);
    let get = |typ: &[u8; 4]| {
        let (at, size, _) = *b.iter().find(|(_, _, t)| t == typ).unwrap();
        mp4[at..at + size].to_vec()
    };
    let (ftyp, moov, mdat) = (get(b"ftyp"), get(b"moov"), get(b"mdat"));

    // CR3: the cameras write `moov` first and the image data last
    let mut cr3_ftyp = ftyp.clone();
    cr3_ftyp[8..12].copy_from_slice(b"crx ");
    let first = [&cr3_ftyp[..], &moov, &mdat].concat();
    let m = run(FileKind::Cr3, &first).unwrap();
    assert_eq!(
        (m.len as usize, m.confidence),
        (first.len(), Confidence::Exact)
    );
    assert!(!m.end_verified);
    let last = [&cr3_ftyp[..], &mdat, &moov].concat();
    assert!(run(FileKind::Cr3, &last).unwrap().end_verified);

    // HEIF: a `meta` behind the item data verifies the end; one in front
    // does not
    let heic = samples()
        .into_iter()
        .find(|s| s.name == "heic_libheif.heic")
        .unwrap()
        .data;
    let hb = boxes(&heic);
    let parts: Vec<Vec<u8>> = hb
        .iter()
        .map(|&(at, size, _)| heic[at..at + size].to_vec())
        .collect();
    let types: Vec<[u8; 4]> = hb.iter().map(|b| b.2).collect();
    assert_eq!(types, [*b"ftyp", *b"meta", *b"mdat"]);
    let swapped = [&parts[0][..], &parts[2], &parts[1]].concat();
    let m = run(FileKind::Heic, &swapped).unwrap();
    assert_eq!(m.len as usize, heic.len());
    assert!(m.end_verified);
    assert!(!run(FileKind::Heic, &heic).unwrap().end_verified);

    // a free box behind the media changes nothing either way
    let mut free = 16u32.to_be_bytes().to_vec();
    free.extend_from_slice(b"free");
    free.extend_from_slice(&[0; 8]);
    let padded = [&heic[..], &free].concat();
    let m = run(FileKind::Heic, &padded).unwrap();
    assert_eq!(m.len as usize, padded.len());
    assert!(!m.end_verified);
    let padded = [&swapped[..], &free].concat();
    assert!(run(FileKind::Heic, &padded).unwrap().end_verified);

    // fragmented MP4: an `mfra` whose `mfro` does not repeat its size is
    // not an end that proves anything
    let frag = samples()
        .into_iter()
        .find(|s| s.name == "mp4_ffmpeg_fragmented.mp4")
        .unwrap()
        .data;
    let (at, size, typ) = *boxes(&frag).last().unwrap();
    assert_eq!(&typ, b"mfra");
    assert_eq!(be32(&frag, at + size - 4) as usize, size);
    assert!(run(FileKind::Mp4, &frag).unwrap().end_verified);
    let mut wrong = frag.clone();
    let last = wrong.len() - 1;
    wrong[last] ^= 0x01;
    let m = run(FileKind::Mp4, &wrong).unwrap();
    assert_eq!(m.len as usize, frag.len());
    assert!(!m.end_verified);
    // without the `mfra` more fragments may have followed
    let m = run(FileKind::Mp4, &frag[..at]).unwrap();
    assert_eq!((m.len as usize, m.confidence), (at, Confidence::Probable));
    assert!(!m.end_verified);

    // AVI: without `idx1` the file ends in `movi`
    let avi = samples()
        .into_iter()
        .find(|s| s.name == "avi_ffmpeg_video_only.avi")
        .unwrap()
        .data;
    let chunks = riff_chunks(&avi);
    let (idx1, id) = *chunks.last().unwrap();
    assert_eq!(&id, b"idx1");
    let mut short = avi[..idx1].to_vec();
    let size = (short.len() - 8) as u32;
    short[4..8].copy_from_slice(&size.to_le_bytes());
    let m = run(FileKind::Avi, &short).unwrap();
    assert_eq!(
        (m.len as usize, m.confidence),
        (short.len(), Confidence::Exact)
    );
    assert!(!m.end_verified);
}
