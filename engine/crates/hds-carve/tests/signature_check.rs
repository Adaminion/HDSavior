//! `matches_extension` on near misses: foreign data that happens to start
//! with a format's magic bytes, as a wrong geometry or an overwritten first
//! cluster presents it (ALGORITHMS H9, F3). The magic bytes alone used to
//! be enough for JPEG (`FF D8 FF`), ZIP (`PK\x03\x04`) and MP3 (a frame
//! sync, or `ID3`); here they must be followed by structure that holds up.

mod common;

use common::{samples, Rng};
use hds_carve::{matches_extension, FileKind, SNIFF_LEN};

const TRIALS: usize = 20_000;

/// Fraction of `TRIALS` random heads of `len` bytes, starting with
/// `magic`, that `ext` accepts (`Some(true)`).
fn pass_rate(seed: u64, magic: &[u8], len: usize, ext: &str) -> f64 {
    let mut rng = Rng::new(seed);
    let mut passed = 0usize;
    for _ in 0..TRIALS {
        let mut h = rng.bytes(len);
        h[..magic.len()].copy_from_slice(magic);
        passed += usize::from(matches_extension(&h, ext) == Some(true));
    }
    passed as f64 / TRIALS as f64
}

#[test]
fn noise_behind_the_magic_bytes_rarely_passes() {
    for len in [SNIFF_LEN, 1024] {
        // JPEG: SOI and a marker byte. A valid first segment, its length
        // and the next marker right behind it are what is left to chance.
        let jpeg = pass_rate(1, &[0xFF, 0xD8, 0xFF], len, "jpg");
        // ZIP: a local header signature; version, flags, method and a
        // printable name are left to chance
        let zip = pass_rate(2, b"PK\x03\x04", len, "zip");
        // MP3: a valid MPEG-1 layer III frame header
        let mp3 = pass_rate(3, &[0xFF, 0xFB, 0x90, 0x64], len, "mp3");
        let id3 = pass_rate(4, b"ID3", len, "mp3");
        println!("{len} bytes: jpeg {jpeg:.5} zip {zip:.5} mp3 frame {mp3:.5} id3 {id3:.5}");
        assert!(jpeg < 0.005, "{len}: jpeg {jpeg}");
        assert!(zip < 0.001, "{len}: zip {zip}");
        assert!(mp3 < 0.002, "{len}: mp3 {mp3}");
        assert!(id3 < 0.001, "{len}: id3 {id3}");
    }
}

