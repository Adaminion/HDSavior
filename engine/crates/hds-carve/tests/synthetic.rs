//! Structures that the encoders at hand do not produce on demand (64-bit
//! boxes, files without `ftyp`, OpenDML continuation chunks, vendor boxes,
//! damaged files), built by rearranging or patching the real samples.
//! Each test states what was changed.

mod common;

use common::{samples, Rng, Sample};
use hds_carve::{measure, sniff, Confidence, FileKind, Measured, SNIFF_LEN};

fn sample(name: &str) -> Sample {
    samples()
        .into_iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("{name} missing"))
}

/// What the tests here compare of a measurement: length, confidence and
/// extension. `Measured::end_verified` is left out; `end_verified.rs`
/// holds every kind to its rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Want {
    len: u64,
    confidence: Confidence,
    ext: &'static str,
}

fn run(kind: FileKind, data: &[u8]) -> Option<Want> {
    measure(kind, &data, data.len() as u64).map(|m: Measured| Want {
        len: m.len,
        confidence: m.confidence,
        ext: m.ext,
    })
}

fn exact(len: usize, ext: &'static str) -> Option<Want> {
    Some(Want {
        len: len as u64,
        confidence: Confidence::Exact,
        ext,
    })
}

fn probable(len: usize, ext: &'static str) -> Option<Want> {
    Some(Want {
        len: len as u64,
        confidence: Confidence::Probable,
        ext,
    })
}

fn find(data: &[u8], needle: &[u8]) -> usize {
    data.windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or_else(|| panic!("{needle:?} not found"))
}

fn find_all(data: &[u8], needle: &[u8]) -> Vec<usize> {
    (0..data.len().saturating_sub(needle.len() - 1))
        .filter(|&i| data[i..].starts_with(needle))
        .collect()
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

// ------------------------------------------------------------ ISO-BMFF --

/// Top-level boxes of a file whose boxes all have 32-bit sizes.
fn boxes(data: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        assert!(size >= 8 && pos + size <= data.len(), "box at {pos}");
        out.push((
            data[pos + 4..pos + 8].try_into().unwrap(),
            data[pos..pos + size].to_vec(),
        ));
        pos += size;
    }
    assert_eq!(pos, data.len());
    out
}

fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(typ);
    v.extend_from_slice(payload);
    v
}

fn bx64(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = 1u32.to_be_bytes().to_vec();
    v.extend_from_slice(typ);
    v.extend_from_slice(&((payload.len() + 16) as u64).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

fn get<'a>(bs: &'a [([u8; 4], Vec<u8>)], typ: &[u8; 4]) -> &'a [u8] {
    &bs.iter()
        .find(|(t, _)| t == typ)
        .unwrap_or_else(|| panic!("no {typ:?} box"))
        .1
}

#[test]
fn bmff_sample_layouts_are_what_the_tests_assume() {
    let order = |name: &str| -> Vec<String> {
        boxes(&sample(name).data)
            .iter()
            .map(|(t, _)| String::from_utf8_lossy(t).into_owned())
            .collect()
    };
    assert_eq!(
        order("mp4_ffmpeg_moov_last.mp4"),
        ["ftyp", "free", "mdat", "moov"]
    );
    assert_eq!(
        order("mp4_ffmpeg_faststart.mp4"),
        ["ftyp", "moov", "free", "mdat"]
    );
    assert_eq!(
        order("mp4_ffmpeg_quicktime.mov"),
        ["ftyp", "wide", "mdat", "moov"]
    );
    let frag = order("mp4_ffmpeg_fragmented.mp4");
    assert_eq!(&frag[..4], ["ftyp", "moov", "moof", "mdat"]);
    assert_eq!(frag.last().unwrap(), "mfra");
    assert!(frag.iter().filter(|t| *t == "moof").count() >= 2);
    assert_eq!(order("heic_libheif.heic"), ["ftyp", "meta", "mdat"]);
}

#[test]
fn bmff_mdat_with_64_bit_size() {
    // ftyp, free, mdat, moov - with the mdat header rewritten to the
    // 16-byte form that files beyond 4 GiB use
    let b = boxes(&sample("mp4_ffmpeg_moov_last.mp4").data);
    let mdat = bx64(b"mdat", &get(&b, b"mdat")[8..]);
    let file = [get(&b, b"ftyp"), get(&b, b"free"), &mdat, get(&b, b"moov")].concat();
    assert_eq!(sniff(&file), Some(FileKind::Mp4));
    assert_eq!(run(FileKind::Mp4, &file), exact(file.len(), "mp4"));
    let mut followed = file.clone();
    followed.extend_from_slice(&file);
    assert_eq!(run(FileKind::Mp4, &followed), exact(file.len(), "mp4"));
    // a 64-bit size smaller than its own header is not a box
    let mut bad = file.clone();
    let at = find(&bad, b"mdat") + 4;
    bad[at..at + 8].copy_from_slice(&15u64.to_be_bytes());
    assert_eq!(run(FileKind::Mp4, &bad), None);
}

#[test]
fn bmff_box_to_end_of_file_is_not_measurable() {
    // ftyp, moov, free, mdat with mdat's size set to 0 ("until end of file"),
    // as an interrupted recording leaves it
    let mut file = sample("mp4_ffmpeg_faststart.mp4").data;
    let at = find(&file, b"mdat") - 4;
    file[at..at + 4].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(sniff(&file), Some(FileKind::Mp4));
    assert_eq!(run(FileKind::Mp4, &file), None);
}

