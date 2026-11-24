//! The carved files of a session, and their place in the session file
//! (ALGORITHMS I5, B1: the `CARVED` block).
//!
//! # The `CARVED` block
//!
//! The block is a list of opaque payloads ([`OpaqueKind::Carved`]), folded
//! in order. Each payload is one piece of a [`CarveRun`], little endian:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | magic `HCV1` |
//! | 4 | 4 | kinds this payload replaces ([`KindSet::bits`]) |
//! | 8 | 8 | cluster size of the geometry the files were carved with |
//! | 16 | 8 | volume offset of that geometry |
//! | 24 | 8 | number of files `n` |
//! | 32 | 28 × n | files, strictly ascending by offset |
//!
//! A file is `offset: u64, len: u64, kind code: u8, flags: u8 (bit 0 end
//! verified, bit 1 exact), extension length: u8, extension: [u8; 8] (zero
//! padded), reserved: u8 (0)`.
//!
//! Folding: a payload whose geometry differs from the one before it starts
//! from nothing (a geometry change discards carved files); then the files
//! of its kinds are removed and its files added. A run with more than
//! [`MAX_FILES_PER_PAYLOAD`] files is written as several payloads; all but
//! the first replace no kinds. [`CarvedSet::store_compacted`] replaces the
//! list with the folded state.
//!
//! Loading is total: a payload that is cut, has an unknown magic, a kind or
//! extension that does not exist, files out of order or outside the
//! session range, makes [`CarvedSet::from_session`] fail with a
//! [`CarvedDecodeError`], never panic or allocate more than the payload's
//! own size. The engine then discards the block ([`discard_carved`]): carving
//! can be run again, the scan cannot be lost through it.

use std::fmt;
use std::ops::Range;

use hds_carve::FileKind;
use hds_io::RangeSet;

use super::file::{CarvedFile, ExtName};
use super::{CarveGeometry, KindSet};
use crate::session::{OpaqueKind, Session, SessionError};
use crate::store::{carve_kind_code, carve_kind_from_code};

const MAGIC: [u8; 4] = *b"HCV1";
const HEADER_LEN: usize = 32;
const FILE_LEN: usize = 28;

/// Most files in one payload (28 MiB of payload). A larger run is split.
pub const MAX_FILES_PER_PAYLOAD: usize = 1 << 20;

const FLAG_END_VERIFIED: u8 = 1;
const FLAG_EXACT: u8 = 2;

/// What one carving job produced (ALGORITHMS I5): the kinds it was asked
/// for - the carved files of these kinds are replaced - and the files it
/// found, in ascending offset order, with the geometry they were found
/// with. A cancelled job's run holds what was found before the cancel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarveRun {
    /// The kinds the job carved.
    pub kinds: KindSet,
    /// The `main` geometry it used.
    pub geometry: CarveGeometry,
    /// The files, strictly ascending by offset, not overlapping.
    pub files: Vec<CarvedFile>,
}

impl CarveRun {
    /// Total bytes of the files.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.files.iter().fold(0_u64, |a, f| a.saturating_add(f.len()))
    }
}

/// The carved files of a session: every file of the latest run of each
/// kind, sorted by offset (offsets are unique: one sector has one sniffed
/// kind). Files of different runs may overlap (a JPEG carved alone, inside
/// a ZIP carved alone).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CarvedSet {
    files: Vec<CarvedFile>,
    geometry: Option<CarveGeometry>,
}

impl CarvedSet {
    /// No carved files.
    #[must_use]
    pub fn new() -> CarvedSet {
        CarvedSet::default()
    }

    /// The files, ascending by offset.
    #[must_use]
    pub fn files(&self) -> &[CarvedFile] {
        &self.files
    }

