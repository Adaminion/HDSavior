//! The destination guard: safety invariants S2 and S3.
//!
//! Before the engine writes anywhere - recovered files, an image, a session
//! file, application data - [`destination_check`] finds out where that
//! location is physically stored and compares it with the source:
//!
//! * inside the session range: `overlaps_source`, refused, no override (S2);
//! * elsewhere on the source disk: `same_disk`, needs `allow_same_disk` (S3);
//! * location not determinable: `unknown_disk`, needs its own consent,
//!   `allow_unknown_disk`;
//! * otherwise `ok`.
//!
//! The location is resolved **by handle**, never from the path string: the
//! deepest existing ancestor of the path is opened without any access
//! rights, which follows every junction, symbolic link, mounted folder and
//! `subst` drive on the way; its final path names the volume; the volume
//! reports all its disk extents; a volume on a virtual disk is traced to the
//! volumes that hold its backing files, recursively. The check creates
//! nothing.

use std::ops::Range;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::source::{BlockSource, FileId, SourceLocation};

#[cfg(unix)]
#[path = "guard/unix.rs"]
mod sys;
#[cfg(windows)]
#[path = "guard/windows.rs"]
mod sys;

#[cfg(unix)]
pub(crate) use sys::fs_name_and_file_limit;

/// What the checked location is going to be used for. Wire form:
/// `"recover"`, `"image"`, `"session"`, `"app_data"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// The directory recovered files are written below.
    Recover,
    /// The path of an image file.
    Image,
    /// The path of a session file. On the Windows system volume this needs
    /// no same-disk consent (S3).
    Session,
    /// A directory the user interface keeps its own data in.
    AppData,
}

impl Purpose {
    /// True when the path names a file rather than a directory.
    fn is_file(self) -> bool {
        matches!(self, Purpose::Image | Purpose::Session)
    }
}

/// Outcome of the guard. Wire form: snake case (`"overlaps_source"`, ...).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The location is not on the source.
    Ok,
    /// The location lies inside a protected range (or is the source image
    /// itself): always refused.
    OverlapsSource,
    /// The location is on the source disk outside the protected ranges: needs
    /// `allow_same_disk`.
    SameDisk,
    /// The physical location could not be established (no extents, Storage
    /// Spaces, a virtual disk that cannot be traced, a network path that
    /// leads back to this machine): needs `allow_unknown_disk`.
    UnknownDisk,
    /// No part of the path exists (unknown drive letter, unreachable share).
    NotFound,
    /// The location exists but cannot be written (read-only volume, a file
    /// where a directory is needed or the reverse, no permission).
    NotWritable,
    /// Not an absolute drive or UNC path.
    InvalidPath,
}

/// Why a byte range of the source must never be written (S2).
/// Wire form: snake case (`"session_range"`, ...).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProtectedReason {
    /// The range the session reads (for imaging: the imaged range).
    SessionRange,
    /// The extent of a lost file system the catalog knows (ALGORITHMS F5).
    LostVolume,
    /// The location is the image file that is the source.
    SourceImage,
}

/// A byte range of the source that must never be written, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protected {
    /// Absolute byte range on the source.
    pub range: Range<u64>,
    /// Why it is protected; reported back when a destination hits it.
    pub reason: ProtectedReason,
}

impl Protected {
    /// The session range (or, before a session exists, the whole source).
    pub fn session(range: Range<u64>) -> Self {
        Protected {
            range,
            reason: ProtectedReason::SessionRange,
        }
    }

    /// The extent of a lost file system.
    pub fn lost_volume(range: Range<u64>) -> Self {
        Protected {
            range,
            reason: ProtectedReason::LostVolume,
        }
    }
}

