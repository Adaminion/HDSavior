//! Recovery plans: what [`recover`](crate::recover) is asked to do.
//!
//! `hds-core` turns a selection of the catalog into a plan (ALGORITHMS H2,
//! H4): the folders to create, with their place in the output tree, and the
//! files with everything needed to write them and to report on them. This
//! crate knows nothing about catalogs; it executes plans.
//!
//! A plan comes in two forms. [`Plan`] holds every file in full; it suits
//! tests and small selections. [`LazyPlan`] holds the folders in full and
//! reaches the files through [`PlanFiles`], which describes a file in full
//! only when it is written: a recovery of millions of files then keeps a
//! few bytes per file instead of its runlist, name and path (ALGORITHMS
//! H5 needs only the first data byte of each file before anything is read,
//! H4 the names of one output directory at a time).

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

/// What a recovery job needs to know about a file before anything is
/// read: its totals (`recover_start.bytes`, H11's `skipped:damaged`) and
/// its report identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFacts {
    /// Catalog node id ([`PlanFile::node`]).
    pub node: u64,
    /// Real size as the catalog shows it ([`PlanFile::size`]).
    pub size: u64,
    /// The flags known before anything is read ([`PlanFile::flags`]).
    pub flags: FlagSet,
    /// The two rating inputs that are not flags ([`PlanFile::context`]).
    pub context: RatingContext,
}

/// Where a file goes and when it is written (ALGORITHMS H4, H5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilePlace {
    /// The folder the file goes into ([`PlanFile::folder`]).
    pub folder: Option<u64>,
    /// Absolute offset of the first data byte: the first non-empty data
    /// run, else the record of resident data (H5's order key).
    pub first_data_offset: Option<u64>,
}

/// What decides a file's output name (ALGORITHMS H4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName {
    /// The name as NTFS stored it ([`PlanFile::name`]).
    pub name: Vec<u16>,
    /// The file was in use ([`PlanFile::in_use`]).
    pub in_use: bool,
    /// MFT record number ([`PlanFile::record_number`]).
    pub record_number: Option<u64>,
}

/// The files of a [`LazyPlan`], described piece by piece when the job
/// needs them. The answers for one index must agree with each other:
/// [`file`](Self::file) returns the entry whose `node`, `size`, `flags`,
/// `context`, `folder`, `name`, `in_use` and `record_number` are those of
/// [`facts`](Self::facts), [`place`](Self::place) and [`name`](Self::name),
/// and whose source's first data byte is `place`'s.
///
/// An error is a contradiction of the caller (a node that vanished from
/// its catalog): from `place` or `name` it ends the job as failed with
/// `internal`; from `file` it fails that one file (`failed:io_error`).
pub trait PlanFiles: Send {
    /// Number of files.
    fn count(&self) -> usize;
    /// The facts of file `index` (`None` only for an index out of range).
    /// Called for every file when the job is prepared: it should be cheap.
    fn facts(&self, index: usize) -> Option<FileFacts>;
    /// The folder and first data byte of file `index`; called once per
    /// file when the job starts running.
    fn place(&mut self, index: usize) -> Result<FilePlace, String>;
    /// The name of file `index`; called once per file when the names of
    /// its output directory are assigned.
    fn name(&mut self, index: usize) -> Result<FileName, String>;
    /// The whole entry of file `index`; called once, when it is written.
    fn file(&mut self, index: usize) -> Result<PlanFile, String>;
}

fn out_of_range(index: usize) -> String {
    format!("file {index} is not in the plan")
}

impl PlanFiles for Vec<PlanFile> {
    fn count(&self) -> usize {
        self.len()
    }

    fn facts(&self, index: usize) -> Option<FileFacts> {
        self.get(index).map(|f| FileFacts {
            node: f.node,
            size: f.size,
            flags: f.flags,
            context: f.context,
        })
    }

    fn place(&mut self, index: usize) -> Result<FilePlace, String> {
        let f = self.get(index).ok_or_else(|| out_of_range(index))?;
        Ok(FilePlace {
            folder: f.folder,
            first_data_offset: f.source.first_offset().or(f.first_data_offset),
        })
    }

    fn name(&mut self, index: usize) -> Result<FileName, String> {
        let f = self.get(index).ok_or_else(|| out_of_range(index))?;
        Ok(FileName {
            name: f.name.clone(),
            in_use: f.in_use,
            record_number: f.record_number,
        })
    }

    fn file(&mut self, index: usize) -> Result<PlanFile, String> {
        self.get(index).cloned().ok_or_else(|| out_of_range(index))
    }
}

/// A plan whose files are described when they are needed (see the
/// [module documentation](self)).
pub struct LazyPlan {
    /// Folders to create, in any order: parents need not come first.
    pub folders: Vec<PlanFolder>,
    /// The files, in any order (processed by ALGORITHMS H5).
    pub files: Box<dyn PlanFiles>,
}

impl std::fmt::Debug for LazyPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyPlan")
            .field("folders", &self.folders.len())
            .field("files", &self.files.count())
            .finish()
    }
}

impl From<Plan> for LazyPlan {
    fn from(plan: Plan) -> LazyPlan {
        LazyPlan {
            folders: plan.folders,
            files: Box::new(plan.files),
        }
    }
}
