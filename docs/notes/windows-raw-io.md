# Windows raw disk I/O, device enumeration and the destination guard (reference sheet)

Facts collected while reviewing ARCHITECTURE.md sections 2, 5.2, 6.8 and 6.10 and the PROTOCOL.md
commands that touch devices and output paths. Companion sheets: [ntfs-on-disk.md](ntfs-on-disk.md)
(structures) and [quick-format-behaviour.md](quick-format-behaviour.md) (what a format and Windows
itself overwrite).

Evidence level of every statement:

- **[D]** Microsoft documentation. `learn.microsoft.com` was not reachable from the review
  environment; the Markdown *sources* of the pages were read in full from the public
  `MicrosoftDocs` repositories (section 15 gives the file of every page used).
- **[S]** read in source code: Rust std 1.97.0 (`rust-src`), `windows-sys` 0.61.2, Microsoft's
  published storage class driver (`microsoft/Windows-driver-samples`, `storage/class/classpnp` and
  `storage/class/disk`), mingw-w64 11 `winioctl.h`, CPython 3.13 `ntpath.py`, Wine's
  `dlls/ntdll/tests/error.c` (a conformance test that is run against real Windows).
- **[C]** compiled here. Every Rust snippet in this sheet was copied by script out of a probe crate
  (not part of the repository) for which `cargo check --target x86_64-pc-windows-msvc` and
  `cargo build --target x86_64-pc-windows-gnu` succeed (Rust 1.97.0, `windows-sys` 0.61.2).
- **[X]** executed under Wine 9.0 (the `-gnu` build). Wine is a re-implementation: an [X] result
  proves what the *code* does and what Wine implements, not what Windows does. Raw disk reads could
  not be run at all (Wine has a dummy `\\.\PhysicalDrive0` without `IOCTL_DISK_GET_LENGTH_INFO`).
- **[L]** executed on Linux with `mkntfs` / `ntfs-3g` 2022.10.3.
- **[R]** reported: search-engine excerpts of pages that could not be opened (Old New Thing, KB articles).
- **[I]** inference, not verified.
- **[CI]** cannot be decided here; the Windows CI job has to pin it down (list in section 14).

## 1. Crate setup

```toml
[target.'cfg(windows)'.dependencies.windows-sys]
version = "0.61.2"      # only dependency: windows-link 0.2.1 (raw-dylib, no import libraries); MSRV 1.71
features = [
    "Win32_Foundation",
    "Win32_Security",
    "Win32_Storage_FileSystem",
    "Win32_System_IO",
    "Win32_System_Ioctl",
    "Win32_System_Threading",
    # only for the optional recipes in sections 7.4 and 13:
    "Win32_Storage_Vhd",
    "Win32_System_Registry",
    "Win32_System_SystemInformation",
]
```

Where the names live in 0.61.2 [S][C] (the surprising ones in bold):