#[test]
fn bmff_quicktime_without_ftyp() {
    let mut rng = Rng::new(7);
    let mov = boxes(&sample("mp4_ffmpeg_quicktime.mov").data);
    let mp4 = boxes(&sample("mp4_ffmpeg_moov_last.mp4").data);
    let fast = boxes(&sample("mp4_ffmpeg_faststart.mp4").data);
    let layouts: [(&str, Vec<u8>); 3] = [
        // as QuickTime wrote it before 2004: placeholder, media, index
        (
            "wide mdat moov",
            [get(&mov, b"wide"), get(&mov, b"mdat"), get(&mov, b"moov")].concat(),
        ),
        // as old digital cameras wrote it: media, index
        (
            "mdat moov",
            [get(&mp4, b"mdat"), get(&mp4, b"moov")].concat(),
        ),
        // index first
        (
            "moov mdat",
            [get(&fast, b"moov"), get(&fast, b"mdat")].concat(),
        ),
    ];
    for (what, file) in &layouts {
        assert_eq!(sniff(&file[..SNIFF_LEN]), Some(FileKind::Mp4), "{what}");
        // never Exact: such a start cannot be told from the middle of a file
        let want = probable(file.len(), "mov");
        assert_eq!(run(FileKind::Mp4, file), want, "{what}");
        for tail in [
            vec![0u8; 3000],
            rng.bytes(3000),
            sample("mp4_ffmpeg_moov_last.mp4").data,
        ] {
            let mut d = file.clone();
            d.extend(tail);
            assert_eq!(run(FileKind::Mp4, &d), want, "{what} + tail");
        }
        // these are not HEIC or CR3 files
        assert_eq!(run(FileKind::Heic, file), None);
        assert_eq!(run(FileKind::Cr3, file), None);
    }
    // a file that ends with its index, directly followed by another one that
    // has no ftyp either: the second media box starts the second file
    for (what, file) in &layouts[..2] {
        let mut d = file.clone();
        d.extend_from_slice(file);
        assert_eq!(
            run(FileKind::Mp4, &d),
            probable(file.len(), "mov"),
            "{what} twice"
        );
    }
    // media without an index is not a file
    let media_only = [get(&mov, b"wide"), get(&mov, b"mdat")].concat();
    assert_eq!(sniff(&media_only), Some(FileKind::Mp4));
    assert_eq!(run(FileKind::Mp4, &media_only), None);
}

#[test]
fn bmff_index_without_media_is_rejected() {
    let b = boxes(&sample("mp4_ffmpeg_faststart.mp4").data);
    let file = [get(&b, b"ftyp"), get(&b, b"moov")].concat();
    assert_eq!(run(FileKind::Mp4, &file), None);
    let file = [get(&b, b"ftyp"), get(&b, b"moov"), get(&b, b"free")].concat();
    assert_eq!(run(FileKind::Mp4, &file), None);
    let h = boxes(&sample("heic_libheif.heic").data);
    let file = [get(&h, b"ftyp"), get(&h, b"meta")].concat();
    assert_eq!(run(FileKind::Heic, &file), None);
    // and a moov that is not a moov (no movie header inside)
    let mut file = sample("mp4_ffmpeg_faststart.mp4").data;
    let at = find(&file, b"mvhd");
    file[at..at + 4].copy_from_slice(b"mvhx");
    assert_eq!(run(FileKind::Mp4, &file), None);
}

#[test]
fn bmff_vendor_boxes() {
    let b = boxes(&sample("mp4_ffmpeg_faststart.mp4").data);
    let (ftyp, moov, free, mdat) = (
        get(&b, b"ftyp"),
        get(&b, b"moov"),
        get(&b, b"free"),
        get(&b, b"mdat"),
    );
    let vendor = bx(b"XyZ1", b"some camera maker's private data");

    // in the middle, a known box behind it: part of the file
    let file = [ftyp, moov, &vendor, free, mdat].concat();
    assert_eq!(run(FileKind::Mp4, &file), exact(file.len(), "mp4"));

    // at the very end: cannot be told from whatever follows the file, so it
    // is left out - and the result says that the end is a judgement call
    let plain = [ftyp, moov, free, mdat].concat();
    let file = [&plain[..], &vendor].concat();
    assert_eq!(run(FileKind::Mp4, &file), probable(plain.len(), "mp4"));
    let mut padded = file.clone();
    padded.resize(file.len() + 4096, 0);
    assert_eq!(run(FileKind::Mp4, &padded), probable(plain.len(), "mp4"));

    // two unknown boxes in a row are not followed
    let file = [ftyp, moov, &vendor, &vendor, free, mdat].concat();
    assert_eq!(
        run(FileKind::Mp4, &file),
        None,
        "the media box is never reached"
    );

    // text behind the file: `size` and `type` are letters, the "box" would
    // be a gigabyte long
    let mut file = plain.clone();
    file.extend_from_slice(b"ABCDEFGH and so on, an ordinary text file follows the video here.\n");
    assert_eq!(run(FileKind::Mp4, &file), exact(plain.len(), "mp4"));

    // a trailer that is not box-structured (Samsung, Insta360 append such)
    let mut file = plain.clone();
    file.extend_from_slice(&[
        0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, b'S', b'E', b'F', b'T',
    ]);
    assert_eq!(run(FileKind::Mp4, &file), exact(plain.len(), "mp4"));
}

#[test]
fn bmff_fragmented_without_trailing_index() {
    let b = boxes(&sample("mp4_ffmpeg_fragmented.mp4").data);
    assert_eq!(b.last().unwrap().0, *b"mfra");
    // drop the random access index: every fragment boundary is now a
    // possible end, and the result admits it
    let without: Vec<u8> = b[..b.len() - 1]
        .iter()
        .flat_map(|(_, d)| d.clone())
        .collect();
    assert_eq!(run(FileKind::Mp4, &without), probable(without.len(), "mp4"));
    // drop the last fragment as well
    let shorter: Vec<u8> = b[..b.len() - 3]
        .iter()
        .flat_map(|(_, d)| d.clone())
        .collect();
    assert_eq!(run(FileKind::Mp4, &shorter), probable(shorter.len(), "mp4"));
    // the initialisation part alone carries no media
    let init: Vec<u8> = b[..2].iter().flat_map(|(_, d)| d.clone()).collect();
    assert_eq!(run(FileKind::Mp4, &init), None);
}

