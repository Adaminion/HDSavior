//! Imaging (ALGORITHMS J): a sequential copy of a byte range of the source
//! to a file, with [`read_resilient`].
//!
//! * Unreadable sectors are zero-filled and listed in `<image>.bad.txt`
//!   (one line per range: image offset, length, source offset, in bytes).
//! * `<image>.hdsimg` records the source's identity, the range and the
//!   number of bytes that are confirmed: written **and** flushed. It is
//!   rewritten after a flush every 1 GiB and when the job stops, by a
//!   temporary name and a rename that never replaces anything but the old
//!   record itself.
//! * `resume` reopens the image without following links, checks the
//!   identity and truncates the image to the confirmed count: the image's
//!   length alone is never trusted. Without `resume` an existing image is
//!   refused.
//! * The destination guard protects the imaged range in addition to the
//!   caller's protected ranges; an image larger than the destination file
//!   system's maximum file size is refused before anything is written.
//! * A lost source ends the job with `source_lost` without extending the
//!   image (S7).

use std::fmt;
use std::fmt::Write as _;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use hds_io::out::{
    classify_error, split_file_path, CommitOutcome, OnExists, OutDir, OutErrorKind, OutFile,
    Timestamps,
};
use hds_io::{
    destination_check, read_resilient, AlignedBuf, BlockSource, DestinationCheck, Protected,
    Purpose, RangedSource, ReadOutcome, SourceKind,
};
use hds_ntfs::fnv1a64;
use serde::Serialize;

use crate::progress::{eta_seconds, Meter};
use crate::recover::{display_path, fallback_limits, JobError, JobStatus};

/// Bytes read and written at once.
const CHUNK: usize = 8 << 20;

/// The side file is rewritten after this many bytes (and a flush).
const CHECKPOINT_BYTES: u64 = 1 << 30;

/// First line of the side file.
const SIDE_HEADER: &str = "HDSavior image progress 1";

/// Largest side file that is read back.
const SIDE_MAX: u64 = 64 * 1024;

/// Largest bad-sector list that is read back on resume.
const BAD_MAP_MAX: u64 = 256 << 20;

/// Content samples kept in the side file.
const MAX_SAMPLES: usize = 8;

/// How an imaging job writes and what it may write to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageOptions {
    /// Continue an image an earlier job left unfinished.
    pub resume: bool,
    /// Consent to a destination on the source disk outside the protected
    /// ranges (S3).
    pub allow_same_disk: bool,
    /// Consent to a destination whose disk cannot be determined (S3).
    pub allow_unknown_disk: bool,
    /// Byte ranges no output may touch besides the imaged range: the
    /// session's protected ranges, if a session is open.
    pub protected: Vec<Protected>,
}

/// Why an imaging job did not start.
#[derive(Debug)]
pub enum ImageStartError {
    /// The destination guard does not allow the location (protocol
    /// `destination_refused`).
    DestinationRefused(Box<DestinationCheck>),
    /// The image (or its progress record or bad-sector list) exists and
    /// `resume` was not asked for, or stale side files lie next to a missing
    /// image (protocol `exists`).
    Exists(PathBuf),
    /// Less free space than the rest of the image needs (protocol
    /// `no_space`).
    NoSpace {
        /// Bytes free at the destination.
        free_bytes: u64,
        /// Bytes the image still needs.
        need_bytes: u64,
    },
    /// The image would be larger than the destination's file system holds
    /// (protocol `no_space`, with this explanation).
    TooLarge {
        /// Size of the image.
        size: u64,
        /// The file system's largest file.
        max_file_bytes: u64,
    },
    /// Resume: the image belongs to another source or range; lists the
    /// checks that failed (`kind`, `size`, `sector_size`, `model`, `serial`,
    /// `range`, `content`) (protocol `source_mismatch`).
    SourceMismatch(Vec<String>),
    /// Resume: there is no usable progress record next to the image, or the
    /// image is shorter than it says (protocol `exists`: the image cannot be
    /// continued and must not be overwritten).
    CannotResume(String),
    /// The source stopped answering while the image was being checked
    /// (protocol `source_lost`).
    SourceLost(io::Error),
    /// The destination could not be prepared (protocol `io_error`).
    Io(io::Error),
}

