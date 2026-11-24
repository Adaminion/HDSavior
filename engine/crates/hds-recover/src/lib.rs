//! HDSavior extraction engine: content reader, recovery writer with report,
//! imaging.
//!
//! This crate executes what `hds-core` decides. It knows nothing about
//! catalogs: it reads the content of files described by a [`FileSource`],
//! writes the files of a [`Plan`] into a destination directory, and copies a
//! byte range of the source into an image file. The rules are those of
//! `docs/ALGORITHMS.md` (G1 and G3 for flags and ratings, H for recovery, J
//! for imaging); the events and their wire form those of `docs/PROTOCOL.md`.
//!
//! # Parts
//!
//! * **Flags and rating** ([`Flag`], [`FlagSet`], [`Rating`],
//!   [`RatingContext`], [`rate`]): the per-file vocabulary shared with the
//!   catalog, with the protocol's wire names.
//! * **Reading** ([`ContentReader`]): the content of one file at any offset
//!   and length, through the session's [`RangedSource`](hds_io::RangedSource):
//!   resident data verified against the record's hash, runs read in large
//!   aligned pieces with [`read_resilient`](hds_io::read_resilient), zeros
//!   for holes, for the parts of runs outside the session range
//!   (`out_of_bounds`), for unreadable sectors (`bad_sectors`) and beyond the
//!   initialized size; a lost device and a cancel are errors of their own.
//!   [`read_preview`] serves `read_file`; [`CarveReader`] adapts the source
//!   to [`hds_carve::ReadAt`] for carving.
//! * **Recovery** ([`Recovery`], [`recover`]): ALGORITHMS H3 to H12 - the
//!   destination guard, output names (H4), disk order (H5), part files and
//!   `on_exists` (H6), content (H7), times (H8), the signature check (H9), the
//!   CSV report (H10), statuses (H11) and ending early (H12).
//! * **Imaging** ([`ImageJob`], [`image`]): ALGORITHMS J.
//!
//! # Safety invariants kept here
//!
//! * **S2a** - every read goes through a `RangedSource`, and the reader never
//!   asks it for a byte outside the session range: such bytes are zeros and
//!   the file is `out_of_bounds`, which can never be rated `good`.
//! * **S5** - a file is `ok` only when its rating after the read is `good`:
//!   no flag of the catalog or of the read speaks against it, its record (for
//!   resident data) is unchanged, and its first bytes match its extension
//!   (`ok`) or the extension allows no check (`ok_unverified`).
//! * **S6** - every file-system write goes through [`hds_io::out`]: part files
//!   with create-new semantics, renames that never replace, directories that
//!   are never links, no truncating opens.
//! * **S7** - a lost source ends a job with `source_lost`; nothing of the file
//!   being written is left, nothing is recorded as bad, an image is not
//!   extended.
//!
//! # Mapping to the protocol
//!
//! | here | protocol |
//! |---|---|
//! | [`StartError::DestinationRefused`], [`ImageStartError::DestinationRefused`] | `destination_refused` with the check as `data` |
//! | [`StartError::NoSpace`], [`ImageStartError::NoSpace`], [`ImageStartError::TooLarge`] | `no_space` |
//! | [`ImageStartError::Exists`], [`ImageStartError::CannotResume`] | `exists` |
//! | [`ImageStartError::SourceMismatch`] | `source_mismatch`, the checks as `data` |
//! | [`StartError::InvalidPlan`] | `internal` |
//! | [`StartError::Io`], [`ImageStartError::Io`] | `io_error` |
//! | [`PreviewError::TooLong`] | `bad_request` |
//! | [`RecoverProgress`], [`RecoverSummary`], [`ImageProgress`], [`ImageSummary`] | `recover_progress`, `recover_done`, `image_progress`, `image_done` (serde gives the wire form) |
//!
//! # Example
//!
//! ```
//! use std::sync::atomic::AtomicBool;
//! use std::sync::Arc;
//! use hds_io::out::Timestamps;
//! use hds_io::testing::MemSource;
//! use hds_io::{Protected, RangedSource};
//! use hds_recover::*;
//!
//! // A disk with one file's content at byte 8192.
//! let mut disk = vec![0u8; 1 << 20];
//! disk[8192..8197].copy_from_slice(b"hello");
//! let source = RangedSource::new(Arc::new(MemSource::new(disk)), 0..1 << 20).unwrap();
//!
//! let plan = Plan {
//!     folders: vec![PlanFolder {
//!         id: 1,
//!         parent: None,
//!         name: "Documents".encode_utf16().collect(),
//!         times: Timestamps::default(),
//!         in_use: true,
//!         record_number: Some(64),
//!     }],
//!     files: vec![PlanFile {
//!         node: 7,
//!         folder: Some(1),
//!         name: "greeting.txt".encode_utf16().collect(),
//!         ext: "txt".into(),
//!         source: FileSource::Extents {
//!             runs: vec![Run::Data { offset: 8192, len: 4096 }],
//!             real_size: 5,
//!             initialized_size: 5,
//!         },
//!         size: 5,
//!         times: Timestamps::default(),
//!         flags: FlagSet::new(),
//!         context: RatingContext::default(),
//!         in_use: true,
//!         record_number: Some(65),
//!         record_offset: None,
//!         first_data_offset: Some(8192),
//!         original_path: "Documents/greeting.txt".into(),
//!     }],
//! };
//! let dest = std::env::temp_dir().join(format!("hds-recover-doc-{}", std::process::id()));
//! let options = RecoverOptions::new(vec![Protected::session(0..1 << 20)]);
//! let job = Recovery::prepare(source, plan, &dest, options).unwrap();
//! assert_eq!((job.files_total(), job.bytes_total()), (1, 5));
//! let summary = job.run(&mut (), &AtomicBool::new(false));
//! assert_eq!(summary.status, JobStatus::Completed);
//! assert_eq!(summary.ok, 1);
//! assert_eq!(std::fs::read(dest.join("Documents").join("greeting.txt")).unwrap(), b"hello");
//! # std::fs::remove_dir_all(&dest).unwrap();
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::get_unwrap,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::string_slice,
    clippy::todo,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_used
)]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::single_range_in_vec_init,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

mod health;
mod image;
mod naming;
mod plan;
mod progress;
mod reader;
mod recover;
mod report;
mod source;
#[cfg(test)]
mod testutil;

pub use health::{
    rate, rating_reasons, Flag, FlagSet, Rating, RatingContext, DAMAGED_FLAGS, UNCERTAIN_FLAGS,
    UNSUPPORTED_FLAGS,
};
pub use image::{
    image, ImageJob, ImageObserver, ImageOptions, ImageProgress, ImageStartError, ImageSummary,
};
pub use plan::{Plan, PlanFile, PlanFolder};
pub use reader::{
    read_preview, signature_flag, CarveFailure, CarveReader, ContentReader, Preview, PreviewError,
    ReadError, ReadReport, HEAD_LEN, MAX_LISTED_RANGES, MAX_PREVIEW_LEN, READ_CHUNK,
};
pub use recover::{
    recover, FailReason, FileOutcome, FileStatus, JobError, JobStatus, RecoverObserver,
    RecoverOptions, RecoverProgress, RecoverSummary, Recovery, SkipReason, StartError,
};
pub use source::{FileSource, Run};

/// The types of `hds_io::out` that plans and options are made of.
pub use hds_io::out::{OnExists, Timestamps};