| Feature | Items used |
|---------|-----------|
| `Win32_Foundation` | `HANDLE` (`*mut c_void`, same as std's `RawHandle`), `INVALID_HANDLE_VALUE`, `FILETIME`, `GENERIC_READ`, `GetLastError`, `CloseHandle`, `ERROR_*`, `NTSTATUS`, `RtlNtStatusToDosError` |
| `Win32_Storage_FileSystem` | `CreateFileW` (also needs `Win32_Security`), `ReadFile` (also needs `Win32_System_IO`), `GetFullPathNameW`, `GetFinalPathNameByHandleW`, `GetVolumePathNameW`, `GetVolumeNameForVolumeMountPointW`, `GetVolumePathNamesForVolumeNameW`, `GetVolumeInformationW`, `GetVolumeInformationByHandleW`, `GetDiskFreeSpaceExW`, `GetDriveTypeW`, `GetFileAttributesW`, `GetFileInformationByHandle`, `SetFileTime`, `MoveFileExW`, `FILE_SHARE_*`, `FILE_FLAG_*`, `FILE_ATTRIBUTE_*`, `FILE_WRITE_ATTRIBUTES`, `VOLUME_NAME_GUID`, **`IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`**, **`STORAGE_BUS_TYPE` / `BusType*`**, **`FILE_DEVICE_DISK`**, **`PARTITION_*_GUID`** |
| `Win32_System_IO` | `DeviceIoControl`, `OVERLAPPED`, `CancelSynchronousIo`, `CancelIoEx` |
| `Win32_System_Ioctl` | `IOCTL_DISK_GET_LENGTH_INFO`, `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`, `IOCTL_STORAGE_QUERY_PROPERTY`, `IOCTL_STORAGE_GET_DEVICE_NUMBER`, `IOCTL_STORAGE_CHECK_VERIFY2`, `GET_LENGTH_INFORMATION`, `DISK_GEOMETRY_EX`, `STORAGE_PROPERTY_QUERY`, `STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR`, `STORAGE_DEVICE_DESCRIPTOR` (**only with `Win32_Storage_FileSystem` enabled too**), `STORAGE_DEVICE_NUMBER`, **`VOLUME_DISK_EXTENTS`, `DISK_EXTENT`** (the IOCTL code itself is in `Storage::FileSystem`), `DEVICE_SEEK_PENALTY_DESCRIPTOR`, `DEVICE_TRIM_DESCRIPTOR`, `Storage*Property`, `PropertyStandardQuery`, `PARTITION_LDM` (0x42), `PARTITION_SPACES` (0xE7) |
| `Win32_Security` | `SECURITY_ATTRIBUTES` (in the signature of `CreateFileW`), `GetTokenInformation`, `TokenElevation`, `TOKEN_ELEVATION`, `TOKEN_QUERY` |
| `Win32_System_Threading` | `GetCurrentProcess`, `GetCurrentThread`, `OpenProcessToken` |
| `Win32_Storage_Vhd` | `GetStorageDependencyInformation`, `STORAGE_DEPENDENCY_INFO`, `STORAGE_DEPENDENCY_INFO_TYPE_2`, `GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES` (imports `virtdisk.dll`) |
| `Win32_System_Registry` | `RegGetValueW`, `HKEY_LOCAL_MACHINE`, `RRF_RT_REG_MULTI_SZ` |
| `Win32_System_SystemInformation` | `GetComputerNameExW`, `ComputerNameNetBIOS`, `ComputerNameDnsHostname`, `GetSystemWindowsDirectoryW` |

Pitfalls of the bindings [S]:

- `BOOLEAN` struct fields are typed as Rust `bool` (`STORAGE_DEVICE_DESCRIPTOR.RemovableMedia`,
  `DEVICE_SEEK_PENALTY_DESCRIPTOR.IncursSeekPenalty`, `DEVICE_TRIM_DESCRIPTOR.TrimEnabled`). A
  driver that returns a byte other than 0/1 makes reading the struct undefined behaviour: read
  the byte at `offset_of!` instead (done below).
- `DRIVE_REMOTE` and friends are in `Win32::System::WindowsProgramming` (not enabled above; the
  values are 0 unknown, 1 no root, 2 removable, 3 fixed, 4 remote, 5 cdrom, 6 ramdisk).

What std already provides on stable [S]: `OpenOptionsExt::{access_mode, share_mode, custom_flags,
attributes}` (1.10), `FileExt::seek_read` (1.15), `File::set_times`, `FileTimes`,
`FileTimesExt::set_created` (1.75), `std::path::absolute` (1.79), `fs::canonicalize`. **Not** stable
in 1.97: the path-based `fs::set_times` / `set_times_nofollow`, `MetadataExt::volume_serial_number`
/ `file_index`, `junction_point` — use the recipes below instead.

Imports of the probe (a superset of what the snippets below use; `wide`, `from_wide_nul` and
`last_err` are three-line helpers: NUL-terminated UTF-16 from an `OsStr`, the reverse, and
`io::Error::last_os_error()`):

```rust
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ffi::{c_void, OsStr, OsString};
use std::fs::{File, FileTimes, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{FileExt, FileTimesExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr::{null, null_mut};
use std::time::{Duration, SystemTime};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, RtlNtStatusToDosError, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, FILETIME,
    GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS,
};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetDriveTypeW, GetFileAttributesW, GetFileInformationByHandle,
    GetFinalPathNameByHandleW, GetFullPathNameW, GetVolumeInformationByHandleW, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, GetVolumePathNamesForVolumeNameW, MoveFileExW, ReadFile,
    SetFileTime,
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DEVICE_DISK,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, INVALID_FILE_ATTRIBUTES,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING, VOLUME_NAME_GUID,
};
use windows_sys::Win32::Storage::Vhd::{
    GetStorageDependencyInformation, GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES, STORAGE_DEPENDENCY_INFO,
    STORAGE_DEPENDENCY_INFO_TYPE_2, STORAGE_DEPENDENCY_INFO_VERSION_2,
};
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageAccessAlignmentProperty, StorageDeviceProperty, StorageDeviceSeekPenaltyProperty,
    StorageDeviceTrimProperty, DEVICE_SEEK_PENALTY_DESCRIPTOR, DEVICE_TRIM_DESCRIPTOR, DISK_EXTENT,
    DISK_GEOMETRY_EX, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_LENGTH_INFO,
    IOCTL_STORAGE_CHECK_VERIFY2, IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
    STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_ID,
    STORAGE_PROPERTY_QUERY, VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_MULTI_SZ};
use windows_sys::Win32::System::SystemInformation::{
    ComputerNameDnsHostname, ComputerNameNetBIOS, GetComputerNameExW, GetSystemWindowsDirectoryW, COMPUTER_NAME_FORMAT,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, OpenProcessToken};
use windows_sys::Win32::System::IO::{CancelSynchronousIo, DeviceIoControl, OVERLAPPED};
```

## 2. Opening devices (S1)

Rules from the `CreateFileW` page, section "Physical Disks and Volumes" [D]:

- Disk path: `\\.\PhysicalDriveN`, "hard disk numbers start at zero".
- "The caller must have administrative privileges." "The dwCreationDisposition parameter must have
  the OPEN_EXISTING flag." "When opening a volume or floppy disk, the dwShareMode parameter must
  have the FILE_SHARE_WRITE flag."
- "The dwDesiredAccess parameter can be zero, allowing the application to query device attributes
  without accessing a device."
- Volume device: `\\.\X:` or `\\?\Volume{GUID}` **without** a trailing backslash; `\\.\C:\` "opens
  the file system of the C: volume" (the root directory).

What std does [S]: `OpenOptions` defaults to share mode `READ | WRITE | DELETE`;
`OpenOptions::new().access_mode(a).open(r"\\.\PhysicalDrive1")` passes the path through unchanged
(it is shorter than 248 units and starts with `\\`) and calls
`CreateFileW(path, a, share, NULL, OPEN_EXISTING, custom_flags | attributes, NULL)`. `access_mode(0)`
is accepted.

Consequences for S1:

- Two access levels exist and both are needed: **query** (access 0, for enumeration and for the
  volume handles of the destination guard) and **read** (`GENERIC_READ`, only for the selected
  source disk). `IOCTL_DISK_GET_LENGTH_INFO` needs the read handle, everything else in section 4
  works on a query handle. [S: access bits in `winioctl.h`] [X: Wine returns error 5 for
  `IOCTL_DISK_GET_LENGTH_INFO` on an access-0 handle]
- Read-only access does **not** stop state-changing control codes: `FSCTL_LOCK_VOLUME`,
  `FSCTL_DISMOUNT_VOLUME` and `IOCTL_DISK_UPDATE_PROPERTIES` are defined with `FILE_ANY_ACCESS`.
  [S: `winioctl.h` lines 1471, 1473, 675] S1 therefore needs a closed allow-list inside the one
  `ioctl()` wrapper: `IOCTL_DISK_GET_LENGTH_INFO`, `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`,
  `IOCTL_STORAGE_QUERY_PROPERTY`, `IOCTL_STORAGE_GET_DEVICE_NUMBER`, `IOCTL_STORAGE_CHECK_VERIFY2`,
  `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` (plus `GetStorageDependencyInformation`).
  `IOCTL_VOLUME_OFFLINE` and `IOCTL_DISK_SET_DISK_ATTRIBUTES` require read **and** write access and
  cannot succeed on these handles. [S: line 3240; value 0x7C0F4]
- A `"disk"` source must be exactly `\\.\PhysicalDriveN`. A volume path (`\\.\E:`) would also open
  and answer `IOCTL_DISK_GET_LENGTH_INFO` ("the length of the specified disk, volume, or partition"
  [D]), but its offsets are volume-relative and "volume handles do not have access to the full
  volume" [D] — the destination guard, which compares disk-absolute extents, would be wrong.
  Check after opening: `IOCTL_STORAGE_GET_DEVICE_NUMBER` must report `DeviceType ==
  FILE_DEVICE_DISK (7)` and `PartitionNumber == 0` [CI], and `DeviceNumber` (not the digits in the
  path string) is the disk number the guard uses.
- Image files: open with `share_mode(FILE_SHARE_READ)`. While that handle is open, opening the
  file for writing, truncating it and deleting it all fail with `ERROR_SHARING_VIOLATION` (32) —
  for the engine itself and for every other process. [X] This enforces S4 in the kernel instead
  of by path comparison (a path comparison misses hard links, short names, junctions and `subst`).
- Errors of the open: 2 no such disk number [X], 5 not elevated [D], 32 sharing violation,
  21 not ready [I].

```rust
/// Full sharing. CreateFile requires FILE_SHARE_WRITE for volumes; on a disk any narrower mode
/// fails with ERROR_SHARING_VIOLATION (32) as soon as another handle to the device is open.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// Data handle on `\\.\PhysicalDriveN`: GENERIC_READ only. Synchronous: never pass
/// FILE_FLAG_OVERLAPPED, std's `seek_read` is written for synchronous handles. FILE_FLAG_NO_BUFFERING
/// is not needed: the disk driver enforces sector-multiple offset and length with or without it.
pub fn open_disk_for_read(path: &str) -> io::Result<File> {
    OpenOptions::new().access_mode(GENERIC_READ).share_mode(SHARE_ALL).open(path)
}

/// Query handle: desired access 0 ("query device attributes without accessing a device").
/// Enough for every FILE_ANY_ACCESS IOCTL (QUERY_PROPERTY, GET_DEVICE_NUMBER, GEOMETRY_EX,
/// GET_VOLUME_DISK_EXTENTS). Use for enumeration and for volume handles in the destination guard.
pub fn open_device_for_query(path: &OsStr) -> io::Result<File> {
    OpenOptions::new().access_mode(0).share_mode(SHARE_ALL).open(path)
}

/// The same data open spelled with windows-sys directly (what std does underneath).
pub fn open_disk_for_read_raw(path: &str) -> io::Result<File> {
    let w = wide(path);
    let h = unsafe { CreateFileW(w.as_ptr(), GENERIC_READ, SHARE_ALL, null(), OPEN_EXISTING, 0, null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return Err(last_err());
    }
    Ok(File::from(unsafe { OwnedHandle::from_raw_handle(h) }))
}
```

Reading a disk whose volumes are mounted and in use is allowed: the Vista restrictions on direct
access concern *writes* to sectors of a mounted volume. [R: KB 942448] No lock or dismount is
needed (and S1 forbids them). What is read is the on-disk state: anything the file system still
holds in memory is not there yet.

## 3. Positional reads

Alignment:

- Offset and length must be multiples of the **logical** sector size, with or without
  `FILE_FLAG_NO_BUFFERING`: the disk class driver itself rejects anything else with
  `STATUS_INVALID_PARAMETER` (Win32 87). [S: `disk.c`, `DiskReadWriteVerification`: "the number of
  bytes to transfer and the byte offset are a multiple of the sector size"] [D: "File access sizes,
  including the optional file offset in the OVERLAPPED structure, … must be for a number of bytes
  that is an integer multiple of the volume sector size"]
- Buffer address: "should be physical sector-aligned … Depending on the disk, this requirement may
  not be enforced." [D: File Buffering] The rule is written for `FILE_FLAG_NO_BUFFERING`; "if the
  noncached option is specified when opening a volume, buffer alignment is enforced regardless of
  the file system". [D: CreateFileW] Reading every device request into a 4096-aligned buffer
  satisfies all cases (page size ≥ any physical sector size in use) and is also what lets classpnp
  move a 64 KiB request in one piece ("If the buffer is page-aligned, that's one less page
  crossing"). [S: `class.c`, `ServiceTransferRequest`] Sub-slices at sector multiples of such a
  buffer (the per-sector retries of `read_resilient`) stay sector-aligned.
- `FILE_FLAG_NO_BUFFERING` is not required on `\\.\PhysicalDriveN` (there is no file cache between
  the handle and the disk driver [I]; Microsoft: "You should assume that all Microsoft file systems
  open volume handles as noncached" [D]). If it is set anyway, the rules above are simply
  mandatory. Do **not** set it for image files: their length need not be a sector multiple.
- 4Kn disks (logical 4096): every offset and length in 4096 units; 512e disks (logical 512,
  physical 4096): 512 units, but an unreadable physical sector takes all eight logical sectors with
  it, so retry granularity below the physical sector size only multiplies the waiting time. [I]

`FileExt::seek_read` [S: `sys/pal/windows/handle.rs`]:

- It calls `NtReadFile` with an explicit byte offset (the equivalent of `ReadFile` with an
  `OVERLAPPED` offset on a synchronous handle [D: "the read operation starts at the offset that is
  specified in the OVERLAPPED structure and ReadFile does not return until the read operation is
  complete"]). The length is clamped to `u32::MAX`.
- It returns the number of bytes read, which may be short, and `Ok(0)` for `STATUS_END_OF_FILE`.
  `BlockSource::read_at -> io::Result<()>` therefore needs the loop below and must treat 0 as an
  error.
- On `STATUS_PENDING` it waits on the file handle. That is only correct for synchronous handles:
  never open the device with `FILE_FLAG_OVERLAPPED`.
- Requests on one synchronous handle are serialised by Windows [I]: a `read_file` preview issued
  while a job runs waits for the job's current request (which can be a long retry on a bad
  sector). A second handle for interactive reads avoids the queue.

Transfer size: classpnp "processes Io requests, splitting them if they are larger than what the
hardware can handle at a time" [S: `class.c`], so a large `ReadFile` is legal; the buffer is locked
in memory for the duration ("ReadFile may fail with ERROR_NOT_ENOUGH_QUOTA, which means the calling
process's buffer could not be page-locked" [D]), and one unreadable sector fails the whole call.
8 MiB for the sweep with 64 KiB as the first retry size is reasonable; ntfs-3g limits a single
Windows read to 2 GiB "to be on the safe side". [S: `win32_io.c`]

End of the device: do not go there. Take `size()` from `IOCTL_DISK_GET_LENGTH_INFO`, clamp the last
chunk, and treat a short count as an error. What Windows returns for a read at or across the end
of a physical disk is not documented: for *files* it is `ERROR_HANDLE_EOF` (38) or a short count
[D]; the disk driver deliberately lets sector-aligned requests beyond its idea of the size through
("Send it down to the drive and let the hardware decide for itself" [S: `disk.c`]), which would
surface as `STATUS_NONEXISTENT_SECTOR` = `ERROR_SECTOR_NOT_FOUND` (27) [I]. [CI]

Volume handles are not used for reading (by design) and should stay unused: "To read or write to
the last few sectors of the volume, you must call DeviceIoControl and specify
FSCTL_ALLOW_EXTENDED_DASD_IO" [D] — on NTFS that is the backup boot sector.

How long a failing read can block [S: `disk.h`, `classp.h`, `retry.c`, `class.c`]: the per-request
timeout of the disk class driver is `SCSI_DISK_TIMEOUT` = 10 s unless the `TimeOutValue` registry
value says otherwise; a SCSI *medium error* ("bad block") is not retried by the class driver
(`retry = FALSE`, status `STATUS_DEVICE_DATA_ERROR`), timeouts and hardware errors are retried a
limited number of times; the drive's own recovery comes on top. A per-sector retry loop can
therefore spend tens of seconds per sector: check for cancellation between attempts, not per
chunk. `CancelSynchronousIo` can interrupt a blocked synchronous call: it needs a thread handle
with `THREAD_TERMINATE`, the cancelled call fails with `ERROR_OPERATION_ABORTED` (995), and "other
operations can continue toward completion before they are actually canceled". [D]

```rust
/// Heap buffer whose address is a multiple of 4096 (page size = largest physical sector size in
/// use; page alignment also lets classpnp move a 64 KiB request in one piece).
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    pub const ALIGN: usize = 4096;
    pub fn new(len: usize) -> AlignedBuf {
        assert!(len > 0 && len % 512 == 0);
        let layout = Layout::from_size_align(len, Self::ALIGN).unwrap();
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        AlignedBuf { ptr, len }
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, Layout::from_size_align(self.len, Self::ALIGN).unwrap()) }
    }
}