impl fmt::Display for ImageStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageStartError::DestinationRefused(check) => {
                write!(f, "the destination is refused: {}", check.message)
            }
            ImageStartError::Exists(path) => write!(f, "{} exists already", path.display()),
            ImageStartError::NoSpace {
                free_bytes,
                need_bytes,
            } => write!(
                f,
                "the destination has {free_bytes} bytes free; the image needs {need_bytes}"
            ),
            ImageStartError::TooLarge {
                size,
                max_file_bytes,
            } => write!(
                f,
                "the image of {size} bytes is larger than the destination's file system can hold ({max_file_bytes} bytes per file)"
            ),
            ImageStartError::SourceMismatch(checks) => write!(
                f,
                "the image belongs to another source or range ({})",
                checks.join(", ")
            ),
            ImageStartError::CannotResume(why) => write!(f, "the image cannot be continued: {why}"),
            ImageStartError::SourceLost(e) => write!(f, "the source stopped answering: {e}"),
            ImageStartError::Io(e) => write!(f, "the destination cannot be prepared: {e}"),
        }
    }
}

impl std::error::Error for ImageStartError {}

fn start_io(e: io::Error) -> ImageStartError {
    if classify_error(&e) == OutErrorKind::NoSpace {
        ImageStartError::NoSpace {
            free_bytes: 0,
            need_bytes: 0,
        }
    } else {
        ImageStartError::Io(e)
    }
}

/// An `image_progress` event.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ImageProgress {
    /// Bytes of the range copied (and, on resume, confirmed before).
    pub bytes_done: u64,
    /// Bytes of the range.
    pub bytes_total: u64,
    /// Bytes per second over the last five seconds.
    pub speed_bps: u64,
    /// Remaining bytes / speed, rounded up; `None` at speed 0.
    pub eta_seconds: Option<u64>,
    /// Whole seconds since the job started.
    pub elapsed_seconds: u64,
    /// Unreadable bytes so far (zero-filled in the image).
    pub bad_bytes: u64,
}

/// The `image_done` event.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ImageSummary {
    /// How the job ended.
    pub status: JobStatus,
    /// Why it failed.
    pub error: Option<JobError>,
    /// Bytes in the image that are confirmed.
    pub bytes_written: u64,
    /// Unreadable bytes, zero-filled in the image.
    pub bad_bytes: u64,
    /// The image, as a display path.
    pub path: String,
    /// The bad-sector list; `None` when there were no unreadable sectors.
    pub bad_map_path: Option<String>,
    /// A sentence about the error, for the log; not part of the event.
    #[serde(skip)]
    pub message: Option<String>,
}

/// Receives the events of an imaging job.
pub trait ImageObserver {
    /// A progress event (at most four per second).
    fn progress(&mut self, progress: &ImageProgress);
}

/// An observer that ignores everything.
impl ImageObserver for () {
    fn progress(&mut self, _: &ImageProgress) {}
}

/// Who the source is, as far as an image file can tell later.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    kind: SourceKind,
    path: String,
    size: u64,
    sector_size: u32,
    model: Option<String>,
    serial: Option<String>,
}

impl Identity {
    fn of(source: &dyn BlockSource) -> Identity {
        let info = source.info();
        Identity {
            kind: info.kind,
            path: one_line(&info.path),
            size: info.size,
            sector_size: info.sector_size,
            model: info.model.as_deref().map(one_line),
            serial: info.serial.as_deref().map(one_line),
        }
    }
}

/// `text` with every control character replaced, so that it fits one line.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// The content of the side file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Progress {
    identity: Identity,
    range: Range<u64>,
    confirmed: u64,
    /// `(absolute offset, FNV-1a 64)` of single sectors of the confirmed part.
    samples: Vec<(u64, u64)>,
}