    /// Number of files (`CatalogInfo.carved`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether there is no carved file.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Total size of the files.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.files
            .iter()
            .fold(0_u64, |a, f| a.saturating_add(f.len()))
    }

    /// The geometry of the latest run; `None` before the first. The engine
    /// compares it with the `main` geometry of a loaded session and
    /// discards the set when they differ (I5).
    #[must_use]
    pub fn geometry(&self) -> Option<CarveGeometry> {
        self.geometry
    }

    /// The file that starts at `offset`.
    #[must_use]
    pub fn at(&self, offset: u64) -> Option<&CarvedFile> {
        let i = self
            .files
            .binary_search_by_key(&offset, CarvedFile::offset)
            .ok()?;
        self.files.get(i)
    }

    /// The bytes of all carved files, for I4: a deleted file whose extents
    /// intersect this set is `overwritten`.
    #[must_use]
    pub fn covered(&self) -> RangeSet {
        self.files.iter().map(CarvedFile::range).collect()
    }

    /// Takes the result of a carving job (I5): a run made with another
    /// geometry first discards everything; then the files of the run's kinds
    /// are replaced by the run's files. Linear in the size of both.
    pub fn apply(&mut self, run: &CarveRun) {
        self.apply_parts(run.kinds, run.geometry, &run.files);
    }

    fn apply_parts(&mut self, kinds: KindSet, geometry: CarveGeometry, added: &[CarvedFile]) {
        if self.geometry != Some(geometry) {
            self.files.clear();
            self.geometry = Some(geometry);
        }
        let mut sorted;
        let mut added = added;
        if !added.windows(2).all(|w| matches!(w, [a, b] if a.offset() < b.offset())) {
            // not what a job produces; keep the set sorted all the same
            sorted = added.to_vec();
            sorted.sort_by_key(CarvedFile::offset);
            sorted.dedup_by_key(|f| f.offset());
            added = &sorted;
        }
        let old = std::mem::take(&mut self.files);
        let mut kept = old
            .into_iter()
            .filter(|f| !kinds.contains(f.kind()))
            .peekable();
        let mut merged = Vec::with_capacity(kept.size_hint().0.saturating_add(added.len()));
        for &file in added {
            while let Some(older) = kept.next_if(|o| o.offset() < file.offset()) {
                merged.push(older);
            }
            // a file of another kind at the same offset gives way to the newer
            let _ = kept.next_if(|o| o.offset() == file.offset());
            merged.push(file);
        }
        merged.extend(kept);
        self.files = merged;
    }

    /// The carved files stored in `session` (its `CARVED` block), folded as
    /// the module documentation describes. An empty block gives an empty
    /// set.
    pub fn from_session(session: &Session) -> Result<CarvedSet, CarvedDecodeError> {
        Self::from_payloads(session.opaque(OpaqueKind::Carved), session.meta().range.clone())
    }

    /// [`from_session`](Self::from_session) on bare payloads, with the
    /// session range the files must lie in.
    pub fn from_payloads(
        payloads: &[Vec<u8>],
        range: Range<u64>,
    ) -> Result<CarvedSet, CarvedDecodeError> {
        // Decode every header first: which payloads still count for each
        // kind, so that every file is decoded and placed once (no repeated
        // merging over many payloads).
        let mut headers = Vec::with_capacity(payloads.len());
        for (index, payload) in payloads.iter().enumerate() {
            headers.push(Header::parse(payload).map_err(|e| e.at(index))?);
        }
        let mut reset = 0_usize; // first payload after the last geometry change
        let mut replaced = [0_usize; 32]; // per kind bit: the last payload replacing it
        for (index, header) in headers.iter().enumerate() {
            let previous = index.checked_sub(1).and_then(|p| headers.get(p));
            if previous.is_some_and(|p| p.geometry != header.geometry) {
                reset = index;
            }
            for kind in header.kinds.iter() {
                if let Some(slot) = replaced.get_mut(usize::from(carve_kind_code(kind))) {
                    *slot = index;
                }
            }
        }
        let mut files: Vec<(CarvedFile, usize)> = Vec::new();
        for (index, (payload, header)) in payloads.iter().zip(&headers).enumerate() {
            if index < reset {
                continue;
            }
            let mut previous: Option<u64> = None;
            for file in header.files(payload) {
                let file = file.map_err(|e| e.at(index))?;
                if previous.is_some_and(|p| p >= file.offset()) {
                    return Err(CarvedDecodeError::new(index, "files out of order"));
                }
                if file.offset() < range.start || file.end() > range.end {
                    return Err(CarvedDecodeError::new(
                        index,
                        "file outside the session range",
                    ));
                }
                previous = Some(file.offset());
                let latest = replaced
                    .get(usize::from(carve_kind_code(file.kind())))
                    .copied()
                    .unwrap_or(0);
                if index >= latest {
                    files.push((file, index));
                }
            }
        }
        // by offset; at equal offsets the later payload last, then kept
        files.sort_by_key(|&(file, index)| (file.offset(), index));
        let mut out: Vec<CarvedFile> = Vec::with_capacity(files.len());
        for (file, _) in files {
            if out.last().is_some_and(|l| l.offset() == file.offset()) {
                out.pop();
            }
            out.push(file);
        }
        Ok(CarvedSet {
            files: out,
            geometry: headers.last().map(|h| h.geometry),
        })
    }

    /// Replaces the session's `CARVED` block by one holding exactly this
    /// set (as one run of every kind, split as needed). Written with the
    /// session's next checkpoint.
    pub fn store_compacted(&self, session: &mut Session) -> Result<(), SessionError> {
        let payloads = match self.geometry {
            Some(geometry) => encode_run(KindSet::all(), geometry, &self.files),
            None => Vec::new(),
        };
        session.replace_opaque(OpaqueKind::Carved, payloads)
    }
}

