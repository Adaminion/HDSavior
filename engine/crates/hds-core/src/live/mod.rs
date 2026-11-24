//! Current volumes (ALGORITHMS C): discovery (C1), the snapshot read
//! through the volumes' own structures (C2), the classification of stored
//! records (C3), current index data (C4) and the overwritten set (C5).
//!
//! # Flow
//!
//! 1. [`take_snapshot`] reads, through the session's [`RangedSource`]
//!    only, the partition table, every candidate boot sector, and for each
//!    current volume its `$MFT` extents (following `$ATTRIBUTE_LIST`), the
//!    `$MFT` bitmap, the initialized size, `format_time`, the cluster
//!    bitmap as live-allocated ranges and the `$MFTMirr` extents. The
//!    result, a [`LiveSnapshot`], is plain data.
//! 2. [`LiveState`] is the session's `LIVE` block: the latest snapshot and
//!    the time of the scan (the reference of "not more than a day after
//!    the scan", ALGORITHMS D2). [`LiveState::load`] and
//!    [`LiveState::save`] go through the opaque session API.
//! 3. [`LiveIndex::build`] classifies every stored record (C3), marks
//!    current index blocks and entries (C4) and builds the overwritten set
//!    (C5). It reads nothing: a loaded session without a source gives the
//!    same index as after the scan.
//!
//! # What the contract leaves open (decided here, see the crate report)
//!
//! * C1 asks for "record 0"; a record without a stored number (pre-3.1
//!   header) at the `$MFT` position is accepted as record 0 too.
//! * A `$MFT` bitmap that cannot be read (or a part of it) counts as "in
//!   use" for the slots it covers: a current record is then hidden rather
//!   than shown as an old one. Parts of the cluster bitmap that cannot be
//!   read count as allocated (overwritten): unknown is not evidence of
//!   intact. Both raise `live_volume_incomplete`.
//! * Parts of a current volume's structures outside the session range are
//!   not read (S2) and make the snapshot incomplete.
//! * `writes_not_detectable` is raised when a candidate holds a known file
//!   system that is not NTFS (FAT, exFAT, ReFS, BitLocker), or when no
//!   current volume was found although a candidate holds an NTFS boot
//!   sector.

#![deny(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_used
)]

mod classify;
pub(crate) mod codec;
#[cfg(test)]
mod tests;

use std::fmt;
use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use hds_io::{
    parse_partition_table, read_unaligned, sniff_boot_sector, FsKind, RangeSet, RangedSource,
    ReadOutcome,
};
use hds_ntfs::{attr_type, parse_boot_sector, parse_file_record, AttrListEntries, AttributeList};

use crate::session::{OpaqueKind, Session, SessionError};

pub use classify::{LiveIndex, RecordClass};
pub use codec::DecodeError;
use codec::{Reader, Writer};

/// Warning code: a current volume could not be read completely.
pub const WARN_LIVE_VOLUME_INCOMPLETE: &str = "live_volume_incomplete";
/// Warning code: writes since the format cannot be detected.
pub const WARN_WRITES_NOT_DETECTABLE: &str = "writes_not_detectable";

/// At most this many `$MFT` slots are described by a snapshot (a bitmap of
/// 128 MiB). Record numbers of the scan store are 32-bit; real volumes
/// stay far below.
pub const MAX_MFT_SLOTS: u64 = 1 << 30;

/// Largest `$ATTRIBUTE_LIST` of `$MFT` or `$Bitmap` that is read.
const MAX_ATTRIBUTE_LIST: u64 = 16 << 20;
/// At most this many extension records of one system file are followed.
const MAX_HOLDERS: usize = 4096;
/// Reads of attribute content are made in pieces of at most this size.
const READ_PIECE: u64 = 8 << 20;

/// One run of the `$MFT`'s `$DATA`: `clusters` clusters from `vcn` on,
/// stored at `lcn` (`None`: a hole).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MftRun {
    /// First VCN.
    pub vcn: u64,
    /// Length in clusters.
    pub clusters: u64,
    /// First LCN, `None` for a sparse run.
    pub lcn: Option<u64>,
}

/// One current volume (ALGORITHMS C1) as its own structures describe it
/// (C2). All offsets are absolute bytes of the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentVolume {
    /// Offset of its boot sector (cluster 0).
    pub offset: u64,
    /// Bytes per sector of the boot sector.
    pub bytes_per_sector: u32,
    /// Cluster size in bytes.
    pub cluster_size: u32,
    /// MFT record size in bytes.
    pub record_size: u32,
    /// Sectors of the volume (boot sector).
    pub total_sectors: u64,
    /// LCN of `$MFT` from the boot sector.
    pub mft_lcn: u64,
    /// The `$MFT`'s `$DATA` runs, by ascending VCN, from record 0 and its
    /// extension records.
    pub mft_runs: Vec<MftRun>,
    /// Initialized size of `$MFT:$DATA` (0 when unknown).
    pub mft_initialized: u64,
    /// The `$MFT` bitmap, one bit per slot below the initialized size (cut
    /// to [`MAX_MFT_SLOTS`]); unreadable parts are set.
    pub mft_bitmap: Vec<u8>,
    /// Absolute byte ranges of `$MFTMirr`'s data.
    pub mirror: Vec<Range<u64>>,
    /// Live-allocated ranges: the clusters set in `$Bitmap`, absolute.
    pub allocated: RangeSet,
    /// Creation time of `$Volume` (record 3), else of record 0 (FILETIME).
    pub format_time: u64,
    /// Some part could not be read.
    pub incomplete: bool,
}

