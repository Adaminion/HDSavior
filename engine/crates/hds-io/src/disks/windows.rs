//! Windows side of the disk list and of the source warnings. Query handles
//! only: nothing here reads a sector or needs elevation.

use std::ffi::OsStr;
use std::ops::Range;

use super::{
    DiskFlag, DiskInfo, MediaKind, Warning, WARN_MOUNTED_VOLUME, WARN_PAGE_FILE_ON_SOURCE,
    WARN_SYSTEM_DISK, WARN_TRIM_SUPPORTED,
};
use crate::partition::{FsKind, PartitionInfo, PartitionTable, TableKind};
use crate::source::{BlockSource, SourceKind, SourceLocation};
use crate::win::device::{open_device, Access, PartitionStyle, DEVICE_TYPE_DISK};
use crate::win::volumes::{
    enumerate_volumes, page_file_letters, system_disks, volume_info_by_root, Volume,
};
use crate::winpath::physical_drive_path;

/// Disk numbers 0 to 63 are probed; numbers can have gaps.
const DISK_NUMBERS: Range<u32> = 0..64;

/// `ERROR_NOT_READY`: a drive without a medium.
const ERROR_NOT_READY: i32 = 21;

/// `FVE_E_LOCKED_VOLUME`: the volume is locked by BitLocker.
const FVE_E_LOCKED_VOLUME: i32 = 0x8031_0000_u32 as i32;

/// File system and label of a mounted volume. A raw volume has neither.
fn fs_and_label(volume: &Volume) -> (Option<FsKind>, Option<String>) {
    match volume_info_by_root(&volume.root()) {
        Ok(info) => {
            let fs = match info.fs_name.to_ascii_uppercase().as_str() {
                "NTFS" => FsKind::Ntfs,
                "FAT" | "FAT12" | "FAT16" | "FAT32" => FsKind::Fat,
                "EXFAT" => FsKind::Exfat,
                "REFS" => FsKind::Refs,
                _ => FsKind::Other,
            };
            (Some(fs), Some(info.label).filter(|label| !label.is_empty()))
        }
        Err(e) if e.raw_os_error() == Some(FVE_E_LOCKED_VOLUME) => (Some(FsKind::Bitlocker), None),
        Err(_) => (None, None),
    }
}

fn on_disk(volume: &Volume, disk: u32) -> bool {
    volume
        .extents
        .as_ref()
        .is_some_and(|extents| extents.iter().any(|e| e.disk == disk))
}

/// The partitions of `disk` that carry a volume, in offset order.
pub(super) fn partitions_of(disk: u32, volumes: &[Volume]) -> Vec<PartitionInfo> {
    let mut partitions = Vec::new();
    for volume in volumes.iter().filter(|v| on_disk(v, disk)) {
        let extents = volume.extents.as_deref().unwrap_or_default();
        let (fs, label) = fs_and_label(volume);
        // The partition number is known for a volume that is one partition of a basic disk.
        let number = volume
            .device_number
            .filter(|n| {
                extents.len() == 1
                    && n.device_type == DEVICE_TYPE_DISK
                    && n.number == disk
                    && n.partition != u32::MAX
            })
            .map(|n| n.partition);
        for extent in extents.iter().filter(|e| e.disk == disk) {
            partitions.push(PartitionInfo {
                index: number.unwrap_or(0),
                offset: extent.start,
                size: extent.len,
                type_name: String::new(),
                type_id: String::new(),
                fs,
                ntfs: None,
                letters: volume.letters(),
                label: label.clone(),
            });
        }
    }
    partitions.sort_by_key(|p| (p.offset, p.size));
    // Volumes that are not a numbered partition get the numbers after the highest one.
    let mut next = partitions.iter().map(|p| p.index).max().unwrap_or(0);
    for partition in partitions.iter_mut().filter(|p| p.index == 0) {
        next = next.saturating_add(1);
        partition.index = next;
    }
    partitions
}