/// Appends `run` to the session's `CARVED` block (I5); written with the
/// session's next checkpoint. The engine calls this after every carving
/// job, completed or not, together with [`CarvedSet::apply`].
pub fn store_run(session: &mut Session, run: &CarveRun) -> Result<(), SessionError> {
    for payload in encode_run(run.kinds, run.geometry, &run.files) {
        session.append_opaque(OpaqueKind::Carved, payload)?;
    }
    Ok(())
}

/// Empties the session's `CARVED` block: `geometry_set` on the main
/// instance, `instance_set` and `scan_start` discard the carved files (I5).
pub fn discard_carved(session: &mut Session) -> Result<(), SessionError> {
    session.replace_opaque(OpaqueKind::Carved, Vec::new())
}

/// The payloads of one run: the first replaces `kinds`, the others only
/// add files.
fn encode_run(kinds: KindSet, geometry: CarveGeometry, files: &[CarvedFile]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut chunks = files.chunks(MAX_FILES_PER_PAYLOAD);
    let first = chunks.next().unwrap_or(&[]);
    out.push(encode(kinds, geometry, first));
    for chunk in chunks {
        out.push(encode(KindSet::EMPTY, geometry, chunk));
    }
    out
}

fn encode(kinds: KindSet, geometry: CarveGeometry, files: &[CarvedFile]) -> Vec<u8> {
    let mut b = Vec::with_capacity(
        files
            .len()
            .saturating_mul(FILE_LEN)
            .saturating_add(HEADER_LEN),
    );
    b.extend_from_slice(&MAGIC);
    b.extend_from_slice(&kinds.bits().to_le_bytes());
    b.extend_from_slice(&geometry.cluster_size().to_le_bytes());
    b.extend_from_slice(&geometry.volume_offset().to_le_bytes());
    b.extend_from_slice(&u64::try_from(files.len()).unwrap_or(u64::MAX).to_le_bytes());
    for f in files {
        b.extend_from_slice(&f.offset().to_le_bytes());
        b.extend_from_slice(&f.len().to_le_bytes());
        b.push(carve_kind_code(f.kind()));
        let mut flags = 0_u8;
        if f.end_verified() {
            flags |= FLAG_END_VERIFIED;
        }
        if f.confidence() == hds_carve::Confidence::Exact {
            flags |= FLAG_EXACT;
        }
        b.push(flags);
        let ext = f.ext().as_bytes();
        let mut padded = [0_u8; 8];
        for (slot, &c) in padded.iter_mut().zip(ext) {
            *slot = c;
        }
        b.push(u8::try_from(ext.len()).unwrap_or(0));
        b.extend_from_slice(&padded);
        b.push(0);
    }
    b
}

