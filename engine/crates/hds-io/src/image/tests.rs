use std::sync::Arc;

use super::*;
use crate::resilient::classify_read_error;
use crate::test_util::{Disk, TempDir};
use crate::testing::XorShift;

const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";

fn write_file(dir: &TempDir, name: &str, data: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, data).unwrap();
    path
}

fn random(len: usize, seed: u64) -> Vec<u8> {
    let mut data = vec![0u8; len];
    XorShift::new(seed).fill(&mut data);
    data
}

#[test]
fn reads_sectors_of_a_plain_image() {
    let dir = TempDir::new("image-plain");
    let data = random(64 * 512, 1);
    let path = write_file(&dir, "plain.img", &data);
    let image = ImageFile::open(&path, None).unwrap();
    assert_eq!(
        (
            image.size(),
            image.sector_size(),
            image.physical_sector_size()
        ),
        (64 * 512, 512, 512)
    );
    assert_eq!(image.file_len(), 64 * 512);
    let mut buf = AlignedBuf::new(8 * 512);
    image.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..], &data[..4096]);
    image.read_at(56 * 512, &mut buf).unwrap();
    assert_eq!(&buf[..], &data[56 * 512..]);
    assert!(image.is_alive());

    let info = image.info();
    assert_eq!(info.kind, SourceKind::Image);
    assert_eq!(
        (info.size, info.sector_size, info.physical_sector_size),
        (64 * 512, 512, 512)
    );
    assert_eq!(
        (info.model, info.serial, info.disk_number),
        (None, None, None)
    );
    assert!(
        std::path::Path::new(&info.path).is_absolute(),
        "{}",
        info.path
    );
    assert!(info.path.ends_with("plain.img"));
    assert!(image.path().is_absolute());
    assert!(format!("{image:?}").contains("plain.img"));

    // Requests that break the contract are caller errors and never reach the file.
    for (offset, len) in [
        (1u64, 512usize),
        (0, 100),
        (64 * 512, 512),
        (63 * 512, 1024),
    ] {
        let err = image.read_at(offset, &mut buf[..len]).unwrap_err();
        assert_eq!(
            classify_read_error(&err),
            ReadFailure::Caller,
            "{offset}+{len}"
        );
    }
}

#[test]
fn a_trailing_partial_sector_is_left_out() {
    let dir = TempDir::new("image-partial");
    let data = random(10 * 512 + 100, 2);
    let path = write_file(&dir, "truncated.img", &data);
    let image = ImageFile::open(&path, None).unwrap();
    assert_eq!(image.size(), 10 * 512);
    assert_eq!(image.file_len(), 10 * 512 + 100);
    let mut buf = AlignedBuf::new(512);
    image.read_at(9 * 512, &mut buf).unwrap();
    assert_eq!(&buf[..], &data[9 * 512..10 * 512]);
    assert!(
        image.read_at(10 * 512, &mut buf).is_err(),
        "the partial sector is not readable"
    );
    // With 4096-byte sectors only one whole sector is left.
    let image = ImageFile::open(&path, Some(4096)).unwrap();
    assert_eq!((image.size(), image.sector_size()), (4096, 4096));
}