impl Progress {
    fn to_text(&self) -> String {
        let id = &self.identity;
        let mut text = String::new();
        let _ = writeln!(text, "{SIDE_HEADER}");
        let kind = match id.kind {
            SourceKind::Disk => "disk",
            SourceKind::Image => "image",
        };
        let _ = writeln!(text, "kind {kind}");
        let _ = writeln!(text, "path {}", id.path);
        let _ = writeln!(text, "size {}", id.size);
        let _ = writeln!(text, "sector_size {}", id.sector_size);
        if let Some(model) = &id.model {
            let _ = writeln!(text, "model {model}");
        }
        if let Some(serial) = &id.serial {
            let _ = writeln!(text, "serial {serial}");
        }
        let _ = writeln!(text, "range {} {}", self.range.start, self.range.end);
        let _ = writeln!(text, "confirmed {}", self.confirmed);
        for (offset, hash) in &self.samples {
            let _ = writeln!(text, "sample {offset} {hash:016x}");
        }
        text
    }

    fn parse(text: &str) -> Result<Progress, String> {
        let mut lines = text.lines();
        if lines.next() != Some(SIDE_HEADER) {
            return Err("it is not a progress record of this version".into());
        }
        let (mut kind, mut path, mut size, mut sector_size) = (None, None, None, None);
        let (mut model, mut serial, mut range, mut confirmed) = (None, None, None, None);
        let mut samples = Vec::new();
        let number = |value: &str| {
            value
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("{value:?} is not a number"))
        };
        for line in lines {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "kind" => {
                    kind = Some(match value {
                        "disk" => SourceKind::Disk,
                        "image" => SourceKind::Image,
                        other => return Err(format!("unknown source kind {other:?}")),
                    })
                }
                "path" => path = Some(value.to_string()),
                "size" => size = Some(number(value)?),
                "sector_size" => {
                    sector_size =
                        Some(u32::try_from(number(value)?).map_err(|_| "bad sector size")?)
                }
                "model" => model = Some(value.to_string()),
                "serial" => serial = Some(value.to_string()),
                "range" => {
                    let (s, e) = value.split_once(' ').ok_or("bad range")?;
                    range = Some(number(s)?..number(e)?);
                }
                "confirmed" => confirmed = Some(number(value)?),
                "sample" => {
                    let (o, h) = value.split_once(' ').ok_or("bad sample")?;
                    let hash = u64::from_str_radix(h.trim(), 16).map_err(|_| "bad sample hash")?;
                    if samples.len() < MAX_SAMPLES {
                        samples.push((number(o)?, hash));
                    }
                }
                // Lines a later version may add.
                _ => {}
            }
        }
        let missing = |what: &str| format!("the record has no {what}");
        Ok(Progress {
            identity: Identity {
                kind: kind.ok_or_else(|| missing("source kind"))?,
                path: path.unwrap_or_default(),
                size: size.ok_or_else(|| missing("source size"))?,
                sector_size: sector_size.ok_or_else(|| missing("sector size"))?,
                model,
                serial,
            },
            range: range.ok_or_else(|| missing("range"))?,
            confirmed: confirmed.ok_or_else(|| missing("confirmed byte count"))?,
            samples,
        })
    }
}

/// One line of the bad-sector list: image offset, length, source offset.
fn bad_line(image_offset: u64, len: u64, source_offset: u64) -> String {
    format!("{image_offset} {len} {source_offset}\r\n")
}

fn bad_header(image_name: &str) -> String {
    format!(
        "# HDSavior: unreadable sectors, recovered as zeros in {}\r\n# image_offset length source_offset (bytes)\r\n",
        one_line(image_name)
    )
}

/// The entries of an existing bad-sector list that lie in the first
/// `confirmed` bytes of the image (clipped to them).
fn parse_bad_map(text: &str, confirmed: u64) -> Result<Vec<(u64, u64, u64)>, String> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_ascii_whitespace().map(str::parse::<u64>);
        match (fields.next(), fields.next(), fields.next(), fields.next()) {
            (Some(Ok(image)), Some(Ok(len)), Some(Ok(source)), None) => {
                if image < confirmed && len > 0 {
                    entries.push((image, len.min(confirmed.saturating_sub(image)), source));
                }
            }
            _ => {
                return Err(format!(
                    "the bad-sector list has a line it cannot read: {line:?}"
                ))
            }
        }
    }
    Ok(entries)
}

