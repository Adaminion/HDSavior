use std::ffi::OsStr;

use super::*;
use crate::test_util::{under_wine, TempDir};
use crate::win::volumes::enumerate_volumes;

fn open(path: &str, access: Access) -> io::Result<DeviceHandle> {
    open_device(OsStr::new(path), access)
}

#[test]
fn the_closed_list_holds_the_six_documented_queries() {
    // Values from winioctl.h, as listed in docs/notes/windows-raw-io.md, section 4.
    let mut allowed = ALLOWED_CONTROL_CODES;
    allowed.sort_unstable();
    assert_eq!(
        allowed,
        [
            0x0007_00A0,
            0x0007_405C,
            0x002D_0800,
            0x002D_1080,
            0x002D_1400,
            0x0056_0000
        ]
    );
}

#[test]
fn control_codes_outside_the_list_never_reach_the_system() {
    let tmp = TempDir::new("win-control");
    let path = tmp.join("plain.bin");
    std::fs::write(&path, b"x").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    // Lock volume, dismount volume, update disk properties, set disk attributes,
    // volume offline, set reparse point, delete reparse point, set sparse, plus noise.
    for code in [
        0x0009_0018u32,
        0x0009_0020,
        0x0007_0140,
        0x0007_C0F4,
        0x0056_C00C,
        0x0009_00A4,
        0x0009_00AC,
        0x0009_00C4,
        0,
        u32::MAX,
    ] {
        let err = control(&file, code, &[], &mut [0u8; 16]).unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::PermissionDenied,
            "code {code:#x}"
        );
        assert!(
            err.raw_os_error().is_none(),
            "code {code:#x} was refused here, not by the system"
        );
        assert!(err.to_string().contains("not on the list"), "{err}");
    }
    // A listed code is passed on to the system. What a file system answers when it
    // is asked for a disk's length is its own business: only "refused here" is wrong.
    if let Err(err) = control(&file, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut [0u8; 8]) {
        assert!(err.raw_os_error().is_some(), "{err:?}");
    }
}

#[test]
fn only_disks_and_volume_devices_can_be_opened() {
    let tmp = TempDir::new("win-open");
    let file = tmp.join("file.bin");
    std::fs::write(&file, b"x").unwrap();
    let volume = enumerate_volumes()
        .into_iter()
        .next()
        .map(|v| v.device.to_string_lossy().into_owned());
    let mut refused = vec![
        file.to_string_lossy().into_owned(),
        tmp.path().to_string_lossy().into_owned(),
        r"C:\".to_string(),
        r"\\.\C:".to_string(),
        r"\\.\PhysicalDrive0\".to_string(),
        r"\\.\PhysicalDrive0\x".to_string(),
        r"\\.\Harddisk0Partition1".to_string(),
        r"\\.\NUL".to_string(),
        r"\\?\GLOBALROOT\Device\Harddisk0\DR0".to_string(),
        String::new(),
    ];
    if let Some(volume) = &volume {
        refused.push(format!(r"{volume}\"));
        refused.push(format!(r"{volume}\Windows"));
    }
    for path in &refused {
        for access in [Access::Query, Access::Read] {
            let err = open(path, access).expect_err(path);
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{path:?}: {err}");
        }
    }
    // Volumes are never opened for reading.
    if let Some(volume) = &volume {
        assert_eq!(
            open(volume, Access::Read).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let handle = open(volume, Access::Query).unwrap();
        assert_eq!(handle.access(), Access::Query);
    }
    // A disk number nobody has.
    let err = open(r"\\.\PhysicalDrive9876", Access::Query).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}

/// Disk 0 exists on every Windows machine (and as a dummy under Wine): a
/// query handle opens without elevation and answers the queries that need
/// no access right.
#[test]
fn disk_0_answers_a_query_handle() {
    let disk = open(r"\\.\PhysicalDrive0", Access::Query).unwrap();
    let number = disk.device_number().unwrap();
    assert_eq!(
        (number.device_type, number.number, number.partition),
        (DEVICE_TYPE_DISK, 0, 0)
    );
    let geometry = disk.geometry().unwrap();
    assert!(
        crate::source::is_valid_sector_size(geometry.bytes_per_sector),
        "{geometry:?}"
    );
    assert!(geometry.disk_size > 0);
    let (logical, physical) = disk.sector_sizes().unwrap();
    assert!(
        crate::source::is_valid_sector_size(logical)
            && physical >= logical
            && physical.is_power_of_two()
    );
    let identity = disk.identity().unwrap();
    assert!(!identity.bus_name().is_empty());
    // The length query needs read access: on a query handle it is refused by the system.
    let err = disk.length().unwrap_err();
    assert_eq!(err.raw_os_error(), Some(5), "{err}");
    // These may or may not be supported, but must not fail in any other way.
    let _ = (
        disk.seek_penalty(),
        disk.trim_enabled(),
        disk.check_verify(),
        disk.access_alignment(),
    );
    if !under_wine() {
        assert!(
            geometry.partition_style.is_some(),
            "Windows appends the partition style to the geometry"
        );
        disk.check_verify().unwrap();
    }
}

#[test]
fn volumes_report_extents_or_nothing_but_never_garbage() {
    for volume in enumerate_volumes() {
        let Ok(handle) = open_device(&volume.device, Access::Query) else {
            continue;
        };
        if let Ok(extents) = handle.volume_disk_extents() {
            for extent in &extents {
                assert!(extent.len > 0, "{extent:?}");
                assert!(extent.start.checked_add(extent.len).is_some());
            }
            assert_eq!(volume.extents.is_some(), !extents.is_empty());
        }
        // On a volume that is not on a virtual disk this is an empty list or an error.
        let _ = handle.storage_dependencies();
        let _ = handle.device_number();
    }
}

#[test]
fn identity_strings_are_cleaned_up() {
    let identity = DeviceIdentity {
        vendor: "  ".to_string(),
        product: "WDC  WD40EZRZ-00GXCB0 ".to_string(),
        revision: String::new(),
        serial: "x".to_string(),
        bus_type: 11,
        removable_media: false,
    };
    assert_eq!(identity.model().as_deref(), Some("WDC WD40EZRZ-00GXCB0"));
    assert_eq!(identity.bus_name(), "SATA");
    let unnamed = DeviceIdentity {
        vendor: String::new(),
        product: " ".to_string(),
        bus_type: 99,
        ..identity.clone()
    };
    assert_eq!(unnamed.model(), None);
    assert_eq!(unnamed.bus_name(), "Unknown");
    for (bus, name) in [
        (7, "USB"),
        (17, "NVMe"),
        (15, "FileBackedVirtual"),
        (16, "Spaces"),
        (1, "SCSI"),
    ] {
        assert_eq!(
            DeviceIdentity {
                bus_type: bus,
                ..identity.clone()
            }
            .bus_name(),
            name
        );
    }
    // Strings of a property answer: offset 0 means "none", garbage is neutralised.
    let mut answer = vec![0u8; 64];
    answer[40..48].copy_from_slice(b" ABC\x01D \0");
    assert_eq!(ascii_at(&answer, 40), "ABC?D");
    assert_eq!(ascii_at(&answer, 0), "");
    assert_eq!(ascii_at(&answer, 64), "");
    assert_eq!(ascii_at(&answer, u32::MAX), "");
    answer[60..64].copy_from_slice(b"TAIL");
    assert_eq!(
        ascii_at(&answer, 60),
        "TAIL",
        "a string without terminator ends with the answer"
    );
}
