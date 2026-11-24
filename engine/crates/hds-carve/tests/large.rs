//! Files of many gigabytes, without the gigabytes: the headers and indexes
//! of real samples laid out around holes. What is checked is that `measure`
//! finds the end beyond 4 GiB and gets there by hopping over the payload
//! instead of reading it.

mod common;

use common::{samples, Counting, Sample, Sparse};
use hds_carve::{measure, Confidence, FileKind, Measured};

const GIB: u64 = 1 << 30;

fn sample(name: &str) -> Sample {
    samples()
        .into_iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("{name} missing"))
}

fn find(data: &[u8], needle: &[u8]) -> usize {
    data.windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or_else(|| panic!("{needle:?} not found"))
}

/// Measures `file` and checks length, confidence and the amount of I/O.
/// `end_verified` is what `Measured::end_verified` must say: whether a
/// structure behind the last payload confirms the end.
fn check(
    what: &str,
    kind: FileKind,
    file: &Sparse,
    ext: &'static str,
    end_verified: bool,
    max_reads: u64,
) {
    let counted = Counting::new(file);
    let got = measure(kind, &counted, u64::MAX);
    assert_eq!(
        got,
        Some(Measured {
            len: file.len,
            confidence: Confidence::Exact,
            ext,
            end_verified,
        }),
        "{what}"
    );
    let (reads, bytes) = (counted.reads.get(), counted.bytes.get());
    println!(
        "{what}: {} GiB measured with {reads} reads, {bytes} bytes",
        file.len / GIB
    );
    assert!(reads <= max_reads, "{what}: {reads} reads");
    assert!(
        bytes <= max_reads * 16_384 + (4 << 20),
        "{what}: {bytes} bytes read"
    );

    // followed by more data, and cut one byte short
    let mut longer = Sparse {
        parts: file.parts.clone(),
        len: file.len,
    };
    longer.gap(5 * GIB);
    assert_eq!(
        measure(kind, &longer, u64::MAX).map(|m| m.len),
        Some(file.len),
        "{what} + zeros"
    );
    // not complete: nothing, or (RAR 4 without its end block) an uncertain
    // shorter file
    let incomplete = |m: Option<Measured>| {
        m.is_none_or(|m| m.len < file.len - 1 && m.confidence == Confidence::Probable)
    };
    assert!(
        incomplete(measure(kind, file, file.len - 1)),
        "{what} with max_len one short"
    );
    let shorter = Sparse {
        parts: file.parts.clone(),
        len: file.len - 1,
    };
    assert!(
        incomplete(measure(kind, &shorter, u64::MAX)),
        "{what} one byte short"
    );
}

fn boxes(data: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        out.push((
            data[pos + 4..pos + 8].try_into().unwrap(),
            data[pos..pos + size].to_vec(),
        ));
        pos += size;
    }
    out
}

fn get<'a>(bs: &'a [([u8; 4], Vec<u8>)], typ: &[u8; 4]) -> &'a [u8] {
    &bs.iter().find(|(t, _)| t == typ).unwrap().1
}

#[test]
fn mp4_with_media_data_beyond_4_gib() {
    // camera layout: ftyp, mdat with a 64-bit size, moov at the very end
    let b = boxes(&sample("mp4_ffmpeg_moov_last.mp4").data);
    let payload = 23 * GIB + 12_345;
    let mut f = Sparse::new();
    f.put(get(&b, b"ftyp")).put(get(&b, b"free"));
    f.put(&1u32.to_be_bytes())
        .put(b"mdat")
        .put(&(payload + 16).to_be_bytes());
    f.gap(payload);
    f.put(get(&b, b"moov"));
    check(
        "mp4, 64-bit mdat, moov last",
        FileKind::Mp4,
        &f,
        "mp4",
        true,
        8,
    );

    // 32-bit size just below the limit, moov first
    let b = boxes(&sample("mp4_ffmpeg_faststart.mp4").data);
    let payload = 4 * GIB - 4096;
    let mut f = Sparse::new();
    f.put(get(&b, b"ftyp"))
        .put(get(&b, b"moov"))
        .put(get(&b, b"free"));
    f.put(&((payload + 8) as u32).to_be_bytes()).put(b"mdat");
    f.gap(payload);
    check(
        "mp4, moov first, 4 GiB mdat",
        FileKind::Mp4,
        &f,
        "mp4",
        false,
        8,
    );
}