#[test]
fn refuses_what_is_not_a_usable_image() {
    let dir = TempDir::new("image-refuse");
    let tiny = write_file(&dir, "tiny.img", &[1u8; 511]);
    assert_eq!(
        ImageFile::open(&tiny, None).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    let empty = write_file(&dir, "empty.img", &[]);
    assert_eq!(
        ImageFile::open(&empty, None).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    let one_sector = write_file(&dir, "one.img", &[1u8; 512]);
    assert_eq!(
        ImageFile::open(&one_sector, Some(4096)).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(ImageFile::open(&one_sector, None).unwrap().size(), 512);
    // A directory is not an image.
    assert!(ImageFile::open(dir.path(), None).is_err());
    assert_eq!(
        ImageFile::open(&dir.join("missing.img"), None)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    for bad in [0u32, 511, 513, 8192] {
        assert_eq!(
            ImageFile::open(&one_sector, Some(bad)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[cfg(windows)]
#[test]
fn devices_are_never_opened_as_images() {
    for device in [
        r"\\.\PhysicalDrive0",
        r"\\.\C:",
        r"\\?\GLOBALROOT\Device\Null",
        r"\\.\NUL",
        "NUL",
        "CON",
        r"\\?\C:",
    ] {
        let err = ImageFile::open(std::path::Path::new(device), None).expect_err(device);
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{device}: {err}");
    }
}

#[test]
fn sector_size_is_detected_from_a_gpt_header() {
    let dir = TempDir::new("image-detect");
    // GPT of a 4096-byte-sector disk: header at byte 4096.
    let mut four_k = Disk::new(600, 4096);
    four_k.put_gpt(&[(BASIC_DATA, 64, 500)]);
    let path = write_file(&dir, "4kn.img", &four_k.data);
    let image = ImageFile::open(&path, None).unwrap();
    assert_eq!((image.sector_size(), image.size()), (4096, 600 * 4096));
    let table = crate::parse_partition_table(&image);
    assert_eq!(table.table, crate::TableKind::Gpt);
    assert_eq!(table.partitions[0].offset, 64 * 4096);
    // The caller's word wins over the detection.
    let forced = ImageFile::open(&path, Some(512)).unwrap();
    assert_eq!(forced.sector_size(), 512);

    // GPT of a 512-byte-sector disk: header at byte 512.
    let mut normal = Disk::new(4096, 512);
    normal.put_gpt(&[(BASIC_DATA, 64, 4000)]);
    let path = write_file(&dir, "512.img", &normal.data);
    assert_eq!(ImageFile::open(&path, None).unwrap().sector_size(), 512);

    // "EFI PART" at byte 4096 without a valid header behind it means nothing.
    let mut fake = vec![0u8; 16 * 4096];
    fake[4096..4104].copy_from_slice(b"EFI PART");
    let path = write_file(&dir, "fake.img", &fake);
    assert_eq!(ImageFile::open(&path, None).unwrap().sector_size(), 512);
    // Neither does a 4096-byte header in a file that also has a valid 512-byte one.
    let mut both = four_k.data.clone();
    both[512..1024].copy_from_slice(&normal.data[512..1024]);
    let path = write_file(&dir, "both.img", &both);
    assert_eq!(ImageFile::open(&path, None).unwrap().sector_size(), 512);
    // A file that ends before byte 4096 + 512.
    let path = write_file(&dir, "short.img", &four_k.data[..4096 + 100]);
    assert_eq!(ImageFile::open(&path, None).unwrap().sector_size(), 512);
}

#[test]
fn location_identifies_the_file_not_the_path() {
    let dir = TempDir::new("image-id");
    let path = write_file(&dir, "a.img", &[7u8; 4096]);
    let other = write_file(&dir, "b.img", &[7u8; 4096]);
    let first = ImageFile::open(&path, None).unwrap();
    let again = ImageFile::open(&dir.join(".").join("a.img"), None).unwrap();
    let different = ImageFile::open(&other, None).unwrap();
    let SourceLocation::File { id } = first.location() else {
        panic!("{:?}", first.location())
    };
    assert_eq!(again.location(), SourceLocation::File { id });
    assert_ne!(different.location(), SourceLocation::File { id });
    // A hard link is the same file under another name.
    let link = dir.join("hardlink.img");
    if std::fs::hard_link(&path, &link).is_ok() {
        let linked = ImageFile::open(&link, None).unwrap();
        assert_eq!(linked.location(), SourceLocation::File { id });
    }
}

#[test]
fn concurrent_positional_reads_do_not_interfere() {
    let dir = TempDir::new("image-threads");
    let data = Arc::new(random(256 * 512, 3));
    let path = write_file(&dir, "shared.img", &data);
    let image = Arc::new(ImageFile::open(&path, None).unwrap());
    let threads: Vec<_> = (0..8u64)
        .map(|t| {
            let (image, data) = (image.clone(), data.clone());
            std::thread::spawn(move || {
                let mut buf = AlignedBuf::new(1024);
                for i in 0..200u64 {
                    let sector = (t * 37 + i * 11) % 255;
                    image.read_at(sector * 512, &mut buf).unwrap();
                    assert_eq!(
                        &buf[..],
                        &data[sector as usize * 512..sector as usize * 512 + 1024]
                    );
                }
            })
        })
        .collect();
    threads.into_iter().for_each(|t| t.join().unwrap());
}

/// S4 on Windows: while the image is the source, the kernel refuses writers,
/// truncation and deletion - for this process and for every other.
#[cfg(windows)]
#[test]
fn the_open_image_cannot_be_written_truncated_or_deleted() {
    let dir = TempDir::new("image-s4");
    let data = random(8 * 512, 4);
    let path = write_file(&dir, "locked.img", &data);
    let image = ImageFile::open(&path, None).unwrap();
    const ERROR_SHARING_VIOLATION: i32 = 32;
    let write = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap_err();
    assert_eq!(
        write.raw_os_error(),
        Some(ERROR_SHARING_VIOLATION),
        "open for writing: {write}"
    );
    let truncate = std::fs::File::create(&path).unwrap_err();
    assert_eq!(
        truncate.raw_os_error(),
        Some(ERROR_SHARING_VIOLATION),
        "truncate: {truncate}"
    );
    let delete = std::fs::remove_file(&path).unwrap_err();
    assert_eq!(
        delete.raw_os_error(),
        Some(ERROR_SHARING_VIOLATION),
        "delete: {delete}"
    );
    // Other readers are welcome.
    assert_eq!(std::fs::read(&path).unwrap(), data);
    let mut buf = AlignedBuf::new(512);
    image.read_at(0, &mut buf).unwrap();
    drop(image);
    // Once the source is closed the file is an ordinary file again.
    std::fs::remove_file(&path).unwrap();
}

/// Unix has no share modes: an image that shrinks under the engine is a lost
/// source, not a run of bad sectors and not a caller's bug.
#[cfg(unix)]
#[test]
fn a_shrinking_image_is_a_lost_source() {
    let dir = TempDir::new("image-shrink");
    let data = random(64 * 512, 5);
    let path = write_file(&dir, "shrinks.img", &data);
    let image = ImageFile::open(&path, None).unwrap();
    let mut buf = AlignedBuf::new(4096);
    image.read_at(0, &mut buf).unwrap();
    assert!(image.is_alive());
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(8 * 512)
        .unwrap();
    assert!(!image.is_alive());
    let err = image.read_at(32 * 512, &mut buf).unwrap_err();
    assert_eq!(classify_read_error(&err), ReadFailure::DeviceLost);
    let outcome = crate::read_resilient(
        &image,
        0,
        &mut AlignedBuf::new(64 * 512),
        &std::sync::atomic::AtomicBool::new(false),
    );
    assert!(
        matches!(outcome, crate::ReadOutcome::SourceLost(_)),
        "{outcome:?}"
    );
}

#[test]
fn known_good_sectors_decide_liveness() {
    let good = KnownGood::new();
    // Nothing read yet: nothing to disprove liveness with.
    assert!(good.still_readable(512, |_, _| panic!("no read expected")));
    good.note(4096);
    good.note(8192);
    good.note(12288);
    let asked = std::sync::Mutex::new(Vec::new());
    assert!(good.still_readable(512, |offset, buf| {
        asked.lock().unwrap().push((offset, buf.len()));
        Ok(())
    }));
    assert_eq!(
        *asked.lock().unwrap(),
        vec![(12288, 512)],
        "the latest success is tried first"
    );
    // The latest one went bad, the first one still answers: alive.
    assert!(good.still_readable(4096, |offset, _| if offset == 4096 {
        Ok(())
    } else {
        Err(io::Error::other("bad"))
    }));
    assert!(!good.still_readable(512, |_, _| Err(io::Error::other("gone"))));
}
