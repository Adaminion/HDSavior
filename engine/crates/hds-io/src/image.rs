//! [`ImageFile`]: a raw image file as a block source.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::buf::AlignedBuf;
use crate::partition::is_gpt_header;
use crate::resilient::{read_error, ReadFailure};
use crate::source::{
    check_read_request, is_valid_sector_size, BlockSource, FileId, SourceInfo, SourceKind,
    SourceLocation,
};

/// Remembers where reads have succeeded, so that "is the device still there"
/// can be answered by reading such a place again.
///
/// Two offsets are kept (the first and the latest success): on a failing disk
/// a sector that was readable can go bad, and one bad witness must not turn
/// a living device into a lost one.
pub(crate) struct KnownGood {
    first: AtomicU64,
    latest: AtomicU64,
}

const NONE: u64 = u64::MAX;

impl KnownGood {
    pub(crate) fn new() -> KnownGood {
        KnownGood {
            first: AtomicU64::new(NONE),
            latest: AtomicU64::new(NONE),
        }
    }

    /// Records a successful read that started at `offset`.
    pub(crate) fn note(&self, offset: u64) {
        let _ = self
            .first
            .compare_exchange(NONE, offset, Ordering::Relaxed, Ordering::Relaxed);
        self.latest.store(offset, Ordering::Relaxed);
    }

    /// True when no read has succeeded yet, or when one sector at a recorded
    /// offset can be read again with `read`.
    pub(crate) fn still_readable(
        &self,
        sector_size: u32,
        read: impl Fn(u64, &mut [u8]) -> io::Result<()>,
    ) -> bool {
        let first = self.first.load(Ordering::Relaxed);
        let latest = self.latest.load(Ordering::Relaxed);
        if first == NONE {
            return true;
        }
        let mut sector = AlignedBuf::new(sector_size as usize);
        read(latest, &mut sector).is_ok() || (first != latest && read(first, &mut sector).is_ok())
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
use crate::win::read_exact_at;

#[cfg(unix)]
fn file_id(file: &File) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().ok()?;
    Some(FileId {
        volume: meta.dev(),
        index: meta.ino(),
    })
}

#[cfg(windows)]
fn file_id(file: &File) -> Option<FileId> {
    crate::win::file_id(file).ok()
}

/// Opens the file read-only and returns it with its absolute path. On
/// Windows with share mode *read* only: while
/// the handle is open the kernel refuses every writer, truncation and
/// deletion (safety invariant S4), for this process and for all others.
#[cfg(windows)]
fn open_read_only(path: &Path) -> io::Result<(File, PathBuf)> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    // A device (`\\.\PhysicalDrive1`, `\\.\E:`, a reserved name such as NUL) must
    // never be opened as an "image": devices are opened by `open_device` only.
    let full = crate::win::full_path(path)?;
    if let crate::winpath::WinPath::Invalid(why) =
        crate::winpath::classify(&crate::win::wide_units(&full))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not the path of an image file: {why}"),
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&full)?;
    Ok((file, full))
}

/// The image files this process has open as sources, by identity; one entry
/// per open [`ImageFile`].
///
/// Unix has no share modes: no kernel keeps a program from deleting or
/// rewriting a file that is open elsewhere. The output primitives therefore
/// look here and refuse to delete, rename or open for writing a file that is
/// being read as a source - safety invariant S4 for the engine's own writes,
/// the only ones it can answer for on such a system. Windows' share modes
/// cover the name the image was opened by, but not its other hard links, so
/// the Windows primitives ask here too before they delete.
mod open_images {
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use crate::source::FileId;

    static OPEN: Mutex<Vec<FileId>> = Mutex::new(Vec::new());

    fn list() -> MutexGuard<'static, Vec<FileId>> {
        // The list is valid after every single operation on it.
        OPEN.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn add(id: FileId) {
        list().push(id);
    }

    pub(super) fn remove(id: FileId) {
        let mut open = list();
        if let Some(at) = open.iter().position(|&other| other == id) {
            open.swap_remove(at);
        }
    }

    /// True while an [`ImageFile`](super::ImageFile) for this file is open.
    pub(crate) fn contains(id: FileId) -> bool {
        list().contains(&id)
    }
}

pub(crate) use open_images::contains as is_open_image;

/// Opens the file read-only and returns it with its absolute path. Unix has
/// no mandatory share modes: other programs are not held back, and the
/// engine's own output primitives ask [`is_open_image`] before they delete,
/// rename or rewrite a file.
#[cfg(unix)]
fn open_read_only(path: &Path) -> io::Result<(File, PathBuf)> {
    let file = File::open(path)?;
    Ok((
        file,
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
    ))
}

