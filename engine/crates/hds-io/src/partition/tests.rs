use std::sync::Arc;

use super::*;
use crate::test_util::*;
use crate::testing::{FaultySource, MemSource, XorShift};

const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
const MSR: &str = "E3C9E316-0B5C-4DB8-817D-F92DF00215AE";
const LINUX: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";

trait DiskExt {
    fn source(&self) -> MemSource;
    fn parse(&self) -> PartitionTable;
}

impl DiskExt for Disk {
    fn source(&self) -> MemSource {
        MemSource::with_sector_sizes(self.data.clone(), self.sector as u32, self.sector as u32)
    }

    fn parse(&self) -> PartitionTable {
        parse_partition_table(&self.source())
    }
}

fn summary(p: &PartitionInfo) -> (u32, u64, u64, &str, Option<FsKind>) {
    (p.index, p.offset, p.size, p.type_id.as_str(), p.fs)
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[test]
fn crc32_matches_the_reference_value() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b""), 0);
    let mut data = vec![0u8; 5000];
    XorShift::new(5).fill(&mut data);
    assert_eq!(crc32(&data), crc32_bitwise(&data));
}

#[test]
fn a_bare_ntfs_volume_is_not_read_as_an_mbr() {
    let mut disk = Disk::new(4096, 512);
    let boot = ntfs_boot_sector(512, 8, 4095, 0xA1B2_C3D4_E5F6_0718);
    disk.put(0, &boot);
    disk.put(4095, &boot); // backup boot sector in the last sector
    let table = disk.parse();
    assert_eq!(table.table, TableKind::None);
    assert!(table.notes.is_empty());
    assert_eq!(
        table.partitions,
        vec![PartitionInfo {
            index: 0,
            offset: 0,
            size: 4096 * 512,
            type_name: String::new(),
            type_id: String::new(),
            fs: Some(FsKind::Ntfs),
            ntfs: Some(NtfsSummary {
                cluster_size: 4096,
                total_sectors: 4095,
                serial: "A1B2C3D4E5F60718".to_string()
            }),
            letters: Vec::new(),
            label: None,
        }]
    );
    // The same sector parsed as an MBR would have "partitions": its boot code
    // happens to contain entries with plausible status bytes.
    let mut tricky = boot;
    tricky[446..462].copy_from_slice(&[
        0x80, 1, 1, 0, 0x07, 0xFE, 0xFF, 0xFF, 0x3F, 0, 0, 0, 0x00, 0x08, 0, 0,
    ]);
    disk.put(0, &tricky);
    let table = disk.parse();
    assert_eq!(
        (
            table.table,
            table.partitions.len(),
            table.partitions[0].index
        ),
        (TableKind::None, 1, 0)
    );
}

#[test]
fn other_file_systems_in_sector_0_are_pseudo_partitions_too() {
    for (boot, kind) in [
        (fat32_boot_sector(), FsKind::Fat),
        (fat16_boot_sector(), FsKind::Fat),
        (exfat_boot_sector(), FsKind::Exfat),
        (refs_boot_sector(), FsKind::Refs),
        (bitlocker_boot_sector(), FsKind::Bitlocker),
    ] {
        let mut disk = Disk::new(256, 512);
        disk.put(0, &boot);
        let table = disk.parse();
        assert_eq!(table.table, TableKind::None, "{kind:?}");
        assert_eq!(table.partitions.len(), 1, "{kind:?}");
        let p = &table.partitions[0];
        assert_eq!(
            (p.index, p.offset, p.size, p.fs, &p.ntfs),
            (0, 0, 256 * 512, Some(kind), &None)
        );
    }
}