/// Reads at most `max` bytes of an existing regular file in `dir`.
fn read_existing(dir: &OutDir, name: &str, max: u64) -> io::Result<String> {
    let file = dir.open_existing(name)?;
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name} is larger than expected"),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{name} is not text")))
}

/// Replaces (or creates) the small file `name` in `dir` with `text`:
/// written under a temporary name, flushed, then renamed; the only thing
/// ever replaced is a regular file of that name (S6).
fn replace_small_file(dir: &OutDir, name: &str, text: &str) -> io::Result<()> {
    let mut part = dir.create_part()?;
    part.write_all(text.as_bytes())?;
    part.sync()?;
    part.finish(&Timestamps::default())?;
    match part.commit(name, OnExists::Overwrite, &|_| false)? {
        CommitOutcome::Committed | CommitOutcome::Overwritten => Ok(()),
        CommitOutcome::NameTaken | CommitOutcome::Skipped => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{name} is taken by something that is not a regular file"),
        )),
    }
}

/// Why a running image stopped.
enum Stop {
    Cancelled,
    SourceLost(String),
    NoSpace(String),
    Io(String),
    Internal(String),
}

/// An imaging job (ALGORITHMS J): a sequential copy of the source's range
/// to a file with [`read_resilient`](hds_io::read_resilient).
///
/// * Unreadable sectors are zero-filled and listed in `<image>.bad.txt`
///   (after two `#` lines, one line per range: image offset, length, source
///   offset, in bytes, CRLF).
/// * `<image>.hdsimg` records the source's identity (kind, size, sector
///   size, model, serial, a few sector hashes), the range and the number of
///   bytes that are confirmed: written **and** flushed. It is rewritten
///   after a flush every 1 GiB, at the start and when the job stops, by a
///   temporary name and a rename that replaces nothing but the old record.
/// * `resume` reopens the image without following links, checks the
///   identity and truncates the image to the confirmed count: the image's
///   length alone is never trusted. Without `resume` an existing image (or
///   record, or list) is refused.
/// * The destination guard protects the imaged range in addition to the
///   caller's protected ranges; an image larger than the destination file
///   system's largest file, or than its free space, is refused before
///   anything is written.
/// * A lost source ends the job with `source_lost` without extending the
///   image (S7); a full destination ends it with `no_space`, keeping only
///   whole chunks.
///
/// [`ImageJob::prepare`] checks and opens everything; [`ImageJob::run`]
/// copies. A prepared job owns everything it needs, so it can be moved to a
/// worker thread. [`image`] does both steps.
pub struct ImageJob {
    source: RangedSource,
    dir: OutDir,
    name: String,
    side_name: String,
    bad_name: String,
    image: OutFile,
    progress: Progress,
    /// The bad-sector list, once it exists.
    bad_file: Option<OutFile>,
    bad_bytes: u64,
    bad_entries: u64,
    checkpoint_bytes: u64,
    /// Bytes read and written at once ([`CHUNK`]; smaller in tests).
    chunk_bytes: usize,
}

impl fmt::Debug for ImageJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageJob")
            .field("image", &self.dir.path().join(&self.name))
            .field("range", &self.progress.range)
            .field("confirmed", &self.progress.confirmed)
            .finish()
    }
}

impl ImageJob {
    /// Checks the destination and prepares the job. The imaged range is the
    /// range of `source`; `dest` is the image file's absolute path.
    pub fn prepare(
        source: RangedSource,
        dest: &Path,
        options: &ImageOptions,
    ) -> Result<ImageJob, ImageStartError> {
        Self::prepare_with(source, dest, options, None)
    }

