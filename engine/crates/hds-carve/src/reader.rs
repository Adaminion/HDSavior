//! Positional reads: the public [`ReadAt`] trait and the budgeted, buffered
//! view of it ([`Src`]) that every format walker reads through.

use std::io;

use crate::Limits;

/// Positional, read-only access to the bytes of a candidate file.
///
/// Offsets are relative to the candidate's first byte (the position where
/// [`sniff`](crate::sniff) matched), not to the disk.
///
/// A read returns the number of bytes copied into `buf`. It may be shorter
/// than `buf` only when the data ends there; a read that starts at or past the
/// end returns `Ok(0)`. An `Err` is an I/O error: [`measure`](crate::measure)
/// then gives up on the candidate (it never guesses across unreadable data).
pub trait ReadAt {
    /// Reads up to `buf.len()` bytes starting at `offset`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
}

impl ReadAt for [u8] {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        let Some(avail) = self.get(start..) else {
            return Ok(0);
        };
        let n = avail.len().min(buf.len());
        buf[..n].copy_from_slice(&avail[..n]);
        Ok(n)
    }
}

impl ReadAt for Vec<u8> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.as_slice().read_at(offset, buf)
    }
}

impl<const N: usize> ReadAt for [u8; N] {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.as_slice().read_at(offset, buf)
    }
}

/// References to readers are readers. This is what lets a plain byte slice be
/// passed where a `&dyn ReadAt` is expected: `measure(kind, &slice, len)` with
/// `slice: &[u8]` coerces `&&[u8]` to `&dyn ReadAt`.
impl<T: ReadAt + ?Sized> ReadAt for &T {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        (**self).read_at(offset, buf)
    }
}

impl<T: ReadAt + ?Sized> ReadAt for Box<T> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        (**self).read_at(offset, buf)
    }
}

/// Largest single contiguous range a walker may ask for in one piece.
const MAX_REQUEST: usize = 4 * 1024 * 1024;

/// How a [`Src`] sizes and places its reads.
#[derive(Clone, Copy)]
pub(crate) struct Tuning {
    /// Refills start on a multiple of this, so that a reader backed by a raw
    /// device (whose candidates start on cluster boundaries) is asked for
    /// whole, aligned blocks.
    align: u64,
    /// Refill size while a walker hops from header to header.
    hop_block: usize,
    /// First refill size while a walker has to look at every byte. Most
    /// candidates are small or turn out not to be files at all: reading a
    /// megabyte for each would double the time a hard disk spends on them.
    scan_first: usize,
    /// Refill size a sequential scan grows to, doubling with every refill.
    scan_max: usize,
    /// Shifts the refill grid; only tests use anything but 0.
    phase: u64,
}

impl Tuning {
    pub(crate) const DEFAULT: Tuning = Tuning {
        align: 4096,
        hop_block: 16 * 1024,
        scan_first: 64 * 1024,
        scan_max: 1024 * 1024,
        phase: 0,
    };

    /// Tiny blocks on a shifted grid: with `block` bytes per refill and
    /// `phase` running over `0..block`, every byte position of a file is
    /// the first byte of a refill once.
    #[cfg(test)]
    pub(crate) fn tiny(block: usize, phase: u64) -> Tuning {
        Tuning {
            align: block as u64,
            hop_block: block,
            scan_first: block,
            scan_max: block,
            phase,
        }
    }
}

/// A window onto a [`ReadAt`] that
///
/// * never shows anything at or beyond `limit` (the caller's `max_len`),
/// * buffers, so that neighbouring headers cost one read and sequential scans
///   run in large blocks,
/// * charges every read against a [`Limits`] budget and goes permanently
///   dead (all reads fail) when the budget is exhausted or the underlying
///   reader reports an error.
pub(crate) struct Src<'a> {
    inner: &'a dyn ReadAt,
    limit: u64,
    /// Smallest offset known to be past the end of the data (a short read was
    /// seen there); `limit` until then.
    end: u64,
    buf: Vec<u8>,
    /// Offset of `buf[0]`.
    start: u64,
    tuning: Tuning,
    /// Size of the next refill.
    block: usize,
    /// Sequential scan (as opposed to hopping between headers).
    scanning: bool,
    bytes_left: u64,
    reads_left: u64,
    failure: Option<Failure>,
}

/// Why a source went dead.
pub(crate) enum Failure {
    /// A [`Limits`] budget was exhausted.
    Limit,
    /// The underlying reader reported an error.
    Io(io::Error),
}

