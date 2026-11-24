//! Reading from a source that may have unreadable sectors or may vanish.
//!
//! "Any error is a bad sector" is wrong twice over: after a USB disconnect
//! *every* request fails at once, and a misaligned request fails because the
//! caller has a bug. [`classify_read_error`] sorts a failed read into the
//! classes of `docs/notes/windows-raw-io.md` (section 6) and
//! [`read_resilient`] acts on them: only a *media* error on a device that
//! proves to be alive becomes a zero-filled bad range.

use std::fmt;
use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::buf::AlignedBuf;
use crate::source::{check_read_request, BlockSource, OutOfRange};

/// Size of the blocks a failed chunk is retried in: 64 KiB.
pub const RETRY_BLOCK_SIZE: u64 = 64 * 1024;

/// After this many consecutive unreadable sectors the rest of the 64 KiB
/// block is given up as bad without further attempts.
pub const MAX_CONSECUTIVE_BAD: u32 = 8;

/// How often one sector is tried while the system reports a lack of resources.
const RESOURCE_ATTEMPTS: u32 = 3;

/// Largest piece [`read_unaligned`] reads through its bounce buffer.
const BOUNCE_SIZE: u64 = 1 << 20;

/// What a failed read means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadFailure {
    /// The sectors cannot be read: zero-fill and record a bad range - but
    /// only once the device has been shown to be alive.
    Media,
    /// The device is gone or was exchanged: the job ends with `source_lost`
    /// and nothing is recorded as read or as bad (safety invariant S7).
    DeviceLost,
    /// The request itself was wrong (alignment, range, handle): an internal
    /// error, never a bad sector.
    Caller,
    /// The system cannot serve a transfer of this size right now: retry with
    /// a smaller one.
    Resource,
    /// The request was cancelled (`CancelSynchronousIo`).
    Cancelled,
    /// In no list: treated like [`Media`](ReadFailure::Media), with the same
    /// proof of liveness.
    Unknown,
}

/// Payload of an `io::Error` whose class is known to the code that made it.
#[derive(Debug)]
struct ClassifiedError {
    class: ReadFailure,
    message: String,
}

impl fmt::Display for ClassifiedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClassifiedError {}

/// Builds an `io::Error` that [`classify_read_error`] sorts into `class`
/// whatever the platform. For [`BlockSource`] implementations that detect a
/// condition themselves (a short read, a simulated fault).
pub fn read_error(class: ReadFailure, message: impl Into<String>) -> io::Error {
    let kind = match class {
        ReadFailure::Caller => io::ErrorKind::InvalidInput,
        ReadFailure::Resource => io::ErrorKind::OutOfMemory,
        ReadFailure::DeviceLost => io::ErrorKind::NotConnected,
        ReadFailure::Media | ReadFailure::Cancelled | ReadFailure::Unknown => io::ErrorKind::Other,
    };
    io::Error::new(
        kind,
        ClassifiedError {
            class,
            message: message.into(),
        },
    )
}

/// Sorts the error of a failed [`BlockSource::read_at`] into a class.
///
/// Errors made with [`read_error`] and the [`OutOfRange`] error of a
/// [`RangedSource`](crate::RangedSource) carry their class; operating-system
/// errors are looked up by code ([`classify_windows_error`] on Windows, the
/// `errno` table on Unix); the rest goes by `io::ErrorKind`.
pub fn classify_read_error(error: &io::Error) -> ReadFailure {
    if let Some(inner) = error.get_ref() {
        if let Some(classified) = inner.downcast_ref::<ClassifiedError>() {
            return classified.class;
        }
        if inner.downcast_ref::<OutOfRange>().is_some() {
            return ReadFailure::Caller;
        }
    }
    if let Some(code) = error.raw_os_error() {
        return classify_os_error(code);
    }
    match error.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidInput => ReadFailure::Caller,
        io::ErrorKind::OutOfMemory => ReadFailure::Resource,
        _ => ReadFailure::Unknown,
    }
}

