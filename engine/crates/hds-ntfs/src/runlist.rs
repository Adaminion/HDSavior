//! Runlists ("mapping pairs") of non-resident attributes
//! (docs/notes/ntfs-on-disk.md, section 5).

use core::iter::FusedIterator;

use crate::bytes::signed_le;

/// One run of a runlist: `length` clusters that are either stored at `lcn`
/// and onwards or not stored at all.
///
/// Runs produced by this crate satisfy `1 <= length`,
/// `lcn + length <= i64::MAX`, and the lengths of all runs of one list add
/// up to at most `i64::MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Run {
    /// First cluster of the run, counted from the start of the volume the
    /// record belongs to, in that volume's cluster size; `None` for a
    /// sparse run (a hole that reads as zeros).
    pub lcn: Option<u64>,
    /// Number of clusters.
    pub length: u64,
}

impl Run {
    /// Whether the run is a hole.
    #[inline]
    #[must_use]
    pub const fn is_sparse(&self) -> bool {
        self.lcn.is_none()
    }
}

/// Why decoding a runlist stopped before its terminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RunlistError {
    /// The bytes ended where a header byte was expected: there is no `0x00`
    /// terminator.
    Unterminated,
    /// A header byte announces more bytes than are left.
    Truncated,
    /// A header byte announces a length or offset field of more than 8
    /// bytes.
    FieldTooLarge,
    /// The run length is absent (field size 0), zero or negative. Lengths
    /// are signed on disk; encoders add a zero byte when the top bit would
    /// be set.
    BadLength,
    /// The run would start at a negative cluster number.
    NegativeLcn,
    /// A cluster number, the end of a run, or the total length of the list
    /// does not fit in 63 bits.
    Overflow,
}

/// How decoding a runlist ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RunlistStatus {
    /// The `0x00` terminator was reached; every run before it is valid.
    Complete,
    /// Decoding stopped early. The runs produced before the fault are valid
    /// by themselves, but the list is incomplete.
    Invalid(RunlistError),
}

impl RunlistStatus {
    /// Whether the terminator was reached.
    #[inline]
    #[must_use]
    pub const fn is_complete(self) -> bool {
        matches!(self, RunlistStatus::Complete)
    }
}

/// Decoder for a runlist; an iterator over its [`Run`]s that allocates
/// nothing.
///
/// Rules (ARCHITECTURE.md 5.1):
///
/// * header byte: low nibble = size of the length field, high nibble = size
///   of the offset field, both in bytes; a `0x00` header byte ends the list
///   and whatever follows it is padding;
/// * length: little-endian, **signed**, must be greater than 0;
/// * offset: little-endian, signed, added to the LCN of the last
///   non-sparse run (0 before the first one);
/// * offset size 0: a sparse run, which leaves that LCN unchanged;
/// * an offset field that is present with value 0 is a real run (`$Boot`
///   is `11 02 00`: two clusters at LCN 0);
/// * field sizes over 8, negative LCNs and anything that overflows 63 bits
///   are rejected.
///
/// The iterator yields runs until the terminator or the first fault;
/// [`status`](Runs::status) then tells which it was.
///
/// ```
/// use hds_ntfs::{Run, RunlistStatus, Runs};
///
/// // 0x1234 clusters at LCN 0x5678, a hole of 0x10, 1 cluster 0x100 before the first run
/// let bytes = [0x22, 0x34, 0x12, 0x78, 0x56, 0x01, 0x10, 0x21, 0x01, 0x00, 0xFF, 0x00];
/// let mut runs = Runs::new(&bytes);
/// assert_eq!(runs.next(), Some(Run { lcn: Some(0x5678), length: 0x1234 }));
/// assert_eq!(runs.next(), Some(Run { lcn: None, length: 0x10 }));
/// assert_eq!(runs.next(), Some(Run { lcn: Some(0x5578), length: 1 }));
/// assert_eq!(runs.next(), None);
/// assert_eq!(runs.status(), Some(RunlistStatus::Complete));
/// assert_eq!(runs.clusters(), 0x1234 + 0x10 + 1);
/// ```
#[derive(Clone, Debug)]
pub struct Runs<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// LCN of the last non-sparse run; 0 before the first.
    lcn: i64,
    /// Sum of the lengths of the runs yielded so far.
    clusters: u64,
    status: Option<RunlistStatus>,
}