#[test]
fn fragmented_mp4_of_many_gigabytes() {
    // ftyp, moov, then 400 fragments of 64 MiB each, then the index
    let b = boxes(&sample("mp4_ffmpeg_fragmented.mp4").data);
    let moof = get(&b, b"moof");
    let mut f = Sparse::new();
    f.put(get(&b, b"ftyp")).put(get(&b, b"moov"));
    for _ in 0..400 {
        f.put(moof);
        f.put(&(64u32 << 20).to_be_bytes()).put(b"mdat");
        f.gap((64 << 20) - 8);
    }
    f.put(get(&b, b"mfra"));
    check("fragmented mp4", FileKind::Mp4, &f, "mp4", true, 420);
}

#[test]
fn avi_opendml_beyond_4_gib() {
    // first RIFF chunk of 1 GiB, then five RIFF-AVIX chunks of 1 GiB
    let avi = sample("avi_ffmpeg_video_only.avi").data;
    let movi = find(&avi, b"movi") - 8;
    let movi_size: u32 = (1 << 30) - 4096;
    let mut f = Sparse::new();
    let mut head = avi[..movi].to_vec();
    head[4..8].copy_from_slice(&(movi as u32 - 8 + 8 + movi_size).to_le_bytes());
    f.put(&head)
        .put(b"LIST")
        .put(&movi_size.to_le_bytes())
        .put(b"movi");
    f.gap(movi_size as u64 - 4);
    for _ in 0..5 {
        f.put(b"RIFF")
            .put(&(movi_size + 12).to_le_bytes())
            .put(b"AVIX");
        f.put(b"LIST").put(&movi_size.to_le_bytes()).put(b"movi");
        f.gap(movi_size as u64 - 4);
    }
    check("avi, OpenDML", FileKind::Avi, &f, "avi", false, 40);
}

#[test]
fn rf64_wav_beyond_4_gib() {
    let wav = sample("wav_rf64.wav").data;
    let data_at = find(&wav, b"data");
    let data_size = 7 * GIB + 2;
    let mut head = wav[..data_at + 8].to_vec();
    head[20..28].copy_from_slice(&(data_at as u64 + data_size).to_le_bytes()); // RIFF size
    head[28..36].copy_from_slice(&data_size.to_le_bytes()); // data size
    let mut f = Sparse::new();
    f.put(&head).gap(data_size);
    check("wav, RF64", FileKind::Wav, &f, "wav", false, 8);
}

#[test]
fn matroska_of_many_gigabytes() {
    // the head of a real file (EBML header to Tracks), then 300 clusters of
    // 100 MiB each, with the segment size to match
    let mkv = sample("mkv_ffmpeg.mkv").data;
    let seg = find(&mkv, &[0x18, 0x53, 0x80, 0x67]);
    let first_cluster = find(&mkv, &[0x1F, 0x43, 0xB6, 0x75]);
    let cluster_payload: u64 = 100 << 20;
    let clusters = 300u64;
    let mut cluster_head = vec![0x1F, 0x43, 0xB6, 0x75];
    cluster_head.extend((cluster_payload | 1 << 56).to_be_bytes());
    let segment_size = (first_cluster - seg - 12) as u64 + clusters * (12 + cluster_payload);
    let mut head = mkv[..first_cluster].to_vec();
    head[seg + 4..seg + 12].copy_from_slice(&(segment_size | 1 << 56).to_be_bytes());
    let mut f = Sparse::new();
    f.put(&head);
    for _ in 0..clusters {
        f.put(&cluster_head).gap(cluster_payload);
    }
    check("matroska", FileKind::Mkv, &f, "mkv", false, 320);
}

