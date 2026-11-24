//! Recovery plans: what [`recover`](crate::recover) is asked to do.
//!
//! `hds-core` turns a selection of the catalog into a plan (ALGORITHMS H2,
//! H4): the folders to create, with their place in the output tree, and the
//! files with everything needed to write them and to report on them. This
//! crate knows nothing about catalogs; it executes plans.

use hds_io::out::Timestamps;

use crate::health::{FlagSet, RatingContext};
use crate::source::FileSource;

/// A folder to create in the output tree.
///
/// Folders are created even when no file goes into them (ALGORITHMS H4:
/// selected folders are created even when empty). Virtual folders
/// (`[Lost folders]`, instance labels, ...) are folders like any other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFolder {
    /// Identifier, unique among the plan's folders (a catalog node id will
    /// do). Files and other folders refer to it.
    pub id: u64,
    /// The folder this one is created in; `None` for the destination
    /// directory itself.
    pub parent: Option<u64>,
    /// The name as NTFS stored it: UTF-16 units, possibly with characters
    /// Windows refuses, reserved names or unpaired surrogates. Made safe by
    /// the rules of ALGORITHMS H4.
    pub name: Vec<u16>,
    /// Creation, modification and access time (FILETIME, 0 = unknown), set
    /// at the end of the job if this run created the folder.
    pub times: Timestamps,
    /// The folder was in use (not deleted): in-use entries get their names
    /// first when names collide.
    pub in_use: bool,
    /// MFT record number, if the folder has one: decides among entries of
    /// the same in-use state, and makes the fallback name.
    pub record_number: Option<u64>,
}

/// A file to recover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFile {
    /// Catalog node id, handed back in [`FileOutcome`](crate::FileOutcome).
    pub node: u64,
    /// The folder the file goes into ([`PlanFolder::id`]); `None` for the
    /// destination directory itself.
    pub folder: Option<u64>,
    /// The name as NTFS stored it (see [`PlanFolder::name`]).
    pub name: Vec<u16>,
    /// Extension, lower case, without the dot (`""` if none): decides the
    /// signature check of ALGORITHMS H9.
    pub ext: String,
    /// Where the content is.
    pub source: FileSource,
    /// Real size as the catalog shows it (`Row.size`). It should equal
    /// `source.size()`; if it does not, the source decides what is written
    /// and the report notes the difference.
    pub size: u64,
    /// Creation, modification and access time from
    /// `$STANDARD_INFORMATION` (FILETIME; only `0 < t < 2^63` is applied).
    pub times: Timestamps,
    /// The flags the catalog knows before anything is read (ALGORITHMS G1).
    /// `signature_ok` and `signature_mismatch` are replaced by the check of
    /// the bytes written.
    pub flags: FlagSet,
    /// The two rating inputs that are not flags.
    pub context: RatingContext,
    /// The file was in use (not deleted).
    pub in_use: bool,
    /// MFT record number (report column `mft_record`; decides name
    /// collisions and makes the fallback name).
    pub record_number: Option<u64>,
    /// Absolute offset of the base record (report column `record_offset`).
    pub record_offset: Option<u64>,
    /// Absolute offset of the first data byte (report column
    /// `first_data_offset`).
    pub first_data_offset: Option<u64>,
    /// Path in the rescued tree, for display (`/` separators): progress
    /// `current` and report column `original_path`.
    pub original_path: String,
}

/// Everything one recovery job creates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Folders to create, in any order: parents need not come first.
    pub folders: Vec<PlanFolder>,
    /// Files to recover, in any order (they are processed in ascending order
    /// of their first data byte, ALGORITHMS H5).
    pub files: Vec<PlanFile>,
}