#[test]
fn bmff_canon_cr3_like_structure() {
    // No CR3 encoder exists outside Canon cameras. This reproduces the box
    // layout documented for the format (ftyp 'crx ', moov starting with
    // Canon's uuid box that holds CNCV, two more uuid boxes, mdat) around
    // the moov of a real MP4.
    let b = boxes(&sample("mp4_ffmpeg_faststart.mp4").data);
    let ftyp = bx(b"ftyp", b"crx \0\0\0\x01crx isom");
    let canon = |version: &[u8]| {
        let mut payload = vec![
            0x85, 0xC0, 0xB6, 0x87, 0x82, 0x0F, 0x11, 0xE0, 0x81, 0x11, 0xF4, 0xCE, 0x46, 0x2B,
            0x6A, 0x48,
        ];
        payload.extend(bx(b"CNCV", version));
        bx(b"uuid", &payload)
    };
    let build = |version: &[u8]| {
        let moov = bx(
            b"moov",
            &[&canon(version)[..], &get(&b, b"moov")[8..]].concat(),
        );
        let xmp = bx(
            b"uuid",
            &[&[0xBE; 16][..], b"<?xpacket begin='' ?>"].concat(),
        );
        let preview = bx(b"uuid", &[&[0xEA; 16][..], &[0u8; 64]].concat());
        [
            &ftyp[..],
            &moov,
            &xmp,
            &preview,
            &bx(b"free", &[0; 32]),
            get(&b, b"mdat"),
        ]
        .concat()
    };
    let cr3 = build(b"CanonCR3_001/00.09.00/00.00.00");
    assert_eq!(sniff(&cr3), Some(FileKind::Cr3));
    assert_eq!(run(FileKind::Cr3, &cr3), exact(cr3.len(), "cr3"));
    assert_eq!(run(FileKind::Mp4, &cr3), None);
    let crm = build(b"CanonCRM0001/02.09.00/00.00.00");
    assert_eq!(run(FileKind::Cr3, &crm), exact(crm.len(), "crm"));
    // cut inside the image data
    assert_eq!(run(FileKind::Cr3, &cr3[..cr3.len() - 100]), None);
}

// ---------------------------------------------------------------- RIFF --

fn riff(form: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = b"RIFF".to_vec();
    v.extend(((body.len() + 4) as u32).to_le_bytes());
    v.extend_from_slice(form);
    v.extend_from_slice(body);
    v
}

/// The whole `LIST movi` chunk of an AVI sample.
fn movi_list(avi: &[u8]) -> &[u8] {
    let at = find(avi, b"movi") - 8;
    assert_eq!(&avi[at..at + 4], b"LIST");
    let size = u32::from_le_bytes(avi[at + 4..at + 8].try_into().unwrap()) as usize;
    &avi[at..at + 8 + size]
}

#[test]
fn avi_opendml_continuation_chunks() {
    // The first RIFF chunk of a real AVI, followed by RIFF-AVIX chunks as an
    // OpenDML writer adds them for every further gigabyte (FFmpeg only does
    // so beyond 1 GiB, so the continuation is assembled here).
    let avi = sample("avi_ffmpeg_mjpeg_pcm.avi").data;
    let part = riff(b"AVIX", movi_list(&avi));
    let file = [&avi[..], &part, &part, &part].concat();
    assert_eq!(run(FileKind::Avi, &file), exact(file.len(), "avi"));

    // another AVI behind it is another file
    let mut two = file.clone();
    two.extend_from_slice(&avi);
    assert_eq!(run(FileKind::Avi, &two), exact(file.len(), "avi"));
    let mut slack = file.clone();
    slack.resize(file.len() + 10_000, 0);
    assert_eq!(run(FileKind::Avi, &slack), exact(file.len(), "avi"));

    // a continuation chunk that is cut short means the file is incomplete
    assert_eq!(run(FileKind::Avi, &file[..file.len() - 50]), None);
    // ... unless the cut is exactly between two chunks: then the shorter
    // file is structurally whole
    assert_eq!(
        run(FileKind::Avi, &file[..file.len() - part.len()]),
        exact(file.len() - part.len(), "avi")
    );

    // AVIX after something that is not an AVI means nothing
    let wav = sample("wav_pcm16.wav").data;
    let odd = [&wav[..], &part].concat();
    assert_eq!(run(FileKind::Wav, &odd), exact(wav.len(), "wav"));
}

#[test]
fn riff_size_field_that_disagrees_with_the_chunks() {
    // size field 8 too large (a classic writer bug: file size instead of
    // file size - 8)
    let wav = sample("wav_pcm16.wav").data;
    let mut bad = wav.clone();
    let size = u32::from_le_bytes(bad[4..8].try_into().unwrap());
    bad[4..8].copy_from_slice(&(size + 8).to_le_bytes());
    // the bytes it claims are not there
    assert_eq!(run(FileKind::Wav, &bad), None);
    // they are there (whatever they are): the header is believed, without
    // certainty
    let mut padded = bad.clone();
    padded.resize(bad.len() + 512, 0);
    assert_eq!(run(FileKind::Wav, &padded), probable(wav.len() + 8, "wav"));

    // a recording that was never finalised
    for unfinished in [0u32, u32::MAX] {
        let mut d = wav.clone();
        d[4..8].copy_from_slice(&unfinished.to_le_bytes());
        d.resize(wav.len() + 512, 0);
        assert_eq!(run(FileKind::Wav, &d), None);
    }
    // no audio data chunk
    let mut d = wav.clone();
    let at = find(&d, b"data");
    d[at..at + 4].copy_from_slice(b"dat_");
    assert_eq!(run(FileKind::Wav, &d), None);
}

#[test]
fn rf64_sizes_come_from_ds64() {
    let wav = sample("wav_rf64.wav").data;
    assert_eq!(&wav[..4], b"RF64");
    // BW64 is the same container under its ITU name
    let mut bw = wav.clone();
    bw[..4].copy_from_slice(b"BW64");
    assert_eq!(sniff(&bw), Some(FileKind::Wav));
    assert_eq!(run(FileKind::Wav, &bw), exact(wav.len(), "wav"));
    // the 64-bit data size is what counts
    let mut bad = wav.clone();
    let data_size = u64::from_le_bytes(bad[28..36].try_into().unwrap());
    bad[28..36].copy_from_slice(&(data_size - 2).to_le_bytes());
    bad.resize(wav.len() + 512, 0);
    assert_eq!(
        run(FileKind::Wav, &bad).map(|m| m.confidence),
        Some(Confidence::Probable)
    );
    // without ds64 the saturated sizes mean nothing
    let mut bad = wav.clone();
    bad[12..16].copy_from_slice(b"JUNK");
    assert_eq!(sniff(&bad), None);
    assert_eq!(run(FileKind::Wav, &bad), None);
}

// ------------------------------------------------------------ Matroska --

