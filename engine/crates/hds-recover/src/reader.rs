//! Reading the content of one file by the rules of ALGORITHMS H7:
//! [`ContentReader`], the carving adapter [`CarveReader`] and the preview
//! of `read_file` ([`read_preview`]).
//!
//! Every read goes through the session's [`RangedSource`] with
//! [`read_resilient`] in sector-aligned pieces of at most [`READ_CHUNK`]
//! bytes, and nothing outside the session range is ever requested: the
//! parts of a run that lie outside it read as zeros and are reported as
//! `out_of_bounds` (S2a), unreadable sectors read as zeros and are reported
//! as `bad_sectors`, and a device that stops answering or a cancel ends the
//! read with its own error (S7).

use std::cell::{Cell, RefCell};
use std::fmt;
use std::io;
use std::ops::Range;
use std::sync::atomic::AtomicBool;

use hds_carve::{matches_extension, sniff, FileKind, ReadAt};
use hds_io::{
    read_resilient, AlignedBuf, BlockSource, OutOfRange, RangeSet, RangedSource, ReadOutcome,
};
use hds_ntfs::{apply_fixups, fnv1a64, FixupStatus};

use crate::health::{Flag, FlagSet};
use crate::source::{FileSource, Run};

/// Largest piece read from the source at once: 8 MiB, as the scan reads.
pub const READ_CHUNK: usize = 8 << 20;

/// How many bytes of a file's start the signature check looks at
/// ([`hds_carve::matches_extension`] wants up to the first kilobyte: a PDF
/// header may sit anywhere in it).
pub const HEAD_LEN: usize = 1024;

/// Largest `length` of a `read_file` preview (PROTOCOL `read_file`).
pub const MAX_PREVIEW_LEN: usize = 8 << 20;

/// Most ranges a [`ReadReport`] lists per kind; bytes beyond are still
/// counted.
pub const MAX_LISTED_RANGES: usize = 1024;

/// Smallest scratch buffer the readers allocate.
const MIN_SCRATCH: usize = 4096;

/// Why a read of a file's content stopped.
#[derive(Debug)]
pub enum ReadError {
    /// The source stopped answering (S7): nothing may be recorded as read
    /// or as bad. A job ends with `source_lost`.
    SourceLost(io::Error),
    /// The cancel flag was raised.
    Cancelled,
    /// Resident content: the record's bytes no longer have the hash the
    /// scan recorded (`failed:record_changed`).
    RecordChanged,
    /// Resident content: sectors of the record cannot be read, so the
    /// record cannot be verified (absolute byte ranges).
    RecordUnreadable {
        /// The unreadable parts of the record, absolute byte ranges.
        bad: Vec<Range<u64>>,
    },
    /// The description of the content cannot be read as given: a record
    /// outside the session range, a value outside its record, an
    /// impossible record size.
    Invalid(String),
    /// A read failed for a reason that is neither the medium nor the
    /// device (an internal error).
    Io(io::Error),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::SourceLost(e) => write!(f, "the source stopped answering: {e}"),
            ReadError::Cancelled => f.write_str("cancelled"),
            ReadError::RecordChanged => {
                f.write_str("the MFT record holding the content has changed since the scan")
            }
            ReadError::RecordUnreadable { bad } => {
                let bytes: u64 = bad.iter().map(|r| r.end.saturating_sub(r.start)).sum();
                write!(
                    f,
                    "the MFT record holding the content cannot be read ({bytes} unreadable bytes)"
                )
            }
            ReadError::Invalid(why) => write!(f, "the content cannot be located: {why}"),
            ReadError::Io(e) => write!(f, "read error: {e}"),
        }
    }
}

impl std::error::Error for ReadError {}

fn invalid(why: impl Into<String>) -> ReadError {
    ReadError::Invalid(why.into())
}

/// Byte ranges, of which at most [`MAX_LISTED_RANGES`] are kept; bytes of
/// ranges that do not fit are counted only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Tally {
    set: RangeSet,
    unlisted: u64,
}