impl CurrentVolume {
    /// Number of clusters (boot sector).
    pub fn total_clusters(&self) -> u64 {
        self.volume_bytes()
            .checked_div(u64::from(self.cluster_size))
            .unwrap_or(0)
    }

    /// Size in bytes (boot sector: total sectors times bytes per sector).
    pub fn volume_bytes(&self) -> u64 {
        self.total_sectors
            .saturating_mul(u64::from(self.bytes_per_sector))
    }

    /// The bytes the volume occupies, `[offset, offset + size)` (saturated).
    pub fn volume_range(&self) -> Range<u64> {
        self.offset..self.offset.saturating_add(self.volume_bytes())
    }

    /// Disk offset of the start of `$MFT` per the boot sector: the origin of
    /// its records' grid (ALGORITHMS D1, D8).
    pub fn mft_origin(&self) -> Option<u64> {
        self.mft_lcn
            .checked_mul(u64::from(self.cluster_size))
            .and_then(|b| b.checked_add(self.offset))
    }

    /// Number of slots below the initialized size that the snapshot
    /// describes.
    pub fn slot_count(&self) -> u64 {
        let slots = self
            .mft_initialized
            .checked_div(u64::from(self.record_size))
            .unwrap_or(0);
        slots.min(MAX_MFT_SLOTS)
    }

    /// Whether slot `n` is in use per the `$MFT` bitmap (a slot beyond the
    /// bitmap is free).
    pub fn slot_in_use(&self, n: u64) -> bool {
        let Ok(byte) = usize::try_from(n >> 3) else {
            return false;
        };
        self.mft_bitmap
            .get(byte)
            .is_some_and(|b| b >> (n & 7) & 1 != 0)
    }

    /// Disk offset of slot `n` (whole slot below the initialized size and
    /// mapped by a non-sparse run), else `None`.
    pub fn slot_offset(&self, n: u64) -> Option<u64> {
        if n >= self.slot_count() {
            return None;
        }
        let pos = n.checked_mul(u64::from(self.record_size))?;
        let cs = u64::from(self.cluster_size);
        let vcn = pos.checked_div(cs)?;
        let within = pos.checked_rem(cs)?;
        let run = self
            .mft_runs
            .iter()
            .find(|r| r.vcn <= vcn && r.vcn.checked_add(r.clusters).is_some_and(|end| vcn < end))?;
        let lcn = run.lcn?.checked_add(vcn.checked_sub(run.vcn)?)?;
        lcn.checked_mul(cs)?
            .checked_add(within)?
            .checked_add(self.offset)
    }

    /// The `$MFT` below the initialized size as absolute byte ranges, each
    /// with the byte position inside `$MFT:$DATA` of its first byte.
    pub fn slot_ranges(&self) -> Vec<(Range<u64>, u64)> {
        let cs = u64::from(self.cluster_size);
        let limit = self
            .slot_count()
            .saturating_mul(u64::from(self.record_size));
        let mut out = Vec::new();
        for run in &self.mft_runs {
            let Some(lcn) = run.lcn else { continue };
            let Some(lo) = run.vcn.checked_mul(cs) else {
                continue;
            };
            let hi = run
                .vcn
                .saturating_add(run.clusters)
                .saturating_mul(cs)
                .min(limit);
            if hi <= lo {
                continue;
            }
            let Some(start) = lcn.checked_mul(cs).and_then(|b| b.checked_add(self.offset)) else {
                continue;
            };
            let Some(end) = start.checked_add(hi.saturating_sub(lo)) else {
                continue;
            };
            out.push((start..end, lo));
        }
        out
    }
}

/// What [`take_snapshot`] found: the current volumes inside the session
/// range and what else C1 needs later.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveSnapshot {
    /// When it was taken (FILETIME).
    pub taken_at: u64,
    /// Partition starts inside the session range, ascending (partition
    /// table as read from the source; the range start is not included
    /// unless a partition starts there).
    pub partition_starts: Vec<u64>,
    /// The current volumes, by ascending offset.
    pub volumes: Vec<CurrentVolume>,
    /// Some part of a current volume could not be read
    /// (`live_volume_incomplete`).
    pub incomplete: bool,
    /// `writes_not_detectable` (see the module documentation).
    pub writes_not_detectable: bool,
}

