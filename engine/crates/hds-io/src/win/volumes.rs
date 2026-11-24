//! Volumes, mount points and the facts about this machine that the
//! destination guard, the disk list and the source warnings need. Everything
//! here asks; nothing changes anything.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FileRemoteProtocolInfo, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    GetDiskFreeSpaceExW, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    GetVolumeInformationByHandleW, GetVolumeInformationW, GetVolumePathNamesForVolumeNameW,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_NAME_NORMALIZED, FILE_REMOTE_PROTOCOL_INFO, VOLUME_NAME_DOS,
    VOLUME_NAME_GUID,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_MULTI_SZ};
use windows_sys::Win32::System::SystemInformation::{
    ComputerNameDnsFullyQualified, ComputerNameDnsHostname, ComputerNameNetBIOS,
    ComputerNamePhysicalDnsFullyQualified, ComputerNamePhysicalDnsHostname,
    ComputerNamePhysicalNetBIOS, GetComputerNameExW, GetSystemWindowsDirectoryW,
};

use super::device::{open_device, Access, DeviceNumber};
use super::{fill_wide, from_wide, string_from_wide, wide_nul, wide_units, SHARE_ALL};
use crate::guard::Extent;
use crate::winpath;

/// `FILE_READ_ONLY_VOLUME` of `GetVolumeInformation`'s flags.
pub(crate) const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;

/// Most volumes one enumeration looks at.
const MAX_VOLUMES: usize = 1024;

/// One volume of this machine.
pub(crate) struct Volume {
    /// `\\?\Volume{GUID}`: the device name, without trailing backslash.
    pub(crate) device: OsString,
    /// Disk extents; `None` when they cannot be determined.
    pub(crate) extents: Option<Vec<Extent>>,
    /// Disk and partition number, for a volume that is one partition of a basic disk.
    pub(crate) device_number: Option<DeviceNumber>,
    /// Drive letters (`E:\`) and mounted folders the volume is reachable through.
    pub(crate) mount_paths: Vec<String>,
}

impl Volume {
    /// `\\?\Volume{GUID}\`: the root directory, as the volume functions want it.
    pub(crate) fn root(&self) -> OsString {
        let mut root = self.device.clone();
        root.push("\\");
        root
    }

    /// True when the volume has a drive letter or mounted folder. Only such a
    /// volume may be asked anything through a path on it
    /// (`GetVolumeInformationW` on its root): opening a path on a volume that
    /// is not mounted mounts it, and a first NTFS mount writes.
    pub(crate) fn is_reachable(&self) -> bool {
        !self.mount_paths.is_empty()
    }

    /// The drive letters among the mount paths, as `"E:"`.
    pub(crate) fn letters(&self) -> Vec<String> {
        let mut letters: Vec<String> = self
            .mount_paths
            .iter()
            .filter(|p| p.len() == 3 && p.as_bytes()[0].is_ascii_alphabetic() && p.ends_with(":\\"))
            .map(|p| p[..2].to_ascii_uppercase())
            .collect();
        letters.sort();
        letters.dedup();
        letters
    }
}

/// Label, file system and limits of a volume.
pub(crate) struct VolumeInfo {
    pub(crate) label: String,
    pub(crate) fs_name: String,
    pub(crate) serial: u32,
    pub(crate) max_component: u32,
    pub(crate) flags: u32,
}

/// Closes a volume search handle when dropped.
struct VolumeSearch(windows_sys::Win32::Foundation::HANDLE);

impl Drop for VolumeSearch {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful FindFirstVolumeW and is closed once.
        unsafe { FindVolumeClose(self.0) };
    }
}

/// The names of all volumes, as `\\?\Volume{GUID}` device names.
fn volume_devices() -> Vec<OsString> {
    let mut devices = Vec::new();
    let mut name = [0u16; 128];
    // SAFETY: `name` is writable for the length passed.
    let search = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if search == INVALID_HANDLE_VALUE {
        return devices;
    }
    let search = VolumeSearch(search);
    while devices.len() < MAX_VOLUMES {
        let units = wide_units(from_wide(&name));
        if let Some(device) = winpath::volume_device_prefix(&units) {
            devices.push(OsString::from_wide(device));
        }
        // SAFETY: the search handle is open and `name` is writable for the length passed.
        if unsafe { FindNextVolumeW(search.0, name.as_mut_ptr(), name.len() as u32) } == 0 {
            // "No more files" ends the list; so does any other failure.
            break;
        }
    }
    devices
}

