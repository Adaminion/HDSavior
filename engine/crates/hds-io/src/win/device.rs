//! Safety invariant S1: the only function that opens a device, and the only
//! wrapper around device control codes.
//!
//! * [`open_device`] opens a physical disk or a volume device with one of
//!   two access levels: **query** (desired access 0: enough for every control
//!   code that needs no access right) or **read** (read access, physical
//!   disks only). There is no third level; the handle is synchronous.
//! * `control` sends a control code, and only one of the six read-only
//!   queries on its list. A read-only handle would not stop a lock or a
//!   dismount (those codes need no access right either), so the list is what
//!   keeps the engine from ever changing the state of a device.
//!
//! Everything a [`DeviceHandle`] offers is built on these two.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, GENERIC_READ};
use windows_sys::Win32::Storage::FileSystem::IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS;
use windows_sys::Win32::Storage::Vhd::{
    GetStorageDependencyInformation, GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES,
    STORAGE_DEPENDENCY_INFO, STORAGE_DEPENDENCY_INFO_TYPE_2, STORAGE_DEPENDENCY_INFO_VERSION_2,
};
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageAccessAlignmentProperty, StorageDeviceProperty,
    StorageDeviceSeekPenaltyProperty, StorageDeviceTrimProperty, DEVICE_SEEK_PENALTY_DESCRIPTOR,
    DEVICE_TRIM_DESCRIPTOR, DISK_EXTENT, DISK_GEOMETRY, DISK_GEOMETRY_EX, DISK_PARTITION_INFO,
    IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_CHECK_VERIFY2,
    IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
    STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER,
    STORAGE_PROPERTY_ID, VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use super::{read_exact_at, wide_units, SHARE_ALL};
use crate::guard::Extent;
use crate::winpath;

/// The control codes this crate may send: queries that change nothing.
const ALLOWED_CONTROL_CODES: [u32; 6] = [
    IOCTL_DISK_GET_LENGTH_INFO,
    IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    IOCTL_STORAGE_QUERY_PROPERTY,
    IOCTL_STORAGE_GET_DEVICE_NUMBER,
    IOCTL_STORAGE_CHECK_VERIFY2,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
];

/// `FILE_DEVICE_DISK`: the device type of disks and of volumes on them.
pub(crate) const DEVICE_TYPE_DISK: u32 = 7;

/// Most disk extents of one volume this crate is prepared to read.
const MAX_EXTENTS: usize = 4096;

/// What a device handle may be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Desired access 0: "query device attributes without accessing a
    /// device". For enumeration and for the volume handles of the
    /// destination guard. Needs no elevation.
    Query,
    /// Read access, for the selected source disk. Needs elevation.
    Read,
}

/// An open device. It can be asked the questions on the closed list of S1
/// and, with [`Access::Read`], be read; nothing else.
#[derive(Debug)]
pub struct DeviceHandle {
    file: File,
    access: Access,
}

/// Opens a device: the only place in the engine where that happens (S1).
///
/// `path` must be `\\.\PhysicalDrive<N>` (query or read) or a volume device
/// `\\?\Volume{GUID}` without trailing backslash (query only: volumes are
/// never read). Anything else is refused with `InvalidInput` before the
/// system is asked. The share mode is full sharing, the handle is
/// synchronous, and no flag is passed.
///
/// Errors: `NotFound` when there is no such disk, `PermissionDenied` when
/// read access is asked for without elevation.
pub fn open_device(path: &OsStr, access: Access) -> io::Result<DeviceHandle> {
    let units = wide_units(path);
    let is_disk = path
        .to_str()
        .and_then(winpath::parse_physical_drive)
        .is_some();
    let is_volume = winpath::is_volume_guid_device(&units);
    let acceptable = match access {
        Access::Query => is_disk || is_volume,
        Access::Read => is_disk,
    };
    if !acceptable {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            r"only \\.\PhysicalDrive<N> (query or read) and \\?\Volume{GUID} (query) can be opened as devices",
        ));
    }
    let desired_access = match access {
        Access::Query => 0,
        Access::Read => GENERIC_READ,
    };
    let file = OpenOptions::new()
        .access_mode(desired_access)
        .share_mode(SHARE_ALL)
        .open(path)?;
    Ok(DeviceHandle { file, access })
}

