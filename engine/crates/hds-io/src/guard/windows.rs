//! Windows side of the destination guard: from a path to physical disk
//! extents, by handle (docs/notes/windows-raw-io.md, section 7).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;

use super::{Extent, Placement, Purpose, Refusal, Resolved};
use crate::source::FileId;
use crate::win::device::{open_device, Access};
use crate::win::volumes::{
    computer_names, final_path, free_space, local_volume_serials, open_for_query,
    system_volume_device, volume_info_by_handle, FILE_READ_ONLY_VOLUME,
};
use crate::win::{full_path, handle_info, wide_units};
use crate::winpath::{self, WinPath};

/// How many layers of virtual disks inside virtual disks are followed.
const MAX_VIRTUAL_DISK_DEPTH: u32 = 8;

const BUS_TYPE_VIRTUAL: i32 = 14;
const BUS_TYPE_FILE_BACKED_VIRTUAL: i32 = 15;
const BUS_TYPE_SPACES: i32 = 16;

/// Errors of an open that mean "this component does not exist (yet)":
/// file not found, path not found, bad path name, invalid name, the name of
/// a file where a directory was expected.
fn is_missing(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(2 | 3 | 123 | 161 | 267))
}

/// The deepest existing ancestor of `path`, opened without access rights.
/// The open follows every junction, symbolic link, mounted folder and
/// `subst` drive, so the handle is on the real object. The flag says whether
/// that object is `path` itself.
fn open_existing_ancestor(path: &Path) -> Result<(File, PathBuf, bool), Refusal> {
    let mut is_leaf = true;
    for ancestor in path.ancestors() {
        match open_for_query(ancestor) {
            Ok(handle) => return Ok((handle, ancestor.to_path_buf(), is_leaf)),
            Err(e) if is_missing(&e) => is_leaf = false,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                return Err(Refusal::NotWritable(format!(
                    "{} cannot be accessed: {e}",
                    ancestor.display()
                )))
            }
            Err(e) => return Err(Refusal::NotFound(format!("{}: {e}", ancestor.display()))),
        }
    }
    Err(Refusal::NotFound(
        "the drive or share does not exist".to_string(),
    ))
}

fn eq_ignore_case(a: &OsStr, b: &OsStr) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

/// Collects the extents of `volume` (a `\\?\Volume{GUID}` device) and, if it
/// lies on a virtual disk, of the volumes that hold its backing files.
/// `Err` says why the location cannot be established.
fn collect_extents(
    volume: &OsStr,
    depth: u32,
    visited: &mut Vec<OsString>,
    out: &mut Vec<Extent>,
) -> Result<(), String> {
    if visited.iter().any(|seen| eq_ignore_case(seen, volume)) {
        return Ok(());
    }
    if depth > MAX_VIRTUAL_DISK_DEPTH {
        return Err("virtual disks are nested too deeply".to_string());
    }
    visited.push(volume.to_os_string());
    let handle = open_device(volume, Access::Query)
        .map_err(|e| format!("the volume cannot be opened: {e}"))?;
    let extents = handle
        .volume_disk_extents()
        .map_err(|e| format!("the volume does not report its disk extents: {e}"))?;
    if extents.is_empty() {
        return Err("the volume reports no disk extents".to_string());
    }
    let mut disks: Vec<u32> = extents.iter().map(|e| e.disk).collect();
    disks.sort_unstable();
    disks.dedup();
    out.extend_from_slice(&extents);

    let mut on_virtual_disk = false;
    for disk in disks {
        let path = winpath::physical_drive_path(disk);
        let bus = open_device(OsStr::new(&path), Access::Query)
            .and_then(|d| d.identity())
            .map(|identity| identity.bus_type)
            .map_err(|e| format!("disk {disk} does not say what kind of device it is: {e}"))?;
        match bus {
            BUS_TYPE_SPACES => return Err("the volume is on a Storage Spaces disk".to_string()),
            BUS_TYPE_VIRTUAL | BUS_TYPE_FILE_BACKED_VIRTUAL => on_virtual_disk = true,
            _ => {}
        }
    }
    if on_virtual_disk {
        let hosts = handle.storage_dependencies().map_err(|e| {
            format!("the backing file of the virtual disk cannot be determined: {e}")
        })?;
        if hosts.is_empty() {
            return Err("the backing file of the virtual disk cannot be determined".to_string());
        }
        for (host_volume, _backing_file) in hosts {
            let units = wide_units(&host_volume);
            // A backing file on a network share or anything else without a volume
            // name cannot be traced further.
            let Some(device) = winpath::volume_device_prefix(&units) else {
                return Err(
                    "the virtual disk is backed by a file outside the local volumes".to_string(),
                );
            };
            collect_extents(&OsString::from_wide(device), depth + 1, visited, out)?;
        }
    }
    Ok(())
}