#[cfg(windows)]
fn classify_os_error(code: i32) -> ReadFailure {
    classify_windows_error(code)
}

#[cfg(unix)]
fn classify_os_error(code: i32) -> ReadFailure {
    classify_unix_error(code)
}

#[cfg(not(any(windows, unix)))]
fn classify_os_error(_code: i32) -> ReadFailure {
    ReadFailure::Unknown
}

/// The class of a Win32 error code returned by a read from a disk device
/// (`GetLastError` / `io::Error::raw_os_error`). The table is the one of
/// `docs/notes/windows-raw-io.md`, section 6.
pub fn classify_windows_error(code: i32) -> ReadFailure {
    use ReadFailure::*;
    match code {
        // STATUS_DEVICE_DATA_ERROR / STATUS_CRC_ERROR: SCSI medium error, "bad block".
        23 => Media, // ERROR_CRC
        // STATUS_IO_DEVICE_ERROR, STATUS_DATA_OVERRUN: hardware error, aborted command.
        1117 => Media, // ERROR_IO_DEVICE
        // STATUS_IO_TIMEOUT: the drive is busy with its own error recovery.
        121 => Media,         // ERROR_SEM_TIMEOUT
        483 => Media,         // ERROR_DEVICE_HARDWARE_ERROR
        1126..=1128 => Media, // ERROR_DISK_RECALIBRATE_FAILED / _OPERATION_FAILED / _RESET_FAILED
        25 | 30 => Media,     // ERROR_SEEK, ERROR_READ_FAULT
        // STATUS_NONEXISTENT_SECTOR. Nothing at or beyond size() is ever requested, so
        // inside the device this is an unreadable sector.
        27 => Media,   // ERROR_SECTOR_NOT_FOUND
        1111 => Media, // ERROR_BUS_RESET (transient: retried once before it counts)

        // STATUS_DEVICE_NOT_READY, _NO_MEDIA_IN_DEVICE, _DEVICE_POWER_FAILURE, _DEVICE_OFF_LINE.
        21 => DeviceLost,   // ERROR_NOT_READY
        1167 => DeviceLost, // ERROR_DEVICE_NOT_CONNECTED
        433 => DeviceLost,  // ERROR_NO_SUCH_DEVICE
        2 => DeviceLost,    // ERROR_FILE_NOT_FOUND (STATUS_NO_SUCH_DEVICE on older Windows)
        55 => DeviceLost,   // ERROR_DEV_NOT_EXIST
        1617 => DeviceLost, // ERROR_DEVICE_REMOVED
        1110 => DeviceLost, // ERROR_MEDIA_CHANGED
        1112 => DeviceLost, // ERROR_NO_MEDIA_IN_DRIVE
        321 => DeviceLost,  // ERROR_DEVICE_UNREACHABLE
        31 => DeviceLost,   // ERROR_GEN_FAILURE (a dying bridge)
        20 => DeviceLost,   // ERROR_BAD_UNIT
        6 => DeviceLost,    // ERROR_INVALID_HANDLE
        5 => DeviceLost,    // ERROR_ACCESS_DENIED

        87 => Caller, // ERROR_INVALID_PARAMETER: offset or length not a sector multiple
        1 | 50 => Caller, // ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED
        38 => Caller, // ERROR_HANDLE_EOF: read at or beyond the end
        998 | 1784 => Caller, // ERROR_NOACCESS, ERROR_INVALID_USER_BUFFER

        // ERROR_NO_SYSTEM_RESOURCES, _WORKING_SET_QUOTA, _NOT_ENOUGH_MEMORY, _NOT_ENOUGH_QUOTA
        1450 | 1453 | 8 | 1816 => Resource,
        995 => Cancelled, // ERROR_OPERATION_ABORTED
        _ => Unknown,
    }
}

