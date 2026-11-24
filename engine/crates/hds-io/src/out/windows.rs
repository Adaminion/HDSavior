//! Windows back end of the output primitives (docs/notes/windows-raw-io.md,
//! sections 7.3, 8, 9 and 10).
//!
//! Paths are the canonical root (with its `\\?\` prefix, under which Windows
//! interprets nothing) plus validated names. An existing name is trusted as
//! a plain directory or a regular file only after three questions have been
//! answered through an open handle and through the directory entry:
//!
//! 1. the entry is not flagged as a reparse point (junctions, symbolic
//!    links, mounted folders, cloud placeholders all are);
//! 2. the object that was opened - without following a link - says the same;
//! 3. the object's fully resolved path is the path that was asked for: a
//!    link that was followed after all leads somewhere else.
//!
//! A directory in use is held open in a way that keeps it from being
//! renamed or deleted, so it cannot be exchanged for a link later.

use std::ffi::{OsStr, OsString};
use std::fs::{File, FileTimes, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileTimesExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileW, MoveFileExW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, WIN32_FIND_DATAW,
};

use super::{Dir, EntryKind, FsLimits, OutErrorKind};
use crate::sanitize::ComponentLimit;
use crate::win::volumes::{final_path, free_space as volume_free_space, volume_info_by_handle};
use crate::win::{full_path, handle_info, string_from_wide, wide_nul, wide_units, SHARE_ALL};
use crate::winpath::{self, WinPath};

pub(super) const CAN_SET_CREATION_TIME: bool = true;

/// How often an operation is repeated when another program (a virus
/// scanner, the indexer) holds the file for a moment.
const BUSY_RETRIES: u32 = 5;

/// Flags for opening an object itself, never what a link at its name points to.
const NO_FOLLOW: u32 = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;

pub(super) fn classify_os_error(code: i32) -> Option<OutErrorKind> {
    match code {
        // ERROR_INVALID_NAME, ERROR_FILENAME_EXCED_RANGE
        123 | 206 => Some(OutErrorKind::NameRejected),
        // ERROR_HANDLE_DISK_FULL, ERROR_DISK_FULL, ERROR_DISK_QUOTA_EXCEEDED
        39 | 112 | 1295 => Some(OutErrorKind::NoSpace),
        // ERROR_FILE_TOO_LARGE
        223 => Some(OutErrorKind::TooLarge),
        // ERROR_FILE_EXISTS, ERROR_ALREADY_EXISTS
        80 | 183 => Some(OutErrorKind::Exists),
        _ => None,
    }
}

pub(super) fn file_times(
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    accessed: Option<SystemTime>,
) -> FileTimes {
    let mut times = FileTimes::new();
    if let Some(created) = created {
        times = times.set_created(created);
    }
    if let Some(modified) = modified {
        times = times.set_modified(modified);
    }
    if let Some(accessed) = accessed {
        times = times.set_accessed(accessed);
    }
    times
}

pub(super) fn path_units(path: &Path) -> usize {
    path.as_os_str().encode_wide().count()
}

/// `dir\name`. The name has been validated: it is one component and holds
/// nothing that the `\\?\` form would take literally by mistake.
fn child(dir: &Dir, name: &str) -> PathBuf {
    let mut path: OsString = dir.path.as_os_str().to_os_string();
    if !wide_units(&path).ends_with(&[b'\\' as u16]) {
        path.push("\\");
    }
    path.push(name);
    PathBuf::from(path)
}

fn not_plain_dir() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "the name is taken by something that is not a plain directory",
    )
}

/// Sharing violation or access denied: what a scanner that holds the file
/// for a moment looks like.
fn is_busy(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32))
}

fn is_missing(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(2 | 3))
}

fn retry_busy<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempt = 0;
    loop {
        match operation() {
            Err(e) if is_busy(&e) && attempt < BUSY_RETRIES => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(
                    20 * u64::from(attempt) * u64::from(attempt),
                ));
            }
            other => return other,
        }
    }
}

/// Question 3: true when the fully resolved path of the open object is
/// `expected` (compared without regard to case and to a trailing
/// backslash). When the system cannot name the object at all, the other two
/// questions have to do.
fn is_at(handle: &File, expected: &Path) -> bool {
    fn normal(path: &OsStr) -> String {
        path.to_string_lossy().trim_end_matches('\\').to_lowercase()
    }
    match final_path(handle, false) {
        Ok(actual) => normal(&actual) == normal(expected.as_os_str()),
        Err(_) => true,
    }
}