/// `GetVolumePathNamesForVolumeNameW`: drive letters and mounted folders of
/// a volume root (`\\?\Volume{GUID}\`).
fn mount_paths(root: &OsStr) -> Vec<String> {
    let Ok(wide) = wide_nul(root) else {
        return Vec::new();
    };
    let mut buf = vec![0u16; 1024];
    for _ in 0..4 {
        let mut needed = 0u32;
        // SAFETY: `wide` is NUL-terminated; `buf` is writable for the length passed.
        let ok = unsafe {
            GetVolumePathNamesForVolumeNameW(
                wide.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut needed,
            )
        };
        if ok != 0 {
            return buf
                .split(|&c| c == 0)
                .take_while(|s| !s.is_empty())
                .map(string_from_wide)
                .collect();
        }
        let more = io::Error::last_os_error().raw_os_error() == Some(ERROR_MORE_DATA as i32);
        if !more || needed as usize <= buf.len() || needed > 1 << 20 {
            break;
        }
        buf.resize(needed as usize + 1, 0);
    }
    Vec::new()
}

/// All volumes with their extents and mount paths, through query handles.
pub(crate) fn enumerate_volumes() -> Vec<Volume> {
    volume_devices()
        .into_iter()
        .map(|device| {
            let handle = open_device(&device, Access::Query).ok();
            let extents = handle
                .as_ref()
                .and_then(|h| h.volume_disk_extents().ok())
                .filter(|e| !e.is_empty());
            let device_number = handle.as_ref().and_then(|h| h.device_number().ok());
            let mut volume = Volume {
                device,
                extents,
                device_number,
                mount_paths: Vec::new(),
            };
            volume.mount_paths = mount_paths(&volume.root());
            volume
        })
        .collect()
}

fn volume_info_with(
    call: impl FnOnce(&mut [u16; 261], &mut [u16; 261], &mut u32, &mut u32, &mut u32) -> i32,
) -> io::Result<VolumeInfo> {
    let (mut label, mut fs) = ([0u16; 261], [0u16; 261]);
    let (mut serial, mut max_component, mut flags) = (0u32, 0u32, 0u32);
    if call(
        &mut label,
        &mut fs,
        &mut serial,
        &mut max_component,
        &mut flags,
    ) == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(VolumeInfo {
        label: string_from_wide(&label),
        fs_name: string_from_wide(&fs),
        serial,
        max_component,
        flags,
    })
}

/// `GetVolumeInformationByHandleW` on any open file or directory of the volume.
pub(crate) fn volume_info_by_handle(file: &File) -> io::Result<VolumeInfo> {
    volume_info_with(|label, fs, serial, max_component, flags| {
        // SAFETY: the handle is open; all buffers are writable for the lengths passed.
        unsafe {
            GetVolumeInformationByHandleW(
                file.as_raw_handle(),
                label.as_mut_ptr(),
                label.len() as u32,
                serial,
                max_component,
                flags,
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        }
    })
}

/// `GetVolumeInformationW` on a root path with trailing backslash. Fails
/// with error 1005 on a raw volume.
pub(crate) fn volume_info_by_root(root: &OsStr) -> io::Result<VolumeInfo> {
    let wide = wide_nul(root)?;
    volume_info_with(|label, fs, serial, max_component, flags| {
        // SAFETY: `wide` is NUL-terminated; all buffers are writable for the lengths passed.
        unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                serial,
                max_component,
                flags,
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        }
    })
}

/// `GetDiskFreeSpaceExW`: bytes available to the caller (quota-aware) in a
/// directory that exists.
pub(crate) fn free_space(dir: &Path) -> io::Result<u64> {
    // "If this parameter is a UNC name, it must include a trailing backslash."
    let mut with_slash = dir.as_os_str().to_os_string();
    if !wide_units(&with_slash).ends_with(&[b'\\' as u16]) {
        with_slash.push("\\");
    }
    let wide = wide_nul(&with_slash)?;
    let mut available = 0u64;
    // SAFETY: `wide` is NUL-terminated; the output pointer is valid, the others may be null.
    if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, null_mut(), null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(available)
}

/// Opens a file or directory without any access right, following every link
/// on the way: the handle is on the real object. Creates nothing.
pub(crate) fn open_for_query(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(0)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// `GetFinalPathNameByHandleW`: the fully resolved path of an open object,
/// as `\\?\Volume{GUID}\...` (`guid`) or as `\\?\C:\...` / `\\?\UNC\...`.
/// The GUID form fails on network shares and on drives that were created
/// without the mount manager.
pub(crate) fn final_path(file: &File, guid: bool) -> io::Result<OsString> {
    let flags = FILE_NAME_NORMALIZED
        | if guid {
            VOLUME_NAME_GUID
        } else {
            VOLUME_NAME_DOS
        };
    // SAFETY: the handle is open and `buf` is writable for the length passed.
    fill_wide(|buf| unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            flags,
        )
    })
}

/// `WNNC_NET_SMB` (= `WNNC_NET_LANMAN`): the network provider of SMB shares.
pub(crate) const WNNC_NET_SMB: u32 = 0x0002_0000;

/// The network provider (`WNNC_NET_*`) that serves an open remote file or
/// directory (`FileRemoteProtocolInfo`). Fails for a local object.
pub(crate) fn remote_protocol(file: &File) -> io::Result<u32> {
    let mut info = FILE_REMOTE_PROTOCOL_INFO::default();
    // SAFETY: the handle is open and `info` is a writable structure of the size passed.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileRemoteProtocolInfo,
            (&mut info as *mut FILE_REMOTE_PROTOCOL_INFO).cast(),
            std::mem::size_of::<FILE_REMOTE_PROTOCOL_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.Protocol)
}

