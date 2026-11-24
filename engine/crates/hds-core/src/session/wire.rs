//! Little-endian encoding helpers for session-file payloads.
//!
//! Reading is total: every read checks the remaining length first, and
//! nothing is allocated by a length taken from the data before that length
//! has been compared with the bytes that are really there.

/// Longest string accepted in a payload (paths, models, warning texts).
pub(crate) const MAX_STRING: usize = 128 * 1024;

/// Why a payload does not decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Malformed(pub &'static str);

/// Appends little-endian values to a buffer.
pub(crate) trait Put {
    fn u8(&mut self, v: u8);
    fn u16(&mut self, v: u16);
    fn u32(&mut self, v: u32);
    fn u64(&mut self, v: u64);
    fn bytes(&mut self, v: &[u8]);
    /// A string as u32 length and UTF-8 bytes (cut at [`MAX_STRING`] on a
    /// character boundary).
    fn str(&mut self, v: &str);
    fn opt_str(&mut self, v: Option<&str>);
}

impl Put for Vec<u8> {
    fn u8(&mut self, v: u8) {
        self.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.extend_from_slice(v);
    }
    fn str(&mut self, v: &str) {
        let mut end = v.len().min(MAX_STRING);
        while !v.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        let cut = v.get(..end).unwrap_or("");
        self.u32(u32::try_from(cut.len()).unwrap_or(0));
        self.extend_from_slice(cut.as_bytes());
    }
    fn opt_str(&mut self, v: Option<&str>) {
        match v {
            Some(s) => {
                self.u8(1);
                self.str(s);
            }
            None => self.u8(0),
        }
    }
}

/// Reads little-endian values from a payload.
#[derive(Clone, Debug)]
pub(crate) struct Get<'a> {
    rest: &'a [u8],
}

impl<'a> Get<'a> {
    pub(crate) fn new(payload: &'a [u8]) -> Self {
        Get { rest: payload }
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        let (head, tail) = self
            .rest
            .split_at_checked(n)
            .ok_or(Malformed("payload ends early"))?;
        self.rest = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        let (head, tail) = self
            .rest
            .split_first_chunk::<N>()
            .ok_or(Malformed("payload ends early"))?;
        self.rest = tail;
        Ok(*head)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Malformed> {
        self.array::<1>().map(|[b]| b)
    }

    pub(crate) fn u16(&mut self) -> Result<u16, Malformed> {
        self.array().map(u16::from_le_bytes)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Malformed> {
        self.array().map(u32::from_le_bytes)
    }

    pub(crate) fn u64(&mut self) -> Result<u64, Malformed> {
        self.array().map(u64::from_le_bytes)
    }

    pub(crate) fn bool(&mut self) -> Result<bool, Malformed> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Malformed("invalid boolean")),
        }
    }

    pub(crate) fn str(&mut self) -> Result<String, Malformed> {
        let len = usize::try_from(self.u32()?).map_err(|_| Malformed("string too long"))?;
        if len > MAX_STRING {
            return Err(Malformed("string too long"));
        }
        let bytes = self.bytes(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| Malformed("string is not UTF-8"))
    }

    pub(crate) fn opt_str(&mut self) -> Result<Option<String>, Malformed> {
        if self.bool()? {
            self.str().map(Some)
        } else {
            Ok(None)
        }
    }

    /// A count of items of `item_size` bytes each that must still fit in
    /// the payload: the guard against allocations sized by hostile counts.
    pub(crate) fn count(&mut self, wide: bool, item_size: usize) -> Result<usize, Malformed> {
        let count = if wide {
            usize::try_from(self.u64()?).map_err(|_| Malformed("count too large"))?
        } else {
            usize::try_from(self.u32()?).map_err(|_| Malformed("count too large"))?
        };
        if count
            .checked_mul(item_size)
            .is_none_or(|need| need > self.rest.len())
        {
            return Err(Malformed("count larger than the payload"));
        }
        Ok(count)
    }

    /// Fails unless every byte has been read.
    pub(crate) fn finish(&self) -> Result<(), Malformed> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(Malformed("trailing bytes in payload"))
        }
    }
}
