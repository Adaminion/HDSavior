//! The session: a scan store with its source identity, range and options,
//! optionally backed by a session file (ALGORITHMS B1 to B4).
//!
//! # The file
//!
//! A session file is `"HDSC"`, a format version and append-only blocks
//! (see [`file`](mod@file) for the framing and `codec` for the payloads). Its state
//! is everything up to the last valid `CHECKPOINT`; later bytes are ignored
//! when it is loaded and cut off before the next append. A checkpoint
//! appends the data added since the previous one and is followed by a
//! sync. [`Session::save_as`] writes a compacted copy, after which the
//! session continues in the copy.
//!
//! # Encoding
//!
//! The payloads use a hand-written little-endian layout instead of a serde
//! codec. The bulk of a session is a few large arrays (records, their aux
//! bytes, index entries, hits); written as such they are copied in and out
//! without per-field overhead, the format cannot change under a codec
//! upgrade, and decoding is total by construction: every count is checked
//! against the bytes that are really there before anything is allocated,
//! and every value is validated before it reaches the store. The aux bytes
//! of records are the store's own encoding and are checked item by item.
//!
//! # Safety rules for the file (S2, S3, S6)
//!
//! * Every location is checked with the destination guard before it is
//!   written ([`SessionGuard`]: the caller's protected ranges and consents).
//! * All writes go through `hds_io::out`: new files with create-new
//!   semantics under a temporary name next to the target (never the system
//!   temporary directory), renamed without replacing; an existing session
//!   file is reopened only as a regular file that is not a link, and never
//!   truncated on open.
//! * A file is replaced only when it is a session file of the same source
//!   and range; anything else at the path is refused.

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

mod codec;
mod crc;
pub mod file;
mod identity;
mod wire;

#[cfg(test)]
mod tests;

use std::fmt;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hds_io::out::{self, EntryKind, OutDir, OutFile};
use hds_io::{
    destination_check, BlockSource, DestinationCheck, Protected, Purpose, SourceInfo, SourceKind,
};

use crate::scan::ScanOptions;
use crate::store::{EntryId, RecordId, ScanStore};
use file::{BlockSink, BlockType, LastBlock, Origin, BLOCK_TARGET, MAX_PAYLOAD};

pub use identity::{
    check_identity, IdentityCheck, IdentityError, MIN_SAMPLES, SAMPLE_COUNT, WARN_IDENTITY_DIFFERS,
};

/// What is known about the source of a session (ALGORITHMS B1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceIdentity {
    /// Disk or image.
    pub kind: SourceKind,
    /// The path it was opened with (for finding it again).
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Logical sector size.
    pub sector_size: u32,
    /// Physical sector size.
    pub physical_sector_size: u32,
    /// Model, for disks.
    pub model: Option<String>,
    /// Serial number, for disks.
    pub serial: Option<String>,
}

impl From<&SourceInfo> for SourceIdentity {
    fn from(info: &SourceInfo) -> Self {
        SourceIdentity {
            kind: info.kind,
            path: info.path.clone(),
            size: info.size,
            sector_size: info.sector_size,
            physical_sector_size: info.physical_sector_size,
            model: info.model.clone(),
            serial: info.serial.clone(),
        }
    }
}

/// The fixed facts of a session: source identity, session range, scan
/// options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMeta {
    /// The source. Path, model and serial can be updated when the source
    /// is attached again ([`Session::set_identity`]).
    pub identity: SourceIdentity,
    /// The session range: never changes.
    pub range: Range<u64>,
    /// The scan options: never change.
    pub options: ScanOptions,
}