#[test]
fn ntfs_boot_sector_rules_follow_the_architecture() {
    let good = ntfs_boot_sector(512, 8, 1000, 1);
    let check = |edit: &dyn Fn(&mut [u8; 512])| {
        let mut s = good;
        edit(&mut s);
        sniff_boot_sector(&s)
    };
    assert_eq!(check(&|_| {}).unwrap().0, FsKind::Ntfs);
    // Sectors per cluster: powers of two up to 0x80, and the 2^(256 - v) form.
    for (raw, cluster) in [
        (1u8, 512u32),
        (2, 1024),
        (0x80, 65536),
        (0xF8, 128 * 1024),
        (0xF5, 1 << 20),
        (0xF4, 2 << 20),
    ] {
        let (kind, summary) = check(&|s| s[0x0D] = raw).unwrap();
        assert_eq!(
            (kind, summary.unwrap().cluster_size),
            (FsKind::Ntfs, cluster),
            "sectors per cluster {raw:#x}"
        );
    }
    for raw in [0u8, 3, 6, 0x81, 0xC0, 0xF3] {
        assert!(
            check(&|s| s[0x0D] = raw).is_none(),
            "sectors per cluster {raw:#x}"
        );
    }
    // 4096-byte sectors with 2 MiB clusters are the largest allowed; 4 MiB is not.
    let (_, s) = check(&|s| {
        s[0x0B..0x0D].copy_from_slice(&4096u16.to_le_bytes());
        s[0x0D] = 0xF7;
    })
    .unwrap();
    assert_eq!(s.unwrap().cluster_size, 2 << 20);
    assert!(check(&|s| {
        s[0x0B..0x0D].copy_from_slice(&4096u16.to_le_bytes());
        s[0x0D] = 0xF6;
    })
    .is_none());
    for bps in [0u16, 256, 513, 1000, 8192] {
        assert!(
            check(&|s| s[0x0B..0x0D].copy_from_slice(&bps.to_le_bytes())).is_none(),
            "bytes per sector {bps}"
        );
    }
    // Record and index block size: clusters, or 2^(-v) bytes with 9 <= -v <= 16.
    for raw in [1u8, 2, 0xF7, 0xF6, 0xF4, 0xF0] {
        assert!(check(&|s| s[0x40] = raw).is_some(), "record size {raw:#x}");
        assert!(check(&|s| s[0x44] = raw).is_some(), "index size {raw:#x}");
    }
    for raw in [0u8, 0xF8, 0xFF, 0xEF, 0x80] {
        assert!(check(&|s| s[0x40] = raw).is_none(), "record size {raw:#x}");
        assert!(check(&|s| s[0x44] = raw).is_none(), "index size {raw:#x}");
    }
    assert!(check(&|s| s[0x28..0x30].fill(0)).is_none(), "no sectors");
    assert!(
        check(&|s| s[0x28..0x30].fill(0xFF)).is_none(),
        "negative sector count"
    );
    assert!(check(&|s| s[510] = 0).is_none(), "no 55 AA");
    assert!(check(&|s| s[3] = b'M').is_none(), "wrong OEM id");
    assert!(sniff_boot_sector(&good[..511]).is_none(), "short sector");
    assert!(sniff_boot_sector(&[]).is_none());
}

#[test]
fn fat_needs_a_parameter_block_and_its_type_string() {
    assert_eq!(
        sniff_boot_sector(&fat32_boot_sector()).unwrap().0,
        FsKind::Fat
    );
    assert_eq!(
        sniff_boot_sector(&fat16_boot_sector()).unwrap().0,
        FsKind::Fat
    );
    let broken = |edit: &dyn Fn(&mut [u8; 512])| {
        let mut s = fat32_boot_sector();
        edit(&mut s);
        sniff_boot_sector(&s).is_none()
    };
    assert!(broken(&|s| s[82..90].fill(b' ')), "no type string");
    assert!(broken(&|s| s[0] = 0x33), "no jump");
    assert!(broken(&|s| s[13] = 3), "sectors per cluster");
    assert!(broken(&|s| s[16] = 0), "no FAT");
    assert!(broken(&|s| s[21] = 0x12), "media byte");
    assert!(broken(&|s| s[14..16].fill(0)), "no reserved sector");
    assert!(broken(&|s| s[32..36].fill(0)), "no sectors");
    assert!(broken(&|s| s[511] = 0), "no 55 AA");
    // An MBR is never mistaken for a file system.
    assert!(sniff_boot_sector(&boot_record(&[(0x80, 0x07, 2048, 1000)])).is_none());
    // exFAT must have zeros where FAT has its parameter block.
    let mut exfat = exfat_boot_sector();
    exfat[20] = 1;
    assert!(sniff_boot_sector(&exfat).is_none());
}

