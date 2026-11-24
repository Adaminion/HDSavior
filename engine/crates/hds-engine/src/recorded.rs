//! The session's source while it is not attached (state B), for the
//! destination guard.
//!
//! A session keeps the identity of its source (kind, path, model, serial)
//! after `close_source`, or after a `session_load` that did not attach it.
//! A disk that is still connected can be overwritten all the same: a
//! session file saved onto the lost volume of that disk would destroy what
//! the session is meant to rescue (S2, which has no override for an
//! overlap). So the guard does not run against "no source" (every
//! destination `unknown_disk`, which a consent unlocks) but against where
//! the recorded source is now:
//!
//! * an image: the file at its recorded path, by identity;
//! * a disk: the connected disk that reports the recorded serial number
//!   (and model), by its disk number.
//!
//! Only when that cannot be established is the location unknown, and the
//! guard's message then says why ([`Recorded::explain`]).

use std::io;
use std::path::Path;

use hds_io::{
    list_disks, open_source, BlockSource, DestinationCheck, SourceInfo, SourceKind, SourceLocation,
    Verdict,
};

/// The recorded source of a session, located again. Reads fail; only
/// [`info`](BlockSource::info) and [`location`](BlockSource::location)
/// mean anything.
pub struct Recorded {
    info: SourceInfo,
    location: SourceLocation,
    unresolved: Option<String>,
}

impl Recorded {
    /// Finds where the source with identity `info` is now. Opens nothing
    /// for writing: an image is opened read-only for its identity and
    /// closed again, disks are only listed.
    pub fn locate(info: &SourceInfo) -> Recorded {
        let (location, unresolved) = match info.kind {
            SourceKind::Image => locate_image(info),
            SourceKind::Disk => locate_disk(info),
        };
        Recorded {
            info: info.clone(),
            location,
            unresolved,
        }
    }

    /// Why the location could not be established (`None` when it was).
    pub fn unresolved(&self) -> Option<&str> {
        self.unresolved.as_deref()
    }

    /// Replaces the guard's generic "the source does not report where it
    /// is stored" by the reason the recorded source could not be found.
    pub fn explain(&self, check: &mut DestinationCheck) {
        if check.verdict != Verdict::UnknownDisk {
            return;
        }
        if let Some(why) = &self.unresolved {
            check.message = format!(
                "The session's source ({}) is not attached and HDSavior cannot find out where it is: \
                 {why}. If this location is on that disk, data that is still to be rescued will be \
                 overwritten.",
                self.info.path
            );
        }
    }
}

fn locate_image(info: &SourceInfo) -> (SourceLocation, Option<String>) {
    let sector = Some(info.sector_size);
    match open_source(SourceKind::Image, Path::new(&info.path), sector) {
        Ok(image) => match image.location() {
            SourceLocation::Unknown => (
                SourceLocation::Unknown,
                Some(format!("the image file {} reports no identity", info.path)),
            ),
            location => (location, None),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => (
            SourceLocation::Unknown,
            Some(format!("the image file {} is no longer there", info.path)),
        ),
        Err(e) => (
            SourceLocation::Unknown,
            Some(format!(
                "the image file {} cannot be opened ({e})",
                info.path
            )),
        ),
    }
}

fn locate_disk(info: &SourceInfo) -> (SourceLocation, Option<String>) {
    let Some(serial) = info.serial.as_deref().filter(|s| !s.trim().is_empty()) else {
        return (
            SourceLocation::Unknown,
            Some("the disk reported no serial number when it was scanned".to_string()),
        );
    };
    let matching: Vec<u32> = list_disks()
        .into_iter()
        .filter(|d| d.serial.as_deref().map(str::trim) == Some(serial.trim()))
        .filter(|d| info.model.is_none() || d.model == info.model)
        .map(|d| d.number)
        .collect();
    match matching.as_slice() {
        [number] => (SourceLocation::Disk { number: *number }, None),
        [] => (
            SourceLocation::Unknown,
            Some(format!(
                "no connected disk reports the serial number {serial}"
            )),
        ),
        _ => (
            SourceLocation::Unknown,
            Some(format!(
                "{} connected disks report the serial number {serial}",
                matching.len()
            )),
        ),
    }
}

impl BlockSource for Recorded {
    fn size(&self) -> u64 {
        self.info.size
    }
    fn sector_size(&self) -> u32 {
        self.info.sector_size
    }
    fn physical_sector_size(&self) -> u32 {
        self.info.physical_sector_size
    }
    fn read_at(&self, _: u64, _: &mut [u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the session's source is not attached",
        ))
    }
    fn info(&self) -> SourceInfo {
        self.info.clone()
    }
    fn is_alive(&self) -> bool {
        false
    }
    fn location(&self) -> SourceLocation {
        self.location
    }
}