#[test]
fn near_misses_that_used_to_pass() {
    let m = matches_extension;
    // JPEG: SOI and an APP0 marker, then text
    assert_eq!(
        m(b"\xFF\xD8\xFF\xE0\x00\x10\x00\x01\x02 garbage...", "jpg"),
        Some(false)
    );
    // a JFIF segment whose length leads to something that is no marker
    let mut h = b"\xFF\xD8\xFF\xE0\x00\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0".to_vec();
    h.extend_from_slice(b"\x12\x34 not a marker");
    assert_eq!(m(&h, "jpg"), Some(false));
    // ... and the same with a marker there
    h.truncate(20);
    h.extend_from_slice(b"\xFF\xDB\x00\x43\x00");
    assert_eq!(m(&h, "jpg"), Some(true));
    // a DQT of impossible length, an SOF with zero components, an
    // unknown marker, a scan before any frame header
    assert_eq!(m(b"\xFF\xD8\xFF\xDB\x00\x44\x00", "jpg"), Some(false));
    assert_eq!(
        m(b"\xFF\xD8\xFF\xC0\x00\x08\x08\x00\x10\x00\x10\x00", "jpg"),
        Some(false)
    );
    assert_eq!(m(b"\xFF\xD8\xFF\x02\x00\x10\x00\x00", "jpg"), Some(false));
    assert_eq!(
        m(b"\xFF\xD8\xFF\xDA\x00\x08\x01\x01\x00\x00\x3F\x00", "jpg"),
        Some(false)
    );
    // ZIP: a local header signature, then noise
    assert_eq!(m(b"PK\x03\x04\xFF\xFF\x12\x34\x99\x00", "zip"), Some(false));
    // a plausible header with a name made of control bytes
    let mut z = b"PK\x03\x04\x14\0\0\0\x08\0".to_vec();
    z.extend_from_slice(&[0; 16]);
    z.extend_from_slice(&[4, 0, 0, 0]);
    z.extend_from_slice(b"\x01\x02\x03\x04");
    assert_eq!(m(&z, "zip"), Some(false));
    z.truncate(30);
    z.extend_from_slice(b"a.tx");
    assert_eq!(m(&z, "zip"), Some(true));
    // an end record with something in its fields is not an empty archive
    assert_eq!(
        m(
            b"PK\x05\x06\0\0\0\0\x01\0\x01\0\x2e\0\0\0\x10\x20\0\0\0\0",
            "zip"
        ),
        Some(false)
    );
    assert_eq!(
        m(b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", "zip"),
        Some(true)
    );
    // MP3: one frame header and noise is no evidence either way
    let mut rng = Rng::new(5);
    let mut f = rng.bytes(1024);
    f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
    f[417] = 0x00;
    assert_eq!(m(&f, "mp3"), None);
    // `ID3` with a size byte that is not 7-bit, or version 9
    assert_eq!(m(b"ID3\x03\0\0\x80\0\0\0TIT2", "mp3"), None);
    assert_eq!(m(b"ID3\x09\0\0\0\0\0\x10TIT2", "mp3"), None);
    assert_eq!(m(b"ID3\x03\0\0\0\0\0\x10\x01\x02\x03\x04", "mp3"), None);
    assert_eq!(m(b"ID3\x03\0\0\0\0\0\x10TIT2", "mp3"), Some(true));
}

/// A comment (COM) is free text: Pillow, exiftool and cameras write a
/// user's comment in UTF-8 or a code page such as Latin-1. An intact photo
/// whose comment starts with such a character is a JPEG.
#[test]
fn comments_in_8_bit_text_are_jpeg() {
    let jfif = b"\xFF\xE0\x00\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0";
    let comments: [&[u8]; 4] = [
        "\u{a9} 2024 Name".as_bytes(),
        "\u{a9} Studio M\u{fc}ller".as_bytes(),
        "\u{6771}\u{4eac}\u{30bf}\u{30ef}\u{30fc}".as_bytes(),
        b"\xA9 2024 Name",
    ];
    for text in comments {
        let com = |h: &mut Vec<u8>| {
            h.extend_from_slice(&[0xFF, 0xFE]);
            h.extend_from_slice(&(text.len() as u16 + 2).to_be_bytes());
            h.extend_from_slice(text);
        };
        // after JFIF (Pillow, exiftool) and right after SOI
        let mut a = b"\xFF\xD8".to_vec();
        a.extend_from_slice(jfif);
        com(&mut a);
        let mut b = b"\xFF\xD8".to_vec();
        com(&mut b);
        for mut h in [a, b] {
            h.extend_from_slice(b"\xFF\xDB\x00\x43\x00");
            h.resize(SNIFF_LEN.max(h.len()), 0x10);
            assert_eq!(matches_extension(&h, "jpg"), Some(true), "{text:?}");
        }
    }
    // an APPn identifier is still ASCII
    assert_eq!(
        matches_extension(b"\xFF\xD8\xFF\xE1\x00\x10\xA9\xA9\xA9\xA9", "jpg"),
        Some(false)
    );
}

/// Heads from inside real files - what a wrong volume offset shows instead
/// of a file's first sector - at every 512-byte position, as a test of the
/// opinion each extension's check gives on them.
#[test]
fn sectors_inside_real_files_rarely_pass() {
    let mut checked = 0usize;
    let mut passed = [0usize; 3];
    for s in samples() {
        for at in (512..s.data.len().saturating_sub(64)).step_by(512) {
            let h = &s.data[at..s.data.len().min(at + 1024)];
            checked += 1;
            for (i, ext) in ["jpg", "zip", "mp3"].iter().enumerate() {
                // a sector that is itself the start of such a file (an
                // embedded JPEG, the next local header of an archive) is
                // a legitimate match
                let own = match i {
                    0 => s.kind == FileKind::Jpeg || h.starts_with(&[0xFF, 0xD8]),
                    1 => s.kind == FileKind::Zip,
                    _ => s.kind == FileKind::Mp3 || s.kind == FileKind::Avi,
                };
                if !own && matches_extension(h, ext) == Some(true) {
                    passed[i] += 1;
                }
            }
        }
    }
    println!("{checked} sectors: {passed:?} pass as jpg, zip, mp3");
    assert!(checked > 1000);
    assert_eq!(passed, [0, 0, 0]);
}
