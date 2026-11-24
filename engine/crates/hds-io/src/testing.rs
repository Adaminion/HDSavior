//! Sources for tests: of this crate and of the crates built on it.
//!
//! * [`MemSource`]: a source backed by a byte vector.
//! * [`FaultySource`]: a wrapper that injects read errors on chosen ranges,
//!   loses the device after a number of reads, limits the transfer size, adds
//!   delays, can raise a cancel flag at a chosen moment and records every
//!   read it is asked for.
//! * [`XorShift`]: a small deterministic random number generator, so that
//!   randomized tests need no extra dependency and are reproducible.
//!
//! ```
//! use std::sync::atomic::AtomicBool;
//! use std::sync::Arc;
//! use hds_io::testing::{FaultySource, MemSource};
//! use hds_io::{read_resilient, AlignedBuf, ReadOutcome};
//!
//! let disk = FaultySource::new(Arc::new(MemSource::new(vec![7u8; 1 << 20])));
//! disk.bad_sectors(8192..8704);
//! let mut buf = AlignedBuf::new(1 << 20);
//! match read_resilient(&disk, 0, &mut buf, &AtomicBool::new(false)) {
//!     ReadOutcome::Data { bad } => assert_eq!(bad, vec![8192..8704]),
//!     other => panic!("{other:?}"),
//! }
//! assert!(buf[8192..8704].iter().all(|&b| b == 0));
//! assert!(buf[8704..].iter().all(|&b| b == 7));
//! ```

use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::rangeset::RangeSet;
use crate::resilient::{read_error, ReadFailure};
use crate::source::{check_read_request, BlockSource, SourceInfo, SourceKind, SourceLocation};

/// Deterministic pseudo-random numbers (xorshift64*). Not for anything but tests.
#[derive(Debug, Clone)]
pub struct XorShift(u64);

