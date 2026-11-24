//! Pure, panic-free parsers for NTFS on-disk structures (no I/O).
//!
//! HDSavior reads disks whose file system was destroyed by a quick format.
//! What it finds there is not a volume but loose pieces of one or several:
//! MFT records, directory index blocks and boot sectors at arbitrary
//! 512-byte offsets, some of them stale, torn or half overwritten, next to
//! data that only looks like them. This crate turns the bytes of one such
//! piece into values. Deciding what the pieces mean together is the job of
//! `hds-core`.
//!
//! The field-level reference is `docs/notes/ntfs-on-disk.md`; the rules are
//! in `docs/ARCHITECTURE.md` section 5.1.
//!
//! # Guarantees
//!
//! * **Total.** Every function accepts arbitrary bytes. There is no
//!   `unsafe`, no indexing that can fail, no unchecked arithmetic on values
//!   read from the data and no unbounded loop: each walk is bounded by the
//!   buffer it was given and advances on every step. The crate is compiled
//!   with the Clippy lints `indexing_slicing`, `arithmetic_side_effects`,
//!   `unwrap_used`, `expect_used` and `panic` denied.
//! * **No allocation** in any parser. Names are borrowed UTF-16LE byte
//!   slices ([`Utf16Name`]), resident values are borrowed together with
//!   their offset in the record ([`ResidentValue`]), runlists are decoded by
//!   an iterator ([`Runs`]). Only [`decode_runlist`] writes to a vector, the
//!   caller's, and [`Utf16Name::to_string_lossy`] builds a `String`.
//! * **Best effort, reported.** A structure with a usable header is parsed
//!   as far as it goes and the damage is reported next to the data
//!   ([`FixupStatus`], [`AttrChain`], [`AttrBody::Malformed`],
//!   [`RunlistStatus`], [`WalkEnd`]). Only a structure whose header makes no
//!   sense is refused with an [`Error`].
//! * Numbers come out as stored unless a type says otherwise ([`Run`] and
//!   [`BootSector`] document what they guarantee). They are untrusted:
//!   consumers must use checked arithmetic on them.
//!
//! # The scan loop
//!
//! ```
//! use hds_ntfs::{attr_type, fnv1a64, parse_file_record, probe_file_record, AttrBody};
//! use hds_ntfs::{DEFAULT_MAX_RECORD_SIZE, SECTOR_SIZE};
//!
//! # fn demo(chunk: &[u8]) -> usize {
//! # let mut parsed = 0;
//! let mut scratch = vec![0u8; 65536];
//! for (index, sector) in chunk.chunks_exact(SECTOR_SIZE).enumerate() {
//!     // (a) probe: one comparison for a sector that is not a record
//!     let Some(probe) = probe_file_record(sector, DEFAULT_MAX_RECORD_SIZE) else { continue };
//!     let start = index * SECTOR_SIZE;
//!     let Some(raw) = chunk.get(start..start + probe.record_size as usize) else {
//!         continue; // crosses the chunk end: read it on its own
//!     };
//!     let hash = fnv1a64(raw); // of the bytes as read, before fixups
//!
//!     // (b) fix up and parse in place, on a copy so the chunk stays as read
//!     let record = &mut scratch[..raw.len()];
//!     record.copy_from_slice(raw);
//!     let Ok(record) = parse_file_record(record) else { continue };
//!     if record.is_never_used() {
//!         continue;
//!     }
//!
//!     // (c) one pass over the attributes
//!     for attr in record.attributes() {
//!         match attr.type_code {
//!             attr_type::FILE_NAME => {
//!                 if let Some(name) = attr.file_name() {
//!                     let _ = (attr.id, name.parent, name.namespace, name.name.as_bytes());
//!                 }
//!             }
//!             attr_type::DATA if attr.is_unnamed() => match attr.body {
//!                 AttrBody::Resident(value) => {
//!                     let _ = (value.offset, value.len()); // location inside the record
//!                 }
//!                 AttrBody::NonResident(extent) => {
//!                     let mut runs = extent.runs();
//!                     for run in &mut runs {
//!                         let _ = (run.lcn, run.length);
//!                     }
//!                     let _complete = runs.status();
//!                 }
//!                 AttrBody::Malformed => {} // content location unknown
//!             },
//!             _ => {}
//!         }
//!     }
//!     let _ = (hash, record.fixup(), record.sequence(), record.record_number());
//! #   parsed += 1;
//! }
//! # parsed
//! # }
//! # // One record in use at the second sector: header and end marker.
//! # let mut chunk = vec![0u8; 4096];
//! # chunk[512..516].copy_from_slice(b"FILE");
//! # chunk[512 + 0x04..512 + 0x08].copy_from_slice(&[0x30, 0, 3, 0]); // array at 0x30, 3 entries
//! # chunk[512 + 0x14..512 + 0x18].copy_from_slice(&[0x38, 0, 1, 0]); // attributes at 0x38, in use
//! # chunk[512 + 0x18..512 + 0x20].copy_from_slice(&[0x40, 0, 0, 0, 0, 4, 0, 0]); // 0x40 of 0x400 used
//! # chunk[512 + 0x38..512 + 0x3C].fill(0xFF); // end marker
//! # assert_eq!(demo(&chunk), 1);
//! ```
//!
//! `examples/scan.rs` is this loop as a program: it prints what an image
//! holds and measures the parsers (`--bench`).
//!
//! # Cost
//!
//! On one core of a 2.1 GHz server CPU, for 1 KiB records taken from real
//! volumes: the probe of a sector that is not a record takes a few
//! nanoseconds; fixing a record up and walking its attribute headers about
//! 20 ns; decoding every attribute, name and run about 70 ns. The FNV-1a
//! hash of the record, which the scanner needs as well, takes about 1.1 µs
//! and so decides the speed of the whole step (0.8 to 0.9 million records
//! per second and thread).
//!
//! # Pitfalls this crate takes care of
//!
//! * The fixup stride is 512 bytes for every record and block size and
//!   every sector size ([`SECTOR_SIZE`]).
//! * Values are valid only after fixups: [`parse_file_record`] and
//!   [`parse_index_block`] apply them before anything is decoded.
//! * An attribute's name and runlist are located only through their offset
//!   fields; a non-resident header is 0x40 or 0x48 bytes long.
//! * The name offset of an unnamed attribute is ignored.
//! * The four bytes behind the end marker are not read.
//! * Runlist lengths are signed, offsets are relative to the last
//!   non-sparse run, a present offset of 0 is a real run, padding behind
//!   the terminator is ignored.
//! * The sizes of a non-resident attribute are offered by
//!   [`Extent::sizes`] only for the extent with start VCN 0.
//! * "Compressed" is non-resident plus a compression flag
//!   ([`Attribute::is_compressed`]); "directory" is the record flag or
//!   [`FileName::is_directory`].
//! * Names are UTF-16 code units and may contain unpaired surrogates.
//!
//! One thing it cannot take care of: not every sector that starts with
//! `FILE` and parses is a record of an MFT. Real volumes hold copies of
//! records elsewhere - `$MFTMirr`, shadow copies, and `$LogFile` pages,
//! where new records are logged in their in-memory form (see
//! [`FixupStatus::Torn`]).

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
        clippy::unwrap_used
    )
)]

