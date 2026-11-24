//! Cluster allocation of a simulated volume: the state behind `$Bitmap`.

use crate::ondisk::runlist::Run;

/// One bit per cluster of a volume.
#[derive(Clone, Debug)]
pub struct Allocator {
    bits: Vec<u64>,
    total: u64,
    cursor: u64,
}

impl Allocator {
    /// An allocator for `total` free clusters.
    pub fn new(total: u64) -> Self {
        Allocator { bits: vec![0; total.div_ceil(64) as usize], total, cursor: 0 }
    }

    /// Number of clusters of the volume.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// `true` when cluster `lcn` is allocated.
    pub fn is_used(&self, lcn: u64) -> bool {
        assert!(lcn < self.total, "cluster {lcn} is outside the volume ({} clusters)", self.total);
        self.bits[(lcn / 64) as usize] >> (lcn % 64) & 1 == 1
    }

    /// `true` when all `len` clusters from `lcn` exist and are free.
    pub fn is_free(&self, lcn: u64, len: u64) -> bool {
        lcn.checked_add(len).is_some_and(|end| end <= self.total) && (lcn..lcn + len).all(|c| !self.is_used(c))
    }

    fn set(&mut self, lcn: u64, len: u64, used: bool) {
        assert!(
            lcn.checked_add(len).is_some_and(|end| end <= self.total),
            "clusters {lcn}+{len} are outside the volume ({} clusters)",
            self.total
        );
        for c in lcn..lcn + len {
            let (word, bit) = ((c / 64) as usize, c % 64);
            if used {
                self.bits[word] |= 1 << bit;
            } else {
                self.bits[word] &= !(1 << bit);
            }
        }
    }

    /// Allocates exactly the given clusters. Panics when one of them is
    /// already in use: two files of one volume never share clusters unless
    /// a scenario forces it with [`Allocator::mark`].
    pub fn alloc_at(&mut self, lcn: u64, len: u64) -> Run {
        assert!(len > 0, "empty allocation");
        assert!(self.is_free(lcn, len), "clusters {lcn}+{len} are not free");
        self.set(lcn, len, true);
        Run::at(lcn, len)
    }

    /// Marks clusters as used without checking (cross-linked files).
    pub fn mark(&mut self, lcn: u64, len: u64) {
        self.set(lcn, len, true);
    }

    /// Frees clusters.
    pub fn free(&mut self, lcn: u64, len: u64) {
        self.set(lcn, len, false);
    }

    /// Frees every allocated run of a run list.
    pub fn free_runs(&mut self, runs: &[Run]) {
        for run in runs {
            if let Some(lcn) = run.lcn {
                self.free(lcn, run.len);
            }
        }
    }

    /// Where the next automatic allocation starts looking.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Moves the search start of automatic allocations.
    pub fn set_cursor(&mut self, lcn: u64) {
        assert!(lcn <= self.total, "cursor outside the volume");
        self.cursor = lcn;
    }

    fn find_free(&self, len: u64, from: u64) -> Option<u64> {
        let mut start = from;
        while start + len <= self.total {
            match (start..start + len).rev().find(|&c| self.is_used(c)) {
                None => return Some(start),
                Some(used) => start = used + 1,
            }
        }
        None
    }

    /// Allocates `len` contiguous clusters at the first free place at or
    /// after the cursor (wrapping to the start of the volume once) and moves
    /// the cursor behind them.
    pub fn alloc(&mut self, len: u64) -> Run {
        assert!(len > 0, "empty allocation");
        let lcn = self
            .find_free(len, self.cursor)
            .or_else(|| self.find_free(len, 0))
            .unwrap_or_else(|| panic!("no {len} contiguous free clusters left on the volume"));
        self.set(lcn, len, true);
        self.cursor = lcn + len;
        Run::at(lcn, len)
    }

    /// Allocates `len` clusters in `pieces` fragments with one free cluster
    /// left between consecutive fragments, so that they can never be merged
    /// into one run.
    pub fn alloc_fragmented(&mut self, len: u64, pieces: u64) -> Vec<Run> {
        assert!(pieces >= 1 && pieces <= len, "cannot cut {len} clusters into {pieces} fragments");
        let base = len / pieces;
        let extra = len % pieces;
        let mut runs = Vec::with_capacity(pieces as usize);
        for i in 0..pieces {
            let piece = base + u64::from(i < extra);
            // find the piece plus a free gap cluster, allocate only the piece
            let lcn = self
                .find_free(piece + 1, self.cursor)
                .or_else(|| self.find_free(piece + 1, 0))
                .unwrap_or_else(|| panic!("no room for a fragment of {piece} clusters"));
            self.set(lcn, piece, true);
            self.cursor = lcn + piece + 1;
            runs.push(Run::at(lcn, piece));
        }
        runs
    }