impl<'a> Src<'a> {
    pub(crate) fn new(inner: &'a dyn ReadAt, limit: u64, limits: &Limits) -> Self {
        Self::tuned(inner, limit, limits, Tuning::DEFAULT)
    }

    pub(crate) fn tuned(
        inner: &'a dyn ReadAt,
        limit: u64,
        limits: &Limits,
        tuning: Tuning,
    ) -> Self {
        Src {
            inner,
            limit,
            end: limit,
            buf: Vec::new(),
            start: 0,
            tuning,
            block: tuning.hop_block,
            scanning: false,
            bytes_left: limits.max_read_bytes,
            reads_left: limits.max_reads,
            failure: None,
        }
    }

    /// The caller's `max_len`: no measured length may exceed it.
    pub(crate) fn limit(&self) -> u64 {
        self.limit
    }

    /// True once a read failed or the work budget ran out. Results computed
    /// after that point must not be trusted.
    pub(crate) fn failed(&self) -> bool {
        self.failure.is_some()
    }

    /// Takes the reason the source went dead, if it did.
    pub(crate) fn take_failure(&mut self) -> Option<Failure> {
        self.failure.take()
    }

    fn fail(&mut self, cause: Failure) {
        // the first cause is the one that matters
        self.failure.get_or_insert(cause);
    }

    /// Switches to sequential refills that grow as the scan goes on (for
    /// walkers that look at every byte).
    pub(crate) fn scan_mode(&mut self) {
        self.scanning = true;
        self.block = self.tuning.scan_first;
    }

    /// Switches back to small refills (for walkers that hop between headers).
    pub(crate) fn hop_mode(&mut self) {
        self.scanning = false;
        self.block = self.tuning.hop_block;
    }