/// Rewrites a central directory entry so that it states a 64-bit
/// compressed size and/or local header offset: the 32-bit field is
/// saturated and the value goes into the ZIP64 extra field, which holds, in
/// this order, whichever of uncompressed size, compressed size and offset
/// are saturated.
fn central_entry_zip64(entry: &[u8], compressed: Option<u64>, offset: Option<u64>) -> Vec<u8> {
    let le16 = |at: usize| u16::from_le_bytes([entry[at], entry[at + 1]]) as usize;
    let (name_len, extra_len) = (le16(28), le16(30));
    let extra = &entry[46 + name_len..46 + name_len + extra_len];
    // the existing field: the uncompressed size alone
    assert_eq!(&entry[24..28], [0xFF; 4]);
    let at = extra
        .windows(4)
        .position(|w| w == [0x01, 0x00, 0x08, 0x00])
        .expect("ZIP64 extra with one value");
    let mut values = extra[at + 4..at + 12].to_vec();
    let mut out = entry[..46 + name_len].to_vec();
    if let Some(c) = compressed {
        out[20..24].copy_from_slice(&[0xFF; 4]);
        values.extend(c.to_le_bytes());
    }
    if let Some(o) = offset {
        out[42..46].copy_from_slice(&[0xFF; 4]);
        values.extend(o.to_le_bytes());
    }
    out.extend_from_slice(&extra[..at]);
    out.extend([0x01, 0x00]);
    out.extend((values.len() as u16).to_le_bytes());
    out.extend(&values);
    out.extend_from_slice(&extra[at + 12..]);
    let new_extra_len = (out.len() - 46 - name_len) as u16;
    out[30..32].copy_from_slice(&new_extra_len.to_le_bytes());
    out.extend_from_slice(&entry[46 + name_len + extra_len..]);
    out
}

#[test]
fn zip64_with_an_entry_beyond_4_gib() {
    // Info-ZIP's forced-ZIP64 archive of two entries, with the first
    // entry's compressed data enlarged by 6 GiB and every size and offset
    // that depends on it rewritten: the ZIP64 field of the first local
    // header, both central directory entries, the ZIP64 end record and its
    // locator.
    let zip = sample("zip_infozip_forced_zip64.zip").data;
    let le16 = |d: &[u8], at: usize| u16::from_le_bytes([d[at], d[at + 1]]) as usize;
    let le32 = |d: &[u8], at: usize| u32::from_le_bytes(d[at..at + 4].try_into().unwrap()) as u64;
    let le64 = |d: &[u8], at: usize| u64::from_le_bytes(d[at..at + 8].try_into().unwrap());
    let grow = 6 * GIB;

    // first local header: its ZIP64 extra field (uncompressed, compressed)
    let extra_at = 30 + le16(&zip, 26);
    let data_at = extra_at + le16(&zip, 28);
    let zip64_at = extra_at + find(&zip[extra_at..data_at], &[0x01, 0x00, 0x10, 0x00]);
    let old_size = le64(&zip, zip64_at + 12);
    let second = data_at + old_size as usize;
    assert_eq!(&zip[second..second + 4], b"PK\x03\x04");
    let mut head = zip[..data_at].to_vec();
    head[zip64_at + 12..zip64_at + 20].copy_from_slice(&(old_size + grow).to_le_bytes());

    // central directory: two entries
    let cd = find(&zip, b"PK\x01\x02");
    let entry_len =
        |at: usize| 46 + le16(&zip, at + 28) + le16(&zip, at + 30) + le16(&zip, at + 32);
    let cd2 = cd + entry_len(cd);
    let eocd64 = cd2 + entry_len(cd2);
    assert_eq!(&zip[eocd64..eocd64 + 4], b"PK\x06\x06");
    assert_eq!(le32(&zip, cd2 + 42), second as u64);
    let first = central_entry_zip64(&zip[cd..cd2], Some(old_size + grow), None);
    let other = central_entry_zip64(&zip[cd2..eocd64], None, Some(second as u64 + grow));
    let directory = [first, other].concat();
    let grown = (directory.len() - (eocd64 - cd)) as u64;

    // ZIP64 end record (directory size and offset), locator, end record
    let mut tail = zip[eocd64..].to_vec();
    let locator = find(&tail, b"PK\x06\x07");
    let eocd = find(&tail, b"PK\x05\x06");
    let size = le64(&tail, 40) + grown;
    tail[40..48].copy_from_slice(&size.to_le_bytes());
    tail[48..56].copy_from_slice(&(cd as u64 + grow).to_le_bytes());
    tail[locator + 8..locator + 16].copy_from_slice(&(eocd64 as u64 + grow + grown).to_le_bytes());
    tail[eocd + 12..eocd + 16].copy_from_slice(&(size as u32).to_le_bytes());

    let mut f = Sparse::new();
    f.put(&head)
        .put(&zip[data_at..second])
        .gap(grow)
        .put(&zip[second..cd])
        .put(&directory)
        .put(&tail);
    check("zip64", FileKind::Zip, &f, "zip", true, 8);
}