pub(super) fn list_disks() -> Vec<DiskInfo> {
    let volumes = enumerate_volumes();
    let system = system_disks();
    let mut disks: Vec<DiskInfo> = Vec::new();
    for probed in DISK_NUMBERS {
        let path = physical_drive_path(probed);
        let Ok(device) = open_device(OsStr::new(&path), Access::Query) else {
            continue;
        };
        // The number is what the device says; only devices of type "disk" have
        // PhysicalDrive numbers.
        let number = match device.device_number() {
            Ok(n) if n.device_type == DEVICE_TYPE_DISK => n.number,
            Ok(_) => continue,
            Err(_) => probed,
        };
        if disks.iter().any(|d| d.number == number) {
            continue;
        }
        let geometry = device.geometry();
        let no_media = matches!(&geometry, Err(e) if e.raw_os_error() == Some(ERROR_NOT_READY));
        let (sector_size, physical_sector_size) = device.sector_sizes().unwrap_or((512, 512));
        let identity = device.identity().ok();
        let bus_type = identity.as_ref().map_or(0, |i| i.bus_type);

        let mut flags = Vec::new();
        if matches!(bus_type, 14 | 15) {
            flags.push(DiskFlag::Virtual);
        }
        if bus_type == 16 {
            flags.push(DiskFlag::Spaces);
        }
        // A volume on this disk that is not a single numbered partition belongs to
        // the Logical Disk Manager.
        let dynamic = volumes.iter().filter(|v| on_disk(v, number)).any(|v| {
            v.device_number.is_none() || v.extents.as_ref().is_some_and(|extents| extents.len() > 1)
        });
        if dynamic {
            flags.push(DiskFlag::Dynamic);
        }
        if no_media {
            flags.push(DiskFlag::NoMedia);
        }

        disks.push(DiskInfo {
            kind: SourceKind::Disk,
            path,
            size: geometry.as_ref().map_or(0, |g| g.disk_size),
            sector_size,
            physical_sector_size,
            model: identity.as_ref().and_then(|i| i.model()),
            serial: identity
                .as_ref()
                .map(|i| i.serial.clone())
                .filter(|s| !s.is_empty()),
            disk_number: Some(number),
            number,
            bus: identity
                .as_ref()
                .map_or("Unknown", |i| i.bus_name())
                .to_string(),
            removable: identity.as_ref().is_some_and(|i| i.removable_media),
            is_system: system.contains(&number),
            media: match device.seek_penalty() {
                Some(true) => MediaKind::Hdd,
                Some(false) => MediaKind::Ssd,
                None => MediaKind::Unknown,
            },
            trim: device.trim_enabled(),
            flags,
            table: match geometry.as_ref().ok().and_then(|g| g.partition_style) {
                Some(PartitionStyle::Mbr) => TableKind::Mbr,
                Some(PartitionStyle::Gpt) => TableKind::Gpt,
                Some(PartitionStyle::Raw) | None => TableKind::None,
            },
            partitions: partitions_of(number, &volumes),
        });
    }
    disks
}

pub(super) fn annotate_partitions(table: &mut PartitionTable, source: &dyn BlockSource) {
    let SourceLocation::Disk { number } = source.location() else {
        return;
    };
    let volumes = enumerate_volumes();
    for partition in &mut table.partitions {
        // A volume belongs to a partition when one of its extents starts where the partition does.
        let mounted = volumes.iter().find(|v| {
            v.extents.as_ref().is_some_and(|extents| {
                extents
                    .iter()
                    .any(|e| e.disk == number && e.start == partition.offset)
            })
        });
        if let Some(volume) = mounted {
            partition.letters = volume.letters();
            partition.label = fs_and_label(volume).1;
        }
    }
}

pub(super) fn source_warnings(source: &dyn BlockSource, range: Range<u64>) -> Vec<Warning> {
    let SourceLocation::Disk { number } = source.location() else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    let warn = |warnings: &mut Vec<Warning>, code: &str, message: String| {
        warnings.push(Warning {
            code: code.to_string(),
            message,
        });
    };

    let trim = open_device(OsStr::new(&physical_drive_path(number)), Access::Query)
        .ok()
        .and_then(|d| d.trim_enabled());
    if trim == Some(true) {
        warn(
            &mut warnings,
            WARN_TRIM_SUPPORTED,
            "This device accepts TRIM. A format tells such a device (SSDs and many SMR hard disks) to discard its \
             data, so the old files may already be gone."
                .to_string(),
        );
    }

    let volumes = enumerate_volumes();
    let mounted: Vec<&Volume> = volumes
        .iter()
        .filter(|v| !v.mount_paths.is_empty())
        .filter(|v| {
            v.extents.as_ref().is_some_and(|extents| {
                extents.iter().any(|e| {
                    e.disk == number
                        && e.start < range.end
                        && range.start < e.start.saturating_add(e.len)
                })
            })
        })
        .collect();
    if !mounted.is_empty() {
        let mut places: Vec<String> = mounted
            .iter()
            .flat_map(|v| {
                let letters = v.letters();
                if letters.is_empty() {
                    v.mount_paths.clone()
                } else {
                    letters
                }
            })
            .collect();
        places.sort();
        places.dedup();
        warn(
            &mut warnings,
            WARN_MOUNTED_VOLUME,
            format!(
                "The area to be scanned is in use as {}. Windows and other programs keep writing there, and every \
                 write can destroy old data: do not save, install or download anything to it and do not run chkdsk.",
                places.join(", ")
            ),
        );
        let page_files = page_file_letters();
        let mut with_page_file: Vec<String> = mounted
            .iter()
            .flat_map(|v| v.letters())
            .filter(|letter| {
                letter
                    .chars()
                    .next()
                    .is_some_and(|c| page_files.contains(&c))
            })
            .collect();
        with_page_file.sort();
        with_page_file.dedup();
        if !with_page_file.is_empty() {
            warn(
                &mut warnings,
                WARN_PAGE_FILE_ON_SOURCE,
                format!(
                    "A page file is configured on {}. Windows writes to it and creates it again at the next start: \
                     do not restart the computer before the rescue is finished.",
                    with_page_file.join(", ")
                ),
            );
        }
    }

    if system_disks().contains(&number) {
        warn(
            &mut warnings,
            WARN_SYSTEM_DISK,
            "This disk carries the volume Windows runs from. Windows writes to it all the time; the longer the \
             computer runs, the more of the old data is overwritten."
                .to_string(),
        );
    }
    warnings
}