impl SessionMeta {
    /// Checks and bundles the facts: the range must be non-empty, aligned
    /// to the logical sector size (a power of two from 512 to 4096) and
    /// inside the source; the options must be valid.
    pub fn new(
        identity: SourceIdentity,
        range: Range<u64>,
        options: ScanOptions,
    ) -> Result<SessionMeta, SessionError> {
        let sector = identity.sector_size;
        let sector_ok = sector.is_power_of_two() && (512..=4096).contains(&sector);
        let physical_ok = identity.physical_sector_size >= sector
            && identity.physical_sector_size.is_power_of_two()
            && identity.physical_sector_size <= 65_536;
        let sector = u64::from(sector);
        let range_ok = range.start < range.end
            && range.end <= identity.size
            && range.start.is_multiple_of(sector.max(1))
            && range.end.is_multiple_of(sector.max(1));
        let strings_ok = [
            Some(&identity.path),
            identity.model.as_ref(),
            identity.serial.as_ref(),
        ]
        .iter()
        .flatten()
        .all(|s| s.len() <= wire::MAX_STRING);
        if !sector_ok || !physical_ok {
            return Err(SessionError::InvalidArgument(
                "unsupported sector size".to_string(),
            ));
        }
        if !range_ok {
            return Err(SessionError::InvalidArgument(
                "the session range must be a non-empty, sector-aligned part of the source"
                    .to_string(),
            ));
        }
        if !options.is_valid() || !strings_ok {
            return Err(SessionError::InvalidArgument(
                "invalid scan options".to_string(),
            ));
        }
        Ok(SessionMeta {
            identity,
            range,
            options,
        })
    }

    /// Whether a session file with these facts may be replaced by one with
    /// `other`'s: same range, and the same source as far as can be told
    /// without reading it (kind and size, and the same path or the same
    /// model and serial).
    #[must_use]
    pub fn same_source_and_range(&self, other: &SessionMeta) -> bool {
        let (a, b) = (&self.identity, &other.identity);
        self.range == other.range
            && a.kind == b.kind
            && a.size == b.size
            && (a.path == b.path
                || (a.serial.is_some() && a.serial == b.serial && a.model == b.model))
    }
}

/// Why a session operation failed.
#[derive(Debug)]
pub enum SessionError {
    /// The destination guard refused the location (protocol
    /// `destination_refused`; the check is the error data).
    DestinationRefused(Box<DestinationCheck>),
    /// The path holds something this session must not replace: a session
    /// file of another source or range, a file that is not a session file,
    /// a directory or a link (protocol `exists`).
    Occupied(String),
    /// The file has a newer format version (protocol `unsupported`).
    Unsupported(u32),
    /// The file is not a session file, has no valid checkpoint, or
    /// contradicts itself (protocol `io_error`).
    Corrupt(String),
    /// An argument is not acceptable: a path without file name, an empty
    /// range, invalid options, a payload over 1 GiB (protocol
    /// `bad_request`).
    InvalidArgument(String),
    /// The session has no file to continue (it was never saved).
    NoFile,
    /// An input/output error (protocol `io_error`, or `no_space`).
    Io(io::Error),
}

impl SessionError {
    /// The protocol error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            SessionError::DestinationRefused(_) => "destination_refused",
            SessionError::Occupied(_) => "exists",
            SessionError::Unsupported(_) => "unsupported",
            SessionError::InvalidArgument(_) => "bad_request",
            SessionError::NoFile => "no_session",
            SessionError::Io(e) if out::classify_error(e) == out::OutErrorKind::NoSpace => {
                "no_space"
            }
            SessionError::Corrupt(_) | SessionError::Io(_) => "io_error",
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::DestinationRefused(check) => {
                write!(f, "the session file may not be written there: {}", check.message)
            }
            SessionError::Occupied(why) => f.write_str(why),
            SessionError::Unsupported(v) => write!(
                f,
                "the session file has format version {v}, which this version of HDSavior cannot read"
            ),
            SessionError::Corrupt(why) => write!(f, "the session file cannot be used: {why}"),
            SessionError::InvalidArgument(why) => f.write_str(why),
            SessionError::NoFile => f.write_str("the session has no file to write to"),
            SessionError::Io(e) => write!(f, "session file: {e}"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SessionError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for SessionError {
    fn from(e: io::Error) -> Self {
        SessionError::Io(e)
    }
}

/// The destination guard for session paths (S2, S3): the source, the
/// protected ranges (session range first, then lost-volume ranges) and the
/// two consents, all from the caller.
#[derive(Clone, Copy)]
pub struct SessionGuard<'a> {
    /// The source being rescued.
    pub source: &'a dyn BlockSource,
    /// Byte ranges of the source that must never be written.
    pub protected: &'a [Protected],
    /// Consent to a location on the source disk outside the protected ranges.
    pub allow_same_disk: bool,
    /// Consent to a location whose disk cannot be determined.
    pub allow_unknown_disk: bool,
}

impl fmt::Debug for SessionGuard<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionGuard")
            .field("source", &self.source.info().path)
            .field("protected", &self.protected)
            .field("allow_same_disk", &self.allow_same_disk)
            .field("allow_unknown_disk", &self.allow_unknown_disk)
            .finish()
    }
}

