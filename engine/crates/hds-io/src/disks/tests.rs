use super::*;
use crate::partition::FsKind;
use crate::testing::MemSource;

fn example_disk() -> DiskInfo {
    DiskInfo {
        kind: SourceKind::Disk,
        path: r"\\.\PhysicalDrive1".to_string(),
        size: 4_000_787_030_016,
        sector_size: 512,
        physical_sector_size: 4096,
        model: Some("WDC WD40EZRZ-00GXCB0".to_string()),
        serial: Some("WD-WCC7K0000000".to_string()),
        disk_number: Some(1),
        number: 1,
        bus: "USB".to_string(),
        removable: false,
        is_system: false,
        media: MediaKind::Hdd,
        trim: Some(false),
        flags: Vec::new(),
        table: TableKind::Gpt,
        partitions: vec![PartitionInfo {
            index: 2,
            offset: 16_777_216,
            size: 4_000_769_204_224,
            type_name: String::new(),
            type_id: String::new(),
            fs: Some(FsKind::Ntfs),
            ntfs: None,
            letters: vec!["E:".to_string()],
            label: Some("New Volume".to_string()),
        }],
    }
}

#[test]
fn disk_objects_match_the_protocol() {
    let disk = example_disk();
    let json = serde_json::to_value(&disk).unwrap();
    // The SourceInfo members of PROTOCOL.md, section 2 ...
    let source: serde_json::Value = serde_json::from_str(
        r#"{"kind": "disk", "path": "\\\\.\\PhysicalDrive1", "size": 4000787030016,
            "sector_size": 512, "physical_sector_size": 4096,
            "model": "WDC WD40EZRZ-00GXCB0", "serial": "WD-WCC7K0000000", "disk_number": 1}"#,
    )
    .unwrap();
    for (key, value) in source.as_object().unwrap() {
        assert_eq!(&json[key], value, "member {key}");
    }
    assert_eq!(serde_json::to_value(disk.source_info()).unwrap(), source);
    // ... plus the members of "Disk".
    let rest: serde_json::Value = serde_json::from_str(
        r#"{"number": 1, "bus": "USB", "removable": false, "is_system": false,
            "media": "hdd", "trim": false, "flags": [], "table": "gpt"}"#,
    )
    .unwrap();
    for (key, value) in rest.as_object().unwrap() {
        assert_eq!(&json[key], value, "member {key}");
    }
    assert_eq!(json["partitions"][0]["letters"][0], "E:");
    assert_eq!(json["partitions"][0]["fs"], "ntfs");
    assert_eq!(
        json.as_object().unwrap().len(),
        17,
        "no member beyond the protocol's"
    );
    assert_eq!(serde_json::from_value::<DiskInfo>(json).unwrap(), disk);
}

#[test]
fn enumerations_have_the_wire_names_of_the_protocol() {
    for (media, text) in [
        (MediaKind::Hdd, "hdd"),
        (MediaKind::Ssd, "ssd"),
        (MediaKind::Unknown, "unknown"),
    ] {
        assert_eq!(serde_json::to_value(media).unwrap(), text);
    }
    let flags = vec![
        DiskFlag::Virtual,
        DiskFlag::Spaces,
        DiskFlag::Dynamic,
        DiskFlag::NoMedia,
    ];
    assert_eq!(
        serde_json::to_string(&flags).unwrap(),
        r#"["virtual","spaces","dynamic","no_media"]"#
    );
    let mut disk = example_disk();
    disk.trim = None;
    disk.media = MediaKind::Unknown;
    disk.table = TableKind::None;
    let json = serde_json::to_value(&disk).unwrap();
    assert_eq!(json["trim"], serde_json::Value::Null);
    assert_eq!(json["table"], "none");

    let warning = Warning {
        code: WARN_TRIM_SUPPORTED.to_string(),
        message: "The device accepts TRIM.".to_string(),
    };
    assert_eq!(
        serde_json::to_string(&warning).unwrap(),
        r#"{"code":"trim_supported","message":"The device accepts TRIM."}"#
    );
    // The codes this crate raises are codes of PROTOCOL.md.
    assert_eq!(
        [
            WARN_TRIM_SUPPORTED,
            WARN_MOUNTED_VOLUME,
            WARN_PAGE_FILE_ON_SOURCE,
            WARN_SYSTEM_DISK
        ],
        [
            "trim_supported",
            "mounted_volume",
            "page_file_on_source",
            "system_disk"
        ]
    );
}

