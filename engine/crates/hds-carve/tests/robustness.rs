//! Arbitrary and hostile input: nothing may panic, hang, read without
//! bound, or report a length that the data does not cover.
//!
//! All randomness comes from a fixed-seed generator, so a failure is
//! reproducible.

mod common;

use common::{all_files, Counting, Endless, Rng, Sample};
use hds_carve::{
    measure, sniff, try_measure, Confidence, FileKind, Limits, MeasureError, ReadAt, SNIFF_LEN,
};

/// Runs `measure` on `data` and checks everything that must hold for any
/// input. Returns (bytes read, reads issued).
fn checked(kind: FileKind, data: &[u8], max_len: u64, what: &dyn Fn() -> String) -> (u64, u64) {
    let counted = Counting::new(data);
    let result = measure(kind, &counted, max_len);
    if let Some(m) = result {
        assert!(m.len > 0, "{}: empty file measured", what());
        assert!(
            m.len <= max_len,
            "{}: {} exceeds max_len {max_len}",
            what(),
            m.len
        );
        assert!(
            m.len <= data.len() as u64,
            "{}: {} bytes measured, {} exist",
            what(),
            m.len,
            data.len()
        );
        assert!(
            !m.ext.is_empty()
                && m.ext
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }
    assert!(
        counted.furthest.get() <= max_len,
        "{}: read beyond max_len",
        what()
    );
    (counted.bytes.get(), counted.reads.get())
}

/// One random mutation of `data`.
fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut d = data.to_vec();
    let len = d.len() as u64;
    // most damage where the headers are
    let at = |rng: &mut Rng| -> usize {
        match rng.below(4) {
            0 => rng.below(len.min(64)) as usize,
            1 => rng.below(len.min(1024)) as usize,
            _ => rng.below(len) as usize,
        }
    };
    const HUGE: [u64; 10] = [
        0,
        1,
        0x7F,
        0xFF,
        0xFFFF,
        0x7FFF_FFFF,
        0x8000_0000,
        0xFFFF_FFFF,
        0x7FFF_FFFF_FFFF_FFFF,
        0xFFFF_FFFF_FFFF_FFFF,
    ];
    match rng.below(12) {
        0 => {
            let p = at(rng);
            d[p] ^= 1 << rng.below(8);
        }
        1 => {
            for _ in 0..1 + rng.below(8) {
                let p = at(rng);
                d[p] ^= 1 << rng.below(8);
            }
        }
        2 => {
            let p = at(rng);
            d[p] = rng.next_u64() as u8;
        }
        3 => d.truncate(at(rng)),
        // a length field that points to the end of the world
        4..=6 => {
            let p = at(rng);
            let v = HUGE[rng.below(HUGE.len() as u64) as usize];
            let width = [1usize, 2, 4, 4, 4, 8][rng.below(6) as usize];
            let bytes = if rng.below(2) == 0 {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            };
            let src = if rng.below(2) == 0 {
                &bytes[..width]
            } else {
                &bytes[8 - width..]
            };
            for (dst, &b) in d.iter_mut().skip(p).zip(src) {
                *dst = b;
            }
        }
        7 => {
            // a run of one value
            let p = at(rng);
            let n = 1 + rng.below(300) as usize;
            let v = [0x00, 0xFF, rng.next_u64() as u8][rng.below(3) as usize];
            for b in d.iter_mut().skip(p).take(n) {
                *b = v;
            }
        }
        8 => {
            // bytes removed: everything behind shifts
            let p = at(rng);
            let n = (1 + rng.below(64) as usize).min(d.len() - p);
            d.drain(p..p + n);
        }
        9 => {
            // bytes inserted
            let p = at(rng);
            let n = 1 + rng.below(64) as usize;
            let extra = rng.bytes(n);
            d.splice(p..p, extra);
        }
        10 => {
            // the tail replaced by noise: a fragmented file
            let p = at(rng);
            rng.fill(&mut d[p..]);
        }
        _ => {
            // a piece of the file copied over another place
            let (a, b) = (at(rng), at(rng));
            let n = (1 + rng.below(512) as usize).min(d.len() - a.max(b));
            d.copy_within(a..a + n, b);
        }
    }
    d
}

