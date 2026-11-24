//! Unix back end of the output primitives: every operation is relative to
//! the open descriptor of a verified directory and never follows a link.

use std::ffi::{CStr, CString};
use std::fs::{File, FileTimes};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::SystemTime;

use super::{Dir, EntryKind, FsLimits, OutErrorKind};
use crate::sanitize::ComponentLimit;
use crate::source::FileId;

/// Unix has no call that sets the creation ("birth") time of a file.
pub(super) const CAN_SET_CREATION_TIME: bool = false;

/// Flags for opening a directory that must be a real directory, not a link to one.
const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

pub(super) fn classify_os_error(errno: i32) -> Option<OutErrorKind> {
    match errno {
        libc::ENAMETOOLONG | libc::EILSEQ => Some(OutErrorKind::NameRejected),
        libc::ENOSPC | libc::EDQUOT => Some(OutErrorKind::NoSpace),
        libc::EFBIG => Some(OutErrorKind::TooLarge),
        libc::EEXIST => Some(OutErrorKind::Exists),
        _ => None,
    }
}

pub(super) fn file_times(
    _created: Option<SystemTime>,
    modified: Option<SystemTime>,
    accessed: Option<SystemTime>,
) -> FileTimes {
    let mut times = FileTimes::new();
    if let Some(modified) = modified {
        times = times.set_modified(modified);
    }
    if let Some(accessed) = accessed {
        times = times.set_accessed(accessed);
    }
    times
}

pub(super) fn path_units(path: &Path) -> usize {
    path.to_string_lossy().encode_utf16().count()
}

fn c_name(name: &str) -> io::Result<CString> {
    CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the name contains a NUL byte"))
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the path contains a NUL byte"))
}

/// Runs a libc call that returns -1 on failure, repeating it while it is interrupted.
fn cvt(mut call: impl FnMut() -> libc::c_int) -> io::Result<libc::c_int> {
    loop {
        let ret = call();
        if ret != -1 {
            return Ok(ret);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn owned(fd: libc::c_int) -> File {
    // SAFETY: `fd` was just returned by a successful open call and is owned by nobody else.
    File::from(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn not_plain_dir() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "the name is taken by something that is not a plain directory",
    )
}

/// Opens `name` below `parent` as a directory, refusing links.
fn open_dir_at(parent: RawFd, name: &CStr) -> io::Result<File> {
    // SAFETY: `name` is a valid NUL-terminated string; the call does not retain it.
    cvt(|| unsafe { libc::openat(parent, name.as_ptr(), DIR_FLAGS) }).map(owned)
}

fn is_link_or_not_dir(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::ELOOP | libc::ENOTDIR | libc::EMLINK)
    )
}

pub(super) fn open_root(path: &Path) -> io::Result<Dir> {
    // Links on the way to the root are resolved once, here; below the root nothing
    // is resolved by path again.
    let canonical = std::fs::canonicalize(path)?;
    let c = c_path(&canonical)?;
    // SAFETY: `c` is a valid NUL-terminated string.
    let handle = cvt(|| unsafe { libc::open(c.as_ptr(), DIR_FLAGS) }).map(owned)?;
    let name_limit =
        fstatvfs(&handle).map_or(ComponentLimit::Utf8Bytes(255), |st| name_limit_of(&st));
    Ok(Dir {
        handle,
        path: canonical,
        name_limit,
    })
}

pub(super) fn create_root(path: &Path) -> io::Result<Dir> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the output directory must be an absolute path",
        ));
    }
    // The deepest ancestor that exists, then one plain mkdir per missing level.
    let mut missing: Vec<&Path> = Vec::new();
    for ancestor in path.ancestors() {
        match std::fs::metadata(ancestor) {
            Ok(meta) if meta.is_dir() => break,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("{} exists and is not a directory", ancestor.display()),
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push(ancestor),
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
    open_root(path)
}

pub(super) fn make_subdir(dir: &Dir, name: &str) -> io::Result<(Dir, bool)> {
    let c = c_name(name)?;
    let fd = dir.handle.as_raw_fd();
    // SAFETY: `c` is a valid NUL-terminated string.
    let created = match cvt(|| unsafe { libc::mkdirat(fd, c.as_ptr(), 0o777) }) {
        Ok(_) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e),
    };
    match open_dir_at(fd, &c) {
        Ok(handle) => Ok((
            Dir {
                handle,
                path: dir.path.join(name),
                name_limit: dir.name_limit,
            },
            created,
        )),
        Err(e) if is_link_or_not_dir(&e) => Err(not_plain_dir()),
        Err(e) => Err(e),
    }
}

pub(super) fn open_subdir(dir: &Dir, name: &str) -> io::Result<Option<Dir>> {
    let c = c_name(name)?;
    match open_dir_at(dir.handle.as_raw_fd(), &c) {
        Ok(handle) => Ok(Some(Dir {
            handle,
            path: dir.path.join(name),
            name_limit: dir.name_limit,
        })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) if is_link_or_not_dir(&e) => Err(not_plain_dir()),
        Err(e) => Err(e),
    }
}

