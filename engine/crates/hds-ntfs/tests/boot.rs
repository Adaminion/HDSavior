//! The boot sector: every field and every rule of ARCHITECTURE.md 5.1 and
//! docs/notes/ntfs-on-disk.md section 1, on hand-built sectors.

mod common;

use common::build::Boot;
use hds_ntfs::{is_boot_sector_candidate, parse_boot_sector, BootSector, Error};

/// A volume large enough that every cluster size up to 2 MiB leaves the
/// MFT locations inside it, with record and index sizes given in bytes
/// (the negative form) so that they do not depend on the cluster size.
fn roomy() -> Boot {
    Boot {
        total_sectors: 1 << 40,
        mft_lcn: 4,
        mftmirr_lcn: 2,
        record_size_raw: 0xF6,
        index_size_raw: 0xF4,
        ..Boot::default()
    }
}

#[test]
fn a_windows_boot_sector_decodes_field_by_field() {
    let sector = Boot::default().build();
    assert!(is_boot_sector_candidate(&sector));
    assert_eq!(
        parse_boot_sector(&sector),
        Ok(BootSector {
            bytes_per_sector: 512,
            sectors_per_cluster: 8,
            cluster_size: 4096,
            total_sectors: 2_097_151,
            hidden_sectors: 2048,
            mft_lcn: 87_381,
            mftmirr_lcn: 2,
            mft_record_size: 1024,
            index_block_size: 4096,
            serial: 0xA1B2_C3D4_E5F6_0718,
        })
    );
}

#[test]
fn derived_values() {
    let boot = parse_boot_sector(&Boot::default().build()).unwrap();
    // One sector less than the 1 GiB partition; the backup boot sector is
    // the sector right behind the volume.
    assert_eq!(boot.volume_size(), 2_097_151 * 512);
    assert_eq!(boot.total_clusters(), 262_143);
    assert_eq!(boot.mft_offset(), 87_381 * 4096);
    assert_eq!(boot.mftmirr_offset(), 8192);
}

#[test]
fn only_the_first_512_bytes_count() {
    let mut sector = Boot::default().build();
    let expected = parse_boot_sector(&sector).unwrap();
    sector.resize(4096, 0xEE);
    assert_eq!(parse_boot_sector(&sector), Ok(expected));
    assert_eq!(parse_boot_sector(&sector[..511]), Err(Error::TooShort));
    assert!(!is_boot_sector_candidate(&sector[..511]));
    assert_eq!(parse_boot_sector(&[]), Err(Error::TooShort));
}

#[test]
fn bytes_the_format_does_not_define_are_ignored() {
    let plain = Boot::default().build();
    let expected = parse_boot_sector(&plain).unwrap();
    let mut noisy = plain.clone();
    // Jump, BPB leftovers, the three unused bytes behind each size byte,
    // the checksum and the boot code.
    for range in [
        0x00..0x03,
        0x0E..0x1C,
        0x20..0x28,
        0x41..0x44,
        0x45..0x48,
        0x50..0x1FE,
    ] {
        noisy[range].fill(0xFF);
    }
    assert_eq!(parse_boot_sector(&noisy), Ok(expected));
}

#[test]
fn both_signatures_are_required() {
    let good = Boot::default().build();
    for position in 0x03..0x0B {
        let mut sector = good.clone();
        sector[position] ^= 0x20;
        assert_eq!(
            parse_boot_sector(&sector),
            Err(Error::BadMagic),
            "OEM id byte {position:#x}"
        );
        assert!(!is_boot_sector_candidate(&sector));
    }
    for position in [0x1FE, 0x1FF] {
        let mut sector = good.clone();
        sector[position] = 0;
        assert_eq!(parse_boot_sector(&sector), Err(Error::BadMagic));
        assert!(!is_boot_sector_candidate(&sector));
    }
    // The trailer is 55 AA in this order.
    let mut swapped = good.clone();
    swapped[0x1FE] = 0xAA;
    swapped[0x1FF] = 0x55;
    assert_eq!(parse_boot_sector(&swapped), Err(Error::BadMagic));
    assert_eq!(parse_boot_sector(&[0u8; 512]), Err(Error::BadMagic));
}