/// 4096 when the file carries a valid GPT header at byte 4096 (LBA 1 of a
/// disk with 4096-byte sectors) and none at byte 512; otherwise 512.
fn detect_sector_size(file: &File, file_len: u64) -> u32 {
    let header_at = |sector_size: u64| {
        let mut sector = [0u8; 512];
        sector_size + 512 <= file_len
            && read_exact_at(file, sector_size, &mut sector).is_ok()
            && is_gpt_header(&sector, 1)
    };
    if !header_at(512) && header_at(4096) {
        4096
    } else {
        512
    }
}

/// A raw image of a disk or of a partition, opened read-only.
///
/// * Sector size: what the caller says, else 4096 if a GPT header shows that
///   the imaged disk had 4096-byte sectors, else 512. Logical and physical
///   size are reported equal.
/// * Size: the file length rounded **down** to whole sectors. A trailing
///   partial sector (a truncated image) is not readable through the
///   sector-based interface; data that maps there is out of bounds, never
///   invented zeros.
/// * Only regular files are accepted; on Windows a path into a device
///   namespace is refused before anything is opened.
/// * While the value lives the image cannot be written, truncated, renamed
///   or deleted (safety invariant S4): on Windows by anybody, because the
///   handle shares reading only; on Unix, which has no share modes, by the
///   output primitives of this crate ([`out`](crate::out)), which refuse to
///   touch a file that is open as a source.
pub struct ImageFile {
    file: File,
    path: PathBuf,
    file_len: u64,
    size: u64,
    sector_size: u32,
    id: Option<FileId>,
    good: KnownGood,
}

impl ImageFile {
    /// Opens `path`. See [`open_source`](crate::open_source) for the errors.
    pub fn open(path: &Path, sector_size: Option<u32>) -> io::Result<ImageFile> {
        if let Some(size) = sector_size {
            if !is_valid_sector_size(size) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "unsupported sector size {size}: must be a power of two from 512 to 4096"
                    ),
                ));
            }
        }
        let (file, path) = open_read_only(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the image must be a regular file",
            ));
        }
        let file_len = meta.len();
        let sector_size = sector_size.unwrap_or_else(|| detect_sector_size(&file, file_len));
        let size = file_len - file_len % u64::from(sector_size);
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the image holds {file_len} bytes, less than one sector"),
            ));
        }
        let id = file_id(&file);
        // Nothing below can fail: the entry is removed again when the value is dropped.
        if let Some(id) = id {
            open_images::add(id);
        }
        Ok(ImageFile {
            file,
            path,
            file_len,
            size,
            sector_size,
            id,
            good: KnownGood::new(),
        })
    }

    /// The absolute path of the image.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Length of the file in bytes, including a trailing partial sector that
    /// [`size`](BlockSource::size) leaves out.
    pub fn file_len(&self) -> u64 {
        self.file_len
    }
}

impl Drop for ImageFile {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            open_images::remove(id);
        }
    }
}

impl std::fmt::Debug for ImageFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageFile")
            .field("path", &self.path)
            .field("size", &self.size)
            .field("sector_size", &self.sector_size)
            .finish()
    }
}

impl BlockSource for ImageFile {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn physical_sector_size(&self) -> u32 {
        self.sector_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_read_request(self.size, self.sector_size, offset, buf.len())?;
        match read_exact_at(&self.file, offset, buf) {
            Ok(()) => {
                self.good.note(offset);
                Ok(())
            }
            // The request was inside the size measured at open: the file has shrunk
            // underneath us, which is a changed source, not a bug of the caller.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && e.raw_os_error().is_none() => {
                Err(read_error(
                    ReadFailure::DeviceLost,
                    "the image file is shorter than when it was opened",
                ))
            }
            Err(e) => Err(e),
        }
    }

    fn info(&self) -> SourceInfo {
        SourceInfo {
            kind: SourceKind::Image,
            path: self.path.to_string_lossy().into_owned(),
            size: self.size,
            sector_size: self.sector_size,
            physical_sector_size: self.sector_size,
            model: None,
            serial: None,
            disk_number: None,
        }
    }

    fn is_alive(&self) -> bool {
        let unchanged = self
            .file
            .metadata()
            .is_ok_and(|meta| meta.len() >= self.file_len);
        unchanged
            && self.good.still_readable(self.sector_size, |offset, buf| {
                read_exact_at(&self.file, offset, buf)
            })
    }

    fn location(&self) -> SourceLocation {
        match self.id {
            Some(id) => SourceLocation::File { id },
            None => SourceLocation::Unknown,
        }
    }
}

#[cfg(test)]
mod tests;
