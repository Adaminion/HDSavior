//! Unix side of the destination guard. Physical disks are not resolved here
//! (raw disks are a Windows feature of the engine): this finds the deepest
//! existing ancestor, refuses what cannot be written, identifies the path
//! itself so that the source image can be recognised, and reports space.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::{Placement, Purpose, Refusal, Resolved};
use crate::source::FileId;

/// File-system name and maximum file size (if it is a practical limit) of
/// the file system an open file or directory is on. Best effort.
#[cfg(target_os = "linux")]
pub(crate) fn fs_name_and_file_limit(file: &File) -> (Option<String>, Option<u64>) {
    // SAFETY: `statfs` is plain data for which all-zero bytes are a valid value.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open and `st` is a valid, writable `statfs`.
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut st) } != 0 {
        return (None, None);
    }
    // Magic numbers from statfs(2); the field's integer type differs between targets.
    let name = match st.f_type as u64 & 0xFFFF_FFFF {
        0xEF53 => "ext",
        0x9123_683E => "btrfs",
        0x5846_5342 => "xfs",
        0x0102_1994 => "tmpfs",
        0x794C_7630 => "overlayfs",
        0x4D44 => "FAT",
        0x2011_BAB0 => "exFAT",
        0x5346_544E | 0x7366_746E => "NTFS",
        0x6573_5546 => "fuse",
        0x6969 => "nfs",
        0xFF53_4D42 | 0xFE53_4D42 => "cifs",
        0x2FC1_2FC1 => "zfs",
        0xF2F5_2010 => "f2fs",
        _ => return (None, None),
    };
    // FAT stores file sizes in 32 bits.
    let limit = (name == "FAT").then_some(u64::from(u32::MAX));
    (Some(name.to_string()), limit)
}

/// File-system name and maximum file size; unknown on this system.
#[cfg(not(target_os = "linux"))]
pub(crate) fn fs_name_and_file_limit(_file: &File) -> (Option<String>, Option<u64>) {
    (None, None)
}

fn statvfs_of(path: &Path) -> Option<libc::statvfs> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` is plain data for which all-zero bytes are a valid value.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is NUL-terminated and `st` is a valid, writable `statvfs`.
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then_some(st)
}

fn may_write(path: &Path, is_dir: bool) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mode = if is_dir {
        libc::W_OK | libc::X_OK
    } else {
        libc::W_OK
    };
    // SAFETY: `c` is a valid NUL-terminated string.
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

pub(super) fn resolve(path: &Path, purpose: Purpose) -> Result<Resolved, Refusal> {
    if !path.is_absolute() {
        return Err(Refusal::InvalidPath("the path is not absolute".to_string()));
    }
    if path.as_os_str().as_bytes().contains(&0) {
        return Err(Refusal::InvalidPath(
            "the path contains a NUL byte".to_string(),
        ));
    }
    // The deepest ancestor that exists; following links, because that is where a
    // write through this path would really land.
    let mut found = None;
    for (depth, ancestor) in path.ancestors().enumerate() {
        match std::fs::metadata(ancestor) {
            Ok(meta) => {
                found = Some((ancestor, meta, depth == 0));
                break;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                return Err(Refusal::NotWritable(format!(
                    "{} cannot be accessed: {e}",
                    ancestor.display()
                )))
            }
            Err(e) => return Err(Refusal::NotFound(format!("{}: {e}", ancestor.display()))),
        }
    }
    let Some((existing, meta, is_leaf)) = found else {
        return Err(Refusal::NotFound("no part of the path exists".to_string()));
    };
    let file_wanted_here = is_leaf && purpose.is_file();
    if file_wanted_here && !meta.is_file() {
        return Err(Refusal::NotWritable(format!(
            "{} exists and is not a regular file",
            existing.display()
        )));
    }
    if !file_wanted_here && !meta.is_dir() {
        return Err(Refusal::NotWritable(format!(
            "{} is not a directory",
            existing.display()
        )));
    }
    let stat = statvfs_of(existing);
    if stat.is_some_and(|st| st.f_flag & libc::ST_RDONLY != 0) {
        return Err(Refusal::NotWritable(
            "the file system is mounted read-only".to_string(),
        ));
    }
    if !may_write(existing, meta.is_dir()) {
        return Err(Refusal::NotWritable(format!(
            "no permission to write to {}",
            existing.display()
        )));
    }
    let (fs_name, max_file_bytes) = match File::open(existing) {
        Ok(handle) => fs_name_and_file_limit(&handle),
        Err(_) => (None, None),
    };
    // The integer types of the statvfs fields differ between targets.
    #[allow(clippy::unnecessary_cast)]
    let free_bytes = stat.map(|st| (st.f_bavail as u64).saturating_mul(st.f_frsize as u64));
    Ok(Resolved {
        placement: Placement::Unknown(
            "physical disks cannot be determined on this system".to_string(),
        ),
        free_bytes,
        fs_name,
        max_file_bytes,
        on_system_volume: false,
        leaf_id: is_leaf.then(|| FileId {
            volume: meta.dev(),
            index: meta.ino(),
        }),
    })
}