/// Offsets of the level-1 elements with the given 4-byte ID inside the
/// segment of a Matroska sample.
fn level1(data: &[u8], id: [u8; 4]) -> Vec<usize> {
    let vint = |p: usize| -> (u64, usize) {
        let n = data[p].leading_zeros() as usize + 1;
        let mut v = (data[p] as u64) & (0xFF >> n);
        for &b in &data[p + 1..p + n] {
            v = v << 8 | b as u64;
        }
        (v, n)
    };
    let seg = find(data, &[0x18, 0x53, 0x80, 0x67]);
    let (_, n) = vint(seg + 4);
    let mut p = seg + 4 + n;
    let mut out = Vec::new();
    while p + 5 <= data.len() {
        let id_len = data[p].leading_zeros() as usize + 1;
        let (size, n) = vint(p + id_len);
        if data[p..p + id_len] == id[..id_len.min(4)] && id_len == 4 {
            out.push(p);
        }
        p += id_len + n + size as usize;
    }
    out
}

#[test]
fn mkv_cluster_of_unknown_size_is_not_measurable() {
    // a live WebM stream writes clusters without a size; here the size of
    // the first cluster of a finished file is overwritten with "unknown"
    for name in [
        "mkv_ffmpeg_vp9_many_clusters.webm",
        "special_mkv_unknown_size.mkv",
    ] {
        let mut data = if name.starts_with("special") {
            common::special(name)
        } else {
            sample(name).data
        };
        let clusters = level1(&data, [0x1F, 0x43, 0xB6, 0x75]);
        assert!(clusters.len() >= 2, "{name}: {} clusters", clusters.len());
        let size_at = clusters[0] + 4;
        let n = data[size_at].leading_zeros() as usize + 1;
        data[size_at] = 0xFF >> (n - 1);
        for b in &mut data[size_at + 1..size_at + n] {
            *b = 0xFF;
        }
        assert_eq!(run(FileKind::Mkv, &data), None, "{name}");
    }
}

#[test]
fn mkv_segment_size_must_match_its_children() {
    let data = sample("mkv_ffmpeg.mkv").data;
    let seg = find(&data, &[0x18, 0x53, 0x80, 0x67]);
    // FFmpeg writes the segment size in 8 bytes
    assert_eq!(data[seg + 4], 0x01);
    let size =
        u64::from_be_bytes(data[seg + 4..seg + 12].try_into().unwrap()) & 0x00FF_FFFF_FFFF_FFFF;
    for wrong in [size - 1, size + 1, size / 2] {
        let mut bad = data.clone();
        bad[seg + 4..seg + 12].copy_from_slice(&(wrong | 1 << 56).to_be_bytes());
        bad.resize(data.len() + 4096, 0);
        assert_eq!(
            run(FileKind::Mkv, &bad),
            None,
            "segment size {wrong} instead of {size}"
        );
    }
    // an unknown top-level element inside the segment
    let mut bad = data.clone();
    let cues = level1(&data, [0x1C, 0x53, 0xBB, 0x6B])[0];
    bad[cues] = 0x1D;
    assert_eq!(run(FileKind::Mkv, &bad), None);
    // not a Matroska document type
    let mut bad = data.clone();
    let at = find(&bad, b"matroska");
    bad[at..at + 8].copy_from_slice(b"notmedia");
    assert_eq!(sniff(&bad), None);
    assert_eq!(run(FileKind::Mkv, &bad), None);
}

// ---------------------------------------------------------------- JPEG --

#[test]
fn jpeg_fill_bytes_before_markers() {
    // any number of FF bytes may precede a marker
    let data = sample("jpeg_baseline.jpg").data;
    let sos = find(&data, &[0xFF, 0xDA]);
    let mut padded = data[..sos].to_vec();
    padded.extend([0xFF; 5]);
    padded.extend_from_slice(&data[sos..data.len() - 2]);
    padded.extend([0xFF; 3]);
    padded.extend_from_slice(&data[data.len() - 2..]);
    assert_eq!(run(FileKind::Jpeg, &padded), exact(padded.len(), "jpg"));
}

#[test]
fn jpeg_restart_markers_must_be_in_order() {
    let data = sample("jpeg_restart.jpg").data;
    let sos = find(&data, &[0xFF, 0xDA]);
    let rst: Vec<usize> = (sos..data.len() - 1)
        .filter(|&i| data[i] == 0xFF && (0xD0..=0xD7).contains(&data[i + 1]))
        .collect();
    assert!(rst.len() > 16, "{} restart markers", rst.len());
    assert_eq!(data[rst[0] + 1], 0xD0);
    assert_eq!(data[rst[9] + 1], 0xD1, "the count wraps after RST7");

    // out of sequence: what a jump into another file's data looks like
    let mut bad = data.clone();
    bad[rst[3] + 1] = 0xD5;
    assert_eq!(run(FileKind::Jpeg, &bad), None);

    // restart markers without a restart interval
    let dri = find(&data, &[0xFF, 0xDD, 0x00, 0x04]);
    let mut bad = data[..dri].to_vec();
    bad.extend_from_slice(&data[dri + 6..]);
    assert_eq!(run(FileKind::Jpeg, &bad), None);
}

#[test]
fn jpeg_structure_violations() {
    let data = sample("jpeg_baseline.jpg").data;
    // no end-of-image marker: zeros to the end of the cluster instead
    let mut bad = data[..data.len() - 2].to_vec();
    bad.resize(data.len() + 4096, 0);
    assert_eq!(run(FileKind::Jpeg, &bad), None);
    // an illegal marker in the entropy-coded data
    let mut bad = data.clone();
    let mid = find(&data, &[0xFF, 0xDA]) + 200;
    bad[mid] = 0xFF;
    bad[mid + 1] = 0x4F;
    assert_eq!(run(FileKind::Jpeg, &bad), None);
    // scan without a frame header
    let sof = find(&data, &[0xFF, 0xC0]);
    let len = u16::from_be_bytes([data[sof + 2], data[sof + 3]]) as usize;
    let mut bad = data[..sof].to_vec();
    bad.extend_from_slice(&data[sof + 2 + len..]);
    assert_eq!(run(FileKind::Jpeg, &bad), None);
    // a segment length that points past the end
    let mut bad = data.clone();
    bad[4] = 0xFF;
    bad[5] = 0xFF;
    assert_eq!(run(FileKind::Jpeg, &bad), None);
    // an image that ends right after its headers
    let mut bad = data[..find(&data, &[0xFF, 0xDA])].to_vec();
    bad.extend([0xFF, 0xD9]);
    assert_eq!(run(FileKind::Jpeg, &bad), None);
}

