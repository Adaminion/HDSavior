//! Streamed ZIP entries (general-purpose bit 3: sizes in a data descriptor
//! behind the data) that are not DEFLATE: stored entries, whose descriptor
//! must carry the CRC-32 of the bytes in front of it, and entries of other
//! methods, which are searched for their descriptor within a bounded window
//! and leave the archive's end unverified. Archives written here by hand.

mod common;

use common::{Counting, Endless, Rng};
use hds_carve::{try_measure, Confidence, FileKind, Limits, MeasureError, Measured};

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

fn u16le(v: usize) -> [u8; 2] {
    (v as u16).to_le_bytes()
}

fn u32le(v: usize) -> [u8; 4] {
    (v as u32).to_le_bytes()
}

/// One entry: what goes into its local header, data and descriptor.
struct Entry<'a> {
    name: &'a str,
    method: u16,
    data: &'a [u8],
    /// The CRC-32 written to the descriptor and the directory.
    crc: u32,
    /// The descriptor carries its optional signature.
    signed: bool,
}

impl<'a> Entry<'a> {
    fn stored(name: &'a str, data: &'a [u8]) -> Entry<'a> {
        Entry {
            name,
            method: 0,
            data,
            crc: crc32(data),
            signed: true,
        }
    }
}

/// A streamed archive: every entry with bit 3 set and zero sizes in its
/// local header, a descriptor behind its data.
fn archive(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for e in entries {
        let at = out.len();
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&u16le(20)); // version needed
        out.extend_from_slice(&u16le(0x0008)); // sizes in the descriptor
        out.extend_from_slice(&e.method.to_le_bytes());
        out.extend_from_slice(&[0; 4]); // time, date
        out.extend_from_slice(&[0; 12]); // crc, sizes: unknown yet
        out.extend_from_slice(&u16le(e.name.len()));
        out.extend_from_slice(&u16le(0));
        out.extend_from_slice(e.name.as_bytes());
        out.extend_from_slice(e.data);
        if e.signed {
            out.extend_from_slice(b"PK\x07\x08");
        }
        out.extend_from_slice(&e.crc.to_le_bytes());
        out.extend_from_slice(&u32le(e.data.len()));
        out.extend_from_slice(&u32le(e.data.len()));

        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&u16le(20));
        central.extend_from_slice(&u16le(20));
        central.extend_from_slice(&u16le(0x0008));
        central.extend_from_slice(&e.method.to_le_bytes());
        central.extend_from_slice(&[0; 4]);
        central.extend_from_slice(&e.crc.to_le_bytes());
        central.extend_from_slice(&u32le(e.data.len()));
        central.extend_from_slice(&u32le(e.data.len()));
        central.extend_from_slice(&u16le(e.name.len()));
        central.extend_from_slice(&[0; 12]); // extra, comment, disk, attributes
        central.extend_from_slice(&u32le(at));
        central.extend_from_slice(e.name.as_bytes());
    }
    let directory = out.len();
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    out.extend_from_slice(&u16le(entries.len()));
    out.extend_from_slice(&u16le(entries.len()));
    out.extend_from_slice(&u32le(central.len()));
    out.extend_from_slice(&u32le(directory));
    out.extend_from_slice(&u16le(0));
    out
}

/// Measures `data` followed by free space.
fn run(data: &[u8]) -> Option<Measured> {
    let mut disk = data.to_vec();
    disk.resize(data.len() + 4096, 0);
    try_measure(FileKind::Zip, &disk, disk.len() as u64, &Limits::DEFAULT).ok()
}

fn summary(m: Option<Measured>) -> Option<(u64, Confidence, bool)> {
    m.map(|m| (m.len, m.confidence, m.end_verified))
}

#[test]
fn stored_entries_are_checked_by_their_crc() {
    let mut rng = Rng::new(11);
    let a = rng.bytes(5000);
    let b = rng.bytes(70_000);
    for signed in [true, false] {
        let entries = [
            Entry {
                signed,
                ..Entry::stored("a.bin", &a)
            },
            Entry::stored("empty/", &[]),
            Entry {
                signed,
                ..Entry::stored("b.bin", &b)
            },
        ];
        let zip = archive(&entries);
        assert_eq!(
            summary(run(&zip)),
            Some((zip.len() as u64, Confidence::Exact, true)),
            "signed {signed}"
        );
        // a descriptor whose CRC does not match the data: the entry has no
        // end (the data was changed, or this is not where it ends)
        let bad = archive(&[
            Entry {
                signed,
                crc: crc32(&a) ^ 1,
                ..Entry::stored("a.bin", &a)
            },
            Entry::stored("b.bin", &b),
        ]);
        assert_eq!(run(&bad), None, "signed {signed}");
    }
}

