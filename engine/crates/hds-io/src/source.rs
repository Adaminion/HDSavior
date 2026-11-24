//! The [`BlockSource`] trait, its description types and [`RangedSource`].

use std::fmt;
use std::io;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::resilient::{read_error, ReadFailure};

/// A read-only, sector-addressed source of bytes: a physical disk or an image
/// file of one.
///
/// There is deliberately no way to write through this trait (safety
/// invariant S1 of the architecture).
pub trait BlockSource: Send + Sync {
    /// Size in bytes; a multiple of [`sector_size`](Self::sector_size).
    fn size(&self) -> u64;

    /// Logical sector size in bytes: 512 or 4096 (a power of two between them).
    fn sector_size(&self) -> u32;

    /// Physical sector size in bytes; at least the logical size.
    fn physical_sector_size(&self) -> u32;

    /// Positional read that fills `buf` completely or fails.
    ///
    /// `offset` and `buf.len()` must be multiples of `sector_size()` and the
    /// request must end at or before `size()`. Implementations reject
    /// anything else with a *caller*-class error (see
    /// [`classify_read_error`](crate::classify_read_error)) without touching
    /// the device. On failure the content of `buf` is unspecified.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Description of the source for the protocol and for session files.
    fn info(&self) -> SourceInfo;

    /// True while the device still answers (safety invariant S7). Called by
    /// [`read_resilient`](crate::read_resilient) before a failed read is
    /// recorded as a bad sector; must not block longer than a failed read.
    fn is_alive(&self) -> bool;

    /// Where the bytes of this source are stored, for the destination guard.
    /// The default, [`SourceLocation::Unknown`], makes the guard answer
    /// `unknown_disk` for every destination: a source that knows better must
    /// say so.
    fn location(&self) -> SourceLocation {
        SourceLocation::Unknown
    }
}

macro_rules! forward_block_source {
    ($($ty:ty),+) => {$(
        impl<T: BlockSource + ?Sized> BlockSource for $ty {
            fn size(&self) -> u64 {
                (**self).size()
            }
            fn sector_size(&self) -> u32 {
                (**self).sector_size()
            }
            fn physical_sector_size(&self) -> u32 {
                (**self).physical_sector_size()
            }
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
                (**self).read_at(offset, buf)
            }
            fn info(&self) -> SourceInfo {
                (**self).info()
            }
            fn is_alive(&self) -> bool {
                (**self).is_alive()
            }
            fn location(&self) -> SourceLocation {
                (**self).location()
            }
        }
    )+};
}

forward_block_source!(&T, Box<T>, Arc<T>);

/// What kind of thing a source is. Wire form: `"disk"` or `"image"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// A physical disk, `\\.\PhysicalDrive<N>`.
    Disk,
    /// A raw image file of a disk or of a partition.
    Image,
}

/// Description of a source: the `SourceInfo` object of the protocol.
///
/// ```
/// let info = hds_io::SourceInfo {
///     kind: hds_io::SourceKind::Disk,
///     path: r"\\.\PhysicalDrive1".into(),
///     size: 4_000_787_030_016,
///     sector_size: 512,
///     physical_sector_size: 4096,
///     model: Some("WDC WD40EZRZ-00GXCB0".into()),
///     serial: Some("WD-WCC7K0000000".into()),
///     disk_number: Some(1),
/// };
/// let json = serde_json::to_string(&info).unwrap();
/// assert!(json.starts_with(r#"{"kind":"disk","path":"\\\\.\\PhysicalDrive1","size":4000787030016,"#));
/// ```
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    /// Disk or image.
    pub kind: SourceKind,
    /// Native path the source was opened with. A path that is not valid
    /// Unicode is converted lossily: this field is for display and for
    /// finding the source again, the open handle is the authority.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Logical sector size in bytes.
    pub sector_size: u32,
    /// Physical sector size in bytes.
    pub physical_sector_size: u32,
    /// Vendor and product as the device reports them; `None` for images.
    pub model: Option<String>,
    /// Serial number as the device reports it (unreliable behind USB
    /// bridges); `None` for images or when the device has none.
    pub serial: Option<String>,
    /// `PhysicalDrive` number the opened device itself reports; `None` for
    /// images.
    pub disk_number: Option<u32>,
}

/// Identity of a file: two open handles with equal `FileId` are the same file
/// (also through hard links, junctions, short names and `subst` drives).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    /// Volume serial number (Windows) or device number (Unix).
    pub volume: u64,
    /// File index (Windows) or inode number (Unix).
    pub index: u64,
}