#[test]
fn jpeg_thumbnail_does_not_end_the_file() {
    let data = sample("jpeg_exif_thumbnail.jpg").data;
    // the Exif segment really holds a complete JPEG, markers and all
    let app1_len = u16::from_be_bytes([data[4], data[5]]) as usize;
    let inside = &data[..4 + app1_len];
    let soi = find_all(inside, &[0xFF, 0xD8, 0xFF]);
    assert!(soi.len() >= 2, "no embedded thumbnail");
    assert!(
        find_all(inside, &[0xFF, 0xD9]).iter().any(|&p| p > soi[1]),
        "thumbnail has no EOI"
    );
    assert_eq!(run(FileKind::Jpeg, &data), exact(data.len(), "jpg"));
}

// ----------------------------------------------------------------- PNG --

#[test]
fn png_checksums_are_verified() {
    let data = sample("png_rgb.png").data;
    let idat = find(&data, b"IDAT");
    // one flipped bit in the image data
    let mut bad = data.clone();
    bad[idat + 100] ^= 0x10;
    assert_eq!(run(FileKind::Png, &bad), None);
    // one flipped bit in a stored CRC
    let mut bad = data.clone();
    let n = bad.len();
    bad[n - 1] ^= 1;
    assert_eq!(run(FileKind::Png, &bad), None);
    // a chunk length that would swallow the rest of the disk
    let mut bad = data.clone();
    bad[idat - 4..idat].copy_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
    bad.resize(data.len() + 100_000, 0x55);
    assert_eq!(run(FileKind::Png, &bad), None);

    // a private chunk with a correct CRC is part of the file
    let iend = data.len() - 12;
    let mut chunk = 5u32.to_be_bytes().to_vec();
    chunk.extend_from_slice(b"prVtHELLO");
    chunk.extend(crc32(&chunk[4..]).to_be_bytes());
    let good = [&data[..iend], &chunk, &data[iend..]].concat();
    assert_eq!(run(FileKind::Png, &good), exact(good.len(), "png"));
    // ... with a wrong one it is not
    let mut bad = good.clone();
    bad[iend + 9] ^= 0x20;
    assert_eq!(run(FileKind::Png, &bad), None);
    // no image data at all
    let ihdr_end = 8 + 25;
    let empty = [&data[..ihdr_end], &data[iend..]].concat();
    assert_eq!(run(FileKind::Png, &empty), None);
}

// ----------------------------------------------------------------- GIF --

#[test]
fn gif_structure_violations() {
    let data = sample("gif_animated.gif").data;
    // trailer replaced: the block chain runs into whatever follows
    let mut bad = data.clone();
    let n = bad.len();
    bad[n - 1] = 0x00;
    bad.resize(n + 4096, 0);
    assert_eq!(run(FileKind::Gif, &bad), None);
    // header only
    let mut bad = data[..13].to_vec();
    bad.push(0x3B);
    assert_eq!(run(FileKind::Gif, &bad), None);
    // an extension label that does not exist
    let ext = find(&data, &[0x21, 0xF9, 0x04]);
    let mut bad = data.clone();
    bad[ext + 1] = 0x77;
    assert_eq!(run(FileKind::Gif, &bad), None);
}

// ----------------------------------------------------------------- ZIP --

#[test]
fn zip_end_record_that_disagrees_with_the_walk() {
    let data = sample("zip_infozip_deflate.zip").data;
    let eocd = data.len() - 22;
    assert_eq!(&data[eocd..eocd + 4], b"PK\x05\x06");
    // wrong central directory offset: not the start of this archive
    let mut bad = data.clone();
    bad[eocd + 16] ^= 0x40;
    assert_eq!(run(FileKind::Zip, &bad), None);
    // which is what a candidate at a later local header looks like
    let second = find_all(&data, b"PK\x03\x04")[1];
    assert_eq!(sniff(&data[second..]), Some(FileKind::Zip));
    assert_eq!(run(FileKind::Zip, &data[second..]), None);
    // wrong entry count
    let mut odd = data.clone();
    odd[eocd + 10] ^= 0x01;
    assert_eq!(run(FileKind::Zip, &odd), probable(data.len(), "zip"));
    // no end record
    let mut bad = data[..eocd].to_vec();
    bad.resize(data.len() + 4096, 0);
    assert_eq!(run(FileKind::Zip, &bad), None);
    // archive comment announced but not there
    let mut bad = data.clone();
    bad[eocd + 20] = 200;
    assert_eq!(run(FileKind::Zip, &bad), None);
    // ... and there
    bad.extend([b'c'; 200]);
    assert_eq!(run(FileKind::Zip, &bad), exact(data.len() + 200, "zip"));
    // a compressed size that jumps into the middle of nowhere
    let mut bad = data.clone();
    bad[18] ^= 0x04;
    assert_eq!(run(FileKind::Zip, &bad), None);
}

#[test]
fn zip_streamed_entry_without_descriptor_signature() {
    // The data descriptor's signature is optional. Remove it from an archive
    // written with descriptors (and fix up the offsets that move).
    let data = sample("zip_python_stream_descriptors.zip").data;
    let descriptors = find_all(&data, b"PK\x07\x08");
    assert_eq!(descriptors.len(), 3);
    let mut out = Vec::new();
    let mut last = 0;
    for &d in &descriptors {
        out.extend_from_slice(&data[last..d]);
        last = d + 4;
    }
    out.extend_from_slice(&data[last..]);
    // the central directory has moved 12 bytes towards the start
    let eocd = out.len() - 22;
    let offset = u32::from_le_bytes(out[eocd + 16..eocd + 20].try_into().unwrap());
    out[eocd + 16..eocd + 20].copy_from_slice(&(offset - 12).to_le_bytes());
    assert_eq!(run(FileKind::Zip, &out), exact(out.len(), "zip"));
    // with ZIP64 descriptors (8-byte sizes) as well
    let data = sample("zip_python_stream_zip64.zip").data;
    let descriptors = find_all(&data, b"PK\x07\x08");
    assert_eq!(descriptors.len(), 2);
    let mut out = [
        &data[..descriptors[0]],
        &data[descriptors[0] + 4..descriptors[1]],
        &data[descriptors[1] + 4..],
    ]
    .concat();
    let eocd = out.len() - 22;
    let offset = u32::from_le_bytes(out[eocd + 16..eocd + 20].try_into().unwrap());
    out[eocd + 16..eocd + 20].copy_from_slice(&(offset - 8).to_le_bytes());
    assert_eq!(run(FileKind::Zip, &out), exact(out.len(), "zip"));
}