/// Question 1: attributes and stored name of a directory entry, from the
/// directory itself (`FindFirstFileW`); `None` when there is no such entry.
fn find_entry(path: &Path) -> io::Result<Option<(u32, String)>> {
    let wide = wide_nul(path)?;
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `wide` is NUL-terminated (and holds no wildcard: names are
    // validated); `data` is a valid, writable structure.
    let search = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
    if search == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        return if is_missing(&error) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    // SAFETY: the handle came from a successful FindFirstFileW and is closed once.
    unsafe { FindClose(search) };
    Ok(Some((
        data.dwFileAttributes,
        string_from_wide(&data.cFileName),
    )))
}

/// Opens a directory itself and verifies that it is a plain directory.
///
/// The handle asks for "list directory" access and shares reading and
/// writing but not deletion: while it is open the directory cannot be
/// renamed, moved or deleted. If another program already holds the
/// directory in a way that conflicts, or the right to change attributes is
/// missing, a handle without access rights is used instead: verified just
/// the same, but not pinning.
///
/// `existing_entry` is set for a name that was already there (not for the
/// root and not for a directory created a moment ago): then the directory
/// entry is asked as well and the resolved path is compared.
fn open_dir(path: &Path, existing_entry: bool) -> io::Result<File> {
    if existing_entry {
        match find_entry(path)? {
            Some((attributes, _))
                if attributes & FILE_ATTRIBUTE_DIRECTORY != 0
                    && attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 => {}
            Some(_) => return Err(not_plain_dir()),
            None => return Err(io::Error::from_raw_os_error(2)),
        }
    }
    let pinned = OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(NO_FOLLOW)
        .open(path);
    let handle = match pinned {
        Ok(handle) => handle,
        Err(e) if is_busy(&e) => OpenOptions::new()
            .access_mode(0)
            .share_mode(SHARE_ALL)
            .custom_flags(NO_FOLLOW)
            .open(path)?,
        Err(e) => return Err(e),
    };
    let attributes = handle_info(&handle)?.dwFileAttributes;
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(not_plain_dir());
    }
    if existing_entry && !is_at(&handle, path) {
        return Err(not_plain_dir());
    }
    Ok(handle)
}

pub(super) fn open_root(path: &Path) -> io::Result<Dir> {
    // The fully resolved path (GetFinalPathNameByHandleW): every junction, symbolic
    // link, mounted folder and SUBST drive on the way to the root is gone from it.
    let canonical = std::fs::canonicalize(path)?;
    let handle = open_dir(&canonical, false)?;
    let name_limit = volume_info_by_handle(&handle).map_or(ComponentLimit::DEFAULT, |info| {
        ComponentLimit::Utf16Units(info.max_component.max(1))
    });
    Ok(Dir {
        handle,
        path: canonical,
        name_limit,
    })
}

pub(super) fn create_root(path: &Path) -> io::Result<Dir> {
    let invalid = |why: &str| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a usable output directory: {why}"),
        )
    };
    let kind = winpath::classify(&wide_units(path));
    if let WinPath::Invalid(why) = kind {
        return Err(invalid(why));
    }
    let full = if kind.is_verbatim() {
        path.to_path_buf()
    } else {
        let full = full_path(path)?;
        if let WinPath::Invalid(why) = winpath::classify(&wide_units(&full)) {
            return Err(invalid(why));
        }
        full
    };
    // The deepest ancestor that exists, then one plain CreateDirectory per missing level.
    let mut missing: Vec<&Path> = Vec::new();
    for ancestor in full.ancestors() {
        match std::fs::metadata(ancestor) {
            Ok(meta) if meta.is_dir() => break,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("{} exists and is not a directory", ancestor.display()),
                ))
            }
            Err(e) if is_missing(&e) => missing.push(ancestor),
            Err(e) => return Err(e),
        }
    }
    for level in missing.into_iter().rev() {
        match std::fs::create_dir(level) {
            Ok(()) => {}
            // Created by someone else in the meantime: fine if it is a directory.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && level.is_dir() => {}
            Err(e) => return Err(e),
        }
    }
    open_root(&full)
}

pub(super) fn make_subdir(dir: &Dir, name: &str) -> io::Result<(Dir, bool)> {
    let path = child(dir, name);
    let created = match std::fs::create_dir(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e),
    };
    let handle = open_dir(&path, !created)?;
    Ok((
        Dir {
            handle,
            path,
            name_limit: dir.name_limit,
        },
        created,
    ))
}

