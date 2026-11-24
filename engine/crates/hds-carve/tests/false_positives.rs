//! `sniff` runs on every sector of a multi-terabyte disk. These tests bound
//! how often it says "a file starts here" when none does.
//!
//! The measured rate over 2^34 random heads is printed by the ignored test
//! at the bottom:
//!
//! ```text
//! cargo test --release -p hds-carve --test false_positives -- --ignored --nocapture
//! ```

mod common;

use common::{all_files, Rng};
use hds_carve::{measure, sniff, FileKind, SNIFF_LEN};

/// Offsets (at `step` intervals) where `sniff` reports a file.
fn hits(data: &[u8], step: usize) -> Vec<(usize, FileKind)> {
    (0..data.len())
        .step_by(step)
        .filter_map(|at| sniff(&data[at..]).map(|k| (at, k)))
        .collect()
}

#[test]
fn random_data_at_sector_steps() {
    // what the scanner does: one look per 512-byte sector
    for seed in [1u64, 2, 3, 4] {
        let data = Rng::new(seed).bytes(8 << 20);
        assert_eq!(hits(&data, 512), [], "seed {seed}");
    }
}

#[test]
fn random_data_at_every_offset() {
    // 48 million looks at random bytes, not one of them a file. The
    // weakest signatures have 32 fixed bits, so about 0.03 hits would be
    // expected by chance; none must occur with these fixed seeds.
    for seed in [11u64, 12, 13] {
        let data = Rng::new(seed).bytes(16 << 20);
        assert_eq!(hits(&data, 1), [], "seed {seed}");
    }
}

#[test]
fn constant_and_patterned_sectors() {
    // what large parts of a disk really hold
    for fill in [
        0x00u8, 0xFF, 0xF6, 0xE5, 0x20, 0x55, 0xAA, b'M', b'I', b'P', b'R', b'B', b'G', 0x1A, 0x89,
    ] {
        assert_eq!(sniff(&[fill; 512]), None, "sector of {fill:#04x}");
    }
    let mut ramp = [0u8; 512];
    for (i, b) in ramp.iter_mut().enumerate() {
        *b = i as u8;
    }
    assert_eq!(hits(&ramp, 1), []);
    // little- and big-endian counters, as in sparse tables and indexes
    let mut data = Vec::new();
    for i in 0..200_000u32 {
        data.extend(i.to_le_bytes());
        data.extend(i.to_be_bytes());
    }
    assert_eq!(hits(&data, 1), []);
}

const WORDS: &[&str] = &[
    "the",
    "of",
    "and",
    "a",
    "to",
    "in",
    "is",
    "you",
    "that",
    "it",
    "he",
    "was",
    "for",
    "on",
    "are",
    "as",
    "with",
    "his",
    "they",
    "I",
    "at",
    "be",
    "this",
    "have",
    "from",
    "or",
    "one",
    "had",
    "by",
    "word",
    "but",
    "not",
    "what",
    "all",
    "were",
    "we",
    "when",
    "your",
    "can",
    "said",
    "there",
    "use",
    "an",
    "each",
    "which",
    "she",
    "do",
    "how",
    "their",
    "if",
    "will",
    "up",
    "other",
    "about",
    "out",
    "many",
    "then",
    "them",
    "these",
    "so",
    "some",
    "her",
    "would",
    "make",
    "like",
    "him",
    "into",
    "time",
    "has",
    "look",
    "two",
    "more",
    "write",
    "go",
    "see",
    "number",
    "Recovery",
    "Photograph",
    "Video",
    "Backup",
    "Important",
    "Document",
    "MMXXIV",
    "II",
    "BM",
    "PK",
    "RIFF",
    "ID3",
    "GIF",
    "Rar",
    "ftyp",
    "moov",
    "mdat",
    "wide",
    "PDF",
    "7z",
    "IMG",
    "DSC",
    "MVI",
    "Thumbs",
];

fn prose(rng: &mut Rng, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    while out.len() < len {
        out.extend_from_slice(WORDS[rng.below(WORDS.len() as u64) as usize].as_bytes());
        out.extend_from_slice(match rng.below(20) {
            0 => b". ",
            1 => b",\r\n",
            2 => b"\n\n",
            3 => b": ",
            4 => b"\t",
            _ => b" ",
        });
    }
    out
}