#[test]
fn mutated_samples() {
    let files = all_files();
    let mut rng = Rng::new(0xB0B0_0001);
    let mut cases = 0u32;
    let mut still_measured = 0u32;
    for s in &files {
        for round in 0..120 {
            let m = mutate(&mut rng, &s.data);
            let what = || format!("{} mutation {round}", s.name);
            let sniffed = sniff(&m);
            // the kind the file had, the kind it now looks like, and one more
            let other = FileKind::all()[rng.below(FileKind::all().len() as u64) as usize];
            for kind in [Some(s.kind), sniffed, Some(other)].into_iter().flatten() {
                let max_len = if round % 3 == 0 {
                    u64::MAX
                } else {
                    m.len() as u64
                };
                let (bytes, reads) = checked(kind, &m, max_len, &what);
                // Work stays proportional to the file: every byte read at
                // most a few times, however the mutation bent the structure.
                assert!(
                    bytes <= 4 * m.len() as u64 + 65_536,
                    "{}: {bytes} bytes read of {}",
                    what(),
                    m.len()
                );
                assert!(reads <= 2_000, "{}: {reads} reads", what());
                cases += 1;
            }
            if measure(s.kind, &m, m.len() as u64).is_some() {
                still_measured += 1;
            }
        }
    }
    println!("{cases} cases, {still_measured} mutated files still measurable");
    assert!(cases > 30_000, "{cases}");
    // sanity of the test itself: mutations do break most files, and some
    // (in payload nobody checks) do not
    let total = files.len() as u32 * 120;
    assert!(
        still_measured > total / 20 && still_measured < total * 3 / 4,
        "{still_measured} of {total}"
    );
}

