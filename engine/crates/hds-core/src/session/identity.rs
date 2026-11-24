//! Does a source hold the data a session was made from? (ALGORITHMS B4)
//!
//! A source matches when its size covers the session range and at least
//! 90 % of up to [`SAMPLE_COUNT`] stored records, spread over the range,
//! re-read from the source have their stored hash. The caller excludes
//! records it knows to belong to the current volume (they change while the
//! disk is in use). Model and serial differences are only warnings. `force`
//! waives exactly one condition: that at least 8 samples were checked; a
//! source whose samples do not match is always refused.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use hds_io::{read_unaligned, BlockSource, RangedSource, ReadOutcome, Warning};
use hds_ntfs::fnv1a64;

use super::Session;
use crate::store::{RawRecord, RecordId};

/// Records re-read at most.
pub const SAMPLE_COUNT: usize = 64;
/// Fewer checked samples than this need `force`.
pub const MIN_SAMPLES: u32 = 8;
/// Warning code: model or serial number differ from the session's.
pub const WARN_IDENTITY_DIFFERS: &str = "identity_differs";

/// The outcome of [`check_identity`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityCheck {
    /// Whether the source may be attached.
    pub matches: bool,
    /// The source is large enough for the session range.
    pub size_ok: bool,
    /// Records chosen as samples.
    pub samples: u32,
    /// Samples whose hash matched.
    pub matched: u32,
    /// Samples whose hash differed.
    pub mismatched: u32,
    /// Samples that could not be read (bad sectors): not counted either way.
    pub unreadable: u32,
    /// The names of the failed checks, for `source_mismatch`'s error data:
    /// `"size"`, `"records"`, `"too_few_samples"`.
    pub failed: Vec<&'static str>,
    /// `identity_differs` when model or serial differ.
    pub warnings: Vec<Warning>,
}

/// Why the check could not be completed.
#[derive(Debug)]
pub enum IdentityError {
    /// The source stopped answering (protocol `source_lost`).
    SourceLost(std::io::Error),
    /// The cancel flag was set.
    Cancelled,
    /// A read failed for a reason that is neither the medium nor the device.
    Internal(std::io::Error),
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::SourceLost(e) => write!(f, "the source stopped answering: {e}"),
            IdentityError::Cancelled => f.write_str("cancelled"),
            IdentityError::Internal(e) => write!(f, "a read failed: {e}"),
        }
    }
}

impl std::error::Error for IdentityError {}

/// Up to [`SAMPLE_COUNT`] records spread evenly over the eligible ones in
/// offset order (the first and the last included).
fn choose_samples(
    session: &Session,
    exclude: &dyn Fn(RecordId, &RawRecord) -> bool,
) -> Vec<RecordId> {
    let store = session.store();
    let eligible: Vec<RecordId> = store
        .records_by_offset()
        .iter()
        .copied()
        .filter(|&id| store.record(id).is_some_and(|r| !exclude(id, r)))
        .collect();
    let n = eligible.len();
    if n <= SAMPLE_COUNT {
        return eligible;
    }
    let last = n.saturating_sub(1);
    let steps = SAMPLE_COUNT.saturating_sub(1);
    (0..SAMPLE_COUNT)
        .filter_map(|i| {
            let at = i
                .checked_mul(last)
                .and_then(|v| v.checked_div(steps))
                .unwrap_or(last);
            eligible.get(at).copied()
        })
        .collect()
}

/// Checks whether `source` holds the data `session` was made from
/// (ALGORITHMS B4). Every read stays inside the session range.
pub fn check_identity(
    session: &Session,
    source: Arc<dyn BlockSource>,
    exclude: &dyn Fn(RecordId, &RawRecord) -> bool,
    force: bool,
    cancel: &AtomicBool,
) -> Result<IdentityCheck, IdentityError> {
    let meta = session.meta();
    let info = source.info();
    let mut check = IdentityCheck {
        matches: false,
        size_ok: false,
        samples: 0,
        matched: 0,
        mismatched: 0,
        unreadable: 0,
        failed: Vec::new(),
        warnings: Vec::new(),
    };
    if info.model != meta.identity.model || info.serial != meta.identity.serial {
        check.warnings.push(Warning {
            code: WARN_IDENTITY_DIFFERS.to_string(),
            message: format!(
                "The disk reports model {:?} and serial {:?}; the session recorded {:?} and {:?}. \
                 USB adapters can change these; the disk's content was checked as well.",
                info.model, info.serial, meta.identity.model, meta.identity.serial
            ),
        });
    }
    let Ok(ranged) = RangedSource::new(source, meta.range.clone()) else {
        check.failed.push("size");
        return Ok(check);
    };
    check.size_ok = true;

    let samples = choose_samples(session, exclude);
    check.samples = u32::try_from(samples.len()).unwrap_or(u32::MAX);
    let mut buf = vec![0_u8; 65_536];
    for id in samples {
        let Some(record) = session.store().record(id) else {
            continue;
        };
        let size = usize::try_from(record.record_size()).unwrap_or(usize::MAX);
        let Some(bytes) = buf.get_mut(..size) else {
            continue;
        };
        match read_unaligned(&ranged, record.disk_offset(), bytes, cancel) {
            ReadOutcome::Data { bad } if bad.is_empty() => {
                if fnv1a64(bytes) == record.hash() {
                    check.matched = check.matched.saturating_add(1);
                } else {
                    check.mismatched = check.mismatched.saturating_add(1);
                }
            }
            ReadOutcome::Data { .. } => check.unreadable = check.unreadable.saturating_add(1),
            ReadOutcome::SourceLost(e) => return Err(IdentityError::SourceLost(e)),
            ReadOutcome::Cancelled => return Err(IdentityError::Cancelled),
            ReadOutcome::Internal(e) => return Err(IdentityError::Internal(e)),
        }
    }
    let checked = check.matched.saturating_add(check.mismatched);
    // At least 90 %: matched / checked >= 9 / 10.
    let ninety =
        u64::from(check.matched).saturating_mul(10) >= u64::from(checked).saturating_mul(9);
    if !ninety {
        check.failed.push("records");
    } else if checked < MIN_SAMPLES && !force {
        check.failed.push("too_few_samples");
    }
    check.matches = check.failed.is_empty();
    Ok(check)
}