#[test]
fn images_and_memory_sources_have_no_volumes_and_no_warnings() {
    let image = MemSource::new(vec![0u8; 1 << 20]);
    assert!(source_warnings(&image, 0..1 << 20).is_empty());
    let mut table = crate::parse_partition_table(&image);
    let before = table.clone();
    annotate_partitions(&mut table, &image);
    assert_eq!(table, before);
}

#[cfg(not(windows))]
#[test]
fn nothing_is_enumerated_outside_windows() {
    assert!(list_disks().is_empty());
    let disk =
        MemSource::new(vec![0u8; 4096]).at_location(crate::SourceLocation::Disk { number: 0 });
    assert!(source_warnings(&disk, 0..4096).is_empty());
}

#[cfg(windows)]
#[test]
fn the_disk_list_is_consistent_and_needs_no_elevation() {
    use crate::test_util::under_wine;
    use crate::SourceLocation;

    let disks = list_disks();
    assert!(!disks.is_empty(), "disk 0 exists on every Windows machine");
    let mut numbers = Vec::new();
    for disk in &disks {
        assert_eq!(disk.kind, SourceKind::Disk);
        assert_eq!(disk.path, format!(r"\\.\PhysicalDrive{}", disk.number));
        assert_eq!(disk.disk_number, Some(disk.number));
        assert!(
            !numbers.contains(&disk.number),
            "disk {} listed twice",
            disk.number
        );
        numbers.push(disk.number);
        assert!(disk.sector_size.is_power_of_two() && (512..=4096).contains(&disk.sector_size));
        assert!(
            disk.physical_sector_size >= disk.sector_size
                && disk.physical_sector_size.is_power_of_two()
        );
        assert!(!disk.bus.is_empty());
        if disk.flags.contains(&DiskFlag::NoMedia) {
            assert_eq!(disk.size, 0);
        }
        let mut previous_offset = 0;
        for partition in &disk.partitions {
            assert!(partition.index >= 1 && partition.size > 0);
            assert!(
                partition.offset >= previous_offset,
                "partitions are in offset order"
            );
            previous_offset = partition.offset;
            assert!(
                partition.offset + partition.size <= disk.size,
                "{partition:?} leaves disk {}",
                disk.number
            );
            // What is on the disk itself is not known without reading it.
            assert!(
                partition.type_name.is_empty()
                    && partition.type_id.is_empty()
                    && partition.ntfs.is_none()
            );
            for letter in &partition.letters {
                assert!(
                    letter.len() == 2
                        && letter.ends_with(':')
                        && letter.as_bytes()[0].is_ascii_uppercase()
                );
            }
        }
        let json = serde_json::to_string(disk).unwrap();
        assert_eq!(&serde_json::from_str::<DiskInfo>(&json).unwrap(), disk);
    }
    if under_wine() {
        // Wine has one dummy disk and reports no extents for its volumes.
        return;
    }

    // The disk Windows runs from, with the system drive among its partitions.
    let system_drive = std::env::var("SystemDrive")
        .unwrap_or_else(|_| "C:".to_string())
        .to_ascii_uppercase();
    let system: Vec<&DiskInfo> = disks.iter().filter(|d| d.is_system).collect();
    assert!(
        !system.is_empty(),
        "no disk is marked as the system disk: {disks:?}"
    );
    let (disk, partition) = system
        .iter()
        .find_map(|d| {
            d.partitions
                .iter()
                .find(|p| p.letters.contains(&system_drive))
                .map(|p| (*d, p))
        })
        .unwrap_or_else(|| panic!("{system_drive} is on none of the system disks: {system:?}"));
    assert_ne!(
        disk.table,
        TableKind::None,
        "Windows reports the partition style of an initialised disk"
    );
    assert_eq!(partition.fs, Some(FsKind::Ntfs), "Windows runs from NTFS");

    // Warnings: the whole disk holds the mounted system volume; its first sector does not.
    let source = MemSource::new(vec![0u8; 512]).at_location(SourceLocation::Disk {
        number: disk.number,
    });
    let codes = |range| {
        source_warnings(&source, range)
            .into_iter()
            .map(|w| w.code)
            .collect::<Vec<_>>()
    };
    let whole = codes(0..u64::MAX);
    assert!(
        whole.iter().any(|c| c == WARN_SYSTEM_DISK)
            && whole.iter().any(|c| c == WARN_MOUNTED_VOLUME),
        "{whole:?}"
    );
    let first_sector = codes(0..512);
    assert!(
        first_sector.iter().any(|c| c == WARN_SYSTEM_DISK),
        "{first_sector:?}"
    );
    assert!(
        !first_sector
            .iter()
            .any(|c| c == WARN_MOUNTED_VOLUME || c == WARN_PAGE_FILE_ON_SOURCE),
        "{first_sector:?}"
    );
    let exactly_the_volume = source_warnings(&source, partition.range());
    let mounted = exactly_the_volume
        .iter()
        .find(|w| w.code == WARN_MOUNTED_VOLUME)
        .expect("the system volume is mounted");
    assert!(
        mounted.message.contains(&system_drive),
        "{}",
        mounted.message
    );

    // A parsed table gets letters and label from the volume that starts where the partition does.
    let mut table = PartitionTable {
        table: disk.table,
        partitions: vec![
            PartitionInfo {
                letters: Vec::new(),
                label: None,
                ..partition.clone()
            },
            PartitionInfo {
                offset: partition.offset + 512,
                letters: Vec::new(),
                label: None,
                ..partition.clone()
            },
        ],
        notes: Vec::new(),
    };
    annotate_partitions(&mut table, &source);
    assert_eq!(table.partitions[0].letters, partition.letters);
    assert_eq!(table.partitions[0].label, partition.label);
    assert!(
        table.partitions[1].letters.is_empty(),
        "no volume starts 512 bytes later"
    );
}