    /// [`prepare`](Self::prepare) with the destination's largest file
    /// replaced (tests reach the size check this way).
    fn prepare_with(
        source: RangedSource,
        dest: &Path,
        options: &ImageOptions,
        max_file_bytes: Option<u64>,
    ) -> Result<ImageJob, ImageStartError> {
        let range = source.range();
        let total = range.end.saturating_sub(range.start);
        let mut protected = vec![Protected::session(range.clone())];
        protected.extend(options.protected.iter().cloned());
        let allowed = |check: &DestinationCheck| {
            check.is_allowed(options.allow_same_disk, options.allow_unknown_disk)
        };
        let check = destination_check(dest, Purpose::Image, &source, &protected, Some(total));
        if !allowed(&check) {
            return Err(ImageStartError::DestinationRefused(Box::new(check)));
        }
        let (dir_path, name) = split_file_path(dest).map_err(ImageStartError::Io)?;
        let dir = OutDir::create_root(&dir_path).map_err(start_io)?;
        let check = destination_check(
            &dir.path().join(&name),
            Purpose::Image,
            &source,
            &protected,
            Some(total),
        );
        if !allowed(&check) {
            return Err(ImageStartError::DestinationRefused(Box::new(check)));
        }
        dir.remove_leftover_parts(false).map_err(start_io)?;
        let limits = dir.limits().unwrap_or_else(|_| fallback_limits());
        if let Some(max) = max_file_bytes.or(limits.max_file_bytes) {
            if total > max {
                return Err(ImageStartError::TooLarge {
                    size: total,
                    max_file_bytes: max,
                });
            }
        }
        let side_name = format!("{name}.hdsimg");
        let bad_name = format!("{name}.bad.txt");
        let exists = |n: &str| -> Result<bool, ImageStartError> {
            dir.entry(n)
                .map(|e| e.is_some())
                .map_err(ImageStartError::Io)
        };
        let identity = Identity::of(&source);
        let image_exists = exists(&name)?;
        let mut job = if options.resume && image_exists {
            Self::reopen(source, dir, name, side_name, bad_name, identity)?
        } else {
            // A fresh image: nothing of that name may be there, side files included.
            for n in [&name, &side_name, &bad_name] {
                if exists(n)? {
                    return Err(ImageStartError::Exists(dir.path().join(n)));
                }
            }
            check_space(&dir, total)?;
            let image = dir.create_new(&name).map_err(start_io)?;
            ImageJob {
                source,
                dir,
                name,
                side_name,
                bad_name,
                image,
                progress: Progress {
                    identity,
                    range,
                    confirmed: 0,
                    samples: Vec::new(),
                },
                bad_file: None,
                bad_bytes: 0,
                bad_entries: 0,
                checkpoint_bytes: CHECKPOINT_BYTES,
                chunk_bytes: CHUNK,
            }
        };
        if job.progress.confirmed == 0 {
            // The record exists from the start, so that a killed job can be resumed.
            job.write_progress().map_err(start_io)?;
        }
        Ok(job)
    }

