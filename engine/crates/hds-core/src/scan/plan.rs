//! The order in which the session range is read (ALGORITHMS A2).
//!
//! The range is cut into a fixed grid of 8 MiB chunks starting at the range
//! start. A chunk is read at most once per scan: the planner keeps the done
//! set together with the chunks it has handed out, and hands out the parts
//! of a grid chunk that are neither. Probes come first, in ascending order
//! of their offsets; then the sweep walks the grid from the range start.
//!
//! Every decision is a function of what has been committed before it, so
//! the order of reading - and with it everything that depends on it - does
//! not depend on how fast the reader thread happens to be.

use std::ops::Range;
use std::sync::atomic::AtomicBool;

use hds_io::{
    parse_partition_table, read_unaligned, RangeSet, RangedSource, ReadOutcome, TableKind,
};
use hds_ntfs::{parse_boot_sector, BOOT_SECTOR_SIZE};

use super::{Abort, ScanPhase};

/// Size of a chunk: 8 MiB (ALGORITHMS A1).
pub const CHUNK_SIZE: u64 = 8 << 20;

/// A probe ends when this many bytes have passed since its last chunk with
/// a `FILE` hit.
pub(crate) const PROBE_QUIET: u64 = 64 << 20;

/// The heuristic probe lies at most this far behind a partition start.
const PROBE_MAX_DISTANCE: u64 = 3 << 30;
const GIB: u64 = 1 << 30;

/// A chunk handed out by the planner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Planned {
    /// Absolute byte range; sector-aligned, at most one grid chunk.
    pub range: Range<u64>,
    /// The phase it belongs to.
    pub phase: ScanPhase,
    /// For a probe chunk: the probe's position in the planner's list.
    pub probe: Option<usize>,
}

#[derive(Clone, Debug)]
struct Probe {
    /// Start of the next grid chunk this probe looks at.
    next: u64,
    /// The probe ends at the first grid chunk starting here or later.
    quiet_until: u64,
}

/// Hands out the chunks of a scan in the order of ALGORITHMS A2.
#[derive(Clone, Debug)]
pub(crate) struct Planner {
    start: u64,
    end: u64,
    probes: Vec<Probe>,
    current: usize,
    sweep_next: u64,
    /// Done set plus everything handed out.
    reserved: RangeSet,
}

impl Planner {
    /// A planner for `range` that skips `done` and starts with probes at
    /// `probe_offsets` (absolute, inside the range).
    pub(crate) fn new(range: Range<u64>, done: &RangeSet, probe_offsets: &[u64]) -> Planner {
        let mut planner = Planner {
            start: range.start,
            end: range.end,
            probes: Vec::new(),
            current: 0,
            sweep_next: range.start,
            reserved: done.clone(),
        };
        for &offset in probe_offsets {
            if !(range.start..range.end).contains(&offset) {
                continue;
            }
            planner.probes.push(Probe {
                next: planner.grid_start(offset),
                quiet_until: offset.saturating_add(PROBE_QUIET),
            });
        }
        planner
    }

    /// Start of the grid chunk that contains `offset`.
    fn grid_start(&self, offset: u64) -> u64 {
        let rel = offset.saturating_sub(self.start);
        let index = rel / CHUNK_SIZE;
        self.start.saturating_add(index.saturating_mul(CHUNK_SIZE))
    }

    fn grid_chunk(&self, start: u64) -> Range<u64> {
        start..start.saturating_add(CHUNK_SIZE).min(self.end)
    }

    /// The first part of `grid` that is not reserved, reserved now.
    fn take_gap(&mut self, grid: Range<u64>) -> Option<Range<u64>> {
        let gap = self.reserved.gaps(grid).next()?;
        self.reserved.insert(gap.clone());
        Some(gap)
    }

    /// The next chunk to read, or `None` when everything is done or handed
    /// out.
    pub(crate) fn next(&mut self) -> Option<Planned> {
        while let Some(probe) = self.probes.get(self.current).cloned() {
            if probe.next >= self.end || probe.next >= probe.quiet_until {
                self.current = self.current.saturating_add(1);
                continue;
            }
            let grid = self.grid_chunk(probe.next);
            if let Some(range) = self.take_gap(grid.clone()) {
                return Some(Planned {
                    range,
                    phase: ScanPhase::MftProbe,
                    probe: Some(self.current),
                });
            }
            if let Some(p) = self.probes.get_mut(self.current) {
                p.next = grid.end;
            }
        }
        while self.sweep_next < self.end {
            let grid = self.grid_chunk(self.sweep_next);
            if let Some(range) = self.take_gap(grid.clone()) {
                return Some(Planned {
                    range,
                    phase: ScanPhase::Sweep,
                    probe: None,
                });
            }
            self.sweep_next = grid.end;
        }
        None
    }