impl Tally {
    fn add(&mut self, range: Range<u64>) {
        if range.start >= range.end {
            return;
        }
        let would_merge = self
            .set
            .intersects(range.start.saturating_sub(1)..range.end.saturating_add(1));
        if self.set.range_count() < MAX_LISTED_RANGES || would_merge {
            self.set.insert(range);
        } else {
            self.unlisted = self
                .unlisted
                .saturating_add(range.end.saturating_sub(range.start));
        }
    }

    fn bytes(&self) -> u64 {
        self.set.total_len().saturating_add(self.unlisted)
    }
}

/// What the reads of one file met besides good data.
///
/// Byte counts are exact as long as every byte is read once (as recovery
/// does); at most [`MAX_LISTED_RANGES`] ranges of each kind are listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadReport {
    flags: FlagSet,
    bad: Tally,
    bad_disk: Tally,
    out_of_bounds: Tally,
    missing: Tally,
}

impl ReadReport {
    /// Flags the reads raised: `bad_sectors`, `out_of_bounds`,
    /// `missing_fragments` (bytes below the initialized size that no run
    /// covers), `torn_record` (resident content in a torn record).
    pub fn flags(&self) -> FlagSet {
        self.flags
    }

    /// Unreadable bytes, which were returned as zeros.
    pub fn bad_bytes(&self) -> u64 {
        self.bad.bytes()
    }

    /// The unreadable bytes as ranges of file offsets.
    pub fn bad_ranges(&self) -> impl Iterator<Item = Range<u64>> + '_ {
        self.bad.set.iter()
    }

    /// The unreadable bytes as absolute ranges on the source.
    pub fn bad_disk_ranges(&self) -> impl Iterator<Item = Range<u64>> + '_ {
        self.bad_disk.set.iter()
    }

    /// Bytes mapped outside the session range, which were returned as zeros.
    pub fn out_of_bounds_bytes(&self) -> u64 {
        self.out_of_bounds.bytes()
    }

    /// Those bytes as ranges of file offsets.
    pub fn out_of_bounds_ranges(&self) -> impl Iterator<Item = Range<u64>> + '_ {
        self.out_of_bounds.set.iter()
    }

    /// Bytes below the initialized size that no run covers, which were
    /// returned as zeros.
    pub fn missing_bytes(&self) -> u64 {
        self.missing.bytes()
    }

    /// Those bytes as ranges of file offsets.
    pub fn missing_ranges(&self) -> impl Iterator<Item = Range<u64>> + '_ {
        self.missing.set.iter()
    }
}

/// Why a span read stopped (internal).
enum SpanStop {
    Lost(io::Error),
    Cancelled,
    Io(io::Error),
}

impl From<SpanStop> for ReadError {
    fn from(stop: SpanStop) -> ReadError {
        match stop {
            SpanStop::Lost(e) => ReadError::SourceLost(e),
            SpanStop::Cancelled => ReadError::Cancelled,
            SpanStop::Io(e) => ReadError::Io(e),
        }
    }
}

/// What kind of zero-filled range a span read met.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unread {
    /// Unreadable sectors.
    Bad,
    /// Refused by the range check of the source (cannot happen for spans
    /// that were clipped to the session range; handled all the same).
    OutOfRange,
}

fn to_usize(value: u64) -> Option<usize> {
    usize::try_from(value).ok()
}

fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn internal_bug(what: &str) -> io::Error {
    io::Error::other(format!("internal error in the content reader: {what}"))
}

/// `buf[start..end]` with offsets given as `u64`, or an internal error.
fn span_mut(buf: &mut [u8], start: u64, end: u64) -> Result<&mut [u8], SpanStop> {
    match (to_usize(start), to_usize(end)) {
        (Some(s), Some(e)) => buf
            .get_mut(s..e)
            .ok_or_else(|| SpanStop::Io(internal_bug("span"))),
        _ => Err(SpanStop::Io(internal_bug("span"))),
    }
}

fn span(buf: &[u8], start: u64, end: u64) -> Result<&[u8], SpanStop> {
    match (to_usize(start), to_usize(end)) {
        (Some(s), Some(e)) => buf
            .get(s..e)
            .ok_or_else(|| SpanStop::Io(internal_bug("span"))),
        _ => Err(SpanStop::Io(internal_bug("span"))),
    }
}