/// Result of [`destination_check`]: the `check_destination` result of the protocol.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DestinationCheck {
    /// The verdict.
    pub verdict: Verdict,
    /// For [`Verdict::OverlapsSource`]: which protection was hit.
    pub reason: Option<ProtectedReason>,
    /// Plain-language explanation; empty for [`Verdict::Ok`].
    pub message: String,
    /// Bytes available to the caller at the location, if known.
    pub free_bytes: Option<u64>,
    /// Whether `free_bytes` covers `need_bytes`; `None` without `need_bytes`
    /// or when the free space is unknown.
    pub enough_space: Option<bool>,
    /// Name of the file system (`"NTFS"`, `"exFAT"`, `"FAT32"`), if known.
    pub fs: Option<String>,
    /// Largest file the file system can hold, if that is a practical limit
    /// (FAT32: 4294967295).
    pub max_file_bytes: Option<u64>,
    /// Physical disks the location is stored on (including the disks that
    /// hold the backing files of virtual disks); empty when unknown.
    pub disk_numbers: Vec<u32>,
    /// The parts of the source disk the location's volumes occupy, as
    /// absolute byte ranges (empty for image sources and when unknown). Not
    /// part of the wire form: the engine uses it to say which catalog files
    /// lie there.
    #[serde(skip)]
    pub source_extents: Vec<Range<u64>>,
}

impl DestinationCheck {
    /// True when the location may be written with the consents given
    /// (S3: the two consents are separate, and nothing overrides an overlap).
    pub fn is_allowed(&self, allow_same_disk: bool, allow_unknown_disk: bool) -> bool {
        match self.verdict {
            Verdict::Ok => true,
            Verdict::SameDisk => allow_same_disk,
            Verdict::UnknownDisk => allow_unknown_disk,
            Verdict::OverlapsSource
            | Verdict::NotFound
            | Verdict::NotWritable
            | Verdict::InvalidPath => false,
        }
    }
}

/// A piece of a volume on a physical disk: `len` bytes at byte `start` of
/// disk `disk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// `PhysicalDrive` number.
    pub disk: u32,
    /// Byte offset from the start of the disk.
    pub start: u64,
    /// Length in bytes.
    pub len: u64,
}

/// The decision at the core of the guard, without any I/O.
///
/// `dest` holds every extent of the destination volume **and** of every
/// volume that hosts it through virtual disks. `protected` are byte ranges
/// on disk `source_disk` that must never be written.
///
/// Returns [`Verdict::UnknownDisk`] for an empty extent list (an empty
/// answer is not "no overlap"), [`Verdict::OverlapsSource`] when an extent
/// on the source disk intersects a protected range, [`Verdict::SameDisk`]
/// when an extent is on the source disk at all, else [`Verdict::Ok`].
pub fn classify_extents(dest: &[Extent], source_disk: u32, protected: &[Range<u64>]) -> Verdict {
    if dest.is_empty() {
        return Verdict::UnknownDisk;
    }
    let mut same_disk = false;
    for extent in dest.iter().filter(|e| e.disk == source_disk) {
        same_disk = true;
        let end = extent.start.saturating_add(extent.len);
        if protected
            .iter()
            .any(|p| p.start < p.end && extent.start < p.end && p.start < end)
        {
            return Verdict::OverlapsSource;
        }
    }
    if same_disk {
        Verdict::SameDisk
    } else {
        Verdict::Ok
    }
}

/// Where a destination is stored, as far as the platform could find out.
#[cfg_attr(not(windows), allow(dead_code))] // only Windows resolves disks
pub(crate) enum Placement {
    /// Extents on local disks: of the destination volume and of all volumes hosting it.
    Extents(Vec<Extent>),
    /// A share on another machine.
    Network,
    /// Could not be established; the text says why.
    Unknown(String),
}

/// What the platform found out about a destination path.
pub(crate) struct Resolved {
    pub(crate) placement: Placement,
    pub(crate) free_bytes: Option<u64>,
    pub(crate) fs_name: Option<String>,
    pub(crate) max_file_bytes: Option<u64>,
    /// The location is on the volume that holds the Windows directory.
    pub(crate) on_system_volume: bool,
    /// Identity of the path itself when it exists (not of an ancestor).
    pub(crate) leaf_id: Option<FileId>,
    /// The files the location's file system is stored in (Unix: a loop
    /// device's backing file, a file mounted through FUSE). Empty on Windows.
    pub(crate) backing_files: Vec<FileId>,
}