/// Where the bytes of a source live. Input of
/// [`destination_check`](crate::destination_check).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLocation {
    /// A physical disk of this machine, by the number the opened device reports.
    Disk {
        /// `PhysicalDrive` number.
        number: u32,
    },
    /// A regular file, identified through its open handle.
    File {
        /// Identity of the image file.
        id: FileId,
    },
    /// Not stored on any device of this machine (in-memory test sources):
    /// nothing that is written anywhere can overwrite it.
    Detached,
    /// The source cannot say. The destination guard then treats the location
    /// of every destination as unknown.
    Unknown,
}

/// The error a [`RangedSource`] returns for a read outside its range. It
/// travels inside the `io::Error` and classifies as a *caller* error, never
/// as a bad sector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutOfRange {
    /// Offset of the refused read.
    pub offset: u64,
    /// Length of the refused read in bytes.
    pub len: u64,
    /// The range the source is restricted to.
    pub range: Range<u64>,
}

impl OutOfRange {
    /// Returns the `OutOfRange` carried by `error`, if that is what it is.
    pub fn from_io(error: &io::Error) -> Option<&OutOfRange> {
        error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<OutOfRange>())
    }
}

impl fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "read of {} bytes at offset {} is outside the session range {}..{}",
            self.len, self.offset, self.range.start, self.range.end
        )
    }
}

impl std::error::Error for OutOfRange {}

/// Why a range was refused by [`RangedSource::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeError {
    /// `start >= end`.
    Empty,
    /// Start or end is not a multiple of the source's sector size.
    Unaligned {
        /// The sector size the bounds must be multiples of.
        sector_size: u32,
    },
    /// The range ends beyond the source.
    BeyondEnd {
        /// Size of the source in bytes.
        size: u64,
    },
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RangeError::Empty => f.write_str("the range is empty"),
            RangeError::Unaligned { sector_size } => {
                write!(
                    f,
                    "the range is not aligned to the sector size of {sector_size} bytes"
                )
            }
            RangeError::BeyondEnd { size } => {
                write!(f, "the range ends beyond the source ({size} bytes)")
            }
        }
    }
}

impl std::error::Error for RangeError {}

/// A source together with the session range `[start, end)`; every read outside
/// that range is refused with [`OutOfRange`].
///
/// This is how safety invariant S2(a) is enforced structurally: the scanner
/// and the recovery engine only ever hold a `RangedSource`, and there is no
/// way to get the unrestricted source back out of it. Offsets stay absolute:
/// offset 0 is the first byte of the underlying source, and
/// [`size`](BlockSource::size) is the size of the underlying source.
#[derive(Clone)]
pub struct RangedSource {
    inner: Arc<dyn BlockSource>,
    start: u64,
    end: u64,
}

impl RangedSource {
    /// Restricts `inner` to `range`, which must be non-empty, sector-aligned
    /// and inside the source.
    pub fn new(inner: Arc<dyn BlockSource>, range: Range<u64>) -> Result<RangedSource, RangeError> {
        if range.start >= range.end {
            return Err(RangeError::Empty);
        }
        let sector_size = inner.sector_size();
        let sector = u64::from(sector_size.max(1));
        if !range.start.is_multiple_of(sector) || !range.end.is_multiple_of(sector) {
            return Err(RangeError::Unaligned { sector_size });
        }
        let size = inner.size();
        if range.end > size {
            return Err(RangeError::BeyondEnd { size });
        }
        Ok(RangedSource {
            inner,
            start: range.start,
            end: range.end,
        })
    }

    /// The session range in absolute bytes.
    pub fn range(&self) -> Range<u64> {
        self.start..self.end
    }

    /// First byte of the range.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// One past the last byte of the range.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Length of the range in bytes.
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Always false: an empty range cannot be constructed.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// True when `range` is non-empty and lies completely inside the session
    /// range, i.e. when a read of it would not be refused for its position.
    pub fn contains(&self, range: &Range<u64>) -> bool {
        range.start < range.end && range.start >= self.start && range.end <= self.end
    }

    /// The part of `range` that lies inside the session range (possibly empty).
    pub fn clamp(&self, range: &Range<u64>) -> Range<u64> {
        let start = range.start.max(self.start);
        let end = range.end.min(self.end);
        if start < end {
            start..end
        } else {
            start..start
        }
    }
}