/// Grows `scratch` to at least `len` bytes (a power of two, at least
/// [`MIN_SCRATCH`]) and returns its first `len` bytes.
fn scratch_of(scratch: &mut AlignedBuf, len: usize) -> Result<&mut [u8], SpanStop> {
    if scratch.len() < len {
        let size = len
            .checked_next_power_of_two()
            .unwrap_or(len)
            .max(MIN_SCRATCH);
        *scratch = AlignedBuf::new(size);
    }
    scratch
        .get_mut(..len)
        .ok_or_else(|| SpanStop::Io(internal_bug("scratch")))
}

/// Reads the absolute range `[start, start + dst.len())`, which must lie
/// inside the session range, into `dst`: sector-aligned requests of at most
/// [`READ_CHUNK`] bytes through `scratch`. Returns the zero-filled parts as
/// absolute ranges inside the requested range.
fn read_span(
    source: &RangedSource,
    start: u64,
    dst: &mut [u8],
    scratch: &mut AlignedBuf,
    cancel: &AtomicBool,
) -> Result<Vec<(Range<u64>, Unread)>, SpanStop> {
    let sector = u64::from(source.sector_size().max(1));
    let chunk = to_u64(READ_CHUNK);
    // Every source has sectors of 512 to 4096 bytes; anything above a chunk
    // could not make progress below.
    if sector > chunk {
        return Err(SpanStop::Io(internal_bug(
            "the source reports an impossible sector size",
        )));
    }
    let end = start
        .checked_add(to_u64(dst.len()))
        .ok_or_else(|| SpanStop::Io(internal_bug("span end")))?;
    let mut unread = Vec::new();
    let mut pos = start;
    while pos < end {
        let aligned = pos.saturating_sub(pos.checked_rem(sector).unwrap_or(0));
        // `aligned > pos - sector >= pos - chunk`, so the piece is never empty.
        let piece_end = end.min(aligned.saturating_add(chunk));
        // The session range is sector-aligned, so rounding up stays inside it.
        let aligned_end = piece_end
            .div_ceil(sector)
            .checked_mul(sector)
            .ok_or_else(|| SpanStop::Io(internal_bug("alignment")))?;
        let request_len = to_usize(aligned_end.saturating_sub(aligned))
            .ok_or_else(|| SpanStop::Io(internal_bug("request length")))?;
        let buf = scratch_of(scratch, request_len)?;
        let from = pos.saturating_sub(aligned);
        let to = piece_end.saturating_sub(aligned);
        match read_resilient(source, aligned, buf, cancel) {
            ReadOutcome::Data { bad } => {
                span_mut(
                    dst,
                    pos.saturating_sub(start),
                    piece_end.saturating_sub(start),
                )?
                .copy_from_slice(span(buf, from, to)?);
                for range in bad {
                    let (s, e) = (range.start.max(pos), range.end.min(piece_end));
                    if s < e {
                        unread.push((s..e, Unread::Bad));
                    }
                }
            }
            ReadOutcome::SourceLost(e) => return Err(SpanStop::Lost(e)),
            ReadOutcome::Cancelled => return Err(SpanStop::Cancelled),
            ReadOutcome::Internal(e) => {
                if OutOfRange::from_io(&e).is_some() {
                    span_mut(
                        dst,
                        pos.saturating_sub(start),
                        piece_end.saturating_sub(start),
                    )?
                    .fill(0);
                    unread.push((pos..piece_end, Unread::OutOfRange));
                } else {
                    return Err(SpanStop::Io(e));
                }
            }
        }
        pos = piece_end;
    }
    Ok(unread)
}