impl LiveSnapshot {
    /// The warning codes this snapshot raises.
    pub fn warnings(&self) -> Vec<&'static str> {
        let mut w = Vec::new();
        if self.incomplete {
            w.push(WARN_LIVE_VOLUME_INCOMPLETE);
        }
        if self.writes_not_detectable {
            w.push(WARN_WRITES_NOT_DETECTABLE);
        }
        w
    }

    /// The range start followed by the partition starts inside the range:
    /// the places a volume of the disk itself starts (ALGORITHMS D7, D8,
    /// F1, F2).
    pub fn volume_starts(&self, range: &Range<u64>) -> Vec<u64> {
        let mut v = vec![range.start];
        for &p in &self.partition_starts {
            if range.contains(&p) && !v.contains(&p) {
                v.push(p);
            }
        }
        v
    }

    fn encode(&self, w: &mut Writer) {
        w.u64(self.taken_at);
        w.bool(self.incomplete);
        w.bool(self.writes_not_detectable);
        w.len(self.partition_starts.len());
        for &p in &self.partition_starts {
            w.u64(p);
        }
        w.len(self.volumes.len());
        for v in &self.volumes {
            w.u64(v.offset);
            w.u32(v.bytes_per_sector);
            w.u32(v.cluster_size);
            w.u32(v.record_size);
            w.u64(v.total_sectors);
            w.u64(v.mft_lcn);
            w.len(v.mft_runs.len());
            for r in &v.mft_runs {
                w.u64(r.vcn);
                w.u64(r.clusters);
                w.opt_u64(r.lcn);
            }
            w.u64(v.mft_initialized);
            w.bytes(&v.mft_bitmap);
            w.ranges(&v.mirror);
            w.range_set(&v.allocated);
            w.u64(v.format_time);
            w.bool(v.incomplete);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<LiveSnapshot, DecodeError> {
        let taken_at = r.u64()?;
        let incomplete = r.bool()?;
        let writes_not_detectable = r.bool()?;
        let n = r.count(8)?;
        let mut partition_starts = Vec::with_capacity(n);
        for _ in 0..n {
            partition_starts.push(r.u64()?);
        }
        let n = r.count(40)?;
        let mut volumes = Vec::with_capacity(n);
        for _ in 0..n {
            let offset = r.u64()?;
            let bytes_per_sector = r.u32()?;
            let cluster_size = r.u32()?;
            let record_size = r.u32()?;
            if !(bytes_per_sector.is_power_of_two()
                && cluster_size.is_power_of_two()
                && record_size.is_power_of_two())
            {
                return Err(DecodeError::new("current volume with impossible sizes"));
            }
            let total_sectors = r.u64()?;
            let mft_lcn = r.u64()?;
            let k = r.count(17)?;
            let mut mft_runs = Vec::with_capacity(k);
            for _ in 0..k {
                let vcn = r.u64()?;
                let clusters = r.u64()?;
                let lcn = r.opt_u64()?;
                mft_runs.push(MftRun { vcn, clusters, lcn });
            }
            let mft_initialized = r.u64()?;
            let mft_bitmap = r.bytes()?.to_vec();
            let mirror = r.ranges()?;
            let allocated = r.range_set()?;
            let format_time = r.u64()?;
            let incomplete = r.bool()?;
            volumes.push(CurrentVolume {
                offset,
                bytes_per_sector,
                cluster_size,
                record_size,
                total_sectors,
                mft_lcn,
                mft_runs,
                mft_initialized,
                mft_bitmap,
                mirror,
                allocated,
                format_time,
                incomplete,
            });
        }
        Ok(LiveSnapshot {
            taken_at,
            partition_starts,
            volumes,
            incomplete,
            writes_not_detectable,
        })
    }
}

/// The session's `LIVE` block: the time of the scan and the latest
/// snapshot (ALGORITHMS B1, C2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveState {
    /// When the scan last ran (FILETIME); the reference of D2's "not more
    /// than a day after the scan". `None` before the engine recorded it;
    /// the snapshot time stands in then.
    pub scan_time: Option<u64>,
    /// The latest snapshot.
    pub snapshot: Option<LiveSnapshot>,
}

const LIVE_TAG: &[u8; 4] = b"HDLV";
const LIVE_VERSION: u8 = 1;

impl LiveState {
    /// The reference time of D2 (FILETIME): the scan time, else the
    /// snapshot time, else `None` (no upper bound).
    pub fn reference_time(&self) -> Option<u64> {
        self.scan_time
            .or_else(|| self.snapshot.as_ref().map(|s| s.taken_at))
    }

    /// Encodes the state as one payload.
    pub fn to_payload(&self) -> Vec<u8> {
        let mut w = Writer::new(LIVE_TAG, LIVE_VERSION);
        w.opt_u64(self.scan_time);
        match &self.snapshot {
            Some(s) => {
                w.u8(1);
                s.encode(&mut w);
            }
            None => w.u8(0),
        }
        w.finish()
    }

    /// Decodes one payload of [`to_payload`](Self::to_payload).
    pub fn from_payload(bytes: &[u8]) -> Result<LiveState, DecodeError> {
        let mut r = Reader::new(bytes, LIVE_TAG, LIVE_VERSION)?;
        let scan_time = r.opt_u64()?;
        let snapshot = match r.u8()? {
            0 => None,
            1 => Some(LiveSnapshot::decode(&mut r)?),
            v => return Err(DecodeError::new(format!("invalid snapshot tag {v}"))),
        };
        r.end()?;
        Ok(LiveState {
            scan_time,
            snapshot,
        })
    }

    /// The state stored in the session (its last `LIVE` payload); the
    /// default when there is none.
    pub fn load(session: &Session) -> Result<LiveState, DecodeError> {
        match session.opaque(OpaqueKind::Live).last() {
            Some(p) => LiveState::from_payload(p),
            None => Ok(LiveState::default()),
        }
    }

    /// Replaces the session's `LIVE` block with this state (written at the
    /// next checkpoint).
    pub fn save(&self, session: &mut Session) -> Result<(), SessionError> {
        session.replace_opaque(OpaqueKind::Live, vec![self.to_payload()])
    }
}