// ------------------------------------------------------------ 7z, RAR --

#[test]
fn sevenz_header_checksums() {
    let data = sample("7z_lzma2.7z").data;
    // a flipped bit in the end header: its CRC no longer matches
    let mut bad = data.clone();
    let n = bad.len();
    bad[n - 3] ^= 0x01;
    assert_eq!(run(FileKind::SevenZip, &bad), None);
    // a flipped bit in the start header is already caught by sniff
    let mut bad = data.clone();
    bad[13] ^= 0x01;
    assert_eq!(sniff(&bad), None);
    assert_eq!(run(FileKind::SevenZip, &bad), None);
    // the packed streams in between are not read, so not checked
    let mut changed = data.clone();
    changed[40] ^= 0xFF;
    assert_eq!(run(FileKind::SevenZip, &changed), exact(data.len(), "7z"));
}

#[test]
fn rar_header_checksums() {
    for name in ["rar_v5.rar", "rar_v4_rar420.rar"] {
        let data = sample(name).data;
        // The first file header follows the archive header: at offset 24
        // in a RAR 5 archive (signature 8, archive header 16), at 20 in a
        // RAR 4 one (marker 7, archive header 13). Offset 30 is inside it
        // in both.
        let at = 30;
        let mut bad = data.clone();
        bad[at] ^= 0x01;
        assert_eq!(run(FileKind::Rar, &bad), None, "{name}");
    }
    // RAR 4 without its end block (old versions, or `rar -en`): what was
    // walked is offered, without certainty
    let data = sample("rar_v4_rar420.rar").data;
    let end_block = data.len() - 7;
    assert_eq!(data[end_block + 2], 0x7B);
    let mut cut = data[..end_block].to_vec();
    assert_eq!(run(FileKind::Rar, &cut), probable(end_block, "rar"));
    cut.resize(end_block + 4096, 0);
    assert_eq!(run(FileKind::Rar, &cut), probable(end_block, "rar"));
    // RAR 5 always has an end block
    let data = sample("rar_v5.rar").data;
    assert_eq!(run(FileKind::Rar, &data[..data.len() - 8]), None);
}

// ----------------------------------------------------------------- PDF --

#[test]
fn pdf_revisions() {
    let data = sample("pdf_incremental_update.pdf").data;
    let ends: Vec<usize> = find_all(&data, b"%%EOF").iter().map(|p| p + 6).collect();
    assert_eq!(ends.len(), 3, "original and two updates");
    assert_eq!(*ends.last().unwrap(), data.len());
    // cut in the middle of the last update: the revision before it is a
    // complete PDF, and that is what is offered
    let cut = &data[..ends[1] + 300];
    assert_eq!(run(FileKind::Pdf, cut), probable(ends[1], "pdf"));
    let cut = &data[..ends[0] + 40];
    assert_eq!(run(FileKind::Pdf, cut), probable(ends[0], "pdf"));
    // a wrong cross-reference offset: not this file's trailer
    let mut bad = data[..ends[0]].to_vec();
    let at = find(&bad, b"startxref\n") + 10;
    bad[at] = if bad[at] == b'9' { b'8' } else { b'9' };
    assert_eq!(run(FileKind::Pdf, &bad), None);
}

#[test]
fn pdf_linearized_length_must_be_confirmed() {
    let data = sample("pdf_qpdf_linearized.pdf").data;
    assert_eq!(run(FileKind::Pdf, &data), exact(data.len(), "pdf"));
    // /L larger than the file (file truncated): falls back to the scan,
    // which finds no trailer for a cut file
    let cut = &data[..data.len() - 200];
    assert_eq!(run(FileKind::Pdf, cut), None);
    // /L pointing somewhere that is not the end: ignored, the scan decides
    let mut odd = data.clone();
    let at = find(&odd, b"/L ") + 3;
    odd[at] = b'1';
    assert_eq!(run(FileKind::Pdf, &odd), probable(data.len(), "pdf"));
}

// ------------------------------------------------------------ BMP, TIFF --

#[test]
fn bmp_size_field_variants() {
    let data = sample("bmp_rgb24.bmp").data;
    let len = data.len();
    // missing size field: the dimensions decide
    let mut odd = data.clone();
    odd[2..6].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(sniff(&odd), Some(FileKind::Bmp));
    assert_eq!(run(FileKind::Bmp, &odd), probable(len, "bmp"));
    // two bytes of padding declared and present
    let mut odd = data.clone();
    odd[2..6].copy_from_slice(&((len + 2) as u32).to_le_bytes());
    odd.extend([0, 0]);
    assert_eq!(run(FileKind::Bmp, &odd), probable(len + 2, "bmp"));
    // far more than the pixels need
    let mut bad = data.clone();
    bad[2..6].copy_from_slice(&((len * 3) as u32).to_le_bytes());
    bad.resize(len * 4, 0);
    assert_eq!(run(FileKind::Bmp, &bad), None);
    // pixel data missing
    assert_eq!(run(FileKind::Bmp, &data[..len - 1]), None);
}

#[test]
fn tiff_broken_directories() {
    let data = sample("tiff_pillow_uncompressed.tif").data;
    assert_eq!(&data[..8], b"II*\0\x08\0\0\0");
    let entries = u16::from_le_bytes([data[8], data[9]]) as usize;
    let next = 10 + 12 * entries;
    // a directory chain that loops back to itself
    let mut bad = data.clone();
    bad[next..next + 4].copy_from_slice(&8u32.to_le_bytes());
    assert_eq!(run(FileKind::Tiff, &bad), None);
    // a next-directory pointer to nowhere
    let mut bad = data.clone();
    bad[next..next + 4].copy_from_slice(&0x00F0_0000u32.to_le_bytes());
    assert_eq!(run(FileKind::Tiff, &bad), None);
    // image data that is not there
    assert_eq!(run(FileKind::Tiff, &data[..data.len() - 1]), None);
    // StripByteCounts removed (tag 279 renamed): the image cannot be sized
    let mut bad = data.clone();
    let tag = (0..entries)
        .map(|i| 10 + 12 * i)
        .find(|&e| data[e..e + 2] == [0x17, 0x01])
        .unwrap();
    bad[tag..tag + 2].copy_from_slice(&[0x18, 0x01]);
    assert_eq!(run(FileKind::Tiff, &bad), None);
}

