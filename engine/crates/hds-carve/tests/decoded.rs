//! The formats whose content is decoded to verify an end (JPEG, GIF) and
//! the streams that are walked to find one (DEFLATE in streamed ZIP
//! entries): foreign data where a fragmented file continues must never
//! give a verified end, whatever the data is - random bytes, zeros, or the
//! data of another file of the same format - and data that never ends
//! must not be read to the work limit.

mod common;

use common::{samples, Counting, Endless, Rng, Sample};
use hds_carve::{measure, FileKind, Measured};

fn of_kind(kind: FileKind) -> Vec<Sample> {
    samples().into_iter().filter(|s| s.kind == kind).collect()
}

/// `data` followed by free space, as a carver finds it.
fn on_disk(kind: FileKind, data: &[u8]) -> Option<Measured> {
    let mut disk = data.to_vec();
    disk.resize(data.len() + 4096, 0);
    measure(kind, &disk, disk.len() as u64)
}

fn find(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len()).position(|w| w == needle)
}

/// Offset of the first scan's entropy-coded data.
fn first_scan(jpeg: &[u8]) -> usize {
    let sos = find(jpeg, &[0xFF, 0xDA]).unwrap();
    sos + 2 + usize::from(u16::from_be_bytes([jpeg[sos + 2], jpeg[sos + 3]]))
}

#[test]
fn a_jpeg_continued_by_another_jpeg_is_not_verified() {
    // A fragmented JPEG A whose next cluster on the disk holds the tail of
    // another JPEG B (all samples are 160x120 pictures from the same
    // encoders, mostly with the same tables): A's clusters up to a cluster
    // boundary inside its scan, then B from one of its cluster boundaries
    // inside its last scan to its EOI. The marker walk alone runs into B's
    // EOI; the decoder must not accept it.
    //
    // (Not cluster-aligned splices can be built to pass: cut A and B at
    // byte positions where the MCUs decoded from A's head and from B's
    // tail add up to A's count, and B's tail is valid JPEG data for A's
    // frame. Cluster boundaries do not line up like that - except when B's
    // entropy-coded data is A's own, as in a copy of the same picture with
    // other metadata: then a splice that skips or repeats a few bytes can
    // decode to the right count, with a small part of the picture missing
    // or doubled. Such pairs are left out here; see "Known gaps".)
    const C: usize = 512;
    let jpegs = of_kind(FileKind::Jpeg);
    let mut verified = Vec::new();
    let mut pairs = 0;
    for a in &jpegs {
        let scan = first_scan(&a.data);
        let own = &a.data[scan..];
        for b in jpegs.iter().filter(|b| b.name != a.name) {
            let last_sos = (0..b.data.len() - 1)
                .rev()
                .find(|&i| b.data[i..].starts_with(&[0xFF, 0xDA]))
                .unwrap();
            if b.data[first_scan(&b.data[last_sos..]) + last_sos..] == *own {
                continue;
            }
            for switch in (scan.next_multiple_of(C)..a.data.len() - 2).step_by(C) {
                for from in (last_sos.next_multiple_of(C)..b.data.len() - 2).step_by(C) {
                    pairs += 1;
                    let d = [&a.data[..switch], &b.data[from..], &a.data[switch..]].concat();
                    if let Some(m) = on_disk(FileKind::Jpeg, &d) {
                        if m.end_verified {
                            verified.push(format!(
                                "{} to {switch} + {} from {from}: {m:?}",
                                a.name, b.name
                            ));
                        }
                    }
                }
            }
        }
    }
    assert!(pairs > 4000, "{pairs}");
    assert!(verified.is_empty(), "{verified:#?}");
}

#[test]
fn a_jpeg_or_gif_with_a_gap_is_not_verified() {
    // A fragmented file as a carver reads it: its first clusters, then
    // clusters of something else, then (shifted) its own rest with its own
    // end marker. Clusters of 512 bytes to 32 KiB.
    const C: usize = 512;
    let mut rng = Rng::new(0xDEC0_0001);
    for kind in [FileKind::Jpeg, FileKind::Gif] {
        for s in of_kind(kind) {
            let n = s.data.len();
            let head = if kind == FileKind::Jpeg {
                first_scan(&s.data)
            } else {
                800
            };
            let cuts: Vec<usize> = (head.next_multiple_of(C)..n - 2).step_by(C).collect();
            for trial in 0..300 {
                let len = C * (1 + rng.below(64) as usize);
                let gap = match trial % 3 {
                    0 => rng.bytes(len),
                    1 => vec![0u8; len],
                    // text: no FF bytes at all
                    _ => b"lorem ipsum dolor sit amet, "
                        .iter()
                        .copied()
                        .cycle()
                        .take(len)
                        .collect(),
                };
                let cut = cuts[rng.below(cuts.len() as u64) as usize];
                let d = [&s.data[..cut], &gap, &s.data[cut..]].concat();
                if let Some(m) = on_disk(kind, &d) {
                    assert!(
                        !m.end_verified,
                        "{}: {} bytes of gap at {cut} (trial {trial}): {m:?}",
                        s.name,
                        gap.len()
                    );
                }
            }
        }
    }
}