/// Why a snapshot could not be taken.
#[derive(Debug)]
pub enum LiveError {
    /// The device stopped answering (S7): end the job with `source_lost`.
    SourceLost(io::Error),
    /// The cancel flag was set.
    Cancelled,
}

impl fmt::Display for LiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiveError::SourceLost(e) => write!(f, "the source stopped answering: {e}"),
            LiveError::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for LiveError {}

/// Reads through the session range only.
struct VolReader<'a> {
    src: &'a RangedSource,
    cancel: &'a AtomicBool,
}

/// One read: the bytes (unreadable sectors zero-filled) and the
/// unreadable parts (relative to the start of the read).
struct Bytes {
    data: Vec<u8>,
    bad: Vec<Range<u64>>,
}

impl VolReader<'_> {
    /// `None` when the range is not inside the session range (never read)
    /// or the read failed for a reason that is not the medium.
    fn read(&self, offset: u64, len: u64) -> Result<Option<Bytes>, LiveError> {
        let Some(end) = offset.checked_add(len) else {
            return Ok(None);
        };
        if len == 0 || !self.src.contains(&(offset..end)) {
            return Ok(None);
        }
        let Ok(n) = usize::try_from(len) else {
            return Ok(None);
        };
        let mut data = vec![0u8; n];
        match read_unaligned(self.src, offset, &mut data, self.cancel) {
            ReadOutcome::Data { bad } => {
                let bad = bad
                    .into_iter()
                    .map(|r| r.start.saturating_sub(offset)..r.end.saturating_sub(offset))
                    .collect();
                Ok(Some(Bytes { data, bad }))
            }
            ReadOutcome::SourceLost(e) => Err(LiveError::SourceLost(e)),
            ReadOutcome::Cancelled => Err(LiveError::Cancelled),
            ReadOutcome::Internal(_) => Ok(None),
        }
    }
}

/// Takes the snapshot of ALGORITHMS C1/C2 through `src` (nothing outside
/// its range is read). `taken_at` is the current time (FILETIME).
///
/// Fails only when the device is lost or `cancel` is set; everything else
/// that cannot be read makes the snapshot incomplete.
pub fn take_snapshot(
    src: &RangedSource,
    taken_at: u64,
    cancel: &AtomicBool,
) -> Result<LiveSnapshot, LiveError> {
    let range = src.range();
    let table = parse_partition_table(src);
    let mut partition_starts: Vec<u64> = table
        .partitions
        .iter()
        .map(|p| p.offset)
        .filter(|o| range.contains(o))
        .collect();
    partition_starts.sort_unstable();
    partition_starts.dedup();
    let mut snap = LiveSnapshot {
        taken_at,
        partition_starts,
        ..LiveSnapshot::default()
    };
    let rd = VolReader { src, cancel };
    let mut ntfs_boot = false;
    let mut other_fs = false;
    for start in snap.volume_starts(&range) {
        let Some(sector) = rd.read(start, 512)? else {
            continue;
        };
        if !sector.bad.is_empty() {
            continue;
        }
        if let Ok(boot) = parse_boot_sector(&sector.data) {
            ntfs_boot = true;
            if let Some(v) = read_volume(&rd, start, &boot)? {
                snap.incomplete |= v.incomplete;
                snap.volumes.push(v);
            }
        } else if let Some((kind, _)) = sniff_boot_sector(&sector.data) {
            if matches!(
                kind,
                FsKind::Fat | FsKind::Exfat | FsKind::Refs | FsKind::Bitlocker
            ) {
                other_fs = true;
            }
        }
    }
    snap.writes_not_detectable = other_fs || (ntfs_boot && snap.volumes.is_empty());
    Ok(snap)
}

/// A non-resident stream as runs `(vcn, clusters, lcn)` plus its sizes.
#[derive(Default)]
struct Stream {
    runs: Vec<MftRun>,
    resident: Option<Vec<u8>>,
    /// `(real, initialized)` from the VCN-0 extent.
    sizes: Option<(u64, u64)>,
    found: bool,
    damaged: bool,
}

impl Stream {
    fn add_extent(&mut self, ext: &hds_ntfs::Extent<'_>) {
        self.found = true;
        let mut vcn = ext.start_vcn;
        let mut runs = ext.runs();
        for run in &mut runs {
            self.runs.push(MftRun {
                vcn,
                clusters: run.length,
                lcn: run.lcn,
            });
            match vcn.checked_add(run.length) {
                Some(v) => vcn = v,
                None => {
                    self.damaged = true;
                    break;
                }
            }
        }
        if !runs.status().is_some_and(|s| s.is_complete()) {
            self.damaged = true;
        }
        if let Some(s) = ext.sizes() {
            self.sizes = Some((s.real, s.initialized));
        }
    }

    fn sort(&mut self) {
        self.runs.sort_by_key(|r| r.vcn);
    }

    /// The run that maps `vcn`, if any. Runs are sorted by VCN (see
    /// [`Stream::sort`]); of overlapping runs (damaged lists) the one that
    /// starts last at or below `vcn` decides. O(log runs).
    fn run_at(&self, vcn: u64) -> Option<&MftRun> {
        let after = self.runs.partition_point(|r| r.vcn <= vcn);
        let run = self.runs.get(after.checked_sub(1)?)?;
        run.vcn
            .checked_add(run.clusters)
            .is_some_and(|e| vcn < e)
            .then_some(run)
    }