#[test]
fn tiff_raw_flavours_by_header() {
    // Canon CR2: TIFF with "CR\x02\0" at offset 8 and the first IFD at 16.
    // Built from a real TIFF by shifting everything 8 bytes (all offsets in
    // this file are in the first IFD and its out-of-line values).
    let s = sample("tiff_pidng_dng.dng");
    assert_eq!(run(FileKind::Tiff, &s.data).map(|m| m.ext), Some("dng"));
    let orf = {
        let mut d = s.data.clone();
        d[2..4].copy_from_slice(b"RO");
        d
    };
    assert_eq!(sniff(&orf), Some(FileKind::Tiff));
    assert_eq!(run(FileKind::Tiff, &orf), probable(orf.len(), "orf"));
    // Panasonic RW2 says where the raw data starts but not how long it is
    let mut rw2 = b"IIU\0\x18\0\0\0".to_vec();
    rw2.extend([
        0x88, 0xE7, 0x74, 0xD8, 0xF8, 0x25, 0x1D, 0x4D, 0x94, 0x7A, 0x6E, 0x77, 0x82, 0x2B, 0x5D,
        0x6A,
    ]);
    rw2.extend(2u16.to_le_bytes());
    rw2.extend([0x01, 0x00, 0x07, 0x00, 4, 0, 0, 0, b'0', b'3', b'1', b'0']); // PanasonicRawVersion
    rw2.extend([0x18, 0x01, 0x04, 0x00, 1, 0, 0, 0, 0x00, 0x02, 0, 0]); // RawDataOffset = 512
    rw2.extend(0u32.to_le_bytes());
    rw2.resize(4096, 0xAA);
    assert_eq!(sniff(&rw2), Some(FileKind::Tiff));
    assert_eq!(hds_carve::matches_extension(&rw2, "rw2"), Some(true));
    assert_eq!(run(FileKind::Tiff, &rw2), None);
    // without the RW2 signature the same bytes are not a TIFF header at all
    let mut not = rw2.clone();
    not[9] ^= 1;
    assert_eq!(sniff(&not), None);
}

// ------------------------------------------- across the read-block edge --
//
// Sequential walkers read in blocks that start at 64 KiB and double up to
// 1 MiB. The samples are far smaller, so these tests stretch real files
// until a two-byte marker, a five-byte keyword or a four-byte signature lies
// across the edge between two blocks, where a scanner that only looks inside
// its current block would miss it. (A unit test in src/lib.rs does this
// exhaustively with tiny blocks; these go through the public API at the
// real block sizes.)

const BLOCK: usize = 1 << 20;
/// Where one read ends and the next begins: for a scan from offset 0, and
/// for one that takes over from a 16 KiB header read.
const EDGES: [usize; 10] = [
    65_536, 196_608, 458_752, 983_040, 2_031_616, 16_384, 81_920, 212_992, 475_136, 999_424,
];

#[test]
fn jpeg_markers_across_read_blocks() {
    let data = sample("jpeg_restart.jpg").data;
    let sos = find(&data, &[0xFF, 0xDA]);
    let scan = sos + 2 + u16::from_be_bytes([data[sos + 2], data[sos + 3]]) as usize;
    // entropy-coded data is not decoded: any bytes without FF will do
    let build = |end: usize, pair_at: &dyn Fn(usize) -> Option<[u8; 2]>| {
        let mut d = data[..scan].to_vec();
        d.resize(end, 0x35);
        // every read starts on a multiple of 4096: put a pair across each
        for (k, edge) in (8192..end - 2).step_by(4096).enumerate() {
            if let Some(pair) = pair_at(k) {
                d[edge - 1..edge + 1].copy_from_slice(&pair);
            }
        }
        d[end - 2..].copy_from_slice(&[0xFF, 0xD9]);
        d
    };
    for &edge in &EDGES {
        // a stuffed FF across every block edge; EOI across the last one,
        // with a fill byte in front
        let mut d = build(edge + 1, &|_| Some([0xFF, 0x00]));
        d[edge - 2] = 0xFF;
        assert_eq!(&d[edge - 2..], [0xFF, 0xFF, 0xD9]);
        assert_eq!(
            run(FileKind::Jpeg, &d),
            exact(d.len(), "jpg"),
            "EOI across {edge}"
        );
        let mut padded = d.clone();
        padded.resize(d.len() + 4096, 0);
        assert_eq!(
            run(FileKind::Jpeg, &padded),
            exact(d.len(), "jpg"),
            "EOI across {edge}, then zeros"
        );
    }
    // restart markers, in order, across every edge
    let d = build(3 * BLOCK, &|k| Some([0xFF, 0xD0 + (k % 8) as u8]));
    assert_eq!(run(FileKind::Jpeg, &d), exact(d.len(), "jpg"));
    // one of them out of order
    let bad = build(3 * BLOCK, &|k| {
        Some([0xFF, 0xD0 + (if k == 300 { 5 } else { k % 8 }) as u8])
    });
    assert_eq!(run(FileKind::Jpeg, &bad), None);
    // an illegal marker across an edge
    let bad = build(3 * BLOCK, &|k| (k == 200).then_some([0xFF, 0x4F]));
    assert_eq!(run(FileKind::Jpeg, &bad), None);
}

#[test]
fn png_chunk_larger_than_a_read_block() {
    let data = sample("png_rgba.png").data;
    let iend = data.len() - 12;
    let payload: Vec<u8> = (0..BLOCK * 5 / 2 + 7)
        .map(|i| (i * 31 % 251) as u8)
        .collect();
    let mut chunk = (payload.len() as u32).to_be_bytes().to_vec();
    chunk.extend_from_slice(b"prVt");
    chunk.extend_from_slice(&payload);
    chunk.extend(crc32(&chunk[4..]).to_be_bytes());
    let big = [&data[..iend], &chunk, &data[iend..]].concat();
    assert_eq!(run(FileKind::Png, &big), exact(big.len(), "png"));
    let mut bad = big.clone();
    bad[iend + 8 + BLOCK + 5] ^= 0x80;
    assert_eq!(run(FileKind::Png, &bad), None);
}