#[test]
fn sevenz_with_packed_streams_beyond_4_gib() {
    // the start header points 9 GiB further to the same end header
    let z = sample("7z_lzma2.7z").data;
    let next_offset = u64::from_le_bytes(z[12..20].try_into().unwrap());
    let grow = 9 * GIB;
    let mut head = z[..32].to_vec();
    head[12..20].copy_from_slice(&(next_offset + grow).to_le_bytes());
    // StartHeaderCRC over the 20 bytes that follow it
    let mut c = !0u32;
    for &b in &head[12..32] {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    head[8..12].copy_from_slice(&(!c).to_le_bytes());
    let mut f = Sparse::new();
    f.put(&head)
        .put(&z[32..32 + next_offset as usize])
        .gap(grow)
        .put(&z[32 + next_offset as usize..]);
    check("7z", FileKind::SevenZip, &f, "7z", true, 8);
}

#[test]
fn rar_with_a_file_beyond_4_gib() {
    // RAR 5: the data size of the first file block is a variable-length
    // integer; replace the block by one with a larger data area and a
    // matching header checksum. Simpler and just as telling: put a real
    // archive's blocks behind a service-free gap by enlarging the stored
    // file's data size field is not possible without re-encoding the vint,
    // so this test uses RAR 4, whose sizes are fixed-width.
    let rar = sample("rar_v4_rar420_stored_recovery.rar").data;
    let block = 7 + 13;
    assert_eq!(
        rar[block + 2],
        0x74,
        "first block after the archive header is a file"
    );
    let flags = u16::from_le_bytes([rar[block + 3], rar[block + 4]]);
    let size = u16::from_le_bytes([rar[block + 5], rar[block + 6]]) as usize;
    let pack = u32::from_le_bytes(rar[block + 7..block + 11].try_into().unwrap()) as usize;
    assert_eq!(flags & 0x0100, 0, "no high size fields yet");
    // new header: flag 0100 set, HIGH_PACK_SIZE and HIGH_UNP_SIZE inserted
    // after the 32 fixed bytes
    let high: u32 = 3; // + 12 GiB
    let mut header = rar[block..block + size].to_vec();
    header[3..5].copy_from_slice(&(flags | 0x0100).to_le_bytes());
    header[5..7].copy_from_slice(&((size + 8) as u16).to_le_bytes());
    header.splice(32..32, [high.to_le_bytes(), high.to_le_bytes()].concat());
    let mut c = !0u32;
    for &b in &header[2..] {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    header[..2].copy_from_slice(&((!c) as u16).to_le_bytes());
    let mut f = Sparse::new();
    f.put(&rar[..block])
        .put(&header)
        .put(&rar[block + size..block + size + pack]);
    f.gap((high as u64) << 32);
    f.put(&rar[block + size + pack..]);
    check(
        "rar 4, 64-bit pack size",
        FileKind::Rar,
        &f,
        "rar",
        true,
        12,
    );
}