mod attr;
mod boot;
mod bytes;
mod error;
mod file_name;
mod fixup;
mod hash;
mod index;
mod name;
mod record;
mod reference;
mod runlist;
mod time;

pub use attr::{
    attr_type, AttrBody, AttrListEntries, AttrListEntry, Attribute, AttributeList, Attributes,
    Extent, ResidentValue, StandardInformation, StreamSizes, ATTR_FLAG_COMPRESSION_MASK,
    ATTR_FLAG_ENCRYPTED, ATTR_FLAG_SPARSE, I30, REPARSE_TAG_CLOUD, REPARSE_TAG_CLOUD_MASK,
    REPARSE_TAG_DEDUP, REPARSE_TAG_MOUNT_POINT, REPARSE_TAG_SYMLINK, REPARSE_TAG_WOF,
    WOF_COMPRESSED_DATA,
};
pub use boot::{
    is_boot_sector_candidate, parse_boot_sector, BootSector, BOOT_SECTOR_SIZE, MAX_CLUSTER_SIZE,
    NTFS_OEM_ID,
};
pub use error::Error;
pub use file_name::{
    FileName, Namespace, Times, FILE_ATTR_ARCHIVE, FILE_ATTR_COMPRESSED, FILE_ATTR_DIRECTORY,
    FILE_ATTR_ENCRYPTED, FILE_ATTR_HIDDEN, FILE_ATTR_NOT_INDEXED, FILE_ATTR_OFFLINE,
    FILE_ATTR_READ_ONLY, FILE_ATTR_REPARSE_POINT, FILE_ATTR_SPARSE, FILE_ATTR_SYSTEM,
    FILE_ATTR_TEMPORARY, FILE_ATTR_VIEW_INDEX, FILE_NAME_MIN_LEN,
};
pub use fixup::{apply_fixups, FixupStatus, SECTOR_SIZE};
pub use hash::{fnv1a64, FNV1A64_OFFSET_BASIS, FNV1A64_PRIME};
pub use index::{
    parse_index_block, parse_index_block_fixed, probe_index_block, IndexBlock, IndexBlockProbe,
    IndexEntries, IndexEntry, IndexRoot, NodeHeader, WalkEnd, INDEX_ENTRY_HAS_SUBNODE,
    INDEX_ENTRY_LAST, INDEX_NODE_HAS_SUBNODES, INDX_MAGIC, MAX_INDEX_BLOCK_SIZE,
    MIN_INDEX_BLOCK_SIZE,
};
pub use name::{Utf16Name, Utf16Units};
pub use record::{
    parse_file_record, parse_file_record_fixed, probe_file_record, AttrChain, FileRecord,
    RecordHeader, RecordProbe, DEFAULT_MAX_RECORD_SIZE, FILE_MAGIC, MAX_RECORD_SIZE,
    MIN_RECORD_SIZE, RECORD_IN_USE, RECORD_IS_DIRECTORY,
};
pub use reference::{next_seq, reference_resolves, FileRef};
pub use runlist::{decode_runlist, DecodedRunlist, Run, RunlistError, RunlistStatus, Runs};
pub use time::{
    filetime_is_valid, filetime_to_unix_ms, filetime_to_unix_time, unix_ms_to_filetime,
    FILETIME_TICKS_PER_MS, FILETIME_TICKS_PER_SECOND, FILETIME_UNIX_EPOCH,
};
