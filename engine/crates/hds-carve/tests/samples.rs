//! Real files written by real encoders (see `tests/data/generate.sh`):
//! `sniff` must recognise each, and `measure` must return exactly its length
//! whatever follows it on the disk.

mod common;

use common::{samples, special, Rng, Sample};
use hds_carve::{
    kinds_for_extension, matches_extension, measure, sniff, Confidence, FileKind, Measured,
    SNIFF_LEN,
};

fn head(data: &[u8]) -> &[u8] {
    &data[..data.len().min(SNIFF_LEN)]
}

/// The confidence a sample must be measured with.
fn expected_confidence(s: &Sample) -> Confidence {
    match s.kind {
        // no end marker, no length field
        FileKind::Tiff | FileKind::Mp3 => Confidence::Probable,
        // only a linearized file that was not updated afterwards states its length
        FileKind::Pdf if s.name.contains("linearized") && !s.name.contains("updated") => {
            Confidence::Exact
        }
        FileKind::Pdf => Confidence::Probable,
        // run-length encoded pixels: two header fields, nothing to walk
        FileKind::Bmp if s.name.contains("rle") => Confidence::Probable,
        _ => Confidence::Exact,
    }
}

/// Whether the end must be verified by structure (see "Verified ends" in
/// the crate documentation), from what each encoder is known to write.
fn expected_end_verified(s: &Sample) -> bool {
    match s.kind {
        // header arithmetic, the furthest pointer, a stream complete after
        // any frame
        FileKind::Bmp | FileKind::Tiff | FileKind::Mp3 => false,
        // libheif and Pillow write `meta` first and the item data last
        FileKind::Heic => false,
        // arithmetic coding is not decoded, so its scans cannot be checked
        FileKind::Jpeg => s.name != "jpeg_arithmetic.jpg",
        // only one WAV sample has a chunk behind its `data` chunk
        FileKind::Wav => s.name == "wav_python_list_after_data.wav",
        // without Cues and Tags mkvmerge ends the file in its last Cluster
        FileKind::Mkv => s.name != "mkv_mkvmerge_no_cues.mkv",
        // only the Exif chunk comes behind the image data
        FileKind::Webp => s.name == "webp_exif.webp",
        // FFmpeg puts `moov` last unless told otherwise; the fragmented
        // sample ends in `mfra`
        FileKind::Mp4 => s.name != "mp4_ffmpeg_faststart.mp4",
        // end markers, end records, `idx1` and `Cues` behind the payload
        _ => true,
    }
}

/// The extension `measure` must report.
fn expected_ext(s: &Sample) -> &str {
    match s.name.as_str() {
        // a TIFF with sensor data and a Nikon Make tag is a NEF
        "tiff_rawlike_nikon.tif" => "nef",
        _ => &s.ext,
    }
}

fn check(s: &Sample, what: &str, data: &[u8]) {
    let expected = Measured {
        len: s.data.len() as u64,
        confidence: expected_confidence(s),
        ext: leak(expected_ext(s)),
        end_verified: expected_end_verified(s),
    };
    let got = measure(s.kind, &data, data.len() as u64);
    assert_eq!(got, Some(expected), "{} {what}", s.name);
}

/// Sample extensions are compared against `&'static str` fields.
fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

#[test]
fn every_kind_has_real_samples() {
    let all = samples();
    for &kind in FileKind::all() {
        let n = all.iter().filter(|s| s.kind == kind).count();
        match kind {
            // no encoder for Canon CR3 exists outside the cameras; the kind
            // is exercised with a synthetic box structure in synthetic.rs
            FileKind::Cr3 => assert_eq!(n, 0),
            _ => assert!(n >= 3, "{kind:?}: only {n} samples"),
        }
    }
    assert!(all.len() >= 120, "{} samples", all.len());
    let total: usize = all.iter().map(|s| s.data.len()).sum();
    assert!(total < 3 * 512 * 1024, "samples grew to {total} bytes");
}

#[test]
fn sniff_recognises_every_sample() {
    for s in samples() {
        assert_eq!(sniff(head(&s.data)), Some(s.kind), "{}", s.name);
        // more than SNIFF_LEN bytes change nothing
        assert_eq!(sniff(&s.data), Some(s.kind), "{} (whole file)", s.name);
        // the kind's name is what the sample file is called after
        assert!(s.name.starts_with(s.kind.name()));
    }
}

#[test]
fn sniff_with_short_heads_is_conservative() {
    for s in samples() {
        for n in 0..SNIFF_LEN.min(s.data.len()) {
            let got = sniff(&s.data[..n]);
            assert!(
                got.is_none() || got == Some(s.kind),
                "{} with {n} bytes: {got:?}",
                s.name
            );
        }
        assert_eq!(sniff(&s.data[..7.min(s.data.len())]), None, "{}", s.name);
    }
}