impl<'a> Runs<'a> {
    /// Starts decoding at the first byte of `bytes`. The list may end
    /// before the slice does.
    #[inline]
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Runs {
            bytes,
            pos: 0,
            lcn: 0,
            clusters: 0,
            status: None,
        }
    }

    /// How decoding ended; `None` while the iterator has not returned
    /// `None` yet.
    #[inline]
    #[must_use]
    pub const fn status(&self) -> Option<RunlistStatus> {
        self.status
    }

    /// Total length, in clusters, of the runs yielded so far.
    #[inline]
    #[must_use]
    pub const fn clusters(&self) -> u64 {
        self.clusters
    }

    /// Number of bytes consumed so far, the terminator included once it has
    /// been reached.
    #[inline]
    #[must_use]
    pub const fn consumed(&self) -> usize {
        self.pos
    }

    /// Decodes the run at the current position.
    fn decode_next(&mut self) -> Result<Option<Run>, RunlistError> {
        let rest = self
            .bytes
            .get(self.pos..)
            .ok_or(RunlistError::Unterminated)?;
        let (&header, fields) = rest.split_first().ok_or(RunlistError::Unterminated)?;
        if header == 0 {
            self.pos = self.pos.saturating_add(1);
            return Ok(None);
        }
        let length_size = usize::from(header & 0x0F);
        let offset_size = usize::from(header >> 4);
        if length_size > 8 || offset_size > 8 {
            return Err(RunlistError::FieldTooLarge);
        }
        if length_size == 0 {
            return Err(RunlistError::BadLength);
        }
        let (length_field, rest) = fields
            .split_at_checked(length_size)
            .ok_or(RunlistError::Truncated)?;
        let offset_field = rest.get(..offset_size).ok_or(RunlistError::Truncated)?;

        let length = signed_le(length_field);
        if length <= 0 {
            return Err(RunlistError::BadLength);
        }
        let lcn = if offset_size == 0 {
            None
        } else {
            let lcn = self
                .lcn
                .checked_add(signed_le(offset_field))
                .ok_or(RunlistError::Overflow)?;
            if lcn < 0 {
                return Err(RunlistError::NegativeLcn);
            }
            // The end of the run must be a valid cluster number as well.
            lcn.checked_add(length).ok_or(RunlistError::Overflow)?;
            Some(lcn)
        };
        let length = length.cast_unsigned();
        let clusters = self
            .clusters
            .checked_add(length)
            .filter(|total| *total <= i64::MAX.cast_unsigned())
            .ok_or(RunlistError::Overflow)?;

        // Commit only now, so that a fault leaves the state at the last
        // good run.
        self.clusters = clusters;
        if let Some(lcn) = lcn {
            self.lcn = lcn;
        }
        self.pos = self
            .pos
            .saturating_add(1)
            .saturating_add(length_size)
            .saturating_add(offset_size);
        Ok(Some(Run {
            lcn: lcn.map(i64::cast_unsigned),
            length,
        }))
    }
}

impl Iterator for Runs<'_> {
    type Item = Run;

    #[inline]
    fn next(&mut self) -> Option<Run> {
        if self.status.is_some() {
            return None;
        }
        match self.decode_next() {
            Ok(Some(run)) => Some(run),
            Ok(None) => {
                self.status = Some(RunlistStatus::Complete);
                None
            }
            Err(error) => {
                self.status = Some(RunlistStatus::Invalid(error));
                None
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.status.is_some() {
            return (0, Some(0));
        }
        // Every run takes at least a header byte and one length byte.
        let left = self.bytes.len().saturating_sub(self.pos);
        (0, Some(left / 2))
    }
}

impl FusedIterator for Runs<'_> {}

/// Result of [`decode_runlist`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DecodedRunlist {
    /// Whether the list was complete and valid, or why not.
    pub status: RunlistStatus,
    /// Number of runs appended to the output vector.
    pub runs: usize,
    /// Total length of those runs in clusters.
    pub clusters: u64,
    /// Bytes of the input that were consumed (terminator included when the
    /// list was complete).
    pub consumed: usize,
}

/// Decodes a runlist into `out`.
///
/// The runs are **appended**: `out` is not cleared, so that a caller can
/// collect the extents of several attributes in one scratch vector and keep
/// index ranges. When the list is faulty, the runs decoded before the fault
/// are appended all the same and `status` says what went wrong.
///
/// At most `bytes.len() / 2` runs can come out of `bytes`, which bounds the
/// growth of `out`. See [`Runs`] for the decoding rules and for a decoder
/// that does not need a vector.
pub fn decode_runlist(bytes: &[u8], out: &mut Vec<Run>) -> DecodedRunlist {
    let mut runs = Runs::new(bytes);
    let before = out.len();
    out.extend(&mut runs);
    DecodedRunlist {
        status: runs
            .status()
            .unwrap_or(RunlistStatus::Invalid(RunlistError::Unterminated)),
        runs: out.len().saturating_sub(before),
        clusters: runs.clusters(),
        consumed: runs.consumed(),
    }
}
