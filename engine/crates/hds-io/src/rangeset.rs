//! [`RangeSet`]: a sorted set of byte ranges.
//!
//! The engine keeps several of these per session: what has been scanned (the
//! *done set*), unreadable sectors (*bad ranges*), what the current file
//! system has allocated (*live-allocated ranges*) and what has been written
//! since the format (the *overwritten set*). A fragmented volume produces
//! millions of ranges, so every operation here costs `O(log n)` plus the
//! number of ranges it actually touches.

use std::collections::btree_map;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A set of `u64` offsets stored as sorted, disjoint, non-adjacent half-open
/// ranges `[start, end)`.
///
/// Ranges that touch or overlap are merged on insertion, so the set has one
/// canonical form: two sets holding the same offsets compare equal and
/// serialize identically. An empty or inverted range (`start >= end`) is the
/// empty set everywhere: inserting or removing it does nothing, it intersects
/// nothing, and it is contained in every set.
///
/// ```
/// use hds_io::RangeSet;
///
/// let mut done = RangeSet::new();
/// done.insert(0..4096);
/// done.insert(8192..12288);
/// done.insert(4096..8192); // closes the gap: one range remains
/// assert_eq!(done.iter().collect::<Vec<_>>(), vec![0..12288]);
/// assert_eq!(done.gaps(0..16384).collect::<Vec<_>>(), vec![12288..16384]);
/// assert_eq!(done.total_len(), 12288);
/// ```
///
/// Serialized form: a sequence of `(start, end)` pairs in ascending order.
/// Deserialization accepts the pairs in any order and merges them, but
/// rejects a pair with `start >= end`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RangeSet {
    /// `start -> end`. Invariant: every range is non-empty, and for
    /// consecutive entries `end(k) < start(k + 1)`.
    map: BTreeMap<u64, u64>,
    /// Sum of the lengths of all ranges. Cannot overflow: the ranges are
    /// disjoint subsets of `0..u64::MAX`.
    total: u64,
}

impl RangeSet {
    /// An empty set.
    pub fn new() -> RangeSet {
        RangeSet::default()
    }

    /// True when the set holds no offset.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Number of disjoint ranges (not bytes).
    pub fn range_count(&self) -> usize {
        self.map.len()
    }

    /// Number of offsets in the set: the sum of the lengths of all ranges.
    pub fn total_len(&self) -> u64 {
        self.total
    }

    /// Removes everything.
    pub fn clear(&mut self) {
        self.map.clear();
        self.total = 0;
    }

    /// The lowest range, if any.
    pub fn first(&self) -> Option<Range<u64>> {
        self.map.first_key_value().map(|(&s, &e)| s..e)
    }

    /// The highest range, if any.
    pub fn last(&self) -> Option<Range<u64>> {
        self.map.last_key_value().map(|(&s, &e)| s..e)
    }

    /// The smallest single range that covers the whole set.
    pub fn span(&self) -> Option<Range<u64>> {
        match (self.map.first_key_value(), self.map.last_key_value()) {
            (Some((&s, _)), Some((_, &e))) => Some(s..e),
            _ => None,
        }
    }

    /// Adds `range` to the set, merging it with every range it overlaps or
    /// touches. Returns the number of offsets that were not in the set before.
    pub fn insert(&mut self, range: Range<u64>) -> u64 {
        let (mut start, mut end) = (range.start, range.end);
        if start >= end {
            return 0;
        }
        // Bytes of existing ranges that end up inside the merged range.
        let mut absorbed = 0u64;
        // The range that begins at or before `start` can reach into or touch the new one.
        if let Some((&ps, &pe)) = self.map.range(..=start).next_back() {
            if pe >= start {
                if pe >= end {
                    return 0;
                }
                start = ps;
                absorbed += pe - ps;
                self.map.remove(&ps);
            }
        }
        // Every range that begins inside the (growing) new range or right at its end.
        while let Some((&s, &e)) = self.map.range(start..=end).next() {
            absorbed += e - s;
            end = end.max(e);
            self.map.remove(&s);
        }
        self.map.insert(start, end);
        let added = (end - start) - absorbed;
        self.total += added;
        added
    }

    /// Removes `range` from the set, splitting ranges that straddle its ends.
    /// Returns the number of offsets that were removed.
    pub fn remove(&mut self, range: Range<u64>) -> u64 {
        let (start, end) = (range.start, range.end);
        if start >= end {
            return 0;
        }
        let mut removed = 0u64;
        // A range that begins before `start` and reaches into the removed part keeps its head.
        if let Some((&ps, &pe)) = self.map.range(..start).next_back() {
            if pe > start {
                self.map.insert(ps, start);
                removed += pe.min(end) - start;
                if pe > end {
                    // ... and its tail, when the removed part lies strictly inside it.
                    self.map.insert(end, pe);
                    self.total -= removed;
                    return removed;
                }
            }
        }
        // Ranges that begin inside the removed part go, except for a tail beyond `end`.
        while let Some((&s, &e)) = self.map.range(start..end).next() {
            self.map.remove(&s);
            if e > end {
                self.map.insert(end, e);
                removed += end - s;
                break;
            }
            removed += e - s;
        }
        self.total -= removed;
        removed
    }

    /// True when `offset` is in the set.
    pub fn contains(&self, offset: u64) -> bool {
        self.map
            .range(..=offset)
            .next_back()
            .is_some_and(|(_, &e)| e > offset)
    }