/// Reads the content of one file described by a [`FileSource`] through the
/// session's [`RangedSource`], at any offset and length (ALGORITHMS H7).
///
/// * **Resident** content: the record is read again at its disk offset; the
///   FNV-1a 64 of the bytes as read must equal the stored hash
///   ([`ReadError::RecordChanged`] otherwise), then the fixups are applied
///   and the value is returned. The record is read once per reader, also
///   for an empty value ([`verify_record`](ContentReader::verify_record)).
/// * **Extents**: holes read as zeros; the parts of runs outside the session
///   range read as zeros (`out_of_bounds`, never a bad sector); unreadable
///   sectors read as zeros (`bad_sectors`, with the ranges in the
///   [`ReadReport`]); bytes at or beyond the initialized size are zeros and
///   are not read; bytes below it that no run covers are zeros
///   (`missing_fragments`); nothing beyond the real size is returned.
///
/// A lost device and a cancel are errors of their own
/// ([`ReadError::SourceLost`], [`ReadError::Cancelled`]); the cancel flag is
/// polled before every request to the source and between retries.
///
/// ```
/// use std::sync::atomic::AtomicBool;
/// use std::sync::Arc;
/// use hds_io::testing::MemSource;
/// use hds_io::RangedSource;
/// use hds_recover::{ContentReader, FileSource, Flag, Run};
///
/// let disk: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
/// let source = RangedSource::new(Arc::new(MemSource::new(disk.clone())), 0..8192).unwrap();
/// let file = FileSource::Extents {
///     runs: vec![Run::Data { offset: 1000, len: 100 }, Run::Hole { len: 50 }, Run::Data { offset: 8150, len: 100 }],
///     real_size: 240,
///     initialized_size: 240,
/// };
/// let cancel = AtomicBool::new(false);
/// let mut reader = ContentReader::new(&source, &file, &cancel);
/// let mut out = vec![0xFF; 300];
/// assert_eq!(reader.read_at(0, &mut out).unwrap(), 240);
/// assert_eq!(&out[..100], &disk[1000..1100]);
/// assert!(out[100..150].iter().all(|&b| b == 0)); // the hole
/// assert_eq!(&out[150..192], &disk[8150..8192]);  // up to the end of the range
/// assert!(out[192..240].iter().all(|&b| b == 0)); // outside it
/// assert!(reader.flags().contains(Flag::OutOfBounds));
/// assert_eq!(reader.report().out_of_bounds_bytes(), 48);
/// ```
pub struct ContentReader<'a> {
    source: &'a RangedSource,
    file: &'a FileSource,
    cancel: &'a AtomicBool,
    /// Extents: the file offset at which each run starts (saturating).
    starts: Vec<u64>,
    /// Resident: the value, once the record has been verified.
    resident: Option<Vec<u8>>,
    scratch: AlignedBuf,
    report: ReadReport,
}

impl fmt::Debug for ContentReader<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContentReader")
            .field("file", &self.file)
            .field("report", &self.report)
            .finish()
    }
}

impl<'a> ContentReader<'a> {
    /// A reader of `file` on `source`; `cancel` is polled before every
    /// request to the source.
    pub fn new(source: &'a RangedSource, file: &'a FileSource, cancel: &'a AtomicBool) -> Self {
        Self::with_buffer(source, file, cancel, AlignedBuf::new(0))
    }

    /// Like [`new`](Self::new), reusing a scratch buffer of an earlier
    /// reader ([`into_buffer`](Self::into_buffer)).
    pub fn with_buffer(
        source: &'a RangedSource,
        file: &'a FileSource,
        cancel: &'a AtomicBool,
        buffer: AlignedBuf,
    ) -> Self {
        let starts = match file {
            FileSource::Extents { runs, .. } => {
                let mut at = 0u64;
                runs.iter()
                    .map(|run| {
                        let start = at;
                        at = at.saturating_add(run.len());
                        start
                    })
                    .collect()
            }
            FileSource::Resident { .. } => Vec::new(),
        };
        ContentReader {
            source,
            file,
            cancel,
            starts,
            resident: None,
            scratch: buffer,
            report: ReadReport::default(),
        }
    }

    /// The scratch buffer, for the next reader.
    pub fn into_buffer(self) -> AlignedBuf {
        self.scratch
    }

    /// Size of the content in bytes.
    pub fn size(&self) -> u64 {
        self.file.size()
    }

    /// What the reads so far have met.
    pub fn report(&self) -> &ReadReport {
        &self.report
    }