pub(super) fn create_new(dir: &Dir, name: &str) -> io::Result<File> {
    let c = c_name(name)?;
    let fd = dir.handle.as_raw_fd();
    let flags = libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `c` is a valid NUL-terminated string. O_EXCL fails on any existing
    // name, a dangling symbolic link included.
    cvt(|| unsafe { libc::openat(fd, c.as_ptr(), flags, 0o666 as libc::c_uint) }).map(owned)
}

pub(super) fn open_existing(dir: &Dir, name: &str) -> io::Result<File> {
    let c = c_name(name)?;
    let fd = dir.handle.as_raw_fd();
    // O_NONBLOCK: a FIFO planted under the name must not hang the open; it has no
    // effect on a regular file, and anything else is refused right below.
    let flags = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    // SAFETY: `c` is a valid NUL-terminated string.
    let file = match cvt(|| unsafe { libc::openat(fd, c.as_ptr(), flags) }) {
        Ok(raw) => owned(raw),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the name is a symbolic link, not a regular file",
            ))
        }
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the name is not a regular file",
        ));
    }
    // Asked of the open file, so no other file can have taken the name in between.
    refuse_source_image(meta.dev(), meta.ino())?;
    Ok(file)
}

/// `renameat2(RENAME_NOREPLACE)`; `None` when the kernel or the file system
/// does not know the flag.
#[cfg(target_os = "linux")]
fn rename_exclusive(fd: RawFd, from: &CStr, to: &CStr) -> Option<io::Result<()>> {
    // SAFETY: both names are valid NUL-terminated strings; the system call takes
    // (int, const char *, int, const char *, unsigned int).
    let ret = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            fd,
            from.as_ptr(),
            fd,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if ret == 0 {
        return Some(Ok(()));
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOSYS | libc::EINVAL) => None,
        _ => Some(Err(err)),
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_exclusive(_fd: RawFd, _from: &CStr, _to: &CStr) -> Option<io::Result<()>> {
    None
}

pub(super) fn rename_noreplace(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    let (c_from, c_to) = (c_name(from)?, c_name(to)?);
    let fd = dir.handle.as_raw_fd();
    refuse_source_image_at(fd, &c_from)?;
    if let Some(result) = rename_exclusive(fd, &c_from, &c_to) {
        return result;
    }
    // Portable fallback with the same guarantee: a hard link cannot be created over
    // an existing name. A file system without hard links cannot do it.
    // SAFETY: both names are valid NUL-terminated strings.
    match cvt(|| unsafe { libc::linkat(fd, c_from.as_ptr(), fd, c_to.as_ptr(), 0) }) {
        Ok(_) => {
            // SAFETY: as above. If this fails the file has both names; the stale part
            // name is removed by the next cleanup.
            let _ = cvt(|| unsafe { libc::unlinkat(fd, c_from.as_ptr(), 0) });
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(e) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("this file system cannot rename without replacing: {e}"),
        )),
    }
}

pub(super) fn remove_file(dir: &Dir, name: &str) -> io::Result<()> {
    let c = c_name(name)?;
    let fd = dir.handle.as_raw_fd();
    refuse_source_image_at(fd, &c)?;
    // SAFETY: `c` is a valid NUL-terminated string. Without AT_REMOVEDIR the call
    // removes a file or the link itself and refuses a directory.
    cvt(|| unsafe { libc::unlinkat(fd, c.as_ptr(), 0) }).map(|_| ())
}

fn kind_of_mode(mode: libc::mode_t) -> EntryKind {
    match mode & libc::S_IFMT {
        libc::S_IFREG => EntryKind::File,
        libc::S_IFDIR => EntryKind::Dir,
        libc::S_IFLNK => EntryKind::Link,
        _ => EntryKind::Other,
    }
}