    /// The first VCN above `vcn` at which a run starts.
    fn next_run_start(&self, vcn: u64) -> Option<u64> {
        let after = self.runs.partition_point(|r| r.vcn <= vcn);
        self.runs.get(after).map(|r| r.vcn)
    }

    /// Disk offset of byte `pos` of the stream, when mapped.
    fn map(&self, vol_offset: u64, cs: u64, pos: u64) -> Option<u64> {
        let vcn = pos.checked_div(cs)?;
        let within = pos.checked_rem(cs)?;
        let run = self.run_at(vcn)?;
        let lcn = run.lcn?.checked_add(vcn.checked_sub(run.vcn)?)?;
        lcn.checked_mul(cs)?
            .checked_add(within)?
            .checked_add(vol_offset)
    }
}

/// The parts of a system file we read: `$DATA` and `$BITMAP` (unnamed),
/// collected from the base record and the extension records its
/// `$ATTRIBUTE_LIST` names.
#[derive(Default)]
struct SystemFile {
    data: Stream,
    bitmap: Stream,
    created: Option<u64>,
    is_mft: bool,
}

/// Collects `$DATA` and `$BITMAP` of one parsed record into `file`;
/// returns the attribute list (resident bytes or extent) when present.
fn collect(record: &hds_ntfs::FileRecord<'_>, file: &mut SystemFile) -> Option<AttrList> {
    let mut list = None;
    for attr in record.attributes() {
        match attr.type_code {
            attr_type::STANDARD_INFORMATION => {
                if let Some(si) = attr.standard_information() {
                    file.created.get_or_insert(si.times.created);
                }
            }
            attr_type::FILE_NAME => {
                if attr.file_name().is_some_and(|f| f.name.eq_str("$MFT")) {
                    file.is_mft = true;
                }
            }
            attr_type::DATA | attr_type::BITMAP if attr.is_unnamed() => {
                let stream = if attr.type_code == attr_type::DATA {
                    &mut file.data
                } else {
                    &mut file.bitmap
                };
                if let Some(ext) = attr.extent() {
                    stream.add_extent(&ext);
                } else if let Some(v) = attr.resident() {
                    stream.found = true;
                    stream.resident = Some(v.bytes.to_vec());
                } else {
                    stream.damaged = true;
                }
            }
            attr_type::ATTRIBUTE_LIST => match attr.attribute_list() {
                Some(AttributeList::Resident(entries)) => {
                    list = Some(AttrList::Entries(holders_of(entries)));
                }
                Some(AttributeList::NonResident(ext)) => {
                    let mut s = Stream::default();
                    s.add_extent(&ext);
                    list = Some(AttrList::Stream(s));
                }
                None => list = Some(AttrList::Damaged),
            },
            _ => {}
        }
    }
    list
}

enum AttrList {
    Entries(Vec<u64>),
    Stream(Stream),
    Damaged,
}

/// The holders of unnamed `$DATA` and `$BITMAP` extents, other than the
/// base record itself (record numbers; at most [`MAX_HOLDERS`]).
fn holders_of(entries: AttrListEntries<'_>) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for e in entries {
        if (e.type_code == attr_type::DATA || e.type_code == attr_type::BITMAP)
            && e.name.is_empty()
            && !out.contains(&e.holder.number())
            && out.len() < MAX_HOLDERS
        {
            out.push(e.holder.number());
        }
    }
    out
}

/// One piece of a stream handed to a [`Sink`].
enum Piece<'a> {
    /// Bytes read from the disk, with the unreadable parts (relative to
    /// the piece, ascending; zero-filled in `bytes`).
    Data(&'a [u8], &'a [Range<u64>]),
    /// `len` bytes that were not read: a sparse run (`sparse`, zeros by
    /// definition) or a part that is unmapped, damaged or outside the
    /// session range (unknown). Never materialised, so a stretch of any
    /// length costs one call.
    Gap {
        /// Length in bytes.
        len: u64,
        /// A sparse run: its content is zeros. Otherwise unknown.
        sparse: bool,
    },
}

/// Receives the pieces of a stream in order.
type Sink<'a> = dyn FnMut(Piece<'_>) + 'a;