    /// The flags the reads so far have raised (see [`ReadReport::flags`]).
    pub fn flags(&self) -> FlagSet {
        self.report.flags
    }

    /// Resident content: reads and verifies the record now (also when the
    /// value is empty, so that an unchanged record is part of every
    /// verdict). Extents: does nothing.
    pub fn verify_record(&mut self) -> Result<(), ReadError> {
        match self.file {
            FileSource::Resident { .. } => self.resident_value().map(|_| ()),
            FileSource::Extents { .. } => Ok(()),
        }
    }

    /// Reads up to `out.len()` bytes of content starting at `offset` and
    /// returns how many were read: fewer only at the end of the content, 0
    /// at or beyond it.
    pub fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, ReadError> {
        let size = self.size();
        if offset >= size || out.is_empty() {
            return Ok(0);
        }
        let n =
            to_usize(size.saturating_sub(offset)).map_or(out.len(), |avail| avail.min(out.len()));
        let out = out
            .get_mut(..n)
            .ok_or_else(|| ReadError::Io(internal_bug("output length")))?;
        let file = self.file;
        match file {
            FileSource::Resident { .. } => {
                let value = self.resident_value()?;
                let from = to_usize(offset).ok_or_else(|| invalid("offset"))?;
                let part = from
                    .checked_add(n)
                    .and_then(|to| value.get(from..to))
                    .ok_or_else(|| ReadError::Io(internal_bug("resident value")))?;
                out.copy_from_slice(part);
            }
            FileSource::Extents {
                runs,
                real_size,
                initialized_size,
            } => self.read_extents(runs, *real_size, *initialized_size, offset, out)?,
        }
        Ok(n)
    }

    fn resident_value(&mut self) -> Result<&[u8], ReadError> {
        if self.resident.is_none() {
            let value = self.load_resident()?;
            self.resident = Some(value);
        }
        Ok(self.resident.as_deref().unwrap_or_default())
    }