    /// Resume: checks the record against the source and reopens the image.
    fn reopen(
        source: RangedSource,
        dir: OutDir,
        name: String,
        side_name: String,
        bad_name: String,
        identity: Identity,
    ) -> Result<ImageJob, ImageStartError> {
        let text = match read_existing(&dir, &side_name, SIDE_MAX) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(ImageStartError::CannotResume(format!(
                    "there is no progress record ({side_name}) next to the image"
                )))
            }
            Err(e) => {
                return Err(ImageStartError::CannotResume(format!(
                    "the progress record cannot be read: {e}"
                )))
            }
        };
        let recorded = Progress::parse(&text).map_err(ImageStartError::CannotResume)?;
        let range = source.range();
        let mut failed = Vec::new();
        let (was, now) = (&recorded.identity, &identity);
        if was.kind != now.kind {
            failed.push("kind");
        }
        if was.size != now.size {
            failed.push("size");
        }
        if was.sector_size != now.sector_size {
            failed.push("sector_size");
        }
        if was.model != now.model {
            failed.push("model");
        }
        if was.serial != now.serial {
            failed.push("serial");
        }
        if recorded.range != range {
            failed.push("range");
        }
        let sector = u64::from(source.sector_size().max(1));
        let confirmed = recorded.confirmed;
        let total = range.end.saturating_sub(range.start);
        if failed.is_empty() && (confirmed > total || !confirmed.is_multiple_of(sector)) {
            return Err(ImageStartError::CannotResume(format!(
                "the recorded byte count {confirmed} does not fit the range"
            )));
        }
        if failed.is_empty() && !Self::samples_match(&source, &recorded, sector)? {
            failed.push("content");
        }
        if !failed.is_empty() {
            return Err(ImageStartError::SourceMismatch(
                failed.into_iter().map(String::from).collect(),
            ));
        }
        check_space(&dir, total.saturating_sub(confirmed))?;
        let mut image = dir.open_existing(&name).map_err(|e| {
            ImageStartError::CannotResume(format!("the image cannot be reopened: {e}"))
        })?;
        let len = image.len().map_err(ImageStartError::Io)?;
        if len < confirmed {
            return Err(ImageStartError::CannotResume(format!(
                "the image holds {len} bytes, fewer than the {confirmed} recorded"
            )));
        }
        image.set_len(confirmed).map_err(ImageStartError::Io)?;
        image
            .seek(SeekFrom::Start(confirmed))
            .map_err(ImageStartError::Io)?;
        // The bad-sector list may name sectors beyond the confirmed part: keep the rest.
        let mut job = ImageJob {
            source,
            dir,
            name,
            side_name,
            bad_name,
            image,
            progress: Progress {
                identity,
                range,
                confirmed,
                samples: recorded.samples,
            },
            bad_file: None,
            bad_bytes: 0,
            bad_entries: 0,
            checkpoint_bytes: CHECKPOINT_BYTES,
            chunk_bytes: CHUNK,
        };
        match read_existing(&job.dir, &job.bad_name, BAD_MAP_MAX) {
            Ok(text) => {
                let entries =
                    parse_bad_map(&text, confirmed).map_err(ImageStartError::CannotResume)?;
                let mut kept = bad_header(&job.name);
                for &(image_offset, len, source_offset) in &entries {
                    kept.push_str(&bad_line(image_offset, len, source_offset));
                    job.bad_bytes = job.bad_bytes.saturating_add(len);
                    job.bad_entries = job.bad_entries.saturating_add(1);
                }
                replace_small_file(&job.dir, &job.bad_name, &kept).map_err(start_io)?;
                let mut file = job
                    .dir
                    .open_existing(&job.bad_name)
                    .map_err(ImageStartError::Io)?;
                file.seek(SeekFrom::End(0)).map_err(ImageStartError::Io)?;
                job.bad_file = Some(file);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ImageStartError::CannotResume(format!(
                    "the bad-sector list cannot be read: {e}"
                )))
            }
        }
        Ok(job)
    }

    /// Whether every recorded sample that can be read now still has its
    /// hash. Samples in sectors that cannot be read decide nothing.
    fn samples_match(
        source: &RangedSource,
        recorded: &Progress,
        sector: u64,
    ) -> Result<bool, ImageStartError> {
        let end = recorded.range.start.saturating_add(recorded.confirmed);
        let mut buf = AlignedBuf::new(usize::try_from(sector).unwrap_or(512));
        let never = AtomicBool::new(false);
        for &(offset, hash) in &recorded.samples {
            let inside = offset >= recorded.range.start
                && offset.saturating_add(sector) <= end
                && offset.is_multiple_of(sector);
            if !inside {
                return Ok(false);
            }
            match read_resilient(source, offset, &mut buf, &never) {
                ReadOutcome::Data { bad } if bad.is_empty() => {
                    if fnv1a64(&buf) != hash {
                        return Ok(false);
                    }
                }
                ReadOutcome::Data { .. } => {}
                ReadOutcome::SourceLost(e) => return Err(ImageStartError::SourceLost(e)),
                ReadOutcome::Cancelled => {}
                ReadOutcome::Internal(e) => return Err(ImageStartError::Io(e)),
            }
        }
        Ok(true)
    }

    /// Bytes of the imaged range (`image_start.bytes`).
    pub fn bytes_total(&self) -> u64 {
        self.progress
            .range
            .end
            .saturating_sub(self.progress.range.start)
    }

    /// Bytes confirmed before this run (non-zero only when resuming).
    pub fn bytes_confirmed(&self) -> u64 {
        self.progress.confirmed
    }

    /// The image file.
    pub fn image_path(&self) -> PathBuf {
        self.dir.path().join(&self.name)
    }

    /// The progress record next to the image.
    pub fn progress_path(&self) -> PathBuf {
        self.dir.path().join(&self.side_name)
    }

    /// The bad-sector list next to the image (it exists only once an
    /// unreadable sector was met).
    pub fn bad_map_path(&self) -> PathBuf {
        self.dir.path().join(&self.bad_name)
    }

    /// Flushes the image and the bad-sector list, then rewrites the record.
    fn write_progress(&mut self) -> io::Result<()> {
        self.image.sync()?;
        if let Some(bad) = &self.bad_file {
            bad.sync()?;
        }
        replace_small_file(&self.dir, &self.side_name, &self.progress.to_text())
    }

    /// Appends unreadable ranges (absolute) to the bad-sector list.
    fn note_bad(&mut self, bad: &[Range<u64>]) -> io::Result<()> {
        if bad.is_empty() {
            return Ok(());
        }
        if self.bad_file.is_none() {
            let mut file = self.dir.create_new(&self.bad_name)?;
            file.write_all(bad_header(&self.name).as_bytes())?;
            self.bad_file = Some(file);
        }
        let start = self.progress.range.start;
        let mut text = String::new();
        let mut bytes = 0u64;
        for range in bad {
            let len = range.end.saturating_sub(range.start);
            text.push_str(&bad_line(
                range.start.saturating_sub(start),
                len,
                range.start,
            ));
            bytes = bytes.saturating_add(len);
        }
        if let Some(file) = &mut self.bad_file {
            file.write_all(text.as_bytes())?;
            file.flush()?;
        }
        // Counted once listed.
        self.bad_bytes = self.bad_bytes.saturating_add(bytes);
        self.bad_entries = self
            .bad_entries
            .saturating_add(u64::try_from(bad.len()).unwrap_or(u64::MAX));
        Ok(())
    }

    /// Remembers one readable sector of a chunk just written as a content
    /// sample: the first sample stays, later ones replace the oldest.
    fn sample(&mut self, chunk: &[u8], chunk_offset: u64, bad: &[Range<u64>]) {
        let sector = usize::try_from(self.source.sector_size().max(1)).unwrap_or(512);
        let Some(first) = chunk.get(..sector) else {
            return;
        };
        let sector_end = chunk_offset.saturating_add(u64::try_from(sector).unwrap_or(512));
        if bad
            .iter()
            .any(|r| r.start < sector_end && chunk_offset < r.end)
        {
            return;
        }
        let entry = (chunk_offset, fnv1a64(first));
        let samples = &mut self.progress.samples;
        if samples.len() >= MAX_SAMPLES && samples.len() > 1 {
            samples.remove(1);
        }
        samples.push(entry);
    }

    /// Copies the rest of the range, then records the result (ALGORITHMS J).
    /// `cancel` is polled before every chunk and between retries.
    pub fn run(mut self, observer: &mut dyn ImageObserver, cancel: &AtomicBool) -> ImageSummary {
        let total = self.bytes_total();
        let start = self.progress.range.start;
        let mut pos = self.progress.confirmed;
        let mut since_checkpoint = 0u64;
        let mut meter = Meter::starting_at(Instant::now(), pos);
        let chunk_len = u64::try_from(self.chunk_bytes).unwrap_or(CHUNK as u64);
        let mut buf = AlignedBuf::new(usize::try_from(total.min(chunk_len)).unwrap_or(CHUNK));
        let mut first_chunk_sampled = !self.progress.samples.is_empty();
        let mut stop: Option<Stop> = None;
        while pos < total {
            if cancel.load(Ordering::Relaxed) {
                stop = Some(Stop::Cancelled);
                break;
            }
            let len = usize::try_from(total.saturating_sub(pos).min(chunk_len)).unwrap_or(CHUNK);
            let Some(chunk) = buf.get_mut(..len) else {
                stop = Some(Stop::Internal("chunk length".into()));
                break;
            };
            let at = start.saturating_add(pos);
            match read_resilient(&self.source, at, chunk, cancel) {
                ReadOutcome::Data { bad } => {
                    // A chunk counts only when its data is written and its
                    // unreadable sectors are listed; otherwise it is cut off again.
                    if let Err(e) = self
                        .image
                        .write_all(chunk)
                        .and_then(|()| self.note_bad(&bad))
                    {
                        let _ = self.image.set_len(pos);
                        let _ = self.image.seek(SeekFrom::Start(pos));
                        stop = Some(write_stop(&e));
                        break;
                    }
                    if !first_chunk_sampled
                        || since_checkpoint.saturating_add(chunk_len) >= self.checkpoint_bytes
                    {
                        self.sample(chunk, at, &bad);
                        first_chunk_sampled = true;
                    }
                    let n = u64::try_from(len).unwrap_or(u64::MAX);
                    pos = pos.saturating_add(n);
                    since_checkpoint = since_checkpoint.saturating_add(n);
                    if since_checkpoint >= self.checkpoint_bytes {
                        self.progress.confirmed = pos;
                        if let Err(e) = self.write_progress() {
                            stop = Some(write_stop(&e));
                            break;
                        }
                        since_checkpoint = 0;
                    }
                }
                ReadOutcome::SourceLost(e) => {
                    stop = Some(Stop::SourceLost(e.to_string()));
                    break;
                }
                ReadOutcome::Cancelled => {
                    stop = Some(Stop::Cancelled);
                    break;
                }
                ReadOutcome::Internal(e) => {
                    stop = Some(Stop::Internal(e.to_string()));
                    break;
                }
            }
            let now = Instant::now();
            if meter.due(now) {
                let rates = meter.event(now, pos);
                observer.progress(&ImageProgress {
                    bytes_done: pos,
                    bytes_total: total,
                    speed_bps: rates.speed_bps,
                    eta_seconds: eta_seconds(total.saturating_sub(pos), rates.speed_bps),
                    elapsed_seconds: rates.elapsed_seconds,
                    bad_bytes: self.bad_bytes,
                });
            }
        }
        // Everything written is flushed before it is recorded as confirmed.
        self.progress.confirmed = pos;
        let recorded = self.write_progress();
        if let (None, Err(e)) = (&stop, &recorded) {
            stop = Some(write_stop(e));
        }
        let (status, error, message) = match stop {
            None => (JobStatus::Completed, None, None),
            Some(Stop::Cancelled) => (JobStatus::Cancelled, None, None),
            Some(Stop::SourceLost(why)) => {
                (JobStatus::Failed, Some(JobError::SourceLost), Some(why))
            }
            Some(Stop::NoSpace(why)) => (JobStatus::Failed, Some(JobError::NoSpace), Some(why)),
            Some(Stop::Io(why)) => (JobStatus::Failed, Some(JobError::IoError), Some(why)),
            Some(Stop::Internal(why)) => (JobStatus::Failed, Some(JobError::Internal), Some(why)),
        };
        ImageSummary {
            status,
            error,
            bytes_written: pos,
            bad_bytes: self.bad_bytes,
            path: display_path(&self.image_path()),
            bad_map_path: (self.bad_entries > 0).then(|| display_path(&self.bad_map_path())),
            message,
        }
    }
}

/// Refuses to start when the destination has less than `need` bytes free.
/// A destination that does not report its free space is not refused here:
/// the job then ends with `no_space` if it fills up.
fn check_space(dir: &OutDir, need: u64) -> Result<(), ImageStartError> {
    let Ok(free) = dir.free_space() else {
        return Ok(());
    };
    if free < need {
        return Err(ImageStartError::NoSpace {
            free_bytes: free,
            need_bytes: need,
        });
    }
    Ok(())
}

fn write_stop(e: &io::Error) -> Stop {
    match classify_error(e) {
        OutErrorKind::NoSpace => Stop::NoSpace(e.to_string()),
        OutErrorKind::TooLarge => {
            Stop::NoSpace(format!("the destination refused a file this large: {e}"))
        }
        _ => Stop::Io(e.to_string()),
    }
}

/// [`ImageJob::prepare`] and [`ImageJob::run`] in one call.
pub fn image(
    source: RangedSource,
    dest: &Path,
    options: &ImageOptions,
    observer: &mut dyn ImageObserver,
    cancel: &AtomicBool,
) -> Result<ImageSummary, ImageStartError> {
    Ok(ImageJob::prepare(source, dest, options)?.run(observer, cancel))
}

#[cfg(test)]
mod tests;