    /// One budgeted read. Fails when the source reports an I/O error or the
    /// budget is exhausted; does not mark the source dead (callers decide).
    fn try_read(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, Failure> {
        if self.failed() || self.reads_left == 0 || (out.len() as u64) > self.bytes_left {
            return Err(Failure::Limit);
        }
        self.reads_left -= 1;
        let mut got = 0usize;
        while got < out.len() {
            let Some(rest) = out.get_mut(got..) else {
                break;
            };
            match self.inner.read_at(offset.saturating_add(got as u64), rest) {
                Ok(0) => break,
                // A reader that claims more than it was asked for is broken;
                // never trust the count beyond the buffer.
                Ok(n) => got = got.saturating_add(n).min(out.len()),
                Err(e) => return Err(Failure::Io(e)),
            }
        }
        self.bytes_left = self.bytes_left.saturating_sub(got as u64);
        if got < out.len() {
            self.end = self.end.min(offset.saturating_add(got as u64));
        }
        Ok(got)
    }

    fn read_into(&mut self, offset: u64, out: &mut [u8]) -> usize {
        match self.try_read(offset, out) {
            Ok(n) => n,
            Err(cause) => {
                self.fail(cause);
                0
            }
        }
    }

    /// Buffered bytes starting at `off`: at least `want` of them unless the
    /// data ends earlier (then as many as exist, possibly none).
    fn fill(&mut self, off: u64, want: usize) -> &[u8] {
        let want = want.clamp(1, MAX_REQUEST);
        let buffered = self.buf.len() as u64;
        if off >= self.start && off - self.start < buffered {
            let rel = (off - self.start) as usize;
            let have = self.buf.len() - rel;
            // Enough is buffered, or the buffer already reaches the end of
            // the data: nothing more to fetch.
            if have >= want || self.start.saturating_add(buffered) >= self.end {
                return self.buf.get(rel..).unwrap_or(&[]);
            }
        }
        if self.failed() || off >= self.end {
            return &[];
        }
        // the refill starts on the grid, at or before `off`
        let align = self.tuning.align.max(1);
        let mut lead = (off.saturating_add(self.tuning.phase) % align).min(off) as usize;
        let mut start = off - lead as u64;
        let need = lead.saturating_add(want);
        let size = (self.block.max(need) as u64)
            .div_ceil(align)
            .saturating_mul(align);
        let size = size.min(self.end - start) as usize;

        let mut buf = std::mem::take(&mut self.buf);
        buf.clear();
        buf.resize(size, 0);
        let got = match self.try_read(start, &mut buf) {
            Ok(n) => n,
            // The read-ahead ran into an unreadable sector. A walker that
            // hops between headers may not need it: ask again for exactly
            // the bytes requested. (A sequential scan needs every byte, and
            // retrying sector by sector through a bad area would take for
            // ever: there the error is final.)
            Err(Failure::Io(_)) if !self.scanning && (lead != 0 || size > want) => {
                let exact = (want as u64).min(self.end - off) as usize;
                buf.clear();
                buf.resize(exact, 0);
                start = off;
                lead = 0;
                match self.try_read(off, &mut buf) {
                    Ok(n) => n,
                    Err(cause) => {
                        self.fail(cause);
                        0
                    }
                }
            }
            Err(cause) => {
                self.fail(cause);
                0
            }
        };
        buf.truncate(got);
        self.buf = buf;
        self.start = start;
        if self.scanning {
            self.block = self.block.saturating_mul(2).min(self.tuning.scan_max);
        }
        self.buf.get(lead..).unwrap_or(&[])
    }

    /// Exactly `len` bytes at `off`, or `None` when they do not all exist
    /// (end of data, `max_len`, I/O error, budget).
    pub(crate) fn bytes(&mut self, off: u64, len: usize) -> Option<&[u8]> {
        if len > MAX_REQUEST {
            return None;
        }
        if len == 0 {
            return Some(&[]);
        }
        self.fill(off, len).get(..len)
    }

    /// Up to `len` bytes at `off` (shorter at the end of the data).
    pub(crate) fn peek(&mut self, off: u64, len: usize) -> &[u8] {
        let s = self.fill(off, len.min(MAX_REQUEST));
        s.get(..len).unwrap_or(s)
    }

    /// Everything buffered from `off` on, refilling when `off` is outside the
    /// buffer. Empty only at the end of the data (or after a failure).
    pub(crate) fn window(&mut self, off: u64) -> &[u8] {
        self.fill(off, 1)
    }

    /// Like [`window`](Self::window), but with at least `min` bytes unless the
    /// data ends earlier. A scanner looking for an `n`-byte pattern advances
    /// by `len - (n - 1)` and asks for `min = n`: a pattern that straddles two
    /// refills is then seen whole.
    pub(crate) fn window_min(&mut self, off: u64, min: usize) -> &[u8] {
        self.fill(off, min)
    }

    /// A fixed-size array at `off`. May move the buffer.
    pub(crate) fn arr<const N: usize>(&mut self, off: u64) -> Option<[u8; N]> {
        let s = self.bytes(off, N)?;
        <[u8; N]>::try_from(s).ok()
    }

    /// A fixed-size array at `off`, read without moving the buffer: for the
    /// occasional look elsewhere in the middle of a sequential scan.
    pub(crate) fn side<const N: usize>(&mut self, off: u64) -> Option<[u8; N]> {
        let mut out = [0u8; N];
        let end = off.checked_add(N as u64)?;
        if end > self.end {
            return None;
        }
        if off >= self.start {
            if let Ok(rel) = usize::try_from(off - self.start) {
                if let Some(s) = rel.checked_add(N).and_then(|e| self.buf.get(rel..e)) {
                    out.copy_from_slice(s);
                    return Some(out);
                }
            }
        }
        (self.read_into(off, &mut out) == N).then_some(out)
    }

    /// Up to `N` bytes at `off` without moving the buffer; returns the array
    /// and how many leading bytes of it are valid.
    pub(crate) fn side_peek<const N: usize>(&mut self, off: u64) -> ([u8; N], usize) {
        let mut out = [0u8; N];
        if off >= self.end {
            return (out, 0);
        }
        let n = (self.end - off).min(N as u64) as usize;
        if off >= self.start {
            if let Ok(rel) = usize::try_from(off - self.start) {
                if let Some(s) = rel.checked_add(n).and_then(|e| self.buf.get(rel..e)) {
                    if let Some(dst) = out.get_mut(..n) {
                        dst.copy_from_slice(s);
                    }
                    return (out, n);
                }
            }
        }
        let got = match out.get_mut(..n) {
            Some(dst) => self.read_into(off, dst),
            None => 0,
        };
        (out, got)
    }

    pub(crate) fn u8(&mut self, off: u64) -> Option<u8> {
        self.arr::<1>(off).map(|a| a[0])
    }

    pub(crate) fn be16(&mut self, off: u64) -> Option<u16> {
        self.arr(off).map(u16::from_be_bytes)
    }

    pub(crate) fn be32(&mut self, off: u64) -> Option<u32> {
        self.arr(off).map(u32::from_be_bytes)
    }

    pub(crate) fn be64(&mut self, off: u64) -> Option<u64> {
        self.arr(off).map(u64::from_be_bytes)
    }

    pub(crate) fn le16(&mut self, off: u64) -> Option<u16> {
        self.arr(off).map(u16::from_le_bytes)
    }

    pub(crate) fn le32(&mut self, off: u64) -> Option<u32> {
        self.arr(off).map(u32::from_le_bytes)
    }

    /// True when the byte at `off` exists (is inside the data and `max_len`).
    pub(crate) fn has_byte(&mut self, off: u64) -> bool {
        self.side::<1>(off).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Counting<'a> {
        data: &'a [u8],
        reads: Cell<u64>,
        bytes: Cell<u64>,
    }

    impl ReadAt for Counting<'_> {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.data.read_at(offset, buf)?;
            self.reads.set(self.reads.get() + 1);
            self.bytes.set(self.bytes.get() + n as u64);
            Ok(n)
        }
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn slice_read_at_edges() {
        let d = [1u8, 2, 3, 4];
        let mut b = [0u8; 8];
        assert_eq!(d.read_at(0, &mut b).unwrap(), 4);
        assert_eq!(d.read_at(3, &mut b).unwrap(), 1);
        assert_eq!(d.read_at(4, &mut b).unwrap(), 0);
        assert_eq!(d.read_at(5, &mut b).unwrap(), 0);
        assert_eq!(d.read_at(u64::MAX, &mut b).unwrap(), 0);
        assert_eq!(d[..].read_at(1, &mut b[..2]).unwrap(), 2);
        assert_eq!(&b[..2], &[2, 3]);
    }