impl SessionGuard<'_> {
    /// Runs `hds_io::destination_check` for a session file at `path` and
    /// refuses when the verdict is not allowed with the consents given.
    pub fn check(&self, path: &Path) -> Result<DestinationCheck, SessionError> {
        let check = destination_check(path, Purpose::Session, self.source, self.protected, None);
        if check.is_allowed(self.allow_same_disk, self.allow_unknown_disk) {
            Ok(check)
        } else {
            Err(SessionError::DestinationRefused(Box::new(check)))
        }
    }
}

/// The blocks that later modules own; to this module they are opaque byte
/// payloads (ALGORITHMS B1, B2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpaqueKind {
    /// The latest current-volume snapshot (ALGORITHMS C).
    Live,
    /// First-sector heads read for geometry checks (F3).
    Heads,
    /// The user's overrides (F7, D8).
    Overrides,
    /// Carved results (I).
    Carved,
}

impl OpaqueKind {
    /// All four kinds.
    pub const ALL: [OpaqueKind; 4] = [
        OpaqueKind::Live,
        OpaqueKind::Heads,
        OpaqueKind::Overrides,
        OpaqueKind::Carved,
    ];

    fn index(self) -> usize {
        match self {
            OpaqueKind::Live => 0,
            OpaqueKind::Heads => 1,
            OpaqueKind::Overrides => 2,
            OpaqueKind::Carved => 3,
        }
    }
}

/// The payloads of one opaque kind and how many of them the file has.
#[derive(Clone, Debug, Default)]
struct OpaqueList {
    items: Vec<Vec<u8>>,
    persisted: usize,
    /// The list was replaced: the file must discard its earlier payloads.
    reset: bool,
}

/// How much of the store is in the file already.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Persisted {
    records: usize,
    entries: usize,
    blocks: usize,
    boots: usize,
    carve: usize,
    bad_log: usize,
}

impl Persisted {
    fn all(store: &ScanStore) -> Persisted {
        Persisted {
            records: store.records.len(),
            entries: store.entries.len(),
            blocks: store.blocks.len(),
            boots: store.boots.len(),
            carve: store.carve.len(),
            bad_log: store.bad_log.len(),
        }
    }
}

/// The open session file.
struct Writer {
    /// The verified directory; holding it keeps it from being renamed or
    /// removed on Windows while the session writes into it.
    _dir: OutDir,
    path: PathBuf,
    file: OutFile,
    valid_end: u64,
}

impl Writer {
    /// The writer of `file`, which is held (`OutFile::hold`): the engine's
    /// own output primitives refuse to delete or replace it while the
    /// session writes to it (a recovery with `overwrite` into the folder
    /// of the session file, S4).
    fn new(dir: OutDir, path: PathBuf, mut file: OutFile, valid_end: u64) -> Writer {
        file.hold();
        Writer {
            _dir: dir,
            path,
            file,
            valid_end,
        }
    }
}

/// What [`Session::load`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadReport {
    /// Valid checkpoints read.
    pub checkpoints: u64,
    /// Length of the file.
    pub file_len: u64,
    /// End of the last valid checkpoint: the state of the file.
    pub valid_end: u64,
    /// Bytes behind it, ignored now and cut off before the next append.
    pub ignored_bytes: u64,
    /// Why they were ignored.
    pub ignored_reason: Option<String>,
}

/// What [`Session::save_as`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveReport {
    /// The canonical path of the session file.
    pub path: PathBuf,
    /// Its size in bytes.
    pub bytes: u64,
}

/// Payloads of more than this many bytes are refused by the opaque API.
pub const MAX_OPAQUE_PAYLOAD: usize = (1 << 30) - 1;

/// Warning raised when a session file of format version 1 is loaded: its
/// records carry no `$FILE_NAME` time and its index entries the MFT-change
/// time of their `$FILE_NAME` copy, so the record and entry times
/// (ALGORITHMS, vocabulary) are approximations. It is kept in every
/// checkpoint written afterwards, also after the file is rewritten in the
/// current format, so the session never passes for one with exact times.
pub const WARN_OLD_SESSION_FORMAT: &str = "old_session_format";
const OLD_SESSION_FORMAT_MESSAGE: &str = "This session was saved by an earlier version of \
HDSavior that did not keep every time a file system records for its files. Files that were \
restored from a backup onto the lost volume may be sorted less well into file systems. Scanning \
the disk again gives the best result.";