    /// A committed chunk of probe `probe` that ends at `chunk_end` held at
    /// least one `FILE` hit: the probe goes on for another 64 MiB.
    pub(crate) fn probe_hit(&mut self, probe: usize, chunk_end: u64) {
        if let Some(p) = self.probes.get_mut(probe) {
            p.quiet_until = p.quiet_until.max(chunk_end.saturating_add(PROBE_QUIET));
        }
    }
}

/// Reads the boot sector at `offset` and returns the absolute offset of the
/// `$MFT` it names, if it is a valid NTFS boot sector and that offset lies
/// inside the range.
fn mft_offset_at(
    source: &RangedSource,
    offset: u64,
    cancel: &AtomicBool,
) -> Result<Option<u64>, Abort> {
    let range = source.range();
    if offset.saturating_add(BOOT_SECTOR_SIZE as u64) > range.end {
        return Ok(None);
    }
    let mut sector = [0_u8; BOOT_SECTOR_SIZE];
    match read_unaligned(source, offset, &mut sector, cancel) {
        ReadOutcome::Data { bad } if bad.is_empty() => {}
        ReadOutcome::Data { .. } => return Ok(None),
        ReadOutcome::SourceLost(e) => return Err(Abort::SourceLost(e)),
        ReadOutcome::Cancelled => return Err(Abort::Cancelled),
        ReadOutcome::Internal(e) => return Err(Abort::Internal(e)),
    }
    let Ok(boot) = parse_boot_sector(&sector) else {
        return Ok(None);
    };
    Ok(offset
        .checked_add(boot.mft_offset())
        .filter(|mft| range.contains(mft)))
}