    fn load_resident(&mut self) -> Result<Vec<u8>, ReadError> {
        let FileSource::Resident {
            record_offset,
            record_size,
            value_offset,
            value_len,
            hash,
        } = *self.file
        else {
            return Err(ReadError::Io(internal_bug("not resident")));
        };
        if !(512..=65_536).contains(&record_size) || record_size % 512 != 0 {
            return Err(invalid(format!(
                "a record size of {record_size} bytes is impossible"
            )));
        }
        let value_end = u64::from(value_offset).saturating_add(u64::from(value_len));
        if value_end > u64::from(record_size) {
            return Err(invalid(format!(
                "the value ({value_len} bytes at {value_offset}) does not fit the record of {record_size} bytes"
            )));
        }
        let record_range = record_offset
            .checked_add(u64::from(record_size))
            .map(|end| record_offset..end)
            .ok_or_else(|| invalid("the record lies beyond the end of the address space"))?;
        if !self.source.contains(&record_range) {
            return Err(invalid(format!(
                "the record at {record_offset} lies outside the session range {}..{}",
                self.source.start(),
                self.source.end()
            )));
        }
        let mut record = vec![0u8; to_usize(u64::from(record_size)).unwrap_or(0)];
        let unread = read_span(
            self.source,
            record_offset,
            &mut record,
            &mut self.scratch,
            self.cancel,
        )?;
        if !unread.is_empty() {
            let bad: Vec<Range<u64>> = unread.into_iter().map(|(range, _)| range).collect();
            self.report.flags.insert(Flag::BadSectors);
            for range in &bad {
                self.report.bad_disk.add(range.clone());
            }
            return Err(ReadError::RecordUnreadable { bad });
        }
        if fnv1a64(&record) != hash {
            return Err(ReadError::RecordChanged);
        }
        match apply_fixups(&mut record) {
            Ok(FixupStatus::Ok) => {}
            Ok(FixupStatus::Torn { .. }) => self.report.flags.insert(Flag::TornRecord),
            Err(e) => {
                return Err(invalid(format!(
                    "the record's update sequence array does not fit it ({e:?})"
                )))
            }
        }
        let from = to_usize(u64::from(value_offset)).unwrap_or(usize::MAX);
        let to = to_usize(value_end).unwrap_or(usize::MAX);
        record
            .get(from..to)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| invalid("the value does not fit the record"))
    }

    fn read_extents(
        &mut self,
        runs: &[Run],
        real_size: u64,
        initialized_size: u64,
        offset: u64,
        out: &mut [u8],
    ) -> Result<(), ReadError> {
        // `offset + out.len()` is at most the real size (see read_at).
        let end = offset.saturating_add(to_u64(out.len()));
        let valid_end = initialized_size.min(real_size);
        if end > valid_end {
            let from = valid_end.saturating_sub(offset);
            span_mut(out, from, end.saturating_sub(offset))?.fill(0);
        }
        let data_end = end.min(valid_end);
        let mut pos = offset;
        let mut index = self
            .starts
            .partition_point(|&start| start <= pos)
            .saturating_sub(1);
        while pos < data_end {
            let (Some(run), Some(&run_start)) = (runs.get(index), self.starts.get(index)) else {
                // Beyond the last run: the chain is shorter than the file.
                span_mut(
                    out,
                    pos.saturating_sub(offset),
                    data_end.saturating_sub(offset),
                )?
                .fill(0);
                self.report.missing.add(pos..data_end);
                self.report.flags.insert(Flag::MissingFragments);
                break;
            };
            let run_end = run_start.saturating_add(run.len());
            if run_end <= pos {
                index = index.saturating_add(1);
                continue;
            }
            let piece_end = run_end.min(data_end);
            let dst = span_mut(
                out,
                pos.saturating_sub(offset),
                piece_end.saturating_sub(offset),
            )?;
            match *run {
                Run::Hole { .. } => dst.fill(0),
                Run::Data {
                    offset: disk_offset,
                    ..
                } => {
                    // `run_start <= pos`: runs are visited in order from the one that holds `offset`.
                    let disk_start = disk_offset.checked_add(pos.saturating_sub(run_start));
                    Self::read_disk(
                        self.source,
                        &mut self.scratch,
                        self.cancel,
                        &mut self.report,
                        disk_start,
                        pos,
                        dst,
                    )?;
                }
            }
            pos = piece_end;
            index = index.saturating_add(1);
        }
        Ok(())
    }

    /// Fills `dst` with the bytes at absolute `disk_start` (`None`: beyond
    /// the address space), which hold the file's bytes from `file_pos` on.
    fn read_disk(
        source: &RangedSource,
        scratch: &mut AlignedBuf,
        cancel: &AtomicBool,
        report: &mut ReadReport,
        disk_start: Option<u64>,
        file_pos: u64,
        dst: &mut [u8],
    ) -> Result<(), ReadError> {
        let len = to_u64(dst.len());
        let file_end = file_pos.saturating_add(len);
        let range = source.range();
        let inside = disk_start.and_then(|a| {
            let b = a.saturating_add(len);
            let (s, e) = (a.max(range.start), b.min(range.end));
            (s < e).then_some((a, s, e))
        });
        let Some((a, inside_start, inside_end)) = inside else {
            dst.fill(0);
            report.out_of_bounds.add(file_pos..file_end);
            report.flags.insert(Flag::OutOfBounds);
            return Ok(());
        };
        let head = inside_start.saturating_sub(a);
        let inside_len = inside_end.saturating_sub(inside_start);
        let tail = head.saturating_add(inside_len);
        if head > 0 {
            span_mut(dst, 0, head)?.fill(0);
            report
                .out_of_bounds
                .add(file_pos..file_pos.saturating_add(head));
            report.flags.insert(Flag::OutOfBounds);
        }
        if tail < len {
            span_mut(dst, tail, len)?.fill(0);
            report
                .out_of_bounds
                .add(file_pos.saturating_add(tail)..file_end);
            report.flags.insert(Flag::OutOfBounds);
        }
        let unread = read_span(
            source,
            inside_start,
            span_mut(dst, head, tail)?,
            scratch,
            cancel,
        )?;
        for (range, kind) in unread {
            let file_start = file_pos
                .saturating_add(head)
                .saturating_add(range.start.saturating_sub(inside_start));
            let file_range =
                file_start..file_start.saturating_add(range.end.saturating_sub(range.start));
            match kind {
                Unread::Bad => {
                    report.bad_disk.add(range);
                    report.bad.add(file_range);
                    report.flags.insert(Flag::BadSectors);
                }
                Unread::OutOfRange => {
                    report.out_of_bounds.add(file_range);
                    report.flags.insert(Flag::OutOfBounds);
                }
            }
        }
        Ok(())
    }
}