#[test]
fn mbr_partitions_are_listed_in_offset_order_with_sniffed_file_systems() {
    let mut disk = Disk::new(40_000, 512);
    // Table slots deliberately not in disk order.
    disk.put(
        0,
        &boot_record(&[
            (0x00, 0x0B, 20_000, 8_000),
            (0x80, 0x07, 2_048, 10_000),
            (0x00, 0x83, 30_000, 5_000),
            (0x00, 0x27, 12_048, 4_000),
        ]),
    );
    disk.put(
        2_048,
        &ntfs_boot_sector(512, 8, 9_999, 0x0123_4567_89AB_CDEF),
    );
    disk.put(20_000, &fat32_boot_sector());
    disk.put(30_000, &[0x53, 0xEF, 1, 2, 3]); // something that is not zero
    let table = disk.parse();
    assert_eq!(table.table, TableKind::Mbr);
    assert!(table.notes.is_empty(), "{:?}", table.notes);
    assert_eq!(
        table.partitions.iter().map(summary).collect::<Vec<_>>(),
        vec![
            (1, 2_048 * 512, 10_000 * 512, "07", Some(FsKind::Ntfs)),
            (2, 12_048 * 512, 4_000 * 512, "27", None),
            (3, 20_000 * 512, 8_000 * 512, "0B", Some(FsKind::Fat)),
            (4, 30_000 * 512, 5_000 * 512, "83", Some(FsKind::Other)),
        ]
    );
    assert_eq!(
        table
            .partitions
            .iter()
            .map(|p| p.type_name.as_str())
            .collect::<Vec<_>>(),
        vec!["NTFS / exFAT", "Windows recovery", "FAT32", "Linux"]
    );
    let ntfs = table.partitions[0].ntfs.as_ref().unwrap();
    assert_eq!(
        (ntfs.cluster_size, ntfs.total_sectors, ntfs.serial.as_str()),
        (4096, 9_999, "0123456789ABCDEF")
    );
    assert!(table.partitions[1..].iter().all(|p| p.ntfs.is_none()));
    assert_eq!(table.partitions[0].range(), 2_048 * 512..12_048 * 512);
    // Unknown type bytes get a generic name.
    disk.put(0, &boot_record(&[(0, 0xD7, 100, 100)]));
    assert_eq!(disk.parse().partitions[0].type_name, "Type D7");
}

/// A chain of logical drives: EBR k at `ext + 1000 * k`, drive right behind it.
fn disk_with_logical_drives(count: u32, container_type: u8) -> Disk {
    let ext = 10_000u32;
    let mut disk = Disk::new((ext + 1000 * (count + 1)) as usize, 512);
    disk.put(
        0,
        &boot_record(&[
            (0x80, 0x07, 2_048, 7_000),
            (0, container_type, ext, 1000 * count),
        ]),
    );
    for k in 0..count {
        let link = if k + 1 < count {
            (0, 0x05, 1000 * (k + 1), 1000)
        } else {
            (0, 0, 0, 0)
        };
        disk.put(
            u64::from(ext + 1000 * k),
            &boot_record(&[(0, 0x07, 63, 900), link]),
        );
    }
    disk
}

#[test]
fn logical_drives_are_found_through_the_ebr_chain() {
    for container_type in [0x05u8, 0x0F, 0x85] {
        let disk = disk_with_logical_drives(3, container_type);
        let table = disk.parse();
        assert_eq!(table.table, TableKind::Mbr);
        assert!(table.notes.is_empty(), "{:?}", table.notes);
        // The primary partition, then the three logical drives; the container is not listed.
        assert_eq!(
            table
                .partitions
                .iter()
                .map(|p| (p.index, p.offset / 512, p.size / 512))
                .collect::<Vec<_>>(),
            vec![
                (1, 2_048, 7_000),
                (2, 10_063, 900),
                (3, 11_063, 900),
                (4, 12_063, 900)
            ]
        );
        assert!(table.partitions.iter().all(|p| p.type_id == "07"));
    }
}

#[test]
fn a_looping_ebr_chain_ends() {
    let mut disk = disk_with_logical_drives(3, 0x0F);
    // The last link points back to the first extended boot record.
    disk.put(
        12_000,
        &boot_record(&[(0, 0x07, 63, 900), (0, 0x05, 0, 1000)]),
    );
    let table = disk.parse();
    assert_eq!(table.notes, vec![TableNote::EbrLoop]);
    assert_eq!(
        table.partitions.len(),
        4,
        "every drive is listed exactly once"
    );
    // A link to itself.
    disk.put(
        10_000,
        &boot_record(&[(0, 0x07, 63, 900), (0, 0x05, 0, 1000)]),
    );
    let table = disk.parse();
    assert_eq!(table.notes, vec![TableNote::EbrLoop]);
    assert_eq!(table.partitions.len(), 2);
}