#[test]
fn gif_sub_blocks_across_read_blocks() {
    let data = sample("gif_static.gif").data;
    // the image data ends with a zero-length sub-block, then the trailer
    assert_eq!(&data[data.len() - 2..], [0x00, 0x3B]);
    for extra in [5000, 5001, 15_000] {
        let mut d = data[..data.len() - 2].to_vec();
        for i in 0..extra {
            // sub-blocks of varying length, so that length bytes fall on
            // both sides of the boundaries
            let n = 200 + (i % 56);
            d.push(n as u8);
            d.extend(std::iter::repeat_n(0xAB, n));
        }
        d.extend([0x00, 0x3B]);
        assert!(d.len() > BLOCK);
        assert_eq!(
            run(FileKind::Gif, &d),
            exact(d.len(), "gif"),
            "{extra} extra sub-blocks"
        );
    }
}

#[test]
fn pdf_trailer_across_read_blocks() {
    let data = sample("pdf_reportlab.pdf").data;
    let marker = find(&data, b"%%EOF");
    let number_at = find(&data, b"startxref\n") + 10;
    let number_end = number_at + data[number_at..].iter().position(|&b| b == b'\n').unwrap();
    let xref: usize = std::str::from_utf8(&data[number_at..number_end])
        .unwrap()
        .parse()
        .unwrap();
    // a comment line after the header pushes everything back; the
    // cross-reference offset is adjusted to match
    let first_line = data.iter().position(|&b| b == b'\n').unwrap() + 1;
    for &edge in &EDGES {
        for shift in 0..5 {
            // "%%EOF" starting 4, 3, 2, 1 and 0 bytes before the edge
            let base = edge - 4 + shift - marker;
            let digits = (xref + base).to_string().len() - (number_end - number_at);
            let pad = base - digits;
            let mut d = data[..first_line].to_vec();
            d.push(b'%');
            d.extend(std::iter::repeat_n(b'x', pad - 2));
            d.push(b'\n');
            d.extend_from_slice(&data[first_line..number_at]);
            d.extend_from_slice((xref + pad).to_string().as_bytes());
            d.extend_from_slice(&data[number_end..]);
            assert_eq!(find(&d, b"%%EOF"), edge - 4 + shift);
            assert_eq!(&d[xref + pad..xref + pad + 4], b"xref");
            assert_eq!(
                run(FileKind::Pdf, &d),
                probable(d.len(), "pdf"),
                "marker at {edge} - 4 + {shift}"
            );
        }
    }
}

#[test]
fn zip_descriptor_across_read_blocks() {
    // A streamed archive whose first entry is stretched so that its data
    // descriptor's signature lies across a block edge. Compressed data is
    // not inflated, so the stretch is arbitrary bytes; the sizes in the
    // descriptor and the directory offset in the end record are fixed up.
    let data = sample("zip_python_stream_descriptors.zip").data;
    let descriptor = find(&data, b"PK\x07\x08");
    let eocd = data.len() - 22;
    for &edge in &EDGES {
        for shift in 0..4 {
            let grow = edge - 3 + shift - descriptor;
            let mut d = data[..descriptor].to_vec();
            d.extend((0..grow).map(|i| (i * 7 % 253) as u8));
            d.extend_from_slice(&data[descriptor..]);
            let at = descriptor + grow;
            assert_eq!(at, edge - 3 + shift);
            let size = u32::from_le_bytes(d[at + 8..at + 12].try_into().unwrap());
            d[at + 8..at + 12].copy_from_slice(&(size + grow as u32).to_le_bytes());
            let e = eocd + grow;
            let offset = u32::from_le_bytes(d[e + 16..e + 20].try_into().unwrap());
            d[e + 16..e + 20].copy_from_slice(&(offset + grow as u32).to_le_bytes());
            assert_eq!(
                run(FileKind::Zip, &d),
                exact(d.len(), "zip"),
                "signature at {edge} - 3 + {shift}"
            );
        }
    }
}

#[test]
fn mp3_frames_across_read_blocks() {
    // the audio frames of a real file, repeated until they fill several
    // read blocks
    let data = sample("mp3_ffmpeg_no_xing.mp3").data;
    let audio = 10
        + ((data[6] as usize) << 21
            | (data[7] as usize) << 14
            | (data[8] as usize) << 7
            | data[9] as usize);
    let mut d = data.clone();
    while d.len() < 3 * BLOCK {
        d.extend_from_slice(&data[audio..]);
    }
    assert_eq!(run(FileKind::Mp3, &d), probable(d.len(), "mp3"));
    let mut tagged = d.clone();
    tagged.extend_from_slice(b"TAG");
    tagged.extend([b' '; 125]);
    assert_eq!(run(FileKind::Mp3, &tagged), probable(tagged.len(), "mp3"));
}

#[test]
fn sevenz_end_header_across_read_blocks() {
    // stored 7z archive: stretch the packed stream so that the end header
    // crosses a block boundary, and re-seal the start header
    let z = sample("7z_stored.7z").data;
    let next_offset = u64::from_le_bytes(z[12..20].try_into().unwrap()) as usize;
    let header = &z[32 + next_offset..];
    let grow = 2 * BLOCK - 32 - next_offset - header.len() / 2;
    let mut d = z[..32 + next_offset].to_vec();
    d.extend(std::iter::repeat_n(0x77, grow));
    d.extend_from_slice(header);
    d[12..20].copy_from_slice(&((next_offset + grow) as u64).to_le_bytes());
    let crc = crc32(&d[12..32]);
    d[8..12].copy_from_slice(&crc.to_le_bytes());
    assert!(32 + next_offset + grow < 2 * BLOCK && d.len() > 2 * BLOCK);
    assert_eq!(run(FileKind::SevenZip, &d), exact(d.len(), "7z"));
    let n = d.len();
    d[n - 1] ^= 1;
    assert_eq!(run(FileKind::SevenZip, &d), None);
}