/// Why a [`CarveReader`] read failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarveFailure {
    /// Unreadable sectors: the candidate is abandoned, the job goes on.
    BadSectors,
    /// The source stopped answering: the job ends with `source_lost`.
    SourceLost,
    /// The cancel flag was raised.
    Cancelled,
    /// Any other read error (an internal error).
    Other,
}

/// [`hds_carve::ReadAt`] over the session's [`RangedSource`], for carving:
/// offset 0 is the candidate's first byte at absolute `start`, and the data
/// ends at the end of the session range or at `start + max_len`, whichever
/// comes first.
///
/// A read that meets unreadable sectors, a lost device or a cancel fails
/// (so `hds_carve` gives up on the candidate instead of guessing across
/// the gap); [`failure`](CarveReader::failure) tells the job which of them
/// it was, because a lost device must end the job (S7) while a bad sector
/// only ends the candidate.
pub struct CarveReader<'a> {
    source: &'a RangedSource,
    start: u64,
    end: u64,
    cancel: &'a AtomicBool,
    scratch: RefCell<AlignedBuf>,
    failure: Cell<Option<CarveFailure>>,
}

impl fmt::Debug for CarveReader<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CarveReader")
            .field("start", &self.start)
            .field("end", &self.end)
            .field("failure", &self.failure.get())
            .finish()
    }
}

impl<'a> CarveReader<'a> {
    /// A reader of the candidate at absolute offset `start`, at most
    /// `max_len` bytes long.
    pub fn new(source: &'a RangedSource, start: u64, max_len: u64, cancel: &'a AtomicBool) -> Self {
        CarveReader {
            source,
            start,
            end: start.saturating_add(max_len).min(source.end()),
            cancel,
            scratch: RefCell::new(AlignedBuf::new(0)),
            failure: Cell::new(None),
        }
    }

    /// Length of the data the candidate can read.
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether no data can be read at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Why the most recent failed read failed; `None` while every read has
    /// succeeded.
    pub fn failure(&self) -> Option<CarveFailure> {
        self.failure.get()
    }

    fn fail(&self, failure: CarveFailure, error: io::Error) -> io::Error {
        self.failure.set(Some(failure));
        error
    }
}

impl ReadAt for CarveReader<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let Some(abs) = self.start.checked_add(offset) else {
            return Ok(0);
        };
        if abs >= self.end || abs < self.source.start() || buf.is_empty() {
            return Ok(0);
        }
        let n =
            to_usize(self.end.saturating_sub(abs)).map_or(buf.len(), |avail| avail.min(buf.len()));
        let dst = buf
            .get_mut(..n)
            .ok_or_else(|| internal_bug("carve read length"))?;
        // Never re-entered; a fresh buffer if it were, rather than a panic.
        let mut fresh = AlignedBuf::new(0);
        let mut borrowed = self.scratch.try_borrow_mut();
        let scratch = match borrowed.as_deref_mut() {
            Ok(scratch) => scratch,
            Err(_) => &mut fresh,
        };
        match read_span(self.source, abs, dst, scratch, self.cancel) {
            Ok(unread) => match unread.first() {
                None => Ok(n),
                Some((range, _)) => Err(self.fail(
                    CarveFailure::BadSectors,
                    io::Error::other(format!(
                        "unreadable sectors at {}..{}",
                        range.start, range.end
                    )),
                )),
            },
            Err(SpanStop::Lost(e)) => Err(self.fail(CarveFailure::SourceLost, e)),
            Err(SpanStop::Cancelled) => Err(self.fail(
                CarveFailure::Cancelled,
                io::Error::new(io::ErrorKind::Interrupted, "cancelled"),
            )),
            Err(SpanStop::Io(e)) => Err(self.fail(CarveFailure::Other, e)),
        }
    }
}

