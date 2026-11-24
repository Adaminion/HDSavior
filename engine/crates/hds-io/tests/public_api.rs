//! The crate used the way the rest of the engine uses it, through its public
//! interface only: open an image, find the partition, restrict the source to
//! a session range, read around bad sectors, check a destination and write
//! recovered files with the output primitives.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use hds_io::out::{classify_error, CommitOutcome, OnExists, OutDir, OutErrorKind, Timestamps};
use hds_io::testing::{FaultySource, MemSource};
use hds_io::{
    destination_check, fallback_component, open_source, parse_partition_table, read_resilient,
    read_unaligned, sanitize_component, source_warnings, AlignedBuf, BlockSource, ComponentLimit,
    FsKind, Purpose, RangeSet, RangedSource, ReadOutcome, SourceKind, SourceLocation, TableKind,
    Verdict,
};

#[path = "../src/test_util.rs"]
mod test_util;
use test_util::{boot_record, ntfs_boot_sector, Disk, TempDir};

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// A disk image: MBR, one NTFS partition at 1 MiB whose content is a
/// recognisable pattern.
fn build_image(dir: &TempDir) -> std::path::PathBuf {
    let mut disk = Disk::new(16_384, 512);
    disk.put(0, &boot_record(&[(0x80, 0x07, 2_048, 12_288)]));
    for lba in 2_049..14_335u64 {
        let mut sector = [0u8; 512];
        sector[..8].copy_from_slice(&lba.to_le_bytes());
        sector[8..].fill((lba % 251) as u8);
        disk.put(lba, &sector);
    }
    let boot = ntfs_boot_sector(512, 8, 12_287, 0x1122_3344_5566_7788);
    disk.put(2_048, &boot);
    disk.put(14_335, &boot);
    let path = dir.join("disk.img");
    std::fs::write(&path, &disk.data).unwrap();
    path
}

