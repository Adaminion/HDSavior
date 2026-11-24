//! Growable storage in fixed-size chunks.
//!
//! A `Vec` that grows to ten million records doubles its capacity a few
//! times: the spare capacity can reach the size of the data, and every
//! doubling holds the old and the new buffer at once. Chunks of fixed size
//! never move what they hold, and at most one chunk is partly empty.

/// A sequence of `T` in chunks of `2^SHIFT` items.
#[derive(Clone, Debug)]
pub(crate) struct Chunked<T, const SHIFT: u32> {
    chunks: Vec<Vec<T>>,
    len: usize,
}

impl<T, const SHIFT: u32> Default for Chunked<T, SHIFT> {
    fn default() -> Self {
        Chunked {
            chunks: Vec::new(),
            len: 0,
        }
    }
}

impl<T: Copy, const SHIFT: u32> Chunked<T, SHIFT> {
    const CHUNK: usize = 1 << SHIFT;
    const MASK: usize = (1 << SHIFT) - 1;

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn push(&mut self, value: T) {
        match self.chunks.last_mut() {
            Some(last) if last.len() < Self::CHUNK => last.push(value),
            _ => {
                let mut chunk = Vec::with_capacity(Self::CHUNK);
                chunk.push(value);
                self.chunks.push(chunk);
            }
        }
        self.len = self.len.saturating_add(1);
    }

    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        self.chunks.get(index >> SHIFT)?.get(index & Self::MASK)
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        let full = len >> SHIFT;
        let rest = len & Self::MASK;
        let keep = if rest == 0 {
            full
        } else {
            full.saturating_add(1)
        };
        self.chunks.truncate(keep);
        if rest != 0 {
            if let Some(last) = self.chunks.last_mut() {
                last.truncate(rest);
            }
        }
        self.len = len;
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + '_ {
        self.chunks.iter().flatten()
    }

    /// Items from `start` on.
    pub(crate) fn iter_from(&self, start: usize) -> impl Iterator<Item = &T> + '_ {
        self.iter().skip(start)
    }

    /// Heap bytes held.
    pub(crate) fn heap_bytes(&self) -> usize {
        let items = self
            .chunks
            .iter()
            .fold(0_usize, |sum, c| sum.saturating_add(c.capacity()));
        items
            .saturating_mul(std::mem::size_of::<T>())
            .saturating_add(
                self.chunks
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Vec<T>>()),
            )
    }
}

/// A byte arena in chunks of `2^SHIFT` bytes. A piece is stored
/// contiguously inside one chunk and addressed by `chunk << SHIFT | offset`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Arena<const SHIFT: u32> {
    chunks: Vec<Vec<u8>>,
}

impl<const SHIFT: u32> Arena<SHIFT> {
    const CHUNK: usize = 1 << SHIFT;
    const MASK: u64 = (1 << SHIFT) - 1;

    /// Appends `bytes` and returns their address; `None` for a piece
    /// larger than a chunk.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Option<u64> {
        if bytes.len() > Self::CHUNK {
            return None;
        }
        if bytes.is_empty() {
            return Some(0);
        }
        let fits = self
            .chunks
            .last()
            .is_some_and(|last| last.len().saturating_add(bytes.len()) <= Self::CHUNK);
        if !fits {
            self.chunks.push(Vec::with_capacity(Self::CHUNK));
        }
        let index = self.chunks.len().checked_sub(1)?;
        let last = self.chunks.last_mut()?;
        let offset = last.len();
        last.extend_from_slice(bytes);
        let chunk = u64::try_from(index).ok()?;
        Some((chunk << SHIFT) | u64::try_from(offset).ok()?)
    }

    /// The `len` bytes at `address`.
    pub(crate) fn get(&self, address: u64, len: usize) -> Option<&[u8]> {
        if len == 0 {
            return Some(&[]);
        }
        let chunk = usize::try_from(address >> SHIFT).ok()?;
        let start = usize::try_from(address & Self::MASK).ok()?;
        self.chunks.get(chunk)?.get(start..start.checked_add(len)?)
    }

    /// The address the arena ends at (to roll back to).
    pub(crate) fn mark(&self) -> (usize, usize) {
        (self.chunks.len(), self.chunks.last().map_or(0, Vec::len))
    }

    /// Drops everything stored after `mark`.
    pub(crate) fn rollback(&mut self, (chunks, last_len): (usize, usize)) {
        self.chunks.truncate(chunks);
        if let Some(last) = self.chunks.last_mut() {
            last.truncate(last_len);
        }
    }

    /// Bytes stored.
    pub(crate) fn len(&self) -> usize {
        self.chunks
            .iter()
            .fold(0_usize, |sum, c| sum.saturating_add(c.len()))
    }

    /// Heap bytes held.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.chunks
            .iter()
            .fold(0_usize, |sum, c| sum.saturating_add(c.capacity()))
            .saturating_add(
                self.chunks
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Vec<u8>>()),
            )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

    use super::*;

    #[test]
    fn chunked_push_get_truncate() {
        let mut v: Chunked<u32, 2> = Chunked::default();
        for i in 0..10 {
            v.push(i);
        }
        assert_eq!(v.len(), 10);
        assert_eq!(
            v.iter().copied().collect::<Vec<_>>(),
            (0..10).collect::<Vec<_>>()
        );
        assert_eq!(v.get(9), Some(&9));
        assert_eq!(v.get(10), None);
        v.truncate(8);
        assert_eq!(v.len(), 8);
        assert_eq!(v.get(8), None);
        v.truncate(5);
        assert_eq!(v.iter().copied().collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);
        v.push(77);
        assert_eq!(v.get(5), Some(&77));
        assert_eq!(v.iter_from(4).copied().collect::<Vec<_>>(), vec![4, 77]);
        v.truncate(0);
        assert_eq!(v.len(), 0);
        assert_eq!(v.iter().count(), 0);
    }

    #[test]
    fn arena_keeps_pieces_whole_and_rolls_back() {
        let mut a: Arena<4> = Arena::default();
        let p1 = a.push(&[1; 10]).unwrap();
        let mark = a.mark();
        let p2 = a.push(&[2; 10]).unwrap(); // does not fit behind p1: next chunk
        assert_eq!(p1, 0);
        assert_eq!(p2, 1 << 4);
        assert_eq!(a.get(p2, 10).unwrap(), &[2; 10]);
        let p3 = a.push(&[]).unwrap();
        assert_eq!(a.get(p3, 0).unwrap(), &[] as &[u8]);
        assert!(a.push(&[0; 17]).is_none());
        a.rollback(mark);
        assert_eq!(a.len(), 10);
        assert!(a.get(p2, 10).is_none());
        assert_eq!(a.get(p1, 10).unwrap(), &[1; 10]);
    }
}