/// The class of a Unix `errno` returned by a read from an image file.
#[cfg(unix)]
pub fn classify_unix_error(errno: i32) -> ReadFailure {
    use ReadFailure::*;
    #[cfg(target_os = "linux")]
    if errno == libc::ENOMEDIUM {
        return DeviceLost;
    }
    match errno {
        libc::EIO => Media,
        libc::ENXIO | libc::ENODEV | libc::EBADF | libc::ESTALE | libc::ENOTCONN => DeviceLost,
        libc::EINVAL | libc::EFAULT | libc::ESPIPE | libc::EOVERFLOW => Caller,
        libc::ENOMEM | libc::EAGAIN | libc::ENOBUFS => Resource,
        libc::ECANCELED => Cancelled,
        _ => Unknown,
    }
}

/// Result of [`read_resilient`] and [`read_unaligned`].
#[derive(Debug)]
#[must_use = "a read can end in four ways; only `Data` means that the buffer may be used"]
pub enum ReadOutcome {
    /// The buffer is filled. Sectors that could not be read are zero-filled
    /// and listed in `bad` as absolute byte ranges, ascending, disjoint and
    /// merged where they touch. An empty list means every byte was read.
    Data {
        /// Unreadable (zero-filled) parts of the requested range.
        bad: Vec<Range<u64>>,
    },
    /// The device stopped answering (S7). The buffer is unspecified and
    /// nothing may be recorded as read or as bad for this request.
    SourceLost(io::Error),
    /// The cancel flag was set, or the request was cancelled by the system.
    /// The buffer is unspecified.
    Cancelled,
    /// The request was wrong or could not be served for a reason that is
    /// neither the medium nor the device: an internal error. The buffer is
    /// unspecified.
    Internal(io::Error),
}

impl ReadOutcome {
    /// True when the whole range was read without any unreadable sector.
    pub fn is_clean(&self) -> bool {
        matches!(self, ReadOutcome::Data { bad } if bad.is_empty())
    }

    /// The bad ranges of a [`Data`](ReadOutcome::Data) outcome; `None` for
    /// every other outcome.
    pub fn bad_ranges(&self) -> Option<&[Range<u64>]> {
        match self {
            ReadOutcome::Data { bad } => Some(bad),
            _ => None,
        }
    }
}

/// Why the retry machinery stopped before the buffer was filled.
enum Stop {
    Lost(io::Error),
    Cancelled,
    Internal(io::Error),
}

impl From<Stop> for ReadOutcome {
    fn from(stop: Stop) -> ReadOutcome {
        match stop {
            Stop::Lost(e) => ReadOutcome::SourceLost(e),
            Stop::Cancelled => ReadOutcome::Cancelled,
            Stop::Internal(e) => ReadOutcome::Internal(e),
        }
    }
}

/// Ascending list of bad ranges; touching ranges are merged as they arrive.
#[derive(Default)]
struct BadList(Vec<Range<u64>>);

impl BadList {
    fn push(&mut self, range: Range<u64>) {
        if range.start >= range.end {
            return;
        }
        match self.0.last_mut() {
            Some(last) if last.end == range.start => last.end = range.end,
            _ => self.0.push(range),
        }
    }
}

fn is_cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

/// Win32 codes of the *device lost* class that a disk which is still
/// present can also answer for a single bad sector: some USB-SATA bridges
/// and UAS stacks complete a medium error as `STATUS_UNSUCCESSFUL`
/// (`ERROR_GEN_FAILURE`), and `ERROR_NOT_READY`, `ERROR_BAD_UNIT` and
/// `ERROR_ACCESS_DENIED` are seen the same way. They end the job only when
/// [`BlockSource::is_alive`] fails; on a device that answers they count as
/// media errors (and are proven again before a sector is recorded as bad).
/// Codes that no present device gives (`ERROR_DEVICE_NOT_CONNECTED`,
/// `ERROR_DEVICE_REMOVED`, `ERROR_INVALID_HANDLE`, ...) end it at once.
pub fn windows_loss_needs_proof(code: i32) -> bool {
    matches!(code, 31 | 21 | 20 | 5)
}