/// `fstatat` of the name itself, never of what a link at that name points
/// to; `None` when there is no such name.
fn lstat_at(fd: RawFd, name: &CStr) -> io::Result<Option<libc::stat>> {
    // SAFETY: `stat` is plain data for which all-zero bytes are a valid value.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is NUL-terminated and `st` is a valid, writable `stat`.
    match cvt(|| unsafe { libc::fstatat(fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) }) {
        Ok(_) => Ok(Some(st)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn stat_at(fd: RawFd, name: &CStr) -> io::Result<Option<EntryKind>> {
    Ok(lstat_at(fd, name)?.map(|st| kind_of_mode(st.st_mode)))
}

/// Safety invariant S4 on a system without share modes: fails when the file
/// with this identity is open as a source image. (On Windows the kernel
/// answers the same operations with a sharing violation.)
fn refuse_source_image(device: u64, inode: u64) -> io::Result<()> {
    let id = FileId {
        volume: device,
        index: inode,
    };
    if crate::image::is_open_image(id) {
        Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            "the file is the disk image that is being read; it must not be changed",
        ))
    } else {
        Ok(())
    }
}

/// [`refuse_source_image`] for the regular file `name`, if there is one.
fn refuse_source_image_at(fd: RawFd, name: &CStr) -> io::Result<()> {
    match lstat_at(fd, name)? {
        Some(st) if st.st_mode & libc::S_IFMT == libc::S_IFREG => {
            // The integer types of the stat fields differ between targets; the
            // conversion is the one `std::fs::Metadata` makes for the same fields.
            #[allow(clippy::unnecessary_cast)]
            let (device, inode) = (st.st_dev as u64, st.st_ino as u64);
            refuse_source_image(device, inode)
        }
        _ => Ok(()),
    }
}

/// Kind of the entry and the name it is stored under (on Unix: the name asked for).
pub(super) fn entry(dir: &Dir, name: &str) -> io::Result<Option<(EntryKind, String)>> {
    let c = c_name(name)?;
    Ok(stat_at(dir.handle.as_raw_fd(), &c)?.map(|kind| (kind, name.to_string())))
}

/// Owner of a `DIR *` stream.
struct DirStream(*mut libc::DIR);

impl Drop for DirStream {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful `fdopendir` and is closed once.
        unsafe { libc::closedir(self.0) };
    }
}

#[cfg(target_os = "linux")]
fn clear_errno() {
    // SAFETY: `__errno_location` returns the address of this thread's errno.
    unsafe { *libc::__errno_location() = 0 };
}

#[cfg(not(target_os = "linux"))]
fn clear_errno() {}

/// Calls `visit` with the name and kind of every entry of the directory.
/// Names that are not UTF-8 are skipped: nothing this module creates has one.
pub(super) fn for_each_entry(dir: &Dir, visit: &mut dyn FnMut(&str, EntryKind)) -> io::Result<()> {
    let fd = dir.handle.as_raw_fd();
    // A second open of the same directory: its own read position, same object.
    // SAFETY: the path is a valid NUL-terminated string.
    let listing = cvt(|| unsafe {
        libc::openat(
            fd,
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })?;
    // SAFETY: `listing` is an open directory descriptor; on success the stream owns it.
    let stream = unsafe { libc::fdopendir(listing) };
    if stream.is_null() {
        let err = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so the descriptor is still ours to close.
        unsafe { libc::close(listing) };
        return Err(err);
    }
    let stream = DirStream(stream);
    loop {
        clear_errno();
        // SAFETY: `stream.0` is a valid open stream used by this thread only.
        let raw = unsafe { libc::readdir(stream.0) };
        if raw.is_null() {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(code) if code != 0 && cfg!(target_os = "linux") => Err(err),
                _ => Ok(()),
            };
        }
        // SAFETY: `readdir` returned a valid entry whose `d_name` is NUL-terminated;
        // it stays valid until the next call on this stream.
        let (name, d_type) = unsafe { (CStr::from_ptr((*raw).d_name.as_ptr()), (*raw).d_type) };
        let Ok(name) = name.to_str() else { continue };
        if name == "." || name == ".." {
            continue;
        }
        let kind = match d_type {
            libc::DT_REG => EntryKind::File,
            libc::DT_DIR => EntryKind::Dir,
            libc::DT_LNK => EntryKind::Link,
            libc::DT_UNKNOWN => match c_name(name).and_then(|c| stat_at(fd, &c)) {
                Ok(Some(kind)) => kind,
                _ => continue,
            },
            _ => EntryKind::Other,
        };
        visit(name, kind);
    }
}

fn fstatvfs(handle: &File) -> io::Result<libc::statvfs> {
    // SAFETY: `statvfs` is plain data for which all-zero bytes are a valid value.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open and `st` is a valid, writable `statvfs`.
    cvt(|| unsafe { libc::fstatvfs(handle.as_raw_fd(), &mut st) })?;
    Ok(st)
}

/// Unix file systems limit the bytes of a name.
fn name_limit_of(st: &libc::statvfs) -> ComponentLimit {
    ComponentLimit::Utf8Bytes(u32::try_from(st.f_namemax).unwrap_or(u32::MAX).max(1))
}

pub(super) fn limits(dir: &Dir) -> io::Result<FsLimits> {
    let st = fstatvfs(&dir.handle)?;
    let (fs_name, max_file_bytes) = crate::guard::fs_name_and_file_limit(&dir.handle);
    Ok(FsLimits {
        max_component: name_limit_of(&st),
        max_file_bytes,
        fs_name,
    })
}

pub(super) fn free_space(dir: &Dir) -> io::Result<u64> {
    let st = fstatvfs(&dir.handle)?;
    // The integer types of the statvfs fields differ between targets.
    #[allow(clippy::unnecessary_cast)]
    let free = (st.f_bavail as u64).saturating_mul(st.f_frsize as u64);
    Ok(free)
}