fn source_code(rng: &mut Rng, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 128);
    while out.len() < len {
        let n = rng.below(100_000);
        let line = match rng.below(8) {
            0 => format!(
                "    let value_{n} = buffer[{}] as u32 + 0x{n:08X};\n",
                n % 512
            ),
            1 => format!("#define MAX_ITEMS_{n} ({n}u)\r\n"),
            2 => format!("if (index{n} >= count) {{ return -1; }} // {n}%\n"),
            3 => format!("<div class=\"item-{n}\" id=\"i{n}\">%d items &amp; more</div>\n"),
            4 => format!(
                "{{\"id\": {n}, \"name\": \"IMG_{n:04}.JPG\", \"size\": {}, \"ok\": true}},\n",
                n * 37
            ),
            5 => format!(
                "{n};{};\"{}\";2024-05-{:02}T12:00:00Z\r\n",
                n * 3,
                n % 97,
                n % 28 + 1
            ),
            6 => format!("%% comment {n}\n%PDF is mentioned here, as is PK and 7z and Rar!\n"),
            _ => format!("\t\treturn self.items.get({n}).map(|i| i.len()).unwrap_or(0);\n"),
        };
        out.extend_from_slice(line.as_bytes());
    }
    out
}

fn encoded(rng: &mut Rng, len: usize) -> Vec<u8> {
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(len + 80);
    while out.len() < len {
        if rng.below(2) == 0 {
            for _ in 0..76 {
                out.push(B64[rng.below(64) as usize]);
            }
            out.extend_from_slice(b"\r\n");
        } else {
            out.extend_from_slice(format!("{:08x}  ", out.len()).as_bytes());
            for _ in 0..16 {
                out.extend_from_slice(format!("{:02x} ", rng.below(256)).as_bytes());
            }
            out.push(b'\n');
        }
    }
    out
}

#[test]
fn text_at_every_offset() {
    let mut rng = Rng::new(0x7E87);
    let corpora: [(&str, Vec<u8>); 3] = [
        ("prose", prose(&mut rng, 3 << 20)),
        (
            "source code, markup, CSV, JSON",
            source_code(&mut rng, 3 << 20),
        ),
        ("base64 and hex dumps", encoded(&mut rng, 3 << 20)),
    ];
    for (what, text) in &corpora {
        assert_eq!(hits(text, 1), [], "{what}");
        // the same as UTF-16, the way Windows stores much of its text
        let wide: Vec<u8> = text.iter().take(1 << 20).flat_map(|&b| [b, 0]).collect();
        assert_eq!(hits(&wide, 1), [], "{what} as UTF-16LE");
    }
}

#[test]
fn text_that_starts_like_a_signature() {
    // Lines that begin with a format's magic letters and continue as text.
    // Every one of them must be rejected by the checks behind the magic.
    let magics: &[&[u8]] = &[
        b"BM",
        b"II",
        b"MM",
        b"II*",
        b"GIF8",
        b"PK",
        b"PK\x03\x04",
        b"RIFF",
        b"RF64",
        b"Rar!",
        b"ID3",
        b"7z",
        b"\x89PNG",
        b"\xFF\xD8\xFF",
        b"\xFF\xD8\xFF\xE0",
        b"\xFF\xD8\xFF\xE1",
        b"\xFF\xD8\xFF\xDB",
        b"\xFF\xFB",
        b"\x1A\x45\xDF\xA3",
        b"%PDF",
        b"%PDF-",
        b"\0\0\x10\0moov",
        b"The ftyp",
        b"The moov",
        b"The mdat",
        b"The wide",
    ];
    let mut rng = Rng::new(0xA5C11);
    for magic in magics {
        for _ in 0..2000 {
            let mut line = magic.to_vec();
            line.extend(prose(&mut rng, 80));
            assert_eq!(
                sniff(&line),
                None,
                "{:?}",
                String::from_utf8_lossy(&line[..24])
            );
        }
    }
    // "GIF89a" is six letters of plain text, and the logical screen
    // descriptor behind it constrains almost nothing: a line of text that
    // starts with it can pass sniff() when a comma (the image separator) or
    // an exclamation mark happens to sit at offset 13. It is rare, and
    // measure() does not follow.
    let mut passed = 0;
    for _ in 0..5000 {
        let mut line = b"GIF89a".to_vec();
        line.extend(prose(&mut rng, 4000));
        if sniff(&line).is_some() {
            passed += 1;
            assert_eq!(measure(FileKind::Gif, &line, line.len() as u64), None);
        }
    }
    assert!(
        passed < 150,
        "{passed} of 5000 text lines starting with GIF89a pass sniff"
    );

    // `mdat` is the one signature with nothing fixed behind its four
    // letters: it is accepted with a 32-bit size below 16 MiB only (a zero
    // byte in front, 40 fixed bits), and measure() asks for a movie index
    // behind the media data. Text has no zero bytes.
    assert_eq!(
        sniff(b"\0\x10\0\0mdat follows, with a megabyte of media data."),
        Some(FileKind::Mp4)
    );
    assert_eq!(
        sniff(b"the mdat box holds the media data of an MP4 file."),
        None
    );
    assert_eq!(
        sniff(b"\x01\0\0\0mdat with a size of 16 MiB or more is not taken."),
        None
    );
}