/// Whether `error`, of the *device lost* class, is one of the ambiguous
/// codes of [`windows_loss_needs_proof`]. Only Windows reports them.
fn loss_needs_proof(error: &io::Error) -> bool {
    cfg!(windows) && error.raw_os_error().is_some_and(windows_loss_needs_proof)
}

/// Gives the error back when smaller reads may still succeed; turns it into
/// a [`Stop`] when retrying is pointless.
fn retryable(src: &dyn BlockSource, error: io::Error) -> Result<io::Error, Stop> {
    match classify_read_error(&error) {
        // A code a present device may give for one bad sector: lost only if the
        // device does not answer; otherwise worked around like a media error.
        ReadFailure::DeviceLost if loss_needs_proof(&error) && src.is_alive() => Ok(error),
        ReadFailure::DeviceLost => Err(Stop::Lost(error)),
        ReadFailure::Caller => Err(Stop::Internal(error)),
        ReadFailure::Cancelled => Err(Stop::Cancelled),
        ReadFailure::Media | ReadFailure::Unknown | ReadFailure::Resource => Ok(error),
    }
}

/// `ERROR_BUS_RESET` is transient: the same request is tried once more
/// before it counts as a failure.
fn is_transient(error: &io::Error) -> bool {
    cfg!(windows) && error.raw_os_error() == Some(1111)
}

/// The first multiple of `step` above `pos`.
fn next_boundary(pos: u64, step: u64) -> u64 {
    (pos / step).saturating_add(1).saturating_mul(step)
}

/// The largest multiple of `sector` that is at most `limit`, but at least one sector.
fn sector_multiple(limit: u64, sector: u64) -> u64 {
    (limit / sector).max(1) * sector
}

/// Reads `buf.len()` bytes at `offset`, working around unreadable sectors.
///
/// `offset` and `buf.len()` must be multiples of the logical sector size and
/// the request must lie inside the source, as for [`BlockSource::read_at`].
/// Pass memory from an [`AlignedBuf`] (use [`read_unaligned`] for anything
/// else).
///
/// 1. The whole request is read at once. Scans call this with 8 MiB chunks.
/// 2. If that fails, the range is read again in blocks of 64 KiB
///    ([`RETRY_BLOCK_SIZE`], on 64 KiB boundaries of the source).
/// 3. A block that fails is read one physical sector at a time. A sector that
///    fails with a media-class error is zero-filled and reported as bad, but
///    only after [`BlockSource::is_alive`] has confirmed the device; if the
///    device does not answer, the outcome is [`ReadOutcome::SourceLost`].
///    After [`MAX_CONSECUTIVE_BAD`] bad sectors in a row the rest of the block
///    is reported bad without being tried.
///
/// A *device lost* error ends the call with `SourceLost` at once, a *caller*
/// error with `Internal`. A *resource* error is retried with smaller
/// transfers and, if even a single sector cannot be transferred, ends with
/// `Internal`. `cancel` is polled before every retry (a failing sector can
/// take the drive many seconds), never in the middle of a successful request.
pub fn read_resilient(
    src: &dyn BlockSource,
    offset: u64,
    buf: &mut [u8],
    cancel: &AtomicBool,
) -> ReadOutcome {
    if let Err(e) = check_read_request(src.size(), src.sector_size(), offset, buf.len()) {
        return ReadOutcome::Internal(e);
    }
    if buf.is_empty() {
        return ReadOutcome::Data { bad: Vec::new() };
    }
    if is_cancelled(cancel) {
        return ReadOutcome::Cancelled;
    }
    let first_error = match src.read_at(offset, buf) {
        Ok(()) => return ReadOutcome::Data { bad: Vec::new() },
        Err(e) => match retryable(src, e) {
            Ok(e) => e,
            Err(stop) => return stop.into(),
        },
    };
    match read_in_blocks(src, offset, buf, cancel, first_error) {
        Ok(bad) => {
            // One more look at the device: errors that announce a disconnect
            // (1117, 121) are indistinguishable from bad sectors while it happens.
            if !bad.is_empty() && !src.is_alive() {
                return ReadOutcome::SourceLost(read_error(
                    ReadFailure::DeviceLost,
                    "the source stopped answering while unreadable sectors were being worked around",
                ));
            }
            ReadOutcome::Data { bad }
        }
        Err(stop) => stop.into(),
    }
}

