//! A time limit for `read_file`.
//!
//! `read_file` reads on a thread of its own, but its response has to come
//! before the responses of every later request (PROTOCOL section 1), so
//! nothing else is answered while it reads; only `shutdown`, the end of
//! input and `job_cancel` of a running job stop it early (they set the
//! same flag). On a failing disk every unreadable sector can take seconds,
//! and the content reader retries per physical sector, so one preview
//! chunk could hold the engine for minutes. [`Deadline`] sets the read's
//! cancel flag once the limit has passed; the reader polls it between
//! reads and stops, and `read_file` answers `io_error`.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hds_io::{BlockSource, SourceInfo, SourceLocation};

/// How long one `read_file` may read.
pub const READ_FILE_DEADLINE: Duration = Duration::from_secs(5);

/// A source that sets `stop` once `limit` has passed since it was made.
pub struct Deadline {
    inner: Arc<dyn BlockSource>,
    end: Instant,
    stop: Arc<AtomicBool>,
}

impl Deadline {
    /// Wraps `inner`; `stop` is the cancel flag of the read.
    pub fn new(inner: Arc<dyn BlockSource>, limit: Duration, stop: Arc<AtomicBool>) -> Deadline {
        Deadline {
            inner,
            end: Instant::now() + limit,
            stop,
        }
    }

    fn check(&self) {
        if Instant::now() >= self.end {
            self.stop.store(true, Ordering::Relaxed);
        }
    }
}

impl BlockSource for Deadline {
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
        self.check();
        let result = self.inner.read_at(offset, buf);
        self.check();
        result
    }
    fn info(&self) -> SourceInfo {
        self.inner.info()
    }
    fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }
    fn location(&self) -> SourceLocation {
        self.inner.location()
    }
}