#[test]
fn scan_and_recover_flow_on_an_image() {
    let tmp = TempDir::new("api-flow");
    let image_path = build_image(&tmp);
    let no_cancel = AtomicBool::new(false);

    // open_source -> partitions -> session range.
    let source = open_source(SourceKind::Image, &image_path, None).unwrap();
    assert_eq!(source.info().kind, SourceKind::Image);
    assert!(source_warnings(&*source, 0..source.size()).is_empty());
    let table = parse_partition_table(&*source);
    assert_eq!(table.table, TableKind::Mbr);
    let partition = &table.partitions[0];
    assert_eq!((partition.index, partition.fs), (1, Some(FsKind::Ntfs)));
    assert_eq!(partition.ntfs.as_ref().unwrap().serial, "1122334455667788");
    let session = RangedSource::new(source.clone(), partition.range()).unwrap();

    // A sequential pass in chunks, with the done set of a scan.
    let mut done = RangeSet::new();
    let mut bad = RangeSet::new();
    let mut chunk = AlignedBuf::new(1 << 20);
    while let Some(gap) = done.gaps(session.range()).next() {
        let len = (gap.end - gap.start).min(chunk.len() as u64) as usize;
        match read_resilient(&session, gap.start, &mut chunk[..len], &no_cancel) {
            ReadOutcome::Data { bad: ranges } => {
                bad.extend(ranges);
                done.insert(gap.start..gap.start + len as u64);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(done.total_len(), session.len());
    assert!(bad.is_empty());
    // Nothing outside the session range can be read through the session.
    let outside = read_resilient(&session, 0, &mut chunk[..512], &no_cancel);
    assert!(matches!(outside, ReadOutcome::Internal(_)), "{outside:?}");

    // The destination: fine next to the image, refused when it is the image.
    let dest = tmp.join("Rescued");
    let check = destination_check(
        &dest,
        Purpose::Recover,
        &session,
        session.range(),
        Some(1 << 20),
    );
    assert_eq!(check.verdict, Verdict::Ok, "{}", check.message);
    assert!(check.is_allowed(false, false));
    assert_eq!(check.enough_space, Some(true));
    let itself = destination_check(&image_path, Purpose::Image, &session, session.range(), None);
    assert_eq!(itself.verdict, Verdict::OverlapsSource);

    // Recovery: the root, a folder per rescued directory, a part file per file.
    let root = OutDir::create_root(&dest).unwrap();
    let again = destination_check(
        root.path(),
        Purpose::Recover,
        &session,
        session.range(),
        None,
    );
    assert_eq!(
        again.verdict,
        Verdict::Ok,
        "the created root is checked again by its canonical path"
    );
    assert_eq!(root.remove_leftover_parts(true).unwrap().removed, 0);
    let limit = root.limits().unwrap().max_component;

    // Names as NTFS stored them, made safe; collisions are the caller's rule 5.
    let rescued = [
        "Photos: 2019?",
        "photos_ 2019_",
        "CON",
        "report.txt",
        "report.txt",
    ];
    let mut produced: Vec<String> = Vec::new();
    for (i, raw) in rescued.iter().enumerate() {
        let mut suffix = 0;
        let name = loop {
            let candidate =
                String::from_utf16(&sanitize_component(&utf16(raw), suffix, limit)).unwrap();
            let taken = produced
                .iter()
                .any(|p| p.to_lowercase() == candidate.to_lowercase());
            if !taken {
                break candidate;
            }
            suffix = if suffix == 0 { 2 } else { suffix + 1 };
        };
        // One file per name, with a sector of the partition as content.
        let mut content = vec![0u8; 700];
        let offset = session.start() + 512 * (10 + i as u64) + 100;
        assert!(read_unaligned(&session, offset, &mut content, &no_cancel).is_clean());
        let mut part = root.create_part().unwrap();
        part.write_all(&content).unwrap();
        let times = Timestamps {
            created: 132_223_104_000_000_000,
            modified: 132_223_104_000_000_000,
            accessed: 0,
        };
        assert_eq!(
            part.finish(&times).unwrap().modified,
            hds_io::out::TimeOutcome::Applied
        );
        let outcome = part
            .commit(&name, OnExists::Skip, &|existing| {
                produced.iter().any(|p| p == existing)
            })
            .unwrap();
        assert_eq!(outcome, CommitOutcome::Committed, "{name}");
        assert_eq!(std::fs::read(root.path().join(&name)).unwrap(), content);
        produced.push(name);
    }
    assert_eq!(
        produced,
        [
            "Photos_ 2019_",
            "photos_ 2019_ (2)",
            "_CON",
            "report.txt",
            "report (2).txt"
        ]
    );
    let mut on_disk: Vec<String> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    on_disk.sort();
    let mut expected = produced.clone();
    expected.sort();
    assert_eq!(
        on_disk, expected,
        "exactly the recovered files, no part file"
    );
}

#[test]
fn a_rejected_name_is_retried_with_the_fallback_name() {
    let tmp = TempDir::new("api-fallback");
    let root = OutDir::create_root(&tmp.join("out")).unwrap();
    let limit = root.limits().unwrap().max_component;
    // A name that was not shortened (as if the limit had been unknown): rejected,
    // and classified so that the caller knows a different name will help.
    let too_long = "x".repeat(400) + ".jpeg";
    let mut part = root.create_part().unwrap();
    part.write_all(b"jpeg").unwrap();
    let err = part
        .commit(&too_long, OnExists::Rename, &|_| false)
        .unwrap_err();
    assert_eq!(classify_error(&err), OutErrorKind::NameRejected);
    // The part file survived the failed commit and takes the fallback name.
    let fallback =
        String::from_utf16(&fallback_component(&utf16(&too_long), 91_234, limit)).unwrap();
    assert_eq!(fallback, "x".repeat(24) + "~91234.jpeg");
    assert_eq!(
        part.commit(&fallback, OnExists::Rename, &|_| false)
            .unwrap(),
        CommitOutcome::Committed
    );
    assert_eq!(std::fs::read(root.path().join(&fallback)).unwrap(), b"jpeg");
}

#[test]
fn bad_sectors_and_a_lost_source_are_told_apart_by_callers() {
    let data: Vec<u8> = (0..(4usize << 20))
        .map(|i| (i / 512 % 250 + 1) as u8)
        .collect();
    let disk = Arc::new(FaultySource::new(Arc::new(
        MemSource::with_sector_sizes(data.clone(), 512, 4096)
            .at_location(SourceLocation::Disk { number: 3 }),
    )));
    disk.bad_sectors(1_000_448..1_000_960);
    let session = RangedSource::new(disk.clone(), 1 << 20..3 << 20).unwrap();
    assert_eq!(session.location(), SourceLocation::Disk { number: 3 });
    let cancel = AtomicBool::new(false);
    let mut chunk = AlignedBuf::new(1 << 20);

    // The session range does not contain the bad sector: clean, and never touched.
    assert!(read_resilient(&session, 1 << 20, &mut chunk, &cancel).is_clean());
    disk.bad_sectors((1 << 20) + 8_192..(1 << 20) + 8_704);
    match read_resilient(&session, 1 << 20, &mut chunk, &cancel) {
        ReadOutcome::Data { bad } => {
            // The whole physical sector is given up, zero-filled, and reported.
            assert_eq!(bad, vec![(1 << 20) + 8_192..(1 << 20) + 12_288]);
            assert!(chunk[8_192..12_288].iter().all(|&b| b == 0));
            assert_eq!(&chunk[12_288..], &data[(1 << 20) + 12_288..2 << 20]);
        }
        other => panic!("{other:?}"),
    }
    let touched = disk.touched();
    assert!(
        touched.first().unwrap().start >= 1 << 20 && touched.last().unwrap().end <= 3 << 20,
        "S2a: {touched:?}"
    );

    // The device goes away: nothing is recorded as bad, the job ends with source_lost.
    disk.lose_after_reads(0);
    let outcome = read_resilient(&session, 2 << 20, &mut chunk, &cancel);
    assert!(matches!(outcome, ReadOutcome::SourceLost(_)), "{outcome:?}");
    assert!(!session.is_alive());
    // Cancelled jobs issue no further requests.
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        read_resilient(&session, 2 << 20, &mut chunk, &cancel),
        ReadOutcome::Cancelled
    ));
}

#[test]
fn disks_cannot_be_opened_as_images_or_by_other_names() {
    for (kind, path, sector) in [
        (SourceKind::Disk, r"\\.\E:", None),
        (SourceKind::Disk, r"\\.\PhysicalDrive1", Some(512)),
        (SourceKind::Disk, "disk.img", None),
        (SourceKind::Image, "missing.img", Some(100)),
    ] {
        let err = open_source(kind, Path::new(path), sector)
            .err()
            .expect(path);
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "{path}: {err}"
        );
    }
    assert!(hds_io::list_disks()
        .iter()
        .all(|d| d.kind == SourceKind::Disk && d.disk_number == Some(d.number)));
    let _ = ComponentLimit::DEFAULT;
}