/// Reads a whole stream (at most `len` bytes) and hands it to `sink` in
/// order: read bytes in pieces of at most [`READ_PIECE`], and every
/// sparse, unmapped or unreadable stretch as one [`Piece::Gap`]. Returns
/// false when some part could not be read. The number of iterations is
/// bounded by `len / READ_PIECE` plus the number of runs, whatever the
/// stored sizes claim, and `cancel` is polled once per iteration.
fn read_stream(
    rd: &VolReader<'_>,
    vol_offset: u64,
    cs: u64,
    stream: &Stream,
    len: u64,
    sink: &mut Sink<'_>,
) -> Result<bool, LiveError> {
    if let Some(bytes) = &stream.resident {
        let n = usize::try_from(len).unwrap_or(usize::MAX).min(bytes.len());
        sink(Piece::Data(bytes.get(..n).unwrap_or(&[]), &[]));
        return Ok(true);
    }
    if cs == 0 {
        return Ok(false);
    }
    let mut complete = !stream.damaged;
    let mut pos = 0u64;
    while pos < len {
        if rd.cancel.load(Ordering::Relaxed) {
            return Err(LiveError::Cancelled);
        }
        let left = len.saturating_sub(pos);
        let vcn = pos.checked_div(cs).unwrap_or(0);
        let step = match stream.run_at(vcn) {
            Some(run) => {
                // The rest of this run, from `pos` on (at least one byte).
                let run_end = run.vcn.saturating_add(run.clusters).saturating_mul(cs);
                let avail = run_end.saturating_sub(pos).min(left).max(1);
                let disk = run.lcn.and_then(|lcn| {
                    lcn.checked_add(vcn.checked_sub(run.vcn)?)?
                        .checked_mul(cs)?
                        .checked_add(pos.checked_rem(cs)?)?
                        .checked_add(vol_offset)
                });
                match (run.lcn, disk) {
                    (None, _) => {
                        sink(Piece::Gap {
                            len: avail,
                            sparse: true,
                        });
                        avail
                    }
                    (Some(_), Some(d0)) => {
                        let range = rd.src.range();
                        if d0 < range.start || d0 >= range.end {
                            // Outside the session range (S2): not read.
                            // Unknown up to where the run enters the
                            // range, or to its end, in one step.
                            let out = if d0 < range.start {
                                avail.min(range.start.saturating_sub(d0))
                            } else {
                                avail
                            };
                            complete = false;
                            sink(Piece::Gap {
                                len: out,
                                sparse: false,
                            });
                            out
                        } else {
                            let piece = avail
                                .min(READ_PIECE)
                                .min(range.end.saturating_sub(d0))
                                .max(1);
                            match rd.read(d0, piece)? {
                                Some(b) => {
                                    if !b.bad.is_empty() {
                                        complete = false;
                                    }
                                    sink(Piece::Data(&b.data, &b.bad));
                                }
                                None => {
                                    complete = false;
                                    sink(Piece::Gap {
                                        len: piece,
                                        sparse: false,
                                    });
                                }
                            }
                            piece
                        }
                    }
                    (Some(_), None) => {
                        complete = false;
                        sink(Piece::Gap {
                            len: avail,
                            sparse: false,
                        });
                        avail
                    }
                }
            }
            None => {
                // Unmapped up to the next run (or the end).
                let next = stream
                    .next_run_start(vcn)
                    .map_or(len, |v| v.saturating_mul(cs));
                let gap = next.saturating_sub(pos).min(left).max(1);
                complete = false;
                sink(Piece::Gap {
                    len: gap,
                    sparse: false,
                });
                gap
            }
        };
        pos = pos.saturating_add(step);
    }
    Ok(complete)
}

/// Reads and parses a record of size `rs` at `offset`; `None` when it
/// cannot be read or parsed.
fn read_record(rd: &VolReader<'_>, offset: u64, rs: u32) -> Result<Option<Vec<u8>>, LiveError> {
    match rd.read(offset, u64::from(rs))? {
        Some(b) if b.bad.is_empty() => Ok(Some(b.data)),
        _ => Ok(None),
    }
}

/// Reads a system file (base record at `offset`), following its attribute
/// list; `own` is its record number, `slot` maps a record number to a
/// disk offset. Returns the file and
/// whether everything could be read.
fn read_system_file(
    rd: &VolReader<'_>,
    vol_offset: u64,
    cs: u64,
    rs: u32,
    (own, offset): (u64, u64),
    slot: &dyn Fn(&SystemFile, u64) -> Option<u64>,
) -> Result<Option<(SystemFile, bool)>, LiveError> {
    let Some(mut raw) = read_record(rd, offset, rs)? else {
        return Ok(None);
    };
    let Ok(rec) = parse_file_record(&mut raw) else {
        return Ok(None);
    };
    let mut file = SystemFile::default();
    let mut complete = true;
    let list = collect(&rec, &mut file);
    let holders = match list {
        None => Vec::new(),
        Some(AttrList::Entries(h)) => h,
        Some(AttrList::Damaged) => {
            complete = false;
            Vec::new()
        }
        Some(AttrList::Stream(s)) => {
            let len = s.sizes.map_or(0, |(real, _)| real).min(MAX_ATTRIBUTE_LIST);
            let mut bytes = Vec::new();
            complete &= read_stream(rd, vol_offset, cs, &s, len, &mut |p| match p {
                Piece::Data(b, _) => bytes.extend_from_slice(b),
                // At most MAX_ATTRIBUTE_LIST bytes in all.
                Piece::Gap { len, .. } => bytes.resize(
                    bytes
                        .len()
                        .saturating_add(usize::try_from(len).unwrap_or(0)),
                    0,
                ),
            })?;
            holders_of(AttrListEntries::new(&bytes))
        }
    };
    let mut pending: Vec<u64> = holders.into_iter().filter(|&h| h != own).collect();
    // Holders can describe the extents that map other holders: retry
    // while progress is made (each round maps at least one more holder).
    while !pending.is_empty() {
        file.data.sort();
        let mut next = Vec::new();
        let before = pending.len();
        for h in pending {
            let Some(at) = slot(&file, h) else {
                next.push(h);
                continue;
            };
            let Some(mut raw) = read_record(rd, at, rs)? else {
                complete = false;
                continue;
            };
            match parse_file_record(&mut raw) {
                Ok(x) => {
                    let _ = collect(&x, &mut file);
                }
                Err(_) => complete = false,
            }
        }
        pending = next;
        if pending.len() == before {
            break;
        }
    }
    if !pending.is_empty() {
        complete = false;
    }
    file.data.sort();
    file.bitmap.sort();
    complete &= !file.data.damaged && !file.bitmap.damaged;
    Ok(Some((file, complete)))
}