/// Stage 2: 64 KiB blocks. `first_error` is the failure of the read of the
/// whole range.
fn read_in_blocks(
    src: &dyn BlockSource,
    offset: u64,
    buf: &mut [u8],
    cancel: &AtomicBool,
    first_error: io::Error,
) -> Result<Vec<Range<u64>>, Stop> {
    let logical = u64::from(src.sector_size().max(1));
    let block_step = sector_multiple(RETRY_BLOCK_SIZE, logical);
    let physical = u64::from(src.physical_sector_size());
    // Retry unit: the physical sector (a damaged one takes all its logical sectors
    // with it), as long as that is a sane multiple of the logical size.
    let unit = if physical > logical && physical % logical == 0 && physical <= block_step {
        physical
    } else {
        logical
    };
    // No overflow: check_read_request has verified offset + len <= size.
    let end = offset + buf.len() as u64;
    let mut bad = BadList::default();
    // When the request is a single block, reading "the block" again would repeat
    // the request that has just failed.
    let mut known = (next_boundary(offset, block_step) >= end).then_some(first_error);
    let mut pos = offset;
    while pos < end {
        let block_end = next_boundary(pos, block_step).min(end);
        let block = &mut buf[(pos - offset) as usize..(block_end - offset) as usize];
        read_block(src, pos, block, unit, cancel, &mut bad, known.take())?;
        pos = block_end;
    }
    Ok(bad.0)
}

/// Stage 2 and 3 for one block. `known` is the error of a read of exactly
/// this block, if one has already failed.
fn read_block(
    src: &dyn BlockSource,
    pos: u64,
    block: &mut [u8],
    unit: u64,
    cancel: &AtomicBool,
    bad: &mut BadList,
    known: Option<io::Error>,
) -> Result<(), Stop> {
    let block_error = match known {
        Some(e) => e,
        None => {
            if is_cancelled(cancel) {
                return Err(Stop::Cancelled);
            }
            match src.read_at(pos, block) {
                Ok(()) => return Ok(()),
                Err(e) => retryable(src, e)?,
            }
        }
    };
    let end = pos + block.len() as u64;
    // A block that is a single retry unit has already been tried as such.
    let mut known_unit = (next_boundary(pos, unit) >= end).then_some(block_error);
    let mut consecutive_bad = 0u32;
    let mut cur = pos;
    while cur < end {
        let unit_end = next_boundary(cur, unit).min(end);
        let slice = &mut block[(cur - pos) as usize..(unit_end - pos) as usize];
        match read_unit(src, cur, slice, cancel, known_unit.take())? {
            Ok(()) => consecutive_bad = 0,
            Err(media_error) => {
                // Nothing is declared bad on the word of a device that has gone away.
                if !src.is_alive() {
                    return Err(Stop::Lost(media_error));
                }
                slice.fill(0);
                bad.push(cur..unit_end);
                consecutive_bad += 1;
                if consecutive_bad >= MAX_CONSECUTIVE_BAD && unit_end < end {
                    block[(unit_end - pos) as usize..].fill(0);
                    bad.push(unit_end..end);
                    return Ok(());
                }
            }
        }
        cur = unit_end;
    }
    Ok(())
}