/// Positional read that either fills `buf` or fails. `seek_read` may return short counts
/// (end of device) and returns Ok(0) for STATUS_END_OF_FILE, so both are turned into errors:
/// the caller must never ask for bytes at or beyond `size()`.
pub fn read_exact_at(f: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match f.seek_read(buf, offset) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past end of device")),
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

/// Asks Windows to cancel the synchronous I/O a thread is blocked in (a dying disk can hold one
/// request for a long time). The cancelled call fails with ERROR_OPERATION_ABORTED (995); it may
/// also still complete normally. The handle needs THREAD_TERMINATE (JoinHandle::as_raw_handle of
/// the reader thread); the pseudo handle below only proves the signature.
pub fn cancel_sync_io_of_current_thread() -> bool {
    unsafe { CancelSynchronousIo(GetCurrentThread()) != 0 }
}
```

## 4. IOCTLs

| Control code | Value | Access [S] | Handle | Output |
|--------------|-------|-----------|--------|--------|
| `IOCTL_DISK_GET_LENGTH_INFO` | 0x0007405C | **read** | disk (or volume) | `GET_LENGTH_INFORMATION { Length: i64 }` bytes |
| `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` | 0x000700A0 | any | disk | `DISK_GEOMETRY_EX`: `Geometry.BytesPerSector` (logical), `DiskSize` |
| `IOCTL_STORAGE_QUERY_PROPERTY` | 0x002D1400 | any | disk | by `PropertyId`: `StorageAccessAlignmentProperty` (6), `StorageDeviceProperty` (0), `StorageDeviceSeekPenaltyProperty` (7), `StorageDeviceTrimProperty` (8) |
| `IOCTL_STORAGE_GET_DEVICE_NUMBER` | 0x002D1080 | any | disk or volume | `STORAGE_DEVICE_NUMBER { DeviceType, DeviceNumber, PartitionNumber }` |
| `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` | 0x00560000 | any | volume | `VOLUME_DISK_EXTENTS { NumberOfDiskExtents, Extents[] }` of `DISK_EXTENT { DiskNumber, StartingOffset, ExtentLength }` (bytes from the start of the disk) |
| `IOCTL_STORAGE_CHECK_VERIFY2` | 0x002D0800 | any | disk | none; succeeds while the device is present and ready |

Notes:

- Sector sizes: `STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR.BytesPerLogicalSector` /
  `BytesPerPhysicalSector` [D]; "Most current Windows APIs, such as IOCTL_DISK_GET_DRIVE_GEOMETRY
  …, will return the logical sector size" [D], so the geometry fallback yields the logical size
  only (then assume physical = logical). Validate what the device says (power of two, physical ≥
  logical); devices behind USB bridges may not answer the alignment query at all. [I]
  512n = 512/512, 512e = 512/4096, 4Kn = 4096/4096.
- Never take on-disk units from the device: the same disk can present another logical sector size
  behind a different bridge. [R] The design already does this right (BPB values, fixed 512-byte
  fixup stride).
- `STORAGE_DEVICE_DESCRIPTOR` [D]: the four `*Offset` members are byte offsets from the start of
  the returned buffer to NUL-terminated ASCII strings, **0 when the device has no such string**.
  Model = vendor + product, trimmed (SATA disks usually have an empty vendor). The serial number
  is often missing or not the drive's own behind USB bridges. [R] `RemovableMedia` says that the
  *media* can be removed (card readers, some flash drives); it is false for ordinary USB hard
  disks, so "external" must be derived from `BusType` (USB 7, 1394 4, SD 12, MMC 13).
- `BusType` values [S]: Unknown 0, Scsi 1, Atapi 2, Ata 3, 1394 4, Ssa 5, Fibre 6, Usb 7, RAID 8,
  iScsi 9, Sas 10, Sata 11, Sd 12, Mmc 13, **Virtual 14, FileBackedVirtual 15 (attached VHD/VHDX),
  Spaces 16 (Storage Spaces virtual disk)**, Nvme 17, SCM 18, Ufs 19.
- `IOCTL_STORAGE_GET_DEVICE_NUMBER`: the values are "guaranteed to remain unchanged until the device
  is removed or the system is restarted" — not longer. [D] `DeviceNumber` is a `PhysicalDrive`
  number only if `DeviceType == FILE_DEVICE_DISK` (7); CD-ROM devices have their own numbering.
  `PartitionNumber` is "–1" when the device cannot be partitioned [D]. On a volume that spans
  several disks the call fails (`ERROR_INVALID_FUNCTION`). [R: Old New Thing 2020-10-21]
- `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`: "When the number of extents returned is greater than one
  (1), the error code ERROR_MORE_DATA is returned. You should call DeviceIoControl again,
  allocating enough buffer space based on the value of NumberOfDiskExtents". [D] An empty result
  must be treated as "unknown", never as "no overlap" (Wine returns zero extents [X]).
- SSD / TRIM: `IncursSeekPenalty == false` means no seek penalty (SSD); `TrimEnabled == true` means
  a format will have discarded the old data (see quick-format-behaviour.md section 6). Both
  queries fail on many USB bridges: report "unknown", not "HDD".

```rust
fn ioctl(h: HANDLE, code: u32, input: &[u8], out: &mut [u8]) -> io::Result<u32> {
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            code,
            if input.is_empty() { null() } else { input.as_ptr().cast() },
            input.len() as u32,
            if out.is_empty() { null_mut() } else { out.as_mut_ptr().cast() },
            out.len() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        Err(last_err())
    } else {
        Ok(returned)
    }
}

/// 8-byte aligned scratch space for variable-length IOCTL output.
struct OutBuf(Vec<u64>);

impl OutBuf {
    fn new(bytes: usize) -> OutBuf {
        OutBuf(vec![0u64; bytes.div_ceil(8)])
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.0.as_mut_ptr().cast(), self.0.len() * 8) }
    }
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast(), self.0.len() * 8) }
    }
    unsafe fn read<T: Copy>(&self) -> T {
        assert!(size_of::<T>() <= self.0.len() * 8);
        std::ptr::read(self.0.as_ptr().cast::<T>())
    }
}

/// IOCTL_DISK_GET_LENGTH_INFO — needs FILE_READ_ACCESS on the handle (fails with
/// ERROR_ACCESS_DENIED on an access-0 handle). Works on disks, partitions and volumes.
pub fn disk_length(f: &File) -> io::Result<u64> {
    let mut out = OutBuf::new(size_of::<GET_LENGTH_INFORMATION>());
    ioctl(f.as_raw_handle(), IOCTL_DISK_GET_LENGTH_INFO, &[], out.bytes_mut())?;
    let info: GET_LENGTH_INFORMATION = unsafe { out.read() };
    Ok(info.Length as u64)
}

/// IOCTL_DISK_GET_DRIVE_GEOMETRY_EX — FILE_ANY_ACCESS. Returns (logical bytes per sector, disk size).
pub fn geometry_ex(f: &File) -> io::Result<(u32, u64)> {
    let mut out = OutBuf::new(size_of::<DISK_GEOMETRY_EX>() + 256);
    ioctl(f.as_raw_handle(), IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], out.bytes_mut())?;
    let g: DISK_GEOMETRY_EX = unsafe { out.read() };
    Ok((g.Geometry.BytesPerSector, g.DiskSize as u64))
}

fn property_query(f: &File, id: STORAGE_PROPERTY_ID, out: &mut OutBuf) -> io::Result<u32> {
    let q = STORAGE_PROPERTY_QUERY { PropertyId: id, QueryType: PropertyStandardQuery, AdditionalParameters: [0] };
    let qb = unsafe { std::slice::from_raw_parts((&q as *const STORAGE_PROPERTY_QUERY).cast::<u8>(), size_of::<STORAGE_PROPERTY_QUERY>()) };
    ioctl(f.as_raw_handle(), IOCTL_STORAGE_QUERY_PROPERTY, qb, out.bytes_mut())
}

#[derive(Debug, Clone, Copy)]
pub struct SectorSizes {
    pub logical: u32,
    pub physical: u32,
    /// BytesOffsetForSectorAlignment: where logical sector 0 sits inside the first physical sector.
    pub alignment_offset: u32,
}

/// IOCTL_STORAGE_QUERY_PROPERTY / StorageAccessAlignmentProperty — FILE_ANY_ACCESS. Fails on
/// devices and bridges that do not implement it: fall back to `geometry_ex` (logical only).
pub fn access_alignment(f: &File) -> io::Result<SectorSizes> {
    let mut out = OutBuf::new(size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>());
    let n = property_query(f, StorageAccessAlignmentProperty, &mut out)?;
    if (n as usize) < size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "short alignment descriptor"));
    }
    let d: STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR = unsafe { out.read() };
    Ok(SectorSizes { logical: d.BytesPerLogicalSector, physical: d.BytesPerPhysicalSector, alignment_offset: d.BytesOffsetForSectorAlignment })
}

/// Sector sizes with validation and fallback; never trusts a device that reports nonsense.
pub fn sector_sizes(f: &File) -> io::Result<SectorSizes> {
    let sane = |v: u32| v.is_power_of_two() && (512..=65536).contains(&v);
    let geo = geometry_ex(f).ok().map(|g| g.0).filter(|&v| sane(v));
    match access_alignment(f) {
        Ok(a) if sane(a.logical) => {
            let physical = if sane(a.physical) && a.physical >= a.logical { a.physical } else { a.logical };
            Ok(SectorSizes { logical: a.logical, physical, alignment_offset: a.alignment_offset })
        }
        _ => match geo {
            Some(l) => Ok(SectorSizes { logical: l, physical: l, alignment_offset: 0 }),
            None => Err(io::Error::new(io::ErrorKind::InvalidData, "device reports no usable sector size")),
        },
    }
}

#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    pub vendor: String,
    pub product: String,
    pub revision: String,
    pub serial: String,
    pub bus_type: i32,
    pub removable_media: bool,
}

fn ascii_at(buf: &[u8], offset: u32) -> String {
    let o = offset as usize;
    if o == 0 || o >= buf.len() {
        return String::new(); // 0 = "the device has no such string"
    }
    let end = buf[o..].iter().position(|&b| b == 0).map(|p| o + p).unwrap_or(buf.len());
    buf[o..end].iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '?' }).collect::<String>().trim().to_string()
}

/// IOCTL_STORAGE_QUERY_PROPERTY / StorageDeviceProperty — FILE_ANY_ACCESS.
pub fn device_identity(f: &File) -> io::Result<DeviceIdentity> {
    let mut out = OutBuf::new(4096);
    let n = property_query(f, StorageDeviceProperty, &mut out)? as usize;
    if n < offset_of!(STORAGE_DEVICE_DESCRIPTOR, RawDeviceProperties) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "short device descriptor"));
    }
    let b = &out.bytes()[..n.min(4096)];
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    // windows-sys types BOOLEAN fields as Rust `bool`; read the raw byte instead of the struct
    // so a driver that returns 0xFF cannot cause undefined behaviour.
    let removable = b[offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia)] != 0;
    Ok(DeviceIdentity {
        vendor: ascii_at(b, u32_at(offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset))),
        product: ascii_at(b, u32_at(offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset))),
        revision: ascii_at(b, u32_at(offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductRevisionOffset))),
        serial: ascii_at(b, u32_at(offset_of!(STORAGE_DEVICE_DESCRIPTOR, SerialNumberOffset))),
        bus_type: u32_at(offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType)) as i32,
        removable_media: removable,
    })
}

/// StorageDeviceSeekPenaltyProperty: Some(false) = no seek penalty = SSD. None = not reported
/// (common behind USB bridges).
pub fn incurs_seek_penalty(f: &File) -> Option<bool> {
    let mut out = OutBuf::new(size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>());
    let n = property_query(f, StorageDeviceSeekPenaltyProperty, &mut out).ok()? as usize;
    let o = offset_of!(DEVICE_SEEK_PENALTY_DESCRIPTOR, IncursSeekPenalty);
    (n > o).then(|| out.bytes()[o] != 0)
}

/// StorageDeviceTrimProperty: Some(true) = the device accepts TRIM (a format will have discarded data).
pub fn trim_enabled(f: &File) -> Option<bool> {
    let mut out = OutBuf::new(size_of::<DEVICE_TRIM_DESCRIPTOR>());
    let n = property_query(f, StorageDeviceTrimProperty, &mut out).ok()? as usize;
    let o = offset_of!(DEVICE_TRIM_DESCRIPTOR, TrimEnabled);
    (n > o).then(|| out.bytes()[o] != 0)
}

/// IOCTL_STORAGE_GET_DEVICE_NUMBER — FILE_ANY_ACCESS. On `\\.\PhysicalDriveN`: (FILE_DEVICE_DISK, N, 0).
/// On a simple volume: disk number + 1-based partition number. Fails on volumes that span disks.
/// DeviceNumber is only a PhysicalDrive number when DeviceType == FILE_DEVICE_DISK (7).
pub fn device_number(f: &File) -> io::Result<STORAGE_DEVICE_NUMBER> {
    let mut out = OutBuf::new(size_of::<STORAGE_DEVICE_NUMBER>());
    ioctl(f.as_raw_handle(), IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], out.bytes_mut())?;
    Ok(unsafe { out.read() })
}

/// IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS — FILE_ANY_ACCESS, on a volume handle
/// (`\\.\X:` or `\\?\Volume{GUID}` WITHOUT trailing backslash). All extents, not just the first.
pub fn volume_disk_extents(vol: &File) -> io::Result<Vec<DISK_EXTENT>> {
    let mut count = 1usize;
    loop {
        let bytes = offset_of!(VOLUME_DISK_EXTENTS, Extents) + count * size_of::<DISK_EXTENT>();
        let mut out = OutBuf::new(bytes);
        match ioctl(vol.as_raw_handle(), IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], out.bytes_mut()) {
            Ok(_) => {
                let n = unsafe { out.read::<u32>() } as usize;
                let base = unsafe { out.0.as_ptr().cast::<u8>().add(offset_of!(VOLUME_DISK_EXTENTS, Extents)) };
                let mut v = Vec::with_capacity(n);
                for i in 0..n.min(count) {
                    v.push(unsafe { std::ptr::read_unaligned(base.add(i * size_of::<DISK_EXTENT>()).cast::<DISK_EXTENT>()) });
                }
                return Ok(v);
            }
            // More than one extent: NumberOfDiskExtents is valid, the array is not. Retry bigger.
            Err(e) if e.raw_os_error() == Some(ERROR_MORE_DATA as i32) || e.raw_os_error() == Some(ERROR_INSUFFICIENT_BUFFER as i32) => {
                let n = unsafe { out.read::<u32>() } as usize;
                count = if n > count { n } else { count * 2 };
                if count > 4096 {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Cheap "is the device still there" probe (FILE_ANY_ACCESS, no data transfer).
pub fn check_verify(f: &File) -> io::Result<()> {
    ioctl(f.as_raw_handle(), IOCTL_STORAGE_CHECK_VERIFY2, &[], &mut []).map(|_| ())
}
```

## 5. Enumeration (`list_disks`)

1. For `n` in `0..64`: `open_device_for_query(r"\\.\PhysicalDrive{n}")`. Error 2 means "no disk
   with this number"; keep going, numbers can have gaps after devices were removed. [I]
2. On the query handle: `device_number` (expect type 7), `geometry_ex` (size and logical sector
   size — `IOCTL_DISK_GET_LENGTH_INFO` would need read access), `access_alignment`,
   `device_identity`, `incurs_seek_penalty`, `trim_enabled`. Card readers without a card exist as
   `PhysicalDriveN`; their queries fail with 21 (`ERROR_NOT_READY`): skip or show "no media". [I]
3. Partition table: read sector 0 … through a **read** handle (`parse_partition_table`). This is
   the only step that needs elevation.
4. Volumes: `FindFirstVolumeW` / `FindNextVolumeW` give `\\?\Volume{GUID}\`; strip the backslash,
   `open_device_for_query`, `volume_disk_extents`; match a partition by `(DiskNumber,
   StartingOffset)`. `GetVolumePathNamesForVolumeNameW` (with the backslash) lists drive letters
   and mounted folders; `GetVolumeInformationW` (with the backslash) gives label and file system
   and fails with 1005 (`ERROR_UNRECOGNIZED_VOLUME`) on RAW volumes.
5. `is_system`: resolve `GetSystemWindowsDirectoryW()` like a destination (section 7) and mark
   every disk in its extents.

Out-of-scope sources can be recognised without any Windows API (works for images too) [S: constants
in `windows-sys`]: dynamic disk = MBR type 0x42 or GPT type `5808c8aa-7e8f-42e0-85d2-e1e90434cfb3`
(LDM metadata) / `af9b60a0-1431-4f62-bc68-3311714a69ad` (LDM data); Storage Spaces pool member = MBR
type 0xE7 or GPT type `e75caf8f-f680-4cee-afa3-b001e56efc2d`; and with the API: `BusType` 16
(Spaces virtual disk), 15 / 14 (VHD). Basic data = `ebd0a0a2-b9e5-4433-87c0-68b6b72699c7`, MSR =
`e3c9e316-0b5c-4db8-817d-f92df00215ae`, EFI system = `c12a7328-f81f-11d2-ba4b-00a0c93ec93b`.

Before parsing sector 0 as an MBR, test it as a boot sector: an NTFS (or FAT/exFAT) boot sector
also ends in 55 AA, so a partition image or an unpartitioned ("superfloppy") disk would otherwise
be read as an MBR whose four entries are boot-code bytes. [L: `mkntfs` image: OEM `NTFS    `,
signature 55 AA, bytes 446–509 zero; on Windows-formatted volumes that area holds boot code and
message text [I]]

```rust
/// Drive letters and mounted folders of a volume GUID path (`\\?\Volume{GUID}\`, WITH trailing backslash).
pub fn volume_mount_paths(volume_guid_root: &OsStr) -> io::Result<Vec<OsString>> {
    let w = wide(volume_guid_root);
    let mut buf = vec![0u16; 1024];
    loop {
        let mut need = 0u32;
        if unsafe { GetVolumePathNamesForVolumeNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32, &mut need) } != 0 {
            break;
        }
        if unsafe { GetLastError() } != ERROR_MORE_DATA {
            return Err(last_err());
        }
        buf.resize(need as usize + 1, 0);
    }
    Ok(buf.split(|&c| c == 0).take_while(|s| !s.is_empty()).map(OsString::from_wide).collect())
}

#[derive(Debug)]
pub struct VolumeInfo {
    pub label: OsString,
    pub fs_name: OsString,
    pub serial: u32,
    pub max_component: u32,
    pub flags: u32,
}

/// GetVolumeInformationByHandleW on any file or directory handle of the volume.
pub fn volume_info_by_handle(f: &File) -> io::Result<VolumeInfo> {
    let (mut label, mut fs) = ([0u16; 261], [0u16; 261]);
    let (mut serial, mut max_comp, mut flags) = (0u32, 0u32, 0u32);
    let ok = unsafe {
        GetVolumeInformationByHandleW(f.as_raw_handle(), label.as_mut_ptr(), label.len() as u32, &mut serial, &mut max_comp, &mut flags, fs.as_mut_ptr(), fs.len() as u32)
    };
    if ok == 0 {
        return Err(last_err());
    }
    Ok(VolumeInfo { label: from_wide_nul(&label), fs_name: from_wide_nul(&fs), serial, max_component: max_comp, flags })
}

/// `C:\\Windows` of the running system (not redirected per session): resolve it like a destination
/// to learn which disk carries the system volume (`is_system`).
pub fn system_windows_dir() -> io::Result<PathBuf> {
    let mut buf = [0u16; 300];
    let n = unsafe { GetSystemWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return Err(last_err());
    }
    Ok(PathBuf::from(OsString::from_wide(&buf[..n])))
}
```

## 6. Error codes of a failed read

What the class driver reports [S: `class.c` `ClassReadWrite`, `ClassInterpretSenseInfo`] and what
`GetLastError` / `io::Error::raw_os_error()` then shows [S: Wine conformance test `error.c`, whose
expectations are checked against Windows; X: the same table printed through
`RtlNtStatusToDosError`]:

| Situation in the driver | NTSTATUS | Win32 |
|-------------------------|----------|-------|
| SCSI medium error ("bad block"), not retried | `STATUS_DEVICE_DATA_ERROR` | 23 `ERROR_CRC` |
| hardware error, aborted command, bus reset, parity, unexpected bus free | `STATUS_IO_DEVICE_ERROR` | 1117 `ERROR_IO_DEVICE` |
| hardware error (not retryable) | `STATUS_DEVICE_HARDWARE_ERROR` | 483 |
| request timed out or was aborted | `STATUS_IO_TIMEOUT` | 121 `ERROR_SEM_TIMEOUT` |
| illegal block / LBA out of range | `STATUS_NONEXISTENT_SECTOR` | 27 `ERROR_SECTOR_NOT_FOUND` |
| not ready, no media | `STATUS_DEVICE_NOT_READY`, `STATUS_NO_MEDIA_IN_DEVICE` | 21 `ERROR_NOT_READY` |
| device object already removed | `STATUS_DEVICE_DOES_NOT_EXIST` | 55 `ERROR_DEV_NOT_EXIST` |
| no device / invalid path, target or LUN ("DL IRPs complete with STATUS_NO_SUCH_DEVICE" after a surprise removal) | `STATUS_NO_SUCH_DEVICE` | 433 `ERROR_NO_SUCH_DEVICE`, **2 `ERROR_FILE_NOT_FOUND` on older Windows** |
| selection timeout | `STATUS_DEVICE_NOT_CONNECTED` | 1167 `ERROR_DEVICE_NOT_CONNECTED`, **21 on older Windows** |
| media changed / verify required | `STATUS_VERIFY_REQUIRED` | 1110 `ERROR_MEDIA_CHANGED` |
| offset or length not a sector multiple | `STATUS_INVALID_PARAMETER` | 87 |
| cancelled | `STATUS_CANCELLED` | 995 `ERROR_OPERATION_ABORTED` |
| (other) | `STATUS_DEVICE_REMOVED` 1617, `STATUS_BUS_RESET` 1111, `STATUS_INSUFFICIENT_RESOURCES` 1450, `STATUS_WORKING_SET_QUOTA` 1453, `STATUS_END_OF_FILE` 38 | |

The table shows why `read_resilient` must not treat "any error" as a bad sector: after a USB
disconnect *every* request fails immediately, and an alignment bug fails with 87. Classification
used by the reference code (the split into classes is a recommendation [I]; the codes are [S]):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFailure {
    /// The sector(s) cannot be read: zero-fill, record a bad range, continue.
    /// Only after the device has been shown to be alive (see below).
    Media,
    /// The device is gone or changed: stop the job, record NOTHING as scanned or bad.
    DeviceLost,
    /// The request itself was wrong (alignment, range, handle): a bug, never a bad sector.
    Caller,
    /// Too large for the driver stack right now: retry with a smaller transfer.
    Resource,
    /// CancelSynchronousIo / CancelIoEx.
    Cancelled,
    /// Not in any list: treat like `Media` but only if the device proves to be alive.
    Unknown,
}

pub fn classify_read_error(code: Option<i32>) -> ReadFailure {
    use ReadFailure::*;
    match code.unwrap_or(-1) {
        // STATUS_DEVICE_DATA_ERROR / STATUS_CRC_ERROR (classpnp: SCSI medium error, "bad block")
        23 => Media,   // ERROR_CRC
        // STATUS_IO_DEVICE_ERROR, STATUS_DATA_OVERRUN (hardware error, aborted command, bus reset)
        1117 => Media, // ERROR_IO_DEVICE
        // STATUS_IO_TIMEOUT (drive busy with its own error recovery)
        121 => Media,  // ERROR_SEM_TIMEOUT
        483 => Media,  // ERROR_DEVICE_HARDWARE_ERROR
        1127 | 1126 | 1128 => Media, // ERROR_DISK_OPERATION_FAILED / RECALIBRATE / RESET
        25 | 30 => Media, // ERROR_SEEK, ERROR_READ_FAULT
        // STATUS_NONEXISTENT_SECTOR (SCSI "illegal block" / ATA IDNF). The caller never asks for
        // sectors at or beyond size(), so inside the device this is an unreadable sector.
        27 => Media,   // ERROR_SECTOR_NOT_FOUND
        1111 => Media, // ERROR_BUS_RESET (transient; retry once before counting it)

        // STATUS_DEVICE_NOT_READY, _NO_MEDIA_IN_DEVICE, _DEVICE_POWER_FAILURE, _DEVICE_OFF_LINE,
        // _VOLUME_DISMOUNTED; STATUS_DEVICE_NOT_CONNECTED on older Windows
        21 => DeviceLost,   // ERROR_NOT_READY
        1167 => DeviceLost, // ERROR_DEVICE_NOT_CONNECTED (STATUS_DEVICE_NOT_CONNECTED)
        433 => DeviceLost,  // ERROR_NO_SUCH_DEVICE (STATUS_NO_SUCH_DEVICE, current Windows)
        2 => DeviceLost,    // ERROR_FILE_NOT_FOUND (STATUS_NO_SUCH_DEVICE, older Windows)
        55 => DeviceLost,   // ERROR_DEV_NOT_EXIST (STATUS_DEVICE_DOES_NOT_EXIST: removed device)
        1617 => DeviceLost, // ERROR_DEVICE_REMOVED
        1110 => DeviceLost, // ERROR_MEDIA_CHANGED (STATUS_VERIFY_REQUIRED)
        1112 => DeviceLost, // ERROR_NO_MEDIA_IN_DRIVE
        321 => DeviceLost,  // ERROR_DEVICE_UNREACHABLE
        31 => DeviceLost,   // ERROR_GEN_FAILURE (STATUS_UNSUCCESSFUL from a dying bridge)
        20 => DeviceLost,   // ERROR_BAD_UNIT
        6 => DeviceLost,    // ERROR_INVALID_HANDLE
        5 => DeviceLost,    // ERROR_ACCESS_DENIED

        87 => Caller,   // ERROR_INVALID_PARAMETER: offset/length not a multiple of the sector size
        1 | 50 => Caller, // ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED
        38 => Caller,   // ERROR_HANDLE_EOF: read at or beyond the end
        998 | 1784 => Caller, // ERROR_NOACCESS, ERROR_INVALID_USER_BUFFER

        1450 | 1453 | 8 | 1816 => Resource, // NO_SYSTEM_RESOURCES, WORKING_SET_QUOTA, NOT_ENOUGH_MEMORY, NOT_ENOUGH_QUOTA
        995 => Cancelled, // ERROR_OPERATION_ABORTED
        _ => Unknown,
    }
}
```

Because 1117 and 121 also appear while a device is dropping off the bus, confirm that the device
is alive before recording a bad range: `IOCTL_STORAGE_CHECK_VERIFY2`, `IOCTL_DISK_GET_LENGTH_INFO`
returning the known size, and a re-read of a sector that was read successfully earlier. If that
fails, the job ends with a "source lost" error and the failed range is recorded neither as scanned
nor as bad. After a reconnect the old handle stays dead; reopen and re-verify the identity (the
disk number may have changed [D]).

## 7. From a path to disk extents (destination guard, S2/S3)

### 7.1 Why path strings are not enough

| Input | What happens | Level |
|-------|--------------|-------|
| relative path | `GetVolumePathName` "returns the drive letter of the boot volume" | [D] |
| path through a junction | `GetVolumePathName` "returns the volume to which the junction point refers" | [D] |
| path through a directory *symbolic link* | not mentioned on the `GetVolumePathName` page; `CreateFile` follows it unless `FILE_FLAG_OPEN_REPARSE_POINT` is given | [D] |
| existing file that is a symbolic link, opened with `CREATE_ALWAYS` / `TRUNCATE_EXISTING` | "the file affected is the target" | [D] |
| `C:\COM2`, `C:\dir\NUL.txt` | `GetVolumePathName` returns `\\.\COM2\`; `GetFullPathNameW("C:\a\NUL.txt")` returns `\\.\NUL` (behaviour before Windows 11) | [D] [X] |
| mapped drive letter, `subst` letter | per-logon-session `DosDevices` links; a letter made by the unelevated user does not exist in the elevated process | [D: KB 3035277 for mapped drives] [I: subst] |
| network share | "SMB does not support volume management functions"; no volume GUID path exists | [D] |

`std::fs::create_dir_all` + `File::create` — the obvious way to write "the original tree below the
destination" — walks through every link: `create_dir_all` accepts an existing name if
`Path::is_dir()` says so, and `is_dir()` follows links [S]; `File::create` truncates through a
file link. [X: a file written "below the destination" through a directory link landed outside it;
an overwrite through a file link replaced the link's target; both with plain std calls]

### 7.2 Resolving the destination root

1. Accept only absolute drive paths (`X:\…`) and UNC paths (`\\server\share\…`). Reject relative,
   drive-relative (`C:foo`), root-relative (`\foo`) and device paths (`\\.\…`). Normalise with
   `GetFullPathNameW` and check the prefix again (a reserved device name has become `\\.\NUL`).
2. `open_existing_ancestor`: the deepest existing ancestor, opened with access 0 and
   `FILE_FLAG_BACKUP_SEMANTICS`. The open follows every junction, symbolic link, mounted folder and
   `subst`, so the handle is on the real directory. It must be a directory. Nothing is created.
3. `final_path_guid`: "A final path is the path that is returned when a path is fully resolved.
   For example, for a symbolic link named `C:\tmp\mydir` that points to `D:\yourdir`, the final
   path would be `D:\yourdir`." [D] With `VOLUME_NAME_GUID` the result starts with
   `\\?\Volume{GUID}\`. Failure with `ERROR_PATH_NOT_FOUND` = network share or a drive created
   without the mount manager [D] → 7.4.
4. `volume_device_of_final_path` (no trailing backslash) → `open_device_for_query` →
   `volume_disk_extents` (all of them).
5. Bus type of every disk in the extents (`device_identity` on a query handle of
   `\\.\PhysicalDrive{DiskNumber}`) → 7.4 for virtual disks.
6. `guard::classify(extents, source_disk_number, protected_ranges)`.
7. `volume_info_by_handle` (file system name, maximum component length, flags) and `free_space`.
8. When the job starts and has created the root directory, run 2–6 again on it and use its
   canonical final path as the base of every output path.

The handle-based route was run under Wine against a directory link: the final path and the file
identity of the handle were those of the link's target. [X] On Windows, `subst` and directory
symbolic links to other volumes are the cases to confirm. [CI]

### 7.3 Below the root

Never enter or write through something that already exists and is a reparse point:

- Directories are created one level at a time (`create_dir_plain` below). An existing name is
  reused only if it is a directory **without** `FILE_ATTRIBUTE_REPARSE_POINT` (this excludes
  junctions, symbolic links and mounted folders alike); anything else is a name collision.
- Files are created with `OpenOptions::create_new(true)`. std adds `FILE_FLAG_OPEN_REPARSE_POINT`
  for `create_new` [S], so an existing link — even a dangling one — fails with
  `ERROR_FILE_EXISTS` (80) instead of being followed. [X]
- Not on output paths: `create_dir_all`, `File::create`, `fs::write`, `OpenOptions::truncate`,
  and `fs::rename` (it passes `MOVEFILE_REPLACE_EXISTING` [S] and silently replaced an existing
  file [X]).

### 7.4 Special destinations

| Destination | What the queries say | Verdict |
|-------------|---------------------|---------|
| partition on another disk | one extent, other disk number | `ok` |
| other partition of the source disk | extent on the source disk | `same_disk`, or `overlaps_source` if it intersects a protected range (7.5) |
| mounted folder, junction, directory symbolic link, `subst` | final path names the real volume | as for that volume |
| dynamic-disk volume (spanned, striped, mirrored) | several extents, first call fails with `ERROR_MORE_DATA` | every extent is checked |
| Storage Spaces | extents name the *virtual* disk (bus type 16); the pool's physical disks are not reported [I] | `unknown_disk` |
| attached VHD / VHDX | extents name the virtual disk (bus type 15 or 14); the backing file can lie on the source | resolve with `GetStorageDependencyInformation`: `GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES` returns "information for volumes or disks hosting the volume specified" with `HostVolumeName` (`\\?\Volume{GUID}\`) and `DependentVolumeRelativePath` [D]; check the host volume too, recursively; if the query fails: `unknown_disk` |
| RAM disk, third-party container | "Some third-party drivers can create a drive letter or mount point without using the Mount Manager … VOLUME_NAME_DOS or VOLUME_NAME_GUID will not succeed" [D]; extent query may fail | `unknown_disk` |
| network share | no volume GUID path [D] | `ok` — unless the server is this machine (`localhost`, a loopback address, one of `GetComputerNameExW`'s names): the administrative shares `\\host\E$` lead straight back to local volumes [I]; then `unknown_disk` |
| removable medium exchanged between check and job | — | step 8 of 7.2 at job start |

`unknown_disk` should need its own consent, separate from `allow_same_disk`: "same disk, outside
the protected ranges" is harmless, "could not find out" is not.

### 7.5 What S2 has to protect

The scanned range is not the region being rescued. After a re-partitioning the old volume can
extend into other partitions of the same disk, and recovery reads `volume_offset + lcn ×
cluster_size` wherever that is. Experiment [L]: an old volume [1 MiB, 256 MiB) with 50 files of
4 MiB, re-partitioned into [1, 128 MiB) and [128, 256 MiB), both halves quick-formatted with
`mkntfs -Q`: all 50 old file records survived inside the first partition (`mkntfs` rewrites 27 MFT
records, Windows 256), and 29 of them describe data that lies entirely inside the second
partition (27 still intact). A scan of the first
partition therefore lists files whose content sits in what the design calls "another partition
(`same_disk`, consent)". The protected ranges handed to `classify` should be: the scan range, the
extent `[volume_offset, volume_offset + total_clusters × cluster_size)` of every geometry
candidate, and the byte ranges of all runs of catalog files (or simply their hull).

### 7.6 Code

```rust
/// GetFullPathNameW: absolute, "."/".." folded, "/" -> "\". Does not touch the disk.
pub fn full_path(p: &Path) -> io::Result<PathBuf> {
    let w = wide(p);
    let mut buf = vec![0u16; 512];
    loop {
        let n = unsafe { GetFullPathNameW(w.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), null_mut()) } as usize;
        if n == 0 {
            return Err(last_err());
        }
        if n < buf.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buf[..n])));
        }
        buf.resize(n + 1, 0);
    }
}