/// C1/C2 for the candidate at `start` with boot sector `boot`: `None` when
/// it is not a current volume.
fn read_volume(
    rd: &VolReader<'_>,
    start: u64,
    boot: &hds_ntfs::BootSector,
) -> Result<Option<CurrentVolume>, LiveError> {
    let cs = u64::from(boot.cluster_size);
    let rs = boot.mft_record_size;
    let Some(mft_at) = boot
        .mft_lcn
        .checked_mul(cs)
        .and_then(|b| b.checked_add(start))
    else {
        return Ok(None);
    };
    // Record 0: must parse, be in use, carry number 0 (or none) and be
    // named $MFT.
    let Some(mut raw0) = read_record(rd, mft_at, rs)? else {
        return Ok(None);
    };
    let Ok(rec0) = parse_file_record(&mut raw0) else {
        return Ok(None);
    };
    if !rec0.is_in_use() || !matches!(rec0.record_number(), Some(0) | None) {
        return Ok(None);
    }
    let mut probe = SystemFile::default();
    let _ = collect(&rec0, &mut probe);
    if !probe.is_mft {
        return Ok(None);
    }
    drop(probe);

    let map_mft = |f: &SystemFile, n: u64| -> Option<u64> {
        f.data.map(start, cs, n.checked_mul(u64::from(rs))?)
    };
    let Some((mft, mut complete)) = read_system_file(rd, start, cs, rs, (0, mft_at), &map_mft)?
    else {
        return Ok(None);
    };
    let (initialized, real) = match mft.data.sizes {
        Some((real, init)) => (init, real),
        None => {
            complete = false;
            (0, 0)
        }
    };
    let _ = real;
    let mut vol = CurrentVolume {
        offset: start,
        bytes_per_sector: boot.bytes_per_sector,
        cluster_size: boot.cluster_size,
        record_size: rs,
        total_sectors: boot.total_sectors,
        mft_lcn: boot.mft_lcn,
        mft_runs: mft.data.runs.clone(),
        mft_initialized: initialized,
        mft_bitmap: Vec::new(),
        mirror: Vec::new(),
        allocated: RangeSet::new(),
        format_time: mft.created.unwrap_or(0),
        incomplete: false,
    };

    // The $MFT bitmap: one bit per slot; unreadable parts count as in use.
    let slots = vol.slot_count();
    let want = slots.div_ceil(8);
    let mut bitmap: Vec<u8> = Vec::new();
    if mft.bitmap.found {
        let mut fill_bad = Vec::new();
        // `want` is at most MAX_MFT_SLOTS / 8 bytes, gaps included.
        let ok = read_stream(rd, start, cs, &mft.bitmap, want, &mut |p| match p {
            Piece::Data(b, bad) => {
                let base = u64::try_from(bitmap.len()).unwrap_or(u64::MAX);
                for r in bad {
                    fill_bad.push(base.saturating_add(r.start)..base.saturating_add(r.end));
                }
                bitmap.extend_from_slice(b);
            }
            Piece::Gap { len, sparse } => bitmap.resize(
                bitmap
                    .len()
                    .saturating_add(usize::try_from(len).unwrap_or(0)),
                if sparse { 0 } else { 0xFF },
            ),
        })?;
        complete &= ok;
        for r in fill_bad {
            let lo = usize::try_from(r.start).unwrap_or(usize::MAX);
            let hi = usize::try_from(r.end)
                .unwrap_or(usize::MAX)
                .min(bitmap.len());
            if let Some(s) = bitmap.get_mut(lo..hi) {
                s.fill(0xFF);
            }
        }
    } else {
        complete = false;
    }
    let want_len = usize::try_from(want).unwrap_or(usize::MAX);
    if bitmap.len() < want_len {
        // The stored bitmap is shorter than the initialized size needs:
        // NTFS keeps it at least that long, so the rest is unknown.
        complete = false;
        bitmap.resize(want_len, 0xFF);
    }
    bitmap.truncate(want_len);
    vol.mft_bitmap = bitmap;

    // Record 3 ($Volume): format time.
    match vol.slot_offset(3) {
        Some(at) => match read_record(rd, at, rs)? {
            Some(mut raw) => match parse_file_record(&mut raw) {
                Ok(r3) => {
                    let mut f = SystemFile::default();
                    let _ = collect(&r3, &mut f);
                    match f.created {
                        Some(t) => vol.format_time = t,
                        None => complete = false,
                    }
                }
                Err(_) => complete = false,
            },
            None => complete = false,
        },
        None => complete = false,
    }

    // Record 6 ($Bitmap): live-allocated ranges.
    let total = vol.total_clusters();
    let map_slot = |_: &SystemFile, n: u64| vol.slot_offset(n);
    match vol.slot_offset(6) {
        Some(at) => match read_system_file(rd, start, cs, rs, (6, at), &map_slot)? {
            Some((bmp, ok)) => {
                complete &= ok;
                let mut alloc = RangeSet::new();
                let mut run_start: Option<u64> = None;
                let mut cluster = 0u64;
                // Only clusters that start inside the session range can
                // matter, and the bitmap holds at most its real size: the
                // read is bounded by both, whatever the boot sector's
                // total_sectors claims. The rest counts as allocated below.
                let in_range = rd
                    .src
                    .range()
                    .end
                    .saturating_sub(start)
                    .div_ceil(cs.max(1));
                let mut want = total.min(in_range).div_ceil(8);
                if let Some((real, _)) = bmp.data.sizes {
                    want = want.min(real);
                }
                let ok = read_stream(rd, start, cs, &bmp.data, want, &mut |p| {
                    let mut conv = BitRuns {
                        alloc: &mut alloc,
                        vol: start,
                        cs,
                        total,
                        run_start: &mut run_start,
                    };
                    let bytes = match p {
                        Piece::Data(b, bad) => {
                            conv.feed(cluster, b, bad);
                            u64::try_from(b.len()).unwrap_or(u64::MAX)
                        }
                        Piece::Gap { len, sparse } => {
                            conv.gap(cluster, !sparse);
                            len
                        }
                    };
                    cluster = cluster.saturating_add(bytes.saturating_mul(8));
                })?;
                complete &= ok;
                let read_clusters = cluster.min(total);
                if let Some(s) = run_start {
                    insert_clusters(&mut alloc, start, cs, s, read_clusters);
                }
                if read_clusters < total {
                    // Bitmap shorter than the volume: the rest is unknown.
                    complete = false;
                    insert_clusters(&mut alloc, start, cs, read_clusters, total);
                }
                vol.allocated = alloc;
            }
            None => complete = false,
        },
        None => complete = false,
    }
    if vol.allocated.is_empty() && !complete {
        // Nothing known about the allocation: everything counts as written.
        insert_clusters(&mut vol.allocated, start, cs, 0, total);
    }

    // Record 1 ($MFTMirr): mirror ranges.
    match vol.slot_offset(1) {
        Some(at) => match read_record(rd, at, rs)? {
            Some(mut raw) => match parse_file_record(&mut raw) {
                Ok(r1) => {
                    let mut f = SystemFile::default();
                    let _ = collect(&r1, &mut f);
                    for run in &f.data.runs {
                        let Some(lcn) = run.lcn else { continue };
                        let (Some(a), Some(len)) = (
                            lcn.checked_mul(cs).and_then(|b| b.checked_add(start)),
                            run.clusters.checked_mul(cs),
                        ) else {
                            continue;
                        };
                        if let Some(b) = a.checked_add(len) {
                            vol.mirror.push(a..b);
                        }
                    }
                }
                Err(_) => complete = false,
            },
            None => complete = false,
        },
        None => complete = false,
    }
    vol.incomplete = !complete;
    Ok(Some(vol))
}