pub(super) fn open_subdir(dir: &Dir, name: &str) -> io::Result<Option<Dir>> {
    let path = child(dir, name);
    match open_dir(&path, true) {
        Ok(handle) => Ok(Some(Dir {
            handle,
            path,
            name_limit: dir.name_limit,
        })),
        Err(e) if is_missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

pub(super) fn create_new(dir: &Dir, name: &str) -> io::Result<File> {
    // CREATE_NEW; std adds FILE_FLAG_OPEN_REPARSE_POINT for create_new, so an
    // existing link - even a dangling one - is "exists", not something to follow.
    // Others may read the file while it is written, but not write, rename or delete it.
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ)
        .open(child(dir, name))
}

pub(super) fn open_existing(dir: &Dir, name: &str) -> io::Result<File> {
    let path = child(dir, name);
    let not_regular = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the name is not a regular file",
        )
    };
    match find_entry(&path)? {
        Some((attributes, _))
            if attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) == 0 => {}
        Some(_) => return Err(not_regular()),
        None => return Err(io::Error::from_raw_os_error(2)),
    }
    let file = retry_busy(|| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
    })?;
    let attributes = handle_info(&file)?.dwFileAttributes;
    if attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || !is_at(&file, &path)
    {
        return Err(not_regular());
    }
    Ok(file)
}

pub(super) fn rename_noreplace(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    let (from, to) = (wide_nul(child(dir, from))?, wide_nul(child(dir, to))?);
    retry_busy(|| {
        // No flags: without the replace flag an existing target makes the call fail
        // (ERROR_ALREADY_EXISTS), and without the copy flag it never leaves the volume.
        // SAFETY: both paths are NUL-terminated UTF-16 strings.
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

pub(super) fn remove_file(dir: &Dir, name: &str) -> io::Result<()> {
    // DeleteFileW removes a file or the link itself, never a link's target, and
    // refuses a directory.
    let path = child(dir, name);
    retry_busy(|| match std::fs::remove_file(&path) {
        // "Access denied" is also what this call answers for a directory (or a
        // link to one): that is final, not a file that is busy for a moment.
        Err(e) if e.raw_os_error() == Some(5) && path.is_dir() => Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "the name is a directory",
        )),
        other => other,
    })
}

fn kind_of_attributes(attributes: u32) -> EntryKind {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        EntryKind::Link
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        EntryKind::Dir
    } else {
        EntryKind::File
    }
}

/// Kind of the entry and the name it is stored under - in the file system's
/// own case and in its long form. An entry that is not flagged as a reparse
/// point is still reported as a link when the object behind it says so or
/// turns out to live somewhere else.
pub(super) fn entry(dir: &Dir, name: &str) -> io::Result<Option<(EntryKind, String)>> {
    let Some((attributes, stored_name)) = find_entry(&child(dir, name))? else {
        return Ok(None);
    };
    let mut kind = kind_of_attributes(attributes);
    if kind != EntryKind::Link {
        let stored_path = child(dir, &stored_name);
        if let Ok(handle) = OpenOptions::new()
            .access_mode(0)
            .share_mode(SHARE_ALL)
            .custom_flags(NO_FOLLOW)
            .open(&stored_path)
        {
            let by_handle = handle_info(&handle).map_or(0, |info| info.dwFileAttributes);
            if by_handle & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !is_at(&handle, &stored_path) {
                kind = EntryKind::Link;
            }
        }
    }
    Ok(Some((kind, stored_name)))
}

/// Calls `visit` with the name and kind of every entry of the directory.
/// Names that are not valid Unicode are skipped: nothing this module creates
/// has one.
pub(super) fn for_each_entry(dir: &Dir, visit: &mut dyn FnMut(&str, EntryKind)) -> io::Result<()> {
    for item in std::fs::read_dir(&dir.path)? {
        let item = item?;
        let Ok(meta) = item.metadata() else { continue };
        if let Some(name) = item.file_name().to_str() {
            visit(name, kind_of_attributes(meta.file_attributes()));
        }
    }
    Ok(())
}

pub(super) fn limits(dir: &Dir) -> io::Result<FsLimits> {
    let info = volume_info_by_handle(&dir.handle)?;
    let fs_name = Some(info.fs_name).filter(|name| !name.is_empty());
    // FAT stores file sizes in 32 bits ("FAT" is what FAT12 and FAT16 report).
    let max_file_bytes = fs_name
        .as_deref()
        .filter(|name| matches!(*name, "FAT" | "FAT12" | "FAT16" | "FAT32"))
        .map(|_| u64::from(u32::MAX));
    Ok(FsLimits {
        max_component: ComponentLimit::Utf16Units(info.max_component.max(1)),
        max_file_bytes,
        fs_name,
    })
}

pub(super) fn free_space(dir: &Dir) -> io::Result<u64> {
    volume_free_space(&dir.path)
}