/// Deepest existing ancestor of an absolute path, opened WITHOUT read or write access. The open
/// follows every junction, symlink, mounted folder and SUBST on the way, so the handle refers to
/// the real target directory.
pub fn open_existing_ancestor(abs: &Path) -> io::Result<(File, PathBuf)> {
    let mut cur = abs.to_path_buf();
    loop {
        match OpenOptions::new().access_mode(0).share_mode(SHARE_ALL).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(&cur) {
            Ok(f) => return Ok((f, cur)),
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => {
                    if !cur.pop() {
                        return Err(e);
                    }
                }
                _ => return Err(e),
            },
        }
    }
}

/// GetFinalPathNameByHandleW(VOLUME_NAME_GUID): `\\?\Volume{GUID}\dir\...` of the opened object.
/// Fails with ERROR_PATH_NOT_FOUND on network shares and on drives created without the mount
/// manager (some RAM disks) -> verdict "unknown".
pub fn final_path_guid(f: &File) -> io::Result<OsString> {
    let mut buf = vec![0u16; 600];
    loop {
        let n = unsafe { GetFinalPathNameByHandleW(f.as_raw_handle(), buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED | VOLUME_NAME_GUID) } as usize;
        if n == 0 {
            return Err(last_err());
        }
        if n < buf.len() {
            return Ok(OsString::from_wide(&buf[..n]));
        }
        buf.resize(n + 1, 0);
    }
}

