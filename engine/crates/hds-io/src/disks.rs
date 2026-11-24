//! Disk enumeration and the source warnings that can be computed without
//! reading or changing anything.

use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::partition::{PartitionInfo, PartitionTable, TableKind};
use crate::source::{BlockSource, SourceInfo, SourceKind};

#[cfg(windows)]
#[path = "disks/windows.rs"]
mod sys;

/// Warning code: the device accepts TRIM, so a format has probably discarded the data.
pub const WARN_TRIM_SUPPORTED: &str = "trim_supported";
/// Warning code: the range holds a mounted volume; Windows keeps writing to it.
pub const WARN_MOUNTED_VOLUME: &str = "mounted_volume";
/// Warning code: a page file is configured on a volume inside the range.
pub const WARN_PAGE_FILE_ON_SOURCE: &str = "page_file_on_source";
/// Warning code: the disk carries the volume Windows runs from.
pub const WARN_SYSTEM_DISK: &str = "system_disk";

/// A warning for the user: the `Warning` object of the protocol.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Machine-readable code, one of the `WARN_*` constants here (other
    /// crates add their own codes).
    pub code: String,
    /// Plain-language explanation.
    pub message: String,
}

/// Kind of medium, from the seek-penalty property. Wire form: `"hdd"`,
/// `"ssd"`, `"unknown"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    /// The device reports a seek penalty: a rotating disk.
    Hdd,
    /// The device reports no seek penalty.
    Ssd,
    /// The device does not answer the question (common behind USB bridges).
    Unknown,
}

/// A special property of a disk. Wire form: snake case.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DiskFlag {
    /// A virtual disk (an attached VHD or VHDX, bus type 14 or 15).
    Virtual,
    /// A Storage Spaces virtual disk (bus type 16).
    Spaces,
    /// The disk carries volumes of the Logical Disk Manager (dynamic disk).
    Dynamic,
    /// A drive without a medium (an empty card reader).
    NoMedia,
}

/// One physical disk: the `Disk` object of the protocol (the members of
/// [`SourceInfo`] plus the rest).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiskInfo {
    /// Always [`SourceKind::Disk`].
    pub kind: SourceKind,
    /// `\\.\PhysicalDrive<N>`.
    pub path: String,
    /// Size in bytes (0 without a medium).
    pub size: u64,
    /// Logical sector size in bytes.
    pub sector_size: u32,
    /// Physical sector size in bytes.
    pub physical_sector_size: u32,
    /// Vendor and product, if reported.
    pub model: Option<String>,
    /// Serial number, if reported.
    pub serial: Option<String>,
    /// The disk number the device reports; same as `number`.
    pub disk_number: Option<u32>,
    /// The disk number.
    pub number: u32,
    /// Bus the disk is attached through: `"SATA"`, `"USB"`, `"NVMe"`, ...
    pub bus: String,
    /// True when the *medium* can be removed (card readers); false for USB hard disks.
    pub removable: bool,
    /// True when the disk carries the volume Windows runs from.
    pub is_system: bool,
    /// Rotating or solid state.
    pub media: MediaKind,
    /// Whether the device accepts TRIM; `None` when it does not say.
    pub trim: Option<bool>,
    /// Special properties.
    pub flags: Vec<DiskFlag>,
    /// Partition style as Windows reports it.
    pub table: TableKind,
    /// The partitions that carry a volume, see [`list_disks`].
    pub partitions: Vec<PartitionInfo>,
}

impl DiskInfo {
    /// The [`SourceInfo`] members of this disk.
    pub fn source_info(&self) -> SourceInfo {
        SourceInfo {
            kind: self.kind,
            path: self.path.clone(),
            size: self.size,
            sector_size: self.sector_size,
            physical_sector_size: self.physical_sector_size,
            model: self.model.clone(),
            serial: self.serial.clone(),
            disk_number: self.disk_number,
        }
    }
}

/// Lists the physical disks of this machine. Windows only; an empty list
/// everywhere else.
///
/// Only *query* handles (desired access 0) are used: no sector of any disk
/// is read, the call needs no elevation, does not wake sleeping disks and
/// cannot hang on a failing one. Consequences for the result:
///
/// * `size` and `sector_size` come from `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`,
///   which also says whether the disk is partitioned as MBR or GPT (`table`;
///   an uninitialised disk is `none`).
/// * `partitions` are the partitions Windows exposes a **volume** for,
///   found through the volumes' disk extents. `letters`, `label` and `fs`
///   describe that volume (`fs` from the file system Windows mounted, `None`
///   for a raw volume, `bitlocker` for a locked one); `type`, `type_id` are
///   empty and `ntfs` is `None`, because they are on the disk, which is not
///   read. `index` is the partition number Windows assigned, which equals
///   the index of [`parse_partition_table`](crate::parse_partition_table)
///   for tables in disk order (a volume without a partition number, on a
///   dynamic disk, gets the numbers after the highest one). Partitions
///   without a volume (the Microsoft reserved partition, foreign partition
///   types) are not listed.
/// * `flags`: `virtual` and `spaces` come from the bus type, `no_media` from
///   a failed geometry query; `dynamic` is concluded from a volume on the
///   disk that is not a single numbered partition.
/// * The complete table of one disk is available after opening it:
///   [`parse_partition_table`](crate::parse_partition_table) plus
///   [`annotate_partitions`].
///
/// Disk numbers may have gaps; numbers 0 to 63 are probed.
pub fn list_disks() -> Vec<DiskInfo> {
    #[cfg(windows)]
    {
        sys::list_disks()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Fills `letters` and `label` of the partitions of `table` from the volumes
/// Windows has mounted on them, when `source` is a physical disk. Does
/// nothing for images and on other systems. Uses query handles only.
pub fn annotate_partitions(table: &mut PartitionTable, source: &dyn BlockSource) {
    #[cfg(windows)]
    {
        sys::annotate_partitions(table, source);
    }
    #[cfg(not(windows))]
    {
        let _ = (table, source);
    }
}

/// Warnings about `range` of `source` that can be computed read-only, with
/// query handles and one registry read:
///
/// * `mounted_volume` - a volume with a drive letter or mount folder lies in
///   the range; Windows and programs keep writing to it;
/// * `page_file_on_source` - a page file is configured or exists on such a
///   volume;
/// * `system_disk` - the disk carries the volume Windows runs from;
/// * `trim_supported` - the device accepts TRIM.
///
/// Empty for images and on systems other than Windows.
pub fn source_warnings(source: &dyn BlockSource, range: Range<u64>) -> Vec<Warning> {
    #[cfg(windows)]
    {
        sys::source_warnings(source, range)
    }
    #[cfg(not(windows))]
    {
        let _ = (source, range);
        Vec::new()
    }
}

#[cfg(test)]
mod tests;
