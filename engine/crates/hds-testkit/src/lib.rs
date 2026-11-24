//! Synthetic NTFS disk images for tests, with the ground truth of what a
//! correct recovery must find (ARCHITECTURE.md section 7 item 2).
//!
//! Real drivers do not produce torn records, resident attribute lists, the
//! VCN-0 extent in an extension record, three generations of file systems
//! on one partition or a million records on demand. This crate builds such
//! images in a few lines, deterministically from a seed, and describes each
//! one in a [`manifest::Manifest`]: every structure that can be found in it,
//! what physically survived, and what a catalog built by the rules of
//! `docs/ALGORITHMS.md` must show.
//!
//! **Independence.** Nothing here depends on or copies from `hds-ntfs` or
//! any other engine crate: the encoders ([`ondisk`]) are written from
//! `docs/notes/ntfs-on-disk.md` and ntfs-3g's `layout.h`, and the
//! expectations ([`expect`]) from `docs/ALGORITHMS.md`. The tests decode
//! every image with an independent Python reader
//! (`scripts/linux/inspect-ntfs.py`, via `tests/crosscheck.py`) and mount
//! healthy volumes and the volumes left by the accident scenarios with
//! ntfs-3g, `ntfsinfo` and `ntfsls` (`tests/real_tools.rs`).
//!
//! Layers, bottom up:
//!
//! * [`ondisk`] — encoders for boot sectors, `FILE` records, attributes,
//!   runlists, `INDX` blocks and index B-trees, `$UpCase`, `$AttrDef`.
//! * [`volume`] — a model of one NTFS volume as a formatter and a driver
//!   leave it: formatting ([`volume::Volume::format`], flavours Windows
//!   2000 to 11 and mkntfs), folders, files ([`volume::spec::FileSpec`]:
//!   resident, fragmented, sparse, partly initialized, multi-record),
//!   names, hard links, deleting, renaming, record reuse, quirks.
//! * [`disk`] — a whole disk: volumes at offsets, formats over formats,
//!   partition tables ([`partition`]), embedded images, copies and damage;
//!   [`disk::Disk::finish`] produces the image and its manifest.
//! * [`expect`] — the rules of ALGORITHMS applied to a finished manifest
//!   (record classes, generation cuts, catalog instances, geometry, paths).
//! * [`scenarios`] — one ready-made image per situation, each documented
//!   with the ALGORITHMS paragraphs it exists to test;
//!   [`scenarios::catalog`] lists them, [`scenarios::scale`] streams a
//!   million-record volume to a sparse file.
//!
//! Images live in memory as sparse page sets ([`image::Image`]) and can be
//! written to (sparse) files; the large one is streamed through
//! [`image::FileSink`]. The library is portable; only the cross-check and
//! the real-tool tests need Unix tools.
//!
//! ```
//! use hds_testkit::scenarios;
//!
//! let built = scenarios::build("quick_format_same_geometry", 7).unwrap();
//! let m = &built.manifest;
//! let main = m.main_instance().unwrap();
//! assert!(main.anchor && main.geometry.as_ref().unwrap().validated == Some(true));
//! let survivor = m.entries_of("old").find(|e| e.must_recover_exactly()).unwrap();
//! assert!(survivor.expect.path.is_some());
//! // `built.image.read_at(offset, &mut buf)` serves a block source over it
//! ```

pub mod content;
pub mod disk;
pub mod expect;
pub mod facts;
pub mod hash;
pub mod image;
pub mod manifest;
pub mod ondisk;
pub mod partition;
pub mod populate;
pub mod rng;
pub mod scenarios;
pub mod time;
pub mod volume;