/// The fixed part of a payload.
struct Header {
    kinds: KindSet,
    geometry: CarveGeometry,
    count: usize,
}

fn le_u64(b: &[u8], at: usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    Some(u64::from_le_bytes(b.get(at..end)?.try_into().ok()?))
}

fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    Some(u32::from_le_bytes(b.get(at..end)?.try_into().ok()?))
}

impl Header {
    fn parse(b: &[u8]) -> Result<Header, Problem> {
        if b.get(..4) != Some(&MAGIC[..]) {
            return Err(Problem("not a carved-files payload"));
        }
        let kinds = le_u32(b, 4)
            .and_then(KindSet::from_bits)
            .ok_or(Problem("unknown kind"))?;
        let geometry = match (le_u64(b, 8), le_u64(b, 16)) {
            (Some(c), Some(o)) => CarveGeometry::new(c, o),
            _ => None,
        }
        .ok_or(Problem("invalid geometry"))?;
        let count = le_u64(b, 24).ok_or(Problem("cut short"))?;
        // the count must match the bytes present: nothing is sized by it
        let body = b.len().checked_sub(HEADER_LEN).ok_or(Problem("cut short"))?;
        if body % FILE_LEN != 0 || u64::try_from(body / FILE_LEN).ok() != Some(count) {
            return Err(Problem("file count does not match the payload"));
        }
        Ok(Header {
            kinds,
            geometry,
            count: body / FILE_LEN,
        })
    }

    fn files<'a>(&self, b: &'a [u8]) -> impl Iterator<Item = Result<CarvedFile, Problem>> + 'a {
        b.get(HEADER_LEN..)
            .unwrap_or(&[])
            .chunks_exact(FILE_LEN)
            .take(self.count)
            .map(decode_file)
    }
}

fn decode_file(r: &[u8]) -> Result<CarvedFile, Problem> {
    let offset = le_u64(r, 0).ok_or(Problem("cut short"))?;
    let len = le_u64(r, 8).ok_or(Problem("cut short"))?;
    let field = |at: usize| r.get(at).copied().ok_or(Problem("cut short"));
    let kind: FileKind = carve_kind_from_code(field(16)?).ok_or(Problem("unknown kind"))?;
    let flags = field(17)?;
    let ext_len = usize::from(field(18)?);
    if flags & !(FLAG_END_VERIFIED | FLAG_EXACT) != 0 || field(27)? != 0 {
        return Err(Problem("unknown flags"));
    }
    let padded = r.get(19..27).ok_or(Problem("cut short"))?;
    let (ext, rest) = padded
        .split_at_checked(ext_len)
        .ok_or(Problem("invalid extension"))?;
    if rest.iter().any(|&c| c != 0) {
        return Err(Problem("invalid extension"));
    }
    let ext = std::str::from_utf8(ext)
        .ok()
        .and_then(ExtName::new)
        .ok_or(Problem("invalid extension"))?;
    CarvedFile::new(
        offset,
        len,
        kind,
        ext,
        flags & FLAG_END_VERIFIED != 0,
        flags & FLAG_EXACT != 0,
    )
    .ok_or(Problem("empty or overflowing file"))
}

/// What was wrong with a payload, before its index is known.
struct Problem(&'static str);

impl Problem {
    fn at(self, payload: usize) -> CarvedDecodeError {
        CarvedDecodeError::new(payload, self.0)
    }
}

/// The session's `CARVED` block cannot be read: which payload, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarvedDecodeError {
    /// Index of the payload in the block.
    pub payload: usize,
    /// What is wrong with it.
    pub reason: &'static str,
}

impl CarvedDecodeError {
    fn new(payload: usize, reason: &'static str) -> CarvedDecodeError {
        CarvedDecodeError { payload, reason }
    }
}

impl fmt::Display for CarvedDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "carved files, payload {}: {}",
            self.payload, self.reason
        )
    }
}

impl std::error::Error for CarvedDecodeError {}

#[cfg(test)]
pub(super) fn encode_for_tests(
    kinds: KindSet,
    geometry: CarveGeometry,
    files: &[CarvedFile],
) -> Vec<u8> {
    encode(kinds, geometry, files)
}