/// Adds clusters `[from, to)` of a volume at `vol` to `set` as bytes.
fn insert_clusters(set: &mut RangeSet, vol: u64, cs: u64, from: u64, to: u64) {
    let a = from.saturating_mul(cs).saturating_add(vol);
    let b = to.saturating_mul(cs).saturating_add(vol);
    if a < b {
        set.insert(a..b);
    }
}

/// Turns cluster-bitmap bytes into allocated byte ranges.
struct BitRuns<'a> {
    alloc: &'a mut RangeSet,
    vol: u64,
    cs: u64,
    total: u64,
    run_start: &'a mut Option<u64>,
}

impl BitRuns<'_> {
    /// A stretch of the bitmap from cluster `first` on that was not read:
    /// unknown (`allocated`, counts as written) or sparse (free). The
    /// caller advances past it; one call whatever its length.
    fn gap(&mut self, first: u64, allocated: bool) {
        if first >= self.total {
            return;
        }
        if allocated {
            if self.run_start.is_none() {
                *self.run_start = Some(first);
            }
        } else if let Some(s) = self.run_start.take() {
            insert_clusters(self.alloc, self.vol, self.cs, s, first);
        }
    }

    /// `bytes` describe clusters from `first` on; `bad` (relative byte
    /// ranges, ascending) were unreadable and count as allocated.
    fn feed(&mut self, first: u64, bytes: &[u8], bad: &[Range<u64>]) {
        let mut bad_iter = bad.iter().peekable();
        for (i, &byte) in bytes.iter().enumerate() {
            let pos = u64::try_from(i).unwrap_or(u64::MAX);
            while bad_iter.peek().is_some_and(|r| r.end <= pos) {
                bad_iter.next();
            }
            let unreadable = bad_iter.peek().is_some_and(|r| r.start <= pos);
            let byte = if unreadable { 0xFF } else { byte };
            let c0 = first.saturating_add(pos.saturating_mul(8));
            if c0 >= self.total {
                break;
            }
            match byte {
                0xFF if self.total.saturating_sub(c0) >= 8 => {
                    if self.run_start.is_none() {
                        *self.run_start = Some(c0);
                    }
                }
                0x00 if self.total.saturating_sub(c0) >= 8 => {
                    if let Some(s) = self.run_start.take() {
                        insert_clusters(self.alloc, self.vol, self.cs, s, c0);
                    }
                }
                _ => {
                    for bit in 0..8u64 {
                        let c = c0.saturating_add(bit);
                        if c >= self.total {
                            break;
                        }
                        let set = byte >> bit & 1 != 0;
                        match (set, *self.run_start) {
                            (true, None) => *self.run_start = Some(c),
                            (false, Some(s)) => {
                                insert_clusters(self.alloc, self.vol, self.cs, s, c);
                                *self.run_start = None;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}