/// Why a path was turned down before its location mattered.
pub(crate) enum Refusal {
    InvalidPath(String),
    NotFound(String),
    NotWritable(String),
}

fn refused(verdict: Verdict, message: String) -> DestinationCheck {
    DestinationCheck {
        verdict,
        message,
        reason: None,
        free_bytes: None,
        enough_space: None,
        fs: None,
        max_file_bytes: None,
        disk_numbers: Vec::new(),
        source_extents: Vec::new(),
    }
}

/// Checks whether `path` may be written to while `source` is being rescued
/// (safety invariants S2 and S3). Creates nothing and changes nothing.
///
/// * `path` - must be an absolute drive path (`X:\...`) or UNC path
///   (`\\server\share\...`), also in their `\\?\` spelling; on Unix an
///   absolute path. It need not exist: the deepest existing ancestor decides.
///   For [`Purpose::Recover`] and [`Purpose::AppData`] it names a directory,
///   for [`Purpose::Image`] and [`Purpose::Session`] a file.
/// * `source` - the source being rescued; its
///   [`location`](BlockSource::location) is what the destination is compared
///   with.
/// * `protected` - the absolute byte ranges of the source that must stay
///   untouched: the session range first (the whole source when no session
///   exists yet), then the lost-volume ranges of the catalog. The first
///   range in list order that the location hits is reported as `reason`.
/// * `need_bytes` - space the caller is going to need, for `enough_space`.
///
/// Verdicts for a disk source are described at [`Verdict`]. For an image
/// source only the image file itself is off limits (`overlaps_source`, also
/// through a hard link); everywhere else is `ok`, because writing other
/// files cannot change the image, and the output primitives refuse to
/// delete, rename or rewrite an image that is open as a source (S4). A
/// session file on the Windows system volume that would be `same_disk` is
/// `ok`.
///
/// On systems other than Windows physical disks cannot be resolved: a disk
/// source always yields `unknown_disk`, an image source works as described.
pub fn destination_check(
    path: &Path,
    purpose: Purpose,
    source: &dyn BlockSource,
    protected: &[Protected],
    need_bytes: Option<u64>,
) -> DestinationCheck {
    let resolved = match sys::resolve(path, purpose) {
        Ok(resolved) => resolved,
        Err(Refusal::InvalidPath(why)) => {
            return refused(
                Verdict::InvalidPath,
                format!(
                    "This is not an absolute path on a drive letter or a network share: {why}."
                ),
            )
        }
        Err(Refusal::NotFound(why)) => {
            return refused(
                Verdict::NotFound,
                format!("This location does not exist or cannot be reached: {why}."),
            )
        }
        Err(Refusal::NotWritable(why)) => {
            return refused(
                Verdict::NotWritable,
                format!("This location cannot be written to: {why}."),
            )
        }
    };
    let location = source.location();
    let (verdict, message, reason) = decide(&resolved, location, protected, purpose);
    let mut disk_numbers: Vec<u32> = match &resolved.placement {
        Placement::Extents(extents) => extents.iter().map(|e| e.disk).collect(),
        _ => Vec::new(),
    };
    disk_numbers.sort_unstable();
    disk_numbers.dedup();
    let source_extents = match (&resolved.placement, location) {
        (Placement::Extents(extents), SourceLocation::Disk { number }) => extents
            .iter()
            .filter(|e| e.disk == number)
            .map(|e| e.start..e.start.saturating_add(e.len))
            .collect(),
        _ => Vec::new(),
    };
    DestinationCheck {
        verdict,
        reason,
        message,
        free_bytes: resolved.free_bytes,
        enough_space: need_bytes.and_then(|need| resolved.free_bytes.map(|free| free >= need)),
        fs: resolved.fs_name,
        max_file_bytes: resolved.max_file_bytes,
        disk_numbers,
        source_extents,
    }
}