#[test]
fn an_endless_ebr_chain_is_cut_and_a_broken_one_reported() {
    let disk = disk_with_logical_drives(300, 0x05);
    let table = disk.parse();
    assert_eq!(table.notes, vec![TableNote::EbrChainTooLong]);
    assert_eq!(table.partitions.len(), 1 + MAX_EBR_LINKS);

    // No signature in the second extended boot record.
    let mut disk = disk_with_logical_drives(3, 0x05);
    disk.put(11_000, &[0u8; 512]);
    let table = disk.parse();
    assert_eq!(table.notes, vec![TableNote::EbrBroken]);
    assert_eq!(table.partitions.len(), 2);
    // A link beyond the end of the disk.
    let mut disk = disk_with_logical_drives(2, 0x05);
    disk.put(
        11_000,
        &boot_record(&[(0, 0x07, 63, 900), (0, 0x05, 4_000_000_000, 1000)]),
    );
    let table = disk.parse();
    assert_eq!(table.notes, vec![TableNote::EbrBroken]);
    assert_eq!(table.partitions.len(), 3);
    // The first extended boot record is unreadable.
    let disk = disk_with_logical_drives(2, 0x05);
    let faulty = FaultySource::new(Arc::new(disk.source()));
    faulty.bad_sectors(10_000 * 512..10_001 * 512);
    let table = parse_partition_table(&faulty);
    assert_eq!(table.notes, vec![TableNote::EbrBroken]);
    assert_eq!(table.partitions.len(), 1);
}

#[test]
fn gpt_partitions_are_read_from_the_primary_table() {
    let mut disk = Disk::new(200_000, 512);
    disk.put_gpt(&[
        (MSR, 34, 32_767),
        (BASIC_DATA, 32_768, 163_839),
        (LINUX, 163_840, 199_000),
    ]);
    disk.put(
        32_768,
        &ntfs_boot_sector(512, 8, 131_071, 0xDEAD_BEEF_0000_0001),
    );
    let table = disk.parse();
    assert_eq!(table.table, TableKind::Gpt);
    assert!(table.notes.is_empty(), "{:?}", table.notes);
    assert_eq!(
        table.partitions.iter().map(summary).collect::<Vec<_>>(),
        vec![
            (1, 34 * 512, 32_734 * 512, MSR, None),
            (
                2,
                32_768 * 512,
                131_072 * 512,
                BASIC_DATA,
                Some(FsKind::Ntfs)
            ),
            (3, 163_840 * 512, 35_161 * 512, LINUX, None),
        ]
    );
    assert_eq!(
        table
            .partitions
            .iter()
            .map(|p| p.type_name.as_str())
            .collect::<Vec<_>>(),
        vec!["Microsoft reserved", "Basic data", "Linux"]
    );
    assert_eq!(
        table.partitions[1].ntfs.as_ref().unwrap().total_sectors,
        131_071
    );
    // An unknown type GUID is still listed.
    let mut disk = Disk::new(4096, 512);
    disk.put_gpt(&[("01234567-89AB-CDEF-0123-456789ABCDEF", 100, 199)]);
    let table = disk.parse();
    assert_eq!(
        table.partitions[0].type_id,
        "01234567-89AB-CDEF-0123-456789ABCDEF"
    );
    assert_eq!(table.partitions[0].type_name, "Unknown");
}