/// The probe offsets of ALGORITHMS A2, duplicates removed: first the
/// `$MFT` of every valid NTFS boot sector at the range start or at a
/// partition start inside the range (ascending), then `P + min(3 GiB,
/// size / 3)` rounded down to 4 KiB, and that distance rounded down to whole
/// GiB when it is at least 1 GiB, for each partition start `P` inside the
/// range (the range start and the range length when there is none),
/// ascending. The boot sectors come first because they are the stronger
/// evidence of where an `$MFT` is.
///
/// The partition table is read through the ranged source, so nothing
/// outside the range is read: for a range that does not contain sector 0
/// the table is simply not found.
pub(crate) fn probe_offsets(source: &RangedSource, cancel: &AtomicBool) -> Result<Vec<u64>, Abort> {
    let range = source.range();
    let table = parse_partition_table(source);
    let partitions: Vec<(u64, u64)> = if table.table == TableKind::None {
        Vec::new()
    } else {
        table
            .partitions
            .iter()
            .filter(|p| range.contains(&p.offset))
            .map(|p| (p.offset, p.size))
            .collect()
    };

    let mut boot_starts = vec![range.start];
    boot_starts.extend(partitions.iter().map(|&(offset, _)| offset));
    boot_starts.sort_unstable();
    boot_starts.dedup();

    let mut offsets = Vec::new();
    for start in boot_starts {
        if let Some(mft) = mft_offset_at(source, start, cancel)? {
            offsets.push(mft);
        }
    }
    offsets.sort_unstable();
    offsets.dedup();
    let bases = if partitions.is_empty() {
        vec![(range.start, range.end.saturating_sub(range.start))]
    } else {
        partitions
    };
    let mut heuristic: Vec<u64> = bases
        .into_iter()
        .flat_map(|(start, size)| {
            let distance = PROBE_MAX_DISTANCE.min(size / 3) & !4095;
            // Windows rounds a distance of a GiB or more down to whole GiB
            // (measured: 4 GB volume -> 1 GiB, 16 and 64 GB -> 3 GiB).
            let whole_gib = (distance >= GIB).then_some(distance & !(GIB - 1));
            [Some(distance), whole_gib]
                .into_iter()
                .flatten()
                .filter_map(move |d| start.checked_add(d))
        })
        .filter(|p| range.contains(p))
        .collect();
    heuristic.sort_unstable();
    heuristic.dedup();
    heuristic.retain(|p| !offsets.contains(p));
    offsets.extend(heuristic);
    Ok(offsets)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::arithmetic_side_effects,
        clippy::unwrap_used,
        clippy::indexing_slicing
    )]

    use super::*;

    fn drain(planner: &mut Planner) -> Vec<(Range<u64>, ScanPhase)> {
        std::iter::from_fn(|| planner.next())
            .map(|p| (p.range, p.phase))
            .collect()
    }

    #[test]
    fn sweep_covers_the_range_once_on_the_grid() {
        let range = 4096..(3 * CHUNK_SIZE + 4096 + 512 * 7);
        let mut planner = Planner::new(range.clone(), &RangeSet::new(), &[]);
        let chunks = drain(&mut planner);
        assert_eq!(
            chunks.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>(),
            vec![
                4096..4096 + CHUNK_SIZE,
                4096 + CHUNK_SIZE..4096 + 2 * CHUNK_SIZE,
                4096 + 2 * CHUNK_SIZE..4096 + 3 * CHUNK_SIZE,
                4096 + 3 * CHUNK_SIZE..range.end,
            ]
        );
        assert!(chunks.iter().all(|(_, phase)| *phase == ScanPhase::Sweep));
    }

    #[test]
    fn done_parts_are_skipped_and_partial_chunks_are_completed() {
        let range = 0..4 * CHUNK_SIZE;
        let mut done = RangeSet::new();
        done.insert(0..CHUNK_SIZE);
        done.insert(2 * CHUNK_SIZE + 4096..2 * CHUNK_SIZE + 8192);
        let mut planner = Planner::new(range, &done, &[]);
        let chunks: Vec<_> = drain(&mut planner).into_iter().map(|(r, _)| r).collect();
        assert_eq!(
            chunks,
            vec![
                CHUNK_SIZE..2 * CHUNK_SIZE,
                2 * CHUNK_SIZE..2 * CHUNK_SIZE + 4096,
                2 * CHUNK_SIZE + 8192..3 * CHUNK_SIZE,
                3 * CHUNK_SIZE..4 * CHUNK_SIZE,
            ]
        );
    }

    #[test]
    fn a_probe_reads_64_mib_past_its_last_hit_then_the_sweep_follows() {
        let range = 0..40 * CHUNK_SIZE;
        let probe_at = 10 * CHUNK_SIZE + 12345;
        let mut planner = Planner::new(range.clone(), &RangeSet::new(), &[probe_at]);
        let mut probe_chunks = Vec::new();
        loop {
            let planned = planner.next().unwrap();
            if planned.phase != ScanPhase::MftProbe {
                // The first sweep chunk is the range start.
                assert_eq!(planned.range, 0..CHUNK_SIZE);
                break;
            }
            // A hit in the probe's second chunk extends it.
            if planned.range.start == 11 * CHUNK_SIZE {
                planner.probe_hit(planned.probe.unwrap(), planned.range.end);
            }
            probe_chunks.push(planned.range);
        }
        // From chunk 10 (containing the offset) through 12 * 8 MiB + 64 MiB.
        assert_eq!(probe_chunks.first().unwrap().start, 10 * CHUNK_SIZE);
        assert_eq!(probe_chunks.last().unwrap().end, 20 * CHUNK_SIZE);
        let rest = drain(&mut planner);
        let swept: u64 = rest.iter().map(|(r, _)| r.end - r.start).sum::<u64>() + CHUNK_SIZE;
        assert_eq!(swept + 10 * CHUNK_SIZE, range.end);
        assert!(rest
            .iter()
            .all(|(r, _)| r.end <= 10 * CHUNK_SIZE || r.start >= 20 * CHUNK_SIZE));
    }

    #[test]
    fn a_probe_inside_done_parts_ends_without_reading() {
        let range = 0..30 * CHUNK_SIZE;
        let mut done = RangeSet::new();
        done.insert(0..20 * CHUNK_SIZE);
        let mut planner = Planner::new(range, &done, &[CHUNK_SIZE]);
        let first = planner.next().unwrap();
        assert_eq!(first.phase, ScanPhase::Sweep);
        assert_eq!(first.range, 20 * CHUNK_SIZE..21 * CHUNK_SIZE);
    }
}