impl XorShift {
    /// A generator whose sequence depends only on `seed`.
    pub fn new(seed: u64) -> XorShift {
        // The state must not be zero; mix the seed so that small seeds differ at once.
        XorShift((seed ^ 0x9E37_79B9_7F4A_7C15).wrapping_mul(0xBF58_476D_1CE4_E5B9) | 1)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number in `0..n` (0 when `n` is 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    /// Fills `buf` with random bytes.
    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

/// A [`BlockSource`] that lives in memory.
///
/// Reports itself as an image named `mem:<name>` and, for the destination
/// guard, as [`SourceLocation::Detached`] unless told otherwise with
/// [`at_location`](MemSource::at_location).
#[derive(Debug, Clone)]
pub struct MemSource {
    data: Vec<u8>,
    sector_size: u32,
    physical_sector_size: u32,
    name: String,
    location: SourceLocation,
}

impl MemSource {
    /// A source with 512-byte logical and physical sectors. `data` is padded
    /// with zeros to a whole number of sectors.
    pub fn new(data: Vec<u8>) -> MemSource {
        MemSource::with_sector_sizes(data, 512, 512)
    }

    /// A source with the given sector sizes (for example 512 / 4096 for a
    /// "512e" disk or 4096 / 4096 for a "4Kn" disk). `data` is padded with
    /// zeros to a whole number of logical sectors. A logical size of 0 is
    /// taken as 512 and a physical size below the logical one as the logical
    /// one.
    pub fn with_sector_sizes(mut data: Vec<u8>, logical: u32, physical: u32) -> MemSource {
        let logical = if logical == 0 { 512 } else { logical };
        let rest = data.len() % logical as usize;
        if rest != 0 {
            data.resize(data.len() + (logical as usize - rest), 0);
        }
        MemSource {
            data,
            sector_size: logical,
            physical_sector_size: physical.max(logical),
            name: "test".to_string(),
            location: SourceLocation::Detached,
        }
    }

    /// Sets the name that appears in [`SourceInfo::path`] as `mem:<name>`.
    pub fn named(mut self, name: &str) -> MemSource {
        self.name = name.to_string();
        self
    }

    /// Makes the source claim to live at `location`, to exercise the
    /// destination guard.
    pub fn at_location(mut self, location: SourceLocation) -> MemSource {
        self.location = location;
        self
    }

    /// The bytes of the source.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

impl BlockSource for MemSource {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn physical_sector_size(&self) -> u32 {
        self.physical_sector_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_read_request(self.size(), self.sector_size, offset, buf.len())?;
        let start = offset as usize;
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }

    fn info(&self) -> SourceInfo {
        let disk_number = match self.location {
            SourceLocation::Disk { number } => Some(number),
            _ => None,
        };
        SourceInfo {
            kind: if disk_number.is_some() {
                SourceKind::Disk
            } else {
                SourceKind::Image
            },
            path: format!("mem:{}", self.name),
            size: self.size(),
            sector_size: self.sector_size,
            physical_sector_size: self.physical_sector_size,
            model: None,
            serial: None,
            disk_number,
        }
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn location(&self) -> SourceLocation {
        self.location
    }
}

/// How many reads [`FaultySource::read_log`] remembers.
const LOG_LIMIT: usize = 100_000;

#[derive(Default)]
struct FaultState {
    faults: Vec<(Range<u64>, ReadFailure)>,
    lose_after: Option<u64>,
    lose_after_checks: Option<u64>,
    lost: bool,
    silent_loss: bool,
    max_transfer: Option<usize>,
    delay: Duration,
    cancel_after: Option<(u64, Arc<AtomicBool>)>,
    reads: u64,
    liveness_checks: u64,
    touched: RangeSet,
    log: Vec<(u64, usize)>,
}

/// A [`BlockSource`] wrapper that misbehaves on request.
///
/// All settings can be changed at any time through `&self`, also while other
/// threads read. A read that fails leaves a recognisable garbage pattern
/// (`0xEE`) in the buffer, so that a consumer that trusts a failed buffer is
/// caught by its tests.
pub struct FaultySource {
    inner: Arc<dyn BlockSource>,
    state: Mutex<FaultState>,
}

impl FaultySource {
    /// Wraps `inner` without any fault.
    pub fn new(inner: Arc<dyn BlockSource>) -> FaultySource {
        FaultySource {
            inner,
            state: Mutex::new(FaultState::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, FaultState> {
        // A panicking test thread must not hide the state from the others.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Every read that touches `range` fails with a media error, as a disk
    /// does when one sector of the request is unreadable.
    pub fn bad_sectors(&self, range: Range<u64>) {
        self.fail_range(range, ReadFailure::Media);
    }

    /// Every read that touches `range` fails with an error of `class`.
    /// Ranges added earlier win where ranges overlap.
    pub fn fail_range(&self, range: Range<u64>, class: ReadFailure) {
        if range.start < range.end {
            self.state().faults.push((range, class));
        }
    }

    /// Removes all ranges added with [`bad_sectors`](Self::bad_sectors) and
    /// [`fail_range`](Self::fail_range).
    pub fn clear_faults(&self) {
        self.state().faults.clear();
    }

    /// The next `reads` reads behave normally (failed ones count too). As
    /// soon as the last of them has been served the device is gone:
    /// [`is_alive`](BlockSource::is_alive) is false and every further read
    /// fails with a *device lost* error. `0` loses the device at once.
    pub fn lose_after_reads(&self, reads: u64) {
        self.schedule_loss(reads, false);
    }

    /// Like [`lose_after_reads`](Self::lose_after_reads), but the reads of
    /// the lost device fail with *media* errors, as happens while a device
    /// drops off the bus: only `is_alive` tells the truth.
    pub fn lose_silently_after_reads(&self, reads: u64) {
        self.schedule_loss(reads, true);
    }

    fn schedule_loss(&self, reads: u64, silent: bool) {
        let mut st = self.state();
        st.silent_loss = silent;
        if reads == 0 {
            st.lost = true;
            st.lose_after = None;
        } else {
            st.lose_after = Some(st.reads.saturating_add(reads));
        }
    }

    /// The next `checks` calls of [`is_alive`](BlockSource::is_alive) answer
    /// normally; after the last of them the device is gone, as if unplugged
    /// between two steps of a retry. Reads then fail with *device lost* errors.
    pub fn lose_after_liveness_checks(&self, checks: u64) {
        let mut st = self.state();
        st.silent_loss = false;
        if checks == 0 {
            st.lost = true;
            st.lose_after_checks = None;
        } else {
            st.lose_after_checks = Some(st.liveness_checks.saturating_add(checks));
        }
    }

    /// Brings a lost device back and cancels a pending loss.
    pub fn revive(&self) {
        let mut st = self.state();
        st.lost = false;
        st.lose_after = None;
        st.lose_after_checks = None;
    }

    /// Reads longer than `bytes` fail with a *resource* error (`None`: no limit).
    pub fn set_max_transfer(&self, bytes: Option<usize>) {
        self.state().max_transfer = bytes;
    }

    /// Every read sleeps this long before it does anything else.
    pub fn set_delay(&self, delay: Duration) {
        self.state().delay = delay;
    }

    /// Sets `flag` when the read number `reads` from now begins (1 = the next
    /// read), to test cancellation in the middle of a job.
    pub fn cancel_after_reads(&self, reads: u64, flag: Arc<AtomicBool>) {
        let mut st = self.state();
        st.cancel_after = Some((st.reads.saturating_add(reads), flag));
    }

    /// Number of reads requested so far, failed ones included.
    pub fn read_count(&self) -> u64 {
        self.state().reads
    }

    /// Number of [`is_alive`](BlockSource::is_alive) calls so far.
    pub fn liveness_checks(&self) -> u64 {
        self.state().liveness_checks
    }

    /// Every byte range a read was requested for, failed reads included.
    pub fn touched(&self) -> RangeSet {
        self.state().touched.clone()
    }

    /// `(offset, length)` of the reads requested so far, in order (the first
    /// 100,000).
    pub fn read_log(&self) -> Vec<(u64, usize)> {
        self.state().log.clone()
    }

    /// Forgets the counters, the touched ranges and the log, and drops a
    /// scheduled loss or cancellation; faults and a lost device stay as they are.
    pub fn reset_counters(&self) {
        let mut st = self.state();
        st.reads = 0;
        st.liveness_checks = 0;
        st.touched.clear();
        st.log.clear();
        st.lose_after = None;
        st.lose_after_checks = None;
        st.cancel_after = None;
    }
}

fn fail(buf: &mut [u8], class: ReadFailure, what: &str) -> io::Result<()> {
    buf.fill(0xEE);
    Err(read_error(class, format!("injected fault: {what}")))
}

impl BlockSource for FaultySource {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn physical_sector_size(&self) -> u32 {
        self.inner.physical_sector_size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let len = buf.len();
        let end = offset.saturating_add(len as u64);
        let (delay, verdict) = {
            let mut st = self.state();
            st.reads += 1;
            st.touched.insert(offset..end);
            if st.log.len() < LOG_LIMIT {
                st.log.push((offset, len));
            }
            if let Some((at, flag)) = &st.cancel_after {
                if st.reads >= *at {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            // A scheduled loss takes effect once this read has been served.
            let lost = st.lost;
            if st.lose_after.is_some_and(|limit| st.reads >= limit) {
                st.lost = true;
                st.lose_after = None;
            }
            let verdict = if lost {
                Some(if st.silent_loss {
                    ReadFailure::Media
                } else {
                    ReadFailure::DeviceLost
                })
            } else if st.max_transfer.is_some_and(|max| len > max) {
                Some(ReadFailure::Resource)
            } else {
                st.faults
                    .iter()
                    .find(|(r, _)| r.start < end && offset < r.end)
                    .map(|(_, class)| *class)
            };
            (st.delay, verdict)
        };
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        // A request that breaks the contract is the caller's bug whatever else is injected.
        check_read_request(self.inner.size(), self.inner.sector_size(), offset, len)?;
        match verdict {
            Some(ReadFailure::DeviceLost) => {
                fail(buf, ReadFailure::DeviceLost, "the device is gone")
            }
            Some(ReadFailure::Resource) => fail(buf, ReadFailure::Resource, "transfer too large"),
            Some(class) => fail(buf, class, "unreadable range"),
            None => self.inner.read_at(offset, buf),
        }
    }

    fn info(&self) -> SourceInfo {
        self.inner.info()
    }

    fn is_alive(&self) -> bool {
        let mut st = self.state();
        st.liveness_checks += 1;
        let alive = !st.lost && self.inner.is_alive();
        if st
            .lose_after_checks
            .is_some_and(|limit| st.liveness_checks >= limit)
        {
            st.lost = true;
            st.lose_after_checks = None;
        }
        alive
    }

    fn location(&self) -> SourceLocation {
        self.inner.location()
    }
}

#[cfg(test)]
mod tests;