    #[test]
    fn bytes_and_limit() {
        let d = pattern(100_000);
        let mut s = Src::new(&d, 50_000, &Limits::default());
        assert_eq!(s.bytes(0, 16).unwrap(), &d[..16]);
        assert_eq!(s.bytes(49_990, 10).unwrap(), &d[49_990..50_000]);
        assert!(s.bytes(49_990, 11).is_none());
        assert!(s.bytes(50_000, 1).is_none());
        assert_eq!(s.peek(49_990, 64), &d[49_990..50_000]);
        assert!(s.has_byte(49_999));
        assert!(!s.has_byte(50_000));
        assert!(!s.failed());
    }

    #[test]
    fn short_data_is_not_a_failure() {
        let d = pattern(5000);
        let mut s = Src::new(&d, u64::MAX, &Limits::default());
        assert_eq!(s.window(4990).len(), 10);
        assert!(s.window(5000).is_empty());
        assert!(s.bytes(4999, 2).is_none());
        assert_eq!(s.side::<2>(4998), Some([d[4998], d[4999]]));
        assert_eq!(s.side::<2>(4999), None);
        let (a, n) = s.side_peek::<8>(4996);
        assert_eq!((n, &a[..4]), (4, &d[4996..]));
        assert!(!s.failed());
    }

    #[test]
    fn sequential_windows_cover_everything_once() {
        let d = pattern(3 * 1024 * 1024 + 123);
        let c = Counting {
            data: &d,
            reads: Cell::new(0),
            bytes: Cell::new(0),
        };
        let mut s = Src::new(&c, u64::MAX, &Limits::default());
        s.scan_mode();
        let mut pos = 0u64;
        let mut sum = 0u64;
        loop {
            let w = s.window(pos);
            if w.is_empty() {
                break;
            }
            sum += w.iter().map(|&b| b as u64).sum::<u64>();
            pos += w.len() as u64;
        }
        assert_eq!(pos, d.len() as u64);
        assert_eq!(sum, d.iter().map(|&b| b as u64).sum::<u64>());
        assert_eq!(c.bytes.get(), d.len() as u64);
        assert!(c.reads.get() <= 8);
    }