#[test]
fn a_damaged_primary_gpt_falls_back_to_the_backup() {
    let parts = [(MSR, 34, 2_047), (BASIC_DATA, 2_048, 60_000)];
    let mut pristine = Disk::new(65_536, 512);
    pristine.put_gpt(&parts);
    let expected = pristine.parse();
    assert_eq!(
        (expected.table, expected.partitions.len()),
        (TableKind::Gpt, 2)
    );

    // One flipped bit in the header: the CRC no longer matches.
    let mut disk = Disk {
        data: pristine.data.clone(),
        sector: 512,
    };
    disk.data[512 + 40] ^= 0x01;
    let table = disk.parse();
    assert_eq!(table.notes, vec![TableNote::GptBackupUsed]);
    assert_eq!(
        (table.table, &table.partitions),
        (TableKind::Gpt, &expected.partitions)
    );

    // The header is intact but an entry was changed.
    let mut disk = Disk {
        data: pristine.data.clone(),
        sector: 512,
    };
    disk.data[2 * 512 + 128 + 32] ^= 0x10;
    let table = disk.parse();
    assert_eq!(table.notes, vec![TableNote::GptBackupUsed]);
    assert_eq!(table.partitions, expected.partitions);

    // The whole first megabyte was wiped, protective MBR included.
    let mut disk = Disk {
        data: pristine.data.clone(),
        sector: 512,
    };
    disk.data[..1 << 20].fill(0);
    let table = disk.parse();
    assert_eq!(table.table, TableKind::Gpt);
    assert_eq!(
        table.notes,
        vec![TableNote::GptBackupUsed, TableNote::ProtectiveMbrMissing]
    );
    assert_eq!(table.partitions, expected.partitions);

    // The primary header sits in an unreadable sector.
    let faulty = FaultySource::new(Arc::new(pristine.source()));
    faulty.bad_sectors(512..1024);
    let table = parse_partition_table(&faulty);
    assert_eq!(table.notes, vec![TableNote::GptBackupUsed]);
    assert_eq!(table.partitions, expected.partitions);

    // Both copies damaged: the protective MBR is all that is left to show.
    let mut disk = Disk {
        data: pristine.data.clone(),
        sector: 512,
    };
    disk.data[512 + 40] ^= 0x01;
    let backup_header = disk.data.len() - 512;
    disk.data[backup_header + 40] ^= 0x01;
    let table = disk.parse();
    assert_eq!(table.table, TableKind::Mbr);
    assert_eq!(table.notes, vec![TableNote::GptInvalid]);
    // (Its first sector is the damaged GPT header: something, but no file system.)
    assert_eq!(
        table.partitions.iter().map(summary).collect::<Vec<_>>(),
        vec![(1, 512, 65_535 * 512, "EE", Some(FsKind::Other))]
    );
    assert_eq!(table.partitions[0].type_name, "GPT protective");
}

#[test]
fn the_backup_gpt_is_found_where_the_primary_says_it_is() {
    // A disk image that was copied to a larger file: the backup header is no longer
    // in the last sector, and the primary entries are damaged.
    let mut disk = Disk::new(8_192, 512);
    disk.put_gpt(&[(BASIC_DATA, 2_048, 8_000)]);
    let expected = disk.parse().partitions;
    disk.data.resize(16_384 * 512, 0);
    disk.data[2 * 512 + 40] ^= 0xFF;
    let table = disk.parse();
    assert_eq!(table.notes, vec![TableNote::GptBackupUsed]);
    assert_eq!(table.partitions, expected);
}

#[test]
fn a_gpt_on_4096_byte_sectors_is_read_with_4096_byte_units() {
    let mut disk = Disk::new(20_000, 4096);
    disk.put_gpt(&[(MSR, 6, 4_095), (BASIC_DATA, 4_096, 19_000)]);
    disk.put(
        4_096,
        &ntfs_boot_sector(4096, 1, 14_904, 0x4B4E_0000_0000_0001),
    );
    let table = disk.parse();
    assert_eq!(table.table, TableKind::Gpt);
    assert!(table.notes.is_empty(), "{:?}", table.notes);
    assert_eq!(
        table.partitions.iter().map(summary).collect::<Vec<_>>(),
        vec![
            (1, 6 * 4096, 4_090 * 4096, MSR, None),
            (
                2,
                4_096 * 4096,
                14_905 * 4096,
                BASIC_DATA,
                Some(FsKind::Ntfs)
            ),
        ]
    );
    assert_eq!(
        table.partitions[1].ntfs.as_ref().unwrap().cluster_size,
        4096
    );
    // Read as a disk with 512-byte sectors, the same bytes hold no valid table at
    // LBA 1: only the protective MBR remains.
    let as_512 = MemSource::new(disk.data.clone());
    let table = parse_partition_table(&as_512);
    assert_eq!(table.table, TableKind::Mbr);
    assert_eq!(table.notes, vec![TableNote::GptInvalid]);
    // The damaged-primary fallback works with 4096-byte sectors too.
    disk.data[4096 + 16] ^= 0xFF;
    let table = disk.parse();
    assert_eq!((table.table, table.partitions.len()), (TableKind::Gpt, 2));
    assert_eq!(table.notes, vec![TableNote::GptBackupUsed]);
}