/// The result of [`read_preview`]: the `read_file` response without the
/// encoding.
#[derive(Debug, Clone)]
pub struct Preview {
    /// The bytes read: at most the requested length, fewer at the end.
    pub data: Vec<u8>,
    /// Size of the whole content.
    pub size: u64,
    /// Whether `data` reaches the end of the content.
    pub eof: bool,
    /// The sniffed kind of `data` when the offset is 0 ([`hds_carve::sniff`]).
    pub kind: Option<FileKind>,
    /// The flags the read raised; when the offset is 0 also
    /// `signature_ok` or `signature_mismatch` (if the extension has an
    /// opinion).
    pub flags: FlagSet,
    /// Details of what the read met.
    pub report: ReadReport,
}

/// Why [`read_preview`] failed.
#[derive(Debug)]
pub enum PreviewError {
    /// More than [`MAX_PREVIEW_LEN`] bytes were asked for.
    TooLong {
        /// The requested length.
        requested: usize,
    },
    /// The content could not be read.
    Read(ReadError),
}

impl fmt::Display for PreviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PreviewError::TooLong { requested } => write!(
                f,
                "{requested} bytes requested; a preview is at most {MAX_PREVIEW_LEN} bytes"
            ),
            PreviewError::Read(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for PreviewError {}

impl From<ReadError> for PreviewError {
    fn from(e: ReadError) -> PreviewError {
        PreviewError::Read(e)
    }
}

/// The signature flag for the first bytes of a file with extension `ext`
/// (ALGORITHMS H9): `signature_ok`, `signature_mismatch` or none when
/// `hds_carve` has no opinion or the file is empty.
pub fn signature_flag(head: &[u8], size: u64, ext: &str) -> Option<Flag> {
    if size == 0 || head.is_empty() {
        return None;
    }
    let head = head.get(..HEAD_LEN).unwrap_or(head);
    matches_extension(head, ext).map(|ok| {
        if ok {
            Flag::SignatureOk
        } else {
            Flag::SignatureMismatch
        }
    })
}

/// Up to `length` bytes of a file's content at `offset`, for `read_file`
/// (PROTOCOL): through the same reader as recovery, with the flags met.
/// `ext` is the file's extension (lower case, no dot) for the signature
/// check, which is made when `offset` is 0 on the first [`HEAD_LEN`] bytes
/// (read in addition when `length` is shorter).
///
/// Resident content is verified against its record hash even when nothing
/// is returned.
pub fn read_preview(
    source: &RangedSource,
    file: &FileSource,
    ext: &str,
    offset: u64,
    length: usize,
    cancel: &AtomicBool,
) -> Result<Preview, PreviewError> {
    if length > MAX_PREVIEW_LEN {
        return Err(PreviewError::TooLong { requested: length });
    }
    let mut reader = ContentReader::new(source, file, cancel);
    reader.verify_record()?;
    let size = reader.size();
    let want = to_usize(size.saturating_sub(offset)).map_or(length, |avail| avail.min(length));
    let mut data = vec![0u8; want];
    let n = reader.read_at(offset, &mut data)?;
    data.truncate(n);
    let eof = offset.saturating_add(to_u64(n)) >= size;
    let mut kind = None;
    let mut flags = FlagSet::new();
    if offset == 0 {
        kind = sniff(&data);
        let head_len = to_usize(size.min(to_u64(HEAD_LEN))).unwrap_or(HEAD_LEN);
        let flag = if data.len() >= head_len {
            signature_flag(&data, size, ext)
        } else {
            let mut head = vec![0u8; head_len];
            let got = reader.read_at(0, &mut head)?;
            head.truncate(got);
            signature_flag(&head, size, ext)
        };
        flags.extend(flag);
    }
    flags |= reader.flags();
    Ok(Preview {
        data,
        size,
        eof,
        kind,
        flags,
        report: reader.report,
    })
}

#[cfg(test)]
mod tests;
