//! Carving (ALGORITHMS I): files found by their signature where no MFT
//! record describes them any more.
//!
//! The scanner keeps a [`CarveHit`] for every sector at which
//! [`hds_carve::sniff`] saw the start of a file of a known kind (A3.4).
//! The carving job turns those hits into files:
//!
//! 1. **Candidates (I2).** [`Carver::new`] keeps the hits of the requested
//!    kinds that lie on the cluster grid of the `main` instance's geometry
//!    (I1), inside the scanned (done) part of the session range, outside
//!    every extent of an in-use `main` file that is neither `overwritten`
//!    nor `out_of_bounds`, and outside the overwritten set. Their number is
//!    the `candidates` of the `carve_start` answer.
//! 2. **Measuring (I3).** [`Carver::run`] measures them in ascending offset
//!    order with [`hds_carve::try_measure`], each bounded by the next extent
//!    of a protected file, the next live-allocated range and the end of the
//!    session range; a candidate inside a file already carved by this run
//!    is skipped (I2 c). A ZIP candidate that runs into the read limit is
//!    measured once more with the limit raised to its bound. Every read goes
//!    through [`hds_recover::CarveReader`]: resilient, inside the session
//!    range, cancellable.
//! 3. **Results (I3, I5).** The job returns a [`CarveRun`]: the kinds it
//!    replaces and the [`CarvedFile`]s it found (all of them, or those found
//!    before a cancel or a lost device). [`CarvedSet::apply`] replaces the
//!    carved files of those kinds; [`store_run`] appends the run to the
//!    session's `CARVED` block, and [`CarvedSet::from_session`] folds the
//!    block back into the same set when a session is loaded.
//!
//! A carved file is a node `[Carved files]/<ext>/f<sector, 10 digits>.<ext>`
//! of the `main` instance; it is recovered like any file with one run
//! ([`CarvedFile::plan_file`]). Its rating is `good` only when the format's
//! structure verified its end ([`hds_carve::Measured::end_verified`]); I4
//! (a carved file inside a deleted file makes that file `overwritten`) uses
//! [`CarvedSet::covered`].
//!
//! # What the caller passes
//!
//! The catalog is built elsewhere; [`CarveInput`] is everything carving
//! needs from it, spelled out field by field. In short: the requested
//! kinds, the `main` geometry, the scan's carve hits and done set, the
//! absolute byte ranges of the protected files (I2 a), the overwritten set
//! (C5) and the live-allocated ranges (C2).
//!
//! # Lifetime (I5)
//!
//! `carve_start` replaces the carved files of the kinds it is given: the
//! engine calls [`CarvedSet::apply`] with the run (also a cancelled one) and
//! [`store_run`]. `geometry_set` on the main instance, `instance_set` and
//! `scan_start` discard every carved file: [`discard_carved`] and a new,
//! empty [`CarvedSet`].
//!
//! # Determinism
//!
//! The same input on the same data gives the same files in the same order,
//! and the same `CARVED` bytes: one thread, ascending offsets, no clock in
//! the results.

#![deny(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_used
)]

mod file;
mod job;
mod set;

#[cfg(test)]
mod tests;

pub use file::{carved_files_folder, ext_folder, CarvedFile, ExtName, CARVED_FILES_FOLDER};
pub use job::{
    CarveFailure, CarveInput, CarveObserver, CarveProgress, CarveStatus, CarveSummary, Carver,
    NoCarveObserver, PROGRESS_INTERVAL,
};
pub use set::{
    discard_carved, store_run, CarveRun, CarvedDecodeError, CarvedSet, MAX_FILES_PER_PAYLOAD,
};

#[cfg(doc)]
use crate::store::CarveHit;

use hds_carve::FileKind;

use crate::store::carve_kind_code;

/// A set of [`FileKind`]s, as `carve_start` names them (`kinds`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct KindSet(u32);

impl KindSet {
    /// No kind.
    pub const EMPTY: KindSet = KindSet(0);

    /// Every kind [`hds_carve`] can measure.
    #[must_use]
    pub fn all() -> KindSet {
        FileKind::all().iter().copied().collect()
    }

    /// The set as a bit mask: bit `carve_kind_code(kind)` per kind (bits 1
    /// to 17), the form stored in the session.
    #[must_use]
    pub fn bits(self) -> u32 {
        self.0
    }

    /// The set of a bit mask; `None` when a bit names no kind.
    #[must_use]
    pub fn from_bits(bits: u32) -> Option<KindSet> {
        let all = KindSet::all().0;
        (bits & !all == 0).then_some(KindSet(bits))
    }

    /// Whether `kind` is in the set.
    #[must_use]
    pub fn contains(self, kind: FileKind) -> bool {
        self.0 & bit(kind) != 0
    }

    /// Adds `kind`.
    pub fn insert(&mut self, kind: FileKind) {
        self.0 |= bit(kind);
    }

    /// Whether the set holds no kind.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The kinds, in [`FileKind::all`] order.
    pub fn iter(self) -> impl Iterator<Item = FileKind> {
        FileKind::all()
            .iter()
            .copied()
            .filter(move |&k| self.contains(k))
    }
}

impl FromIterator<FileKind> for KindSet {
    fn from_iter<I: IntoIterator<Item = FileKind>>(iter: I) -> Self {
        let mut set = KindSet::EMPTY;
        for kind in iter {
            set.insert(kind);
        }
        set
    }
}

fn bit(kind: FileKind) -> u32 {
    // codes are 1..=17 (store::hits), so the shift never overflows
    1_u32
        .checked_shl(u32::from(carve_kind_code(kind)))
        .unwrap_or(0)
}

/// The geometry carving works with: the `main` instance's (I1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CarveGeometry {
    cluster_size: u64,
    volume_offset: u64,
}

impl CarveGeometry {
    /// A geometry; `None` unless `cluster_size` is a power of two from 512
    /// to 2 MiB and `volume_offset` a multiple of 512 (as PROTOCOL's
    /// `geometry_set` requires).
    #[must_use]
    pub fn new(cluster_size: u64, volume_offset: u64) -> Option<CarveGeometry> {
        let ok = cluster_size.is_power_of_two()
            && (512..=2 << 20).contains(&cluster_size)
            && volume_offset.is_multiple_of(512);
        ok.then_some(CarveGeometry {
            cluster_size,
            volume_offset,
        })
    }

    /// Cluster size in bytes.
    #[must_use]
    pub fn cluster_size(self) -> u64 {
        self.cluster_size
    }

    /// Absolute byte offset of cluster 0.
    #[must_use]
    pub fn volume_offset(self) -> u64 {
        self.volume_offset
    }

    /// Whether `offset - volume_offset` is a multiple of the cluster size
    /// (I2). The difference is taken as a signed number: the grid continues
    /// in front of cluster 0, where a whole-disk scan can hold data of an
    /// earlier partition layout.
    #[must_use]
    pub fn on_grid(self, offset: u64) -> bool {
        let distance = offset.abs_diff(self.volume_offset);
        distance & (self.cluster_size.wrapping_sub(1)) == 0
    }

    /// The cluster number of `offset` on this volume; `None` in front of
    /// cluster 0.
    #[must_use]
    pub fn lcn(self, offset: u64) -> Option<u64> {
        offset
            .checked_sub(self.volume_offset)?
            .checked_div(self.cluster_size)
    }
}