#[test]
fn a_descriptor_inside_stored_data_does_not_end_the_entry() {
    // The stored entry holds bytes that look like the rest of an archive
    // ending there: a signature-less descriptor whose size is right, a
    // central directory, an end record that points at it. Without the CRC
    // that was taken for the end of the archive, 1 byte past the data's
    // first stretch - Exact and verified, and wrong.
    let filler = vec![b'x'; 300];
    let fake_dir_at = 30 + "a.bin".len() + filler.len() + 12;
    let mut inner = filler.clone();
    inner.extend_from_slice(&u32le(0x1234_5678)); // not the CRC
    inner.extend_from_slice(&u32le(filler.len()));
    inner.extend_from_slice(&u32le(filler.len()));
    let mut fake_central = b"PK\x01\x02".to_vec();
    fake_central.extend_from_slice(&[20, 0, 20, 0, 8, 0, 0, 0, 0, 0, 0, 0]);
    fake_central.extend_from_slice(&u32le(0x1234_5678));
    fake_central.extend_from_slice(&u32le(filler.len()));
    fake_central.extend_from_slice(&u32le(filler.len()));
    fake_central.extend_from_slice(&u16le(5));
    fake_central.extend_from_slice(&[0; 12]);
    fake_central.extend_from_slice(&u32le(0));
    fake_central.extend_from_slice(b"a.bin");
    inner.extend_from_slice(&fake_central);
    inner.extend_from_slice(b"PK\x05\x06\0\0\0\0\x01\0\x01\0");
    inner.extend_from_slice(&u32le(fake_central.len()));
    inner.extend_from_slice(&u32le(fake_dir_at));
    inner.extend_from_slice(&u16le(0));
    let fake_end = 30 + 5 + inner.len();
    inner.extend_from_slice(b" and the entry goes on");

    let zip = archive(&[Entry {
        signed: false,
        ..Entry::stored("a.bin", &inner)
    }]);
    assert_eq!(&zip[fake_end - 22..fake_end - 18], b"PK\x05\x06");
    assert_eq!(
        summary(run(&zip)),
        Some((zip.len() as u64, Confidence::Exact, true))
    );
}

#[test]
fn other_methods_are_searched_within_a_window_and_not_verified() {
    let mut rng = Rng::new(12);
    let data = rng.bytes(100_000);
    for method in [12u16, 14, 93, 95, 98] {
        let zip = archive(&[Entry {
            method,
            ..Entry::stored("a.bin", &data)
        }]);
        // found, the length is right, but nothing vouches for the data
        assert_eq!(
            summary(run(&zip)),
            Some((zip.len() as u64, Confidence::Exact, false)),
            "method {method}"
        );
    }
    // a stored entry that is encrypted: its CRC is of the plain text
    let mut zip = archive(&[Entry {
        crc: 0xDEAD_BEEF,
        ..Entry::stored("a.bin", &data)
    }]);
    zip[6] |= 1;
    let directory = zip.len() - 22 - 46 - 5;
    zip[directory + 8] |= 1;
    assert_eq!(
        summary(run(&zip)),
        Some((zip.len() as u64, Confidence::Exact, false))
    );

    // beyond the window: not measured (a documented gap)
    let big = vec![0x5Au8; 33 << 20];
    let zip = archive(&[Entry {
        method: 12,
        ..Entry::stored("a.bin", &big)
    }]);
    assert_eq!(run(&zip), None);
}

/// A local header announcing a streamed entry, then noise for ever: what a
/// false candidate looks like (a header whose archive was overwritten).
fn false_candidate(method: u16) -> Endless {
    let mut prefix = archive(&[Entry {
        method,
        ..Entry::stored("a.bin", &[])
    }]);
    prefix.truncate(30 + 5);
    Endless {
        prefix,
        // a unit of odd length, so that the noise does not repeat on block
        // boundaries
        unit: Rng::new(13).bytes((1 << 20) + 7),
    }
}

#[test]
fn a_false_candidate_costs_at_most_the_window() {
    for method in [12u16, 14, 95] {
        let src = Counting::new(false_candidate(method));
        let r = try_measure(FileKind::Zip, &src, u64::MAX, &Limits::DEFAULT);
        assert!(matches!(r, Err(MeasureError::NotMeasurable)), "{r:?}");
        let read = src.bytes.get();
        assert!(
            read > 32 << 20 && read <= (32 << 20) + (2 << 20),
            "method {method}: {read} bytes read"
        );
    }
    // a stored entry cannot be bounded this way (its data may be anything,
    // any length): it is read to the work limit
    let limits = Limits {
        max_read_bytes: 48 << 20,
        max_reads: 100_000,
    };
    let src = Counting::new(false_candidate(0));
    let r = try_measure(FileKind::Zip, &src, u64::MAX, &limits);
    assert!(matches!(r, Err(MeasureError::LimitReached)), "{r:?}");
}
