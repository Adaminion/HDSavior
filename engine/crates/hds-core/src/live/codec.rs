//! The byte layout of the session blocks this layer owns (`LIVE`, `HEADS`,
//! `OVERRIDES`): a 4-byte tag, a version byte, then little-endian fields.
//!
//! Decoding is total: every count is checked against the bytes that are
//! left before anything is allocated, every value is validated by its
//! owner, and a payload with bytes after its end is refused.

use std::fmt;
use std::ops::Range;

use hds_io::RangeSet;

/// A session payload of this layer that cannot be decoded (written by a
/// newer engine, cut, or not ours).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    /// What was wrong.
    pub message: String,
}

impl DecodeError {
    pub(crate) fn new(message: impl Into<String>) -> DecodeError {
        DecodeError {
            message: message.into(),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DecodeError {}

/// Builds a payload.
pub(crate) struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub(crate) fn new(tag: &[u8; 4], version: u8) -> Writer {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(tag);
        buf.push(version);
        Writer { buf }
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub(crate) fn bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// A length or count (as u64).
    pub(crate) fn len(&mut self, n: usize) {
        self.u64(u64::try_from(n).unwrap_or(u64::MAX));
    }

    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.buf.extend_from_slice(b);
    }

    pub(crate) fn opt_u64(&mut self, v: Option<u64>) {
        match v {
            Some(v) => {
                self.u8(1);
                self.u64(v);
            }
            None => self.u8(0),
        }
    }

    pub(crate) fn range(&mut self, r: &Range<u64>) {
        self.u64(r.start);
        self.u64(r.end);
    }

    pub(crate) fn ranges(&mut self, rs: &[Range<u64>]) {
        self.len(rs.len());
        for r in rs {
            self.range(r);
        }
    }

    pub(crate) fn range_set(&mut self, set: &RangeSet) {
        self.len(set.range_count());
        for r in set.iter() {
            self.range(&r);
        }
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Reads a payload.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Checks the tag and the version (only `version` is understood).
    pub(crate) fn new(bytes: &'a [u8], tag: &[u8; 4], version: u8) -> Result<Reader<'a>, DecodeError> {
        let mut r = Reader { bytes, pos: 0 };
        let got = r.take(4)?;
        if got != tag {
            return Err(DecodeError::new(format!(
                "not a {} payload",
                String::from_utf8_lossy(tag)
            )));
        }
        let v = r.u8()?;
        if v != version {
            return Err(DecodeError::new(format!(
                "{} payload version {v} is not supported (this engine writes {version})",
                String::from_utf8_lossy(tag)
            )));
        }
        Ok(r)
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| DecodeError::new("length overflow"))?;
        let s = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| DecodeError::new("payload is cut short"))?;
        self.pos = end;
        Ok(s)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        let s = self.take(1)?;
        s.first().copied().ok_or_else(|| DecodeError::new("payload is cut short"))
    }

    pub(crate) fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(DecodeError::new(format!("invalid boolean {v}"))),
        }
    }

    pub(crate) fn u32(&mut self) -> Result<u32, DecodeError> {
        let s = self.take(4)?;
        let a: [u8; 4] = s.try_into().map_err(|_| DecodeError::new("payload is cut short"))?;
        Ok(u32::from_le_bytes(a))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, DecodeError> {
        let s = self.take(8)?;
        let a: [u8; 8] = s.try_into().map_err(|_| DecodeError::new("payload is cut short"))?;
        Ok(u64::from_le_bytes(a))
    }

    /// A count of items of at least `item_bytes` bytes each: refused when
    /// the payload cannot hold that many.
    pub(crate) fn count(&mut self, item_bytes: usize) -> Result<usize, DecodeError> {
        let n = usize::try_from(self.u64()?).map_err(|_| DecodeError::new("count too large"))?;
        let need = n
            .checked_mul(item_bytes.max(1))
            .ok_or_else(|| DecodeError::new("count too large"))?;
        if need > self.remaining() {
            return Err(DecodeError::new("count larger than the payload"));
        }
        Ok(n)
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.count(1)?;
        self.take(n)
    }

    pub(crate) fn opt_u64(&mut self) -> Result<Option<u64>, DecodeError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.u64()?)),
            v => Err(DecodeError::new(format!("invalid option tag {v}"))),
        }
    }

    pub(crate) fn range(&mut self) -> Result<Range<u64>, DecodeError> {
        let start = self.u64()?;
        let end = self.u64()?;
        if start > end {
            return Err(DecodeError::new("range with start after end"));
        }
        Ok(start..end)
    }

    pub(crate) fn ranges(&mut self) -> Result<Vec<Range<u64>>, DecodeError> {
        let n = self.count(16)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.range()?);
        }
        Ok(out)
    }

    /// A range set: ascending, disjoint, non-empty ranges.
    pub(crate) fn range_set(&mut self) -> Result<RangeSet, DecodeError> {
        let n = self.count(16)?;
        let mut set = RangeSet::new();
        let mut last_end: Option<u64> = None;
        for _ in 0..n {
            let r = self.range()?;
            if r.start >= r.end || last_end.is_some_and(|e| r.start <= e) {
                return Err(DecodeError::new("range set out of order"));
            }
            last_end = Some(r.end);
            set.insert(r);
        }
        Ok(set)
    }

    /// Refuses trailing bytes.
    pub(crate) fn end(self) -> Result<(), DecodeError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(DecodeError::new("bytes after the end of the payload"))
        }
    }
}
