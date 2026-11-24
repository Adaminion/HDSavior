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
    let backing = backing_files(meta.dev());
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
        backing_files: backing.files,
        backing_unknown: backing.unresolved,
    })
}

/// What is known about the files the file system with device number `dev`
/// is stored in.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Backing {
    /// The regular files it is stored in.
    pub(crate) files: Vec<FileId>,
    /// Set when it is stored in a file that can no longer be found by the name
    /// it was mounted from: a FUSE file system (ntfs-3g) mounted directly from
    /// a file that was renamed, moved or deleted since. That file may be the
    /// source image under its new name.
    pub(crate) unresolved: Option<String>,
}

/// The regular files the file system with device number `dev` is stored
/// in: the backing file of a loop device it is mounted from (also through
/// a partition of it or a device-mapper device stacked on it), or the file
/// a FUSE file system (ntfs-3g) was mounted from directly. A destination
/// there writes into that file: if it is the source image, the engine
/// would write into its own source.
#[cfg(target_os = "linux")]
fn backing_files(dev: u64) -> Backing {
    let mut out = Backing::default();
    collect_block_device(dev, &mut out.files, 0);
    // The mount table names the device or file a file system was mounted
    // from; FUSE file systems have an anonymous device number of their own.
    let wanted = format!("{}:{}", libc::major(dev), libc::minor(dev));
    if let Ok(table) = std::fs::read_to_string("/proc/self/mountinfo") {
        for (fs_type, source) in mount_sources(&table, &wanted) {
            match std::fs::metadata(&source) {
                Ok(meta) => {
                    use std::os::unix::fs::FileTypeExt;
                    if meta.file_type().is_block_device() {
                        collect_block_device(meta.rdev(), &mut out.files, 0);
                    } else if meta.is_file() {
                        push_id(&mut out.files, meta.dev(), meta.ino());
                    }
                }
                Err(_) => {
                    if out.unresolved.is_none() {
                        out.unresolved = unresolved_fuse_source(&fs_type, &source);
                    }
                }
            }
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn backing_files(_dev: u64) -> Backing {
    Backing::default()
}

/// The file-system type and mount source of every entry of a
/// `/proc/self/mountinfo` text whose device is `wanted` (`major:minor`).
#[cfg(target_os = "linux")]
pub(crate) fn mount_sources(table: &str, wanted: &str) -> Vec<(String, std::path::PathBuf)> {
    let mut out = Vec::new();
    for line in table.lines() {
        if line.split(' ').nth(2) != Some(wanted) {
            continue;
        }
        // After the separator: file-system type, then the mount source.
        let mut rest = match line.split(" - ").nth(1) {
            Some(rest) => rest.split(' '),
            None => continue,
        };
        let (Some(fs_type), Some(source)) = (rest.next(), rest.next()) else {
            continue;
        };
        out.push((fs_type.to_string(), unescape_mountinfo(source)));
    }
    out
}

/// Why a mount source that no longer exists makes the location's backing
/// unknown, or `None` when it does not. Only a FUSE file system whose source
/// is an absolute path counts: that is a file system mounted directly from a
/// file (ntfs-3g), and the file has been renamed, moved or deleted since.
/// Sources that are not paths (`user@host:/dir` of sshfs, the names of
/// virtual file systems) say nothing about local files.
#[cfg(target_os = "linux")]
pub(crate) fn unresolved_fuse_source(fs_type: &str, source: &Path) -> Option<String> {
    let fuse = fs_type == "fuse" || fs_type == "fuseblk" || fs_type.starts_with("fuse.");
    (fuse && source.is_absolute()).then(|| {
        format!(
            "the file system there was mounted from {}, which no longer exists under that name; it may be \
             the disk image being rescued under another name",
            source.display()
        )
    })
}

#[cfg(target_os = "linux")]
fn push_id(out: &mut Vec<FileId>, volume: u64, index: u64) {
    let id = FileId { volume, index };
    if !out.contains(&id) {
        out.push(id);
    }
}

/// `\040` and the other octal escapes of `/proc/self/mountinfo`.
#[cfg(target_os = "linux")]
fn unescape_mountinfo(text: &str) -> std::path::PathBuf {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|d| bytes[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match octal {
            Some(d) => {
                out.push(
                    d.iter()
                        .fold(0u8, |n, c| n.wrapping_mul(8).wrapping_add(c - b'0')),
                );
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    std::path::PathBuf::from(std::ffi::OsString::from(
        <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&out),
    ))
}

/// Follows block device `dev` in sysfs to the files it is stored in: a
/// loop device's backing file, the device a partition belongs to, the
/// devices a device-mapper device is built from.
#[cfg(target_os = "linux")]
fn collect_block_device(dev: u64, out: &mut Vec<FileId>, depth: u32) {
    let dir = format!("/sys/dev/block/{}:{}", libc::major(dev), libc::minor(dev));
    if let Ok(dir) = std::fs::canonicalize(dir) {
        collect_sysfs(&dir, out, depth);
    }
}

#[cfg(target_os = "linux")]
fn collect_sysfs(dir: &Path, out: &mut Vec<FileId>, depth: u32) {
    if depth > 8 {
        return;
    }
    if let Ok(name) = std::fs::read_to_string(dir.join("loop").join("backing_file")) {
        // A deleted backing file is shown with " (deleted)"; it can no
        // longer be the source image's name, but may still be its inode.
        let name = name.trim_end_matches('\n');
        if let Ok(meta) = std::fs::metadata(name) {
            push_id(out, meta.dev(), meta.ino());
        }
    }
    if dir.join("partition").exists() {
        if let Some(parent) = dir.parent() {
            collect_sysfs(parent, out, depth + 1);
        }
    }
    if let Ok(slaves) = std::fs::read_dir(dir.join("slaves")) {
        for slave in slaves.flatten() {
            if let Ok(target) = std::fs::canonicalize(slave.path()) {
                collect_sysfs(&target, out, depth + 1);
            }
        }
    }
}