#[test]
fn magic_bytes_are_all_required() {
    // Flipping any single bit of a format's fixed bytes must make sniff()
    // lose that file (it may not turn into another kind either).
    let fixed = |kind: FileKind, head: &[u8]| -> Vec<usize> {
        match kind {
            FileKind::Jpeg => (0..3).collect(),
            FileKind::Png => (0..16).collect(),
            FileKind::Gif => (0..6).collect(),
            FileKind::Bmp => (0..2).chain(6..10).collect(),
            FileKind::Tiff => (0..4).collect(),
            FileKind::Pdf => (0..5).collect(),
            FileKind::Zip => (0..4).collect(),
            FileKind::Mp4 | FileKind::Heic | FileKind::Cr3 => (4..8).collect(),
            FileKind::Avi => (0..4).chain(8..16).chain(20..24).collect(),
            FileKind::Wav | FileKind::Webp => (0..4).chain(8..12).collect(),
            FileKind::Mkv => (0..4).collect(),
            FileKind::Mp3 if head.starts_with(b"ID3") => (0..3).collect(),
            FileKind::Mp3 => vec![0],
            FileKind::SevenZip => (0..6).chain(8..32).collect(),
            FileKind::Rar => (0..7).collect(),
        }
    };
    let mut checked = 0;
    for s in all_files() {
        let head = &s.data[..SNIFF_LEN.min(s.data.len())];
        let Some(kind) = sniff(head) else { continue };
        for byte in fixed(kind, head) {
            for bit in 0..8 {
                let mut h = head.to_vec();
                h[byte] ^= 1 << bit;
                assert_eq!(sniff(&h), None, "{}: byte {byte} bit {bit} flipped", s.name);
                checked += 1;
            }
        }
    }
    assert!(checked > 5000, "{checked}");
}

#[test]
fn real_file_interiors_at_sector_steps() {
    // Inside a file, at sector boundaries, sniff() may see something that
    // starts like a file: a local header of a ZIP entry, a `moov` box. What
    // matters is that measure() does not turn it into a file.
    let mut candidates = 0;
    for s in all_files() {
        for (at, kind) in hits(&s.data, 512) {
            if at == 0 {
                continue;
            }
            candidates += 1;
            let rest = &s.data[at..];
            assert_eq!(
                measure(kind, &rest, rest.len() as u64),
                None,
                "{} at {at} as {kind:?}",
                s.name
            );
        }
    }
    // very few to begin with
    assert!(candidates <= 4, "{candidates} interior candidates");
}

/// Measures the false-positive rate on uniformly random heads. Takes a few
/// minutes in release mode; not part of the normal test run.
#[test]
#[ignore = "long-running measurement; run with --release -- --ignored --nocapture"]
fn measure_false_positive_rate() {
    const TRIALS: u64 = 1 << 34;
    let mut rng = Rng::new(0xFA15E);
    let mut head = [0u8; SNIFF_LEN];
    let mut by_kind = std::collections::BTreeMap::new();
    for _ in 0..TRIALS {
        rng.fill(&mut head);
        if let Some(k) = sniff(&head) {
            *by_kind.entry(k.name()).or_insert(0u64) += 1;
        }
    }
    let total: u64 = by_kind.values().sum();
    println!("{TRIALS} random heads: {total} false positives {by_kind:?}");
    println!(
        "rate: {:.3e} per head = {:.2} per terabyte of random data scanned in 512-byte sectors",
        total as f64 / TRIALS as f64,
        total as f64 / TRIALS as f64 * (1u64 << 40) as f64 / 512.0
    );
    // 2^34 heads against a handful of 32-bit signatures: a few hits
    assert!(
        total <= 40,
        "{total} false positives in {TRIALS} random heads"
    );
}
