//! Windows-only plumbing shared by the modules of this crate.
//!
//! [`device`] is where safety invariant S1 lives: the one function that
//! opens devices and the one wrapper around device control codes.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::FileExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, GetFullPathNameW, BY_HANDLE_FILE_INFORMATION, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use crate::source::FileId;

pub mod device;
pub(crate) mod volumes;

/// Full sharing: what `CreateFile` requires for volumes, and the only mode
/// that does not fail on a disk somebody else has open.
pub(crate) const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// Longest string, in UTF-16 units, a path-returning API is allowed to make
/// this crate allocate.
const MAX_PATH_BUFFER: usize = 1 << 16;

/// The UTF-16 units of a string, without terminator (unpaired surrogates survive).
pub(crate) fn wide_units(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().collect()
}

/// NUL-terminated UTF-16 for the `W` functions. A string with an interior
/// NUL would be cut short by them, so it is refused.
pub(crate) fn wide_nul(text: impl AsRef<OsStr>) -> io::Result<Vec<u16>> {
    let mut units = wide_units(text);
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path contains a NUL character",
        ));
    }
    units.push(0);
    Ok(units)
}

/// The string in `buf` up to its first NUL.
pub(crate) fn from_wide(buf: &[u16]) -> OsString {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..end])
}

/// Like [`from_wide`], as a `String` (lossy for unpaired surrogates).
pub(crate) fn string_from_wide(buf: &[u16]) -> String {
    from_wide(buf).to_string_lossy().into_owned()
}

/// Calls a Windows function that fills a caller-supplied UTF-16 buffer and
/// returns the length written, or the length needed when the buffer is too
/// small, or 0 on failure (`GetFullPathNameW`, `GetFinalPathNameByHandleW`).
pub(crate) fn fill_wide(mut call: impl FnMut(&mut [u16]) -> u32) -> io::Result<OsString> {
    let mut buf = vec![0u16; 600];
    loop {
        let n = call(&mut buf) as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            return Ok(OsString::from_wide(&buf[..n]));
        }
        if n >= MAX_PATH_BUFFER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the system returned an absurdly long path",
            ));
        }
        buf.resize(n + 1, 0);
    }
}

/// `GetFullPathNameW`: absolute, `.` and `..` folded, `/` turned into `\`,
/// reserved device names turned into `\\.\NAME`. Does not touch the disk.
pub(crate) fn full_path(path: &Path) -> io::Result<PathBuf> {
    let wide = wide_nul(path)?;
    // SAFETY: `wide` is NUL-terminated and `buf` is writable for the length passed.
    let full = fill_wide(|buf| unsafe {
        GetFullPathNameW(
            wide.as_ptr(),
            buf.len() as u32,
            buf.as_mut_ptr(),
            null_mut(),
        )
    })?;
    Ok(PathBuf::from(full))
}

/// Positional read that fills `buf` or fails. `seek_read` may return short
/// counts and returns `Ok(0)` at the end of the file or device: both are
/// errors here, because nothing at or beyond the size is ever requested.
pub(crate) fn read_exact_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "read past the end",
                ))
            }
            Ok(n) => {
                offset += n as u64;
                buf = &mut buf[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// `GetFileInformationByHandle`: attributes, volume serial number and file index.
pub(crate) fn handle_info(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is open and `info` is a valid, writable structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

/// Identity of an open file or directory: volume serial number and file index.
pub(crate) fn file_id(file: &File) -> io::Result<FileId> {
    let info = handle_info(file)?;
    Ok(FileId {
        volume: u64::from(info.dwVolumeSerialNumber),
        index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

#[cfg(test)]
mod tests;