#[test]
fn bytes_per_sector_is_a_power_of_two_from_512_to_4096() {
    for (value, valid) in [
        (0u16, false),
        (256, false),
        (511, false),
        (512, true),
        (513, false),
        (1024, true),
        (1536, false),
        (2048, true),
        (4096, true),
        (8192, false),
        (0xFFFF, false),
    ] {
        let result = parse_boot_sector(
            &Boot {
                bytes_per_sector: value,
                sectors_per_cluster_raw: 1,
                ..roomy()
            }
            .build(),
        );
        match result {
            Ok(boot) => {
                assert!(valid, "{value} accepted");
                assert_eq!(boot.bytes_per_sector, u32::from(value));
                assert_eq!(boot.cluster_size, u32::from(value));
            }
            Err(error) => {
                assert!(!valid, "{value} refused");
                assert_eq!(error, Error::BadBytesPerSector);
            }
        }
    }
}

#[test]
fn sectors_per_cluster_every_byte_value() {
    for raw in 0..=255u8 {
        let expected: Option<u32> = match raw {
            1 | 2 | 4 | 8 | 16 | 32 | 64 | 128 => Some(u32::from(raw)),
            // 2^(256 - raw): 0xFF = 2 sectors ... 0xF4 = 4096 sectors.
            0xF4 => Some(4096),
            0xF5 => Some(2048),
            0xF6 => Some(1024),
            0xF7 => Some(512),
            0xF8 => Some(256),
            0xF9 => Some(128),
            0xFA => Some(64),
            0xFB => Some(32),
            0xFC => Some(16),
            0xFD => Some(8),
            0xFE => Some(4),
            0xFF => Some(2),
            _ => None,
        };
        // 512-byte sectors: even 4096 sectors are a legal 2 MiB cluster.
        let result = parse_boot_sector(
            &Boot {
                sectors_per_cluster_raw: raw,
                ..roomy()
            }
            .build(),
        );
        match expected {
            Some(sectors) => {
                let boot = result.unwrap_or_else(|e| panic!("raw {raw:#04x} refused: {e}"));
                assert_eq!(boot.sectors_per_cluster, sectors, "raw {raw:#04x}");
                assert_eq!(boot.cluster_size, sectors * 512, "raw {raw:#04x}");
            }
            None => assert_eq!(result, Err(Error::BadSectorsPerCluster), "raw {raw:#04x}"),
        }
    }
}

#[test]
fn cluster_sizes_seen_on_real_volumes() {
    // (bytes per sector, raw byte, cluster size): docs/notes, section 1.
    for (bytes_per_sector, raw, cluster) in [
        (512u16, 0x01u8, 512u32),
        (512, 0x08, 4096),
        (512, 0x80, 65_536),
        (512, 0xF8, 128 * 1024),
        (512, 0xF5, 1024 * 1024),
        (512, 0xF4, 2 * 1024 * 1024),
        (4096, 0x01, 4096),
        (4096, 0x20, 128 * 1024),
        (4096, 0xF7, 2 * 1024 * 1024),
    ] {
        let boot = parse_boot_sector(
            &Boot {
                bytes_per_sector,
                sectors_per_cluster_raw: raw,
                ..roomy()
            }
            .build(),
        )
        .unwrap();
        assert_eq!(
            boot.cluster_size, cluster,
            "{bytes_per_sector} / {raw:#04x}"
        );
    }
}

#[test]
fn a_cluster_is_at_most_2_mib() {
    for (bytes_per_sector, raw, valid) in [
        (512u16, 0xF4u8, true), // 4096 sectors: 2 MiB
        (1024, 0xF5, true),     // 2048 sectors: 2 MiB
        (1024, 0xF4, false),    // 4 MiB
        (2048, 0xF6, true),     // 1024 sectors: 2 MiB
        (2048, 0xF5, false),    // 4 MiB
        (4096, 0xF7, true),     // 512 sectors: 2 MiB
        (4096, 0xF6, false),    // 4 MiB
        (4096, 0xF4, false),    // 16 MiB
        (4096, 0x80, true),     // 512 KiB
    ] {
        let result = parse_boot_sector(
            &Boot {
                bytes_per_sector,
                sectors_per_cluster_raw: raw,
                ..roomy()
            }
            .build(),
        );
        if valid {
            assert!(result.unwrap().cluster_size <= 2 * 1024 * 1024);
        } else {
            assert_eq!(
                result,
                Err(Error::BadClusterSize),
                "{bytes_per_sector} / {raw:#04x}"
            );
        }
    }
}

/// What a size byte must decode to with the given cluster size.
fn expected_size(raw: u8, cluster: u32) -> Option<u32> {
    let value = raw as i8;
    let size = if value > 0 {
        u64::from(cluster) * value as u64
    } else if (-16..=-9).contains(&value) {
        1u64 << -value
    } else {
        return None;
    };
    (size.is_power_of_two() && (512..=65_536).contains(&size)).then_some(size as u32)
}

