//! Partition tables and file-system sniffing against the real-driver
//! fixtures (`scripts/linux/make-fixtures.sh`): images written by `sfdisk`,
//! `mkntfs` and `ntfs-3g`, with ground truth in their JSON manifests.
//!
//! The images are looked for in the directory named by `HDS_FIXTURES`, else
//! in `<repository>/fixtures-out`. They are build products, not part of the
//! repository: where they are missing (the Windows CI job) the tests say so
//! and pass. With `HDS_FIXTURES` set, a missing image is a failure.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use hds_io::{
    parse_partition_table, read_resilient, read_unaligned, AlignedBuf, BlockSource, FsKind,
    ImageFile, RangedSource, TableKind,
};
use serde_json::Value;

struct Fixture {
    image: ImageFile,
    manifest: Value,
}

fn fixture(name: &str) -> Option<Fixture> {
    let (dir, required) = match std::env::var_os("HDS_FIXTURES") {
        Some(dir) => (PathBuf::from(dir), true),
        None => (
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures-out"),
            false,
        ),
    };
    let image_path = dir.join(format!("{name}.img"));
    let manifest_path = dir.join(format!("{name}.manifest.json"));
    if !image_path.is_file() || !manifest_path.is_file() {
        assert!(
            !required,
            "HDS_FIXTURES is set but {} is missing",
            image_path.display()
        );
        eprintln!("SKIPPED: fixture {name} not found in {}", dir.display());
        return None;
    }
    let manifest: Value = serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    let image = ImageFile::open(&image_path, None).unwrap();
    assert_eq!(
        image.size(),
        manifest["image"]["size"].as_u64().unwrap(),
        "{name}: image size"
    );
    Some(Fixture { image, manifest })
}

/// Compares the parsed table of a partitioned fixture with its manifest.
fn check_partitioned(name: &str, expected_table: TableKind) {
    let Some(Fixture { image, manifest }) = fixture(name) else {
        return;
    };
    assert_eq!(image.sector_size(), 512, "{name}: detected sector size");
    let table = parse_partition_table(&image);
    assert_eq!(table.table, expected_table, "{name}");
    assert!(table.notes.is_empty(), "{name}: {:?}", table.notes);

    let truth = &manifest["image"]["partition_table"];
    assert_eq!(truth["type"], serde_json::to_value(expected_table).unwrap());
    let expected = truth["partitions"].as_array().unwrap();
    assert_eq!(
        table.partitions.len(),
        expected.len(),
        "{name}: {:?}",
        table.partitions
    );
    let volume = &manifest["new_volume"];
    let mut ntfs_partitions = 0;
    for (found, want) in table.partitions.iter().zip(expected) {
        assert_eq!(
            u64::from(found.index),
            want["index"].as_u64().unwrap(),
            "{name}: index"
        );
        assert_eq!(
            found.offset,
            want["offset"].as_u64().unwrap(),
            "{name}: offset of partition {}",
            found.index
        );
        assert_eq!(
            found.size,
            want["length"].as_u64().unwrap(),
            "{name}: size of partition {}",
            found.index
        );
        assert_eq!(found.offset, want["start_sector"].as_u64().unwrap() * 512);
        assert_eq!(
            found.type_id,
            want["type"].as_str().unwrap(),
            "{name}: type of partition {}",
            found.index
        );
        assert!(
            found.letters.is_empty() && found.label.is_none(),
            "images have no mounted volumes"
        );

        if found.offset == volume["offset"].as_u64().unwrap() {
            // The partition that holds the (re-formatted) NTFS volume.
            ntfs_partitions += 1;
            assert_eq!(found.fs, Some(FsKind::Ntfs), "{name}");
            let ntfs = found
                .ntfs
                .as_ref()
                .expect("an NTFS partition has a geometry summary");
            assert_eq!(
                u64::from(ntfs.cluster_size),
                volume["cluster_size"].as_u64().unwrap()
            );
            assert_eq!(
                ntfs.total_sectors,
                volume["total_sectors"].as_u64().unwrap()
            );
            assert_eq!(ntfs.serial, volume["serial"].as_str().unwrap());
            // One sector less than the partition: the last sector is the backup boot sector.
            assert_eq!((ntfs.total_sectors + 1) * 512, found.size);
            check_boot_sectors(&image, found.offset, found.size);
        } else {
            assert_ne!(
                found.fs,
                Some(FsKind::Ntfs),
                "{name}: partition {} holds no NTFS",
                found.index
            );
            assert!(found.ntfs.is_none());
        }
    }
    assert_eq!(ntfs_partitions, 1, "{name}");
}