/// `\\?\Volume{GUID}\...` -> `\\?\Volume{GUID}` (no trailing backslash: that is the volume device,
/// with the backslash it would be the root directory).
pub fn volume_device_of_final_path(final_path: &OsStr) -> Option<OsString> {
    let w: Vec<u16> = final_path.encode_wide().collect();
    let prefix: Vec<u16> = r"\\?\Volume{".encode_utf16().collect();
    if !w.starts_with(&prefix) {
        return None;
    }
    let close = w.iter().position(|&c| c == b'}' as u16)?;
    Some(OsString::from_wide(&w[..=close]))
}

/// GetDiskFreeSpaceExW: (free bytes available to the caller — quota aware —, total bytes, total free).
/// `dir` must be an existing directory; a UNC name needs a trailing backslash.
pub fn free_space(dir: &Path) -> io::Result<(u64, u64, u64)> {
    let w = wide(dir);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } == 0 {
        return Err(last_err());
    }
    Ok((avail, total, free))
}

/// (volume serial number, 64-bit file index): equal for two handles <=> same file on NTFS.
pub fn file_identity(f: &File) -> io::Result<(u32, u64)> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
        return Err(last_err());
    }
    Ok((info.dwVolumeSerialNumber, ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64))
}

/// Attributes of the directory entry itself (does not follow a link). None = does not exist.
pub fn attributes_nofollow(p: &Path) -> Option<u32> {
    let w = wide(p);
    let a = unsafe { GetFileAttributesW(w.as_ptr()) };
    (a != INVALID_FILE_ATTRIBUTES).then_some(a)
}

/// Backing file of an attached VHD/VHDX/ISO, from a handle to a volume or disk on it
/// (virtdisk.dll). Returns (host volume, path relative to it) per layer; empty if not virtual.
pub fn storage_dependencies(vol_or_disk: &File) -> io::Result<Vec<(OsString, OsString)>> {
    let mut bytes = 4096usize;
    loop {
        let mut out = OutBuf::new(bytes);
        let info = out.0.as_mut_ptr().cast::<STORAGE_DEPENDENCY_INFO>();
        unsafe { (*info).Version = STORAGE_DEPENDENCY_INFO_VERSION_2 };
        let mut used = 0u32;
        let rc = unsafe { GetStorageDependencyInformation(vol_or_disk.as_raw_handle(), GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES, bytes as u32, info, &mut used) };
        if rc == ERROR_INSUFFICIENT_BUFFER && (used as usize) > bytes {
            bytes = used as usize;
            continue;
        }
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let n = unsafe { (*info).NumberEntries } as usize;
        let first = unsafe { std::ptr::addr_of!((*info).Anonymous).cast::<STORAGE_DEPENDENCY_INFO_TYPE_2>() };
        let mut v = Vec::new();
        for i in 0..n {
            let e = unsafe { std::ptr::read_unaligned(first.add(i)) };
            let s = |p: *const u16| {
                if p.is_null() {
                    return OsString::new();
                }
                let mut len = 0;
                while unsafe { *p.add(len) } != 0 {
                    len += 1;
                }
                OsString::from_wide(unsafe { std::slice::from_raw_parts(p, len) })
            };
            v.push((s(e.HostVolumeName), s(e.DependentVolumeRelativePath)));
        }
        return Ok(v);
    }
}

/// Names under which this machine is reachable: a UNC path whose server is one of these (or
/// "localhost", 127.0.0.0/8, ::1) is NOT a network destination.
pub fn local_computer_names() -> Vec<OsString> {
    let mut v = Vec::new();
    for fmt in [ComputerNameNetBIOS, ComputerNameDnsHostname] {
        let fmt: COMPUTER_NAME_FORMAT = fmt;
        let mut buf = [0u16; 256];
        let mut n = buf.len() as u32;
        if unsafe { GetComputerNameExW(fmt, buf.as_mut_ptr(), &mut n) } != 0 {
            v.push(OsString::from_wide(&buf[..n as usize]));
        }
    }
    v
}
```

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub disk: u32,
    pub start: u64,
    pub len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    /// S2: hard refusal, no override.
    OverlapsSource,
    /// S3: needs `allow_same_disk`.
    SameDisk,
    /// Location could not be established: needs its own consent, never silently `Ok`.
    Unknown,
}

/// `dest`: every extent of the destination volume AND of every volume that hosts it (VHD chain).
/// `protected`: byte ranges [start, end) on the source disk that recovery may still read:
/// the scan range, the extent of every old volume the geometry implies, every run of a catalog file.
pub fn classify(dest: &[Extent], source_disk: u32, protected: &[(u64, u64)]) -> Verdict {
    if dest.is_empty() {
        return Verdict::Unknown; // an empty answer is not "no overlap"
    }
    let mut same = false;
    for e in dest.iter().filter(|e| e.disk == source_disk) {
        same = true;
        let (a, b) = (e.start, e.start.saturating_add(e.len));
        if protected.iter().any(|&(s, t)| a < t && s < b) {
            return Verdict::OverlapsSource;
        }
    }
    if same {
        Verdict::SameDisk
    } else {
        Verdict::Ok
    }
}
```

## 8. Writing recovered files

Order of operations per file (everything after "create" on one handle):

1. Parent directories with `create_dir_plain`, level by level.
2. Create a temporary name in the final directory with `create_new` (for example
   `~hds-<n>.part`; short, so it never hits the component limit).
3. Write; `set_len` if needed; then `set_times` (section 9) as the last call on the handle; close.
4. `rename_noreplace(temp, final)`. `ERROR_ALREADY_EXISTS` (183) is decided by the destination
   file system itself, with its own rules for case, 8.3 aliases and trailing dots. [X: a name that
   differs only in case is refused with 183; `create_new` reports 80] If the existing file was
   produced by this run, take the next ` (n)` suffix whatever `on_exists` says; otherwise apply
   `on_exists` (`rename` → next suffix, `skip` → delete the temporary file, `overwrite` → only if
   the existing entry is a regular file without reparse point: delete it, rename again).
5. Leftover `~hds-*.part` files of a killed run are deleted when the next job starts in that
   directory tree; a file under its final name is therefore always complete as far as the engine
   is concerned.

A virus scanner can hold a file for a moment after it was closed; `rename_noreplace` (and the
replacing rename of the session file) should be retried a few times on errors 5 and 32. [I]

```rust
/// One directory level, never through a link. Ok(true) = created now, Ok(false) = a plain
/// directory was already there. Err(AlreadyExists) = the name is taken by a file, junction,
/// symlink or mounted folder: the caller picks another name (" (2)") instead of entering it.
pub fn create_dir_plain(path: &Path) -> io::Result<bool> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match attributes_nofollow(path) {
            Some(a) if a & FILE_ATTRIBUTE_DIRECTORY != 0 && a & FILE_ATTRIBUTE_REPARSE_POINT == 0 => Ok(false),
            _ => Err(e),
        },
        Err(e) => Err(e),
    }
}

/// Rename that FAILS when the new name exists (ERROR_ALREADY_EXISTS = 183). `std::fs::rename`
/// passes MOVEFILE_REPLACE_EXISTING and silently replaces the target, so it must not be used to
/// move a finished `*.part` file to its final name. Same volume only (no MOVEFILE_COPY_ALLOWED).
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (wide(from), wide(to));
    if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), 0) } == 0 {
        return Err(last_err());
    }
    Ok(())
}
```

Other facts:

- `GetDiskFreeSpaceExW`: the first output is "free bytes … available to the user who is
  associated with the calling thread" (quota-aware) — the number to use; the directory must exist;
  "If this parameter is a UNC name, it must include a trailing backslash". [D] Needed space is
  more than the sum of file sizes (cluster rounding, about 1 KiB of MFT per file on NTFS).
- A full destination shows as 112 `ERROR_DISK_FULL` (or 39): stop the job instead of failing every
  remaining file.
- Limits of the destination file system [D: File System Functionality Comparison]: maximum file
  size 4 GiB on FAT32, 2^64−1 elsewhere; maximum component 255 UTF-16 units on NTFS, exFAT and
  FAT32, 127 (or 254 ASCII) on UDF; path 32,760. `volume_info_by_handle` gives the file system
  name (`"NTFS"`, `"exFAT"`, `"FAT32"`) and the actual component limit, so over-size files can be
  reported before 4 GiB have been written (the write fails with 223 `ERROR_FILE_TOO_LARGE` [I]).
- "Typically the WriteFile and WriteFileEx functions write data to an internal buffer that the
  operating system writes to a disk … on a regular basis"; `FlushFileBuffers` (`File::sync_all`)
  forces it. A file has a size and a separate "valid data length: the length of the data in a
  file that is actually written … always less than or equal to the file size". [D] After a crash
  or power failure the length of an output file therefore says nothing about how much of it is
  good [I]: the image of section 6.10 should record its resume position only after a flush, in a
  side file together with the source identity and range, instead of resuming "from the existing
  image length".

## 9. Timestamps