#[test]
fn hostile_gpt_headers_are_refused_without_large_allocations() {
    let mut pristine = Disk::new(4_096, 512);
    pristine.put_gpt(&[(BASIC_DATA, 100, 4_000)]);
    let wreck = |edit: &dyn Fn(&mut [u8])| {
        let mut disk = Disk {
            data: pristine.data.clone(),
            sector: 512,
        };
        for header_at in [512usize, disk.data.len() - 512] {
            let header = &mut disk.data[header_at..header_at + 512];
            edit(header);
            let crc = crc32_bitwise(&{
                let mut h = header[..92].to_vec();
                h[16..20].fill(0);
                h
            });
            header[16..20].copy_from_slice(&crc.to_le_bytes());
        }
        disk.parse()
    };
    // Four billion entries of four gigabytes each, with valid header CRCs.
    let table = wreck(&|h| {
        h[80..84].copy_from_slice(&u32::MAX.to_le_bytes());
        h[84..88].copy_from_slice(&0xFFFF_FFF8u32.to_le_bytes());
    });
    assert_eq!(table.table, TableKind::Mbr);
    assert_eq!(
        table.notes,
        vec![TableNote::GptTooLarge, TableNote::GptInvalid]
    );
    // Entry array beyond the end of the disk, at LBA 0, at u64::MAX.
    for lba in [5_000u64, u64::MAX, u64::MAX / 512 + 1] {
        let table = wreck(&|h| h[72..80].copy_from_slice(&lba.to_le_bytes()));
        assert_eq!(
            (table.table, &table.notes),
            (TableKind::Mbr, &vec![TableNote::GptInvalid]),
            "entries at {lba}"
        );
    }
    // Entry size below the minimum or not a multiple of 8; header size out of range.
    for size in [0u32, 64, 127, 130] {
        let table = wreck(&|h| h[84..88].copy_from_slice(&size.to_le_bytes()));
        assert_eq!(table.table, TableKind::Mbr, "entry size {size}");
    }
    for header_size in [0u32, 91, 513, u32::MAX] {
        let mut disk = Disk {
            data: pristine.data.clone(),
            sector: 512,
        };
        disk.data[512 + 12..512 + 16].copy_from_slice(&header_size.to_le_bytes());
        let last = disk.data.len() - 512;
        disk.data[last + 12..last + 16].copy_from_slice(&header_size.to_le_bytes());
        assert_eq!(
            disk.parse().table,
            TableKind::Mbr,
            "header size {header_size}"
        );
    }
    // Entries with first > last, or reaching beyond u64, are left out.
    let mut disk = Disk {
        data: pristine.data.clone(),
        sector: 512,
    };
    disk.put_gpt(&[
        (BASIC_DATA, 500, 400),
        (BASIC_DATA, 100, u64::MAX),
        (BASIC_DATA, u64::MAX - 5, u64::MAX - 1),
    ]);
    let table = disk.parse();
    assert_eq!(table.table, TableKind::Gpt);
    assert!(table.partitions.is_empty(), "{:?}", table.partitions);
}

#[test]
fn partitions_are_clipped_to_the_source() {
    let mut disk = Disk::new(10_000, 512);
    disk.put(
        0,
        &boot_record(&[
            (0, 0x07, 2_048, 20_000),
            (0, 0x07, 50_000, 1_000),
            (0, 0x07, 100, 0),
        ]),
    );
    disk.put(2_048, &ntfs_boot_sector(512, 8, 19_999, 7));
    let table = disk.parse();
    assert_eq!(table.notes, vec![TableNote::PartitionBeyondEnd]);
    // The truncated partition is kept with what is left of it; the one that starts
    // beyond the end and the empty one are dropped.
    assert_eq!(
        table.partitions.iter().map(summary).collect::<Vec<_>>(),
        vec![(
            1,
            2_048 * 512,
            (10_000 - 2_048) * 512,
            "07",
            Some(FsKind::Ntfs)
        )]
    );
    // u32::MAX sectors on a 4096-byte-sector disk: no overflow.
    let mut disk = Disk::new(64, 4096);
    disk.put(
        0,
        &boot_record(&[(0, 0x07, u32::MAX, u32::MAX), (0, 0x07, 8, u32::MAX)]),
    );
    let table = disk.parse();
    assert_eq!(
        table.partitions.iter().map(summary).collect::<Vec<_>>(),
        vec![(1, 8 * 4096, 56 * 4096, "07", None)]
    );
}