/// Reads the boot sector and its backup through a session range that is
/// exactly the partition: both are inside it and byte-identical, and the
/// sector after the partition is refused.
fn check_boot_sectors(image: &ImageFile, offset: u64, size: u64) {
    let shared: Arc<dyn BlockSource> = Arc::new(ImageFile::open(image.path(), None).unwrap());
    let ranged = RangedSource::new(shared, offset..offset + size).unwrap();
    let cancel = AtomicBool::new(false);
    let (mut primary, mut backup) = (AlignedBuf::new(512), AlignedBuf::new(512));
    assert!(read_resilient(&ranged, offset, &mut primary, &cancel).is_clean());
    assert!(read_resilient(&ranged, offset + size - 512, &mut backup, &cancel).is_clean());
    assert_eq!(&primary[3..11], b"NTFS    ");
    assert_eq!(
        &primary[..],
        &backup[..],
        "the backup boot sector is the last sector of the partition"
    );
    if offset + size < ranged.size() {
        let outcome = read_resilient(&ranged, offset + size, &mut backup, &cancel);
        assert!(
            matches!(outcome, hds_io::ReadOutcome::Internal(_)),
            "{outcome:?}"
        );
    }
    // The same bytes through the unaligned helper.
    let mut oem = [0u8; 8];
    assert!(read_unaligned(&ranged, offset + 3, &mut oem, &cancel).is_clean());
    assert_eq!(&oem, b"NTFS    ");
}

#[test]
fn mbr_with_the_partition_at_one_mebibyte() {
    check_partitioned("offset-mbr", TableKind::Mbr);
}

#[test]
fn mbr_with_the_partition_at_sector_63() {
    check_partitioned("offset-mbr63", TableKind::Mbr);
}

#[test]
fn gpt_with_a_reserved_partition_in_front() {
    check_partitioned("offset-gpt", TableKind::Gpt);
    // The type names of the two partitions Windows puts on a GPT data disk.
    let Some(Fixture { image, .. }) = fixture("offset-gpt") else {
        return;
    };
    let table = parse_partition_table(&image);
    let names: Vec<&str> = table
        .partitions
        .iter()
        .map(|p| p.type_name.as_str())
        .collect();
    assert_eq!(names, ["Microsoft reserved", "Basic data"]);
}

#[test]
fn bare_volumes_are_one_pseudo_partition() {
    for name in ["basic", "small-clusters", "big-clusters", "4k-records"] {
        let Some(Fixture { image, manifest }) = fixture(name) else {
            continue;
        };
        assert!(
            manifest["image"]["partition_table"].is_null(),
            "{name} has no partition table"
        );
        let table = parse_partition_table(&image);
        assert_eq!(table.table, TableKind::None, "{name}");
        assert_eq!(table.partitions.len(), 1, "{name}");
        let whole = &table.partitions[0];
        assert_eq!(
            (whole.index, whole.offset, whole.size),
            (0, 0, image.size()),
            "{name}"
        );
        assert_eq!(whole.fs, Some(FsKind::Ntfs), "{name}");
        let volume = &manifest["new_volume"];
        let ntfs = whole.ntfs.as_ref().unwrap();
        assert_eq!(
            u64::from(ntfs.cluster_size),
            volume["cluster_size"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(
            ntfs.total_sectors,
            volume["total_sectors"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(ntfs.serial, volume["serial"].as_str().unwrap(), "{name}");
        // The sector count is in the volume's own sector size (4096 for one fixture).
        let volume_sector = volume["sector_size"].as_u64().unwrap();
        assert_eq!(
            (ntfs.total_sectors + 1) * volume_sector,
            image.size(),
            "{name}"
        );
    }
}