#[test]
fn measure_file_alone() {
    for s in samples() {
        // Alone, nothing follows the file. Walkers that confirm an end by
        // looking at what follows it (a PDF update, a further box or RIFF
        // chunk) find nothing there and do not call the end verified
        // (`end_verified.rs`); everything else is as on a disk.
        let got = measure(s.kind, &s.data, s.data.len() as u64);
        let looks_behind = matches!(s.kind, FileKind::Pdf | FileKind::Mp4 | FileKind::Avi);
        let expected = Measured {
            len: s.data.len() as u64,
            confidence: expected_confidence(&s),
            ext: leak(expected_ext(&s)),
            end_verified: expected_end_verified(&s) && !looks_behind,
        };
        assert_eq!(got, Some(expected), "{} alone", s.name);
        // a generous max_len changes nothing: the data simply ends
        let got = measure(s.kind, &s.data, u64::MAX).map(|m| m.len);
        assert_eq!(
            got,
            Some(s.data.len() as u64),
            "{} with max_len = MAX",
            s.name
        );
    }
}

#[test]
fn measure_followed_by_zeros() {
    for s in samples() {
        for pad in [1usize, 2, 7, 512, 4096, 70_000] {
            let mut data = s.data.clone();
            data.resize(data.len() + pad, 0);
            check(&s, &format!("+ {pad} zero bytes"), &data);
        }
        // as on a disk: zeros up to the next cluster, then another file
        let mut data = s.data.clone();
        data.resize(data.len().next_multiple_of(4096), 0);
        data.extend_from_slice(&s.data);
        check(&s, "+ cluster slack + itself", &data);
    }
}

#[test]
fn measure_followed_by_random_bytes() {
    let mut rng = Rng::new(0x5EED_0001);
    for s in samples() {
        for round in 0..24 {
            let mut data = s.data.clone();
            let n = if round % 4 == 0 { 70_000 } else { 600 };
            data.extend(rng.bytes(n));
            check(&s, &format!("+ random bytes (round {round})"), &data);
        }
    }
}

#[test]
fn measure_followed_by_ff_bytes() {
    // erased flash and some formatters fill with FF
    for s in samples() {
        let mut data = s.data.clone();
        data.resize(data.len() + 5000, 0xFF);
        check(&s, "+ FF bytes", &data);
    }
}

#[test]
fn measure_followed_by_a_file_of_the_same_kind() {
    let all = samples();
    for a in &all {
        for b in all.iter().filter(|b| b.kind == a.kind) {
            let mut data = a.data.clone();
            data.extend_from_slice(&b.data);
            check(a, &format!("+ {}", b.name), &data);
        }
    }
}

#[test]
fn measure_followed_by_a_file_of_another_kind() {
    let all = samples();
    // one representative of every kind is enough to stand behind each sample
    let mut firsts: Vec<&Sample> = Vec::new();
    for s in &all {
        if !firsts.iter().any(|f| f.kind == s.kind) {
            firsts.push(s);
        }
    }
    for a in &all {
        for b in &firsts {
            let mut data = a.data.clone();
            data.extend_from_slice(&b.data);
            check(a, &format!("+ {}", b.name), &data);
        }
    }
}

#[test]
fn measure_rejects_other_kinds() {
    for s in samples() {
        for &kind in FileKind::all() {
            if kind != s.kind {
                assert_eq!(
                    measure(kind, &s.data, s.data.len() as u64),
                    None,
                    "{} as {kind:?}",
                    s.name
                );
            }
        }
    }
}