#[test]
fn record_and_index_size_every_byte_value() {
    // (sectors per cluster raw, cluster size)
    for (cluster_raw, cluster) in [
        (1u8, 512u32),
        (2, 1024),
        (8, 4096),
        (0x80, 65_536),
        (0xF4, 2 << 20),
    ] {
        for raw in 0..=255u8 {
            let expected = expected_size(raw, cluster);
            let as_record = parse_boot_sector(
                &Boot {
                    sectors_per_cluster_raw: cluster_raw,
                    record_size_raw: raw,
                    index_size_raw: 0xF4,
                    ..roomy()
                }
                .build(),
            );
            let as_index = parse_boot_sector(
                &Boot {
                    sectors_per_cluster_raw: cluster_raw,
                    record_size_raw: 0xF6,
                    index_size_raw: raw,
                    ..roomy()
                }
                .build(),
            );
            match expected {
                Some(size) => {
                    assert_eq!(
                        as_record.unwrap().mft_record_size,
                        size,
                        "record {raw:#04x} at {cluster}"
                    );
                    assert_eq!(
                        as_index.unwrap().index_block_size,
                        size,
                        "index {raw:#04x} at {cluster}"
                    );
                }
                None => {
                    assert_eq!(
                        as_record,
                        Err(Error::BadRecordSize),
                        "record {raw:#04x} at {cluster}"
                    );
                    assert_eq!(
                        as_index,
                        Err(Error::BadIndexBlockSize),
                        "index {raw:#04x} at {cluster}"
                    );
                }
            }
        }
    }
}

#[test]
fn size_bytes_seen_on_real_volumes() {
    // docs/notes, section 1: what Windows and mkntfs write.
    assert_eq!(expected_size(0x02, 512), Some(1024)); // record, 512-byte clusters
    assert_eq!(expected_size(0x08, 512), Some(4096)); // index, 512-byte clusters
    assert_eq!(expected_size(0x01, 1024), Some(1024));
    assert_eq!(expected_size(0x04, 1024), Some(4096));
    assert_eq!(expected_size(0xF6, 4096), Some(1024)); // -10
    assert_eq!(expected_size(0x01, 4096), Some(4096));
    assert_eq!(expected_size(0xF4, 65_536), Some(4096)); // -12
                                                         // The bounds of the negative form: 2^9 and 2^16.
    assert_eq!(expected_size(0xF7, 4096), Some(512));
    assert_eq!(expected_size(0xF0, 4096), Some(65_536));
    assert_eq!(expected_size(0xF8, 4096), None); // 2^8
    assert_eq!(expected_size(0xEF, 4096), None); // 2^17
    assert_eq!(expected_size(0x00, 4096), None);
    assert_eq!(expected_size(0x80, 4096), None); // -128
                                                 // Positive values count clusters; the product must still be a size.
    assert_eq!(expected_size(0x03, 512), None); // 1536
    assert_eq!(expected_size(0x10, 4096), Some(65_536));
    assert_eq!(expected_size(0x20, 4096), None); // 128 KiB
    assert_eq!(expected_size(0x01, 2 << 20), None); // one 2 MiB cluster

    let boot = parse_boot_sector(
        &Boot {
            sectors_per_cluster_raw: 1,
            record_size_raw: 0x02,
            index_size_raw: 0x08,
            ..roomy()
        }
        .build(),
    )
    .unwrap();
    assert_eq!((boot.mft_record_size, boot.index_block_size), (1024, 4096));
}

#[test]
fn the_sector_count_must_be_positive_and_the_volume_below_two_to_the_63_bytes() {
    let largest = i64::MAX / 512;
    for (total, valid) in [
        (i64::MIN, false),
        (-1, false),
        (0, false),
        (largest, true),
        (largest + 1, false),
        (i64::MAX, false),
    ] {
        let result = parse_boot_sector(
            &Boot {
                total_sectors: total,
                mft_lcn: 0,
                mftmirr_lcn: 0,
                sectors_per_cluster_raw: 1,
                ..Boot::default()
            }
            .build(),
        );
        if valid {
            let boot = result.unwrap();
            assert_eq!(boot.total_sectors, total as u64);
            assert_eq!(boot.volume_size(), total as u64 * 512);
            assert!(boot.volume_size() <= i64::MAX as u64);
        } else {
            assert_eq!(result, Err(Error::BadTotalSectors), "{total}");
        }
    }
    // With 4096-byte sectors the limit is eight times lower.
    let result = parse_boot_sector(
        &Boot {
            bytes_per_sector: 4096,
            sectors_per_cluster_raw: 1,
            total_sectors: largest,
            record_size_raw: 0xF4,
            ..Boot::default()
        }
        .build(),
    );
    assert_eq!(result, Err(Error::BadTotalSectors));
}