/// Sends a control code to a device: the only place in the engine where that
/// happens (S1). Codes that are not on the closed list are refused without
/// asking the system. Returns the number of bytes written to `out`.
fn control(device: &File, code: u32, input: &[u8], out: &mut [u8]) -> io::Result<u32> {
    if !ALLOWED_CONTROL_CODES.contains(&code) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("control code {code:#010x} is not on the list of read-only queries"),
        ));
    }
    let mut returned = 0u32;
    // SAFETY: the handle is open; both buffers are valid for the lengths passed (a
    // null pointer goes with length 0); no OVERLAPPED is used on this synchronous handle.
    let ok = unsafe {
        DeviceIoControl(
            device.as_raw_handle(),
            code,
            if input.is_empty() {
                null()
            } else {
                input.as_ptr().cast()
            },
            input.len() as u32,
            if out.is_empty() {
                null_mut()
            } else {
                out.as_mut_ptr().cast()
            },
            out.len() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(returned.min(out.len() as u32))
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn i64_at(bytes: &[u8], offset: usize) -> Option<i64> {
    Some(i64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}

fn short_answer(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the device returned a truncated {what}"),
    )
}

/// Partition style of a disk as Windows sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionStyle {
    /// Master boot record.
    Mbr,
    /// GUID partition table.
    Gpt,
    /// Not initialised.
    Raw,
}

/// Answer to `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Logical sector size in bytes.
    pub bytes_per_sector: u32,
    /// Size of the disk in bytes.
    pub disk_size: u64,
    /// Partition style, when the driver appends it to its answer.
    pub partition_style: Option<PartitionStyle>,
}

/// Answer to `IOCTL_STORAGE_GET_DEVICE_NUMBER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceNumber {
    /// `FILE_DEVICE_*` type; 7 is a disk.
    pub device_type: u32,
    /// The `PhysicalDrive` number when the type is a disk.
    pub number: u32,
    /// 0 for a whole disk, the 1-based partition number for a volume on a
    /// basic disk, `u32::MAX` when the device cannot be partitioned.
    pub partition: u32,
}

/// What a device says about itself (`StorageDeviceProperty`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// Vendor string (often empty for SATA disks).
    pub vendor: String,
    /// Product string.
    pub product: String,
    /// Firmware revision.
    pub revision: String,
    /// Serial number; often missing or not the drive's own behind USB bridges.
    pub serial: String,
    /// `STORAGE_BUS_TYPE` value: 7 USB, 11 SATA, 17 NVMe, 14/15 virtual, 16 Storage Spaces, ...
    pub bus_type: i32,
    /// The *medium* can be removed (card readers); false for USB hard disks.
    pub removable_media: bool,
}

impl DeviceIdentity {
    /// Vendor and product joined, or `None` when the device reports neither.
    pub fn model(&self) -> Option<String> {
        let model = format!("{} {}", self.vendor, self.product);
        let model = model.split_whitespace().collect::<Vec<_>>().join(" ");
        (!model.is_empty()).then_some(model)
    }

    /// The name of the bus type.
    pub fn bus_name(&self) -> &'static str {
        match self.bus_type {
            1 => "SCSI",
            2 => "ATAPI",
            3 => "ATA",
            4 => "1394",
            5 => "SSA",
            6 => "Fibre",
            7 => "USB",
            8 => "RAID",
            9 => "iSCSI",
            10 => "SAS",
            11 => "SATA",
            12 => "SD",
            13 => "MMC",
            14 => "Virtual",
            15 => "FileBackedVirtual",
            16 => "Spaces",
            17 => "NVMe",
            18 => "SCM",
            19 => "UFS",
            _ => "Unknown",
        }
    }
}

/// The NUL-terminated ASCII string at `offset` of a property answer; empty
/// when the offset is 0 ("the device has no such string") or out of bounds.
fn ascii_at(bytes: &[u8], offset: u32) -> String {
    let start = offset as usize;
    if start == 0 || start >= bytes.len() {
        return String::new();
    }
    let end = bytes[start..]
        .iter()
        .position(|&b| b == 0)
        .map_or(bytes.len(), |p| start + p);
    let text: String = bytes[start..end]
        .iter()
        .map(|&b| {
            if (0x20..0x7F).contains(&b) {
                b as char
            } else {
                '?'
            }
        })
        .collect();
    text.trim().to_string()
}

impl DeviceHandle {
    /// The access level the device was opened with.
    pub fn access(&self) -> Access {
        self.access
    }