#[test]
fn verified_jpeg_and_gif_samples() {
    // every sample of an encoding the decoder covers verifies
    for kind in [FileKind::Jpeg, FileKind::Gif] {
        for s in of_kind(kind) {
            let m = on_disk(kind, &s.data).unwrap();
            assert_eq!(m.len as usize, s.data.len(), "{}", s.name);
            assert_eq!(
                m.end_verified,
                s.name != "jpeg_arithmetic.jpg",
                "{}",
                s.name
            );
        }
    }
}

#[test]
fn a_damaged_scan_followed_by_free_space_is_not_read_to_the_limit() {
    // A JPEG cut off inside its scan, then zeros for ever: the scan does
    // not decode, and the search for a marker behind it gives up in the
    // zeros instead of reading a gigabyte of them.
    for s in of_kind(FileKind::Jpeg) {
        if s.name == "jpeg_arithmetic.jpg" {
            continue;
        }
        let cut = (first_scan(&s.data) + s.data.len()) / 2;
        let endless = Endless {
            prefix: s.data[..cut].to_vec(),
            unit: vec![0],
        };
        let counted = Counting::new(&endless);
        assert_eq!(
            measure(FileKind::Jpeg, &counted, u64::MAX),
            None,
            "{}",
            s.name
        );
        let read = counted.bytes.get();
        assert!(read < 4 << 20, "{}: {read} bytes read", s.name);
    }
}

#[test]
fn a_streamed_zip_entry_that_breaks_off_is_not_read_to_the_limit() {
    // Local headers of streamed DEFLATE entries (bit 3, sizes 0), followed
    // by noise or zeros for ever: the stream is walked and breaks off
    // within a few bytes; nothing searches on for a descriptor.
    let mut header = vec![
        0x50, 0x4B, 0x03, 0x04, 20, 0, 0x08, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 5, 0, 0, 0,
    ];
    header.extend_from_slice(b"a.txt");
    let mut rng = Rng::new(0xDEC0_0002);
    for unit in [rng.bytes(1 << 20), vec![0u8; 1], vec![0xFF; 1]] {
        let endless = Endless {
            prefix: header.clone(),
            unit,
        };
        let counted = Counting::new(&endless);
        assert_eq!(measure(FileKind::Zip, &counted, u64::MAX), None);
        assert!(
            counted.bytes.get() < 4 << 20,
            "{} bytes",
            counted.bytes.get()
        );
    }

    // the archives in the samples that stream their entries, cut inside a
    // streamed entry and continued by noise: not measured, and little read
    // (only up to the first streamed entry of another method: those are
    // still searched for their descriptor - see the carving job's budget)
    for s in samples().into_iter().filter(|s| {
        s.name.contains("stream") || s.name.contains("descriptors") || s.name.contains("stdin")
    }) {
        let n = s.data.len();
        let other_method = (0..n - 30)
            .filter(|&i| s.data[i..].starts_with(b"PK\x03\x04"))
            .find(|&i| s.data[i + 6] & 8 != 0 && s.data[i + 8] != 8)
            .unwrap_or(n - 64);
        for cut in (64..other_method).step_by(97) {
            let endless = Endless {
                prefix: s.data[..cut].to_vec(),
                unit: rng.bytes(65_536),
            };
            let counted = Counting::new(&endless);
            let got = measure(FileKind::Zip, &counted, u64::MAX);
            assert!(
                got.is_none_or(|m| (m.len as usize) < cut),
                "{} cut at {cut}: {got:?}",
                s.name
            );
            assert!(
                counted.bytes.get() < 64 << 20,
                "{} cut at {cut}: {} bytes",
                s.name,
                counted.bytes.get()
            );
        }
    }
}