#[test]
fn the_mft_and_its_mirror_start_inside_the_volume() {
    // 2 097 151 sectors of 512 bytes in clusters of 8 sectors.
    let clusters = 262_143i64;
    for (lcn, valid) in [
        (0, true),
        (clusters - 1, true),
        (clusters, false),
        (clusters + 1, false),
        (-1, false),
        (i64::MIN, false),
        (i64::MAX, false),
    ] {
        let as_mft = parse_boot_sector(
            &Boot {
                mft_lcn: lcn,
                ..Boot::default()
            }
            .build(),
        );
        let as_mirror = parse_boot_sector(
            &Boot {
                mftmirr_lcn: lcn,
                ..Boot::default()
            }
            .build(),
        );
        if valid {
            let boot = as_mft.unwrap();
            assert_eq!(boot.mft_lcn, lcn as u64);
            assert!(boot.mft_offset() < boot.volume_size());
            assert_eq!(as_mirror.unwrap().mftmirr_lcn, lcn as u64);
        } else {
            assert_eq!(as_mft, Err(Error::BadMftLcn), "mft {lcn}");
            assert_eq!(as_mirror, Err(Error::BadMftLcn), "mirror {lcn}");
        }
    }
    // A volume smaller than one cluster has no place for an MFT.
    let tiny = parse_boot_sector(
        &Boot {
            total_sectors: 7,
            mft_lcn: 0,
            mftmirr_lcn: 0,
            ..Boot::default()
        }
        .build(),
    );
    assert_eq!(tiny, Err(Error::BadMftLcn));
}

#[test]
fn the_first_failing_rule_is_reported() {
    // Everything wrong at once: the checks run in the documented order.
    let mut boot = Boot {
        bytes_per_sector: 100,
        sectors_per_cluster_raw: 3,
        record_size_raw: 0,
        index_size_raw: 0,
        total_sectors: 0,
        mft_lcn: -1,
        mftmirr_lcn: -1,
        ..Boot::default()
    };
    assert_eq!(
        parse_boot_sector(&boot.build()),
        Err(Error::BadBytesPerSector)
    );
    boot.bytes_per_sector = 512;
    assert_eq!(
        parse_boot_sector(&boot.build()),
        Err(Error::BadSectorsPerCluster)
    );
    boot.sectors_per_cluster_raw = 8;
    assert_eq!(parse_boot_sector(&boot.build()), Err(Error::BadRecordSize));
    boot.record_size_raw = 0xF6;
    assert_eq!(
        parse_boot_sector(&boot.build()),
        Err(Error::BadIndexBlockSize)
    );
    boot.index_size_raw = 1;
    assert_eq!(
        parse_boot_sector(&boot.build()),
        Err(Error::BadTotalSectors)
    );
    boot.total_sectors = 2_097_151;
    assert_eq!(parse_boot_sector(&boot.build()), Err(Error::BadMftLcn));
    boot.mft_lcn = 4;
    boot.mftmirr_lcn = 2;
    assert!(parse_boot_sector(&boot.build()).is_ok());
}

#[test]
fn hidden_sectors_and_serial_are_taken_as_they_are() {
    let boot = parse_boot_sector(
        &Boot {
            hidden_sectors: 0xFFFF_FFFF,
            serial: u64::MAX,
            ..Boot::default()
        }
        .build(),
    )
    .unwrap();
    assert_eq!(boot.hidden_sectors, 0xFFFF_FFFF);
    assert_eq!(boot.serial, u64::MAX);
    let boot = parse_boot_sector(
        &Boot {
            hidden_sectors: 63,
            serial: 0,
            ..Boot::default()
        }
        .build(),
    )
    .unwrap();
    assert_eq!((boot.hidden_sectors, boot.serial), (63, 0));
}

#[test]
fn helpers_do_not_overflow_on_values_built_by_hand() {
    // The fields are public: a value that did not come from the parser
    // must not make the helpers panic.
    let odd = BootSector {
        bytes_per_sector: u32::MAX,
        sectors_per_cluster: 0,
        cluster_size: u32::MAX,
        total_sectors: u64::MAX,
        hidden_sectors: 0,
        mft_lcn: u64::MAX,
        mftmirr_lcn: u64::MAX,
        mft_record_size: 0,
        index_block_size: 0,
        serial: 0,
    };
    assert_eq!(odd.volume_size(), u64::MAX);
    assert_eq!(odd.total_clusters(), 0);
    assert_eq!(odd.mft_offset(), u64::MAX);
    assert_eq!(odd.mftmirr_offset(), u64::MAX);
}
