//! The first-sector heads read for geometry checks (ALGORITHMS F3) and
//! their session block `HEADS`.
//!
//! Every head read is kept, keyed by its absolute offset: the first
//! [`SNIFF_LEN`] bytes, or the fact that the sector could not be read. A
//! catalog built without an attached source uses the stored heads and
//! reads nothing; since the analysis is deterministic it asks for the same
//! offsets and comes out the same.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::sync::atomic::AtomicBool;

use hds_carve::SNIFF_LEN;
use hds_io::{read_unaligned, RangedSource, ReadOutcome};

use crate::live::codec::{Reader, Writer};
use crate::live::DecodeError;
use crate::session::{OpaqueKind, Session, SessionError};

const HEADS_TAG: &[u8; 4] = b"HDHD";
const HEADS_VERSION: u8 = 1;

/// One stored head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    /// The first [`SNIFF_LEN`] bytes at the offset.
    Bytes([u8; SNIFF_LEN]),
    /// The sector could not be read (bad sectors) or lies outside the
    /// session range.
    Unreadable,
}

/// Why a head could not be obtained.
#[derive(Debug)]
pub enum HeadError {
    /// The device stopped answering (S7).
    SourceLost(io::Error),
    /// The cancel flag was set.
    Cancelled,
}

impl fmt::Display for HeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeadError::SourceLost(e) => write!(f, "the source stopped answering: {e}"),
            HeadError::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for HeadError {}

/// The heads known so far, and the source to read new ones from (if
/// attached).
#[derive(Default)]
pub struct Heads {
    known: BTreeMap<u64, Head>,
    /// Offsets read since the last [`take_new`](Heads::take_new).
    fresh: Vec<u64>,
    source: Option<RangedSource>,
    reads: u64,
    misses: u64,
}

impl fmt::Debug for Heads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Heads")
            .field("known", &self.known.len())
            .field("fresh", &self.fresh.len())
            .field("attached", &self.source.is_some())
            .finish()
    }
}

impl Heads {
    /// No heads, no source.
    pub fn new() -> Heads {
        Heads::default()
    }

    /// The heads stored in the session's `HEADS` payloads.
    pub fn load(session: &Session) -> Result<Heads, DecodeError> {
        let mut h = Heads::new();
        for p in session.opaque(OpaqueKind::Heads) {
            h.merge_payload(p)?;
        }
        Ok(h)
    }

    /// Adds the heads of one payload.
    pub fn merge_payload(&mut self, bytes: &[u8]) -> Result<(), DecodeError> {
        let mut r = Reader::new(bytes, HEADS_TAG, HEADS_VERSION)?;
        let n = r.count(9)?;
        for _ in 0..n {
            let off = r.u64()?;
            let head = match r.u8()? {
                0 => Head::Unreadable,
                1 => {
                    let mut b = [0u8; SNIFF_LEN];
                    for slot in &mut b {
                        *slot = r.u8()?;
                    }
                    Head::Bytes(b)
                }
                v => return Err(DecodeError::new(format!("invalid head tag {v}"))),
            };
            self.known.insert(off, head);
        }
        r.end()
    }

    /// Lets new heads be read from `source` (`None`: use stored heads only).
    pub fn attach(&mut self, source: Option<RangedSource>) {
        self.source = source;
    }

    /// Whether a source is attached.
    pub fn has_source(&self) -> bool {
        self.source.is_some()
    }

    /// Number of known heads.
    pub fn len(&self) -> usize {
        self.known.len()
    }

    /// Whether no head is known.
    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }

    /// Device reads made so far.
    pub fn reads(&self) -> u64 {
        self.reads
    }

    /// Heads asked for that were not stored while no source was attached
    /// (those files were left unchecked). Non-zero means the analysis is
    /// not the one a source would give: the set of sampled files (F3)
    /// depends on the overwritten set, so a `LIVE` snapshot replaced since
    /// the heads were stored can ask for others. Attach the source and
    /// analyse again (and [`save_new`](Heads::save_new)) to make it so.
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// The stored head at `offset`, if known.
    pub fn stored(&self, offset: u64) -> Option<Head> {
        self.known.get(&offset).copied()
    }

    /// Records a head (tests, or heads obtained elsewhere).
    pub fn insert(&mut self, offset: u64, head: Head) {
        if self.known.insert(offset, head).is_none() {
            self.fresh.push(offset);
        }
    }

    /// The head at `offset`: stored, else read from the attached source
    /// (and kept). `Ok(None)` when it is not stored and no source is
    /// attached.
    pub fn get(&mut self, offset: u64, cancel: &AtomicBool) -> Result<Option<Head>, HeadError> {
        if let Some(h) = self.known.get(&offset) {
            return Ok(Some(*h));
        }
        let Some(src) = &self.source else {
            self.misses = self.misses.saturating_add(1);
            return Ok(None);
        };
        let len = u64::try_from(SNIFF_LEN).unwrap_or(64);
        let inside = offset
            .checked_add(len)
            .is_some_and(|end| src.contains(&(offset..end)));
        let head = if inside {
            let mut buf = [0u8; SNIFF_LEN];
            self.reads = self.reads.saturating_add(1);
            match read_unaligned(src, offset, &mut buf, cancel) {
                ReadOutcome::Data { bad } if bad.is_empty() => Head::Bytes(buf),
                ReadOutcome::Data { .. } | ReadOutcome::Internal(_) => Head::Unreadable,
                ReadOutcome::SourceLost(e) => return Err(HeadError::SourceLost(e)),
                ReadOutcome::Cancelled => return Err(HeadError::Cancelled),
            }
        } else {
            Head::Unreadable
        };
        self.insert(offset, head);
        Ok(Some(head))
    }

    /// The heads added since the last call, as one payload to append to the
    /// session's `HEADS` block (`None` when there is nothing new).
    pub fn take_new(&mut self) -> Option<Vec<u8>> {
        if self.fresh.is_empty() {
            return None;
        }
        let mut offs = std::mem::take(&mut self.fresh);
        offs.sort_unstable();
        offs.dedup();
        Some(self.payload_of(&offs))
    }

    /// All heads as one payload (for a compacted session).
    pub fn to_payload(&self) -> Vec<u8> {
        let offs: Vec<u64> = self.known.keys().copied().collect();
        self.payload_of(&offs)
    }

    fn payload_of(&self, offs: &[u64]) -> Vec<u8> {
        let mut w = Writer::new(HEADS_TAG, HEADS_VERSION);
        w.len(offs.len());
        for &o in offs {
            w.u64(o);
            match self.known.get(&o) {
                Some(Head::Bytes(b)) => {
                    w.u8(1);
                    for &x in b {
                        w.u8(x);
                    }
                }
                _ => w.u8(0),
            }
        }
        w.finish()
    }

    /// Appends the new heads to the session (written at the next
    /// checkpoint).
    pub fn save_new(&mut self, session: &mut Session) -> Result<(), SessionError> {
        match self.take_new() {
            Some(p) => session.append_opaque(OpaqueKind::Heads, p),
            None => Ok(()),
        }
    }
}