impl fmt::Debug for RangedSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RangedSource")
            .field("start", &self.start)
            .field("end", &self.end)
            .field("source", &self.inner.info().path)
            .finish()
    }
}

impl BlockSource for RangedSource {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn physical_sector_size(&self) -> u32 {
        self.inner.physical_sector_size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let len = buf.len() as u64;
        let inside =
            offset >= self.start && offset.checked_add(len).is_some_and(|end| end <= self.end);
        if !inside {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                OutOfRange {
                    offset,
                    len,
                    range: self.start..self.end,
                },
            ));
        }
        self.inner.read_at(offset, buf)
    }

    fn info(&self) -> SourceInfo {
        self.inner.info()
    }

    fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }

    fn location(&self) -> SourceLocation {
        self.inner.location()
    }
}

/// Checks a read request against the contract of [`BlockSource::read_at`]:
/// sector-aligned offset and length, ending inside the source. Returns a
/// *caller*-class error otherwise. Every source in this crate calls it first;
/// other implementations of the trait should too.
pub fn check_read_request(size: u64, sector_size: u32, offset: u64, len: usize) -> io::Result<()> {
    let sector = u64::from(sector_size.max(1));
    let len = len as u64;
    if !offset.is_multiple_of(sector) || !len.is_multiple_of(sector) {
        return Err(read_error(
            ReadFailure::Caller,
            format!(
                "read of {len} bytes at offset {offset} is not aligned to the sector size {sector}"
            ),
        ));
    }
    match offset.checked_add(len) {
        Some(end) if end <= size => Ok(()),
        _ => Err(read_error(
            ReadFailure::Caller,
            format!("read of {len} bytes at offset {offset} ends beyond the source ({size} bytes)"),
        )),
    }
}

/// True for the logical sector sizes this crate works with: a power of two
/// from 512 to 4096.
pub(crate) fn is_valid_sector_size(size: u32) -> bool {
    size.is_power_of_two() && (512..=4096).contains(&size)
}

/// Opens a source for reading.
///
/// * [`SourceKind::Disk`]: `path` must be exactly `\\.\PhysicalDrive<N>`
///   (Windows only; see `RawDisk`). `sector_size` must be `None`: a disk
///   reports its own.
/// * [`SourceKind::Image`]: a regular file (see [`ImageFile`](crate::ImageFile)).
///   `sector_size` overrides the detection (512 unless a GPT header shows
///   4096); it must be a power of two from 512 to 4096.
///
/// Errors by `io::ErrorKind`: `InvalidInput` for a path or sector size that
/// is not acceptable (protocol error `bad_request`), `NotFound`,
/// `PermissionDenied` (opening a disk without elevation: protocol error
/// `access_denied`), `Unsupported` for a disk on a system other than Windows;
/// anything else is an I/O error of the open itself.
pub fn open_source(
    kind: SourceKind,
    path: &Path,
    sector_size: Option<u32>,
) -> io::Result<Arc<dyn BlockSource>> {
    match kind {
        SourceKind::Image => Ok(Arc::new(crate::image::ImageFile::open(path, sector_size)?)),
        SourceKind::Disk => {
            if sector_size.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a sector size can only be given for image files; a disk reports its own",
                ));
            }
            open_disk(path)
        }
    }
}

/// True when `path` is a name a disk source may have: exactly
/// `\\.\PhysicalDrive<N>` (the word in any case, `N` decimal without sign or
/// padding). Says nothing about whether such a disk exists.
pub fn is_disk_source_name(path: &str) -> bool {
    crate::winpath::parse_physical_drive(path).is_some()
}

#[cfg(windows)]
fn open_disk(path: &Path) -> io::Result<Arc<dyn BlockSource>> {
    let text = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            r"a disk source must be named \\.\PhysicalDrive<N>",
        )
    })?;
    Ok(Arc::new(crate::rawdisk::RawDisk::open(text)?))
}

#[cfg(not(windows))]
fn open_disk(path: &Path) -> io::Result<Arc<dyn BlockSource>> {
    // Validate the name first so that a malformed path is reported the same way everywhere.
    if path
        .to_str()
        .and_then(crate::winpath::parse_physical_drive)
        .is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            r"a disk source must be named \\.\PhysicalDrive<N>",
        ));
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "physical disks can only be opened on Windows",
    ))
}

#[cfg(test)]
mod tests;