#[test]
fn nothing_recognisable_is_no_table() {
    // All zeros.
    let table = Disk::new(64, 512).parse();
    assert_eq!(
        (table.table, table.partitions.len(), table.notes.len()),
        (TableKind::None, 0, 0)
    );
    // 55 AA with an empty table.
    let mut disk = Disk::new(64, 512);
    disk.put(0, &boot_record(&[]));
    let table = disk.parse();
    assert_eq!(
        (table.table, table.partitions.len(), table.notes.len()),
        (TableKind::None, 0, 0)
    );
    // 55 AA behind boot code whose "entries" have impossible status bytes.
    let mut junk = [0u8; 512];
    XorShift::new(77).fill(&mut junk);
    junk[446] = 0x41;
    junk[450] = 0x07;
    junk[458..462].copy_from_slice(&1000u32.to_le_bytes());
    junk[510] = 0x55;
    junk[511] = 0xAA;
    disk.put(0, &junk);
    let table = disk.parse();
    assert_eq!((table.table, table.partitions.len()), (TableKind::None, 0));
    assert_eq!(table.notes, vec![TableNote::MbrImplausible]);
    // Sector 0 unreadable and nothing else to go by.
    let faulty = FaultySource::new(Arc::new(Disk::new(64, 512).source()));
    faulty.bad_sectors(0..512);
    let table = parse_partition_table(&faulty);
    assert_eq!((table.table, table.partitions.len()), (TableKind::None, 0));
    assert_eq!(table.notes, vec![TableNote::Sector0Unreadable]);
    // A source smaller than a sector, and one with a nonsensical sector size.
    let tiny = MemSource::with_sector_sizes(vec![0u8; 100], 4096, 4096);
    assert_eq!(parse_partition_table(&tiny).table, TableKind::None);
    let odd = MemSource::with_sector_sizes(vec![0u8; 400], 100, 100);
    assert_eq!(parse_partition_table(&odd).table, TableKind::None);
    let huge = MemSource::with_sector_sizes(vec![0u8; 1 << 20], 1 << 20, 1 << 20);
    assert_eq!(parse_partition_table(&huge).table, TableKind::None);
}

#[test]
fn an_unreadable_first_sector_of_a_partition_gives_no_file_system() {
    let mut disk = Disk::new(8_192, 512);
    disk.put(0, &boot_record(&[(0x80, 0x07, 2_048, 4_000)]));
    disk.put(2_048, &ntfs_boot_sector(512, 8, 3_999, 9));
    let faulty = FaultySource::new(Arc::new(disk.source()));
    faulty.bad_sectors(2_048 * 512..2_049 * 512);
    let table = parse_partition_table(&faulty);
    assert_eq!(
        table.partitions.iter().map(summary).collect::<Vec<_>>(),
        vec![(1, 2_048 * 512, 4_000 * 512, "07", None)]
    );
    assert!(table.partitions[0].ntfs.is_none());
    // A corrupt NTFS boot sector is "other", not NTFS without geometry.
    let mut corrupt = ntfs_boot_sector(512, 8, 3_999, 9);
    corrupt[0x0D] = 3;
    disk.put(2_048, &corrupt);
    let p = &disk.parse().partitions[0];
    assert_eq!((p.fs, &p.ntfs), (Some(FsKind::Other), &None));
}