#[test]
fn truncation_never_measures_beyond_the_data() {
    let mut rng = Rng::new(0x5EED_0002);
    for s in samples() {
        let len = s.data.len();
        let mut cuts = vec![
            0,
            1,
            2,
            7,
            8,
            16,
            31,
            64,
            100,
            len / 4,
            len / 2,
            len - len / 4,
            len - 1,
            len - 2,
            len - 17,
        ];
        cuts.extend([
            len.saturating_sub(100),
            len.saturating_sub(1000),
            len.saturating_sub(4096),
        ]);
        cuts.extend((0..24).map(|_| rng.below(len as u64) as usize));
        for cut in cuts {
            if cut >= len {
                continue;
            }
            let data = &s.data[..cut];
            // once with max_len at the cut, once with the data simply ending
            for max_len in [cut as u64, len as u64, u64::MAX] {
                match measure(s.kind, &data, max_len) {
                    None => {}
                    Some(m) => {
                        assert!(
                            m.len <= cut as u64,
                            "{} cut at {cut}: measured {} bytes",
                            s.name,
                            m.len
                        );
                        assert_eq!(
                            m.confidence,
                            Confidence::Probable,
                            "{} cut at {cut}: an incomplete file measured as exact ({} bytes)",
                            s.name,
                            m.len
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn max_len_is_respected() {
    for s in samples() {
        let len = s.data.len() as u64;
        // the whole file is there, but the caller says it cannot be that long
        for max_len in [0, 1, len / 2, len - 1] {
            if let Some(m) = measure(s.kind, &s.data, max_len) {
                assert!(
                    m.len <= max_len,
                    "{}: {} > max_len {max_len}",
                    s.name,
                    m.len
                );
                assert_eq!(
                    m.confidence,
                    Confidence::Probable,
                    "{} with max_len {max_len}",
                    s.name
                );
            }
        }
        assert_eq!(
            measure(s.kind, &s.data, len).map(|m| m.len),
            Some(len),
            "{}",
            s.name
        );
    }
}

#[test]
fn extensions_match_their_content() {
    let all = samples();
    for s in &all {
        let h = head(&s.data);
        assert_eq!(
            matches_extension(h, &s.ext),
            Some(true),
            "{} as .{}",
            s.name,
            s.ext
        );
        assert_eq!(
            matches_extension(h, &s.ext.to_uppercase()),
            Some(true),
            "{} upper case",
            s.name
        );
        assert_eq!(
            matches_extension(h, s.kind.default_ext()),
            Some(true),
            "{}",
            s.name
        );
        assert!(
            kinds_for_extension(&s.ext).contains(&s.kind),
            "{}: .{} does not list {:?}",
            s.name,
            s.ext,
            s.kind
        );
        assert_eq!(matches_extension(h, "txt"), None);

        // the content of one family does not pass for another
        let family = |k: FileKind| match k {
            FileKind::Heic | FileKind::Cr3 => FileKind::Mp4,
            other => other,
        };
        for &other in FileKind::all() {
            if family(other) == family(s.kind) {
                continue;
            }
            // MPEG audio in a RIFF/WAVE wrapper is a legitimate .mp3
            let expected =
                Some(other == FileKind::Mp3 && s.kind == FileKind::Wav && h.starts_with(b"RIFF"));
            assert_eq!(
                matches_extension(h, other.default_ext()),
                expected,
                "{} as .{}",
                s.name,
                other.default_ext()
            );
        }
    }
}

#[test]
fn aliases_and_containers() {
    let all = samples();
    let find = |name: &str| {
        all.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
    };
    let cases: &[(&str, &[&str])] = &[
        (
            "jpeg_exif_thumbnail.jpg",
            &["jpg", "jpeg", "jpe", "jfif", "thm", "mpo", "JPG"],
        ),
        (
            "tiff_pidng_dng.dng",
            &["dng", "tif", "tiff", "nef", "cr2", "arw", "pef", "srw"],
        ),
        (
            "tiff_libtiff_tiled_bigendian.tif",
            &["tif", "tiff", "nef", "dng"],
        ),
        (
            "zip_ooxml_word.docx",
            &[
                "docx", "docm", "zip", "xlsx", "pptx", "jar", "apk", "epub", "odt",
            ],
        ),
        ("zip_epub.epub", &["epub", "zip"]),
        (
            "mp4_ffmpeg_moov_last.mp4",
            &[
                "mp4", "m4v", "mov", "m4a", "3gp", "3g2", "qt", "lrv", "heic",
            ],
        ),
        ("mp4_ffmpeg_quicktime.mov", &["mov", "qt", "mp4"]),
        ("heic_libheif.heic", &["heic", "heif", "hif", "avif"]),
        ("wav_rf64.wav", &["wav", "wave", "bwf", "rf64"]),
        ("mkv_ffmpeg_vp8_opus.webm", &["webm", "mkv", "mka", "mk3d"]),
        ("avi_ffmpeg_mpeg4_mp3.avi", &["avi", "divx"]),
    ];
    for (name, exts) in cases {
        let s = find(name);
        for ext in *exts {
            assert_eq!(
                matches_extension(head(&s.data), ext),
                Some(true),
                "{name} as .{ext}"
            );
        }
    }
}

// ------------------------------------------------- samples with a twist --

#[test]
fn multi_picture_jpeg_measures_the_first_image() {
    let data = special("special_jpeg_mpo.jpg");
    assert_eq!(sniff(head(&data)), Some(FileKind::Jpeg));
    let m = measure(FileKind::Jpeg, &data, data.len() as u64).unwrap();
    let end = m.len as usize;
    assert_eq!(m.confidence, Confidence::Exact);
    assert!(end < data.len(), "the second image is trailing data");
    assert_eq!(&data[end - 2..end], [0xFF, 0xD9], "ends with EOI");
    // what follows is the second picture, a JPEG of its own
    let rest = &data[end..];
    let second = rest.iter().position(|&b| b != 0).unwrap();
    assert_eq!(&rest[second..second + 3], [0xFF, 0xD8, 0xFF]);
    assert_eq!(matches_extension(head(&data), "mpo"), Some(true));
}

#[test]
fn padding_after_the_last_structure_is_not_measured() {
    // Pillow pads a multi-frame TIFF to a multiple of 16 bytes
    let data = special("special_tiff_pillow_multipage_padded.tif");
    assert_eq!(sniff(head(&data)), Some(FileKind::Tiff));
    let m = measure(FileKind::Tiff, &data, data.len() as u64).unwrap();
    assert_eq!(m.confidence, Confidence::Probable);
    assert!(m.len < data.len() as u64 && data.len() as u64 - m.len < 16);
    assert!(data[m.len as usize..].iter().all(|&b| b == 0));

    // RAR 5 fills a volume with zeros behind its end-of-archive block
    let data = special("special_rar_v5_volume_padded.rar");
    assert_eq!(sniff(head(&data)), Some(FileKind::Rar));
    let m = measure(FileKind::Rar, &data, data.len() as u64).unwrap();
    assert_eq!(m.confidence, Confidence::Exact);
    assert!(m.len < data.len() as u64);
    assert!(data[m.len as usize..].iter().all(|&b| b == 0));
}

#[test]
fn matroska_without_segment_size_is_probable() {
    let mut rng = Rng::new(0x5EED_0003);
    for name in [
        "special_mkv_unknown_size.mkv",
        "special_mkv_live_stream.webm",
    ] {
        let data = special(name);
        let len = data.len() as u64;
        assert_eq!(sniff(head(&data)), Some(FileKind::Mkv), "{name}");
        let ext = if name.ends_with("webm") {
            "webm"
        } else {
            "mkv"
        };
        let want = Some(Measured {
            len,
            confidence: Confidence::Probable,
            ext,
            end_verified: false,
        });
        assert_eq!(measure(FileKind::Mkv, &data, len), want, "{name}");

        for tail in [vec![0u8; 5000], rng.bytes(5000), data.clone()] {
            let mut d = data.clone();
            d.extend(tail);
            assert_eq!(
                measure(FileKind::Mkv, &d, d.len() as u64),
                want,
                "{name} + tail"
            );
        }
        // cut inside the last cluster: the complete clusters remain
        let cut = &data[..data.len() - 10];
        match measure(FileKind::Mkv, &cut, len) {
            None => {}
            Some(m) => assert!(
                m.len < cut.len() as u64 && m.confidence == Confidence::Probable,
                "{name}"
            ),
        }
    }
    // the first file really has several clusters to fall back on
    let data = special("special_mkv_unknown_size.mkv");
    let cut = &data[..data.len() - 10];
    let m = measure(FileKind::Mkv, &cut, u64::MAX).expect("earlier clusters are complete");
    assert!(m.len > 4000 && m.len < cut.len() as u64);
}

#[test]
fn bare_mp3_frames_are_not_a_signature() {
    let data = special("special_mp3_bare_frames.mp3");
    // 11 sync bits are no signature: not offered as a carving candidate
    assert_eq!(sniff(head(&data)), None);
    // but a file known to be an MP3 checks out, and can be measured
    assert_eq!(matches_extension(head(&data), "mp3"), Some(true));
    let m = measure(FileKind::Mp3, &data, data.len() as u64).unwrap();
    assert_eq!(
        (m.len, m.confidence, m.ext),
        (data.len() as u64, Confidence::Probable, "mp3")
    );
}

#[test]
fn archives_with_encrypted_headers_cannot_be_measured() {
    for name in [
        "special_rar_v4_encrypted_headers.rar",
        "special_rar_v5_encrypted_headers.rar",
    ] {
        let data = special(name);
        assert_eq!(sniff(head(&data)), Some(FileKind::Rar), "{name}");
        assert_eq!(matches_extension(head(&data), "rar"), Some(true), "{name}");
        assert_eq!(
            measure(FileKind::Rar, &data, data.len() as u64),
            None,
            "{name}"
        );
    }
}

#[test]
fn odd_sized_chunk_is_padded() {
    // the sample really has an odd data chunk followed by a pad byte
    let all = samples();
    let s = all
        .iter()
        .find(|s| s.name == "wav_pcm8_odd_length.wav")
        .unwrap();
    let at = s.data.windows(4).position(|w| w == b"data").unwrap();
    let size = u32::from_le_bytes(s.data[at + 4..at + 8].try_into().unwrap());
    assert_eq!(size % 2, 1);
    assert_eq!(s.data.len(), at + 8 + size as usize + 1);
}
