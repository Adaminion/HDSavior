//! Read-only block sources, disk enumeration, partition tables, the
//! destination guard and the output primitives of the HDSavior engine.
//!
//! This crate is where the engine touches the operating system, and where
//! most of its safety invariants (ARCHITECTURE.md, section 2) are enforced:
//!
//! * **S1** - devices are opened by one function, for querying or for
//!   reading, never for writing; control codes go through one wrapper with a
//!   closed list (Windows-only module `win::device`). [`BlockSource`] has no
//!   write method.
//! * **S2 / S3** - [`RangedSource`] refuses every read outside the session
//!   range; [`destination_check`] resolves an output location by handle to
//!   the physical disks it lives on and compares them with the source.
//! * **S4** - [`ImageFile`] opens images with share mode *read*; where the
//!   system has no share modes, [`out`] refuses to delete, rename or rewrite
//!   an image that is open as a source.
//! * **S6** - [`out`] is the only code that creates, writes, renames or
//!   deletes files; it never follows links and never replaces.
//! * **S7** - [`read_resilient`] tells unreadable sectors from a device that
//!   went away.
//!
//! Everything except raw disks and disk enumeration also works on Linux,
//! with the same guarantees, because the engine is tested there against
//! image files. Recipes and their evidence: `docs/notes/windows-raw-io.md`.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

mod buf;
mod disks;
mod elevation;
mod guard;
mod image;
pub mod out;
mod partition;
mod rangeset;
mod resilient;
mod sanitize;
mod source;
pub mod testing;
mod winpath;

#[cfg(test)]
mod test_util;

#[cfg(windows)]
mod rawdisk;
#[cfg(windows)]
mod win;

pub use buf::AlignedBuf;
pub use disks::{
    annotate_partitions, list_disks, source_warnings, DiskFlag, DiskInfo, MediaKind, Warning,
    WARN_MOUNTED_VOLUME, WARN_PAGE_FILE_ON_SOURCE, WARN_SYSTEM_DISK, WARN_TRIM_SUPPORTED,
};
pub use elevation::is_elevated;
pub use guard::{
    classify_extents, destination_check, DestinationCheck, Extent, Protected, ProtectedReason,
    Purpose, Verdict,
};
pub use image::ImageFile;
pub use partition::{
    parse_partition_table, sniff_boot_sector, FsKind, NtfsSummary, PartitionInfo, PartitionTable,
    TableKind, TableNote,
};
pub use rangeset::{Gaps, Intersection, Iter as RangeSetIter, RangeSet};
#[cfg(unix)]
pub use resilient::classify_unix_error;
pub use resilient::{
    classify_read_error, classify_windows_error, read_error, read_resilient, read_unaligned,
    windows_loss_needs_proof, ReadFailure, ReadOutcome, MAX_CONSECUTIVE_BAD, RETRY_BLOCK_SIZE,
};
pub use sanitize::{
    fallback_component, fit_component, is_engine_file_name, is_reserved_device_name,
    is_valid_component, sanitize_component, sanitize_name, split_extension, ComponentLimit,
    MIN_COMPONENT_LIMIT,
};
pub use source::{
    check_read_request, is_disk_source_name, open_source, BlockSource, FileId, OutOfRange,
    RangeError, RangedSource, SourceInfo, SourceKind, SourceLocation,
};

#[cfg(windows)]
pub use rawdisk::RawDisk;
#[cfg(windows)]
pub use win::device::{
    open_device, Access, DeviceHandle, DeviceIdentity, DeviceNumber, Geometry, PartitionStyle,
};