    #[test]
    fn scan_blocks_start_small_and_grow() {
        use std::cell::RefCell;
        struct Recording<'a>(&'a [u8], RefCell<Vec<(u64, usize)>>);
        impl ReadAt for Recording<'_> {
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
                self.1.borrow_mut().push((offset, buf.len()));
                self.0.read_at(offset, buf)
            }
        }
        let d = pattern(5 << 20);
        let r = Recording(&d, RefCell::new(Vec::new()));
        // a header look in hop mode, then a scan from a position inside it
        let mut s = Src::new(&r, u64::MAX, &Limits::default());
        assert!(s.bytes(0, 30).is_some());
        s.scan_mode();
        let mut pos = 100u64;
        loop {
            let n = s.window(pos).len() as u64;
            if n == 0 {
                break;
            }
            pos += n;
        }
        let k = 1024;
        assert_eq!(
            r.1.borrow()[..7],
            [
                (0, 16 * k),
                (16 * k as u64, 64 * k),
                (80 * k as u64, 128 * k),
                (208 * k as u64, 256 * k),
                (464 * k as u64, 512 * k),
                (976 * k as u64, 1024 * k),
                (2000 * k as u64, 1024 * k),
            ]
        );
        // every read is a whole number of 4 KiB blocks at a 4 KiB offset
        assert!(r
            .1
            .borrow()
            .iter()
            .all(|&(o, n)| o % 4096 == 0 && n % 4096 == 0));

        // a scan from the very start
        let r = Recording(&d, RefCell::new(Vec::new()));
        let mut s = Src::new(&r, u64::MAX, &Limits::default());
        s.scan_mode();
        let mut pos = 0u64;
        while pos < 3 << 20 {
            pos += s.window(pos).len() as u64;
        }
        let starts: Vec<u64> = r.1.borrow().iter().map(|&(o, _)| o).collect();
        assert_eq!(
            starts[..6],
            [0, 65_536, 196_608, 458_752, 983_040, 2_031_616]
        );
    }

    #[test]
    fn budget_exhaustion_is_sticky() {
        let d = pattern(1 << 20);
        let lim = Limits {
            max_read_bytes: 40_000,
            max_reads: 100,
        };
        let mut s = Src::new(&d, u64::MAX, &lim);
        assert!(s.bytes(0, 8).is_some());
        assert!(s.bytes(200_000, 8).is_some());
        assert!(s.bytes(400_000, 8).is_none());
        assert!(s.failed());
        assert!(s.bytes(0, 8).is_none(), "a dead source stays dead");
        assert!(s.side::<4>(900_000).is_none());

        let lim = Limits {
            max_read_bytes: u64::MAX,
            max_reads: 2,
        };
        let mut s = Src::new(&d, u64::MAX, &lim);
        assert!(s.bytes(0, 8).is_some());
        assert!(s.side::<4>(500_000).is_some());
        assert!(s.side::<4>(900_000).is_none());
        assert!(s.failed());
    }

    #[test]
    fn io_error_fails() {
        struct Bad;
        impl ReadAt for Bad {
            fn read_at(&self, _: u64, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("bad sector"))
            }
        }
        let mut s = Src::new(&Bad, u64::MAX, &Limits::default());
        assert!(s.bytes(0, 4).is_none());
        assert!(s.failed());
    }

    #[test]
    fn hop_reads_survive_a_bad_sector_in_the_read_ahead() {
        /// Bytes 4096.. are unreadable.
        struct Partly(Vec<u8>);
        impl ReadAt for Partly {
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
                if offset + buf.len() as u64 > 4096 {
                    return Err(io::Error::other("bad sector"));
                }
                self.0.read_at(offset, buf)
            }
        }
        let d = Partly(pattern(100_000));
        let mut s = Src::new(&d, u64::MAX, &Limits::default());
        // the 16 KiB read-ahead fails, the 8 bytes asked for do not
        assert_eq!(s.bytes(100, 8).unwrap(), &d.0[100..108]);
        assert!(!s.failed());
        assert!(s.bytes(4090, 8).is_none());
        assert!(s.failed());
        // a sequential scan does not retry
        let mut s = Src::new(&d, u64::MAX, &Limits::default());
        s.scan_mode();
        assert!(s.window(0).is_empty());
        assert!(s.failed());
    }

    #[test]
    fn lying_reader_cannot_overrun() {
        struct Liar;
        impl ReadAt for Liar {
            fn read_at(&self, _: u64, buf: &mut [u8]) -> io::Result<usize> {
                Ok(buf.len() * 2 + 1)
            }
        }
        let mut s = Src::new(&Liar, 1 << 20, &Limits::default());
        assert_eq!(s.bytes(10, 4), Some(&[0u8; 4][..]));
    }
}