    /// Number of allocated clusters.
    pub fn used_count(&self) -> u64 {
        self.bits.iter().map(|w| w.count_ones() as u64).sum()
    }

    /// The content of `$Bitmap`: one bit per cluster, least significant bit
    /// first, padded to a multiple of 8 bytes; the padding bits beyond the
    /// last cluster are set, as Windows writes them.
    pub fn bitmap_bytes(&self) -> Vec<u8> {
        let len = (self.total.div_ceil(8) as usize).div_ceil(8) * 8;
        let mut out = vec![0u8; len];
        for (i, word) in self.bits.iter().enumerate() {
            let bytes = word.to_le_bytes();
            let at = i * 8;
            let take = bytes.len().min(len - at);
            out[at..at + take].copy_from_slice(&bytes[..take]);
        }
        for bit in self.total..len as u64 * 8 {
            out[(bit / 8) as usize] |= 1 << (bit % 8);
        }
        out
    }

    /// Allocated clusters as sorted, maximal `(first, count)` ranges.
    pub fn used_ranges(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut start = None;
        for c in 0..self.total {
            match (self.is_used(c), start) {
                (true, None) => start = Some(c),
                (false, Some(s)) => {
                    out.push((s, c - s));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            out.push((s, self.total - s));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_fit_from_the_cursor_and_wrap_around() {
        let mut a = Allocator::new(100);
        assert_eq!(a.alloc(10), Run::at(0, 10));
        assert_eq!(a.alloc(5), Run::at(10, 5));
        a.alloc_at(20, 10);
        // 15..20 is too small for 6: the next fit is behind the used block
        assert_eq!(a.alloc(6), Run::at(30, 6));
        a.set_cursor(95);
        assert_eq!(a.alloc(5), Run::at(95, 5));
        // nothing behind the cursor: wraps to the hole at 15
        assert_eq!(a.alloc(5), Run::at(15, 5));
        assert_eq!(a.used_count(), 10 + 5 + 10 + 6 + 5 + 5);
        assert_eq!(a.used_ranges(), [(0, 36), (95, 5)]);
    }

    #[test]
    #[should_panic(expected = "are not free")]
    fn explicit_allocation_refuses_used_clusters() {
        let mut a = Allocator::new(100);
        a.alloc_at(10, 10);
        a.alloc_at(19, 2);
    }

    #[test]
    #[should_panic(expected = "contiguous free clusters")]
    fn a_full_volume_is_an_error() {
        let mut a = Allocator::new(10);
        a.alloc(8);
        a.alloc(3);
    }

    #[test]
    fn freeing_makes_clusters_reusable() {
        let mut a = Allocator::new(64);
        let r = a.alloc(64);
        a.free_runs(&[Run::at(8, 8), Run::hole(100), Run::at(32, 4)]);
        assert_eq!(a.used_count(), 52);
        a.set_cursor(0);
        assert_eq!(a.alloc(8), Run::at(8, 8));
        assert_eq!(a.alloc(4), Run::at(32, 4));
        assert_eq!(r, Run::at(0, 64));
    }

    #[test]
    fn fragments_never_touch() {
        let mut a = Allocator::new(1000);
        let runs = a.alloc_fragmented(10, 4);
        assert_eq!(runs, [Run::at(0, 3), Run::at(4, 3), Run::at(8, 2), Run::at(11, 2)]);
        assert_eq!(runs.iter().map(|r| r.len).sum::<u64>(), 10);
        // the gap clusters stay free for later files
        assert!(a.is_free(3, 1) && a.is_free(7, 1) && a.is_free(10, 1));
    }

    #[test]
    fn bitmap_bytes_are_lsb_first_and_padded_with_ones() {
        let mut a = Allocator::new(70);
        a.alloc_at(0, 3);
        a.alloc_at(9, 1);
        a.alloc_at(64, 2);
        let b = a.bitmap_bytes();
        assert_eq!(b.len(), 16, "70 bits need 9 bytes, rounded up to 16");
        assert_eq!(b[0], 0b0000_0111);
        assert_eq!(b[1], 0b0000_0010);
        assert_eq!(&b[2..8], &[0; 6]);
        // bits 64 and 65 used; 70..127 are padding and set
        assert_eq!(b[8], 0b1100_0011);
        assert_eq!(&b[9..], &[0xFF; 7]);
        // a volume of exactly 64 clusters has no padding
        assert_eq!(Allocator::new(64).bitmap_bytes(), [0; 8]);
    }
}
