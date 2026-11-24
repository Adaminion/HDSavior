//! HDSavior scanner, session, catalog, geometry, health, recovery, imaging and carving.
//!
//! The rules are in `docs/ALGORITHMS.md`; the safety invariants and crate
//! boundaries in `docs/ARCHITECTURE.md`. Each module names the sections it
//! implements.
//!
//! * [`store`] - the scan store: everything a scan finds, in memory, with
//!   the read API the later steps (current volumes, instances, catalog,
//!   geometry, health) build on.
//! * [`scan`] - the scanner (ALGORITHMS A).
//! * [`session`] - the session file and the identity check (ALGORITHMS B).
//!
//! ```
//! use std::sync::atomic::AtomicBool;
//! use std::sync::Arc;
//!
//! use hds_core::scan::{scan, NoObserver, ScanOptions, ScanStatus};
//! use hds_core::session::{Session, SessionMeta, SourceIdentity};
//! use hds_core::store::DataExtent;
//! use hds_io::testing::MemSource;
//! use hds_io::{BlockSource, RangedSource};
//!
//! let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(vec![0; 16 << 20]));
//! let range = 0..source.size();
//! let options = ScanOptions::default();
//! let meta = SessionMeta::new(SourceIdentity::from(&source.info()), range.clone(), options)?;
//! // In memory only; `Session::create(meta, path, &guard)` keeps a session file.
//! let mut session = Session::new(meta);
//! let ranged = RangedSource::new(source, range)?;
//! let cancel = AtomicBool::new(false);
//! let summary = scan(&ranged, &options, &mut session, &mut NoObserver, &cancel);
//! assert_eq!(summary.status, ScanStatus::Completed);
//!
//! let store = session.store();
//! for &id in store.records_by_offset() {
//!     let record = store.record(id).expect("ids come from the store");
//!     for name in store.names(id) {
//!         println!("{} in {}", name.name.to_string_lossy(), name.parent);
//!     }
//!     for extent in store.data_extents(id) {
//!         if let DataExtent::NonResident(extent) = extent {
//!             let clusters: u64 = extent.runs().map(|run| run.length).sum();
//!             println!("{:#x}: {clusters} clusters", record.disk_offset());
//!         }
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod scan;
pub mod session;
pub mod store;

#[cfg(test)]
mod test_build;