#[test]
fn partition_objects_match_the_protocol() {
    let p = PartitionInfo {
        index: 1,
        offset: 1_048_576,
        size: 4_000_785_104_896,
        type_name: "Basic data".to_string(),
        type_id: BASIC_DATA.to_string(),
        fs: Some(FsKind::Ntfs),
        ntfs: Some(NtfsSummary {
            cluster_size: 4096,
            total_sectors: 7_814_033_407,
            serial: "A1B2C3D4E5F60718".to_string(),
        }),
        letters: vec!["E:".to_string()],
        label: Some("New Volume".to_string()),
    };
    // The example of PROTOCOL.md, section 2.
    let expected: serde_json::Value = serde_json::from_str(
        r#"{"index": 1, "offset": 1048576, "size": 4000785104896,
            "type": "Basic data", "type_id": "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7", "fs": "ntfs",
            "ntfs": {"cluster_size": 4096, "total_sectors": 7814033407, "serial": "A1B2C3D4E5F60718"},
            "letters": ["E:"], "label": "New Volume"}"#,
    )
    .unwrap();
    assert_eq!(serde_json::to_value(&p).unwrap(), expected);
    assert_eq!(
        serde_json::from_value::<PartitionInfo>(expected).unwrap(),
        p
    );

    let table = PartitionTable {
        table: TableKind::Gpt,
        partitions: vec![p],
        notes: Vec::new(),
    };
    let json = serde_json::to_value(&table).unwrap();
    assert_eq!(json["table"], "gpt");
    assert_eq!(json["notes"], serde_json::json!([]));
    assert_eq!(
        serde_json::from_value::<PartitionTable>(json).unwrap(),
        table
    );
    let noted = PartitionTable {
        table: TableKind::None,
        partitions: Vec::new(),
        notes: vec![TableNote::EbrLoop],
    };
    assert_eq!(
        serde_json::to_string(&noted).unwrap(),
        r#"{"table":"none","partitions":[],"notes":["ebr_loop"]}"#
    );
    for (kind, text) in [
        (FsKind::Ntfs, "ntfs"),
        (FsKind::Fat, "fat"),
        (FsKind::Exfat, "exfat"),
        (FsKind::Refs, "refs"),
        (FsKind::Bitlocker, "bitlocker"),
        (FsKind::Other, "other"),
    ] {
        assert_eq!(serde_json::to_value(kind).unwrap(), text);
    }
    assert_eq!(serde_json::to_value(TableKind::Mbr).unwrap(), "mbr");
    assert_eq!(
        serde_json::to_value(None::<FsKind>).unwrap(),
        serde_json::Value::Null
    );
}

#[test]
fn arbitrary_and_mutated_tables_never_panic_or_hang() {
    let mut rng = XorShift::new(2026);
    // Valid layouts with a few bytes flipped in the structures that are parsed.
    let mut gpt = Disk::new(2_048, 512);
    gpt.put_gpt(&[(MSR, 34, 99), (BASIC_DATA, 100, 1_900)]);
    let mut mbr = disk_with_logical_drives(4, 0x0F);
    mbr.data.truncate(16_000 * 512);
    for round in 0..1_500 {
        let base = if round % 2 == 0 { &gpt } else { &mbr };
        let mut disk = Disk {
            data: base.data.clone(),
            sector: 512,
        };
        let hot_spots: &[std::ops::Range<usize>] = if round % 2 == 0 {
            &[446..1024, 1024..1024 + 512, 2_047 * 512..2_048 * 512]
        } else {
            &[
                446..512,
                10_000 * 512 + 446..10_001 * 512,
                11_000 * 512 + 446..11_001 * 512,
            ]
        };
        for _ in 0..=rng.below(6) {
            let spot = &hot_spots[rng.below(hot_spots.len() as u64) as usize];
            let at = spot.start + rng.below((spot.end - spot.start) as u64) as usize;
            disk.data[at] = rng.next_u64() as u8;
        }
        let table = disk.parse();
        assert!(table.partitions.len() <= 1 + MAX_EBR_LINKS + 128);
        for (i, p) in table.partitions.iter().enumerate() {
            assert!(p.size > 0 && p.offset < disk.data.len() as u64);
            assert!(
                p.offset + p.size <= disk.data.len() as u64,
                "partition {i} leaves the source"
            );
            assert_eq!(p.index as usize, i + 1);
            assert_eq!(p.ntfs.is_some(), p.fs == Some(FsKind::Ntfs));
        }
        assert!(table
            .partitions
            .windows(2)
            .all(|w| w[0].offset <= w[1].offset));
    }
    // Pure noise with the signatures in place.
    for _ in 0..300 {
        let mut disk = Disk::new(128, 512);
        rng.fill(&mut disk.data);
        disk.data[510] = 0x55;
        disk.data[511] = 0xAA;
        if rng.below(2) == 0 {
            disk.data[512..520].copy_from_slice(b"EFI PART");
        }
        let table = disk.parse();
        for p in &table.partitions {
            assert!(p.offset + p.size <= 128 * 512);
        }
    }
    // Noise handed to the sniffer directly, at every length.
    for len in 0..600 {
        let mut sector = vec![0u8; len];
        rng.fill(&mut sector);
        let _ = sniff_boot_sector(&sector);
        if len >= 16 {
            sector[3..11].copy_from_slice(b"NTFS    ");
            let _ = sniff_boot_sector(&sector);
        }
    }
}
