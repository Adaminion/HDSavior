//! Names as they are on disk: UTF-16LE code units, borrowed.

use core::fmt;
use core::iter::FusedIterator;

/// A name borrowed from a record or index block: UTF-16LE code units, not
/// guaranteed to be valid UTF-16 (NTFS allows unpaired surrogates).
///
/// Nothing is copied or decoded until asked for. The byte length is always
/// even.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Utf16Name<'a>(&'a [u8]);

impl<'a> Utf16Name<'a> {
    /// The empty name (of an unnamed attribute).
    pub const EMPTY: Utf16Name<'static> = Utf16Name(&[]);

    /// Wraps UTF-16LE bytes. A trailing odd byte is dropped.
    #[inline]
    #[must_use]
    pub fn from_bytes(bytes: &'a [u8]) -> Self {
        let (units, _) = bytes.as_chunks::<2>();
        Utf16Name(units.as_flattened())
    }

    /// The UTF-16LE bytes, two per code unit.
    #[inline]
    #[must_use]
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.0
    }

    /// Number of UTF-16 code units.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.0.len() / 2
    }

    /// Whether the name has no code units.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The code units.
    #[inline]
    #[must_use]
    pub fn units(&self) -> Utf16Units<'a> {
        let (units, _) = self.0.as_chunks::<2>();
        Utf16Units(units.iter())
    }

    /// Whether the name consists of exactly the UTF-16 code units of `s`
    /// (case-sensitive).
    #[inline]
    #[must_use]
    pub fn eq_str(&self, s: &str) -> bool {
        self.units().eq(s.encode_utf16())
    }

    /// The name as a `String`; every unpaired surrogate becomes U+FFFD.
    /// Allocates: meant for messages and tests, not for the scan loop.
    #[must_use]
    pub fn to_string_lossy(&self) -> String {
        char::decode_utf16(self.units())
            .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect()
    }
}

impl PartialEq<str> for Utf16Name<'_> {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.eq_str(other)
    }
}

impl PartialEq<&str> for Utf16Name<'_> {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.eq_str(other)
    }
}

impl fmt::Debug for Utf16Name<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.to_string_lossy())
    }
}

impl fmt::Display for Utf16Name<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

/// Iterator over the UTF-16 code units of a [`Utf16Name`].
#[derive(Clone, Debug)]
pub struct Utf16Units<'a>(core::slice::Iter<'a, [u8; 2]>);

impl Iterator for Utf16Units<'_> {
    type Item = u16;

    #[inline]
    fn next(&mut self) -> Option<u16> {
        self.0.next().map(|unit| u16::from_le_bytes(*unit))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl DoubleEndedIterator for Utf16Units<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<u16> {
        self.0.next_back().map(|unit| u16::from_le_bytes(*unit))
    }
}

impl ExactSizeIterator for Utf16Units<'_> {}

impl FusedIterator for Utf16Units<'_> {}
