//! [`RawDisk`]: a physical disk as a block source (Windows only).

use std::ffi::OsStr;
use std::fmt;
use std::io;

use crate::buf::AlignedBuf;
use crate::image::KnownGood;
use crate::resilient::{read_error, ReadFailure};
use crate::source::{check_read_request, BlockSource, SourceInfo, SourceKind, SourceLocation};
use crate::win::device::{open_device, Access, DeviceHandle, DEVICE_TYPE_DISK};
use crate::winpath::{parse_physical_drive, physical_drive_path};

/// A physical disk, `\\.\PhysicalDrive<N>`, opened for reading.
///
/// * Only that exact kind of path is accepted. A volume path such as
///   `\\.\E:` would open too, but its offsets are relative to the volume and
///   the destination guard compares disk-absolute extents.
/// * After opening, the device itself is asked what it is
///   (`IOCTL_STORAGE_GET_DEVICE_NUMBER`): it must be a disk, partition 0.
///   The number it reports - not the digits of the path - is the disk number
///   in [`SourceInfo`] and for the destination guard.
/// * The handle has read access only, full sharing, and is synchronous
///   (safety invariant S1). Mounted volumes on the disk stay mounted: what is
///   read is the on-disk state.
pub struct RawDisk {
    device: DeviceHandle,
    path: String,
    size: u64,
    sector_size: u32,
    physical_sector_size: u32,
    model: Option<String>,
    serial: Option<String>,
    disk_number: u32,
    good: KnownGood,
}

/// Size of the disk: from the length query, which a read handle answers;
/// from the geometry if the driver does not implement it.
fn device_size(device: &DeviceHandle) -> io::Result<u64> {
    device.length().or_else(|length_error| {
        device
            .geometry()
            .map(|g| g.disk_size)
            .map_err(|_| length_error)
    })
}

impl RawDisk {
    /// Opens the disk. Errors by `io::ErrorKind`: `InvalidInput` when `path`
    /// is not `\\.\PhysicalDrive<N>` or the device is not a whole disk,
    /// `NotFound` when there is no such disk, `PermissionDenied` without
    /// elevation.
    pub fn open(path: &str) -> io::Result<RawDisk> {
        let number_in_path = parse_physical_drive(path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                r"a disk source must be named \\.\PhysicalDrive<N>",
            )
        })?;
        let path = physical_drive_path(number_in_path);
        let device = open_device(OsStr::new(&path), Access::Read)?;
        let number = device.device_number()?;
        if number.device_type != DEVICE_TYPE_DISK || number.partition != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{path} is not a whole physical disk (device type {}, partition {})",
                    number.device_type, number.partition
                ),
            ));
        }
        let (sector_size, physical_sector_size) = device.sector_sizes()?;
        let size = device_size(&device)?;
        let size = size - size % u64::from(sector_size);
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{path} reports no capacity (no medium?)"),
            ));
        }
        let identity = device.identity().ok();
        Ok(RawDisk {
            device,
            path,
            size,
            sector_size,
            physical_sector_size,
            model: identity.as_ref().and_then(|i| i.model()),
            serial: identity.map(|i| i.serial).filter(|s| !s.is_empty()),
            disk_number: number.number,
            good: KnownGood::new(),
        })
    }

    /// The disk number the opened device reports.
    pub fn disk_number(&self) -> u32 {
        self.disk_number
    }
}

impl fmt::Debug for RawDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawDisk")
            .field("path", &self.path)
            .field("size", &self.size)
            .field("sector_size", &self.sector_size)
            .field("disk_number", &self.disk_number)
            .finish()
    }
}

impl BlockSource for RawDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn physical_sector_size(&self) -> u32 {
        self.physical_sector_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_read_request(self.size, self.sector_size, offset, buf.len())?;
        let result = if (buf.as_ptr() as usize).is_multiple_of(self.sector_size as usize) {
            self.device.read_exact_at(offset, buf)
        } else {
            // The disk driver may insist on sector-aligned memory: go through a
            // buffer that is, rather than fail in a way that looks like a bug.
            let mut bounce = AlignedBuf::new(buf.len());
            self.device
                .read_exact_at(offset, &mut bounce)
                .map(|()| buf.copy_from_slice(&bounce))
        };
        match result {
            Ok(()) => {
                self.good.note(offset);
                Ok(())
            }
            // The request was inside the size measured at open: the medium has changed.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && e.raw_os_error().is_none() => {
                Err(read_error(
                    ReadFailure::DeviceLost,
                    "the disk is smaller than when it was opened",
                ))
            }
            Err(e) => Err(e),
        }
    }

    fn info(&self) -> SourceInfo {
        SourceInfo {
            kind: SourceKind::Disk,
            path: self.path.clone(),
            size: self.size,
            sector_size: self.sector_size,
            physical_sector_size: self.physical_sector_size,
            model: self.model.clone(),
            serial: self.serial.clone(),
            disk_number: Some(self.disk_number),
        }
    }

    /// Three proofs, as the notes ask: the device answers a readiness check,
    /// still reports the size it had, and a sector that was readable before
    /// can be read again.
    fn is_alive(&self) -> bool {
        self.device.check_verify().is_ok()
            && device_size(&self.device)
                .is_ok_and(|size| size - size % u64::from(self.sector_size) == self.size)
            && self.good.still_readable(self.sector_size, |offset, buf| {
                self.device.read_exact_at(offset, buf)
            })
    }

    fn location(&self) -> SourceLocation {
        SourceLocation::Disk {
            number: self.disk_number,
        }
    }
}