    /// `IOCTL_DISK_GET_LENGTH_INFO`: size of the disk in bytes. Needs
    /// [`Access::Read`]; fails with access denied on a query handle.
    pub fn length(&self) -> io::Result<u64> {
        let mut out = [0u8; 8];
        let n = control(&self.file, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut out)? as usize;
        let length = i64_at(&out[..n], 0).ok_or_else(|| short_answer("length"))?;
        u64::try_from(length).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the device reports a negative length",
            )
        })
    }

    /// `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`: logical sector size, disk size and
    /// partition style. Works on a query handle.
    pub fn geometry(&self) -> io::Result<Geometry> {
        let mut out = [0u8; size_of::<DISK_GEOMETRY_EX>() + 256];
        let n = control(&self.file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut out)? as usize;
        let out = &out[..n];
        let sector_at =
            offset_of!(DISK_GEOMETRY_EX, Geometry) + offset_of!(DISK_GEOMETRY, BytesPerSector);
        let bytes_per_sector = u32_at(out, sector_at).ok_or_else(|| short_answer("geometry"))?;
        let size = i64_at(out, offset_of!(DISK_GEOMETRY_EX, DiskSize))
            .ok_or_else(|| short_answer("geometry"))?;
        // The partition information follows the fixed part when the driver provides it.
        let info_at = offset_of!(DISK_GEOMETRY_EX, Data);
        let style_at = info_at + offset_of!(DISK_PARTITION_INFO, PartitionStyle);
        let info_size = u32_at(
            out,
            info_at + offset_of!(DISK_PARTITION_INFO, SizeOfPartitionInfo),
        )
        .unwrap_or(0);
        let partition_style =
            if info_size as usize >= offset_of!(DISK_PARTITION_INFO, PartitionStyle) + 4 {
                match u32_at(out, style_at) {
                    Some(0) => Some(PartitionStyle::Mbr),
                    Some(1) => Some(PartitionStyle::Gpt),
                    Some(2) => Some(PartitionStyle::Raw),
                    _ => None,
                }
            } else {
                None
            };
        Ok(Geometry {
            bytes_per_sector,
            disk_size: u64::try_from(size).unwrap_or(0),
            partition_style,
        })
    }

    fn property(&self, id: STORAGE_PROPERTY_ID, out: &mut [u8]) -> io::Result<usize> {
        // STORAGE_PROPERTY_QUERY { PropertyId, QueryType, AdditionalParameters[1] }, 12 bytes.
        let mut query = [0u8; 12];
        query[0..4].copy_from_slice(&id.to_le_bytes());
        query[4..8].copy_from_slice(&PropertyStandardQuery.to_le_bytes());
        Ok(control(&self.file, IOCTL_STORAGE_QUERY_PROPERTY, &query, out)? as usize)
    }

    /// `StorageAccessAlignmentProperty`: (logical, physical) sector size as
    /// the device reports them, unvalidated. Fails on devices and bridges
    /// that do not implement the query.
    pub fn access_alignment(&self) -> io::Result<(u32, u32)> {
        let mut out = [0u8; size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>()];
        let n = self.property(StorageAccessAlignmentProperty, &mut out)?;
        let out = &out[..n];
        let logical = u32_at(
            out,
            offset_of!(STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, BytesPerLogicalSector),
        );
        let physical = u32_at(
            out,
            offset_of!(STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, BytesPerPhysicalSector),
        );
        logical
            .zip(physical)
            .ok_or_else(|| short_answer("alignment descriptor"))
    }

    /// Logical and physical sector size, validated: the logical size must be
    /// a power of two from 512 to 4096 (from the alignment property, else
    /// from the geometry); a physical size that is not a power of two of at
    /// least the logical size and at most 64 KiB is replaced by the logical
    /// one.
    pub fn sector_sizes(&self) -> io::Result<(u32, u32)> {
        let from_geometry = || {
            self.geometry()
                .map(|g| g.bytes_per_sector)
                .ok()
                .filter(|&s| crate::source::is_valid_sector_size(s))
        };
        match self.access_alignment() {
            Ok((logical, physical)) if crate::source::is_valid_sector_size(logical) => {
                let physical_ok =
                    physical.is_power_of_two() && physical >= logical && physical <= 65536;
                Ok((logical, if physical_ok { physical } else { logical }))
            }
            _ => match from_geometry() {
                Some(logical) => Ok((logical, logical)),
                None => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the device reports no usable sector size",
                )),
            },
        }
    }

    /// `StorageDeviceProperty`: vendor, product, serial number, bus type.
    pub fn identity(&self) -> io::Result<DeviceIdentity> {
        let mut out = vec![0u8; 4096];
        let n = self.property(StorageDeviceProperty, &mut out)?;
        let out = &out[..n];
        let field =
            |offset: usize| u32_at(out, offset).ok_or_else(|| short_answer("device descriptor"));
        // The binding types BOOLEAN members as `bool`; the raw byte is read instead,
        // so that a driver that answers 0xFF cannot cause undefined behaviour.
        let removable = *out
            .get(offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia))
            .ok_or_else(|| short_answer("device descriptor"))?;
        Ok(DeviceIdentity {
            vendor: ascii_at(
                out,
                field(offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset))?,
            ),
            product: ascii_at(
                out,
                field(offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset))?,
            ),
            revision: ascii_at(
                out,
                field(offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductRevisionOffset))?,
            ),
            serial: ascii_at(
                out,
                field(offset_of!(STORAGE_DEVICE_DESCRIPTOR, SerialNumberOffset))?,
            ),
            bus_type: field(offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType))? as i32,
            removable_media: removable != 0,
        })
    }

    fn flag_property(
        &self,
        id: STORAGE_PROPERTY_ID,
        flag_offset: usize,
        size: usize,
    ) -> Option<bool> {
        let mut out = [0u8; 64];
        let n = self.property(id, &mut out[..size.min(64)]).ok()?;
        out[..n].get(flag_offset).map(|&b| b != 0)
    }

    /// `StorageDeviceSeekPenaltyProperty`: `Some(false)` means no seek
    /// penalty (a solid-state device); `None` when the device does not say.
    pub fn seek_penalty(&self) -> Option<bool> {
        self.flag_property(
            StorageDeviceSeekPenaltyProperty,
            offset_of!(DEVICE_SEEK_PENALTY_DESCRIPTOR, IncursSeekPenalty),
            size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>(),
        )
    }

    /// `StorageDeviceTrimProperty`: `Some(true)` means the device accepts
    /// TRIM, so a format has probably discarded its data.
    pub fn trim_enabled(&self) -> Option<bool> {
        self.flag_property(
            StorageDeviceTrimProperty,
            offset_of!(DEVICE_TRIM_DESCRIPTOR, TrimEnabled),
            size_of::<DEVICE_TRIM_DESCRIPTOR>(),
        )
    }

    /// `IOCTL_STORAGE_GET_DEVICE_NUMBER`. Fails on volumes that are not a
    /// single partition (dynamic and spanned volumes).
    pub fn device_number(&self) -> io::Result<DeviceNumber> {
        let mut out = [0u8; size_of::<STORAGE_DEVICE_NUMBER>()];
        let n = control(&self.file, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut out)? as usize;
        let out = &out[..n];
        let field =
            |offset: usize| u32_at(out, offset).ok_or_else(|| short_answer("device number"));
        Ok(DeviceNumber {
            device_type: field(offset_of!(STORAGE_DEVICE_NUMBER, DeviceType))?,
            number: field(offset_of!(STORAGE_DEVICE_NUMBER, DeviceNumber))?,
            partition: field(offset_of!(STORAGE_DEVICE_NUMBER, PartitionNumber))?,
        })
    }

    /// `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` on a volume handle: **all**
    /// extents of the volume, in bytes from the start of each disk. An empty
    /// list is possible and means "unknown", never "nowhere".
    pub fn volume_disk_extents(&self) -> io::Result<Vec<Extent>> {
        let header = offset_of!(VOLUME_DISK_EXTENTS, Extents);
        let stride = size_of::<DISK_EXTENT>();
        let mut capacity = 1usize;
        loop {
            let mut out = vec![0u8; header + capacity * stride];
            match control(
                &self.file,
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                &[],
                &mut out,
            ) {
                Ok(n) => {
                    let out = &out[..n as usize];
                    let count = (u32_at(out, 0).ok_or_else(|| short_answer("extent list"))?
                        as usize)
                        .min(capacity);
                    let mut extents = Vec::with_capacity(count);
                    for i in 0..count {
                        let at = header + i * stride;
                        let disk = u32_at(out, at + offset_of!(DISK_EXTENT, DiskNumber));
                        let start = i64_at(out, at + offset_of!(DISK_EXTENT, StartingOffset));
                        let len = i64_at(out, at + offset_of!(DISK_EXTENT, ExtentLength));
                        match (disk, start.map(u64::try_from), len.map(u64::try_from)) {
                            (Some(disk), Some(Ok(start)), Some(Ok(len))) => {
                                extents.push(Extent { disk, start, len })
                            }
                            _ => return Err(short_answer("extent list")),
                        }
                    }
                    return Ok(extents);
                }
                // More than fits: the count is valid, the array is not. Ask again with room.
                Err(e) if matches!(e.raw_os_error(), Some(code) if code == ERROR_MORE_DATA as i32 || code == ERROR_INSUFFICIENT_BUFFER as i32) =>
                {
                    let wanted = u32_at(&out, 0).unwrap_or(0) as usize;
                    capacity = if wanted > capacity {
                        wanted
                    } else {
                        capacity * 2
                    };
                    if capacity > MAX_EXTENTS {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// `IOCTL_STORAGE_CHECK_VERIFY2`: succeeds while the device is present
    /// and ready. Transfers no data.
    pub fn check_verify(&self) -> io::Result<()> {
        control(&self.file, IOCTL_STORAGE_CHECK_VERIFY2, &[], &mut []).map(|_| ())
    }

    /// The volumes that hold the backing files of the virtual disk this
    /// volume is on (`GetStorageDependencyInformation` with the host-volumes
    /// flag): one `(host volume, path of the backing file on it)` per layer.
    /// Empty for a volume that is not on a virtual disk.
    pub fn storage_dependencies(&self) -> io::Result<Vec<(OsString, OsString)>> {
        const MAX_INFO_BYTES: usize = 1 << 20;
        let mut bytes = 4096usize;
        loop {
            // u64 elements: the structure holds pointers and needs their alignment.
            let mut buf = vec![0u64; bytes.div_ceil(8)];
            let buf_bytes = buf.len() * 8;
            let info = buf.as_mut_ptr().cast::<STORAGE_DEPENDENCY_INFO>();
            let mut used = 0u32;
            // SAFETY: `info` points to `buf_bytes` writable, suitably aligned bytes, which
            // is more than the fixed part of the structure.
            let rc = unsafe {
                (*info).Version = STORAGE_DEPENDENCY_INFO_VERSION_2;
                GetStorageDependencyInformation(
                    self.file.as_raw_handle(),
                    GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES,
                    buf_bytes as u32,
                    info,
                    &mut used,
                )
            };
            if rc == ERROR_INSUFFICIENT_BUFFER
                && used as usize > buf_bytes
                && used as usize <= MAX_INFO_BYTES
            {
                bytes = used as usize;
                continue;
            }
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
            let base = buf.as_ptr() as usize;
            let entries_at = offset_of!(STORAGE_DEPENDENCY_INFO, Anonymous);
            let stride = size_of::<STORAGE_DEPENDENCY_INFO_TYPE_2>();
            // SAFETY: the fixed part was written by the call above.
            let announced = unsafe { (*info).NumberEntries } as usize;
            let count = announced.min((buf_bytes - entries_at) / stride);
            // A string of the answer lies inside the buffer the call filled.
            let string_at = |ptr: *const u16| -> OsString {
                let addr = ptr as usize;
                if ptr.is_null()
                    || addr < base
                    || addr >= base + buf_bytes
                    || !(addr - base).is_multiple_of(2)
                {
                    return OsString::new();
                }
                let first = (addr - base) / 2;
                // SAFETY: `buf` holds `buf_bytes / 2` initialised u16 values at alignment >= 2.
                let units = unsafe {
                    std::slice::from_raw_parts(buf.as_ptr().cast::<u16>(), buf_bytes / 2)
                };
                let tail = &units[first..];
                OsString::from_wide(
                    &tail[..tail.iter().position(|&u| u == 0).unwrap_or(tail.len())],
                )
            };
            let mut hosts = Vec::with_capacity(count);
            for i in 0..count {
                // SAFETY: entry `i` lies inside the buffer (see `count`); it is read by value.
                let entry = unsafe {
                    std::ptr::read_unaligned(
                        buf.as_ptr()
                            .cast::<u8>()
                            .add(entries_at + i * stride)
                            .cast::<STORAGE_DEPENDENCY_INFO_TYPE_2>(),
                    )
                };
                hosts.push((
                    string_at(entry.HostVolumeName),
                    string_at(entry.DependentVolumeRelativePath),
                ));
            }
            return Ok(hosts);
        }
    }

    /// Positional read that fills `buf` or fails; for [`Access::Read`] handles.
    pub(crate) fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        read_exact_at(&self.file, offset, buf)
    }
}

#[cfg(test)]
mod tests;