/// The volume device (`\\?\Volume{GUID}`) an open file or directory is on.
pub(crate) fn volume_device_of(file: &File) -> io::Result<OsString> {
    let path = final_path(file, true)?;
    let units = wide_units(&path);
    winpath::volume_device_prefix(&units)
        .map(OsString::from_wide)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the final path names no volume"))
}

/// The Windows directory of the running system (`C:\Windows`).
fn system_windows_dir() -> io::Result<PathBuf> {
    let mut buf = [0u16; 300];
    // SAFETY: `buf` is writable for the length passed.
    let n = unsafe { GetSystemWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(OsString::from_wide(&buf[..n])))
}

/// The device of the volume Windows runs from.
pub(crate) fn system_volume_device() -> Option<OsString> {
    let dir = system_windows_dir().ok()?;
    volume_device_of(&open_for_query(&dir).ok()?).ok()
}

/// The disks that carry the volume Windows runs from.
pub(crate) fn system_disks() -> Vec<u32> {
    let extents = system_volume_device()
        .and_then(|device| open_device(&device, Access::Query).ok())
        .and_then(|volume| volume.volume_disk_extents().ok())
        .unwrap_or_default();
    let mut disks: Vec<u32> = extents.iter().map(|e| e.disk).collect();
    disks.sort_unstable();
    disks.dedup();
    disks
}

/// The names under which this machine is reachable (NetBIOS and DNS names).
pub(crate) fn computer_names() -> Vec<String> {
    let mut names = Vec::new();
    for format in [
        ComputerNameNetBIOS,
        ComputerNameDnsHostname,
        ComputerNameDnsFullyQualified,
        ComputerNamePhysicalNetBIOS,
        ComputerNamePhysicalDnsHostname,
        ComputerNamePhysicalDnsFullyQualified,
    ] {
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        // SAFETY: `buf` is writable for `len` units.
        if unsafe { GetComputerNameExW(format, buf.as_mut_ptr(), &mut len) } != 0 {
            let name = string_from_wide(&buf[..(len as usize).min(buf.len())]);
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// The serial numbers of the file systems on all local volumes that are
/// stored on a disk and reachable through a drive letter or mounted folder.
/// (Volumes without disk extents - an empty card reader, a CD drive - are not
/// asked: there is nothing on them to lead back to. Volumes without a mount
/// path are not asked either: asking would mount them, and nothing can share
/// a volume that has no path.)
pub(crate) fn local_volume_serials() -> Vec<u32> {
    enumerate_volumes()
        .iter()
        .filter(|volume| volume.extents.is_some() && volume.is_reachable())
        .filter_map(|volume| volume_info_by_root(&volume.root()).ok())
        .map(|info| info.serial)
        .collect()
}

/// Reads a `REG_MULTI_SZ` value of the memory-management key; empty when it
/// does not exist or cannot be read.
fn memory_management_strings(value: &str) -> Vec<String> {
    let (Ok(key), Ok(value)) = (
        wide_nul(r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management"),
        wide_nul(value),
    ) else {
        return Vec::new();
    };
    let mut buf = vec![0u16; 2048];
    for _ in 0..3 {
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: key and value names are NUL-terminated; `buf` is writable for `size` bytes.
        let rc = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_MULTI_SZ,
                null_mut(),
                buf.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if rc == 0 {
            buf.truncate((size as usize / 2).min(buf.len()));
            return buf
                .split(|&c| c == 0)
                .filter(|s| !s.is_empty())
                .map(string_from_wide)
                .collect();
        }
        if rc != ERROR_MORE_DATA || size as usize > 1 << 20 {
            break;
        }
        buf.resize(size as usize / 2 + 1, 0);
    }
    Vec::new()
}

/// The drive letter of a page-file entry: `D:\pagefile.sys 0 0` (configured)
/// or `\??\D:\pagefile.sys` (existing). `?:\pagefile.sys` (the system picks
/// the drive) has none.
pub(crate) fn page_file_letter(entry: &str) -> Option<char> {
    let path = entry.strip_prefix(r"\??\").unwrap_or(entry);
    let mut chars = path.chars();
    match (chars.next(), chars.next()) {
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic() => {
            Some(letter.to_ascii_uppercase())
        }
        _ => None,
    }
}

/// Drive letters on which a page file is configured (it is recreated at the
/// next boot) or exists right now.
pub(crate) fn page_file_letters() -> Vec<char> {
    let mut letters: Vec<char> = ["PagingFiles", "ExistingPageFiles"]
        .iter()
        .flat_map(|value| memory_management_strings(value))
        .filter_map(|entry| page_file_letter(&entry))
        .collect();
    letters.sort_unstable();
    letters.dedup();
    letters
}