- `SetFileTime` needs a handle with `FILE_WRITE_ATTRIBUTES`. [D] (Wine does not enforce this. [X])
- Special values [D]: a `FILETIME` of 0 means "do not change this time"; 0xFFFFFFFF/0xFFFFFFFF means
  "prevent file operations using the given handle from modifying" that time. std's
  `File::set_times` rejects both with `InvalidInput` [S] (0: also [X]), and its `SystemTime` cannot represent
  values of 2^63 and above: `SystemTime + Duration` **panics** on such input ("overflow when adding
  duration to `SystemTime`") while `checked_add` returns `None`. [X] Timestamps come from damaged
  records, so convert with checked arithmetic and skip what does not convert.
- "The only guarantee about a file time stamp is that the file time is correctly reflected when
  the handle that makes the change is closed", and "the last write time is not fully updated until
  all handles that are used for writing are closed". [D] Set the times on the writing handle, after
  the last write, immediately before closing it. Verify signatures from the bytes in memory, not
  by reopening the output.
- Directories: open with `FILE_FLAG_BACKUP_SEMANTICS` (a plain `File::open` of a directory fails
  with 5 [X]) and `FILE_WRITE_ATTRIBUTES`; do it after the last change inside the directory,
  including the renames of section 8 and, for the root, the report.
- Other file systems store less: "on FAT, create time has a resolution of 10 milliseconds, write
  time has a resolution of 2 seconds, and access time has a resolution of 1 day"; FAT stores local
  time. [D] A failed or rounded timestamp is a note in the report, never a failed file; tests that
  compare times need a tolerance on non-NTFS destinations.
- Under Wine on Linux the creation time is not applied (modified and accessed are). [X]

```rust
/// NTFS FILETIME (100 ns since 1601-01-01 UTC) -> SystemTime, or None for values that must not be
/// passed to SetFileTime: 0 means "leave unchanged", 0xFFFFFFFF_FFFFFFFF means "stop updating
/// this time on this handle", anything >= 2^63 is negative for the kernel.
pub fn filetime_to_systemtime(ft: u64) -> Option<SystemTime> {
    if ft == 0 || ft > i64::MAX as u64 {
        return None;
    }
    let epoch_1601 = SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(11_644_473_600))?;
    epoch_1601.checked_add(Duration::new(ft / 10_000_000, ((ft % 10_000_000) * 100) as u32))
}

fn file_times(created: Option<SystemTime>, modified: Option<SystemTime>, accessed: Option<SystemTime>) -> FileTimes {
    let mut t = FileTimes::new();
    if let Some(c) = created {
        t = t.set_created(c); // std::os::windows::fs::FileTimesExt
    }
    if let Some(m) = modified {
        t = t.set_modified(m);
    }
    if let Some(a) = accessed {
        t = t.set_accessed(a);
    }
    t
}

/// On the handle that wrote the data, after the last write and before it is closed.
pub fn set_times_on_open_file(f: &File, created: Option<SystemTime>, modified: Option<SystemTime>, accessed: Option<SystemTime>) -> io::Result<()> {
    f.set_times(file_times(created, modified, accessed))
}

/// Directories (and files by path): FILE_WRITE_ATTRIBUTES + FILE_FLAG_BACKUP_SEMANTICS.
/// FILE_FLAG_OPEN_REPARSE_POINT so that a link is never followed.
pub fn set_times_by_path(p: &Path, created: Option<SystemTime>, modified: Option<SystemTime>, accessed: Option<SystemTime>) -> io::Result<()> {
    let f = OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(p)?;
    f.set_times(file_times(created, modified, accessed))
}
```

## 10. Long paths

- `\\?\` "tells the Windows APIs to disable all string parsing and to send the string that follows
  it straight to the file system": no `/`, no `.` or `..`, no relative paths, no trimming of
  trailing dots and spaces, no device-name handling. Total length about 32,767 UTF-16 units,
  "each component up to the value returned in the lpMaximumComponentLength parameter of the
  GetVolumeInformation function (this value is commonly 255 characters)"; UNC form
  `\\?\UNC\server\share\…`. [D]
- "The file system treats path and file names as an opaque sequence of WCHARs": no Unicode
  normalisation happens, and none should be applied. [D]
- Because nothing is parsed, the prefix must only be put in front of a path that is already
  absolute and normalised (`GetFullPathNameW`), and every component must already be sanitised:
  with `\\?\` a name such as `COM1.log` or one with a trailing space is created without complaint
  [X], and Explorer and most programs cannot open or delete it afterwards.
- std adds the prefix by itself only when a path is long (248 units or more) or not absolute [S];
  adding it explicitly for every output path makes behaviour independent of the length.
- A component of 255 units was created, one of 256 was refused. [X]

```rust
/// Absolute path -> `\\?\C:\...` / `\\?\UNC\server\share\...`. The input must already be absolute
/// and normalised (GetFullPathNameW), because nothing after `\\?\` is interpreted: no "/", no
/// ".", no "..", no trailing-dot stripping, no device-name magic.
pub fn to_verbatim(abs: &Path) -> PathBuf {
    let mut comps = abs.components();
    match comps.next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(_) => {
                let mut s = OsString::from(r"\\?\");
                s.push(abs.as_os_str());
                PathBuf::from(s)
            }
            Prefix::UNC(server, share) => {
                let mut s = OsString::from(r"\\?\UNC\");
                s.push(server);
                s.push(r"\");
                s.push(share);
                let mut out = PathBuf::from(s);
                for c in comps {
                    if let Component::Normal(n) = c {
                        out.push(n);
                    }
                }
                out
            }
            _ => abs.to_path_buf(), // already verbatim or a device path
        },
        _ => abs.to_path_buf(),
    }
}
```

## 11. File names

Windows rules [D: Naming Files, Paths, and Namespaces, revision of 2024-08-28]:

- Not allowed: `< > : " / \ | ? *`, NUL, and "characters whose integer representations are in the
  range from 1 through 31"; "any other character that the target file system does not allow".
- Reserved names: "CON, PRN, AUX, NUL, COM1 … COM9, COM¹, COM², COM³, LPT1 … LPT9, LPT¹, LPT², and
  LPT³. Also avoid these names followed immediately by an extension; for example, NUL.txt and
  NUL.tar.gz are both equivalent to NUL." The superscripts are U+00B9, U+00B2, U+00B3. Earlier
  revisions also listed COM0 and LPT0 [I]; CPython additionally treats `CONIN$` and `CONOUT$` as
  reserved and compares the part before the **first** dot with trailing spaces removed (`nul .txt`).
  [S: `ntpath._isreservedname`] The reference sanitizer uses the union.
- Windows 11 no longer treats a reserved name with an extension, or one inside a directory path,
  as a device ("Prior to Windows 11, a path that begins with a legacy device name is always
  interpreted as a legacy device … Because this no longer applies with Windows 11 …"). [D: .NET
  file path formats] Output must be usable on Windows 10 as well, so the old rules are the ones to
  apply.
- "Do not end a file or directory name with a space or a period."
- A colon is the stream separator: an unsanitised `a:b` would create a file `a` with an alternate
  data stream. [D: File Streams]
- Case: "Do not assume case sensitivity"; NTFS can hold names that differ only in case.

All of these exist on real NTFS volumes written by other drivers. [L: one directory created
through ntfs-3g held `CON`, `COM1`, `aux.txt`, `nul.tar.gz`, `LPT²`, `trailing dot.`,
`trailing space `, `...`, `a:b`, `q?*<>|"x`, names with 0x01 and TAB, `Readme.txt` next to
`README.TXT`, `Dir` next to `DIR` and `dir.`, and a name containing the lone surrogate 0xD83D]

Rules of the reference sanitizer, in this order:

1. Each of `< > : " / \ | ? *` and each unit below 0x20 becomes `_`.
2. A lone surrogate becomes U+FFFD (it cannot be carried in JSON, CSV or a Rust `String`; the
   displayed name and the created name then agree).
3. Trailing dots and spaces are removed; an empty result becomes `_` (this also covers `.` and `..`).
4. A reserved device name (base before the first dot, trailing spaces ignored, ASCII
   case-insensitive) gets a `_` prefix.
5. The collision suffix ` (n)` goes before the extension; then the stem is shortened so that the
   whole component fits the limit (255, or the volume's own value), never cutting a surrogate pair
   and never losing the extension.

```rust
/// Maximum component length used when the destination volume does not say otherwise
/// (GetVolumeInformationW lpMaximumComponentLength is 255 on NTFS, exFAT, FAT32-LFN, ReFS).
pub const MAX_COMPONENT: usize = 255;

const RESERVED_CHARS: &[u16] = &[
    b'<' as u16, b'>' as u16, b':' as u16, b'"' as u16, b'/' as u16, b'\\' as u16, b'|' as u16,
    b'?' as u16, b'*' as u16,
];

fn is_high(u: u16) -> bool {
    (0xD800..=0xDBFF).contains(&u)
}

fn is_low(u: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&u)
}

/// True for CON, PRN, AUX, NUL, CONIN$, CONOUT$, COM0-9, COM¹²³, LPT0-9, LPT¹²³ — compared
/// case-insensitively against the part of the name before the FIRST dot with trailing spaces
/// removed ("nul", "NUL.txt", "nul .tar.gz", "Com¹.log" are all reserved).
pub fn is_reserved_device_name(name: &[u16]) -> bool {
    let base_end = name.iter().position(|&u| u == b'.' as u16).unwrap_or(name.len());
    let mut base = &name[..base_end];
    while let [rest @ .., last] = base {
        if *last == b' ' as u16 {
            base = rest;
        } else {
            break;
        }
    }
    let up: Vec<u16> = base
        .iter()
        .map(|&u| if (b'a' as u16..=b'z' as u16).contains(&u) { u - 32 } else { u })
        .collect();
    let eq = |s: &str| up.iter().copied().eq(s.encode_utf16());
    if eq("CON") || eq("PRN") || eq("AUX") || eq("NUL") || eq("CONIN$") || eq("CONOUT$") {
        return true;
    }
    if up.len() == 4 {
        let head = &up[..3];
        let is_com = head.iter().copied().eq("COM".encode_utf16());
        let is_lpt = head.iter().copied().eq("LPT".encode_utf16());
        let d = up[3];
        // ASCII digits and the Latin-1 superscripts ¹ (U+00B9), ² (U+00B2), ³ (U+00B3).
        let digit = (b'0' as u16..=b'9' as u16).contains(&d) || d == 0xB9 || d == 0xB2 || d == 0xB3;
        return (is_com || is_lpt) && digit;
    }
    false
}

fn trim_trailing_dots_spaces(v: &mut Vec<u16>) {
    while matches!(v.last(), Some(&u) if u == b'.' as u16 || u == b' ' as u16) {
        v.pop();
    }
}

/// Cuts `v` to at most `max` units without leaving half a surrogate pair behind.
fn truncate_units(v: &mut Vec<u16>, max: usize) {
    if v.len() > max {
        v.truncate(max);
        if matches!(v.last(), Some(&u) if is_high(u)) {
            v.pop();
        }
    }
}

/// Splits into (stem, extension-with-dot). The extension is kept only when it is short, so that
/// truncation and the " (2)" suffix never destroy the file type.
fn split_ext(v: &[u16]) -> (&[u16], &[u16]) {
    match v.iter().rposition(|&u| u == b'.' as u16) {
        Some(p) if p > 0 && v.len() - p <= 16 => (&v[..p], &v[p..]),
        _ => (v, &[]),
    }
}

/// One path component of the rescued tree -> a name that Win32 programs can open.
/// `suffix` is 0 for the first attempt, n >= 2 for " (n)".
pub fn sanitize_component(raw: &[u16], suffix: u32, max: usize) -> Vec<u16> {
    // 1. reserved characters, control characters, lone surrogates
    let mut v: Vec<u16> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let u = raw[i];
        if u < 0x20 || RESERVED_CHARS.contains(&u) {
            v.push(b'_' as u16);
        } else if is_high(u) && i + 1 < raw.len() && is_low(raw[i + 1]) {
            v.push(u);
            v.push(raw[i + 1]);
            i += 1;
        } else if is_high(u) || is_low(u) {
            v.push(0xFFFD); // lone surrogate: not representable in JSON / CSV / Rust String
        } else {
            v.push(u);
        }
        i += 1;
    }
    // 2. trailing dots and spaces (Win32 strips them silently; "\\?\" would create an
    //    undeletable-from-Explorer name)
    trim_trailing_dots_spaces(&mut v);
    if v.is_empty() {
        v.push(b'_' as u16);
    }
    // 3. reserved device names
    if is_reserved_device_name(&v) {
        v.insert(0, b'_' as u16);
    }
    // 4. collision suffix + length limit, applied last so the result always fits
    let tail: Vec<u16> = if suffix >= 2 { format!(" ({suffix})").encode_utf16().collect() } else { Vec::new() };
    let (stem, ext) = split_ext(&v);
    let (mut stem, ext) = (stem.to_vec(), ext.to_vec());
    let budget = max.saturating_sub(tail.len() + ext.len()).max(1);
    truncate_units(&mut stem, budget);
    if tail.is_empty() && ext.is_empty() {
        trim_trailing_dots_spaces(&mut stem);
        if stem.is_empty() {
            stem.push(b'_' as u16);
        }
    }
    let mut out = stem;
    out.extend_from_slice(&tail);
    out.extend_from_slice(&ext);
    out
}
```

Output of the reference sanitizer [X] (input → result, and with collision suffix 2):

```text
"report.txt"                 -> "report.txt"   suffix2 -> "report (2).txt"
"a<b>c:d\"e/f\\g|h?i*j"      -> "a_b_c_d_e_f_g_h_i_j"   suffix2 -> "a_b_c_d_e_f_g_h_i_j (2)"
"tab\there"                  -> "tab_here"   suffix2 -> "tab_here (2)"
"trailing dot."              -> "trailing dot"   suffix2 -> "trailing dot (2)"
"trailing space "            -> "trailing space"   suffix2 -> "trailing space (2)"
"..."                        -> "_"   suffix2 -> "_ (2)"
" "                          -> "_"   suffix2 -> "_ (2)"
"."                          -> "_"   suffix2 -> "_ (2)"
".."                         -> "_"   suffix2 -> "_ (2)"
".gitignore"                 -> ".gitignore"   suffix2 -> ".gitignore (2)"
"CON"                        -> "_CON"   suffix2 -> "_CON (2)"
"con"                        -> "_con"   suffix2 -> "_con (2)"
"Con.txt"                    -> "_Con.txt"   suffix2 -> "_Con (2).txt"
"nul.tar.gz"                 -> "_nul.tar.gz"   suffix2 -> "_nul.tar (2).gz"
"NUL .txt"                   -> "_NUL .txt"   suffix2 -> "_NUL  (2).txt"
"aux"                        -> "_aux"   suffix2 -> "_aux (2)"
"prn."                       -> "_prn"   suffix2 -> "_prn (2)"
"COM1"                       -> "_COM1"   suffix2 -> "_COM1 (2)"
"com9.log"                   -> "_com9.log"   suffix2 -> "_com9 (2).log"
"COM0"                       -> "_COM0"   suffix2 -> "_COM0 (2)"
"LPT0"                       -> "_LPT0"   suffix2 -> "_LPT0 (2)"
"COM10"                      -> "COM10"   suffix2 -> "COM10 (2)"
"COM¹"                       -> "_COM¹"   suffix2 -> "_COM¹ (2)"
"lpt².txt"                   -> "_lpt².txt"   suffix2 -> "_lpt² (2).txt"
"COM⁴"                       -> "COM⁴"   suffix2 -> "COM⁴ (2)"
"CONIN$"                     -> "_CONIN$"   suffix2 -> "_CONIN$ (2)"
"conout$.txt"                -> "_conout$.txt"   suffix2 -> "_conout$ (2).txt"
"CONSOLE"                    -> "CONSOLE"   suffix2 -> "CONSOLE (2)"
"CLOCK$"                     -> "CLOCK$"   suffix2 -> "CLOCK$ (2)"
"name:stream"                -> "name_stream"   suffix2 -> "name_stream (2)"
"café"                       -> "café"   suffix2 -> "café (2)"
"ok name (2).txt"            -> "ok name (2).txt"   suffix2 -> "ok name (2) (2).txt"
300 x + .jpeg -> 255 units, ends "xxx.jpeg"
300 x + .jpeg, suffix 12 -> 255 units, ends "xx (12).jpeg"
lone high surrogate -> "a�b"
pair straddling unit 255 -> 254 units, last 0x0079
```

## 12. Elevation

- `\\.\PhysicalDriveN` with `GENERIC_READ` needs an administrator ("The caller must have
  administrative privileges" [D]); image files and saved sessions do not.
- `TokenElevation` → `TOKEN_ELEVATION.TokenIsElevated` ("a nonzero value if the token has elevated
  privileges" [D]) is what `hello.elevated` can report. Treat it as advisory [R: reports of
  misleading values with UAC switched off]; the authoritative answer is `ERROR_ACCESS_DENIED` (5)
  from the open, which should map to the protocol error `access_denied`.
- An elevated process runs in another logon session: drive letters the user mapped to network
  shares (and `subst` letters) "are specific for a logon session and are not shared between logon
  sessions". [D: KB 3035277] The elevated UI will not show them; destinations on a NAS have to be
  entered as UNC paths.
- Everything the elevated UI launches ("open folder", "open report") runs elevated as well. [I]

```rust
/// TokenElevation of the current process token (advapi32). Advisory only: the authoritative test
/// is whether `\\.\PhysicalDriveN` opens with GENERIC_READ (ERROR_ACCESS_DENIED = 5 otherwise).
pub fn is_elevated() -> io::Result<bool> {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(last_err());
        }
        let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elev as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        let err = last_err();
        CloseHandle(token);
        if ok == 0 {
            return Err(err);
        }
        Ok(elev.TokenIsElevated != 0)
    }
}
```

## 13. Windows keeps writing to the formatted volume

What is written, and where, is in quick-format-behaviour.md section 5. For the application:

- Nothing in the engine may stop it: locking and dismounting are control codes that S1 forbids
  (the access mask of the handle is no protection there, see section 2); taking a volume or disk
  offline and setting the read-only attribute need a handle with write access. [S]
- What the user can do, as instructions in the UI [D: Windows commands reference]:
  `mountvol X: /p` ("removes the volume mount point …, dismounts the basic volume, and takes the
  basic volume offline, making it unmountable"; undone by assigning a letter again); Disk
  Management or DiskPart `offline disk` / `attributes disk set readonly` for a whole non-system
  disk. Whether `\\.\PhysicalDriveN` stays readable for an offline disk is [CI].
- What the application can detect and say before a scan: the source volume is mounted with a
  drive letter; a page file is configured on that letter (`paging_files()` below reads the
  `PagingFiles` registry value [I: value format]; the system recreates the file at the next
  boot); the source disk carries the system volume; the application's own
  executable, its settings directory, `%TEMP%` or the default session directory resolve (section 7)
  into the source; the number of in-use records of the *new* MFT grows between two looks.
- The folder picker of the destination dialog has a "New folder" button: used on the source
  volume it writes there before any engine check can run. [I]

```rust
/// Configured page files (REG_MULTI_SZ, e.g. `D:\pagefile.sys 0 0`, `?:\pagefile.sys`): a page
/// file configured on the formatted drive letter is recreated at the next boot.
pub fn paging_files() -> io::Result<Vec<OsString>> {
    let key = wide(r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management");
    let val = wide("PagingFiles");
    let mut buf = vec![0u16; 2048];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), val.as_ptr(), RRF_RT_REG_MULTI_SZ, null_mut(), buf.as_mut_ptr().cast::<c_void>(), &mut size) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    buf.truncate(size as usize / 2);
    Ok(buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(OsString::from_wide).collect())
}
```

## 14. To be confirmed by the Windows CI job

The probe behind this sheet could not touch a real disk. One run of these checks on the VHD the CI
job attaches anyway settles the open points:

1. Read at `size` and across `size` on `\\.\PhysicalDriveN`: which error, or a short count?
2. Offset 1 and length 100: error 87? An odd buffer address without `FILE_FLAG_NO_BUFFERING`: works?
3. A query handle (access 0) opens without elevation; `IOCTL_DISK_GET_LENGTH_INFO` on it fails with 5.
4. `IOCTL_STORAGE_GET_DEVICE_NUMBER` on the disk handle: `PartitionNumber` 0.
5. `GetFinalPathNameByHandleW(VOLUME_NAME_GUID)` through a junction, a directory symbolic link to
   another volume, a mounted folder and a `subst` letter names the target volume.
6. `GetStorageDependencyInformation` on a volume of the test VHD returns the backing file; on an
   ordinary volume it fails or returns no entry.
7. Bus type and `TrimEnabled` of the test VHD (a VHD that honours TRIM would make the round trip
   meaningless after a quick format, see quick-format-behaviour.md section 6).
8. `set_times` on a handle opened read-only fails with 5; creation time is applied.
9. `create_new` on `NUL.txt`, `aux`, `name.` with and without `\\?\` on the runner's Windows version.
10. Whether an offline disk can still be read through its `PhysicalDrive` path.

## 15. Sources

Microsoft documentation, read as Markdown source (`raw.githubusercontent.com/MicrosoftDocs/…`):

- `sdk-api/docs/sdk-api-src/content/fileapi/`: `nf-fileapi-createfilew.md`, `nf-fileapi-readfile.md`,
  `nf-fileapi-getvolumepathnamew.md`, `nf-fileapi-getvolumenameforvolumemountpointw.md`,
  `nf-fileapi-getfinalpathnamebyhandlew.md`, `nf-fileapi-getdiskfreespaceexw.md`,
  `nf-fileapi-setfiletime.md`, `nf-fileapi-flushfilebuffers.md`, `nf-fileapi-setfilevaliddata.md`
- `sdk-api/…/winioctl/`: `ni-winioctl-ioctl_disk_get_length_info.md`,
  `ni-winioctl-ioctl_volume_get_volume_disk_extents.md`, `ns-winioctl-volume_disk_extents.md`,
  `ns-winioctl-disk_extent.md`, `ni-winioctl-ioctl_storage_get_device_number.md`,
  `ns-winioctl-storage_device_number.md`, `ns-winioctl-storage_device_descriptor.md`,
  `ns-winioctl-storage_access_alignment_descriptor.md`, `ne-winioctl-storage_bus_type.md`,
  `ni-winioctl-fsctl_allow_extended_dasd_io.md`, `ns-winioctl-set_disk_attributes.md`
- `sdk-api/…/virtdisk/`: `nf-virtdisk-getstoragedependencyinformation.md`,
  `ne-virtdisk-get_storage_dependency_flag.md`, `ns-virtdisk-storage_dependency_info_type_2.md`;
  `…/winnt/ns-winnt-token_elevation.md`; `…/ioapiset/nf-ioapiset-cancelsynchronousio.md`;
  `…/winbase/nf-winbase-movefileexw.md`
- `win32/docs/desktop-src/FileIO/`: `naming-a-file.md`, `maximum-file-path-limitation.md`,
  `file-buffering.md`, `symbolic-link-effects-on-file-systems-functions.md`,
  `filesystem-functionality-comparison.md`, `file-streams.md`; `…/SysInfo/file-times.md`
- `windowsserverdocs/…/administration/windows-commands/`: `mountvol.md`, `offline-disk.md`,
  `attributes-disk.md`
- `SupportArticles-docs/…/windows-client/networking/mapped-drives-not-available-from-elevated-command.md`
  (KB 3035277)
- `dotnet/docs`: `docs/standard/io/file-path-formats.md`

Source code:

- Rust 1.97.0 `library/std/src/sys/fs/windows.rs`, `sys/pal/windows/handle.rs`,
  `sys/path/windows.rs`, `sys/time/windows.rs`, `os/windows/fs.rs`
- `windows-sys` 0.61.2 (`Cargo.toml` features, `src/Windows/Win32/**/mod.rs`)
- `microsoft/Windows-driver-samples`, branch `main`: `storage/class/classpnp/src/class.c`,
  `retry.c`, `classp.h`; `storage/class/disk/src/disk.c`, `disk.h`
- mingw-w64 11.0.1 `winioctl.h`; CPython 3.13 `Lib/ntpath.py`
- `wine-mirror/wine` `dlls/ntdll/tests/error.c`; `tuxera/ntfs-3g` `libntfs-3g/win32_io.c`

Reported only (search excerpts): Raymond Chen, The Old New Thing, 2020-10-20 "How do I get from a
file path to the volume that holds it?" and 2020-10-21 (volume to physical disk); KB 942448
(restricted direct disk access since Windows Vista).