/// The first protected range, in list order, that an extent on the source
/// disk intersects. A zero-length extent inside a range counts.
fn first_hit(
    extents: &[Extent],
    source_disk: u32,
    protected: &[Protected],
) -> Option<ProtectedReason> {
    protected
        .iter()
        .find(|p| {
            p.range.start < p.range.end
                && extents.iter().filter(|e| e.disk == source_disk).any(|e| {
                    let end = e.start.saturating_add(e.len);
                    e.start < p.range.end && p.range.start < end
                })
        })
        .map(|p| p.reason)
}

fn decide(
    resolved: &Resolved,
    location: SourceLocation,
    protected: &[Protected],
    purpose: Purpose,
) -> (Verdict, String, Option<ProtectedReason>) {
    const UNKNOWN_TAIL: &str =
        "HDSavior cannot find out where this location is stored. If it is on the disk being rescued, data will be overwritten";
    match location {
        SourceLocation::Detached => (Verdict::Ok, String::new(), None),
        SourceLocation::File { id } => {
            if resolved.leaf_id == Some(id) {
                (
                    Verdict::OverlapsSource,
                    "This is the disk image that is being rescued. Choose another file."
                        .to_string(),
                    Some(ProtectedReason::SourceImage),
                )
            } else if resolved.backing_files.contains(&id) {
                (
                    Verdict::OverlapsSource,
                    "This location is on a file system stored inside the disk image that is being \
                     rescued (the image is attached as a disk). Writing there would change the \
                     image. Choose a location elsewhere."
                        .to_string(),
                    Some(ProtectedReason::SourceImage),
                )
            } else {
                (Verdict::Ok, String::new(), None)
            }
        }
        SourceLocation::Unknown => (
            Verdict::UnknownDisk,
            format!("The source does not report where it is stored. {UNKNOWN_TAIL}."),
            None,
        ),
        SourceLocation::Disk { number } => match &resolved.placement {
            Placement::Network => (Verdict::Ok, String::new(), None),
            Placement::Unknown(why) => (
                Verdict::UnknownDisk,
                format!("{UNKNOWN_TAIL} ({why})."),
                None,
            ),
            Placement::Extents(extents) => {
                let ranges: Vec<Range<u64>> = protected.iter().map(|p| p.range.clone()).collect();
                match classify_extents(extents, number, &ranges) {
                    Verdict::OverlapsSource => match first_hit(extents, number, protected) {
                        Some(ProtectedReason::LostVolume) => (
                            Verdict::OverlapsSource,
                            format!(
                                "This location lies inside the lost volume on disk {number}: the lost file system \
                                 reached into this part of the disk, and files that can still be rescued are stored \
                                 here. Writing there would destroy them. Choose a location on another disk."
                            ),
                            Some(ProtectedReason::LostVolume),
                        ),
                        reason => (
                            Verdict::OverlapsSource,
                            format!(
                                "This location lies inside the area of disk {number} that is being rescued. Writing \
                                 there would overwrite the data that is still to be read. Choose a location on \
                                 another disk."
                            ),
                            reason.or(Some(ProtectedReason::SessionRange)),
                        ),
                    },
                    Verdict::SameDisk if purpose == Purpose::Session && resolved.on_system_volume => {
                        (Verdict::Ok, String::new(), None)
                    }
                    Verdict::SameDisk => (
                        Verdict::SameDisk,
                        format!(
                            "This location is on the disk that is being rescued (disk {number}), outside the area \
                             known to hold lost data. The lost volume may have been larger than what has been \
                             scanned; a location on another disk is safer."
                        ),
                        None,
                    ),
                    Verdict::Ok => (Verdict::Ok, String::new(), None),
                    _ => (
                        Verdict::UnknownDisk,
                        format!("{UNKNOWN_TAIL} (the volume reports no disk extents)."),
                        None,
                    ),
                }
            }
        },
    }
}

#[cfg(test)]
mod tests;