    /// True when every offset of `range` is in the set (vacuously true for an
    /// empty range).
    pub fn contains_range(&self, range: Range<u64>) -> bool {
        if range.start >= range.end {
            return true;
        }
        self.map
            .range(..=range.start)
            .next_back()
            .is_some_and(|(_, &e)| e >= range.end)
    }

    /// True when at least one offset of `range` is in the set.
    pub fn intersects(&self, range: Range<u64>) -> bool {
        if range.start >= range.end {
            return false;
        }
        // Ends ascend with starts, so the last range that begins before
        // `range.end` is the one that reaches furthest.
        self.map
            .range(..range.end)
            .next_back()
            .is_some_and(|(_, &e)| e > range.start)
    }

    /// The parts of `range` that are in the set, in ascending order, clipped
    /// to `range`.
    pub fn intersection(&self, range: Range<u64>) -> Intersection<'_> {
        if range.start >= range.end {
            return Intersection {
                head: None,
                tail: self.map.range(0..0),
                end: 0,
            };
        }
        let head = match self.map.range(..range.start).next_back() {
            Some((_, &pe)) if pe > range.start => Some(range.start..pe.min(range.end)),
            _ => None,
        };
        Intersection {
            head,
            tail: self.map.range(range.start..range.end),
            end: range.end,
        }
    }

    /// Number of offsets of `range` that are in the set.
    pub fn covered_len(&self, range: Range<u64>) -> u64 {
        self.intersection(range).map(|r| r.end - r.start).sum()
    }

    /// The parts of `range` that are **not** in the set, in ascending order.
    pub fn gaps(&self, range: Range<u64>) -> Gaps<'_> {
        let (cursor, end) = if range.start >= range.end {
            (0, 0)
        } else {
            (range.start, range.end)
        };
        Gaps {
            cursor,
            end,
            inner: self.intersection(range),
        }
    }

    /// All ranges in ascending order.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            inner: self.map.iter(),
        }
    }

    /// Adds every range of `other`.
    pub fn union_with(&mut self, other: &RangeSet) {
        for r in other {
            self.insert(r);
        }
    }
}

/// Iterator over the ranges of a [`RangeSet`], see [`RangeSet::iter`].
#[derive(Clone, Debug)]
pub struct Iter<'a> {
    inner: btree_map::Iter<'a, u64, u64>,
}

impl Iterator for Iter<'_> {
    type Item = Range<u64>;

    fn next(&mut self) -> Option<Range<u64>> {
        self.inner.next().map(|(&s, &e)| s..e)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl DoubleEndedIterator for Iter<'_> {
    fn next_back(&mut self) -> Option<Range<u64>> {
        self.inner.next_back().map(|(&s, &e)| s..e)
    }
}

impl ExactSizeIterator for Iter<'_> {}

/// Iterator returned by [`RangeSet::intersection`].
#[derive(Clone, Debug)]
pub struct Intersection<'a> {
    /// Clipped part of a range that begins before the queried range.
    head: Option<Range<u64>>,
    tail: btree_map::Range<'a, u64, u64>,
    end: u64,
}

impl Iterator for Intersection<'_> {
    type Item = Range<u64>;

    fn next(&mut self) -> Option<Range<u64>> {
        if let Some(head) = self.head.take() {
            return Some(head);
        }
        self.tail.next().map(|(&s, &e)| s..e.min(self.end))
    }
}

/// Iterator returned by [`RangeSet::gaps`].
#[derive(Clone, Debug)]
pub struct Gaps<'a> {
    cursor: u64,
    end: u64,
    inner: Intersection<'a>,
}

impl Iterator for Gaps<'_> {
    type Item = Range<u64>;

    fn next(&mut self) -> Option<Range<u64>> {
        while self.cursor < self.end {
            match self.inner.next() {
                Some(covered) => {
                    let gap = self.cursor..covered.start;
                    self.cursor = covered.end;
                    if gap.start < gap.end {
                        return Some(gap);
                    }
                }
                None => {
                    let gap = self.cursor..self.end;
                    self.cursor = self.end;
                    return Some(gap);
                }
            }
        }
        None
    }
}

impl<'a> IntoIterator for &'a RangeSet {
    type Item = Range<u64>;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

impl Extend<Range<u64>> for RangeSet {
    fn extend<I: IntoIterator<Item = Range<u64>>>(&mut self, iter: I) {
        for r in iter {
            self.insert(r);
        }
    }
}

impl FromIterator<Range<u64>> for RangeSet {
    fn from_iter<I: IntoIterator<Item = Range<u64>>>(iter: I) -> RangeSet {
        let mut set = RangeSet::new();
        set.extend(iter);
        set
    }
}

impl fmt::Debug for RangeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl Serialize for RangeSet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.map.iter().map(|(&s, &e)| (s, e)))
    }
}

impl<'de> Deserialize<'de> for RangeSet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<RangeSet, D::Error> {
        struct PairsVisitor;

        impl<'de> Visitor<'de> for PairsVisitor {
            type Value = RangeSet;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a sequence of (start, end) pairs with start < end")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<RangeSet, A::Error> {
                // No pre-allocation from the announced length: the input is untrusted.
                let mut set = RangeSet::new();
                while let Some((start, end)) = seq.next_element::<(u64, u64)>()? {
                    if start >= end {
                        return Err(de::Error::custom(format_args!(
                            "invalid range {start}..{end}: start must be below end"
                        )));
                    }
                    set.insert(start..end);
                }
                Ok(set)
            }
        }

        deserializer.deserialize_seq(PairsVisitor)
    }
}

#[cfg(test)]
mod tests;