/// Stage 3 for one retry unit. `Ok(Err(e))` means: unreadable for a
/// media-class reason (the caller still has to confirm the device).
fn read_unit(
    src: &dyn BlockSource,
    pos: u64,
    slice: &mut [u8],
    cancel: &AtomicBool,
    known: Option<io::Error>,
) -> Result<Result<(), io::Error>, Stop> {
    let mut attempts = 0u32;
    let mut transient_retried = false;
    let mut pending = known;
    loop {
        let error = match pending.take() {
            Some(e) => e,
            None => {
                if is_cancelled(cancel) {
                    return Err(Stop::Cancelled);
                }
                match src.read_at(pos, slice) {
                    Ok(()) => return Ok(Ok(())),
                    Err(e) => e,
                }
            }
        };
        attempts += 1;
        let error = retryable(src, error)?;
        match classify_read_error(&error) {
            ReadFailure::Resource => {
                // Even one sector cannot be transferred: not the medium's fault.
                if attempts >= RESOURCE_ATTEMPTS {
                    return Err(Stop::Internal(error));
                }
                std::thread::sleep(Duration::from_millis(20 * u64::from(attempts)));
            }
            _ => {
                if is_transient(&error) && !transient_retried {
                    transient_retried = true;
                } else {
                    return Ok(Err(error));
                }
            }
        }
    }
}

/// Reads an arbitrary byte range: `offset` and `buf.len()` need not be
/// sector-aligned and `buf` need not be aligned memory. The range must lie
/// inside the source.
///
/// The sectors that cover the range are read with [`read_resilient`] through
/// an [`AlignedBuf`] in pieces of at most 1 MiB and copied out. The `bad`
/// ranges of a [`ReadOutcome::Data`] outcome are the unreadable sectors
/// clipped to the requested range, so their bytes are exactly the zero-filled
/// bytes of `buf`.
pub fn read_unaligned(
    src: &dyn BlockSource,
    offset: u64,
    buf: &mut [u8],
    cancel: &AtomicBool,
) -> ReadOutcome {
    let len = buf.len() as u64;
    let end = match offset.checked_add(len) {
        Some(end) if end <= src.size() => end,
        _ => {
            return ReadOutcome::Internal(read_error(
                ReadFailure::Caller,
                format!(
                    "read of {len} bytes at offset {offset} ends beyond the source ({} bytes)",
                    src.size()
                ),
            ))
        }
    };
    if buf.is_empty() {
        return ReadOutcome::Data { bad: Vec::new() };
    }
    let sector = u64::from(src.sector_size().max(1));
    if offset.is_multiple_of(sector)
        && len.is_multiple_of(sector)
        && (buf.as_ptr() as usize as u64).is_multiple_of(sector)
    {
        return read_resilient(src, offset, buf, cancel);
    }
    let aligned_start = offset - offset % sector;
    let Some(aligned_end) = end.div_ceil(sector).checked_mul(sector) else {
        return ReadOutcome::Internal(read_error(
            ReadFailure::Caller,
            "read range overflows the offset space",
        ));
    };
    let piece_len = (aligned_end - aligned_start).min(sector_multiple(BOUNCE_SIZE, sector));
    let mut bounce = AlignedBuf::new(piece_len as usize);
    let mut bad = BadList::default();
    let mut pos = aligned_start;
    while pos < aligned_end {
        let n = (aligned_end - pos).min(piece_len);
        let piece = &mut bounce[..n as usize];
        match read_resilient(src, pos, piece, cancel) {
            ReadOutcome::Data { bad: piece_bad } => {
                for r in piece_bad {
                    bad.push(r.start.max(offset)..r.end.min(end));
                }
                let from = pos.max(offset);
                let to = (pos + n).min(end);
                buf[(from - offset) as usize..(to - offset) as usize]
                    .copy_from_slice(&piece[(from - pos) as usize..(to - pos) as usize]);
            }
            other => return other,
        }
        pos += n;
    }
    ReadOutcome::Data { bad: bad.0 }
}

#[cfg(test)]
mod tests;