/// Placement of an object whose final path has no volume GUID form: a
/// network share, or a drive that was created without the mount manager.
fn placement_without_volume(handle: &File, volume_serial: u32) -> Placement {
    let Ok(dos_path) = final_path(handle, false) else {
        return Placement::Unknown("the volume has no name the system can resolve".to_string());
    };
    let Some(server) = winpath::classify(&wide_units(&dos_path))
        .server()
        .map(str::to_string)
    else {
        return Placement::Unknown(
            "the drive was not created through the mount manager".to_string(),
        );
    };
    // A share of this very machine leads straight back to local volumes
    // (the administrative shares \\host\E$ exist for an elevated program).
    if winpath::is_local_server(&server, &computer_names()) {
        return Placement::Unknown(format!(
            "the network path \\\\{server} leads back to this computer"
        ));
    }
    // The server may be this machine under a name that cannot be recognised (an
    // alias, one of its own addresses): a share that shows the serial number of a
    // local volume is treated as that volume.
    if volume_serial != 0 && local_volume_serials().contains(&volume_serial) {
        return Placement::Unknown(format!(
            "the share on \\\\{server} appears to be a volume of this computer"
        ));
    }
    Placement::Network
}

pub(super) fn resolve(path: &Path, purpose: Purpose) -> Result<Resolved, Refusal> {
    let kind = winpath::classify(&wide_units(path));
    if let WinPath::Invalid(why) = kind {
        return Err(Refusal::InvalidPath(why.to_string()));
    }
    // Windows does not normalise verbatim paths; everything else is made absolute
    // and checked again (a reserved device name has become `\\.\NAME` by now).
    let full = if kind.is_verbatim() {
        path.to_path_buf()
    } else {
        let full = full_path(path).map_err(|e| Refusal::InvalidPath(e.to_string()))?;
        if let WinPath::Invalid(why) = winpath::classify(&wide_units(&full)) {
            return Err(Refusal::InvalidPath(why.to_string()));
        }
        full
    };

    let (handle, existing, is_leaf) = open_existing_ancestor(&full)?;
    let info = handle_info(&handle)
        .map_err(|e| Refusal::NotFound(format!("{}: {e}", existing.display())))?;
    let is_dir = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let file_wanted_here = is_leaf && purpose.is_file();
    if file_wanted_here && is_dir {
        return Err(Refusal::NotWritable(format!(
            "{} exists and is a directory",
            existing.display()
        )));
    }
    if !file_wanted_here && !is_dir {
        return Err(Refusal::NotWritable(format!(
            "{} is not a directory",
            existing.display()
        )));
    }
    let volume = volume_info_by_handle(&handle).ok();
    if volume
        .as_ref()
        .is_some_and(|v| v.flags & FILE_READ_ONLY_VOLUME != 0)
    {
        return Err(Refusal::NotWritable("the volume is read-only".to_string()));
    }

    let existing_dir = if is_dir {
        Some(existing.as_path())
    } else {
        existing.parent()
    };
    let free_bytes = existing_dir.and_then(|dir| free_space(dir).ok());
    let fs_name = volume
        .as_ref()
        .map(|v| v.fs_name.clone())
        .filter(|name| !name.is_empty());
    // FAT stores file sizes in 32 bits ("FAT" is what FAT12 and FAT16 report).
    let max_file_bytes = fs_name
        .as_deref()
        .filter(|name| matches!(*name, "FAT" | "FAT12" | "FAT16" | "FAT32"))
        .map(|_| u64::from(u32::MAX));

    let mut on_system_volume = false;
    let placement = match final_path(&handle, true) {
        Ok(guid_path) => match winpath::volume_device_prefix(&wide_units(&guid_path)) {
            Some(device) => {
                let device = OsString::from_wide(device);
                on_system_volume =
                    system_volume_device().is_some_and(|system| eq_ignore_case(&system, &device));
                let mut extents = Vec::new();
                match collect_extents(&device, 0, &mut Vec::new(), &mut extents) {
                    Ok(()) => Placement::Extents(extents),
                    Err(why) => Placement::Unknown(why),
                }
            }
            None => Placement::Unknown("the final path names no volume".to_string()),
        },
        Err(_) => placement_without_volume(&handle, info.dwVolumeSerialNumber),
    };

    Ok(Resolved {
        placement,
        free_bytes,
        fs_name,
        max_file_bytes,
        on_system_volume,
        leaf_id: is_leaf.then(|| FileId {
            volume: u64::from(info.dwVolumeSerialNumber),
            index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        }),
    })
}