/// A session: store, facts and (optionally) its file.
pub struct Session {
    meta: SessionMeta,
    store: ScanStore,
    opaque: [OpaqueList; 4],
    writer: Option<Writer>,
    persisted: Persisted,
    /// The file this session was last loaded from or written to.
    origin: Option<(PathBuf, Origin)>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("meta", &self.meta)
            .field("store", &self.store)
            .field("file", &self.file_path())
            .finish()
    }
}

/// Counter for the names of temporary files.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Creates a temporary file next to the session file.
fn create_temp(dir: &OutDir) -> Result<(String, OutFile), SessionError> {
    for _ in 0..1000 {
        let name = format!(
            "~hds-session-{}.tmp",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        match dir.create_new(&name) {
            Ok(file) => return Ok((name, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(SessionError::Io(e)),
        }
    }
    Err(SessionError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name for a temporary session file",
    )))
}

fn split(path: &Path) -> Result<(OutDir, String), SessionError> {
    let (dir, name) = out::split_file_path(path)
        .map_err(|e| SessionError::InvalidArgument(format!("{}: {e}", path.display())))?;
    let dir = OutDir::open_root(&dir).map_err(SessionError::Io)?;
    Ok((dir, name))
}

/// Number of items per block for fixed-size items.
const ITEMS_PER_BLOCK: usize = 1 << 20;

impl Session {
    /// A new session kept in memory only.
    #[must_use]
    pub fn new(meta: SessionMeta) -> Session {
        Session {
            meta,
            store: ScanStore::new(),
            opaque: Default::default(),
            writer: None,
            persisted: Persisted::default(),
            origin: None,
        }
    }

    /// A new session with a new session file at `path` (see
    /// [`save_as`](Self::save_as) for what may be at that path already).
    pub fn create(
        meta: SessionMeta,
        path: &Path,
        guard: &SessionGuard<'_>,
    ) -> Result<Session, SessionError> {
        let mut session = Session::new(meta);
        session.save_as(path, guard)?;
        Ok(session)
    }

    /// Loads a session file: the state of its last valid checkpoint. The
    /// file is only read; to go on writing to it, call
    /// [`attach_file`](Self::attach_file).
    pub fn load(path: &Path) -> Result<(Session, LoadReport), SessionError> {
        let loaded = file::load(path)?;
        let report = LoadReport {
            checkpoints: loaded.checkpoints,
            file_len: loaded.file_len,
            valid_end: loaded.origin.valid_end,
            ignored_bytes: loaded.file_len.saturating_sub(loaded.origin.valid_end),
            ignored_reason: loaded.stop.map(str::to_string),
        };
        let mut opaque: [OpaqueList; 4] = Default::default();
        for (list, items) in opaque.iter_mut().zip(loaded.opaque) {
            list.persisted = items.len();
            list.items = items;
        }
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let mut store = loaded.store;
        if loaded.origin.version < file::FORMAT_VERSION {
            store.raise_warning(WARN_OLD_SESSION_FORMAT, OLD_SESSION_FORMAT_MESSAGE);
        }
        Ok((
            Session {
                persisted: Persisted::all(&store),
                meta: loaded.meta,
                store,
                opaque,
                writer: None,
                origin: Some((path, loaded.origin)),
            },
            report,
        ))
    }

    /// The facts of the session.
    #[must_use]
    pub fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    /// Updates path, model and serial of the source (after it was attached
    /// at another path); written with the next checkpoint. Kind and size
    /// must not change.
    pub fn set_identity(&mut self, identity: SourceIdentity) -> Result<(), SessionError> {
        if identity.kind != self.meta.identity.kind
            || identity.size != self.meta.identity.size
            || identity.sector_size != self.meta.identity.sector_size
        {
            return Err(SessionError::InvalidArgument(
                "the source's kind, size and sector size cannot change".to_string(),
            ));
        }
        self.meta = SessionMeta::new(identity, self.meta.range.clone(), self.meta.options)?;
        Ok(())
    }

    /// The scan store.
    #[must_use]
    pub fn store(&self) -> &ScanStore {
        &self.store
    }

    pub(crate) fn store_mut(&mut self) -> &mut ScanStore {
        &mut self.store
    }

    /// Whether the whole session range has been scanned.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.store.done().contains_range(self.meta.range.clone())
    }

    /// Whether a file is attached (checkpoints are written).
    #[must_use]
    pub fn has_file(&self) -> bool {
        self.writer.is_some()
    }

    /// The canonical path of the attached file.
    #[must_use]
    pub fn file_path(&self) -> Option<&Path> {
        self.writer.as_ref().map(|w| w.path.as_path())
    }

    /// The file this session was last loaded from or written to, attached
    /// or not: the default `session_path` of a resumed scan.
    #[must_use]
    pub fn last_file(&self) -> Option<&Path> {
        self.origin.as_ref().map(|(path, _)| path.as_path())
    }

    /// Stops writing to the session file (for example when its location
    /// turned out to lie inside a lost volume, ALGORITHMS B3). The session
    /// stays in memory; [`save_as`](Self::save_as) writes it elsewhere.
    pub fn detach_file(&mut self) {
        self.writer = None;
    }

    /// Runs the guard again on the attached file, with the protected ranges
    /// of now (they grow when a catalog finds lost volumes; the guard runs
    /// at every job start). When the location is no longer allowed the file
    /// is detached and the refusal returned: the engine then raises
    /// `session_file_at_risk` (ALGORITHMS B3). `Ok` without a file.
    pub fn recheck_file(&mut self, guard: &SessionGuard<'_>) -> Result<(), SessionError> {
        let Some(path) = self.file_path().map(Path::to_path_buf) else {
            return Ok(());
        };
        match guard.check(&path) {
            Ok(_) => Ok(()),
            Err(refusal) => {
                self.writer = None;
                Err(refusal)
            }
        }
    }

    /// The payloads of one opaque kind, oldest first.
    #[must_use]
    pub fn opaque(&self, kind: OpaqueKind) -> &[Vec<u8>] {
        self.opaque
            .get(kind.index())
            .map_or(&[][..], |list| list.items.as_slice())
    }

    /// Appends a payload of `kind`; written with the next checkpoint.
    pub fn append_opaque(
        &mut self,
        kind: OpaqueKind,
        payload: Vec<u8>,
    ) -> Result<(), SessionError> {
        if payload.len() > MAX_OPAQUE_PAYLOAD {
            return Err(SessionError::InvalidArgument(
                "payload larger than 1 GiB".to_string(),
            ));
        }
        if let Some(list) = self.opaque.get_mut(kind.index()) {
            list.items.push(payload);
        }
        Ok(())
    }

    /// Replaces all payloads of `kind`; the file discards the earlier ones
    /// from the next checkpoint on.
    pub fn replace_opaque(
        &mut self,
        kind: OpaqueKind,
        payloads: Vec<Vec<u8>>,
    ) -> Result<(), SessionError> {
        if payloads.iter().any(|p| p.len() > MAX_OPAQUE_PAYLOAD) {
            return Err(SessionError::InvalidArgument(
                "payload larger than 1 GiB".to_string(),
            ));
        }
        if let Some(list) = self.opaque.get_mut(kind.index()) {
            list.items = payloads;
            list.persisted = 0;
            list.reset = true;
        }
        Ok(())
    }

    fn opaque_lengths(&self) -> [u64; 4] {
        let mut lengths = [0_u64; 4];
        for (slot, list) in lengths.iter_mut().zip(&self.opaque) {
            *slot = u64::try_from(list.items.len()).unwrap_or(u64::MAX);
        }
        lengths
    }

    fn checkpoint_data(&self, sequence: u64) -> codec::Checkpoint {
        codec::Checkpoint {
            sequence,
            meta: self.meta.clone(),
            totals: file::totals_of(&self.store, self.opaque_lengths()),
            done: self.store.done.clone(),
            drops: self.store.drops,
            zero: self.store.zero,
            warnings: self.store.warnings.clone(),
        }
    }

    fn next_sequence(&self) -> u64 {
        self.origin
            .as_ref()
            .map_or(1, |(_, o)| o.sequence.saturating_add(1))
    }

    /// Writes the data blocks for everything from `from` on, the opaque
    /// payloads not yet in the file (all of them when `all_opaque`), and a
    /// checkpoint.
    fn write_blocks<W: io::Write>(
        &self,
        sink: &mut BlockSink<W>,
        from: Persisted,
        all_opaque: bool,
        sequence: u64,
    ) -> io::Result<()> {
        let store = &self.store;
        let to_u32 = |i: usize| u32::try_from(i).unwrap_or(u32::MAX);
        if all_opaque && from == Persisted::default() {
            // A compacted copy: everything in canonical order.
            codec::encode_records(
                store,
                store.records_by_offset().iter().copied(),
                BLOCK_TARGET,
                |payload| sink.emit(BlockType::Records, &payload),
            )?;
            for ids in store.index_entries_by_source().chunks(ITEMS_PER_BLOCK) {
                sink.emit(
                    BlockType::IndexEntries,
                    &codec::encode_entries(store, ids.iter().copied()),
                )?;
            }
            let order = store.index_blocks_by_offset();
            for ids in order.chunks(ITEMS_PER_BLOCK) {
                let blocks = ids
                    .iter()
                    .filter_map(|&i| usize::try_from(i).ok().and_then(|i| store.blocks.get(i)));
                sink.emit(BlockType::IndexBlocks, &codec::encode_blocks(blocks))?;
            }
            let mut boots = store.boots.clone();
            boots.sort_by_key(|b| b.disk_offset);
            for chunk in boots.chunks(ITEMS_PER_BLOCK) {
                sink.emit(BlockType::BootHits, &codec::encode_boots(chunk.iter()))?;
            }
            for chunk in store
                .carve_hits_sorted()
                .chunks(ITEMS_PER_BLOCK.saturating_mul(4))
            {
                sink.emit(BlockType::CarveHits, &codec::encode_carve(chunk))?;
            }
            let bad: Vec<Range<u64>> = store.bad.iter().collect();
            for chunk in bad.chunks(ITEMS_PER_BLOCK) {
                sink.emit(BlockType::BadRanges, &codec::encode_bad(chunk))?;
            }
        } else {
            codec::encode_records(
                store,
                (from.records..store.records.len()).map(|i| RecordId(to_u32(i))),
                BLOCK_TARGET,
                |payload| sink.emit(BlockType::Records, &payload),
            )?;
            let mut start = from.entries;
            while start < store.entries.len() {
                let end = start
                    .saturating_add(ITEMS_PER_BLOCK)
                    .min(store.entries.len());
                sink.emit(
                    BlockType::IndexEntries,
                    &codec::encode_entries(store, (start..end).map(|i| EntryId(to_u32(i)))),
                )?;
                start = end;
            }
            for chunk in store
                .blocks
                .get(from.blocks..)
                .unwrap_or(&[])
                .chunks(ITEMS_PER_BLOCK)
            {
                sink.emit(BlockType::IndexBlocks, &codec::encode_blocks(chunk.iter()))?;
            }
            for chunk in store
                .boots
                .get(from.boots..)
                .unwrap_or(&[])
                .chunks(ITEMS_PER_BLOCK)
            {
                sink.emit(BlockType::BootHits, &codec::encode_boots(chunk.iter()))?;
            }
            let pending: Vec<crate::store::CarveHit> =
                store.carve.iter_from(from.carve).copied().collect();
            for chunk in pending.chunks(ITEMS_PER_BLOCK.saturating_mul(4)) {
                sink.emit(BlockType::CarveHits, &codec::encode_carve(chunk))?;
            }
            for chunk in store
                .bad_log
                .get(from.bad_log..)
                .unwrap_or(&[])
                .chunks(ITEMS_PER_BLOCK)
            {
                sink.emit(BlockType::BadRanges, &codec::encode_bad(chunk))?;
            }
        }
        for kind in OpaqueKind::ALL {
            let Some(list) = self.opaque.get(kind.index()) else {
                continue;
            };
            let ty = BlockType::of_opaque(kind);
            let (reset, pending) = if all_opaque {
                (false, list.items.as_slice())
            } else {
                (list.reset, list.items.get(list.persisted..).unwrap_or(&[]))
            };
            if reset && pending.is_empty() {
                sink.emit(
                    ty,
                    &codec::encode_opaque(codec::OPAQUE_RESET | codec::OPAQUE_EMPTY, &[]),
                )?;
            }
            for (i, payload) in pending.iter().enumerate() {
                let flags = if reset && i == 0 {
                    codec::OPAQUE_RESET
                } else {
                    0
                };
                sink.emit(ty, &codec::encode_opaque(flags, payload))?;
            }
        }
        sink.emit(
            BlockType::Checkpoint,
            &codec::encode_checkpoint(&self.checkpoint_data(sequence)),
        )
    }

    /// Marks everything as written to the file that now ends at `end`.
    fn written(&mut self, path: PathBuf, end: u64, last: Option<LastBlock>, sequence: u64) {
        self.persisted = Persisted::all(&self.store);
        for list in &mut self.opaque {
            list.persisted = list.items.len();
            list.reset = false;
        }
        if let Some((block_offset, block_len, block_crc)) = last {
            self.origin = Some((
                path,
                Origin {
                    valid_end: end,
                    block_offset,
                    block_len,
                    block_crc,
                    sequence,
                    version: file::FORMAT_VERSION,
                },
            ));
        }
    }

    /// Appends what is new since the last checkpoint and a checkpoint, then
    /// syncs (ALGORITHMS B3). Bytes behind the last valid checkpoint (left by
    /// a crash or a failed checkpoint) are cut off first. A no-op without a
    /// file.
    pub fn checkpoint(&mut self) -> Result<(), SessionError> {
        let sequence = self.next_sequence();
        let Some(mut writer) = self.writer.take() else {
            return Ok(());
        };
        let result = self.append(&mut writer, sequence);
        let path = writer.path.clone();
        let end = writer.valid_end;
        self.writer = Some(writer);
        let last = result?;
        self.written(path, end, Some(last), sequence);
        Ok(())
    }

    fn append(&self, writer: &mut Writer, sequence: u64) -> Result<LastBlock, SessionError> {
        use std::io::{Seek, SeekFrom};
        let len = writer.file.len()?;
        if len < writer.valid_end {
            return Err(SessionError::Occupied(
                "the session file is shorter than its last checkpoint: it was changed by another program"
                    .to_string(),
            ));
        }
        if len > writer.valid_end {
            writer.file.set_len(writer.valid_end)?;
        }
        writer.file.seek(SeekFrom::Start(writer.valid_end))?;
        let mut sink = BlockSink::new(&mut writer.file, writer.valid_end);
        self.write_blocks(&mut sink, self.persisted, false, sequence)?;
        let (end, last) = sink.finish()?;
        writer.file.sync()?;
        writer.valid_end = end;
        last.ok_or_else(|| SessionError::Io(io::Error::other("no checkpoint was written")))
    }

    /// Writes a compacted copy of the session to `path` and continues in
    /// it (`session_save`; also how a new session file is created).
    ///
    /// The copy is written to a temporary file next to `path`, synced and
    /// renamed without replacing. If `path` exists it must be a session
    /// file of the same source and range (or this session's own file); it
    /// is then removed right before the rename. Anything else at `path` is
    /// refused ([`SessionError::Occupied`]), as is a location the guard
    /// does not allow.
    pub fn save_as(
        &mut self,
        path: &Path,
        guard: &SessionGuard<'_>,
    ) -> Result<SaveReport, SessionError> {
        guard.check(path)?;
        let (dir, name) = split(path)?;
        let target = dir.path().join(&name);
        let is_own = self.writer.as_ref().is_some_and(|w| w.path == target);
        let existing = dir.entry(&name)?;
        match existing {
            None => {}
            Some(EntryKind::File) if is_own => {}
            Some(EntryKind::File) => {
                let mut file = dir.open_existing(&name).map_err(|e| {
                    SessionError::Occupied(format!("{} cannot be replaced: {e}", target.display()))
                })?;
                let other = file::peek_meta(&mut file).map_err(|_| {
                    SessionError::Occupied(format!(
                        "{} exists and is not an HDSavior session file",
                        target.display()
                    ))
                })?;
                if !other.same_source_and_range(&self.meta) {
                    return Err(SessionError::Occupied(format!(
                        "{} is the session file of another source or range",
                        target.display()
                    )));
                }
            }
            Some(_) => {
                return Err(SessionError::Occupied(format!(
                    "{} exists and is not a regular file",
                    target.display()
                )))
            }
        }

        // A new file: its checkpoints count from the start again.
        let sequence = 1;
        let (temp_name, temp) = create_temp(&dir)?;
        let written = self.write_copy(temp, sequence);
        let (end, last) = match written {
            Ok(done) => done,
            Err(e) => {
                let _ = dir.remove_file(&temp_name);
                return Err(e);
            }
        };
        if existing.is_some() {
            // A file cannot be removed while it is open on Windows: let go of
            // our own first, and take it up again if it cannot be removed.
            let own_end = if is_own {
                self.writer.take().map(|w| w.valid_end)
            } else {
                None
            };
            if let Err(e) = dir.remove_file(&name) {
                let _ = dir.remove_file(&temp_name);
                if let (Some(valid_end), Ok(file)) = (own_end, dir.open_existing(&name)) {
                    self.writer = Some(Writer::new(dir.clone(), target.clone(), file, valid_end));
                }
                return Err(SessionError::Io(e));
            }
        }
        if let Err(e) = dir.rename_noreplace(&temp_name, &name) {
            if existing.is_none() {
                let _ = dir.remove_file(&temp_name);
                return Err(SessionError::Io(e));
            }
            // The old file is gone and the copy is the only one left: keep it
            // under its temporary name and go on writing to it.
            let temp_path = dir.path().join(&temp_name);
            if let Ok(file) = dir.open_existing(&temp_name) {
                self.writer = Some(Writer::new(dir, temp_path.clone(), file, end));
                self.written(temp_path.clone(), end, last, sequence);
            }
            return Err(SessionError::Io(io::Error::new(
                e.kind(),
                format!(
                    "the session was saved as {} but could not be renamed to {}: {e}",
                    temp_path.display(),
                    target.display()
                ),
            )));
        }
        let file = dir.open_existing(&name)?;
        self.writer = Some(Writer::new(dir, target.clone(), file, end));
        self.written(target.clone(), end, last, sequence);
        Ok(SaveReport {
            path: target,
            bytes: end,
        })
    }

    fn write_copy(
        &self,
        mut file: OutFile,
        sequence: u64,
    ) -> Result<(u64, Option<LastBlock>), SessionError> {
        let mut sink = BlockSink::new(&mut file, 0);
        sink.raw(&file::header_bytes())?;
        // The first block fixes identity, range and options before any data.
        let mut first = self.checkpoint_data(sequence);
        first.totals = codec::Totals::default();
        first.done = hds_io::RangeSet::new();
        first.drops = crate::store::DropCounters::default();
        first.zero = crate::store::ZeroCheck::Checking(0);
        first.warnings = Vec::new();
        first.sequence = sequence.saturating_sub(1);
        sink.emit(BlockType::Checkpoint, &codec::encode_checkpoint(&first))?;
        self.write_blocks(&mut sink, Persisted::default(), true, sequence)?;
        let (end, last) = sink.finish()?;
        file.sync()?;
        Ok((end, last))
    }

    /// Continues writing to the session file at `path`: the file this
    /// session was loaded from or last written to, with the checkpoint it
    /// was left at (checked). Used to resume a loaded session. The guard
    /// runs first. A file of an older format version is first rewritten in
    /// the current one, as [`save_as`](Self::save_as) does.
    pub fn attach_file(
        &mut self,
        path: &Path,
        guard: &SessionGuard<'_>,
    ) -> Result<(), SessionError> {
        guard.check(path)?;
        let Some((_, origin)) = self.origin.clone() else {
            return Err(SessionError::NoFile);
        };
        let (dir, name) = split(path)?;
        let target = dir.path().join(&name);
        if self.writer.as_ref().is_some_and(|w| w.path == target) {
            return Ok(());
        }
        let mut file = dir.open_existing(&name).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => SessionError::Io(e),
            _ => SessionError::Occupied(format!(
                "{} cannot be opened for appending: {e}",
                target.display()
            )),
        })?;
        if !file::matches_origin(&mut file, &origin)? {
            return Err(SessionError::Occupied(format!(
                "{} is not the session file this session was loaded from or last saved to",
                target.display()
            )));
        }
        if origin.version < file::FORMAT_VERSION {
            // A file of an older format: blocks of the current format cannot
            // be appended to it. Rewrite it in the current format (a
            // compacted copy that replaces it) and go on in that.
            drop(file);
            self.save_as(path, guard)?;
            return Ok(());
        }
        self.writer = Some(Writer::new(dir, target, file, origin.valid_end));
        Ok(())
    }
}

/// Payload size limit, re-exported for documentation.
pub const MAX_BLOCK_PAYLOAD: u64 = MAX_PAYLOAD;