#[cfg(windows)]
#[test]
fn partitions_are_derived_from_volume_extents() {
    use crate::guard::Extent;
    use crate::win::device::DeviceNumber;
    use crate::win::volumes::Volume;
    use std::ffi::OsString;

    let volume =
        |n: u8, extents: Vec<Extent>, partition: Option<u32>, disk: u32, mounts: &[&str]| Volume {
            // Devices that do not exist: label and file system stay unknown.
            device: OsString::from(format!(
                r"\\?\Volume{{00000000-0000-0000-0000-0000000000{n:02x}}}"
            )),
            extents: Some(extents),
            device_number: partition.map(|partition| DeviceNumber {
                device_type: 7,
                number: disk,
                partition,
            }),
            mount_paths: mounts.iter().map(|m| m.to_string()).collect(),
        };
    let extent = |disk, start, len| Extent { disk, start, len };
    let mib = 1u64 << 20;
    let volumes = vec![
        // Listed in mount-manager order, not in disk order.
        volume(
            0xa1,
            vec![extent(1, 600 * mib, 300 * mib)],
            Some(3),
            1,
            &["F:\\", "C:\\mnt\\data\\"],
        ),
        volume(
            0xa2,
            vec![extent(1, 16 * mib, 500 * mib)],
            Some(2),
            1,
            &["e:\\"],
        ),
        // Another disk.
        volume(0xa3, vec![extent(0, mib, 100 * mib)], Some(1), 0, &["C:\\"]),
        // A spanned volume: one extent on each disk, no partition number.
        volume(
            0xa4,
            vec![extent(1, 950 * mib, 10 * mib), extent(2, mib, 10 * mib)],
            None,
            0,
            &["S:\\"],
        ),
        // A volume whose extents are not known is on no disk.
        Volume {
            extents: None,
            ..volume(0xa5, Vec::new(), Some(9), 1, &["X:\\"])
        },
    ];
    let partitions = sys::partitions_of(1, &volumes);
    let summary: Vec<(u32, u64, u64, Vec<String>)> = partitions
        .iter()
        .map(|p| (p.index, p.offset / mib, p.size / mib, p.letters.clone()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (2, 16, 500, vec!["E:".to_string()]),
            (3, 600, 300, vec!["F:".to_string()]),
            // No partition number: the next number after the highest one.
            (4, 950, 10, vec!["S:".to_string()]),
        ]
    );
    assert!(partitions
        .iter()
        .all(|p| p.fs.is_none() && p.label.is_none() && p.ntfs.is_none()));
    assert!(partitions
        .iter()
        .all(|p| p.type_name.is_empty() && p.type_id.is_empty()));
    assert_eq!(sys::partitions_of(2, &volumes).len(), 1);
    assert!(sys::partitions_of(7, &volumes).is_empty());
}