#[test]
fn every_single_bit_of_the_first_bytes() {
    // exhaustive over the first 48 bytes, where the fields live that steer
    // everything else
    let mut wrong = Vec::new();
    for s in all_files() {
        let original = measure(s.kind, &s.data, s.data.len() as u64);
        let n = s.data.len().min(48);
        for bit in 0..n * 8 {
            let mut m = s.data.clone();
            m[bit / 8] ^= 1 << (bit % 8);
            let what = || format!("{} bit {bit} flipped", s.name);
            for kind in [Some(s.kind), sniff(&m)].into_iter().flatten() {
                checked(kind, &m, m.len() as u64, &what);
            }
            // One flipped bit never yields a confident answer that differs
            // from the undamaged file's: the damage is either irrelevant to
            // the length, or noticed. One exception is inherent: a RIFF
            // size that shrinks by exactly the chunks behind the payload
            // (one bit can do that to `wav_python_list_after_data.wav`)
            // describes a complete, shorter file - whose end then rests on
            // header arithmetic alone and is not reported as verified.
            if let Some(r) = measure(s.kind, &m, m.len() as u64) {
                let shorter_unverified =
                    !r.end_verified && original.is_some_and(|o| o.end_verified && r.len < o.len);
                if r.confidence == Confidence::Exact
                    && Some(r.len) != original.map(|o| o.len)
                    && !shorter_unverified
                {
                    wrong.push(format!("{}: {r:?} instead of {original:?}", what()));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn hostile_length_fields() {
    // every 32-bit field position in the first 160 bytes, set to values a
    // walker might be tempted to follow
    let values: [u32; 6] = [
        0xFFFF_FFFF,
        0x7FFF_FFFF,
        0x8000_0000,
        0x0000_0001,
        0x0000_0000,
        0xFFFF_FFF0,
    ];
    for s in all_files() {
        let n = s.data.len().saturating_sub(4).min(160);
        for at in 0..n {
            for v in values {
                for bytes in [v.to_le_bytes(), v.to_be_bytes()] {
                    let mut m = s.data.clone();
                    m[at..at + 4].copy_from_slice(&bytes);
                    // followed by plenty of data to run into
                    m.resize(s.data.len() + 8192, 0x5A);
                    let what = || format!("{} with {v:#x} at {at}", s.name);
                    let (bytes_read, _) = checked(s.kind, &m, u64::MAX, &what);
                    assert!(
                        bytes_read <= 4 * m.len() as u64 + 65_536,
                        "{}: {bytes_read} bytes read",
                        what()
                    );
                }
            }
        }
    }
}

#[test]
fn random_buffers() {
    let mut rng = Rng::new(0xB0B0_0002);
    for round in 0..6_000 {
        let len = match round % 4 {
            0 => rng.below(64),
            1 => rng.below(600),
            _ => rng.below(5000),
        } as usize;
        let data = rng.bytes(len);
        let what = || format!("random buffer {round} ({len} bytes)");
        assert_eq!(sniff(&data), None, "{}", what());
        for &kind in FileKind::all() {
            let counted = Counting::new(&data[..]);
            assert_eq!(
                measure(kind, &counted, u64::MAX),
                None,
                "{} as {kind:?}",
                what()
            );
        }
    }
}

/// Formats whose last structure is opaque payload of a length stated up
/// front (pixels, samples, one compressed image, an `mdat` behind its
/// `moov`): nothing behind the headers can contradict the headers.
fn length_from_header_alone(s: &Sample) -> bool {
    match s.kind {
        FileKind::Bmp | FileKind::Heic => true,
        // the samples with a chunk behind the payload are the exceptions
        FileKind::Wav => s.name != "wav_python_list_after_data.wav",
        FileKind::Webp => s.name != "webp_exif.webp",
        _ => s.name == "mp4_ffmpeg_faststart.mp4" || s.name == "mkv_mkvmerge_no_cues.mkv",
    }
}

#[test]
fn real_heads_with_random_bodies() {
    // The first sectors of a real file survive, the rest was overwritten:
    // the common fate of a fragmented or partly reused file.
    let mut rng = Rng::new(0xB0B0_0003);
    let mut unnoticed = 0;
    for s in all_files() {
        for keep in [
            16usize,
            64,
            512,
            4096,
            s.data.len() / 2,
            s.data.len() * 9 / 10,
        ] {
            let keep = keep.min(s.data.len());
            for tail in [512usize, 20_000] {
                let mut m = s.data[..keep].to_vec();
                m.extend(rng.bytes(tail));
                let what = || format!("{}: {keep} real bytes + {tail} random", s.name);
                checked(s.kind, &m, u64::MAX, &what);
                let Some(r) = measure(s.kind, &m, m.len() as u64) else {
                    continue;
                };
                // A verified end is never one the noise provided: it is the
                // end of a complete part, or the file's own end, found
                // through structures that lie before the noise (the payload
                // of the last structure of a chain format is not read).
                if r.end_verified {
                    assert!(
                        r.len <= keep as u64 || r.len == s.data.len() as u64,
                        "{}: {r:?}",
                        what()
                    );
                    if r.len > keep as u64 {
                        assert!(
                            matches!(
                                s.kind,
                                FileKind::Avi
                                    | FileKind::Wav
                                    | FileKind::Webp
                                    | FileKind::Mkv
                                    | FileKind::Mp4
                                    | FileKind::Heic
                                    | FileKind::Cr3
                            ),
                            "{}: {r:?}",
                            what()
                        );
                    }
                }
                if keep == s.data.len()
                    || r.len <= keep as u64
                    || r.confidence == Confidence::Probable
                {
                    // complete, or a complete part of it (the first image of
                    // a multi-picture JPEG), or flagged as uncertain
                    continue;
                }
                // Reported as certain although the tail is noise. This is
                // the known blind spot of the formats that end in payload
                // sized by a header - and of no other.
                // Or, with a verified end, the noise fell into the payload
                // of the last structure of a chain (checked above).
                assert!(
                    length_from_header_alone(&s) != r.end_verified,
                    "{}: {r:?}",
                    what()
                );
                assert_eq!(r.len, s.data.len() as u64, "{}", what());
                unnoticed += 1;
            }
        }
    }
    assert!(unnoticed > 0);
}

/// Endless inputs: a real header, then one unit repeated for ever.
fn endless_inputs() -> Vec<(&'static str, FileKind, Endless)> {
    let files = all_files();
    let get = |name: &str| -> &Sample {
        files
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name}"))
    };
    let find = |d: &[u8], n: &[u8]| d.windows(n.len()).position(|w| w == n).unwrap();
    let mut v = Vec::new();

    // JPEG: headers, then entropy data that never ends
    let jpeg = &get("jpeg_baseline.jpg").data;
    let sos = find(jpeg, &[0xFF, 0xDA]);
    v.push((
        "jpeg: scan without end",
        FileKind::Jpeg,
        Endless {
            prefix: jpeg[..sos + 14].to_vec(),
            unit: vec![0x5A],
        },
    ));
    v.push((
        "jpeg: stuffed FF for ever",
        FileKind::Jpeg,
        Endless {
            prefix: jpeg[..sos + 14].to_vec(),
            unit: vec![0xFF, 0x00],
        },
    ));
    v.push((
        "jpeg: fill bytes for ever",
        FileKind::Jpeg,
        Endless {
            prefix: jpeg[..sos].to_vec(),
            unit: vec![0xFF],
        },
    ));
    let app = [0xFF, 0xE5, 0xFF, 0xFF];
    v.push((
        "jpeg: APP segments for ever",
        FileKind::Jpeg,
        Endless {
            prefix: jpeg[..2].to_vec(),
            unit: [&app[..], &[0u8; 65533]].concat(),
        },
    ));
    // PNG: one chunk of 2 GiB
    let png = &get("png_rgb.png").data;
    let idat = find(png, b"IDAT");
    let mut prefix = png[..idat - 4].to_vec();
    prefix.extend(0x7FFF_FFFFu32.to_be_bytes());
    prefix.extend(b"IDAT");
    v.push((
        "png: huge chunk",
        FileKind::Png,
        Endless {
            prefix,
            unit: vec![0x33],
        },
    ));
    // GIF: data sub-blocks without a terminator
    let gif = &get("gif_static.gif").data;
    let image = find(gif, &[0x2C, 0, 0, 0, 0]);
    v.push((
        "gif: sub-blocks for ever",
        FileKind::Gif,
        Endless {
            prefix: gif[..image + 11].to_vec(),
            unit: vec![0xFF],
        },
    ));
    let comment = [&[0x21u8, 0xFE, 0x01, 0x00][..], &[0x00]].concat();
    v.push((
        "gif: extensions for ever",
        FileKind::Gif,
        Endless {
            prefix: gif[..image].to_vec(),
            unit: comment,
        },
    ));
    // PDF: never a %%EOF
    let pdf = &get("pdf_reportlab.pdf").data;
    v.push((
        "pdf: no trailer",
        FileKind::Pdf,
        Endless {
            prefix: pdf[..600].to_vec(),
            unit: b"1 0 obj\n<< >>\nendobj\n".to_vec(),
        },
    ));
    v.push((
        "pdf: silence",
        FileKind::Pdf,
        Endless {
            prefix: pdf[..600].to_vec(),
            unit: vec![0],
        },
    ));
    // MP3: the same frame for ever
    let mp3 = &get("mp3_ffmpeg_no_xing.mp3").data;
    let frame = find(mp3, &[0xFF, 0xFB]);
    let frame_len = 417.min(mp3.len() - frame);
    let next = frame
        + (frame_len - 10..frame_len + 10)
            .find(|&n| mp3[frame + n] == 0xFF && mp3[frame + n + 1] == 0xFB)
            .unwrap();
    v.push((
        "mp3: frames for ever",
        FileKind::Mp3,
        Endless {
            prefix: mp3[..frame].to_vec(),
            unit: mp3[frame..next].to_vec(),
        },
    ));
    // ZIP: a streamed entry whose descriptor never comes
    let zip = &get("zip_python_stream_descriptors.zip").data;
    v.push((
        "zip: no descriptor",
        FileKind::Zip,
        Endless {
            prefix: zip[..60].to_vec(),
            unit: b"PK\x07\x08junk".to_vec(),
        },
    ));
    // ZIP: local headers of empty entries for ever
    let mut entry = b"PK\x03\x04\x14\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x01\0\0\0".to_vec();
    entry.push(b'a');
    v.push((
        "zip: entries for ever",
        FileKind::Zip,
        Endless {
            prefix: Vec::new(),
            unit: entry,
        },
    ));
    // MP4: fragments for ever
    let mp4 = &get("mp4_ffmpeg_fragmented.mp4").data;
    let moof = find(mp4, b"moof") - 4;
    let second = moof + 8 + find(&mp4[moof + 8..], b"moof") - 4;
    v.push((
        "mp4: fragments for ever",
        FileKind::Mp4,
        Endless {
            prefix: mp4[..moof].to_vec(),
            unit: mp4[moof..second].to_vec(),
        },
    ));
    let free = [0, 0, 0, 8, b'f', b'r', b'e', b'e'];
    v.push((
        "mp4: free boxes for ever",
        FileKind::Mp4,
        Endless {
            prefix: mp4[..moof].to_vec(),
            unit: free.to_vec(),
        },
    ));
    // far-apart boxes: every header is a seek
    let far = [
        &[0u8, 0x10, 0, 0, b'f', b'r', b'e', b'e'][..],
        &vec![0u8; 0x10_0000 - 8],
    ]
    .concat();
    v.push((
        "mp4: megabyte hops for ever",
        FileKind::Mp4,
        Endless {
            prefix: mp4[..moof].to_vec(),
            unit: far,
        },
    ));
    // AVI: continuation chunks for ever
    let avi = &get("avi_ffmpeg_video_only.avi").data;
    let avix = b"RIFF\x0c\0\0\0AVIXJUNK\0\0\0\0".to_vec();
    v.push((
        "avi: AVIX for ever",
        FileKind::Avi,
        Endless {
            prefix: avi.clone(),
            unit: avix,
        },
    ));
    // Matroska: unknown segment size, clusters for ever
    let mkv = common::special("special_mkv_unknown_size.mkv");
    let cluster = find(&mkv, &[0x1F, 0x43, 0xB6, 0x75]);
    let unit = [
        &[0x1F, 0x43, 0xB6, 0x75, 0x84][..],
        &[0xE7, 0x81, 0x00, 0xEC],
    ]
    .concat();
    v.push((
        "mkv: clusters for ever",
        FileKind::Mkv,
        Endless {
            prefix: mkv[..cluster].to_vec(),
            unit,
        },
    ));
    // RAR 4: the first file block for ever
    let rar = &get("rar_v4_rar420.rar").data;
    let size = u16::from_le_bytes([rar[25], rar[26]]) as usize;
    let pack = u32::from_le_bytes(rar[27..31].try_into().unwrap()) as usize;
    v.push((
        "rar: file blocks for ever",
        FileKind::Rar,
        Endless {
            prefix: rar[..20].to_vec(),
            unit: rar[20..20 + size + pack].to_vec(),
        },
    ));
    // everything: zeros and FF for ever behind a real head
    for s in &files {
        if s.name.starts_with("special") {
            continue;
        }
        let keep = s.data.len().min(256);
        let name: &'static str = Box::leak(format!("{}: head + zeros", s.name).into_boxed_str());
        v.push((
            name,
            s.kind,
            Endless {
                prefix: s.data[..keep].to_vec(),
                unit: vec![0],
            },
        ));
    }
    v
}

#[test]
fn endless_inputs_stop_at_the_limits() {
    let limits = Limits {
        max_read_bytes: 6 << 20,
        max_reads: 3_000,
    };
    for (what, kind, input) in endless_inputs() {
        let counted = Counting::new(&input);
        let result = try_measure(kind, &counted, u64::MAX, &limits);
        assert!(
            counted.bytes.get() <= limits.max_read_bytes,
            "{what}: {} bytes read",
            counted.bytes.get()
        );
        assert!(
            counted.reads.get() <= limits.max_reads,
            "{what}: {} reads",
            counted.reads.get()
        );
        // Never a file: either the structure breaks or the budget does, and
        // the caller is told which. (A real head whose header alone gives
        // the length is a file even with zeros for a body.)
        match result {
            Ok(m) => assert!(
                what.ends_with("head + zeros") && m.len < 100_000,
                "{what}: measured {m:?}"
            ),
            Err(MeasureError::NotMeasurable) => {}
            Err(MeasureError::LimitReached) => {
                let spent = counted.bytes.get() + 2 * (1 << 20) > limits.max_read_bytes;
                assert!(
                    spent || counted.reads.get() == limits.max_reads,
                    "{what}: limit reported too early"
                );
            }
            Err(MeasureError::Io(e)) => panic!("{what}: {e}"),
        }
    }
}

#[test]
fn structure_count_caps_are_not_ends() {
    // Chains of tiny, valid structures that never end: the walker's own cap
    // on their number is reached long before the read budget is. Reaching
    // it must not be mistaken for the end of the file.
    for (what, kind, input) in endless_inputs() {
        if what == "avi: AVIX for ever" || what == "mp4: free boxes for ever" {
            let counted = Counting::new(&input);
            assert_eq!(measure(kind, &counted, u64::MAX), None, "{what}");
            assert!(
                counted.bytes.get() < 64 << 20,
                "{what}: {} bytes read",
                counted.bytes.get()
            );
        }
    }
}

#[test]
fn a_budget_that_runs_out_is_not_an_end() {
    // Real files, read with budgets too small to finish: the answer must be
    // None, never a shorter file.
    for s in all_files() {
        let full = measure(s.kind, &s.data, s.data.len() as u64);
        // either the budget was enough for the full answer, or the caller
        // learns that it was not - never "invalid", never a shorter file
        let check =
            |limits: Limits| match try_measure(s.kind, &s.data, s.data.len() as u64, &limits) {
                Ok(m) => assert_eq!(Some(m), full, "{} with {limits:?}", s.name),
                Err(MeasureError::LimitReached) => {}
                Err(MeasureError::NotMeasurable) => {
                    assert_eq!(full, None, "{} with {limits:?}", s.name)
                }
                Err(MeasureError::Io(e)) => panic!("{}: {e}", s.name),
            };
        for max_reads in [0u64, 1, 2, 3, 5, 8] {
            check(Limits {
                max_read_bytes: u64::MAX,
                max_reads,
            });
        }
        for max_read_bytes in [
            0u64,
            100,
            4096,
            s.data.len() as u64 / 2,
            s.data.len() as u64 - 1,
        ] {
            check(Limits {
                max_read_bytes,
                max_reads: u64::MAX,
            });
        }
    }
}

#[test]
fn read_errors_are_not_ends() {
    /// Readable up to `bad`, an I/O error from there on.
    struct Failing<'a> {
        data: &'a [u8],
        bad: u64,
    }
    impl ReadAt for Failing<'_> {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
            if offset + buf.len() as u64 > self.bad {
                return Err(std::io::Error::other("unreadable sector"));
            }
            self.data.read_at(offset, buf)
        }
    }
    let mut rng = Rng::new(0xB0B0_0004);
    for s in all_files() {
        let len = s.data.len() as u64;
        let full = measure(s.kind, &s.data, len);
        // behind the file there are 64 KiB more, then the bad area begins
        // somewhere: inside the file, or behind it
        let mut padded = s.data.clone();
        padded.resize(s.data.len() + 65_536, 0);
        for _ in 0..12 {
            let bad = rng.below(padded.len() as u64);
            let reader = Failing { data: &padded, bad };
            match try_measure(s.kind, &reader, padded.len() as u64, &Limits::DEFAULT) {
                // whatever is measured lies entirely in the readable part,
                // and is what a healthy disk would have given
                Ok(m) => {
                    assert!(
                        m.len <= bad,
                        "{}: {} bytes measured, unreadable from {bad}",
                        s.name,
                        m.len
                    );
                    assert_eq!(Some(m), full, "{} with bad area at {bad}", s.name);
                }
                // otherwise the error is reported as what it is
                Err(MeasureError::Io(e)) => assert_eq!(e.to_string(), "unreadable sector"),
                Err(MeasureError::NotMeasurable) => {
                    assert_eq!(full, None, "{} with bad area at {bad}", s.name)
                }
                Err(MeasureError::LimitReached) => panic!("{}: no limit was near", s.name),
            }
        }
    }
}

#[test]
fn sniff_accepts_any_slice() {
    let mut rng = Rng::new(0xB0B0_0005);
    for s in all_files() {
        // every prefix length, and every start offset in the first sector
        for n in 0..=s.data.len().min(2 * SNIFF_LEN) {
            sniff(&s.data[..n]);
        }
        for at in 0..s.data.len().min(600) {
            sniff(&s.data[at..]);
        }
        // heads with one random byte
        for _ in 0..200 {
            let mut h = s.data[..s.data.len().min(SNIFF_LEN)].to_vec();
            let p = rng.below(h.len() as u64) as usize;
            h[p] = rng.next_u64() as u8;
            sniff(&h);
        }
    }
    for len in 0..300 {
        for fill in [0x00u8, 0xFF, 0x80, 0x7F] {
            assert_eq!(sniff(&vec![fill; len]), None);
        }
    }
}

/// The same endless inputs against the default limits: how long a hostile
/// candidate can keep `measure` busy. Reads up to 1 GiB per input.
#[test]
#[ignore = "reads gigabytes of synthetic data; run with --release -- --ignored --nocapture"]
fn endless_inputs_with_default_limits() {
    for (what, kind, input) in endless_inputs() {
        if what.ends_with("head + zeros") {
            continue;
        }
        let counted = Counting::new(&input);
        let started = std::time::Instant::now();
        let result = measure(kind, &counted, u64::MAX);
        println!(
            "{what:<32} {:>10} bytes in {:>6} reads, {:>6.2} s -> {result:?}",
            counted.bytes.get(),
            counted.reads.get(),
            started.elapsed().as_secs_f64()
        );
        assert!(counted.bytes.get() <= Limits::DEFAULT.max_read_bytes);
        assert!(counted.reads.get() <= Limits::DEFAULT.max_reads);
        assert_eq!(result, None, "{what}");
    }
}
